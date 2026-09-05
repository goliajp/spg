//! v7.40.0 — the wire OIDs of the six array types this version adds.
//!
//! A type that exists in the engine still reaches a driver only if
//! `RowDescription` names it. That table is hand-kept, so the pin has
//! to be taken on the wire, not in the engine: the engine's own
//! `pg_typeof` reads a DIFFERENT table and would report `real[]` for a
//! column the wire was still advertising as text.
//!
//! Every OID below was read off PostgreSQL 18.6's `pg_type`
//! (`_float4` 1021, `_time` 1183, `_timetz` 1270, `_inet` 1041,
//! `_xml` 143, `_oid` 1028) before it was written down.

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
    let p = crate::common::tmp_base().join(format!("spg-e2e-arr-oid-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

struct PgMessage {
    ty: u8,
    body: Vec<u8>,
}

fn read_message(s: &mut TcpStream) -> PgMessage {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("pg header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let body_len = len.saturating_sub(4);
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
        s.read_exact(&mut body).expect("pg body");
    }
    PgMessage { ty, body }
}

fn send_startup(s: &mut TcpStream, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608u32.to_be_bytes());
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

fn handshake(addr: &str) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "anyone");
    assert_eq!(read_message(&mut s).ty, b'R');
    loop {
        if read_message(&mut s).ty == b'Z' {
            break;
        }
    }
    s
}

/// Run a simple query and return the type OID of each column in the
/// `RowDescription`, or an empty vec when the server sent none.
fn column_type_oids(s: &mut TcpStream, sql: &str) -> Vec<u32> {
    let mut q = Vec::new();
    q.push(b'Q');
    let mut body = sql.as_bytes().to_vec();
    body.push(0);
    q.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
    q.extend_from_slice(&body);
    s.write_all(&q).unwrap();

    let mut oids = Vec::new();
    loop {
        let m = read_message(s);
        match m.ty {
            b'T' => {
                let n = u16::from_be_bytes([m.body[0], m.body[1]]) as usize;
                let mut at = 2usize;
                for _ in 0..n {
                    // name (NUL-terminated), then table OID (4) + column
                    // attnum (2), then the type OID.
                    let end = m.body[at..].iter().position(|b| *b == 0).unwrap() + at;
                    at = end + 1 + 6;
                    oids.push(u32::from_be_bytes([
                        m.body[at],
                        m.body[at + 1],
                        m.body[at + 2],
                        m.body[at + 3],
                    ]));
                    // type OID (4) + typlen (2) + typmod (4) + format (2).
                    at += 12;
                }
            }
            b'E' => panic!("{sql}: server returned an error"),
            b'Z' => return oids,
            _ => {}
        }
    }
}

#[test]
fn the_new_array_columns_advertise_their_postgres_oids() {
    let dir = unique_tmpdir("oids");
    let db = dir.join("spg.db");
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut s = handshake(addrs.pgwire.as_ref().unwrap());

    assert!(
        column_type_oids(
            &mut s,
            "CREATE TABLE t (a real[], b time[], c timetz[], d inet[], e xml[], f oid[])",
        )
        .is_empty()
    );
    assert_eq!(
        column_type_oids(&mut s, "SELECT a, b, c, d, e, f FROM t"),
        [1021, 1183, 1270, 1041, 143, 1028],
        "RowDescription must carry PostgreSQL 18.6's array OIDs"
    );
    // The fence: the array OIDs that already worked still do.
    assert_eq!(
        column_type_oids(
            &mut s,
            "SELECT '{1}'::int[], '{a}'::text[], '{1.5}'::float8[], '{2024-01-01}'::date[]",
        ),
        [1007, 1009, 1022, 1182]
    );
}
