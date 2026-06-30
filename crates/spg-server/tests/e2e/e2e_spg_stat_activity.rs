//! v6.5.2 — `spg_stat_activity` virtual table over pgwire.
//!
//! Active pgwire connections register themselves in the server-side
//! registry; the virtual table reads through the engine's
//! activity_provider callback.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn local_spawn(db: &std::path::Path) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .with_pgwire()
        .spawn()
}

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-activity-{label}-{nanos}"));
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
    let body_len = len.saturating_sub(4);
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
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
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn write_msg(buf: &mut Vec<u8>, ty: u8, body: &[u8]) {
    buf.push(ty);
    let len = (body.len() + 4) as u32;
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(body);
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let mut out = Vec::with_capacity(body.len() + 5);
    write_msg(&mut out, b'Q', &body);
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

fn open(addr: &str, user: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, user);
    let _ = read_until_ready(&mut s);
    s
}

#[test]
fn open_connection_appears_in_activity() {
    let dir = unique_tmpdir("one-conn");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap(), "alice");

    send_query(&mut s, "SELECT * FROM spg_stat_activity");
    let msgs = read_until_ready(&mut s);
    // Count DataRow frames.
    let data_rows: Vec<&PgMessage> = msgs.iter().filter(|m| m.ty == b'D').collect();
    // At minimum the connection running this query is in the registry.
    assert!(
        !data_rows.is_empty(),
        "expected at least one row for the open connection"
    );
}

#[test]
fn two_open_connections_each_have_a_row() {
    let dir = unique_tmpdir("two-conn");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);

    let _other = open(addrs.pgwire.as_ref().unwrap(), "bob");
    let mut s = open(addrs.pgwire.as_ref().unwrap(), "alice");

    send_query(&mut s, "SELECT * FROM spg_stat_activity");
    let msgs = read_until_ready(&mut s);
    let data_rows: Vec<&PgMessage> = msgs.iter().filter(|m| m.ty == b'D').collect();
    assert!(
        data_rows.len() >= 2,
        "expected >= 2 rows for two open connections, got {}",
        data_rows.len()
    );
}

#[test]
fn columns_match_design() {
    let dir = unique_tmpdir("cols");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap(), "carol");

    send_query(&mut s, "SELECT * FROM spg_stat_activity");
    let msgs = read_until_ready(&mut s);
    let rd = msgs.iter().find(|m| m.ty == b'T').expect("RowDescription");
    // Body: [u16 cell_count] [name\0 ...] per column.
    let cell_count = u16::from_be_bytes([rd.body[0], rd.body[1]]) as usize;
    assert_eq!(
        cell_count, 9,
        "spg_stat_activity has 9 columns (v7.37.14 B6.3 added wait_event_type)"
    );
}

/// v7.37.14 (B6.3 TDD) — wait_event_type appears immediately
/// before wait_event so projection-by-ordinal stays robust.
#[test]
fn v7_37_14_wait_event_type_column_position() {
    let dir = unique_tmpdir("wet-pos");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap(), "carol");

    send_query(&mut s, "SELECT * FROM spg_stat_activity");
    let msgs = read_until_ready(&mut s);
    let rd = msgs.iter().find(|m| m.ty == b'T').expect("RowDescription");

    // RowDescription body: [u16 cell_count] then per-col:
    //   [name\0][i32 oid][i16 attno][i32 type_oid][i16 type_len]
    //   [i32 type_mod][i16 format]
    let mut cursor = 2usize;
    let mut names: Vec<String> = Vec::new();
    while cursor < rd.body.len() && names.len() < 9 {
        let nul = rd.body[cursor..]
            .iter()
            .position(|&b| b == 0)
            .expect("col name NUL");
        let name = std::str::from_utf8(&rd.body[cursor..cursor + nul])
            .expect("utf8 col name")
            .to_string();
        names.push(name);
        cursor += nul + 1 + 4 + 2 + 4 + 2 + 4 + 2; // name\0 + 18 bytes attrs
    }
    let wet_pos = names
        .iter()
        .position(|n| n == "wait_event_type")
        .expect("wait_event_type column present");
    let we_pos = names
        .iter()
        .position(|n| n == "wait_event")
        .expect("wait_event column present");
    assert_eq!(
        wet_pos + 1,
        we_pos,
        "wait_event_type must immediately precede wait_event (PG order); got names={names:?}"
    );
}

/// v7.37.14 (B6.5 TDD) — `pg_locks` is a queryable SQL surface.
/// Row set is empty until v7.37.15 lands per-row tuple locks; the
/// 9-column schema (locktype / database / relation /
/// virtualtransaction / pid / mode / granted / fastpath /
/// waitstart_us) ships now so adopters can build monitoring
/// queries that survive the v7.37.15 transition unchanged.
#[test]
fn v7_37_14_pg_locks_surface_queryable() {
    let dir = unique_tmpdir("pglocks");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap(), "carol");

    send_query(&mut s, "SELECT * FROM pg_locks");
    let msgs = read_until_ready(&mut s);
    let rd = msgs.iter().find(|m| m.ty == b'T').expect("RowDescription");
    let cell_count = u16::from_be_bytes([rd.body[0], rd.body[1]]) as usize;
    assert_eq!(
        cell_count, 9,
        "pg_locks has 9 columns (locktype, database, relation, virtualtransaction, \
         pid, mode, granted, fastpath, waitstart_us)"
    );
    // Row set empty until v7.37.15 lands tuple locks.
    let data_rows: Vec<&PgMessage> = msgs.iter().filter(|m| m.ty == b'D').collect();
    assert!(
        data_rows.is_empty(),
        "pre-v7.37.15 pg_locks should be empty (single-writer + Arc snapshot — \
         no per-tuple lock chain to enumerate); got {} rows",
        data_rows.len()
    );
}

/// v7.37.14 (B6.5 TDD) — `pg_blocking_pids(pid)` is callable as a
/// scalar SQL function. Returns NULL pre-v7.37.15 (no per-row
/// lock chain to walk) — but the function exists so dashboards
/// can use the standard PG monitoring shape unchanged.
#[test]
fn v7_37_14_pg_blocking_pids_function_callable() {
    let dir = unique_tmpdir("pblock");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap(), "carol");

    send_query(&mut s, "SELECT pg_blocking_pids(1234) AS blockers");
    let msgs = read_until_ready(&mut s);
    let data_rows: Vec<&PgMessage> = msgs.iter().filter(|m| m.ty == b'D').collect();
    assert_eq!(
        data_rows.len(),
        1,
        "pg_blocking_pids returns exactly one row per call (got {} rows)",
        data_rows.len()
    );
    // The single column is NULL pre-v7.37.15.
    // pgwire DataRow body: [u16 col_count][per col: i32 len, len bytes (or -1 = NULL)]
    let body = &data_rows[0].body;
    let col_count = u16::from_be_bytes([body[0], body[1]]);
    assert_eq!(col_count, 1, "one column (the function result)");
    let len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
    assert_eq!(
        len, -1,
        "pre-v7.37.15 pg_blocking_pids returns NULL (sentinel len = -1)"
    );
}

/// v7.37.15 (Phase F TDD) — `spg_stat_mvcc` exposes the engine's
/// MVCC visibility state. Verifies the 3-column schema +
/// single-row response.
#[test]
fn v7_37_15_spg_stat_mvcc_surface_queryable() {
    let dir = unique_tmpdir("mvcc-view");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap(), "carol");

    send_query(&mut s, "SELECT * FROM spg_stat_mvcc");
    let msgs = read_until_ready(&mut s);
    let rd = msgs.iter().find(|m| m.ty == b'T').expect("RowDescription");
    let cell_count = u16::from_be_bytes([rd.body[0], rd.body[1]]) as usize;
    assert_eq!(
        cell_count, 3,
        "spg_stat_mvcc has 3 columns (current_version, active_writer_count, oldest_active_version)"
    );
    let data_rows: Vec<&PgMessage> = msgs.iter().filter(|m| m.ty == b'D').collect();
    assert_eq!(
        data_rows.len(),
        1,
        "spg_stat_mvcc returns exactly one row per call"
    );
}
