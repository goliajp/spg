//! v7.39 (round 283) — one transaction slot PER CONNECTION.
//!
//! The server runs ONE shared `Engine`, and every pgwire path called
//! `Engine::execute()`, which routes through `IMPLICIT_TX` — slot 0. So
//! all connections shared a single transaction. Two consequences, both
//! measured against live PG 18.4 before the fix:
//!
//!   * a second client's `BEGIN` answered `a transaction is already
//!     open`, and that client's session then sat in the aborted state;
//!   * a READ COMMITTED transaction never saw another connection's
//!     commit — not because the isolation was wrong, but because there
//!     was no other connection's transaction to see. The engine's own
//!     pins had this right all along: they drive `execute_in(sql, tx)`
//!     with `alloc_tx_id()`, which the server never called.
//!
//! The engine has been multi-slot since v4.41.1. This round is the
//! server finally asking for a slot.
//!
//! These must speak the PG wire directly — the defect is in how the
//! server addresses the engine, so the embedded API cannot reach it.

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
    let p = std::env::temp_dir().join(format!("spg-e2e-conctx-{label}-{nanos}"));
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

fn datarow_cell(body: &[u8], col: usize) -> Option<String> {
    let cells = u16::from_be_bytes([body[0], body[1]]) as usize;
    if col >= cells {
        return None;
    }
    let mut p = 2;
    for i in 0..cells {
        let len = i32::from_be_bytes([body[p], body[p + 1], body[p + 2], body[p + 3]]);
        p += 4;
        if len < 0 {
            if i == col {
                return None;
            }
            continue;
        }
        let l = len as usize;
        if i == col {
            return Some(std::str::from_utf8(&body[p..p + l]).unwrap().to_string());
        }
        p += l;
    }
    None
}

/// Run `sql`; panic on ErrorResponse. Returns the first cell of the
/// first DataRow.
fn query_one(s: &mut TcpStream, sql: &str) -> Option<String> {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    for m in &msgs {
        assert!(m.ty != b'E', "{sql}: unexpected ErrorResponse");
    }
    msgs.iter()
        .find(|m| m.ty == b'D')
        .and_then(|m| datarow_cell(&m.body, 0))
}

/// Run `sql` expecting it to SUCCEED or FAIL; returns the error text.
fn query_err(s: &mut TcpStream, sql: &str) -> Option<String> {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    msgs.iter().find(|m| m.ty == b'E').map(|m| {
        let frame = spg_wire::Frame {
            op: spg_wire::Op::ErrorResponse,
            payload: m.body.clone(),
        };
        spg_wire::parse_error_response(&frame)
            .unwrap_or("<undecodable>")
            .to_string()
    })
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "admin");
    let _ = read_until_ready(&mut s);
    s
}

fn boot(label: &str) -> (common::ChildGuard, String) {
    let dir = unique_tmpdir(label);
    let db = dir.join("spg.db");
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .with_pgwire()
        .spawn();
    let addr = addrs.pgwire.as_ref().unwrap().clone();
    (common::ChildGuard(raw), addr)
}

#[test]
fn two_connections_can_hold_transactions_at_the_same_time() {
    let (_child, addr) = boot("two");
    let mut a = open(&addr);
    let mut b = open(&addr);
    query_one(&mut a, "CREATE TABLE t (id int primary key, v int)");

    query_one(&mut a, "BEGIN");
    // Before this round: "a transaction is already open", and B's
    // session was poisoned for every statement that followed.
    assert_eq!(query_err(&mut b, "BEGIN"), None);
    query_one(&mut a, "INSERT INTO t VALUES (1, 10)");
    query_one(&mut b, "INSERT INTO t VALUES (2, 20)");
    query_one(&mut a, "COMMIT");
    query_one(&mut b, "COMMIT");

    let mut c = open(&addr);
    assert_eq!(query_one(&mut c, "SELECT count(*) FROM t"), Some("2".into()));
}

#[test]
fn one_connections_uncommitted_write_is_invisible_to_another() {
    let (_child, addr) = boot("dirty");
    let mut a = open(&addr);
    let mut b = open(&addr);
    query_one(&mut a, "CREATE TABLE t (id int primary key, v int)");

    query_one(&mut a, "BEGIN");
    query_one(&mut a, "INSERT INTO t VALUES (1, 10)");
    // The slots are separate, so B must not read A's uncommitted row.
    assert_eq!(query_one(&mut b, "SELECT count(*) FROM t"), Some("0".into()));
    query_one(&mut a, "COMMIT");
    assert_eq!(query_one(&mut b, "SELECT count(*) FROM t"), Some("1".into()));
}

#[test]
fn a_rollback_on_one_connection_leaves_the_other_alone() {
    let (_child, addr) = boot("rb");
    let mut a = open(&addr);
    let mut b = open(&addr);
    query_one(&mut a, "CREATE TABLE t (id int primary key, v int)");

    query_one(&mut a, "BEGIN");
    query_one(&mut b, "BEGIN");
    query_one(&mut a, "INSERT INTO t VALUES (1, 10)");
    query_one(&mut b, "INSERT INTO t VALUES (2, 20)");
    query_one(&mut a, "ROLLBACK");
    query_one(&mut b, "COMMIT");

    let mut c = open(&addr);
    assert_eq!(query_one(&mut c, "SELECT count(*) FROM t"), Some("1".into()));
    assert_eq!(
        query_one(&mut c, "SELECT v FROM t WHERE id = 2"),
        Some("20".into()),
    );
}

#[test]
fn read_committed_sees_another_connections_commit() {
    // PG 18.4, same script: the second read returns 99. Before this
    // round SPG returned 10 — which looked like READ COMMITTED being
    // silently upgraded to REPEATABLE READ, and was really both
    // sessions sharing one transaction slot.
    let (_child, addr) = boot("rc");
    let mut a = open(&addr);
    let mut b = open(&addr);
    query_one(&mut a, "CREATE TABLE t (id int primary key, v int)");
    query_one(&mut a, "INSERT INTO t VALUES (1, 10)");

    query_one(&mut a, "BEGIN ISOLATION LEVEL READ COMMITTED");
    assert_eq!(
        query_one(&mut a, "SELECT v FROM t WHERE id = 1"),
        Some("10".into()),
    );
    query_one(&mut b, "UPDATE t SET v = 99 WHERE id = 1");
    assert_eq!(
        query_one(&mut a, "SELECT v FROM t WHERE id = 1"),
        Some("99".into()),
        "READ COMMITTED takes a fresh snapshot per statement",
    );
    query_one(&mut a, "COMMIT");
}

#[test]
fn repeatable_read_still_freezes_its_view() {
    // The other half of the same story: RR must NOT see the commit.
    // This was already true, but only provably so once two connections
    // could hold transactions at once.
    let (_child, addr) = boot("rr");
    let mut a = open(&addr);
    let mut b = open(&addr);
    query_one(&mut a, "CREATE TABLE t (id int primary key, v int)");
    query_one(&mut a, "INSERT INTO t VALUES (1, 10)");

    query_one(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(
        query_one(&mut a, "SELECT v FROM t WHERE id = 1"),
        Some("10".into()),
    );
    query_one(&mut b, "UPDATE t SET v = 99 WHERE id = 1");
    assert_eq!(
        query_one(&mut a, "SELECT v FROM t WHERE id = 1"),
        Some("10".into()),
        "REPEATABLE READ holds the snapshot it took at BEGIN",
    );
    query_one(&mut a, "COMMIT");
}

#[test]
fn a_disconnect_mid_transaction_does_not_strand_the_slot() {
    // A client that vanishes inside BEGIN must not leave its shadow
    // catalog — with uncommitted rows — sitting in the engine.
    let (_child, addr) = boot("drop");
    let mut a = open(&addr);
    query_one(&mut a, "CREATE TABLE t (id int primary key, v int)");
    {
        let mut doomed = open(&addr);
        query_one(&mut doomed, "BEGIN");
        query_one(&mut doomed, "INSERT INTO t VALUES (7, 70)");
        // dropped without COMMIT or ROLLBACK
    }
    assert_eq!(query_one(&mut a, "SELECT count(*) FROM t"), Some("0".into()));
    // …and the engine still accepts new transactions afterwards.
    query_one(&mut a, "BEGIN");
    query_one(&mut a, "INSERT INTO t VALUES (8, 80)");
    query_one(&mut a, "COMMIT");
    assert_eq!(query_one(&mut a, "SELECT count(*) FROM t"), Some("1".into()));
}

/// v7.39 (round 494) — a transaction that changed nothing installs nothing.
///
/// COMMIT replaces the committed catalog with the transaction's shadow,
/// and that shadow is a clone taken at BEGIN. Phase E2 folds concurrent
/// commits back in for READ COMMITTED and Phase E3 merges the write-set
/// for RR/SERIALIZABLE — but E3 is gated on the transaction having touched
/// a table, so a READ-ONLY repeatable-read transaction reached the install
/// and put its stale clone over everything committed since it began.
///
/// Measured against PG18 over this same wire: A opens REPEATABLE READ and
/// reads, B commits an UPDATE, A commits. PG then has B's value for every
/// session; SPG had the OLD one everywhere — including for B itself and
/// for connections opened afterwards. B's committed write was gone.
///
/// This lives here, next to round 283's slot tests, for the reason that
/// file already states: the embedded API drives one transaction at a time,
/// so it cannot pose the question. Round 493 wrote this assertion against
/// the embedded API, saw it fail with that round's change compiled out,
/// and withdrew it as a separate question — this is that question.
#[test]
fn a_readonly_repeatable_read_commit_does_not_revert_another_connection() {
    let (_child, addr) = boot("ro-rr");
    let mut a = open(&addr);
    let mut b = open(&addr);
    query_one(&mut a, "CREATE TABLE t (id int primary key, v int)");
    query_one(&mut a, "INSERT INTO t VALUES (1, 10), (2, 20)");

    query_one(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(query_one(&mut a, "SELECT v FROM t WHERE id = 1"), Some("10".into()));

    query_one(&mut b, "UPDATE t SET v = 99 WHERE id = 1");
    query_one(&mut b, "INSERT INTO t VALUES (3, 30)");

    // A's own view stays frozen while it is open — that part always worked,
    // and it is what makes the clobber below silent.
    assert_eq!(query_one(&mut a, "SELECT v FROM t WHERE id = 1"), Some("10".into()));
    assert_eq!(query_one(&mut a, "SELECT count(*) FROM t"), Some("2".into()));
    query_one(&mut a, "COMMIT");

    // PG18: 99 and 3 rows, for every connection.
    assert_eq!(query_one(&mut a, "SELECT v FROM t WHERE id = 1"), Some("99".into()));
    assert_eq!(query_one(&mut a, "SELECT count(*) FROM t"), Some("3".into()));
    assert_eq!(query_one(&mut b, "SELECT v FROM t WHERE id = 1"), Some("99".into()));
    let mut c = open(&addr);
    assert_eq!(query_one(&mut c, "SELECT count(*) FROM t"), Some("3".into()));
}

/// The same for ROLLBACK, which discards the shadow rather than installing
/// it — so it was already correct, and stays a witness that the skip did
/// not change the discarding path.
#[test]
fn a_readonly_rollback_leaves_another_connections_commit_alone() {
    let (_child, addr) = boot("ro-rb");
    let mut a = open(&addr);
    let mut b = open(&addr);
    query_one(&mut a, "CREATE TABLE t (id int primary key, v int)");
    query_one(&mut a, "INSERT INTO t VALUES (1, 10)");
    query_one(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(query_one(&mut a, "SELECT v FROM t WHERE id = 1"), Some("10".into()));
    query_one(&mut b, "UPDATE t SET v = 77 WHERE id = 1");
    query_one(&mut a, "ROLLBACK");
    assert_eq!(query_one(&mut a, "SELECT v FROM t WHERE id = 1"), Some("77".into()));
}

/// The skip must not swallow a transaction that DID write, nor one whose
/// only change came through a path the statement classification calls
/// read-only. `SELECT nextval(…)` is the second kind: the first version of
/// this fix gated on `touched_tables`, and the large-object pins failed
/// because `SELECT lo_write(…)` mutates while classifying read-only.
#[test]
fn a_transaction_that_did_write_still_installs_its_work() {
    let (_child, addr) = boot("wrote");
    let mut a = open(&addr);
    query_one(&mut a, "CREATE TABLE t (id int primary key, v int)");
    query_one(&mut a, "INSERT INTO t VALUES (1, 10)");
    query_one(&mut a, "CREATE SEQUENCE s1");

    query_one(&mut a, "BEGIN");
    query_one(&mut a, "UPDATE t SET v = 55 WHERE id = 1");
    query_one(&mut a, "COMMIT");
    assert_eq!(query_one(&mut a, "SELECT v FROM t WHERE id = 1"), Some("55".into()));

    query_one(&mut a, "BEGIN");
    query_one(&mut a, "CREATE TABLE made_in_tx (a int)");
    query_one(&mut a, "COMMIT");
    query_one(&mut a, "INSERT INTO made_in_tx VALUES (1)");
    assert_eq!(query_one(&mut a, "SELECT count(*) FROM made_in_tx"), Some("1".into()));

    assert_eq!(query_one(&mut a, "SELECT nextval('s1')"), Some("1".into()));
    query_one(&mut a, "BEGIN");
    assert_eq!(query_one(&mut a, "SELECT nextval('s1')"), Some("2".into()));
    query_one(&mut a, "COMMIT");
    assert_eq!(query_one(&mut a, "SELECT nextval('s1')"), Some("3".into()));
}

/// v7.39 (round 495) — `SET` inside a transaction must not cost another
/// session its committed write.
///
/// Round 494 closed the read-only gate into the wholesale shadow install.
/// The same install has a second gate: `rebase_poisoned`, set by any
/// statement the classifier does not recognise as DML — and `SET` fell
/// into that catch-all. So `BEGIN ISOLATION LEVEL REPEATABLE READ; SET …;
/// UPDATE …; COMMIT` skipped the Phase E3 merge and reverted whatever had
/// been committed meanwhile. PG18 keeps both writes.
///
/// The SET family is session state, not catalog data, so it cannot
/// invalidate a write-set replay and no longer poisons.
#[test]
fn a_set_inside_a_transaction_does_not_revert_another_connection() {
    let (_child, addr) = boot("set-poison");
    let mut a = open(&addr);
    let mut b = open(&addr);
    query_one(&mut a, "CREATE TABLE t (id int primary key, v int)");
    query_one(&mut a, "INSERT INTO t VALUES (1, 10), (2, 20)");

    query_one(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    query_one(&mut a, "SET application_name = 'pin'");
    query_one(&mut a, "UPDATE t SET v = 111 WHERE id = 2");
    query_one(&mut b, "UPDATE t SET v = 99 WHERE id = 1");
    query_one(&mut a, "COMMIT");

    // PG18: both writes survive.
    let mut c = open(&addr);
    assert_eq!(query_one(&mut c, "SELECT v FROM t WHERE id = 1"), Some("99".into()));
    assert_eq!(query_one(&mut c, "SELECT v FROM t WHERE id = 2"), Some("111".into()));
}

/// v7.39 (round 496) — DDL inside a transaction must not cost another
/// session its committed write.
///
/// The last of the three gates into the wholesale shadow install. Phase
/// E3's row-level merge is gated on the transaction being un-poisoned,
/// and DDL poisons it — a write-set replay has no notion of a schema
/// change. So `BEGIN ISOLATION LEVEL REPEATABLE READ; CREATE TABLE …;
/// COMMIT` installed its BEGIN-time clone and deleted whatever had been
/// committed meanwhile. PG18 keeps both.
///
/// Such a commit now installs only the tables the transaction changed,
/// recorded by the catalog itself at `get_mut` / `create_table` /
/// `drop_table` rather than inferred from the statement classifier —
/// round 494 tried classification for a correctness gate and it was
/// wrong.
#[test]
fn ddl_inside_a_transaction_does_not_revert_another_connection() {
    let (_child, addr) = boot("ddl-poison");
    let mut a = open(&addr);
    let mut b = open(&addr);
    query_one(&mut a, "CREATE TABLE t (id int primary key, v int)");
    query_one(&mut a, "INSERT INTO t VALUES (1, 10)");

    query_one(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    query_one(&mut a, "CREATE TABLE made_in_tx (a int)");
    query_one(&mut b, "UPDATE t SET v = 99 WHERE id = 1");
    query_one(&mut a, "COMMIT");

    let mut c = open(&addr);
    // B's write survives...
    assert_eq!(query_one(&mut c, "SELECT v FROM t WHERE id = 1"), Some("99".into()));
    // ...and A's DDL landed.
    query_one(&mut c, "INSERT INTO made_in_tx VALUES (1)");
    assert_eq!(query_one(&mut c, "SELECT count(*) FROM made_in_tx"), Some("1".into()));
}

/// A DROP inside the transaction still drops, and still leaves the other
/// session's unrelated write alone.
#[test]
fn a_drop_inside_a_transaction_keeps_its_effect_and_spares_the_rest() {
    let (_child, addr) = boot("ddl-drop");
    let mut a = open(&addr);
    let mut b = open(&addr);
    query_one(&mut a, "CREATE TABLE keep (id int primary key, v int)");
    query_one(&mut a, "INSERT INTO keep VALUES (1, 10)");
    query_one(&mut a, "CREATE TABLE goes (a int)");

    query_one(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    query_one(&mut a, "DROP TABLE goes");
    query_one(&mut b, "UPDATE keep SET v = 99 WHERE id = 1");
    query_one(&mut a, "COMMIT");

    let mut c = open(&addr);
    assert_eq!(query_one(&mut c, "SELECT v FROM keep WHERE id = 1"), Some("99".into()));
    assert!(query_err(&mut c, "SELECT count(*) FROM goes").is_some(), "goes should be gone");
}
