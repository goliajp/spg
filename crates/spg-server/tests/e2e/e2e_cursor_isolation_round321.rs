//! read01 round 321 (V54) — a cursor belongs to the connection that
//! declared it.
//!
//! `Engine.cursors` sat on the shared engine rather than in the
//! per-connection session bag (where session GUCs, prepared statements and
//! large-object descriptors already live). One process-wide namespace for
//! every client means two connections cannot both `DECLARE c`, one
//! connection's `FETCH` reads another's rows, and `CLOSE ALL` closes
//! everybody's.
//!
//! `DECLARE` also gated on the engine's GLOBAL in-transaction flag, so a
//! connection with no transaction of its own was allowed to declare a
//! cursor as long as SOME other connection had a block open — the same
//! global-vs-slot trap rounds 298 / 304 / 316 each fixed elsewhere.
//!
//! PG 18.4 measured: `DECLARE c CURSOR …` outside a transaction block is
//! `ERROR: DECLARE CURSOR can only be used in transaction blocks`, and a
//! second `DECLARE c` in the same session is `ERROR: cursor "c" already
//! exists`.

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
    let p = std::env::temp_dir().join(format!("spg-e2e-cursoriso-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

struct PgMessage {
    ty: u8,
    body: Vec<u8>,
}

fn read_message(s: &mut TcpStream) -> Option<PgMessage> {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).ok()?;
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let body_len = len.saturating_sub(4);
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
        s.read_exact(&mut body).ok()?;
    }
    Some(PgMessage { ty, body })
}

fn send_startup(s: &mut TcpStream, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.push(0);
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn exchange(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(b'Q');
    out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    let mut msgs = Vec::new();
    while let Some(m) = read_message(s) {
        let z = m.ty == b'Z';
        msgs.push(m);
        if z {
            break;
        }
    }
    msgs
}

fn datarow_cell(body: &[u8], col_idx: usize) -> Option<String> {
    let cells = u16::from_be_bytes([body[0], body[1]]) as usize;
    if col_idx >= cells {
        return None;
    }
    let mut p = 2;
    for i in 0..cells {
        let len = i32::from_be_bytes([body[p], body[p + 1], body[p + 2], body[p + 3]]);
        p += 4;
        if len < 0 {
            if i == col_idx {
                return None;
            }
            continue;
        }
        let l = len as usize;
        if i == col_idx {
            return Some(std::str::from_utf8(&body[p..p + l]).unwrap().to_string());
        }
        p += l;
    }
    None
}

fn error_text(msgs: &[PgMessage]) -> Option<String> {
    msgs.iter().find(|m| m.ty == b'E').map(|m| {
        let mut p = 0;
        let mut out = String::new();
        while p < m.body.len() && m.body[p] != 0 {
            let code = m.body[p];
            p += 1;
            let end = m.body[p..].iter().position(|&b| b == 0).unwrap() + p;
            if code == b'M' {
                out = String::from_utf8_lossy(&m.body[p..end]).into_owned();
            }
            p = end + 1;
        }
        out
    })
}

/// The single-column values of every DataRow in the reply.
fn cells(msgs: &[PgMessage]) -> Vec<String> {
    msgs.iter()
        .filter(|m| m.ty == b'D')
        .filter_map(|m| datarow_cell(&m.body, 0))
        .collect()
}

fn ok(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    let msgs = exchange(s, sql);
    assert!(
        msgs.iter().all(|m| m.ty != b'E'),
        "`{sql}` failed: {:?}",
        error_text(&msgs)
    );
    msgs
}

fn open(addr: &str, user: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, user);
    while let Some(m) = read_message(&mut s) {
        if m.ty == b'Z' {
            break;
        }
    }
    s
}

fn spawn() -> (common::ChildGuard, String) {
    let db = unique_tmpdir("svc").join("spg.db");
    let (child, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .with_pgwire()
        .spawn();
    let addr = addrs.pgwire.expect("pgwire addr");
    (common::ChildGuard(child), addr)
}

/// Two connections each declare `c`, and each fetches its own rows.
#[test]
fn cursor_names_do_not_collide_across_connections() {
    let (_guard, addr) = spawn();
    let mut a = open(&addr, "alice");
    let mut b = open(&addr, "bob");
    ok(&mut a, "CREATE TABLE t (id INT NOT NULL)");
    for i in 1..=6 {
        ok(&mut a, &format!("INSERT INTO t VALUES ({i})"));
    }

    ok(&mut a, "BEGIN");
    ok(
        &mut a,
        "DECLARE c CURSOR FOR SELECT id FROM t WHERE id <= 3 ORDER BY id",
    );
    ok(&mut b, "BEGIN");
    let msgs = exchange(
        &mut b,
        "DECLARE c CURSOR FOR SELECT id FROM t WHERE id >= 4 ORDER BY id",
    );
    assert!(
        msgs.iter().all(|m| m.ty != b'E'),
        "B must have its own cursor namespace: {:?}",
        error_text(&msgs)
    );

    assert_eq!(cells(&ok(&mut a, "FETCH ALL FROM c")), vec!["1", "2", "3"]);
    assert_eq!(cells(&ok(&mut b, "FETCH ALL FROM c")), vec!["4", "5", "6"]);

    ok(&mut a, "ROLLBACK");
    ok(&mut b, "ROLLBACK");
}

/// `CLOSE ALL` closes this connection's cursors, not everybody's.
#[test]
fn close_all_only_closes_your_own_cursors() {
    let (_guard, addr) = spawn();
    let mut a = open(&addr, "alice");
    let mut b = open(&addr, "bob");
    ok(&mut a, "CREATE TABLE t (id INT NOT NULL)");
    ok(&mut a, "INSERT INTO t VALUES (1)");

    ok(&mut a, "BEGIN");
    ok(&mut a, "DECLARE mine CURSOR FOR SELECT id FROM t");
    ok(&mut b, "BEGIN");
    ok(&mut b, "DECLARE theirs CURSOR FOR SELECT id FROM t");

    ok(&mut b, "CLOSE ALL");
    assert_eq!(
        cells(&ok(&mut a, "FETCH ALL FROM mine")),
        vec!["1"],
        "A's cursor must survive B's CLOSE ALL"
    );

    ok(&mut a, "ROLLBACK");
    ok(&mut b, "ROLLBACK");
}

/// DECLARE gates on THIS connection's transaction, not on any connection
/// having one open.
#[test]
fn declare_requires_this_connections_own_transaction() {
    let (_guard, addr) = spawn();
    let mut a = open(&addr, "alice");
    let mut b = open(&addr, "bob");
    ok(&mut a, "CREATE TABLE t (id INT NOT NULL)");

    ok(&mut a, "BEGIN");
    ok(&mut a, "DECLARE c CURSOR FOR SELECT id FROM t");

    // B is in autocommit — PG refuses regardless of what A is doing.
    let msgs = exchange(&mut b, "DECLARE d CURSOR FOR SELECT id FROM t");
    assert_eq!(
        error_text(&msgs).as_deref(),
        Some("DECLARE CURSOR can only be used in transaction blocks"),
        "A's open block must not license B's cursor"
    );

    ok(&mut a, "ROLLBACK");
}

/// A second `DECLARE c` in the SAME connection still collides, as PG does —
/// per-connection namespaces must not become no namespace at all.
#[test]
fn a_repeat_declare_in_one_connection_still_collides() {
    let (_guard, addr) = spawn();
    let mut a = open(&addr, "alice");
    ok(&mut a, "CREATE TABLE t (id INT NOT NULL)");
    ok(&mut a, "BEGIN");
    ok(&mut a, "DECLARE c CURSOR FOR SELECT id FROM t");
    let msgs = exchange(&mut a, "DECLARE c CURSOR FOR SELECT id FROM t");
    assert_eq!(
        error_text(&msgs).as_deref(),
        Some("cursor \"c\" already exists"),
    );
    ok(&mut a, "ROLLBACK");
}

/// A connection that goes away takes its cursors with it.
#[test]
fn a_disconnect_drops_that_connections_cursors() {
    let (_guard, addr) = spawn();
    let mut a = open(&addr, "alice");
    ok(&mut a, "CREATE TABLE t (id INT NOT NULL)");
    ok(&mut a, "INSERT INTO t VALUES (1)");

    {
        let mut b = open(&addr, "bob");
        ok(&mut b, "BEGIN");
        ok(&mut b, "DECLARE gone CURSOR WITH HOLD FOR SELECT id FROM t");
        ok(&mut b, "COMMIT");
    }

    // A fresh connection must not inherit it.
    let mut c = open(&addr, "carol");
    ok(&mut c, "BEGIN");
    let msgs = exchange(&mut c, "FETCH ALL FROM gone");
    assert!(
        error_text(&msgs).is_some(),
        "a departed connection's cursor must not be visible"
    );
    ok(&mut c, "ROLLBACK");
}

/// PG's `DISCARD ALL` includes `CLOSE ALL`. Round 320 had to leave cursors
/// alone because the table was process-wide; now that it is per-session,
/// the discard is complete — and still only touches the caller.
#[test]
fn discard_all_closes_this_connections_cursors_only() {
    let (_guard, addr) = spawn();
    let mut a = open(&addr, "alice");
    let mut b = open(&addr, "bob");
    ok(&mut a, "CREATE TABLE t (id INT NOT NULL)");
    ok(&mut a, "INSERT INTO t VALUES (1)");

    ok(&mut a, "BEGIN");
    ok(&mut a, "DECLARE keep CURSOR WITH HOLD FOR SELECT id FROM t");
    ok(&mut a, "COMMIT");
    ok(&mut b, "BEGIN");
    ok(&mut b, "DECLARE mine CURSOR WITH HOLD FOR SELECT id FROM t");
    ok(&mut b, "COMMIT");

    ok(&mut b, "DISCARD ALL");
    let msgs = exchange(&mut b, "FETCH ALL FROM mine");
    assert!(
        error_text(&msgs).is_some(),
        "DISCARD ALL must close the caller's cursors"
    );
    assert_eq!(
        cells(&ok(&mut a, "FETCH ALL FROM keep")),
        vec!["1"],
        "…and only the caller's"
    );
}
