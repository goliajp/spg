//! v7.38.19 — a database keeps the collation it was created with, even
//! when the environment on the next start says otherwise.
//!
//! `set_db_collation` already refused to change a collation once the
//! database had tables — every index key was built under the old one.
//! That guard is only as good as what the catalog holds when it runs,
//! and it ran BEFORE the WAL was replayed.
//!
//! A server killed without checkpointing — which is every crash and
//! every plain `kill` — starts from an empty catalog and rebuilds
//! itself from the WAL. So the guard saw no tables, the environment
//! won, and then replay brought the rows back under a collation nobody
//! chose. Measured before the fix: a database created under `C`
//! answering `Bob,Zebra,apple`, restarted from the same directory with
//! `SPG_LC_COLLATE=en_US.utf8`, came back declaring `en_US.utf8` and
//! answering `apple,Bob,Zebra`.
//!
//! Every text sort in the database silently changed answer because an
//! environment variable did. PostgreSQL cannot do this: `initdb` writes
//! the collation into the cluster and a later `LANG` does not touch it.

use crate::common;
use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

use spg_wire::{
    FRAME_HEADER_LEN, Frame, Op, WireValue, build_query, encode, parse_data_row,
    parse_data_row_batch,
};

fn read_frame(stream: &mut TcpStream) -> Frame {
    use std::io::Read;
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

fn first(stream: &mut TcpStream, sql: &str) -> String {
    match send(stream, sql).first().and_then(|r| r.first()) {
        Some(WireValue::Text(t)) => t.clone(),
        other => panic!("{sql}: expected text, got {other:?}"),
    }
}

#[test]
fn an_existing_database_keeps_its_collation() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = common::tmp_base().join(format!("spg-coll-restart-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("d.spgdb");
    let wal = dir.join("d.wal");

    let build = || {
        common::ServerBuilder::new()
            .arg_path(&db)
            .arg("-")
            .arg_path(&wal)
    };

    // Created under `C`, then killed WITHOUT a checkpoint, which is what
    // leaves the next start rebuilding from the WAL.
    let (child, addrs) = build().env("SPG_LC_COLLATE", "C").spawn();
    {
        let _guard = common::ChildGuard(child);
        let mut s = common::connect_to(&addrs.native);
        send(&mut s, "CREATE TABLE t (s text)");
        send(&mut s, "INSERT INTO t VALUES ('apple'),('Bob'),('Zebra')");
        assert_eq!(
            first(&mut s, "SELECT string_agg(s, ',' ORDER BY s) FROM t"),
            "Bob,Zebra,apple",
            "byte order is what `C` gives"
        );
        // Dropped here: killed, with no checkpoint, which is the state
        // the next start has to survive.
    }
    std::thread::sleep(Duration::from_millis(300));

    // The same directory, an environment asking for something else.
    let (child2, addrs2) = build().env("SPG_LC_COLLATE", "en_US.utf8").spawn();
    let _guard = common::ChildGuard(child2);
    let mut s = common::connect_to(&addrs2.native);
    assert_eq!(
        first(
            &mut s,
            "SELECT datcollate FROM pg_database WHERE datname = current_database()"
        ),
        "C",
        "the environment must not redeclare a database that already exists"
    );
    assert_eq!(
        first(&mut s, "SELECT string_agg(s, ',' ORDER BY s) FROM t"),
        "Bob,Zebra,apple",
        "and every row must still sort the way it did before the restart"
    );
}
