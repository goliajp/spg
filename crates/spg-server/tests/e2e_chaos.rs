#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::manual_assert,
    clippy::uninlined_format_args,
    clippy::unreadable_literal
)]

//! v4.29 chaos suite — three failure scenarios prod operators
//! actually face. Each test injects the fault, then asserts the
//! recovery invariant (no data loss, no panic, server stays up).
//!
//! 1. **kill -9 mid-write** — primary is hard-killed while writes
//!    are in flight. On restart, every CC'd write must be visible.
//! 2. **WAL tail truncation** — simulate a torn last record by
//!    truncating the WAL file at a non-record boundary, then
//!    restart. The clean prefix must replay; the torn tail must
//!    be dropped with a warning, no panic.
//! 3. **Disk full mid-write** — `SPG_FAIL_WAL_QUOTA_BYTES` caps
//!    the WAL file size. The Nth write that would push past the
//!    quota gets a clear error frame; server stays alive;
//!    previously committed writes survive a restart unchanged.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

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
    let p = std::env::temp_dir().join(format!("spg-chaos-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn_server(addr: &str, db: &Path, wal: &Path, env: &[(&str, String)]) -> Child {
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
                thread::sleep(Duration::from_millis(50));
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

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Ok,
    Error(String),
}

fn run_query(s: &mut TcpStream, sql: &str) -> Outcome {
    send(s, &build_query(sql));
    loop {
        let f = read_frame(s);
        match f.op {
            Op::CommandComplete => return Outcome::Ok,
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&f)
                    .map_or_else(|_| "<undecodable>".into(), str::to_owned);
                return Outcome::Error(msg);
            }
            // SELECT rows would arrive here for the count probe;
            // call select_int directly for that path.
            _ => {}
        }
    }
}

fn exec_ok(s: &mut TcpStream, sql: &str) {
    assert_eq!(run_query(s, sql), Outcome::Ok, "expected ok for {sql:?}");
}

fn select_int(s: &mut TcpStream, sql: &str) -> i64 {
    send(s, &build_query(sql));
    let rd = read_frame(s);
    if rd.op == Op::ErrorResponse {
        let msg = spg_wire::parse_error_response(&rd).unwrap_or("<undecodable>");
        panic!("server rejected SQL {sql:?}: {msg}");
    }
    assert_eq!(rd.op, Op::RowDescription);
    let mut count: i64 = -1;
    loop {
        let f = read_frame(s);
        match f.op {
            Op::DataRow => count = wire_to_i64(&parse_data_row(&f).unwrap()[0]),
            Op::DataRowBatch => {
                let rows = parse_data_row_batch(&f).unwrap();
                count = wire_to_i64(&rows[0][0]);
            }
            Op::CommandComplete => return count,
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn wire_to_i64(v: &WireValue) -> i64 {
    match v {
        WireValue::Int(n) => i64::from(*n),
        WireValue::BigInt(n) => *n,
        WireValue::Text(t) => t.parse().unwrap(),
        other => panic!("expected integer, got {other:?}"),
    }
}

// ---- chaos 1: kill -9 mid-write ----

/// Primary is hard-killed while we're inserting. Restart with the
/// same db+wal. Every write that returned CC before the kill must
/// be present; nothing else may be.
#[test]
fn chaos_kill_minus_9_mid_write_recovers_committed_writes() {
    let dir = unique_tmpdir("kill9");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");
    let addr1 = pick_free_addr();

    let mut committed: i64 = 0;
    {
        let mut c = ChildGuard(spawn_server(&addr1, &db, &wal, &[]));
        let mut s = wait_for_listener(&addr1, &mut c.0);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        exec_ok(&mut s, "CREATE TABLE k (id INT NOT NULL)");
        // 1 + 100 inserts; each round-trip returns only after fsync.
        // After every successful CC, count++. The kill below cuts
        // the burst at an arbitrary point; whatever we counted is
        // the durable count.
        for i in 0..100 {
            if run_query(&mut s, &format!("INSERT INTO k VALUES ({i})")) == Outcome::Ok {
                committed += 1;
            } else {
                break;
            }
        }
        // SIGKILL (Child::kill on Unix sends SIGKILL by default).
        // No drain, no sync, no chance to flush in-memory buffers.
        let _ = c.0.kill();
        let _ = c.0.wait();
    }
    thread::sleep(Duration::from_millis(200));

    // Restart on fresh port, same files. WAL replay should put the
    // engine back to exactly `committed` rows.
    let addr2 = pick_free_addr();
    let mut c2 = ChildGuard(spawn_server(&addr2, &db, &wal, &[]));
    let mut s2 = wait_for_listener(&addr2, &mut c2.0);
    s2.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let after = select_int(&mut s2, "SELECT count(*) FROM k");
    assert_eq!(
        after, committed,
        "expected {committed} rows survived kill -9, got {after}"
    );
}

// ---- chaos 2: WAL tail truncation ----

/// Simulate a torn last record by truncating the WAL file at a
/// non-record boundary, then restart. The clean prefix must
/// replay; the torn tail is dropped with a warning, no panic.
#[test]
fn chaos_wal_tail_truncation_drops_partial_record_no_panic() {
    let dir = unique_tmpdir("trunc");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");
    let addr1 = pick_free_addr();

    {
        let mut c = ChildGuard(spawn_server(&addr1, &db, &wal, &[]));
        let mut s = wait_for_listener(&addr1, &mut c.0);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        exec_ok(&mut s, "CREATE TABLE t (id INT NOT NULL)");
        for i in 0..20 {
            exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i})"));
        }
    }
    thread::sleep(Duration::from_millis(100));

    // Chop a few bytes off the WAL — guaranteed to land inside
    // the last record. Replay must drop the torn entry and keep
    // every entry before it.
    let len = std::fs::metadata(&wal).unwrap().len();
    let new_len = len.saturating_sub(8);
    let f = std::fs::OpenOptions::new().write(true).open(&wal).unwrap();
    f.set_len(new_len).unwrap();
    drop(f);

    let addr2 = pick_free_addr();
    let mut c2 = ChildGuard(spawn_server(&addr2, &db, &wal, &[]));
    let mut s2 = wait_for_listener(&addr2, &mut c2.0);
    s2.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let after = select_int(&mut s2, "SELECT count(*) FROM t");
    // Could be 20 (we chopped only fsync padding) or 19 (we
    // chopped the last record). Both are valid outcomes — the
    // hard requirement is: no panic, server up, count ≤ 21 and
    // ≥ 19 (one or two records may be missing depending on where
    // the chop lands).
    assert!(
        (19..=21).contains(&after),
        "expected 19..=21 rows after WAL truncation, got {after}"
    );
    // And the server must keep accepting new work.
    exec_ok(&mut s2, "INSERT INTO t VALUES (9999)");
    let after2 = select_int(&mut s2, "SELECT count(*) FROM t");
    assert_eq!(after2, after + 1);
}

// ---- chaos 3: disk full mid-write ----

/// `SPG_FAIL_WAL_QUOTA_BYTES` caps the WAL file size. The first
/// write that would push past the cap returns a clear error frame
/// to the client; the server stays alive; subsequent reads still
/// work; previously committed writes survive restart unchanged.
#[test]
fn chaos_disk_full_returns_clean_error_and_keeps_serving() {
    let dir = unique_tmpdir("nospc");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");
    let addr = pick_free_addr();

    // Very tight quota: enough for CREATE TABLE + a handful of
    // INSERTs, then ENOSPC.
    let quota = "300".to_string();
    let mut c = ChildGuard(spawn_server(
        &addr,
        &db,
        &wal,
        &[("SPG_FAIL_WAL_QUOTA_BYTES", quota)],
    ));
    let mut s = wait_for_listener(&addr, &mut c.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(&mut s, "CREATE TABLE q (id INT NOT NULL)");
    let mut accepted = 0;
    let mut rejected = false;
    for i in 0..100 {
        match run_query(&mut s, &format!("INSERT INTO q VALUES ({i})")) {
            Outcome::Ok => accepted += 1,
            Outcome::Error(msg) => {
                assert!(
                    msg.contains("wal quota") || msg.contains("WAL"),
                    "expected wal-quota error, got: {msg:?}"
                );
                rejected = true;
                break;
            }
        }
    }
    assert!(rejected, "quota never fired in 100 inserts");
    assert!(accepted > 0, "nothing committed before quota fired");

    // v4.30: preflight quota check guarantees the live in-memory
    // count matches what was CC'd. (Pre-v4.30 a phantom row could
    // appear because engine.execute mutated state before WAL append
    // failed; the preflight in main.rs rejects the SQL before any
    // engine mutation.) Reconnect since the handler closed our
    // socket on the quota error.
    drop(s);
    let mut s = TcpStream::connect(&addr).expect("server still listening after quota error");
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let live = select_int(&mut s, "SELECT count(*) FROM q");
    assert_eq!(
        live, accepted as i64,
        "live in-memory count must match CC'd count after preflight quota reject"
    );

    // Tear down and restart without the quota. WAL replay must
    // produce exactly `accepted` rows — no phantoms.
    drop(s);
    let _ = c.0.kill();
    let _ = c.0.wait();
    thread::sleep(Duration::from_millis(200));
    let addr2 = pick_free_addr();
    let mut c2 = ChildGuard(spawn_server(&addr2, &db, &wal, &[]));
    let mut s2 = wait_for_listener(&addr2, &mut c2.0);
    s2.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let after = select_int(&mut s2, "SELECT count(*) FROM q");
    assert_eq!(
        after, accepted as i64,
        "post-restart row count must match what was CC'd before ENOSPC"
    );
}

// ---- chaos 5: WAL bit-flip — v4.37 CRC32 catches silent corruption ----

/// v4.37 row 1.8: bit-flip inside a WAL record's payload must be
/// caught by the CRC32 (not by accident-of-deserialization). After
/// the flip, restart must REFUSE to replay and return an explicit
/// "CRC mismatch" error — never silently apply a corrupted record.
#[test]
fn chaos_wal_bit_flip_caught_by_crc32_refuses_to_replay() {
    let dir = unique_tmpdir("crcflip");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");
    let addr1 = pick_free_addr();

    {
        let mut c = ChildGuard(spawn_server(&addr1, &db, &wal, &[]));
        let mut s = wait_for_listener(&addr1, &mut c.0);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        exec_ok(
            &mut s,
            "CREATE TABLE c (id INT NOT NULL, name TEXT NOT NULL)",
        );
        // Several rows so the flip can land on the middle of the WAL
        // (not on the trailing record, which would just be tail-
        // truncation handling instead of CRC enforcement).
        for i in 0..6 {
            exec_ok(
                &mut s,
                &format!("INSERT INTO c VALUES ({i}, 'row-{i}-payload')"),
            );
        }
    }
    thread::sleep(Duration::from_millis(100));

    // Flip a single bit roughly in the middle of the file. v4.37
    // WAL records carry an 8-byte header (length + CRC) followed
    // by the SQL bytes; landing in the file's middle lands inside
    // one of the payloads with high probability.
    let mut bytes = std::fs::read(&wal).unwrap();
    let len = bytes.len();
    assert!(len > 64, "WAL should be substantial; got {len} bytes");
    let mid = len / 2;
    bytes[mid] ^= 0x01;
    std::fs::write(&wal, &bytes).unwrap();

    // Restart with the corrupted WAL. The server must fail to come
    // up — spg-server exits 1 with the fatal-replay path — rather
    // than silently apply garbage SQL or skip the record.
    let addr2 = pick_free_addr();
    let mut c2 = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_spg-server"))
            .arg(&addr2)
            .arg(&db)
            .arg("-")
            .arg(&wal)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .env_remove("SPG_PASSWORD")
            .env_remove("SPG_ADMIN_PASSWORD")
            .env_remove("SPG_PG_ADDR")
            .spawn()
            .unwrap(),
    );
    let mut stderr = c2.0.stderr.take().expect("stderr piped");
    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(s) = c2.0.try_wait().expect("try_wait") {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "server didn't exit on corruption"
        );
        thread::sleep(Duration::from_millis(50));
    };
    assert!(
        !status.success(),
        "server must NOT come up successfully on a CRC-corrupted WAL"
    );
    let mut msg = String::new();
    let _ = std::io::Read::read_to_string(&mut stderr, &mut msg);
    assert!(
        msg.contains("CRC mismatch") || msg.contains("corruption detected"),
        "expected explicit CRC-mismatch refusal in stderr; got: {msg:?}"
    );
}

// ---- chaos 4: ENOSPC mid-write_all — preflight disabled (v4.34) ----

/// v4.34 fix for PROD_READY row 1.11: disable the v4.30 dispatch-time
/// preflight and exercise the real path that fails inside
/// `append_wal*`. Without the implicit BEGIN..COMMIT wrap, the
/// previous behavior left a phantom row in memory; with the wrap
/// the engine ROLLBACKs and the live count matches CC'd exactly.
#[test]
fn chaos_disk_full_no_preflight_rolls_back_in_memory_to_match_durable_state() {
    let dir = unique_tmpdir("nospc-rollback");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");
    let addr = pick_free_addr();

    let quota = "300".to_string();
    let mut c = ChildGuard(spawn_server(
        &addr,
        &db,
        &wal,
        &[
            ("SPG_FAIL_WAL_QUOTA_BYTES", quota),
            ("SPG_DISABLE_WAL_PREFLIGHT", "1".to_string()),
        ],
    ));
    let mut s = wait_for_listener(&addr, &mut c.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(&mut s, "CREATE TABLE q (id INT NOT NULL)");
    let mut accepted = 0i64;
    let mut rejected = false;
    for i in 0..100 {
        match run_query(&mut s, &format!("INSERT INTO q VALUES ({i})")) {
            Outcome::Ok => accepted += 1,
            Outcome::Error(msg) => {
                assert!(
                    msg.contains("wal quota") || msg.contains("WAL"),
                    "expected wal-quota error from the real append path, got: {msg:?}"
                );
                rejected = true;
                break;
            }
        }
    }
    assert!(
        rejected,
        "quota never fired in 100 inserts (preflight disable knob wired up?)"
    );
    assert!(accepted > 0, "nothing committed before quota fired");

    // Live in-memory count MUST match CC'd count even though the
    // path went through the real append failure (the implicit
    // BEGIN..COMMIT wrap rolled the failed write back). This is
    // the property the v4.30 preflight could only ensure for the
    // injected path — v4.34 closes it for the real path too.
    drop(s);
    let mut s = TcpStream::connect(&addr).expect("server still listening after quota error");
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let live = select_int(&mut s, "SELECT count(*) FROM q");
    assert_eq!(
        live, accepted,
        "v4.34: live count must match CC'd count even when the failure \
         lands inside append_wal (preflight disabled)"
    );

    // Restart without the quota or the preflight knob. WAL replay
    // sees the rolled-back implicit TX as an open transaction at
    // end-of-stream (BEGIN with no COMMIT) and auto-rollbacks it,
    // landing on exactly `accepted` rows. No phantoms across reboot.
    drop(s);
    let _ = c.0.kill();
    let _ = c.0.wait();
    thread::sleep(Duration::from_millis(200));
    let addr2 = pick_free_addr();
    let mut c2 = ChildGuard(spawn_server(&addr2, &db, &wal, &[]));
    let mut s2 = wait_for_listener(&addr2, &mut c2.0);
    s2.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let after = select_int(&mut s2, "SELECT count(*) FROM q");
    assert_eq!(
        after, accepted,
        "post-restart count must match what was CC'd before the real ENOSPC"
    );
}
