//! v7.40.11 — over the extended protocol, `SHOW` and `EXPLAIN` sent
//! rows with no header, and every utility statement was tagged `OK`.
//!
//! Reported against 7.40.9 (§3.15). psql names the first one exactly:
//!
//! ```text
//!   server sent data ("D" message) without prior row description ("T" message)
//! ```
//!
//! sqlx surfaces it as a row with zero columns
//! (`ColumnIndexOutOfBounds { index: 0, len: 0 }`), which is how the
//! reporter found it: their timezone readback was `SHOW TimeZone`. The
//! simple query protocol is correct throughout, which is why nothing
//! found this before — psql uses it by default and every driver does
//! not.
//!
//! The command tag is the second half. Measured on PostgreSQL 18.6
//! through Parse/Bind/Execute, against SPG 7.40.10:
//!
//! ```text
//!                        PG 18.6      SPG            PG 18.6   SPG
//!   SET work_mem         SET          OK   COMMENT   COMMENT   OK
//!   RESET work_mem       RESET        OK   GRANT     GRANT     OK
//!   RESET ALL            RESET        OK   REVOKE    REVOKE    OK
//!   DISCARD ALL          DISCARD ALL  OK   PREPARE   PREPARE   OK
//!   ANALYZE              ANALYZE      OK   DEALLOC   DEALLOCATE OK
//!   VACUUM               VACUUM       OK
//! ```
//!
//! A tag is not decoration: psycopg and JDBC read it to decide whether
//! a statement returned rows, and pgbouncer reads it to track
//! transaction state. The simple path derives it from the SQL's first
//! word and got them all right; the extended path derives it from the
//! AST and had no arms for any of them.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(60);

fn unique_db() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = common::tmp_base().join(format!("spg-utility-shape-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p.join("d.spgdb")
}

fn pg_msg(s: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("pg header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len.saturating_sub(4)];
    if !body.is_empty() {
        s.read_exact(&mut body).expect("pg body");
    }
    (ty, body)
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

fn simple(s: &mut TcpStream, sql: &str) {
    let mut q: Vec<u8> = vec![b'Q'];
    let mut b = sql.as_bytes().to_vec();
    b.push(0);
    q.extend_from_slice(&((b.len() + 4) as u32).to_be_bytes());
    q.extend_from_slice(&b);
    s.write_all(&q).unwrap();
    loop {
        if pg_msg(s).0 == b'Z' {
            break;
        }
    }
}

/// What one Parse / Describe(S) / Bind / Execute / Sync produced.
struct Run {
    /// Column names from the RowDescription, if one arrived.
    described: Option<Vec<String>>,
    /// True if a DataRow arrived before any RowDescription.
    data_without_header: bool,
    rows: usize,
    tag: String,
    error: Option<String>,
}

fn parse_describe_execute(s: &mut TcpStream, sql: &str) -> Run {
    let mut out: Vec<u8> = Vec::new();
    let mut p: Vec<u8> = vec![0];
    p.extend_from_slice(sql.as_bytes());
    p.push(0);
    p.extend_from_slice(&0u16.to_be_bytes());
    out.push(b'P');
    out.extend_from_slice(&((p.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&p);

    let d: Vec<u8> = vec![b'S', 0];
    out.push(b'D');
    out.extend_from_slice(&((d.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&d);

    let b: Vec<u8> = vec![0, 0, 0, 0, 0, 0, 0, 0];
    out.push(b'B');
    out.extend_from_slice(&((b.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&b);

    let mut e: Vec<u8> = vec![0];
    e.extend_from_slice(&0u32.to_be_bytes());
    out.push(b'E');
    out.extend_from_slice(&((e.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&e);

    out.push(b'S');
    out.extend_from_slice(&4u32.to_be_bytes());
    s.write_all(&out).unwrap();

    let mut run = Run {
        described: None,
        data_without_header: false,
        rows: 0,
        tag: String::new(),
        error: None,
    };
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'T' => {
                let n = u16::from_be_bytes([body[0], body[1]]) as usize;
                let mut pos = 2;
                let mut names = Vec::new();
                for _ in 0..n {
                    let end = body[pos..].iter().position(|&c| c == 0).unwrap() + pos;
                    names.push(String::from_utf8_lossy(&body[pos..end]).into_owned());
                    pos = end + 1 + 18;
                }
                run.described = Some(names);
            }
            b'D' => {
                if run.described.is_none() {
                    run.data_without_header = true;
                }
                run.rows += 1;
            }
            b'C' => {
                let end = body.iter().position(|&c| c == 0).unwrap_or(body.len());
                run.tag = String::from_utf8_lossy(&body[..end]).into_owned();
            }
            b'E' => {
                let mut pos = 0;
                while pos < body.len() && body[pos] != 0 {
                    let tag = body[pos];
                    pos += 1;
                    let end = body[pos..].iter().position(|&c| c == 0).unwrap() + pos;
                    if tag == b'M' {
                        run.error = Some(String::from_utf8_lossy(&body[pos..end]).into_owned());
                    }
                    pos = end + 1;
                }
            }
            b'Z' => return run,
            _ => {}
        }
    }
}

fn server() -> (std::process::Child, TcpStream) {
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&unique_db())
        .with_pgwire()
        .spawn();
    let s = pg_connect(addrs.pgwire.as_ref().unwrap());
    (raw, s)
}

/// The invariant a client enforces: never a DataRow without a
/// RowDescription in front of it.
#[test]
fn every_row_producing_statement_sends_its_header_first() {
    let (raw, mut s) = server();
    let _guard = common::ChildGuard(raw);

    // (sql, the column names PG 18.6 describes)
    let cases: &[(&str, &[&str])] = &[
        ("SHOW TimeZone", &["TimeZone"]),
        ("SHOW work_mem", &["work_mem"]),
        ("SHOW ALL", &["name", "setting", "description"]),
        (
            "SHOW TRANSACTION ISOLATION LEVEL",
            &["transaction_isolation"],
        ),
        ("EXPLAIN SELECT 1", &["QUERY PLAN"]),
        ("EXPLAIN (FORMAT JSON) SELECT 1", &["QUERY PLAN"]),
        ("SELECT 1", &["?column?"]),
    ];
    for (sql, want) in cases {
        let run = parse_describe_execute(&mut s, sql);
        assert!(run.error.is_none(), "{sql}: {:?}", run.error);
        assert!(
            !run.data_without_header,
            "{sql}: a DataRow before any RowDescription — psql refuses this stream"
        );
        assert_eq!(
            run.described.as_deref(),
            Some(
                want.iter()
                    .map(|c| (*c).to_string())
                    .collect::<Vec<_>>()
                    .as_slice()
            ),
            "{sql}: described columns"
        );
        assert!(run.rows > 0, "{sql}: no rows at all");
    }
}

/// The command tag, which the simple path already got right.
#[test]
fn a_utility_statement_carries_pgs_command_tag() {
    let (raw, mut s) = server();
    let _guard = common::ChildGuard(raw);
    simple(&mut s, "CREATE TABLE ut (a INT)");

    // (sql, PG 18.6's tag)
    let cases: &[(&str, &str)] = &[
        ("SET work_mem = '5MB'", "SET"),
        ("RESET work_mem", "RESET"),
        ("RESET ALL", "RESET"),
        ("SET TIME ZONE 'UTC'", "SET"),
        ("DISCARD ALL", "DISCARD ALL"),
        ("ANALYZE", "ANALYZE"),
        ("VACUUM", "VACUUM"),
        ("COMMENT ON TABLE ut IS 'x'", "COMMENT"),
        ("GRANT SELECT ON ut TO PUBLIC", "GRANT"),
        ("REVOKE SELECT ON ut FROM PUBLIC", "REVOKE"),
        ("PREPARE utp AS SELECT 1", "PREPARE"),
        ("DEALLOCATE utp", "DEALLOCATE"),
    ];
    for (sql, want) in cases {
        let run = parse_describe_execute(&mut s, sql);
        assert!(run.error.is_none(), "{sql}: {:?}", run.error);
        assert_eq!(&run.tag, want, "{sql}");
    }
}

/// And the tags that were already right, so the new arms cannot have
/// displaced one.
#[test]
fn the_tags_that_were_already_right_still_are() {
    let (raw, mut s) = server();
    let _guard = common::ChildGuard(raw);
    simple(&mut s, "CREATE TABLE ut2 (a INT)");
    for (sql, want) in [
        ("INSERT INTO ut2 VALUES (1)", "INSERT 0 1"),
        ("UPDATE ut2 SET a = 2", "UPDATE 1"),
        ("DELETE FROM ut2", "DELETE 1"),
        ("SELECT 1", "SELECT 1"),
        ("BEGIN", "BEGIN"),
        ("COMMIT", "COMMIT"),
        ("CREATE INDEX ut2_a ON ut2 (a)", "CREATE INDEX"),
        ("DROP TABLE ut2", "DROP TABLE"),
    ] {
        let run = parse_describe_execute(&mut s, sql);
        assert!(run.error.is_none(), "{sql}: {:?}", run.error);
        assert_eq!(&run.tag, want, "{sql}");
    }
}
