//! r828 — role DDL participates in the transaction, as it does in PG.
//!
//! PG treats roles as ordinary catalog rows: `BEGIN; CREATE ROLE r;
//! ROLLBACK` leaves no role and `COMMIT` publishes one (measured
//! against PG18: count 0 after rollback, 1 after commit). SPG used to
//! refuse the statement inside a transaction outright — a refusal no
//! drop-in client expects, and one that PG itself reserves for the few
//! things a rollback genuinely cannot undo (CREATE DATABASE, ALTER
//! SYSTEM), which roles are not.
//!
//! The engine gives the transaction a shadow of the user store, made
//! on first role DDL and following the catalog's own model: writes go
//! to the shadow, COMMIT installs it, ROLLBACK drops it, savepoints
//! bookmark it. Other sessions — and the auth path — read the
//! committed store, so an uncommitted role is invisible elsewhere and
//! cannot log in.
//!
//! One installation subtlety is pinned by the commit test: the catalog
//! install is gated on `shadow_dirty`, which means "a &mut Catalog was
//! handed out", and a transaction whose only work was role DDL never
//! sets it. The role shadow therefore installs on its own terms; when
//! it rode the catalog's gate instead, a committed role vanished.
//!
//! Durability is probed rather than pinned here: after kill -9 and WAL
//! replay, psql (a real SCRAM client) logged in as the transactionally
//! created role — `current_user` answered `rtx`. This suite's harness
//! speaks unauthenticated pgwire only, and once a credentialed role
//! exists the server demands SCRAM on new connections, so that check
//! lives in the round record, not in a test that could not speak the
//! protocol it needs.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(30);

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

fn q(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    send_query(s, sql);
    read_until_ready(s)
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s);
    let _ = read_until_ready(&mut s);
    s
}

fn error_field(msgs: &[PgMessage], want: u8) -> Option<String> {
    let m = msgs.iter().find(|m| m.ty == b'E')?;
    let mut i = 0;
    while i < m.body.len() && m.body[i] != 0 {
        let code = m.body[i];
        let start = i + 1;
        let end = start + m.body[start..].iter().position(|&b| b == 0)?;
        if code == want {
            return Some(String::from_utf8_lossy(&m.body[start..end]).into_owned());
        }
        i = end + 1;
    }
    None
}

fn ok(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    let msgs = q(s, sql);
    assert_eq!(
        error_field(&msgs, b'C'),
        None,
        "statement failed: {sql} -> {}",
        error_field(&msgs, b'M').unwrap_or_default()
    );
    msgs
}

fn col0(msgs: &[PgMessage]) -> Vec<String> {
    let mut out = Vec::new();
    for m in msgs.iter().filter(|m| m.ty == b'D') {
        let len = i32::from_be_bytes([m.body[2], m.body[3], m.body[4], m.body[5]]);
        if len < 0 {
            out.push(String::new());
            continue;
        }
        out.push(String::from_utf8_lossy(&m.body[6..6 + len as usize]).into_owned());
    }
    out
}

fn role_count(s: &mut TcpStream, name: &str) -> String {
    col0(&ok(
        s,
        &format!("SELECT count(*) FROM pg_roles WHERE rolname = '{name}'"),
    ))
    .pop()
    .unwrap()
}

fn spawn(label: &str) -> (common::ChildGuard, common::ServerAddrs) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir: PathBuf = std::env::temp_dir().join(format!("spg-e2e-roletx-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    (common::ChildGuard(raw), addrs)
}

#[test]
fn a_role_created_in_a_rolled_back_transaction_never_existed() {
    let (_child, addrs) = spawn("rollback");
    let addr = addrs.pgwire.as_ref().unwrap();
    let mut a = open(addr);
    let mut b = open(addr);

    ok(&mut a, "BEGIN");
    ok(&mut a, "CREATE ROLE r828_rb LOGIN PASSWORD 'x'");

    // A sees its own uncommitted role; B must not — B's view is the
    // committed store, which is also what the auth path reads.
    assert_eq!(role_count(&mut a, "r828_rb"), "1", "creator sees it in-tx");
    assert_eq!(role_count(&mut b, "r828_rb"), "0", "another session must not");

    ok(&mut a, "ROLLBACK");
    assert_eq!(role_count(&mut a, "r828_rb"), "0", "rollback leaves nothing");
    assert_eq!(role_count(&mut b, "r828_rb"), "0");
}

#[test]
fn a_role_committed_in_a_transaction_is_published() {
    let (_child, addrs) = spawn("commit");
    let addr = addrs.pgwire.as_ref().unwrap();
    let mut a = open(addr);
    let mut b = open(addr);

    ok(&mut a, "BEGIN");
    ok(&mut a, "CREATE ROLE r828_ok LOGIN PASSWORD 'x'");
    ok(&mut a, "COMMIT");

    // The transaction touched no table, so `shadow_dirty` is unset and
    // the catalog install is skipped whole — the role shadow must not
    // ride that gate, or this reads 0.
    assert_eq!(role_count(&mut a, "r828_ok"), "1", "committed role exists");
    assert_eq!(role_count(&mut b, "r828_ok"), "1", "for every session");
}

#[test]
fn rollback_to_a_savepoint_takes_the_roles_made_after_it() {
    let (_child, addrs) = spawn("savepoint");
    let addr = addrs.pgwire.as_ref().unwrap();
    let mut s = open(addr);

    ok(&mut s, "BEGIN");
    ok(&mut s, "CREATE ROLE r828_before LOGIN PASSWORD 'x'");
    ok(&mut s, "SAVEPOINT sp");
    ok(&mut s, "CREATE ROLE r828_after LOGIN PASSWORD 'x'");
    ok(&mut s, "ROLLBACK TO SAVEPOINT sp");

    assert_eq!(
        role_count(&mut s, "r828_after"),
        "0",
        "made after the savepoint: rolled back with it"
    );
    assert_eq!(
        role_count(&mut s, "r828_before"),
        "1",
        "made before the savepoint: still pending"
    );

    ok(&mut s, "COMMIT");
    assert_eq!(role_count(&mut s, "r828_before"), "1", "and commits");
    assert_eq!(role_count(&mut s, "r828_after"), "0");
}

#[test]
fn drop_and_membership_roll_back_too() {
    let (_child, addrs) = spawn("dropgrant");
    let addr = addrs.pgwire.as_ref().unwrap();
    let mut s = open(addr);

    ok(&mut s, "CREATE ROLE r828_grp PASSWORD 'x'");
    ok(&mut s, "CREATE ROLE r828_member LOGIN PASSWORD 'x'");

    // DROP inside a rolled-back tx: the role stays.
    ok(&mut s, "BEGIN");
    ok(&mut s, "DROP ROLE r828_grp");
    assert_eq!(role_count(&mut s, "r828_grp"), "0", "dropped in-tx");
    ok(&mut s, "ROLLBACK");
    assert_eq!(role_count(&mut s, "r828_grp"), "1", "drop rolled back");

    // Membership granted inside a rolled-back tx: gone with it.
    let members = |s: &mut TcpStream| -> String {
        col0(&ok(
            s,
            "SELECT count(*) FROM pg_auth_members m \
             JOIN pg_roles r ON m.roleid = r.oid WHERE r.rolname = 'r828_grp'",
        ))
        .pop()
        .unwrap()
    };
    let baseline = members(&mut s);
    ok(&mut s, "BEGIN");
    ok(&mut s, "GRANT r828_grp TO r828_member");
    ok(&mut s, "ROLLBACK");
    assert_eq!(
        members(&mut s),
        baseline,
        "membership granted in a rolled-back tx must not persist"
    );
}
