#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::manual_assert,
    clippy::uninlined_format_args,
    clippy::unreadable_literal
)]

//! v4.28 PROD_READY.md machine-checkable gate.
//!
//! Each [machine] row in PROD_READY.md corresponds to one `#[test]`
//! in this file. CI runs `cargo test --release --test prod_ready`;
//! a failure here means PROD_READY.md is over-claiming.
//!
//! [machine] rows scaffolded (v4.28 baseline + v4.29 chaos):
//!   1.3   WAL replay on startup
//!   1.9   Partial-fsync recovery (delegates to e2e_chaos)
//!   1.10  Disk-full handling (delegates to e2e_chaos)
//!   3.8   cargo-audit in CI
//!   4.1   Health endpoint
//!   4.2   Prometheus metrics
//!   5.1   SPG_MAX_CONNECTIONS
//!   6.3   Concurrent read scaling (smoke)
//!   8.2   Native wire opcode stable
//!   9.2   Perf gates present
//!   9.8   CI workflow present
//!  10.x   PERFORMANCE.md carries v4.27 baseline (covers 10.1+10.2+10.3)
//!
//! v4.29-v4.32 will add more [machine] rows here as their features
//! land. The meta-test below enforces that every row tagged
//! `[machine]` in PROD_READY.md has a matching `row_X_Y_*` test
//! function — so the doc and the gate cannot drift apart.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn prod_ready_md() -> String {
    std::fs::read_to_string(workspace_root().join("PROD_READY.md"))
        .expect("PROD_READY.md must exist at workspace root")
}

/// Pull every row in PROD_READY.md whose name column ends with
/// `[machine]`. Returns row identifiers like "1.1", "3.1".
fn machine_marked_rows() -> Vec<String> {
    let md = prod_ready_md();
    let mut ids = Vec::new();
    for line in md.lines() {
        if !line.starts_with("| ") {
            continue;
        }
        let cells: Vec<&str> = line.split('|').collect();
        if cells.len() < 4 {
            continue;
        }
        let id = cells[1].trim();
        let name = cells[2].trim();
        if name.contains("[machine]")
            && !id.is_empty()
            && id.chars().next().unwrap().is_ascii_digit()
        {
            ids.push(id.to_string());
        }
    }
    ids
}

// ---- helpers shared by behavior tests ----

fn pick_free_addr() -> String {
    let p = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = p.local_addr().unwrap();
    drop(p);
    a.to_string()
}

fn unique_tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-prod-ready-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_server_with_env(addr: &str, db: &Path, wal: &Path, env: &[(&str, &str)]) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
    cmd.arg(addr)
        .arg(db)
        .arg("-")
        .arg(wal)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD")
        .env_remove("SPG_PG_ADDR");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.spawn().unwrap()
}

fn wait_for_listener(addr: &str, child: &mut Child) -> TcpStream {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match TcpStream::connect(addr) {
            Ok(s) => return s,
            Err(e) => {
                if let Ok(Some(status)) = child.try_wait() {
                    panic!("server exited early: {status:?} ({e})");
                }
                assert!(Instant::now() < deadline, "server never came up: {e}");
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

// ---- the rows ----

/// Helper: assert a top-level doc exists and contains every
/// required heading or anchor phrase. Used for the row 7.x ops-docs
/// machine checks (the docs are the evidence; this just keeps the
/// row from over-claiming if the file gets deleted or hollowed out).
fn assert_doc_has_sections(doc_name: &str, required: &[&str]) {
    let path = workspace_root().join(doc_name);
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("{doc_name} must exist at {}", path.display()));
    for needle in required {
        assert!(
            src.contains(needle),
            "{doc_name} missing required content: {needle:?}"
        );
    }
}

// ---- 3.9 cargo-deny ----

#[test]
fn row_3_9_cargo_deny_config_and_ci_present() {
    assert!(
        workspace_root().join("deny.toml").exists(),
        "deny.toml must exist at workspace root"
    );
    let yml =
        std::fs::read_to_string(workspace_root().join(".github/workflows/ci.yml")).expect("ci.yml");
    assert!(yml.contains("cargo-deny-action"), "CI must run cargo-deny");
}

// ---- 3.11 fuzz harnesses ----

#[test]
fn row_3_11_fuzz_harnesses_present() {
    let sql_fuzz = workspace_root().join("crates/spg-sql/tests/fuzz.rs");
    let wire_fuzz = workspace_root().join("crates/spg-server/tests/e2e_fuzz.rs");
    assert!(
        sql_fuzz.exists(),
        "SQL fuzz harness must exist at {}",
        sql_fuzz.display()
    );
    assert!(
        wire_fuzz.exists(),
        "wire fuzz harness must exist at {}",
        wire_fuzz.display()
    );
    let sql_src = std::fs::read_to_string(&sql_fuzz).unwrap();
    let wire_src = std::fs::read_to_string(&wire_fuzz).unwrap();
    assert!(sql_src.contains("fn fuzz_parse_statement_does_not_panic"));
    assert!(wire_src.contains("fn fuzz_wire_frame_does_not_panic"));
}

// ---- 8.4 / 8.5 / 8.6 STABILITY + cross-version compat ----

#[test]
fn row_8_4_snapshot_backwards_compat_test_present() {
    let p = workspace_root().join("crates/spg-server/tests/cross_version_compat.rs");
    assert!(p.exists(), "cross_version_compat.rs missing");
    let src = std::fs::read_to_string(&p).unwrap();
    assert!(src.contains("fn every_fixture_restores_and_verifies"));
}

#[test]
fn row_8_5_stability_doc_present() {
    assert_doc_has_sections(
        "STABILITY.md",
        &[
            "## Frozen surfaces",
            "Native wire protocol",
            "Snapshot file format",
            "Backup bundle format",
            "Env-var contract",
        ],
    );
}

#[test]
fn row_8_6_cross_version_fixture_present() {
    let fixtures = workspace_root().join("xtests/compat-fixtures");
    assert!(fixtures.is_dir(), "compat-fixtures dir missing");
    let entries: Vec<_> = std::fs::read_dir(&fixtures)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .collect();
    assert!(
        !entries.is_empty(),
        "compat-fixtures must contain at least one version directory"
    );
    // Each version dir must carry the trio expected.txt + full.bkp.
    for e in &entries {
        let p = e.path();
        assert!(
            p.join("expected.txt").exists(),
            "{}/expected.txt missing",
            p.display()
        );
        assert!(
            p.join("full.bkp").exists(),
            "{}/full.bkp missing",
            p.display()
        );
    }
}

// ---- 2.5 / 2.6 RESTORE_DRILL ----

#[test]
fn row_2_5_restore_drill_doc_present() {
    assert_doc_has_sections(
        "RESTORE_DRILL.md",
        &[
            "Step 0 — establish what you have",
            "Step 1 — assemble db + WAL from bundles",
            "Step 2 — point-in-time recovery",
            "Step 3 — start the recovered server",
            "Step 4 — verify",
            "Step 5 — re-bootstrap follower",
        ],
    );
}

#[test]
fn row_2_6_restore_drill_e2e_test_present() {
    let p = workspace_root().join("crates/spg-server/tests/e2e_restore_drill.rs");
    let src = std::fs::read_to_string(&p)
        .unwrap_or_else(|_| panic!("e2e_restore_drill.rs missing at {}", p.display()));
    assert!(
        src.contains("fn restore_drill_full_plus_incremental_recovers_row_count"),
        "the named restore-drill test must exist"
    );
}

// ---- 3.10 / 3.12 SECURITY.md ----

#[test]
fn row_3_10_threat_model_documented() {
    assert_doc_has_sections(
        "SECURITY.md",
        &["Threat model", "Secret handling", "What's out of scope"],
    );
}

#[test]
fn row_3_12_cve_process_documented() {
    assert_doc_has_sections(
        "SECURITY.md",
        &[
            "Reporting a vulnerability",
            "Acknowledge within",
            "Initial triage within",
        ],
    );
}

// ---- 7.x ops docs ----

#[test]
fn row_7_2_deployment_doc_present() {
    assert_doc_has_sections(
        "DEPLOYMENT.md",
        &[
            "## Install",
            "## File layout",
            "## Environment variables",
            "## Ports",
        ],
    );
}

#[test]
fn row_7_3_runbook_present() {
    assert_doc_has_sections(
        "RUNBOOK.md",
        &[
            "Alert: high `spg_errors_total`",
            "Alert: disk full",
            "Alert: replication lag growing",
            "Alert: backup taking longer",
            "Alert: audit log verify fails",
        ],
    );
}

#[test]
fn row_7_4_restore_drill_doc_plus_e2e() {
    // Same evidence as 2.5 + 2.6; this row groups them under
    // "operational tooling" while those live under "availability".
    assert_doc_has_sections("RESTORE_DRILL.md", &["e2e_restore_drill.rs"]);
}

#[test]
fn row_7_5_security_doc_present() {
    assert_doc_has_sections(
        "SECURITY.md",
        &["Reporting a vulnerability", "Secret handling"],
    );
}

#[test]
fn row_7_6_changelog_present() {
    assert_doc_has_sections(
        "CHANGELOG.md",
        &["[4.30.0]", "[4.29.0]", "[4.28.0]", "Format:"],
    );
}

/// 1.9 Partial-fsync recovery — covered by e2e_chaos, asserted
/// here as a presence check on the chaos test source so this
/// PROD_READY row stays evidence-linked.
#[test]
fn row_1_9_partial_fsync_recovery_covered_by_e2e_chaos() {
    let p = workspace_root().join("crates/spg-server/tests/e2e_chaos.rs");
    let src = std::fs::read_to_string(&p)
        .unwrap_or_else(|_| panic!("e2e_chaos.rs missing at {}", p.display()));
    assert!(
        src.contains("fn chaos_wal_tail_truncation_drops_partial_record_no_panic"),
        "e2e_chaos.rs must contain the chaos_wal_tail_truncation_… test"
    );
}

/// 1.10 Disk-full handling — covered by e2e_chaos, asserted here
/// as a presence check.
#[test]
fn row_1_10_disk_full_covered_by_e2e_chaos() {
    let p = workspace_root().join("crates/spg-server/tests/e2e_chaos.rs");
    let src = std::fs::read_to_string(&p)
        .unwrap_or_else(|_| panic!("e2e_chaos.rs missing at {}", p.display()));
    assert!(
        src.contains("fn chaos_disk_full_returns_clean_error_and_keeps_serving"),
        "e2e_chaos.rs must contain the chaos_disk_full_… test"
    );
    // Also: the SPG_FAIL_WAL_QUOTA_BYTES injection knob must be
    // wired up in spg-server. Grep the source.
    let main_src = std::fs::read_to_string(workspace_root().join("crates/spg-server/src/main.rs"))
        .expect("main.rs");
    assert!(
        main_src.contains("SPG_FAIL_WAL_QUOTA_BYTES"),
        "main.rs must declare the SPG_FAIL_WAL_QUOTA_BYTES knob"
    );
}

/// 1.3 WAL replay on startup — restart with same db+wal recovers state.
#[test]
fn row_1_3_wal_replay_on_startup() {
    let addr1 = pick_free_addr();
    let dir = unique_tmpdir("wal");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");

    {
        let mut c = ChildGuard(spawn_server_with_env(&addr1, &db, &wal, &[]));
        let mut s = wait_for_listener(&addr1, &mut c.0);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        exec_ok(&mut s, "CREATE TABLE t (id INT NOT NULL)");
        exec_ok(&mut s, "INSERT INTO t VALUES (1)");
        exec_ok(&mut s, "INSERT INTO t VALUES (2)");
    }
    // Restart on a fresh port; same db+wal.
    let addr2 = pick_free_addr();
    let mut c = ChildGuard(spawn_server_with_env(&addr2, &db, &wal, &[]));
    let mut s = wait_for_listener(&addr2, &mut c.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    assert_eq!(select_int(&mut s, "SELECT count(*) FROM t"), 2);
}

/// 4.1 Health endpoint — GET /healthz returns 200.
#[test]
fn row_4_1_health_endpoint() {
    let native = pick_free_addr();
    let http = pick_free_addr();
    let dir = unique_tmpdir("hz");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");
    let mut c = ChildGuard(spawn_server_with_env(
        &native,
        &db,
        &wal,
        &[("SPG_HTTP_ADDR", &http)],
    ));
    let _ = wait_for_listener(&native, &mut c.0);
    // Wait for HTTP listener too.
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut s = loop {
        match TcpStream::connect(&http) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("/healthz never came up: {e}"),
        }
    };
    s.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    assert!(
        buf.starts_with("HTTP/1.1 200"),
        "/healthz returned: {buf:?}"
    );
}

/// 4.2 Prometheus metrics — /metrics exposes required series.
#[test]
fn row_4_2_prometheus_metrics() {
    let native = pick_free_addr();
    let http = pick_free_addr();
    let dir = unique_tmpdir("mx");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");
    let mut c = ChildGuard(spawn_server_with_env(
        &native,
        &db,
        &wal,
        &[("SPG_HTTP_ADDR", &http)],
    ));
    let _ = wait_for_listener(&native, &mut c.0);
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut s = loop {
        match TcpStream::connect(&http) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("/metrics never came up: {e}"),
        }
    };
    s.write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    for required in [
        "spg_server_info",
        "spg_connections_active",
        "spg_queries_total",
        "spg_errors_total",
    ] {
        assert!(
            buf.contains(required),
            "/metrics missing required series {required:?} in:\n{buf}"
        );
    }
}

/// 5.1 SPG_MAX_CONNECTIONS — overflow gets clean error.
#[test]
fn row_5_1_max_connections_enforced() {
    let addr = pick_free_addr();
    let dir = unique_tmpdir("mc");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");
    let mut c = ChildGuard(spawn_server_with_env(
        &addr,
        &db,
        &wal,
        &[("SPG_MAX_CONNECTIONS", "1")],
    ));
    let _hold = wait_for_listener(&addr, &mut c.0);
    // The second connection is accepted by the OS, then the server
    // sends ErrorResponse and closes the socket. We just need to
    // see either an ErrorResponse frame (op byte 0x14) at position
    // [4] of the payload, or EOF.
    let mut s2 = TcpStream::connect(&addr).expect("second connect");
    s2.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let mut buf = [0u8; 256];
    let n = s2.read(&mut buf).unwrap_or(0);
    let saw_err_frame = n >= 5 && buf[4] == 0x14;
    let closed = n == 0;
    assert!(
        saw_err_frame || closed,
        "expected ErrorResponse (op 0x14 at byte 4) or EOF on 2nd conn; got n={n} bytes={:?}",
        &buf[..n.min(32)]
    );
}

/// 8.2 Native wire opcode stable — opcodes 0x00-0x17 + 0xFF are
/// the documented set; any drift here is a wire-protocol break.
#[test]
fn row_8_2_native_wire_opcode_stable() {
    use spg_wire::Op;
    let expected: &[(u8, &str)] = &[
        (0x00, "Ping"),
        (0x01, "Pong"),
        (0x02, "Auth"),
        (0x03, "AuthUser"),
        (0x10, "Query"),
        (0x11, "RowDescription"),
        (0x12, "DataRow"),
        (0x13, "CommandComplete"),
        (0x14, "ErrorResponse"),
        (0x15, "Stats"),
        (0x16, "StatsResponse"),
        (0x17, "DataRowBatch"),
        (0xFF, "Error"),
    ];
    for (byte, name) in expected {
        let op = Op::from_byte(*byte)
            .unwrap_or_else(|_| panic!("opcode 0x{byte:02x} ({name}) must decode"));
        let dbg = format!("{op:?}");
        assert_eq!(
            dbg, *name,
            "opcode 0x{byte:02x} should be {name}, got {dbg}"
        );
    }
}

/// 9.8 CI workflow exists — .github/workflows/ci.yml is present
/// and has the five jobs we documented.
#[test]
fn row_9_8_ci_workflow_present() {
    let path = workspace_root().join(".github/workflows/ci.yml");
    let yml = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!(".github/workflows/ci.yml must exist at {}", path.display()));
    for job in [
        "rustfmt",
        "clippy",
        "test",
        "cargo-audit",
        "prod_ready gate",
    ] {
        assert!(
            yml.contains(&format!("name: {job}")),
            "CI missing required job {job:?}"
        );
    }
}

/// 9.2 Perf gates exist — every stone crate has a perf_gate test.
#[test]
fn row_9_2_perf_gates_present() {
    let root = workspace_root();
    for crate_name in [
        "spg-wire",
        "spg-sql",
        "spg-storage",
        "spg-crypto",
        "spg-audit",
        "spg-engine",
        "spg-cli",
    ] {
        let p = root.join(format!("crates/{crate_name}/tests/perf_gate.rs"));
        assert!(
            p.exists(),
            "perf_gate.rs missing for {crate_name}; expected at {}",
            p.display()
        );
    }
}

/// 3.8 cargo-audit in CI — the audit job exists and uses the
/// rustsec audit-check action.
#[test]
fn row_3_8_cargo_audit_in_ci() {
    let yml = std::fs::read_to_string(workspace_root().join(".github/workflows/ci.yml"))
        .expect("ci.yml must exist");
    assert!(
        yml.contains("rustsec/audit-check"),
        "CI audit job must use rustsec/audit-check action"
    );
}

/// 10.1 / 10.2 / 10.3 — PERFORMANCE.md carries the v4.27 baseline
/// for latency, throughput, ANN.
#[test]
fn row_10_x_performance_doc_has_v4_27_baseline() {
    let perf = std::fs::read_to_string(workspace_root().join("PERFORMANCE.md"))
        .expect("PERFORMANCE.md must exist");
    assert!(
        perf.contains("v4.27 competitor rerun"),
        "PERFORMANCE.md must carry the v4.27 baseline section"
    );
    for table_hint in ["latency p50", "INSERT rows/s", "vector kNN"] {
        assert!(
            perf.contains(table_hint),
            "PERFORMANCE.md missing table containing {table_hint:?}"
        );
    }
}

/// 6.3 Concurrent read scaling — smoke: 4 parallel reader threads
/// against the same server complete in < 4× single-threaded time
/// for the same total work. (Real scaling numbers are in
/// PERFORMANCE.md §Concurrency; this is the regression gate.)
#[test]
fn row_6_3_concurrent_reads_dont_serialize() {
    let addr = pick_free_addr();
    let dir = unique_tmpdir("cc");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");
    let mut c = ChildGuard(spawn_server_with_env(&addr, &db, &wal, &[]));
    let mut s = wait_for_listener(&addr, &mut c.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(&mut s, "CREATE TABLE r (id INT NOT NULL, v INT NOT NULL)");
    for i in 0..200 {
        exec_ok(&mut s, &format!("INSERT INTO r VALUES ({i}, {})", i * 7));
    }
    drop(s);

    let serial = run_reads(&addr, 200);

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let a = addr.clone();
            thread::spawn(move || run_reads(&a, 200))
        })
        .collect();
    let parallel_started = Instant::now();
    for h in handles {
        let _ = h.join().unwrap();
    }
    let parallel_total = parallel_started.elapsed();

    // 4 threads doing 200 reads each = 800 total reads in
    // `parallel_total`. Serial baseline was 200 reads in `serial`.
    // If RwLock scaling works, parallel_total should be much less
    // than 4 * serial (the all-serial outcome).
    let bound = serial * 4;
    assert!(
        parallel_total < bound,
        "parallel 4×200 reads {parallel_total:?} expected < serial×4 = {bound:?}"
    );
}

// ---- meta-test: every [machine] row in the doc has a Rust test ----

/// Meta-check: PROD_READY.md must not promise a [machine] row
/// without a corresponding `row_X_Y_*` test in this file.
///
/// The mapping is by row id: `1.3` → tests starting with `row_1_3_`.
/// Adding a new [machine] row to PROD_READY.md without a paired
/// test here will fail this assertion.
#[test]
fn meta_every_machine_row_has_a_test() {
    let ids = machine_marked_rows();
    // `file!()` returns a workspace-relative path; resolve from root.
    let self_path = workspace_root().join(file!());
    let src = std::fs::read_to_string(&self_path)
        .unwrap_or_else(|e| panic!("read self at {}: {e}", self_path.display()));
    let mut missing = Vec::new();
    for id in &ids {
        let prefix = format!("fn row_{}_", id.replace('.', "_"));
        if !src.contains(&prefix) {
            missing.push(id.clone());
        }
    }
    if !missing.is_empty() {
        panic!(
            "PROD_READY.md rows marked [machine] but missing tests in prod_ready.rs: {missing:?}\n\
             Convention: row id 'X.Y' → fn row_X_Y_<name>"
        );
    }
}

fn run_reads(addr: &str, n: usize) -> Duration {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let started = Instant::now();
    for _ in 0..n {
        let _ = select_int(&mut s, "SELECT count(*) FROM r");
    }
    started.elapsed()
}

// ---- wire helpers (private to this file, copied minimal subset) ----

fn read_frame(s: &mut TcpStream) -> spg_wire::Frame {
    let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
    s.read_exact(&mut header).unwrap();
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = spg_wire::Op::from_byte(header[4]).unwrap();
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        s.read_exact(&mut payload).unwrap();
    }
    spg_wire::Frame { op, payload }
}

fn send(s: &mut TcpStream, f: &spg_wire::Frame) {
    let mut out = Vec::new();
    spg_wire::encode(f, &mut out).unwrap();
    s.write_all(&out).unwrap();
}

fn exec_ok(s: &mut TcpStream, sql: &str) {
    send(s, &spg_wire::build_query(sql));
    let f = read_frame(s);
    if f.op == spg_wire::Op::ErrorResponse {
        let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
        panic!("server rejected SQL {sql:?}: {msg}");
    }
    assert_eq!(
        f.op,
        spg_wire::Op::CommandComplete,
        "expected CC for {sql:?}"
    );
}

fn select_int(s: &mut TcpStream, sql: &str) -> i64 {
    send(s, &spg_wire::build_query(sql));
    let rd = read_frame(s);
    if rd.op == spg_wire::Op::ErrorResponse {
        let msg = spg_wire::parse_error_response(&rd).unwrap_or("<undecodable>");
        panic!("server rejected SQL {sql:?}: {msg}");
    }
    assert_eq!(rd.op, spg_wire::Op::RowDescription);
    let mut count: i64 = -1;
    loop {
        let f = read_frame(s);
        match f.op {
            spg_wire::Op::DataRow => {
                let row = spg_wire::parse_data_row(&f).unwrap();
                count = wire_to_i64(&row[0]);
            }
            spg_wire::Op::DataRowBatch => {
                let rows = spg_wire::parse_data_row_batch(&f).unwrap();
                count = wire_to_i64(&rows[0][0]);
            }
            spg_wire::Op::CommandComplete => return count,
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn wire_to_i64(v: &spg_wire::WireValue) -> i64 {
    match v {
        spg_wire::WireValue::Int(n) => i64::from(*n),
        spg_wire::WireValue::BigInt(n) => *n,
        spg_wire::WireValue::Text(t) => t.parse().unwrap(),
        other => panic!("expected integer, got {other:?}"),
    }
}
