//! read01 round 319 (V52) — a connection's row describes THAT connection.
//!
//! Rounds 317/318 made the row set and the control surface real; the
//! columns inside each row were still invented. `pg_stat_activity` reported
//! `client_addr` / `client_hostname` / `client_port` as NULL for every
//! connection, and `datname` was read from the ASKING session's GUC and
//! stamped on every row — so one connection's database was reported as
//! everybody's.
//!
//! Measured on PG 18.4 over TCP: `client_addr` is the peer IP,
//! `client_port` its ephemeral port, `client_hostname` NULL (PG only fills
//! it with `log_hostname` on), and a connection with no TCP peer reports
//! port **-1**, not NULL.
//!
//! Seeding the login identity and the database also had to move: both are
//! session state, and they were written BEFORE this connection's session
//! was installed on the shared engine — i.e. into whichever bag happened to
//! be current.

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
    let p = crate::common::tmp_base().join(format!("spg-e2e-connattrs-{label}-{nanos}"));
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

fn send_startup(s: &mut TcpStream, user: &str, database: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    if !database.is_empty() {
        body.extend_from_slice(b"database\0");
        body.extend_from_slice(database.as_bytes());
        body.push(0);
    }
    body.push(0);
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(b'Q');
    out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn read_until_ready(s: &mut TcpStream) -> Vec<PgMessage> {
    let mut out = Vec::new();
    while let Some(m) = read_message(s) {
        let z = m.ty == b'Z';
        out.push(m);
        if z {
            break;
        }
    }
    out
}

/// One DataRow's cells. `None` = NULL.
fn datarow_cells(body: &[u8]) -> Vec<Option<String>> {
    let cells = u16::from_be_bytes([body[0], body[1]]) as usize;
    let mut out = Vec::with_capacity(cells);
    let mut p = 2;
    for _ in 0..cells {
        let len = i32::from_be_bytes([body[p], body[p + 1], body[p + 2], body[p + 3]]);
        p += 4;
        if len < 0 {
            out.push(None);
            continue;
        }
        let l = len as usize;
        out.push(Some(
            std::str::from_utf8(&body[p..p + l]).unwrap().to_string(),
        ));
        p += l;
    }
    out
}

fn rows(s: &mut TcpStream, sql: &str) -> Vec<Vec<Option<String>>> {
    send_query(s, sql);
    read_until_ready(s)
        .iter()
        .filter(|m| m.ty == b'D')
        .map(|m| datarow_cells(&m.body))
        .collect()
}

fn scalar(s: &mut TcpStream, sql: &str) -> Option<String> {
    rows(s, sql)
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next())
        .unwrap_or_else(|| panic!("no row for `{sql}`"))
}

fn open(addr: &str, user: &str, database: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, user, database);
    let _ = read_until_ready(&mut s);
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

/// The peer really is reported. All three columns were NULL before.
#[test]
fn stat_activity_reports_the_real_peer() {
    let (_guard, addr) = spawn();
    let mut s = open(&addr, "alice", "shop");
    let local = s.local_addr().unwrap();

    let row = rows(
        &mut s,
        "SELECT client_addr, client_hostname, client_port FROM pg_stat_activity \
         WHERE pid = pg_backend_pid()",
    )
    .pop()
    .expect("our own row");

    assert_eq!(
        row[0].as_deref(),
        Some(local.ip().to_string().as_str()),
        "client_addr is the peer's IP"
    );
    assert_eq!(
        row[1], None,
        "client_hostname stays NULL — PG only fills it with log_hostname on"
    );
    assert_eq!(
        row[2].as_deref(),
        Some(local.port().to_string().as_str()),
        "client_port is the peer's port"
    );
}

/// `datname` is per-connection. It used to be read off the ASKING session
/// and stamped on every row, so two connections on different databases both
/// showed the asker's.
#[test]
fn stat_activity_datname_is_per_connection() {
    let (_guard, addr) = spawn();
    let mut a = open(&addr, "alice", "shop");
    let mut b = open(&addr, "bob", "warehouse");
    let b_pid = scalar(&mut b, "SELECT pg_backend_pid()").expect("pid");

    assert_eq!(
        scalar(&mut a, "SELECT current_database()").as_deref(),
        Some("shop")
    );
    let seen_from_a = scalar(
        &mut a,
        &format!("SELECT datname FROM pg_stat_activity WHERE pid = {b_pid}"),
    );
    assert_eq!(
        seen_from_a.as_deref(),
        Some("warehouse"),
        "B's row must name B's database, not the asker's"
    );
    // And B still sees its own.
    assert_eq!(
        scalar(&mut b, "SELECT current_database()").as_deref(),
        Some("warehouse")
    );
}

/// The login identity is session state too, and it was seeded before the
/// connection's session existed — so it landed in another connection's bag.
#[test]
fn current_user_is_the_connections_own_login() {
    let (_guard, addr) = spawn();
    let mut a = open(&addr, "alice", "shop");
    let mut b = open(&addr, "bob", "shop");

    assert_eq!(
        scalar(&mut a, "SELECT current_user").as_deref(),
        Some("alice")
    );
    assert_eq!(
        scalar(&mut b, "SELECT current_user").as_deref(),
        Some("bob")
    );
    assert_eq!(
        scalar(&mut a, "SELECT current_user").as_deref(),
        Some("alice"),
        "B connecting must not have rewritten A's identity"
    );
}

/// A client that names no database reports NULL, as PG does for one that
/// has no TCP peer reporting -1 — neither is invented.
#[test]
fn a_connection_without_a_database_reports_null() {
    let (_guard, addr) = spawn();
    let mut s = open(&addr, "alice", "");
    assert_eq!(
        scalar(
            &mut s,
            "SELECT datname FROM pg_stat_activity WHERE pid = pg_backend_pid()"
        ),
        None,
    );
}
