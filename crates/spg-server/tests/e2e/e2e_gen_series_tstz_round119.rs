//! v7.39 (read01 round 119, B4) — a top-level `SELECT generate_series(date,
//! date, interval)` yields `timestamp with time zone` (OID 1184) with a `+00`
//! offset, matching PG. The FROM-clause form was already correct; the
//! SELECT-list form derived its column type from the runtime `Value::Timestamp`
//! (SPG has no scalar `Value::Timestamptz`), dropping the offset. Verified over
//! the real pgwire protocol (the embedded probe renders the bare value and
//! cannot show the offset).

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);
const OID_TIMESTAMPTZ: i32 = 1184;
const OID_TIMESTAMP: i32 = 1114;

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
    let p = std::env::temp_dir().join(format!("spg-e2e-gstz-{label}-{nanos}"));
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

fn send_msg(s: &mut TcpStream, ty: u8, body: &[u8]) {
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.push(ty);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(body);
    s.write_all(&out).unwrap();
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
    let mut body = Vec::new();
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    send_msg(s, b'Q', &body);
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

/// First column's type OID from the RowDescription (`T`).
fn first_col_oid(rowdesc: &[u8]) -> i32 {
    // int16 field count, then per field: name\0, tableoid(i32), attnum(i16),
    // typeoid(i32), ...
    let mut p = 2;
    while rowdesc[p] != 0 {
        p += 1;
    }
    p += 1; // past name NUL
    p += 4 + 2; // tableoid + attnum
    i32::from_be_bytes([rowdesc[p], rowdesc[p + 1], rowdesc[p + 2], rowdesc[p + 3]])
}

/// All DataRow (`D`) first-cell text values.
fn cells(msgs: &[PgMessage]) -> Vec<String> {
    msgs.iter()
        .filter(|m| m.ty == b'D')
        .map(|m| {
            let body = &m.body;
            let len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
            assert!(len >= 0, "NULL cell");
            String::from_utf8_lossy(&body[6..6 + len as usize]).into_owned()
        })
        .collect()
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "admin");
    let _ = read_until_ready(&mut s);
    s
}

#[test]
fn top_level_generate_series_over_date_is_timestamptz() {
    let dir = unique_tmpdir("date");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    send_query(
        &mut s,
        "SELECT generate_series('2024-01-01'::date, '2024-01-02'::date, '1 day'::interval)",
    );
    let msgs = read_until_ready(&mut s);
    let t = msgs.iter().find(|m| m.ty == b'T').expect("RowDescription");
    assert_eq!(
        first_col_oid(&t.body),
        OID_TIMESTAMPTZ,
        "date bounds → timestamptz"
    );
    assert_eq!(
        cells(&msgs),
        vec!["2024-01-01 00:00:00+00", "2024-01-02 00:00:00+00"]
    );
}

#[test]
fn top_level_generate_series_over_timestamp_stays_timestamp() {
    let dir = unique_tmpdir("ts");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    send_query(
        &mut s,
        "SELECT generate_series('2024-01-01'::timestamp, '2024-01-02'::timestamp, '1 day'::interval)",
    );
    let msgs = read_until_ready(&mut s);
    let t = msgs.iter().find(|m| m.ty == b'T').expect("RowDescription");
    assert_eq!(
        first_col_oid(&t.body),
        OID_TIMESTAMP,
        "timestamp bounds stay timestamp"
    );
    assert_eq!(
        cells(&msgs),
        vec!["2024-01-01 00:00:00", "2024-01-02 00:00:00"]
    );
}
