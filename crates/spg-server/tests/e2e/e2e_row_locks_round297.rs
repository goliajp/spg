//! v7.39 (round 297, E3 Phase 1b) — `FOR UPDATE` actually locks.
//!
//! SPG accepted the entire locking syntax and locked nothing. Two
//! workers running the classic `SKIP LOCKED` queue both took the same
//! row. Measured against live PG 18.4 with two sessions before the fix:
//! PG handed B row 2 while A held row 1; SPG handed B row 1.
//!
//! The lock manager was already complete (`spg-engine/src/locks.rs`:
//! PG's 4x4 conflict matrix, three wait policies, deadlock victim
//! selection, release wired into COMMIT/ROLLBACK). Nothing called it.
//!
//! Two things this round established the hard way, both recorded in
//! `.claude/state/e3-row-locks-rfc.md`:
//!
//!   * THREE layers routed around the lock table before the engine ever
//!     saw a locking SELECT — the streaming fast path, the `is_read`
//!     first-word classification, and dispatch ordering.
//!   * the exclusion has to ride the SNAPSHOT. Adding it at individual
//!     scan sites missed the live path three times running; the row
//!     sources are many (sequential, predicate, index seek, PK-ordered
//!     top-N, cold tier) but every one of them asks
//!     `Table::is_row_visible`. A row another transaction holds is, for
//!     this statement, exactly as unavailable as a row the snapshot
//!     cannot see — so it is the same test.
//!
//! These are wire-level because the defect lives in how the server
//! addresses the engine; two connections are the whole point.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-lock-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

struct PgMessage {
    ty: u8,
    body: Vec<u8>,
}

fn read_message(s: &mut TcpStream) -> PgMessage {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len.saturating_sub(4)];
    if !body.is_empty() {
        s.read_exact(&mut body).expect("body");
    }
    PgMessage { ty, body }
}

fn send_startup(s: &mut TcpStream, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.push(0);
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn read_until_ready(s: &mut TcpStream) -> Vec<PgMessage> {
    let mut out = Vec::new();
    loop {
        let m = read_message(s);
        let z = m.ty == b'Z';
        out.push(m);
        if z {
            return out;
        }
    }
}

fn datarow_cell(body: &[u8]) -> Option<String> {
    let cells = u16::from_be_bytes([body[0], body[1]]) as usize;
    if cells == 0 {
        return None;
    }
    let len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
    if len < 0 {
        return None;
    }
    let l = len as usize;
    Some(std::str::from_utf8(&body[6..6 + l]).unwrap().to_string())
}

/// Every first-column value the query returned, in order.
fn query_all(s: &mut TcpStream, sql: &str) -> Vec<String> {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    for m in &msgs {
        if m.ty == b'E' {
            let text: String = m
                .body
                .split(|b| *b == 0)
                .filter(|f| f.first() == Some(&b'M'))
                .map(|f| String::from_utf8_lossy(&f[1..]).into_owned())
                .collect();
            panic!("{sql}: unexpected ErrorResponse: {text}");
        }
    }
    msgs.iter()
        .filter(|m| m.ty == b'D')
        .filter_map(|m| datarow_cell(&m.body))
        .collect()
}

fn query_err(s: &mut TcpStream, sql: &str) -> Option<String> {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    // The ErrorResponse body is a run of NUL-terminated fields, each
    // tagged by its leading byte: `C` is the SQLSTATE, `M` the message.
    msgs.iter().find(|m| m.ty == b'E').map(|m| {
        let mut code = String::new();
        let mut text = String::new();
        for field in m.body.split(|b| *b == 0) {
            match field.first() {
                Some(b'C') => code = String::from_utf8_lossy(&field[1..]).into_owned(),
                Some(b'M') => text = String::from_utf8_lossy(&field[1..]).into_owned(),
                _ => {}
            }
        }
        format!("{code}|{text}")
    })
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "admin");
    let _ = read_until_ready(&mut s);
    s
}

/// A server plus a seeded `lk` table and a session A holding row 1.
fn boot_with_holder(label: &str) -> (common::ChildGuard, String, TcpStream) {
    let dir = unique_tmpdir(label);
    let db = dir.join("spg.db");
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .with_pgwire()
        .spawn();
    let addr = addrs.pgwire.as_ref().unwrap().clone();
    let mut a = open(&addr);
    query_all(&mut a, "CREATE TABLE lk (id int primary key, v int)");
    query_all(&mut a, "INSERT INTO lk VALUES (1,10),(2,20),(3,30),(4,40)");
    query_all(&mut a, "BEGIN");
    assert_eq!(
        query_all(&mut a, "SELECT id FROM lk WHERE id = 1 FOR UPDATE"),
        vec!["1"],
    );
    (common::ChildGuard(raw), addr, a)
}

#[test]
fn skip_locked_hands_back_the_next_free_row() {
    // The queue take. This returned the LOCKED row before.
    let (_child, addr, _a) = boot_with_holder("queue");
    let mut b = open(&addr);
    assert_eq!(
        query_all(&mut b, "SELECT id FROM lk ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED"),
        vec!["2"],
    );
}

#[test]
fn skip_locked_covers_every_row_source() {
    // Each of these takes a DIFFERENT executor inside the engine —
    // sequential scan, PK-ordered top-N walk, index seek. All three
    // must honour the exclusion, which is why it rides the snapshot.
    let (_child, addr, _a) = boot_with_holder("sources");
    let mut b = open(&addr);
    // sequential scan
    assert_eq!(
        query_all(&mut b, "SELECT id FROM lk FOR UPDATE SKIP LOCKED"),
        vec!["2", "3", "4"],
    );
    // PK-ordered top-N
    assert_eq!(
        query_all(&mut b, "SELECT id FROM lk ORDER BY id LIMIT 2 FOR UPDATE SKIP LOCKED"),
        vec!["2", "3"],
    );
    assert_eq!(
        query_all(&mut b, "SELECT id FROM lk ORDER BY id DESC LIMIT 1 FOR UPDATE SKIP LOCKED"),
        vec!["4"],
    );
    // index seek onto the locked row — empty, not the row
    assert!(
        query_all(&mut b, "SELECT id FROM lk WHERE id = 1 FOR UPDATE SKIP LOCKED").is_empty(),
    );
    // …and onto a free one
    assert_eq!(
        query_all(&mut b, "SELECT id FROM lk WHERE id = 3 FOR UPDATE SKIP LOCKED"),
        vec!["3"],
    );
}

#[test]
fn nowait_reports_pgs_55p03_wording() {
    let (_child, addr, _a) = boot_with_holder("nowait");
    let mut b = open(&addr);
    assert_eq!(
        query_err(&mut b, "SELECT id FROM lk WHERE id = 1 FOR UPDATE NOWAIT"),
        // PG's 55P03 LOCK_NOT_AVAILABLE — clients catch this code to
        // back off, so the code matters as much as the wording.
        Some("55P03|could not obtain lock on row in relation \"lk\"".into()),
    );
    // A row nobody holds is handed over without complaint — on the SAME
    // connection, which is the point. PG's autocommit rolls back only
    // the failed statement; the session carries on. Round 298 made the
    // aborted flag per-slot so this is true here too.
    assert_eq!(
        query_all(&mut b, "SELECT id FROM lk WHERE id = 3 FOR UPDATE NOWAIT"),
        vec!["3"],
    );
    // …and an unrelated connection is untouched by someone else's
    // failure. That was the actual defect: the aborted flag lived on
    // the shared engine, guarded by the GLOBAL `in_transaction()`, so a
    // failed autocommit statement poisoned every connection whenever
    // any other one held a transaction.
    let mut c = open(&addr);
    assert_eq!(
        query_all(&mut c, "SELECT id FROM lk WHERE id = 4 FOR UPDATE NOWAIT"),
        vec!["4"],
    );
}

#[test]
fn a_weaker_strength_still_conflicts_with_the_exclusive_holder() {
    // PG's matrix: FOR SHARE conflicts with a held FOR UPDATE.
    let (_child, addr, _a) = boot_with_holder("share");
    let mut b = open(&addr);
    assert_eq!(
        query_all(&mut b, "SELECT id FROM lk ORDER BY id LIMIT 1 FOR SHARE SKIP LOCKED"),
        vec!["2"],
    );
}

#[test]
fn the_locks_release_at_commit() {
    let (_child, addr, mut a) = boot_with_holder("release");
    let mut b = open(&addr);
    assert_eq!(
        query_all(&mut b, "SELECT id FROM lk ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED"),
        vec!["2"],
    );
    query_all(&mut a, "COMMIT");
    // Row 1 is free again.
    assert_eq!(
        query_all(&mut b, "SELECT id FROM lk ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED"),
        vec!["1"],
    );
}

#[test]
fn an_ordinary_select_is_untouched_by_a_held_lock() {
    // Row locks must not make rows invisible to readers that did not
    // ask for locks — the exclusion rides the snapshot, so this is the
    // check that it rides it only when asked.
    let (_child, addr, _a) = boot_with_holder("plain");
    let mut b = open(&addr);
    assert_eq!(
        query_all(&mut b, "SELECT id FROM lk ORDER BY id"),
        vec!["1", "2", "3", "4"],
    );
    assert_eq!(query_all(&mut b, "SELECT count(*) FROM lk"), vec!["4"]);
}

#[test]
fn a_bare_for_update_waits_for_the_holder_and_then_proceeds() {
    // v7.39 (round 299, E3 Phase 2) — PG's default policy BLOCKS until
    // the holder commits, then takes the lock. Measured against live
    // PG 18.4: the waiter returns the row after ~2s.
    //
    // The wait cannot happen inside the engine write lock — that would
    // stop every connection, including the one whose COMMIT frees the
    // row, so the server would deadlock against itself. The engine
    // reports `LockWouldBlock` and the retry runs after the guard drops.
    let (_child, addr, mut a) = boot_with_holder("wait");
    let mut b = open(&addr);

    // B blocks on row 1. Hand it to a thread so this one can commit A.
    let handle = std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let got = query_all(&mut b, "SELECT id FROM lk WHERE id = 1 FOR UPDATE");
        (got, started.elapsed())
    });

    // Give the waiter time to actually block, then release.
    std::thread::sleep(Duration::from_millis(300));
    query_all(&mut a, "COMMIT");

    let (got, waited) = handle.join().expect("waiter thread");
    assert_eq!(got, vec!["1"], "the waiter gets the row once it is free");
    assert!(
        waited >= Duration::from_millis(250),
        "it should have WAITED, not returned immediately: {waited:?}",
    );
}

#[test]
fn a_deadlock_kills_one_side_and_lets_the_other_through() {
    // v7.39 (round 300) — A holds row 1 and reaches for row 2 while B
    // holds row 2 and reaches for row 1. PG breaks the cycle by
    // aborting ONE transaction with 40P01; the other proceeds.
    //
    // Testing this mattered. It found two defects the happy paths could
    // not: a retryable `LockWouldBlock` was marking the transaction
    // aborted, so the first block poisoned the waiter and it lost a
    // deadlock it should have won; and the outcome carries a NAMED
    // victim which was being ignored, so both sides died where PG kills
    // one.
    let dir = unique_tmpdir("deadlock");
    let db = dir.join("spg.db");
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let addr = addrs.pgwire.as_ref().unwrap().clone();

    let mut setup = open(&addr);
    query_all(&mut setup, "CREATE TABLE dl (id int primary key, v int)");
    query_all(&mut setup, "INSERT INTO dl VALUES (1,10),(2,20)");

    let mut a = open(&addr);
    let mut b = open(&addr);
    // The waiter blocks until the cycle is detected; the default 5s
    // read timeout is shorter than that takes.
    a.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    b.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    query_all(&mut a, "BEGIN");
    query_all(&mut a, "SELECT id FROM dl WHERE id = 1 FOR UPDATE");
    query_all(&mut b, "BEGIN");
    query_all(&mut b, "SELECT id FROM dl WHERE id = 2 FOR UPDATE");

    // A reaches for B's row; it will block, then the cycle closes when
    // B reaches for A's.
    let addr2 = addr.clone();
    let waiter = std::thread::spawn(move || {
        let _ = addr2;
        (
            query_err(&mut a, "SELECT id FROM dl WHERE id = 2 FOR UPDATE"),
            a,
        )
    });
    std::thread::sleep(Duration::from_millis(200));
    let b_err = query_err(&mut b, "SELECT id FROM dl WHERE id = 1 FOR UPDATE");
    // The victim keeps its locks until it ends the block — that is PG's
    // behaviour too (an aborted transaction still holds what it took).
    // Without this the survivor waits forever for a row the loser is
    // still sitting on.
    let _ = query_err(&mut b, "ROLLBACK");
    let (a_err, _a) = waiter.join().expect("waiter thread");

    // Exactly one side is the victim, and it carries PG's 40P01.
    let victims = [&a_err, &b_err].iter().filter(|e| e.is_some()).count();
    assert_eq!(victims, 1, "a={a_err:?} b={b_err:?}");
    let victim = a_err.or(b_err).unwrap();
    assert_eq!(victim, "40P01|deadlock detected", "{victim}");
}

#[test]
fn lock_timeout_gives_up_and_reports_pgs_wording() {
    // v7.39 (round 301) — `SET lock_timeout` bounds the blocking wait.
    // PG cancels with `canceling statement due to lock timeout` (55P03,
    // the class clients catch to back off).
    //
    // Rounds 299-301 twice recorded this as broken. Both were probe
    // timing, not code: the earlier scripts held the row only ~4s while
    // the SET + connect overhead let the holder commit before the
    // waiter reached for the lock, so the deadline was never tested. A
    // 10s hold made it fire in ~600ms.
    let (_child, addr, mut a) = boot_with_holder("timeout");
    // A already holds row 1 and is not going to commit during this test.
    let mut b = open(&addr);
    b.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    query_all(&mut b, "SET lock_timeout = '400ms'");

    let started = std::time::Instant::now();
    let err = query_err(&mut b, "SELECT id FROM lk WHERE id = 1 FOR UPDATE");
    let waited = started.elapsed();

    assert_eq!(
        err,
        Some("55P03|canceling statement due to lock timeout".into()),
    );
    // It gave up NEAR the deadline, not at the holder's release.
    assert!(
        waited >= Duration::from_millis(300) && waited < Duration::from_secs(3),
        "gave up after {waited:?}",
    );
    // A is untouched — it still holds its lock.
    let _ = query_all(&mut a, "COMMIT");
}
