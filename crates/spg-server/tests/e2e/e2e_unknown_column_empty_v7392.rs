//! v7.39.2 — an unknown column is refused whatever the row count.
//!
//! The projection resolves its names before the scan; a predicate only
//! met them per ROW. So on an EMPTY table `SELECT a FROM t WHERE nosuch
//! = 1` answered zero rows and no error, and the same statement over a
//! table with one row raised. Measured on PostgreSQL 18.6 and MySQL
//! 9.7.2: both refuse it whatever the count.
//!
//! This is a WIRE test because that is where the defect survived
//! longest. The check went into the SELECT entry first and the
//! PostgreSQL wire still answered zero rows for `WHERE` and `ORDER BY`
//! while raising for `GROUP BY` and `HAVING` — the autocommit SELECT
//! route is a STREAMING one that runs below that entry, and the two
//! clauses that raised are the shapes the shortcut declines. Three
//! separate entries had to be told, and the ACL check had been patched
//! into the same three for the same reason.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_db() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-unkcol-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p.join("d.spgdb")
}

fn pg_msg(s: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("read header");
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len - 4];
    if !body.is_empty() {
        s.read_exact(&mut body).expect("read body");
    }
    (header[0], body)
}

fn pg_connect(addr: &str) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let mut body = Vec::new();
    body.extend_from_slice(&196_608u32.to_be_bytes());
    body.extend_from_slice(b"user\0bench\0\0");
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    loop {
        if pg_msg(&mut s).0 == b'Z' {
            break;
        }
    }
    s
}

/// Run one simple query. `Ok(row_count)` or `Err(error text)`.
fn run(s: &mut TcpStream, sql: &str) -> Result<usize, String> {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    let mut rows = 0usize;
    let mut err: Option<String> = None;
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'D' => rows += 1,
            b'E' => err = Some(String::from_utf8_lossy(&body).into_owned()),
            b'Z' => {
                return match err {
                    Some(e) => Err(e),
                    None => Ok(rows),
                };
            }
            _ => {}
        }
    }
}

#[test]
fn an_unknown_column_is_refused_on_an_empty_table() {
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&unique_db())
        .with_pgwire()
        .spawn();
    let _guard = common::ChildGuard(raw);
    let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
    run(&mut s, "CREATE TABLE ec (a int)").expect("create");

    // Every clause, on a table with NO rows. `WHERE` and `ORDER BY` are
    // the two the streaming route serves, and the two that stayed wrong
    // after the SELECT entry was told.
    for sql in [
        "SELECT a FROM ec WHERE nosuch = 1",
        "SELECT a FROM ec ORDER BY nosuch",
        "SELECT a FROM ec GROUP BY nosuch",
        "SELECT a FROM ec GROUP BY a HAVING nosuch > 1",
        "SELECT nosuch FROM ec",
        "SELECT * FROM ec WHERE nosuch = 1",
    ] {
        let got = run(&mut s, sql);
        assert!(
            got.as_ref().is_err_and(|e| e.contains("nosuch")),
            "{sql} must be refused with no rows in the table, got {got:?}"
        );
    }

    // The control, and the reason the walk is narrow: a query whose
    // names all resolve still runs, empty or not.
    assert_eq!(run(&mut s, "SELECT a FROM ec WHERE a = 1"), Ok(0));
    assert_eq!(run(&mut s, "SELECT a FROM ec ORDER BY a"), Ok(0));
    // A system column is not in the table's list and is a good predicate.
    assert_eq!(
        run(&mut s, "SELECT a FROM ec WHERE ctid = '(0,1)'::tid"),
        Ok(0)
    );
    // And with a row present, which is how it always behaved.
    run(&mut s, "INSERT INTO ec VALUES (1)").expect("insert");
    assert_eq!(run(&mut s, "SELECT a FROM ec WHERE a = 1"), Ok(1));
    assert!(run(&mut s, "SELECT a FROM ec WHERE nosuch = 1").is_err());
}
