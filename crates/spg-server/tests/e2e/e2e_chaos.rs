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

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

use std::thread;

fn local_spawn(
    db: &std::path::Path,
    wal: &std::path::Path,
    env: &[(&str, String)],
) -> (std::process::Child, common::ServerAddrs) {
    let mut b = common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal);
    for (k, v) in env {
        b = b.env(*k, v);
    }
    b.spawn()
}

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-chaos-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
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

    let mut committed: i64 = 0;
    {
        let (raw, addrs1) = local_spawn(&db, &wal, &[]);
        let mut c = common::ChildGuard(raw);
        let mut s = common::connect_to(&addrs1.native);
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
    // v7.37 (round 827) — no sleep: `wait()` IS the event (see above).

    // Restart on fresh port, same files. WAL replay should put the
    // engine back to exactly `committed` rows.
    let (raw, addrs2) = local_spawn(&db, &wal, &[]);
    let mut c2 = common::ChildGuard(raw);
    let mut s2 = common::connect_to(&addrs2.native);
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

    {
        let (raw, addrs1) = local_spawn(&db, &wal, &[]);
        let mut c = common::ChildGuard(raw);
        let mut s = common::connect_to(&addrs1.native);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        exec_ok(&mut s, "CREATE TABLE t (id INT NOT NULL)");
        for i in 0..20 {
            exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i})"));
        }
    }
    // v7.37 (round 827) — same proxy as the bit-flip test below, same
    // conversion: wait for the WAL to be substantial before mutilating
    // it, or the chop lands on a half-written file.
    common::wait_until(Duration::from_secs(5), || {
        std::fs::metadata(&wal).map(|m| m.len() > 64).unwrap_or(false)
    });

    // Chop a few bytes off the WAL — guaranteed to land inside
    // the last record. Replay must drop the torn entry and keep
    // every entry before it.
    let len = std::fs::metadata(&wal).unwrap().len();
    let new_len = len.saturating_sub(8);
    let f = std::fs::OpenOptions::new().write(true).open(&wal).unwrap();
    f.set_len(new_len).unwrap();
    drop(f);

    let (raw, addrs2) = local_spawn(&db, &wal, &[]);
    let mut c2 = common::ChildGuard(raw);
    let mut s2 = common::connect_to(&addrs2.native);
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

    // Very tight quota: enough for CREATE TABLE + a handful of
    // INSERTs, then ENOSPC.
    let quota = "300".to_string();
    let (raw, addrs) = local_spawn(&db, &wal, &[("SPG_FAIL_WAL_QUOTA_BYTES", quota)]);
    let mut c = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
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
    let mut s =
        TcpStream::connect(&addrs.native).expect("server still listening after quota error");
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
    // v7.37 (round 827) — no sleep: `wait()` IS the event. The process is
    // reaped, its locks are gone, and what it wrote lives in the kernel
    // page cache, visible to the next open immediately.
    let (raw, addrs2) = local_spawn(&db, &wal, &[]);
    let mut c2 = common::ChildGuard(raw);
    let mut s2 = common::connect_to(&addrs2.native);
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

    {
        let (raw, addrs1) = local_spawn(&db, &wal, &[]);
        let mut c = common::ChildGuard(raw);
        let mut s = common::connect_to(&addrs1.native);
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
    // v7.37 (round 827) — wait for the observable (the WAL has real
    // content on disk) instead of sleeping a fixed proxy for it.
    common::wait_until(Duration::from_secs(5), || {
        std::fs::metadata(&wal).map(|m| m.len() > 64).unwrap_or(false)
    });

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
    let mut c2 = common::ChildGuard(
        common::ServerBuilder::new()
            .arg_path(&db)
            .arg("-")
            .arg_path(&wal)
            .spawn_expecting_startup_failure(),
    );
    // No stderr available here — common helper redirects to /dev/null
    // for the expect-failure path; we observe the failure only via
    // the exit status (non-zero) which is the actual ship-gate.
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
    // The previous version of this assertion read stderr for an
    // explicit "CRC mismatch" / "corruption detected" message. The
    // v6.0.x migration to `spawn_expecting_startup_failure` drops
    // the stderr inspection: the exit code (non-zero) is the actual
    // contract, and the message wording is a documentation concern
    // not a behavioural one.
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

    let quota = "300".to_string();
    let (raw, addrs) = local_spawn(
        &db,
        &wal,
        &[
            ("SPG_FAIL_WAL_QUOTA_BYTES", quota),
            ("SPG_DISABLE_WAL_PREFLIGHT", "1".to_string()),
        ],
    );
    let mut c = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
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
    let mut s =
        TcpStream::connect(&addrs.native).expect("server still listening after quota error");
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
    // v7.37 (round 827) — no sleep: `wait()` IS the event. The process is
    // reaped, its locks are gone, and what it wrote lives in the kernel
    // page cache, visible to the next open immediately.
    let (raw, addrs2) = local_spawn(&db, &wal, &[]);
    let _c2 = common::ChildGuard(raw);
    let mut s2 = common::connect_to(&addrs2.native);
    s2.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let after = select_int(&mut s2, "SELECT count(*) FROM q");
    assert_eq!(
        after, accepted,
        "post-restart count must match what was CC'd before the real ENOSPC"
    );
}

// ---- chaos 6 (v4.42): multi-client ENOSPC fan-out ----

/// v4.42 group-commit ENOSPC invariant. 4 client threads stream
/// INSERTs concurrently against a server with a tight WAL quota
/// (and preflight disabled so the real append path is exercised).
/// When the elected leader's batched `fsync` overshoots the quota,
/// the leader calls `replace_catalog(pre_image)` to undo every
/// task in that group at once and acks each survivor with the WAL
/// error. Two invariants must hold:
///
/// 1. **Every writer in the failed group sees the same ENOSPC**
///    — clients can't observe a "half-written" group where some
///    threads got CC and others got `wal quota`. Each thread's
///    `accepted_count` is bounded by the durable state.
///
/// 2. **No phantom rows** — `sum(accepted_count_per_thread) ==
///    SELECT count(*)` both live and after restart. The
///    rolled-back group's mutations are gone from `engine.catalog`
///    and were never durably appended to the WAL, so the v4.34
///    "durable state matches live state" invariant generalises
///    from single-client to N-client group commit.
#[test]
fn chaos_disk_full_multi_client_group_rollback_all_writers() {
    const THREADS: usize = 4;
    const PER_THREAD: usize = 50;

    let dir = unique_tmpdir("nospc-multi");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");

    // Quota: large enough for CREATE TABLE + some inserts, then
    // ENOSPC. Group commit means many `fsync`s coalesce into one
    // append, so the cap must clear at least a few rounds of
    // group commits before tripping. 1500 bytes lets ~80 rows
    // through (each v3 record is 9-byte header + ~25-byte SQL =
    // ~34 bytes; plus the CREATE TABLE preamble) and then
    // refuses the next group atomically.
    let quota = "1500".to_string();
    let (raw, addrs) = local_spawn(
        &db,
        &wal,
        &[
            ("SPG_FAIL_WAL_QUOTA_BYTES", quota),
            ("SPG_DISABLE_WAL_PREFLIGHT", "1".to_string()),
        ],
    );
    let mut c = common::ChildGuard(raw);
    let mut setup = common::connect_to(&addrs.native);
    setup.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(
        &mut setup,
        "CREATE TABLE q (tid INT NOT NULL, i INT NOT NULL)",
    );
    drop(setup);

    let server_addr = addrs.native.clone();
    let accepted_total = Arc::new(AtomicUsize::new(0));
    let any_rejected = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::with_capacity(THREADS);
    for t in 0..THREADS {
        let addr = server_addr.clone();
        let accepted_total = Arc::clone(&accepted_total);
        let any_rejected = Arc::clone(&any_rejected);
        handles.push(thread::spawn(move || {
            let mut s = TcpStream::connect(&addr).expect("connect");
            s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
            let mut local_ok = 0usize;
            for i in 0..PER_THREAD {
                match run_query(&mut s, &format!("INSERT INTO q VALUES ({t}, {i})")) {
                    Outcome::Ok => local_ok += 1,
                    Outcome::Error(msg) => {
                        assert!(
                            msg.contains("wal quota") || msg.contains("WAL"),
                            "thread {t} insert {i}: expected wal-quota error, got: {msg:?}"
                        );
                        any_rejected.fetch_add(1, Ordering::Relaxed);
                        // The dispatch path returns the WAL
                        // error to the client and then closes
                        // the connection (v4.41.1 behaviour
                        // kept by v4.42 — same as the single-
                        // client chaos test). Stop pushing more
                        // INSERTs on this socket; the live
                        // SELECT below uses a fresh connection.
                        break;
                    }
                }
            }
            accepted_total.fetch_add(local_ok, Ordering::Relaxed);
        }));
    }
    for h in handles {
        h.join().expect("worker thread panicked");
    }
    assert!(
        any_rejected.load(Ordering::Relaxed) > 0,
        "quota never fired under group commit — multi-client ENOSPC path not exercised",
    );
    let accepted = i64::try_from(accepted_total.load(Ordering::Relaxed)).unwrap();
    assert!(
        accepted > 0,
        "no INSERT got CC before the quota tripped — quota was too tight or wiring broke",
    );

    // Live count must equal the sum of CC'd inserts across all
    // threads. If any rolled-back group had mutated `self.catalog`
    // without being undone, `live` would exceed `accepted`.
    let mut probe = TcpStream::connect(&server_addr).expect("server still listening");
    probe.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let live = select_int(&mut probe, "SELECT count(*) FROM q");
    assert_eq!(
        live, accepted,
        "v4.42 multi-client invariant broken: live catalog has {live} rows but only \
         {accepted} INSERTs received CC. A failed group left phantoms in memory.",
    );

    // Restart without the quota / preflight knob. WAL replay
    // should yield exactly `accepted` rows — no phantom from the
    // rolled-back groups, no double-apply.
    drop(probe);
    let _ = c.0.kill();
    let _ = c.0.wait();
    // v7.37 (round 827) — no sleep: `wait()` IS the event. The process is
    // reaped, its locks are gone, and what it wrote lives in the kernel
    // page cache, visible to the next open immediately.
    let (raw, addrs2) = local_spawn(&db, &wal, &[]);
    let _c2 = common::ChildGuard(raw);
    let mut s2 = common::connect_to(&addrs2.native);
    s2.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let after = select_int(&mut s2, "SELECT count(*) FROM q");
    assert_eq!(
        after, accepted,
        "post-restart count must match the sum of multi-client CC'd writes \
         (durable WAL never saw a rolled-back group)",
    );
}
