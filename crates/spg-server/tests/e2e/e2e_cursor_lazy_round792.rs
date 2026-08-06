//! r792 — cursors over a plain scan produce their rows as the client
//! fetches them instead of running the whole query at DECLARE.
//!
//! The behaviour these pin was measured against live PG 18.4 first: a
//! cursor over `100/(100 - id)` on a 200-row table declares fine, hands
//! back the first batches, and raises 22012 on the batch that reaches
//! row 100. SPG used to raise it at DECLARE, which aborted the whole
//! transaction and left the client with nothing.
//!
//! The rest pin what must NOT change while rows arrive in batches:
//! batched drains see every row exactly once, backward motion still
//! works over the prefix already fetched, FETCH ALL after a partial
//! fetch returns exactly the remainder, MOVE skips without returning,
//! and the shapes that keep the eager path (ORDER BY, WITH HOLD) keep
//! their old behaviour.
//!
//! The diff corpus already compares cursors against PG, but both of its
//! cursors carry ORDER BY, so none of this path is covered there.

use crate::common;
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(10);

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

fn send_startup(s: &mut TcpStream) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0anyone\0\0");
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
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
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

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s);
    let _ = read_until_ready(&mut s);
    s
}

/// First column of every DataRow, as text.
fn first_col(msgs: &[PgMessage]) -> Vec<String> {
    let mut out = Vec::new();
    for m in msgs.iter().filter(|m| m.ty == b'D') {
        let len = i32::from_be_bytes([m.body[2], m.body[3], m.body[4], m.body[5]]);
        if len < 0 {
            out.push(String::from("NULL"));
            continue;
        }
        let end = 6 + len as usize;
        out.push(String::from_utf8_lossy(&m.body[6..end]).into_owned());
    }
    out
}

fn has_error(msgs: &[PgMessage]) -> bool {
    msgs.iter().any(|m| m.ty == b'E')
}

fn sqlstate(msgs: &[PgMessage]) -> String {
    let Some(m) = msgs.iter().find(|m| m.ty == b'E') else {
        return String::new();
    };
    let mut i = 0;
    while i < m.body.len() && m.body[i] != 0 {
        let code = m.body[i];
        let start = i + 1;
        let end = start + m.body[start..].iter().position(|&b| b == 0).unwrap();
        if code == b'C' {
            return String::from_utf8_lossy(&m.body[start..end]).into_owned();
        }
        i = end + 1;
    }
    String::new()
}

fn q(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    send_query(s, sql);
    read_until_ready(s)
}

fn rows(s: &mut TcpStream, sql: &str) -> Vec<String> {
    let msgs = q(s, sql);
    assert!(!has_error(&msgs), "unexpected error from: {sql}");
    first_col(&msgs)
}

fn spawn(label: &str) -> (common::ChildGuard, common::ServerAddrs) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir: PathBuf = std::env::temp_dir().join(format!("spg-e2e-lazycur-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    (common::ChildGuard(raw), addrs)
}

fn seed(s: &mut TcpStream, n: usize) {
    q(s, "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)");
    q(
        s,
        &format!("INSERT INTO t SELECT g, 'r' || g FROM generate_series(1,{n}) g"),
    );
}

#[test]
fn batched_drain_sees_every_row_exactly_once() {
    let (_child, addrs) = spawn("drain");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed(&mut s, 250);

    q(&mut s, "BEGIN");
    q(&mut s, "DECLARE c CURSOR FOR SELECT id FROM t");
    let mut seen: Vec<String> = Vec::new();
    loop {
        let batch = rows(&mut s, "FETCH 40 FROM c");
        if batch.is_empty() {
            break;
        }
        seen.extend(batch);
    }
    q(&mut s, "COMMIT");

    assert_eq!(seen.len(), 250, "every row arrives across the batches");
    let uniq: BTreeSet<&String> = seen.iter().collect();
    assert_eq!(uniq.len(), 250, "and none of them twice");
}

#[test]
fn fetch_all_after_a_partial_fetch_returns_the_remainder() {
    let (_child, addrs) = spawn("remainder");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed(&mut s, 100);

    q(&mut s, "BEGIN");
    q(&mut s, "DECLARE c CURSOR FOR SELECT id FROM t");
    let head = rows(&mut s, "FETCH 10 FROM c");
    let rest = rows(&mut s, "FETCH ALL FROM c");
    q(&mut s, "COMMIT");

    assert_eq!(head.len(), 10);
    assert_eq!(rest.len(), 90);
    let mut all = head;
    all.extend(rest);
    let uniq: BTreeSet<&String> = all.iter().collect();
    assert_eq!(uniq.len(), 100, "the two halves do not overlap");
}

#[test]
fn backward_motion_walks_the_prefix_already_fetched() {
    let (_child, addrs) = spawn("backward");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed(&mut s, 50);

    q(&mut s, "BEGIN");
    q(&mut s, "DECLARE c CURSOR FOR SELECT id FROM t");
    let first_five = rows(&mut s, "FETCH 5 FROM c");
    let back = rows(&mut s, "FETCH PRIOR FROM c");
    let forward = rows(&mut s, "FETCH NEXT FROM c");
    q(&mut s, "COMMIT");

    assert_eq!(back.len(), 1);
    assert_eq!(
        back[0], first_five[3],
        "PRIOR lands on the row before the one we were on"
    );
    assert_eq!(
        forward[0], first_five[4],
        "and NEXT walks back onto the row we came from"
    );
}

#[test]
fn move_skips_without_returning_rows() {
    let (_child, addrs) = spawn("move");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed(&mut s, 60);

    q(&mut s, "BEGIN");
    q(&mut s, "DECLARE c CURSOR FOR SELECT id FROM t");
    let skipped = q(&mut s, "MOVE 20 FROM c");
    assert!(!has_error(&skipped));
    assert!(
        first_col(&skipped).is_empty(),
        "MOVE returns no rows, only a tag"
    );
    let after = rows(&mut s, "FETCH ALL FROM c");
    q(&mut s, "COMMIT");

    assert_eq!(after.len(), 40, "MOVE 20 of 60 leaves 40");
}

#[test]
fn where_clause_filters_across_batches() {
    let (_child, addrs) = spawn("where");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed(&mut s, 200);

    q(&mut s, "BEGIN");
    q(&mut s, "DECLARE c CURSOR FOR SELECT id FROM t WHERE id > 150");
    let a = rows(&mut s, "FETCH 20 FROM c");
    let b = rows(&mut s, "FETCH ALL FROM c");
    q(&mut s, "COMMIT");

    assert_eq!(a.len(), 20);
    assert_eq!(b.len(), 30, "50 rows match, 20 already fetched");
    for id in a.iter().chain(b.iter()) {
        assert!(
            id.parse::<i64>().unwrap() > 150,
            "the predicate holds in every batch"
        );
    }
}

/// The behaviour PG 18.4 was measured to have: DECLARE succeeds, early
/// batches come back, and the error surfaces on the batch that reaches
/// the offending row.
#[test]
fn a_row_that_errors_fails_the_batch_that_reaches_it() {
    let (_child, addrs) = spawn("errtiming");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed(&mut s, 200);

    q(&mut s, "BEGIN");
    let declared = q(&mut s, "DECLARE c CURSOR FOR SELECT 100/(100 - id) FROM t");
    assert!(
        !has_error(&declared),
        "DECLARE does not run the query, so it cannot fail on row 100"
    );

    let early = q(&mut s, "FETCH 10 FROM c");
    assert!(!has_error(&early), "the first ten rows are fine");
    assert_eq!(first_col(&early).len(), 10);

    let rest = q(&mut s, "FETCH ALL FROM c");
    assert!(has_error(&rest), "row 100 divides by zero");
    assert_eq!(sqlstate(&rest), "22012", "and reports what PG reports");
    q(&mut s, "ROLLBACK");
}

/// ORDER BY has to see every row before it can answer the first one, so
/// those cursors keep materialising at DECLARE. Pinned because the shape
/// gate is what decides it.
#[test]
fn ordered_cursors_keep_working() {
    let (_child, addrs) = spawn("ordered");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed(&mut s, 40);

    q(&mut s, "BEGIN");
    q(&mut s, "DECLARE c SCROLL CURSOR FOR SELECT id FROM t ORDER BY id DESC");
    let head = rows(&mut s, "FETCH 3 FROM c");
    let last = rows(&mut s, "FETCH LAST FROM c");
    q(&mut s, "COMMIT");

    assert_eq!(head, vec!["40", "39", "38"]);
    assert_eq!(last, vec!["1"]);
}

/// WITH HOLD outlives its transaction, and PG materialises those at
/// COMMIT so a held cursor cannot see later changes. The shape gate
/// keeps them eager for that reason.
#[test]
fn with_hold_survives_commit_and_ignores_later_writes() {
    let (_child, addrs) = spawn("hold");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed(&mut s, 20);

    q(&mut s, "BEGIN");
    q(&mut s, "DECLARE c CURSOR WITH HOLD FOR SELECT id FROM t");
    q(&mut s, "COMMIT");
    q(&mut s, "INSERT INTO t VALUES (999, 'late')");
    let all = rows(&mut s, "FETCH ALL FROM c");

    assert_eq!(all.len(), 20, "the row inserted after COMMIT is not in it");
    assert!(!all.iter().any(|r| r == "999"));
}

/// A cursor is insensitive to what commits after it was declared. The
/// eager path got that for free by reading everything at DECLARE; the
/// lazy one has to pin the snapshot, because outside RR/SER
/// `current_snapshot` is per-statement and a batch taken later would
/// otherwise pick up another connection's commit mid-drain.
#[test]
fn a_commit_from_another_connection_stays_out_of_an_open_cursor() {
    let (_child, addrs) = spawn("insensitive");
    let addr = addrs.pgwire.as_ref().unwrap();
    let mut a = open(addr);
    seed(&mut a, 30);

    q(&mut a, "BEGIN");
    q(&mut a, "DECLARE c CURSOR FOR SELECT id FROM t");
    let head = rows(&mut a, "FETCH 5 FROM c");
    assert_eq!(head.len(), 5);

    // Second connection, committed while the cursor sits mid-drain. The
    // row lands at the end of the table, which is exactly where the
    // remaining batches are headed.
    let mut b = open(addr);
    q(&mut b, "INSERT INTO t VALUES (777, 'later')");

    let rest = rows(&mut a, "FETCH ALL FROM c");
    q(&mut a, "COMMIT");

    assert_eq!(rest.len(), 25, "the 30 rows that existed at DECLARE, less 5");
    assert!(
        !rest.iter().any(|r| r == "777"),
        "the later commit is not in the cursor"
    );
    // The same connection sees it once the cursor is done with it.
    let after = rows(&mut a, "SELECT id FROM t WHERE id = 777");
    assert_eq!(after, vec!["777"], "and it really did commit");
}

/// Laziness, asserted behaviourally rather than by watching RSS.
///
/// Under a byte ceiling, an eager cursor cannot get past DECLARE on a
/// table it has to materialise whole, while a lazy one only charges the
/// batch in hand. That difference is observable at the wire, which RSS
/// deltas at this size are not — once the large allocation went away
/// they landed inside allocator-reuse noise (one cell measured -2 MB).
#[test]
fn a_byte_ceiling_bounds_the_batch_not_the_cursor() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir: PathBuf = std::env::temp_dir().join(format!("spg-e2e-lazycur-budget-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        // Room for a few thousand narrow rows, nowhere near 20k of them.
        .env("SPG_MAX_QUERY_BYTES", "200000")
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed(&mut s, 20000);

    q(&mut s, "BEGIN");
    let declared = q(&mut s, "DECLARE c CURSOR FOR SELECT id FROM t");
    assert!(
        !has_error(&declared),
        "DECLARE produces no rows, so it charges nothing"
    );
    let batch = q(&mut s, "FETCH 100 FROM c");
    assert!(!has_error(&batch), "100 rows fit under the ceiling");
    assert_eq!(first_col(&batch).len(), 100);

    let drain = q(&mut s, "FETCH ALL FROM c");
    assert!(
        has_error(&drain),
        "draining 20k rows in one batch does not fit, and is refused          the same way the bare SELECT would be"
    );
    q(&mut s, "ROLLBACK");

    // The eager shape charges the whole set at DECLARE instead.
    q(&mut s, "BEGIN");
    let ordered = q(&mut s, "DECLARE d CURSOR FOR SELECT id FROM t ORDER BY id");
    assert!(
        has_error(&ordered),
        "an ordered cursor still materialises up front, and the ceiling          catches it there"
    );
    q(&mut s, "ROLLBACK");
}

