#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::items_after_statements,
    clippy::manual_assert,
    clippy::uninlined_format_args,
    clippy::unreadable_literal
)]

//! v4.32 SLO smoke test — a fast bench-shaped sanity check that
//! the published SLO numbers in PERFORMANCE.md §SLO still hold.
//!
//! **Purpose**: catch regressions in the latency / throughput hot
//! path before they ship. Not a perf bench (those live in
//! `xbench/competitor/`); this just gates against gross drift.
//!
//! Budget: should take < 5 s on M-series 8-core. Numbers are
//! intentionally loose — set 2× the v4.27 baseline as the floor
//! so noise / shared CI runners don't false-alarm.
//!
//! If this test fails, look at the long-form numbers in
//! `xbench/competitor/src/bin/latency.rs` /
//! `throughput.rs` for the real story.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, build_query, encode};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// SLOs from PERFORMANCE.md §SLO (v4.32). Numbers in µs.
///
/// Loose p99 ceilings (~6-7× the measured v4.27 baseline of
/// 77/70 µs) so a shared/loaded CI runner doesn't false-alarm.
/// The bench harness in `xbench/competitor/` is the source of
/// truth for actual numbers; this gates only against gross
/// regression. Raising these ceilings requires a PR comment
/// explaining why the floor moved.
const SLO_SEL_P99_US: u128 = 500;
const SLO_INS_P99_US: u128 = 500;

/// v4.34 — WAL-on INSERT p99 ceiling for the implicit-BEGIN..COMMIT
/// wrap path. fsync per write dominates this number; the ceiling
/// has to absorb pathological host I/O contention (concurrent
/// builds, other test binaries fsync'ing the same volume) since
/// the SLO test is a CI gate, not a microbenchmark. The 1 s
/// ceiling still catches the regressions the wrap is most at risk
/// of — repeated catalog clones, multiple fsyncs per write, lost
/// batching — which would push p99 by orders of magnitude. Local
/// quiet-disk baseline sits ~20 ms p99 on APFS; the
/// `xbench/competitor/src/bin/latency.rs` harness is the source
/// of truth for actual numbers.
const SLO_WAL_INS_P99_US: u128 = 1_000_000;

fn pick_free_addr() -> String {
    let p = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = p.local_addr().unwrap();
    drop(p);
    a.to_string()
}

#[allow(dead_code)]
fn unique_tmpdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-slo-{nanos}"));
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

fn spawn(addr: &str) -> Child {
    // Pure in-memory: no db_path, no WAL. Matches the v4.27
    // latency bench's setup (xbench/competitor/src/bin/latency.rs)
    // so the SLO numbers compare apples-to-apples.
    Command::new(env!("CARGO_BIN_EXE_spg-server"))
        .arg(addr)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD")
        .env_remove("SPG_PG_ADDR")
        .env_remove("SPG_DB")
        .env_remove("SPG_WAL")
        .spawn()
        .unwrap()
}

fn wait_for_listener(addr: &str, child: &mut Child) -> TcpStream {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match TcpStream::connect(addr) {
            Ok(s) => {
                s.set_nodelay(true).ok();
                return s;
            }
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

fn round_trip(s: &mut TcpStream, sql: &str) {
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).unwrap();
    s.write_all(&out).unwrap();
    // Drain frames until CommandComplete.
    loop {
        let mut hdr = [0u8; spg_wire::FRAME_HEADER_LEN];
        s.read_exact(&mut hdr).unwrap();
        let plen = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
        let op = Op::from_byte(hdr[4]).unwrap();
        let mut payload = vec![0u8; plen];
        if plen > 0 {
            s.read_exact(&mut payload).unwrap();
        }
        match op {
            Op::CommandComplete => return,
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&Frame { op, payload })
                    .unwrap_or("<undecodable>")
                    .to_string();
                panic!("server rejected {sql:?}: {msg}");
            }
            _ => {}
        }
    }
}

fn p99(samples_us: &mut [u128]) -> u128 {
    samples_us.sort_unstable();
    let idx = ((samples_us.len() as f64) * 0.99) as usize;
    samples_us[idx.min(samples_us.len() - 1)]
}

#[test]
fn slo_smoke_select_and_insert_p99_under_budget() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn(&addr));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // Setup + warm-up.
    round_trip(&mut s, "CREATE TABLE slo (id INT NOT NULL, v INT NOT NULL)");
    for i in 0..200 {
        round_trip(&mut s, &format!("INSERT INTO slo VALUES ({i}, {})", i * 7));
    }
    // Warm-up.
    for _ in 0..200 {
        round_trip(&mut s, "SELECT count(*) FROM slo");
    }

    // Measure.
    const N: usize = 500;
    let mut sel = Vec::with_capacity(N);
    for _ in 0..N {
        let t = Instant::now();
        round_trip(&mut s, "SELECT count(*) FROM slo");
        sel.push(t.elapsed().as_micros());
    }

    let mut ins = Vec::with_capacity(N);
    for i in 1000..1000 + N {
        let t = Instant::now();
        round_trip(&mut s, &format!("INSERT INTO slo VALUES ({i}, {})", i * 7));
        ins.push(t.elapsed().as_micros());
    }

    let sel_p99 = p99(&mut sel);
    let ins_p99 = p99(&mut ins);
    eprintln!(
        "SLO smoke: SEL p99 = {sel_p99} µs (SLO ≤ {SLO_SEL_P99_US}) | INS p99 = {ins_p99} µs (SLO ≤ {SLO_INS_P99_US})"
    );

    assert!(
        sel_p99 <= SLO_SEL_P99_US,
        "SEL p99 {sel_p99} µs blew the SLO ceiling of {SLO_SEL_P99_US} µs — see PERFORMANCE.md §SLO and xbench/competitor/src/bin/latency.rs"
    );
    assert!(
        ins_p99 <= SLO_INS_P99_US,
        "INS p99 {ins_p99} µs blew the SLO ceiling of {SLO_INS_P99_US} µs — see PERFORMANCE.md §SLO and xbench/competitor/src/bin/latency.rs"
    );
}

fn spawn_wal(addr: &str, db: &std::path::Path, wal: &std::path::Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_spg-server"))
        .arg(addr)
        .arg(db)
        .arg("-")
        .arg(wal)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD")
        .env_remove("SPG_PG_ADDR")
        .spawn()
        .unwrap()
}

/// v4.34: perf gate for the implicit BEGIN..COMMIT wrap. Runs the
/// same shape of INSERTs as the in-memory smoke above, but against
/// a server with WAL enabled (so every write goes through the
/// wrap → atomic WAL block → COMMIT path).
///
/// The ceiling is set well above pure-disk fsync latency to keep
/// CI noise / shared-runner I/O contention from false-alarming. A
/// real regression in the wrap (e.g. an extra catalog clone, or a
/// missed batched fsync) would still blow it.
#[test]
fn slo_wal_insert_p99_under_budget() {
    let addr = pick_free_addr();
    let dir = unique_tmpdir();
    let db = dir.join("slo.db");
    let wal = dir.join("slo.wal");
    let mut child = ChildGuard(spawn_wal(&addr, &db, &wal));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    round_trip(
        &mut s,
        "CREATE TABLE slo_wal (id INT NOT NULL, v INT NOT NULL)",
    );
    // Warm-up: amortize first-write cost (catalog grow, pagecache
    // priming, fsync metadata journaling on cold FS).
    for i in 0..200 {
        round_trip(
            &mut s,
            &format!("INSERT INTO slo_wal VALUES ({i}, {})", i * 7),
        );
    }

    const N: usize = 200;
    let mut ins = Vec::with_capacity(N);
    for i in 1000..1000 + N {
        let t = Instant::now();
        round_trip(
            &mut s,
            &format!("INSERT INTO slo_wal VALUES ({i}, {})", i * 7),
        );
        ins.push(t.elapsed().as_micros());
    }
    let ins_p99 = p99(&mut ins);
    eprintln!("SLO smoke (WAL-on): INS p99 = {ins_p99} µs (ceiling ≤ {SLO_WAL_INS_P99_US})");
    assert!(
        ins_p99 <= SLO_WAL_INS_P99_US,
        "WAL-mode INS p99 {ins_p99} µs blew the v4.34 ceiling of {SLO_WAL_INS_P99_US} µs — \
         the implicit BEGIN..COMMIT wrap may have regressed; see PROD_READY row 1.11"
    );
}
