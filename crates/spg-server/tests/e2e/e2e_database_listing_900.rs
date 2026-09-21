//! 9.0.0 — `pg_class` listed another database's relations, and `pg_dump`
//! stopped on its own `LOCK TABLE`.
//!
//! 9.0.0 shipped with this. `information_schema.tables` filtered by the
//! session's database and `pg_class` did not, because the two are built
//! by different walks: the view goes through the visible-relation list,
//! and `pg_class` walks the RAW list so a relation's oid stays tied to
//! its catalog position. The database filter reached one of them.
//!
//! Measured on the published `goliakk/spg:9.0.0`:
//!
//! ```text
//!   psql -d probe -c "SELECT count(*) FROM pg_class WHERE relname='c8t'"   1
//!   pg_dump -d probe
//!     pg_dump: error: query failed: ERROR:  relation "c8t" does not exist
//!     Query was: LOCK TABLE c9a.t, c9b.t, public.c8t IN ACCESS SHARE MODE
//! ```
//!
//! `pg_dump` reads `pg_class` to decide what to lock, so a dump of a
//! server that has ever run `CREATE DATABASE` produced nothing at all.
//!
//! This test is on the WIRE because the defect is: an in-process engine
//! driven with `SET spg.database` does not reproduce it, and a pin
//! written there passes without reaching the fault. The ablation was run
//! here too — with the filter removed, `pg_class` answers 1 and
//! `pg_dump` fails with the sentence above.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
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

fn send_msg(s: &mut TcpStream, ty: u8, body: &[u8]) {
    let mut out = vec![ty];
    out.extend_from_slice(&(body.len() as u32 + 4).to_be_bytes());
    out.extend_from_slice(body);
    s.write_all(&out).expect("write");
}

/// A connection whose startup packet names `database`, which is the only
/// way a session's database is set the way a client sets it.
fn open(addr: &str, database: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let mut startup = Vec::new();
    startup.extend_from_slice(&196_608u32.to_be_bytes());
    for (k, v) in [("user", "postgres"), ("database", database)] {
        startup.extend_from_slice(k.as_bytes());
        startup.push(0);
        startup.extend_from_slice(v.as_bytes());
        startup.push(0);
    }
    startup.push(0);
    let mut framed = ((startup.len() + 4) as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&startup);
    s.write_all(&framed).unwrap();
    loop {
        if read_message(&mut s).ty == b'Z' {
            break;
        }
    }
    s
}

/// The first column of the first row, or the error message.
fn scalar(s: &mut TcpStream, sql: &str) -> String {
    let mut body = Vec::new();
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    send_msg(s, b'Q', &body);
    let mut out = String::new();
    let mut err = None;
    loop {
        let m = read_message(s);
        match m.ty {
            b'D' => {
                // DataRow: i16 field count, then i32 len + bytes.
                let n = i16::from_be_bytes([m.body[0], m.body[1]]);
                if n > 0 && out.is_empty() {
                    let len =
                        i32::from_be_bytes([m.body[2], m.body[3], m.body[4], m.body[5]]) as usize;
                    out = String::from_utf8_lossy(&m.body[6..6 + len]).into_owned();
                }
            }
            b'E' => {
                let mut i = 0;
                while i < m.body.len() && m.body[i] != 0 {
                    let code = m.body[i];
                    let start = i + 1;
                    let end = m.body[start..]
                        .iter()
                        .position(|&b| b == 0)
                        .map_or(m.body.len(), |p| start + p);
                    if code == b'M' {
                        err = Some(String::from_utf8_lossy(&m.body[start..end]).into_owned());
                    }
                    i = end + 1;
                }
            }
            b'Z' => break,
            _ => {}
        }
    }
    err.unwrap_or(out)
}

#[test]
fn pg_class_lists_only_this_databases_relations() {
    let dir = crate::common::tmp_base().join(format!("spg-e2e-dblist-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let addr = addrs.pgwire.as_ref().unwrap();

    let mut home = open(addr, "probe");
    assert_eq!(scalar(&mut home, "CREATE TABLE home_t(a int)"), "");
    assert_eq!(scalar(&mut home, "CREATE DATABASE c8a"), "");

    // A connection to `c8a`, which `CREATE DATABASE` made, makes a
    // relation of its own.
    let mut other = open(addr, "c8a");
    assert_eq!(scalar(&mut other, "CREATE TABLE c8t(id int)"), "");
    assert_eq!(
        scalar(
            &mut other,
            "SELECT count(*) FROM pg_class WHERE relname = 'c8t'"
        ),
        "1"
    );
    assert_eq!(
        scalar(
            &mut other,
            "SELECT count(*) FROM pg_class WHERE relname = 'home_t'"
        ),
        "0",
        "and it does not see the other database's either"
    );

    // …and from `probe` no relation listing names it. `pg_class` is the
    // one `pg_dump` reads to decide what to LOCK.
    for sql in [
        "SELECT count(*) FROM pg_class WHERE relname = 'c8t'",
        "SELECT count(*) FROM information_schema.tables WHERE table_name = 'c8t'",
        "SELECT count(*) FROM pg_tables WHERE tablename = 'c8t'",
        "SELECT count(*) FROM pg_matviews WHERE matviewname = 'c8t'",
    ] {
        assert_eq!(scalar(&mut home, sql), "0", "{sql}");
    }
    // The floor: `probe`'s own relation is listed by the same queries,
    // so a zero above is a filter and not an empty catalog.
    assert_eq!(
        scalar(
            &mut home,
            "SELECT count(*) FROM pg_class WHERE relname = 'home_t'"
        ),
        "1"
    );
    assert_eq!(
        scalar(
            &mut home,
            "SELECT count(*) FROM information_schema.tables WHERE table_name = 'home_t'"
        ),
        "1"
    );
    // The other direction of the same question.
    assert!(
        scalar(&mut home, "SELECT 'c8t'::regclass").contains("does not exist"),
        "a name another database owns does not resolve here"
    );

    // …and neither does its OID. A numeric `::regclass` had a SECOND
    // implementation — a bare positional lookup over the raw relation
    // list — which knew nothing about databases and answered the STORED
    // KEY, so this rendered `c8a`: the key truncated at the separator on
    // its way to the client. PostgreSQL prints the bare number for an
    // oid that names nothing (`SELECT 987654::regclass` is `987654` on
    // 18.6).
    let home_oid = scalar(
        &mut home,
        "SELECT oid FROM pg_class WHERE relname = 'home_t'",
    );
    let other_oid = scalar(&mut other, "SELECT oid FROM pg_class WHERE relname = 'c8t'");
    assert_ne!(home_oid, other_oid, "two relations, two oids");
    assert_eq!(
        scalar(&mut home, &format!("SELECT {home_oid}::regclass")),
        "home_t"
    );
    assert_eq!(
        scalar(&mut home, &format!("SELECT {other_oid}::regclass")),
        other_oid,
        "an oid this database does not hold renders as the number"
    );
    assert_eq!(
        scalar(&mut other, &format!("SELECT {other_oid}::regclass")),
        "c8t"
    );
    assert_eq!(
        scalar(&mut other, &format!("SELECT {home_oid}::regclass")),
        home_oid
    );
}
