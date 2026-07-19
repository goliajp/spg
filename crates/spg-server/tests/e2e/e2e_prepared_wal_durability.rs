//! v7.33 (A1) — extended-protocol (prepared) writes must reach the WAL.
//!
//! Pre-7.33 `pgwire::handle_execute` persisted nothing after a successful
//! prepared write — no WAL append, no snapshot. Any server-mode client
//! using the extended query protocol (sqlx, every prepared-statement
//! driver) therefore lost all its writes on crash. A database that loses
//! acknowledged writes is broken.
//!
//! This test does prepared INSERTs over the PG wire (parameterised + no
//! param), kills the daemon with no graceful shutdown (durability rides
//! on the WAL alone), restarts on the same db + wal, and asserts the rows
//! replayed. Before the fix Phase 2 sees 0 rows; after it, they survive.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(15);

fn unique_tmpdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // v7.39 (round 262) — an atomic serial as well as the timestamp: the
    // tests here run in parallel and each removes its directory, and
    // `SystemTime::now()` is not nanosecond-distinct on macOS, so two
    // could otherwise share a directory and one's cleanup would delete
    // the other's database mid-run.
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let p = std::env::temp_dir().join(format!(
        "spg-prepared-wal-{}-{nanos}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn local_spawn(db: &Path, wal: &Path) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal)
        .with_pgwire()
        .with_mysqlwire()
        .spawn()
}

// ── minimal PG-wire client ────────────────────────────────────────

struct Msg {
    ty: u8,
    body: Vec<u8>,
}

fn read_message(s: &mut TcpStream) -> Msg {
    let mut h = [0u8; 5];
    s.read_exact(&mut h).expect("pg header");
    let len = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
    let n = len.saturating_sub(4);
    let mut body = vec![0u8; n];
    if n > 0 {
        s.read_exact(&mut body).expect("pg body");
    }
    Msg { ty: h[0], body }
}

fn write_msg(buf: &mut Vec<u8>, ty: u8, body: &[u8]) {
    buf.push(ty);
    buf.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    buf.extend_from_slice(body);
}

fn send_startup(s: &mut TcpStream, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196608u32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.push(0);
    let mut out = Vec::new();
    out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn read_until_ready(s: &mut TcpStream) {
    while read_message(s).ty != b'Z' {}
}

// Drain to ReadyForQuery, panicking on ErrorResponse so a failed write
// can't masquerade as a durable one.
fn read_until_ready_ok(s: &mut TcpStream, ctx: &str) {
    loop {
        let m = read_message(s);
        match m.ty {
            b'Z' => return,
            b'E' => panic!(
                "{ctx}: server ErrorResponse: {}",
                String::from_utf8_lossy(&m.body)
            ),
            _ => {}
        }
    }
}

fn simple_query(s: &mut TcpStream, sql: &str) {
    let mut q = Vec::new();
    let mut body = sql.as_bytes().to_vec();
    body.push(0);
    write_msg(&mut q, b'Q', &body);
    s.write_all(&q).unwrap();
    read_until_ready_ok(s, sql);
}

// Parse a (possibly parameterised) statement, Bind the text params,
// Execute, Sync — the extended-protocol write path the fix WALs.
fn prepared_exec(s: &mut TcpStream, sql: &str, params: &[&str]) {
    let mut q = Vec::new();
    // Parse: unnamed statement + sql + 0 param type OIDs.
    let mut p = Vec::new();
    p.push(0);
    p.extend_from_slice(sql.as_bytes());
    p.push(0);
    p.extend_from_slice(&0u16.to_be_bytes());
    write_msg(&mut q, b'P', &p);
    // Bind: unnamed portal + unnamed stmt + 0 format codes (text) +
    // params (each length-prefixed text) + 0 result-format codes.
    let mut b = Vec::new();
    b.push(0);
    b.push(0);
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&(params.len() as u16).to_be_bytes());
    for pv in params {
        b.extend_from_slice(&(pv.len() as u32).to_be_bytes());
        b.extend_from_slice(pv.as_bytes());
    }
    b.extend_from_slice(&0u16.to_be_bytes());
    write_msg(&mut q, b'B', &b);
    // Execute unnamed portal, max-rows 0.
    let mut e = Vec::new();
    e.push(0);
    e.extend_from_slice(&0u32.to_be_bytes());
    write_msg(&mut q, b'E', &e);
    write_msg(&mut q, b'S', &[]);
    s.write_all(&q).unwrap();
    read_until_ready_ok(s, sql);
}

fn count_rows(s: &mut TcpStream, sql: &str) -> usize {
    let mut q = Vec::new();
    let mut body = sql.as_bytes().to_vec();
    body.push(0);
    write_msg(&mut q, b'Q', &body);
    s.write_all(&q).unwrap();
    let mut rows = 0usize;
    loop {
        let m = read_message(s);
        match m.ty {
            b'D' => rows += 1,
            b'Z' => return rows,
            b'E' => panic!("{sql}: ErrorResponse: {}", String::from_utf8_lossy(&m.body)),
            _ => {}
        }
    }
}

fn connect_pg(addrs: &common::ServerAddrs) -> TcpStream {
    let pg = addrs.pgwire.clone().expect("pgwire enabled");
    let mut s = common::connect_to(&pg);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "anyone");
    assert_eq!(read_message(&mut s).ty, b'R', "expected AuthenticationOk");
    read_until_ready(&mut s);
    s
}

#[test]
fn prepared_writes_survive_crash_via_wal() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let wal = dir.join("wal.log");

    // Phase 1: prepared (extended-protocol) writes; kill the daemon.
    {
        let (raw, addrs) = local_spawn(&db, &wal);
        let _g = common::ChildGuard(raw);
        let mut s = connect_pg(&addrs);
        simple_query(&mut s, "CREATE TABLE t (a TEXT, b TEXT)");
        prepared_exec(&mut s, "INSERT INTO t VALUES ($1, $2)", &["alpha", "one"]);
        prepared_exec(&mut s, "INSERT INTO t VALUES ($1, $2)", &["beta", "two"]);
        // no-param prepared write — covers the placeholderless shape too.
        prepared_exec(&mut s, "INSERT INTO t VALUES ('gamma', 'three')", &[]);
        // _g drops here → kill -9, no graceful shutdown.
    }

    // Phase 2: fresh daemon replays the WAL.
    let (raw, addrs) = local_spawn(&db, &wal);
    let _g = common::ChildGuard(raw);
    let mut s = connect_pg(&addrs);
    assert_eq!(
        count_rows(&mut s, "SELECT a, b FROM t"),
        3,
        "extended-protocol prepared writes must replay from the WAL after a crash"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ── minimal MySQL-wire client ─────────────────────────────────────

fn my_read_packet(s: &mut TcpStream) -> Vec<u8> {
    let mut hdr = [0u8; 4];
    s.read_exact(&mut hdr).expect("my header");
    let len = u32::from(hdr[0]) | (u32::from(hdr[1]) << 8) | (u32::from(hdr[2]) << 16);
    let mut payload = vec![0u8; len as usize];
    s.read_exact(&mut payload).expect("my payload");
    payload
}

fn my_write_packet(s: &mut TcpStream, seqno: u8, payload: &[u8]) {
    let len = payload.len() as u32;
    s.write_all(&[len as u8, (len >> 8) as u8, (len >> 16) as u8, seqno])
        .unwrap();
    s.write_all(payload).unwrap();
}

fn my_auth(addr: &str) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let _greeting = my_read_packet(&mut s);
    // open-mode handshake response (empty auth_response accepted).
    let caps: u32 = 0x0000_0200 | 0x0000_8000 | 0x0008_0000;
    let mut p = Vec::new();
    p.extend_from_slice(&caps.to_le_bytes());
    p.extend_from_slice(&16_777_215u32.to_le_bytes());
    p.push(0xff);
    p.extend_from_slice(&[0u8; 23]);
    p.extend_from_slice(b"anyone\0");
    p.push(0);
    p.extend_from_slice(b"mysql_native_password\0");
    my_write_packet(&mut s, 1, &p);
    let ok = my_read_packet(&mut s);
    assert_eq!(ok[0], 0x00, "mysql auth: expected OK, got {:#x}", ok[0]);
    s
}

// COM_QUERY a write; the response is a single OK packet (0x00). A
// durability failure would come back as an ERR packet (0xff).
fn my_write(s: &mut TcpStream, sql: &str) {
    let mut payload = Vec::with_capacity(1 + sql.len());
    payload.push(0x03); // COM_QUERY
    payload.extend_from_slice(sql.as_bytes());
    my_write_packet(s, 0, &payload);
    let resp = my_read_packet(s);
    assert_eq!(
        resp[0], 0x00,
        "mysql COM_QUERY {sql:?}: expected OK (write durable), got {:#x}",
        resp[0]
    );
}

#[test]
fn mysqlwire_writes_survive_crash_via_wal() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let wal = dir.join("wal.log");

    // Phase 1: writes over the MySQL wire (COM_QUERY); kill the daemon.
    {
        let (raw, addrs) = local_spawn(&db, &wal);
        let _g = common::ChildGuard(raw);
        let my = addrs.mysqlwire.clone().expect("mysqlwire enabled");
        let mut s = my_auth(&my);
        my_write(&mut s, "CREATE TABLE m (a TEXT)");
        my_write(&mut s, "INSERT INTO m VALUES ('one')");
        my_write(&mut s, "INSERT INTO m VALUES ('two')");
        // _g drops here → kill -9, no graceful shutdown.
    }

    // Phase 2: fresh daemon replays the WAL; verify over the PG wire
    // (same engine state — any protocol sees the replayed rows).
    let (raw, addrs) = local_spawn(&db, &wal);
    let _g = common::ChildGuard(raw);
    let mut s = connect_pg(&addrs);
    assert_eq!(
        count_rows(&mut s, "SELECT a FROM m"),
        2,
        "mysql-wire writes must replay from the WAL after a crash"
    );

    std::fs::remove_dir_all(&dir).ok();
}
