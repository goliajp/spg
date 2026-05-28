#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::manual_assert,
    clippy::uninlined_format_args,
    clippy::unreadable_literal
)]

//! v5.3.1 — CatalogManifest end-to-end through the server lifecycle.
//!
//! The freezer (v5.2.2) writes cold-tier segments to
//! `<db>.spg/segments/seg_<id>.spg`; before v5.3.1 those files were
//! only reachable on the next boot via an operator-supplied
//! `SPG_PRELOAD_COLD_SEGMENT` env var. v5.3.1 attaches a sidecar
//! manifest (`<db>.spg/manifest.v10`) to every snapshot write that
//! records each segment's path + CRC32 alongside the snapshot's own
//! CRC32, so restart auto-loads them.
//!
//! This test exercises the full path: insert + freeze (segments
//! land on disk + manifest captures them via the per-statement
//! snapshot in no-WAL mode) → kill server → restart with freezer
//! disabled → confirm cold_segments_total > 0 and the frozen PKs
//! still resolve via SQL.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{
    Frame, Op, WireValue, build_auth_user, build_query, encode, parse_data_row,
    parse_data_row_batch,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const FREEZE_DEADLINE: Duration = Duration::from_secs(10);

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
    let p = std::env::temp_dir().join(format!("spg-manifest-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn_server(addr: &str, db: &Path, http_addr: Option<&str>, env: &[(&str, String)]) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
    cmd.arg(addr)
        .arg(db)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD")
        .env_remove("SPG_PG_ADDR")
        .env_remove("SPG_FREEZER_DISABLE");
    if let Some(h) = http_addr {
        cmd.env("SPG_HTTP_ADDR", h);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.spawn().unwrap()
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn read_frame(s: &mut TcpStream) -> Frame {
    let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
    s.read_exact(&mut header).unwrap();
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).unwrap();
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        s.read_exact(&mut payload).unwrap();
    }
    Frame { op, payload }
}

fn send(s: &mut TcpStream, f: &Frame) {
    let mut out = Vec::new();
    encode(f, &mut out).unwrap();
    s.write_all(&out).unwrap();
}

fn exec_ok(s: &mut TcpStream, sql: &str) {
    send(s, &build_query(sql));
    loop {
        let f = read_frame(s);
        match f.op {
            Op::CommandComplete => return,
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
                panic!("server rejected {sql:?}: {msg}");
            }
            _ => {}
        }
    }
}

fn select_int(s: &mut TcpStream, sql: &str) -> i64 {
    send(s, &build_query(sql));
    let rd = read_frame(s);
    if rd.op == Op::ErrorResponse {
        let msg = spg_wire::parse_error_response(&rd).unwrap_or("<undecodable>");
        panic!("server rejected {sql:?}: {msg}");
    }
    assert_eq!(rd.op, Op::RowDescription);
    let mut got: Option<WireValue> = None;
    loop {
        let f = read_frame(s);
        match f.op {
            Op::DataRow => got = parse_data_row(&f).unwrap().into_iter().next(),
            Op::DataRowBatch => {
                got = parse_data_row_batch(&f)
                    .unwrap()
                    .into_iter()
                    .next()
                    .and_then(|r| r.into_iter().next());
            }
            Op::CommandComplete => {
                let v = got.expect("no row");
                return match v {
                    WireValue::Int(n) => i64::from(n),
                    WireValue::BigInt(n) => n,
                    WireValue::Text(t) => t.parse().unwrap(),
                    other => panic!("expected integer, got {other:?}"),
                };
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn http_get(addr: &str, path: &str) -> (u16, String) {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut stream = loop {
        if let Ok(s) = TcpStream::connect(addr) {
            break s;
        }
        assert!(
            Instant::now() <= deadline,
            "http listener at {addr} never came up"
        );
        thread::sleep(Duration::from_millis(20));
    };
    stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();
    let response = String::from_utf8_lossy(&buf).to_string();
    let (_status_line, rest) = response.split_once("\r\n").unwrap_or((&response, ""));
    let code: u16 = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = rest.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (code, body.to_string())
}

fn metric_value(body: &str, name: &str) -> Option<u64> {
    body.lines()
        .find(|l| l.starts_with(&format!("{name} ")))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
}

/// Full round-trip: server with WAL off + a tight freezer budget,
/// freeze writes cold segments + the manifest captures them via
/// each per-statement snapshot, kill, restart with freezer disabled,
/// manifest auto-loads the cold segments + frozen PKs still resolve
/// via SQL.
#[test]
fn manifest_restores_cold_segments_across_restart() {
    let dir = unique_tmpdir("roundtrip");
    let db = dir.join("a.db");
    let manifest_path = dir.join("a.spg").join("manifest.v10");

    let addr1 = pick_free_addr();
    let http1 = pick_free_addr();
    let env: Vec<(&str, String)> = vec![
        ("SPG_HOT_TIER_BYTES", "512".to_string()),
        ("SPG_FREEZER_TICK_MS", "50".to_string()),
        ("SPG_FREEZER_BATCH_ROWS", "4".to_string()),
    ];

    // ---- session 1: insert, freeze, ensure manifest captured ----
    let frozen_pks_to_probe: Vec<i64>;
    {
        let mut c = ChildGuard(spawn_server(&addr1, &db, Some(&http1), &env));
        let mut s = wait_for_listener(&addr1, &mut c.0);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

        exec_ok(
            &mut s,
            "CREATE TABLE big (id BIGINT NOT NULL, name TEXT NOT NULL)",
        );
        exec_ok(&mut s, "CREATE INDEX by_id ON big (id)");
        for i in 0..40i64 {
            exec_ok(&mut s, &format!("INSERT INTO big VALUES ({i}, 'u-{i}')"));
        }

        // Wait for the freezer to produce ≥1 cold segment (visible
        // on /metrics) and then for a follow-up insert to land —
        // that insert's snapshot is what writes the manifest with
        // the segment already registered. Without it the manifest
        // would still be the bootstrap (empty) one.
        let deadline = Instant::now() + FREEZE_DEADLINE;
        let mut cold: u64 = 0;
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
            let (_code, body) = http_get(&http1, "/metrics");
            cold = metric_value(&body, "spg_cold_segments_total").unwrap_or(0);
            if cold > 0 {
                break;
            }
        }
        assert!(cold > 0, "freezer never produced a cold segment");

        // One more insert to trigger another snapshot+manifest
        // write — this time with cold_segments populated.
        exec_ok(&mut s, "INSERT INTO big VALUES (100, 'post-freeze')");
        // Brief settle for the snapshot write to actually land.
        thread::sleep(Duration::from_millis(150));

        // Manifest must exist on disk now.
        assert!(
            manifest_path.exists(),
            "manifest not written at {}",
            manifest_path.display()
        );
        let manifest_bytes = std::fs::read(&manifest_path).unwrap();
        // Smallest viable manifest with 1 segment is well over 50
        // bytes; this asserts something landed.
        assert!(
            manifest_bytes.len() > 50,
            "manifest at {} suspiciously short ({} bytes)",
            manifest_path.display(),
            manifest_bytes.len()
        );

        // Pick a PK that's almost certainly cold (inserted early).
        frozen_pks_to_probe = vec![0, 1, 2, 3];
        // Kill — no graceful shutdown, simulating a crash.
        let _ = c.0.kill();
        let _ = c.0.wait();
    }
    thread::sleep(Duration::from_millis(200));

    // ---- session 2: same db file, freezer disabled, manifest preloads ----
    let addr2 = pick_free_addr();
    let http2 = pick_free_addr();
    let env2: Vec<(&str, String)> = vec![("SPG_FREEZER_DISABLE", "1".to_string())];
    let mut c2 = ChildGuard(spawn_server(&addr2, &db, Some(&http2), &env2));
    let mut s2 = wait_for_listener(&addr2, &mut c2.0);
    s2.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // Frozen PKs must resolve through the manifest-restored cold
    // tier — without the manifest auto-preload, every cold key
    // would return 0 rows.
    for id in &frozen_pks_to_probe {
        let got = select_int(
            &mut s2,
            &format!("SELECT count(*) FROM big WHERE id = {id}"),
        );
        assert_eq!(
            got, 1,
            "PK {id} must resolve post-restart via manifest preload (got {got})"
        );
    }

    // Post-freeze still-hot rows (we inserted id=100 after the freezer
    // fired) must also still resolve — through WAL replay.
    // Note: this session uses WAL-disabled mode so the WAL replay
    // skip path isn't exercised here; the contract is "manifest
    // brings cold segments back, snapshot+WAL handle the rest".
    let post = select_int(&mut s2, "SELECT count(*) FROM big WHERE id = 100");
    assert_eq!(post, 1, "post-freeze hot row also resolves post-restart");
}

// --- v5.3.2 CHECKPOINT + WAL truncate -----------------------------

/// Server runs with explicit admin password so CHECKPOINT (admin-
/// gated) is reachable. Returns the spawned child + a client socket
/// already authenticated as the admin.
fn spawn_server_with_admin(
    addr: &str,
    db: &Path,
    wal: &Path,
    http_addr: Option<&str>,
    extra_env: &[(&str, String)],
) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
    cmd.arg(addr)
        .arg(db)
        .arg("-")
        .arg(wal)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("SPG_ADMIN_PASSWORD", "adm-pw")
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_PG_ADDR")
        .env_remove("SPG_FREEZER_DISABLE");
    if let Some(h) = http_addr {
        cmd.env("SPG_HTTP_ADDR", h);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.spawn().unwrap()
}

fn auth_admin(s: &mut TcpStream) {
    send(s, &build_auth_user("admin", "adm-pw").unwrap());
    let f = read_frame(s);
    match f.op {
        Op::Pong | Op::CommandComplete => {}
        Op::ErrorResponse | Op::Error => {
            let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
            panic!("AUTH rejected: {msg}");
        }
        other => panic!("unexpected AUTH response: {other:?}"),
    }
}

/// `CHECKPOINT` writes a fresh snapshot, updates the manifest, and
/// truncates the WAL file to 0 bytes. The next boot reads the
/// manifest (auto-loads cold segments) and replays the empty WAL —
/// no work. Subsequent writes append to a fresh WAL.
#[test]
fn checkpoint_truncates_wal_and_persists_through_restart() {
    let dir = unique_tmpdir("checkpoint-truncate");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");
    let manifest_path = dir.join("a.spg").join("manifest.v10");

    let addr1 = pick_free_addr();
    let env: Vec<(&str, String)> = vec![
        ("SPG_HOT_TIER_BYTES", "512".to_string()),
        ("SPG_FREEZER_TICK_MS", "50".to_string()),
        ("SPG_FREEZER_BATCH_ROWS", "4".to_string()),
    ];

    let committed: i64;
    {
        let mut c = ChildGuard(spawn_server_with_admin(&addr1, &db, &wal, None, &env));
        let mut s = wait_for_listener(&addr1, &mut c.0);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        auth_admin(&mut s);

        exec_ok(
            &mut s,
            "CREATE TABLE big (id BIGINT NOT NULL, name TEXT NOT NULL)",
        );
        exec_ok(&mut s, "CREATE INDEX by_id ON big (id)");
        let mut written: i64 = 0;
        for i in 0..30i64 {
            exec_ok(&mut s, &format!("INSERT INTO big VALUES ({i}, 'u-{i}')"));
            written += 1;
        }
        committed = written;

        // Let the freezer fire so cold segments end up on disk.
        thread::sleep(Duration::from_millis(300));

        let wal_size_before = std::fs::metadata(&wal).unwrap().len();
        assert!(
            wal_size_before > 0,
            "WAL must have grown past 0 before CHECKPOINT"
        );

        exec_ok(&mut s, "CHECKPOINT");

        let wal_size_after = std::fs::metadata(&wal).unwrap().len();
        assert_eq!(
            wal_size_after, 0,
            "CHECKPOINT must truncate WAL to 0 (was {wal_size_before}, after {wal_size_after})"
        );
        assert!(
            manifest_path.exists(),
            "CHECKPOINT must leave a manifest at {}",
            manifest_path.display()
        );

        // Post-checkpoint INSERT to confirm the WAL is writable
        // again. This row gets WAL'd into a fresh file growing
        // from offset 0.
        exec_ok(&mut s, "INSERT INTO big VALUES (1000, 'post-cp')");
        let wal_size_post = std::fs::metadata(&wal).unwrap().len();
        assert!(
            wal_size_post > 0,
            "WAL must accept new appends after truncate (got {wal_size_post})"
        );

        let _ = c.0.kill();
        let _ = c.0.wait();
    }
    thread::sleep(Duration::from_millis(200));

    // Restart — admin password required because BACKUP/CHECKPOINT
    // bootstrap a user. Freezer disabled so the post-restart state
    // is what the manifest + WAL produce, no background tweaks.
    let addr2 = pick_free_addr();
    let mut c2 = ChildGuard(spawn_server_with_admin(
        &addr2,
        &db,
        &wal,
        None,
        &[("SPG_FREEZER_DISABLE", "1".to_string())],
    ));
    let mut s2 = wait_for_listener(&addr2, &mut c2.0);
    s2.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    auth_admin(&mut s2);

    // All 30 original rows + the post-checkpoint row = 31. Every
    // PK must resolve through some tier (the early ones through
    // the manifest-restored cold tier, the rest through snapshot
    // + WAL replay of the post-CHECKPOINT INSERT).
    for id in [0i64, committed / 2, committed - 1, 1000] {
        let got = select_int(
            &mut s2,
            &format!("SELECT count(*) FROM big WHERE id = {id}"),
        );
        assert_eq!(got, 1, "PK {id} must resolve post-restart");
    }
}

/// CHECKPOINT requires admin role. A bare client (no AUTH) gets
/// the same permission-denied surface as BACKUP / CREATE USER.
#[test]
fn checkpoint_rejects_non_admin_caller() {
    let dir = unique_tmpdir("checkpoint-rbac");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");
    let addr = pick_free_addr();
    let mut c = ChildGuard(spawn_server_with_admin(
        &addr,
        &db,
        &wal,
        None,
        &[("SPG_FREEZER_DISABLE", "1".to_string())],
    ));
    let mut s = wait_for_listener(&addr, &mut c.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    // Skip auth_admin — connect as anonymous. CHECKPOINT must be
    // rejected before the engine sees it.
    send(&mut s, &build_query("CHECKPOINT"));
    let f = read_frame(&mut s);
    assert!(
        matches!(f.op, Op::ErrorResponse | Op::Error),
        "expected error response, got {:?}",
        f.op
    );
}
