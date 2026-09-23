//! 9.0.5 — a transaction that writes nothing writes no WAL and does not
//! fsync.
//!
//! PostgreSQL assigns no transaction id and writes no WAL for a
//! transaction that never writes, so `BEGIN; SELECT …; COMMIT;` costs it
//! two round trips and no disk. SPG wrote both ends of the transaction
//! and fsynced at the commit. Measured at one client through pgbench,
//! both engines in containers with the same limits:
//!
//! ```text
//!   BEGIN; SELECT count(*) FROM issues; COMMIT;
//!     SPG 9.0.4                           863 tps
//!     SPG 9.0.4, synchronous_commit=off  3,419 tps
//!     PostgreSQL 18                      4,511 tps
//! ```
//!
//! An ORM wraps its reads in transactions, so that was every read — and
//! the WAL grew from pure reads, which shows up again in recovery time
//! and in the size of a backup.
//!
//! Counted in BYTES OF WAL rather than timed: the bytes are the thing
//! that was wrong, and a timing bound loose enough for a shared machine
//! could not see it. Measured on 9.0.4, ten read-only transactions wrote
//! 940 bytes — more than three real INSERTs, which wrote 225.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use spg_wire::{
    FRAME_HEADER_LEN, Frame, Op, WireValue, build_query, encode, parse_data_row,
    parse_data_row_batch,
};

fn dirs(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = common::tmp_base().join(format!("spg-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    (dir.join("d.spgdb"), dir.join("d.wal"), dir)
}

fn read_frame(stream: &mut TcpStream) -> Frame {
    let mut header = [0u8; FRAME_HEADER_LEN];
    stream.read_exact(&mut header).expect("read header");
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).expect("known op");
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream.read_exact(&mut payload).expect("read payload");
    }
    Frame { op, payload }
}

fn send(stream: &mut TcpStream, sql: &str) -> Vec<Vec<WireValue>> {
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).unwrap();
    stream.write_all(&out).unwrap();
    let mut rows = Vec::new();
    loop {
        let f = read_frame(stream);
        match f.op {
            Op::RowDescription => {}
            Op::DataRow => rows.push(parse_data_row(&f).expect("row")),
            Op::DataRowBatch => rows.extend(parse_data_row_batch(&f).expect("batch")),
            Op::CommandComplete => break,
            other => panic!("{sql}: unexpected {other:?}"),
        }
    }
    rows
}

fn count(stream: &mut TcpStream, sql: &str) -> String {
    match send(stream, sql).first().and_then(|r| r.first()) {
        Some(WireValue::Int(n)) => n.to_string(),
        Some(WireValue::BigInt(n)) => n.to_string(),
        Some(WireValue::Text(t)) => t.clone(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The pgwire handshake, up to the first ReadyForQuery.
fn pg_startup(s: &mut TcpStream, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196608u32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    pg_drain_to_ready(s);
}

/// One simple query, read to its ReadyForQuery. Panics on an
/// ErrorResponse: a statement that did not run cannot be the reason the
/// WAL stayed still.
fn pg_simple(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    if let Some(err) = pg_drain_to_ready(s) {
        panic!("{sql:?} was refused: {err}");
    }
}

fn pg_drain_to_ready(s: &mut TcpStream) -> Option<String> {
    let mut err = None;
    loop {
        let mut header = [0u8; 5];
        s.read_exact(&mut header).expect("pg header");
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut body = vec![0u8; len.saturating_sub(4)];
        if !body.is_empty() {
            s.read_exact(&mut body).expect("pg body");
        }
        if header[0] == b'E' {
            err = Some(String::from_utf8_lossy(&body).to_string());
        }
        if header[0] == b'Z' {
            return err;
        }
    }
}

fn wal_len(wal: &std::path::Path) -> u64 {
    std::fs::metadata(wal).map(|m| m.len()).unwrap_or(0)
}

/// Give the append a moment to reach the file; the assertions are about
/// bytes, and reading before the write lands would report a 0 that says
/// nothing.
fn settled(wal: &std::path::Path) -> u64 {
    std::thread::sleep(Duration::from_millis(200));
    wal_len(wal)
}

#[test]
fn a_read_only_transaction_writes_no_wal() {
    let (db, wal, _dir) = dirs("emptytx");
    let (child, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .arg("-")
        .arg_path(&wal)
        .with_pgwire()
        .spawn();
    let _guard = common::ChildGuard(child);
    let addr = addrs.pgwire.clone().expect("pgwire addr");

    // A table to read from, so the read is a real one.
    {
        let mut s = common::connect_to(&addrs.native);
        send(&mut s, "CREATE TABLE t (id int PRIMARY KEY)");
        send(&mut s, "INSERT INTO t VALUES (1)");
    }

    let mut c = TcpStream::connect(&addr).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    pg_startup(&mut c, "anyone");

    let before = settled(&wal);
    for _ in 0..10 {
        pg_simple(&mut c, "BEGIN");
        pg_simple(&mut c, "SELECT count(*) FROM t");
        pg_simple(&mut c, "COMMIT");
    }
    let after = settled(&wal);
    assert_eq!(
        after,
        before,
        "ten read-only transactions grew the WAL by {} bytes; PostgreSQL writes none",
        after - before
    );

    // The control: a transaction that DOES write still logs, so the
    // assertion above is not passing because nothing reaches the WAL.
    pg_simple(&mut c, "BEGIN");
    pg_simple(&mut c, "INSERT INTO t VALUES (2)");
    pg_simple(&mut c, "COMMIT");
    let wrote = settled(&wal);
    assert!(
        wrote > after,
        "a writing transaction has to log: {after} -> {wrote}"
    );

    // And the row is there, read back on a fresh connection.
    let mut s = common::connect_to(&addrs.native);
    assert_eq!(count(&mut s, "SELECT count(*) FROM t"), "2");
}

#[test]
fn a_read_only_transaction_leaves_a_database_that_restarts() {
    let (db, wal, _dir) = dirs("emptytx-restart");
    let build = || {
        common::ServerBuilder::new()
            .arg_path(&db)
            .arg("-")
            .arg_path(&wal)
            .with_pgwire()
    };

    {
        let (child, addrs) = build().spawn();
        let _guard = common::ChildGuard(child);
        let mut s = common::connect_to(&addrs.native);
        send(&mut s, "CREATE TABLE t (id int PRIMARY KEY)");
        send(&mut s, "INSERT INTO t VALUES (1)");

        let addr = addrs.pgwire.clone().expect("pgwire addr");
        let mut c = TcpStream::connect(&addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        pg_startup(&mut c, "anyone");
        // Read-only transactions, then a writing one AFTER them: the
        // write has to be there on the other side, which is what says
        // the deferred BEGIN was not lost along the way.
        for _ in 0..5 {
            pg_simple(&mut c, "BEGIN");
            pg_simple(&mut c, "SELECT count(*) FROM t");
            pg_simple(&mut c, "COMMIT");
        }
        pg_simple(&mut c, "BEGIN");
        pg_simple(&mut c, "INSERT INTO t VALUES (2)");
        pg_simple(&mut c, "INSERT INTO t VALUES (3)");
        pg_simple(&mut c, "COMMIT");
        std::thread::sleep(Duration::from_millis(200));
    }

    let (child, addrs) = build().spawn();
    let _guard = common::ChildGuard(child);
    let mut s = common::connect_to(&addrs.native);
    assert_eq!(
        count(&mut s, "SELECT count(*) FROM t"),
        "3",
        "all three rows survive the restart"
    );
}

#[test]
fn a_transaction_that_is_rolled_back_after_writing_still_logs_it() {
    // The deferred BEGIN must be written by the first record, so the
    // ROLLBACK that follows has a transaction to undo on replay. If the
    // BEGIN were skipped, replay would apply the write as an autocommit
    // statement and keep it.
    let (db, wal, _dir) = dirs("emptytx-rollback");
    let build = || {
        common::ServerBuilder::new()
            .arg_path(&db)
            .arg("-")
            .arg_path(&wal)
            .with_pgwire()
    };
    {
        let (child, addrs) = build().spawn();
        let _guard = common::ChildGuard(child);
        let mut s = common::connect_to(&addrs.native);
        send(&mut s, "CREATE TABLE t (id int PRIMARY KEY)");

        let addr = addrs.pgwire.clone().expect("pgwire addr");
        let mut c = TcpStream::connect(&addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        pg_startup(&mut c, "anyone");
        pg_simple(&mut c, "BEGIN");
        pg_simple(&mut c, "INSERT INTO t VALUES (7)");
        pg_simple(&mut c, "ROLLBACK");
        std::thread::sleep(Duration::from_millis(200));
    }

    let (child, addrs) = build().spawn();
    let _guard = common::ChildGuard(child);
    let mut s = common::connect_to(&addrs.native);
    assert_eq!(
        count(&mut s, "SELECT count(*) FROM t"),
        "0",
        "the rolled-back row must not come back on replay"
    );
}

/// COPY appends its rows itself, not through `persist_wire_write`. If
/// the deferred `BEGIN` were not written before them, the rows would sit
/// in the log with no transaction around them, and a COPY torn by a
/// crash would replay as a run of separate commits instead of nothing.
#[test]
fn a_copy_inside_a_transaction_keeps_its_envelope() {
    let (db, wal, _dir) = dirs("emptytx-copy");
    let build = || {
        common::ServerBuilder::new()
            .arg_path(&db)
            .arg("-")
            .arg_path(&wal)
            .with_pgwire()
    };
    {
        let (child, addrs) = build().spawn();
        let _guard = common::ChildGuard(child);
        let mut s = common::connect_to(&addrs.native);
        send(&mut s, "CREATE TABLE c (id int)");

        let addr = addrs.pgwire.clone().expect("pgwire addr");
        let mut c = TcpStream::connect(&addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        pg_startup(&mut c, "anyone");
        pg_simple(&mut c, "BEGIN");
        pg_copy_in(&mut c, "COPY c FROM STDIN", "1\n2\n3\n");
        pg_simple(&mut c, "COMMIT");
        std::thread::sleep(Duration::from_millis(200));
    }

    // The log has the transaction the rows belong to.
    let bytes = std::fs::read(&wal).unwrap();
    assert!(
        bytes.windows(5).any(|w| w == b"BEGIN"),
        "the deferred BEGIN has to be written before COPY's rows"
    );

    let (child, addrs) = build().spawn();
    let _guard = common::ChildGuard(child);
    let mut s = common::connect_to(&addrs.native);
    assert_eq!(
        count(&mut s, "SELECT count(*) FROM c"),
        "3",
        "the copied rows survive the restart"
    );
}

/// `COPY … FROM STDIN` over pgwire: the CopyInResponse, the data, and
/// CopyDone, read to ReadyForQuery.
fn pg_copy_in(s: &mut TcpStream, sql: &str, data: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    loop {
        let mut header = [0u8; 5];
        s.read_exact(&mut header).expect("pg header");
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut b = vec![0u8; len.saturating_sub(4)];
        if !b.is_empty() {
            s.read_exact(&mut b).expect("pg body");
        }
        match header[0] {
            b'G' => break,
            b'E' => panic!("{sql} refused: {}", String::from_utf8_lossy(&b)),
            b'Z' => panic!("ReadyForQuery before CopyInResponse for {sql}"),
            _ => {}
        }
    }
    let mut d = Vec::new();
    d.push(b'd');
    d.extend_from_slice(&((data.len() + 4) as u32).to_be_bytes());
    d.extend_from_slice(data.as_bytes());
    d.push(b'c');
    d.extend_from_slice(&4u32.to_be_bytes());
    s.write_all(&d).unwrap();
    if let Some(err) = pg_drain_to_ready(s) {
        panic!("{sql} failed: {err}");
    }
}
