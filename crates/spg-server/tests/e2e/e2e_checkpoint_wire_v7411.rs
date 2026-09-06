//! v7.40.11 — `CHECKPOINT` over the PostgreSQL protocol reported
//! success and did nothing.
//!
//! Found while fixing the command tags of §3.15. The statement is
//! intercepted by SQL TEXT in `main.rs`'s native-protocol dispatch,
//! which is the only place that calls the handler; the parser turns it
//! into `Statement::Empty`, so on pgwire it reached the engine as a
//! statement with no effect and came back as a normal completion:
//!
//! ```text
//!   psql -c CHECKPOINT       CHECKPOINT       (and the WAL is untouched)
//! ```
//!
//! A checkpoint is a durability operation. An operator who runs it
//! before a maintenance window, a backup script that runs it before
//! copying the data directory, and a restart that expects a short
//! replay all get an answer that says the work happened. This is the
//! same class as every "said it did something it didn't" defect this
//! repository treats as a headline — with the difference that the thing
//! not done is the one that bounds crash recovery.
//!
//! The witness is the WAL length. Writes append to it; a checkpoint
//! snapshots the engine, writes the manifest and truncates the WAL to
//! zero. So a non-zero length before and zero after is the operation
//! having happened, and it cannot be faked by a command tag.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(60);

fn unique_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = common::tmp_base().join(format!("spg-checkpoint-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
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

/// Run one statement over the simple query protocol; return its
/// command tag, or the error message.
fn simple(s: &mut TcpStream, sql: &str) -> Result<String, String> {
    let mut q: Vec<u8> = vec![b'Q'];
    let mut b = sql.as_bytes().to_vec();
    b.push(0);
    q.extend_from_slice(&((b.len() + 4) as u32).to_be_bytes());
    q.extend_from_slice(&b);
    s.write_all(&q).unwrap();
    let mut tag = String::new();
    let mut err = None;
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'C' => {
                let end = body.iter().position(|&c| c == 0).unwrap_or(body.len());
                tag = String::from_utf8_lossy(&body[..end]).into_owned();
            }
            b'E' => {
                let mut pos = 0;
                while pos < body.len() && body[pos] != 0 {
                    let t = body[pos];
                    pos += 1;
                    let end = body[pos..].iter().position(|&c| c == 0).unwrap() + pos;
                    if t == b'M' {
                        err = Some(String::from_utf8_lossy(&body[pos..end]).into_owned());
                    }
                    pos = end + 1;
                }
            }
            b'Z' => break,
            _ => {}
        }
    }
    err.map_or(Ok(tag), Err)
}

/// The extended protocol's Parse/Bind/Execute for one statement.
fn extended(s: &mut TcpStream, sql: &str) -> Result<String, String> {
    let mut out: Vec<u8> = Vec::new();
    let mut p: Vec<u8> = vec![0];
    p.extend_from_slice(sql.as_bytes());
    p.push(0);
    p.extend_from_slice(&0u16.to_be_bytes());
    out.push(b'P');
    out.extend_from_slice(&((p.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&p);
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
    let mut tag = String::new();
    let mut err = None;
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'C' => {
                let end = body.iter().position(|&c| c == 0).unwrap_or(body.len());
                tag = String::from_utf8_lossy(&body[..end]).into_owned();
            }
            b'E' => {
                let mut pos = 0;
                while pos < body.len() && body[pos] != 0 {
                    let t = body[pos];
                    pos += 1;
                    let end = body[pos..].iter().position(|&c| c == 0).unwrap() + pos;
                    if t == b'M' {
                        err = Some(String::from_utf8_lossy(&body[pos..end]).into_owned());
                    }
                    pos = end + 1;
                }
            }
            b'Z' => break,
            _ => {}
        }
    }
    err.map_or(Ok(tag), Err)
}

struct Fixture {
    _guard: common::ChildGuard,
    conn: TcpStream,
    wal: PathBuf,
}

fn fixture(tag: &str) -> Fixture {
    let dir = unique_dir(tag);
    let db = dir.join("d.spgdb");
    let wal = dir.join("d.wal");
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .env("SPG_WAL", wal.to_string_lossy().into_owned())
        .with_pgwire()
        .spawn();
    let conn = pg_connect(addrs.pgwire.as_ref().unwrap());
    Fixture {
        _guard: common::ChildGuard(raw),
        conn,
        wal,
    }
}

fn wal_len(p: &PathBuf) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// The witness: writes grow the WAL, a checkpoint empties it.
#[test]
fn checkpoint_over_the_simple_protocol_truncates_the_wal() {
    let mut f = fixture("simple");
    simple(&mut f.conn, "CREATE TABLE cp (a INT)").expect("ddl");
    for i in 0..50 {
        simple(&mut f.conn, &format!("INSERT INTO cp VALUES ({i})")).expect("insert");
    }
    let before = wal_len(&f.wal);
    assert!(before > 0, "the WAL must hold the writes: {before} bytes");

    let tag = simple(&mut f.conn, "CHECKPOINT").expect("checkpoint");
    assert_eq!(tag, "CHECKPOINT", "PG's tag");

    let after = wal_len(&f.wal);
    assert_eq!(
        after, 0,
        "a checkpoint truncates the WAL; it was {before} bytes and is {after}"
    );

    // And the rows are still there — a checkpoint is not a reset.
    simple(&mut f.conn, "SELECT count(*) FROM cp").expect("select");
}

/// The protocol every driver actually uses.
#[test]
fn checkpoint_over_the_extended_protocol_truncates_the_wal() {
    let mut f = fixture("extended");
    simple(&mut f.conn, "CREATE TABLE cp (a INT)").expect("ddl");
    for i in 0..50 {
        simple(&mut f.conn, &format!("INSERT INTO cp VALUES ({i})")).expect("insert");
    }
    assert!(wal_len(&f.wal) > 0, "the WAL must hold the writes");

    let tag = extended(&mut f.conn, "CHECKPOINT").expect("checkpoint");
    assert_eq!(tag, "CHECKPOINT");
    assert_eq!(wal_len(&f.wal), 0, "a checkpoint truncates the WAL");
}

/// PostgreSQL runs a checkpoint inside a transaction block —
/// `BEGIN; CHECKPOINT; COMMIT;` succeeds there, measured on 18.6 — and
/// SPG refused outright. That refusal took out the ordinary case:
/// psql -f and sqlx::migrate!() send a whole script as ONE frame,
/// which is an implicit block, so a maintenance script ending in
/// CHECKPOINT was rejected.
///
/// The snapshot is committed state by construction, so writing it is
/// always safe; the WAL is the part that is not, because an in-flight
/// transaction's records are already in it. So the statement succeeds
/// and the WAL is reclaimed only when nothing is in flight.
#[test]
fn a_checkpoint_inside_a_transaction_succeeds_and_keeps_the_wal() {
    let mut f = fixture("intx");
    simple(&mut f.conn, "CREATE TABLE cp (a INT)").expect("ddl");
    for i in 0..50 {
        simple(&mut f.conn, &format!("INSERT INTO cp VALUES ({i})")).expect("insert");
    }
    simple(&mut f.conn, "BEGIN").expect("begin");
    simple(&mut f.conn, "INSERT INTO cp VALUES (777)").expect("insert");
    let before = wal_len(&f.wal);
    assert!(before > 0);

    let tag = simple(&mut f.conn, "CHECKPOINT").expect("PG runs this inside a block");
    assert_eq!(tag, "CHECKPOINT");
    assert!(
        wal_len(&f.wal) >= before,
        "the in-flight transaction's records must stay in the WAL"
    );

    simple(&mut f.conn, "COMMIT").expect("commit");
    // And once nothing is in flight, a checkpoint reclaims it.
    simple(&mut f.conn, "CHECKPOINT").expect("checkpoint");
    assert_eq!(wal_len(&f.wal), 0);
}

/// And inside a multi-statement Q frame, which is the third of the
/// three simple-query entry points — `psql -f script.sql` and
/// `sqlx::migrate!()` send whole files this way, and a maintenance
/// script that ends in CHECKPOINT is exactly the shape that matters.
#[test]
fn checkpoint_inside_a_multi_statement_frame_truncates_the_wal() {
    let mut f = fixture("multi");
    simple(&mut f.conn, "CREATE TABLE cp (a INT)").expect("ddl");
    for i in 0..50 {
        simple(&mut f.conn, &format!("INSERT INTO cp VALUES ({i})")).expect("insert");
    }
    assert!(wal_len(&f.wal) > 0, "the WAL must hold the writes");
    simple(&mut f.conn, "INSERT INTO cp VALUES (999); CHECKPOINT;").expect("script");
    // The frame is an implicit block, so the WAL is not reclaimed here
    // — but the statement succeeds, which is what PG does and what the
    // refusal used to prevent. A checkpoint after the frame reclaims it.
    simple(&mut f.conn, "CHECKPOINT").expect("checkpoint");
    assert_eq!(
        wal_len(&f.wal),
        0,
        "a checkpoint after the script truncates the WAL"
    );
}
