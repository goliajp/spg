//! 9.0.4 — what a statement drew from the clock and the random number
//! generator is what it still holds after a restart.
//!
//! SPG's WAL records SQL TEXT, so recovery runs the statement again.
//! Measured on the published 9.0.3 against PostgreSQL 18.6:
//!
//! ```text
//!   CREATE TABLE n (id int PRIMARY KEY,
//!                   lit timestamptz, ts timestamptz DEFAULT now());
//!   INSERT INTO n (id, lit) VALUES (1, '2020-01-02 03:04:05.123456+00');
//!
//!                            lit                     ts
//!   written                  2020-01-02 03:04:05     2026-09-22 21:48:25.745648+00
//!   after a restart          2020-01-02 03:04:05     2026-09-22 21:48:26.517822+00
//!   after another            2020-01-02 03:04:05     2026-09-22 21:48:27.422939+00
//!   after a CHECKPOINT       2020-01-02 03:04:05     2026-09-22 21:48:27.422939+00
//! ```
//!
//! The literal stayed; `now()` moved to the moment of recovery, again on
//! the next restart, and settled only once a CHECKPOINT had written the
//! row out. So a stopped-and-copied data directory held different data
//! from the one it was copied from — found by `xtests/gates/g5-backup.sh`,
//! which compares table CONTENTS rather than row counts.
//!
//! `gen_random_uuid()` and `random()` looked as though they survived.
//! They did not survive; they were REDERIVED to the same values, because
//! the PRNG started every process from compile-time constants. Two
//! servers from the same image answered `gen_random_uuid()` with one
//! uuid:
//!
//! ```text
//!   SPG 9.0.3 instance 1   52911af6-eed6-4042-8068-81cc36732d62  0.6090247626271179
//!   SPG 9.0.3 instance 2   52911af6-eed6-4042-8068-81cc36732d62  0.6090247626271179
//!   PostgreSQL 18.6 #1     06a08118-e0c1-4f7f-a259-2703ecee2cf3  0.06221011206089533
//!   PostgreSQL 18.6 #2     59d95124-b73d-432d-b195-69ca56f7ea90  0.8876655586216671
//! ```
//!
//! The two are one fix: seeding the PRNG per process without recording
//! what a statement drew would make recovery invent DIFFERENT uuids,
//! which is worse than a moved timestamp — `events.id` is a
//! `gen_random_uuid()` primary key in the customer's schema.

use crate::common;
use std::time::Duration;

use spg_wire::{
    FRAME_HEADER_LEN, Frame, Op, WireValue, build_query, encode, parse_data_row,
    parse_data_row_batch,
};

fn read_frame(stream: &mut std::net::TcpStream) -> Frame {
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

fn send(stream: &mut std::net::TcpStream, sql: &str) -> Vec<Vec<WireValue>> {
    use std::io::Write;
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

/// Every row's columns joined by `|`, one row per line — so a
/// comparison is over the VALUES, not over a count.
fn table(stream: &mut std::net::TcpStream, sql: &str) -> String {
    send(stream, sql)
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    WireValue::Text(t) => t.clone(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const READ: &str = "SELECT id, lit, ts, u, r FROM n ORDER BY id";

#[test]
fn a_restart_gives_back_the_values_that_were_written() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = common::tmp_base().join(format!("spg-volatile-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("d.spgdb");
    let wal = dir.join("d.wal");
    let build = || {
        common::ServerBuilder::new()
            .arg_path(&db)
            .arg("-")
            .arg_path(&wal)
    };

    // Killed without a checkpoint, which is what leaves the next start
    // rebuilding from the WAL — and the only state where this was ever
    // visible.
    let written;
    let (child, addrs) = build().spawn();
    {
        let _guard = common::ChildGuard(child);
        let mut s = common::connect_to(&addrs.native);
        send(
            &mut s,
            "CREATE TABLE n (id int PRIMARY KEY, lit timestamptz, \
             ts timestamptz DEFAULT now(), u uuid DEFAULT gen_random_uuid(), \
             r double precision DEFAULT random())",
        );
        send(
            &mut s,
            "INSERT INTO n (id, lit) VALUES \
             (1, '2020-01-02 03:04:05.123456+00'), (2, '2020-01-02 03:04:05.123456+00')",
        );
        written = table(&mut s, READ);
        assert_eq!(written.lines().count(), 2, "two rows to compare: {written}");
        // PostgreSQL reads the clock once per statement, so both rows of
        // one INSERT carry the same `now()`. SPG read it per ROW.
        let stamps: Vec<&str> = written
            .lines()
            .map(|l| l.split('|').nth(2).unwrap())
            .collect();
        assert_eq!(
            stamps[0], stamps[1],
            "one statement, one instant: {written}"
        );
        let uuids: Vec<&str> = written
            .lines()
            .map(|l| l.split('|').nth(3).unwrap())
            .collect();
        assert_ne!(uuids[0], uuids[1], "two rows, two uuids: {written}");
    }
    std::thread::sleep(Duration::from_millis(300));

    let (child2, addrs2) = build().spawn();
    let guard = common::ChildGuard(child2);
    let mut s = common::connect_to(&addrs2.native);
    assert_eq!(
        table(&mut s, READ),
        written,
        "recovery runs the statement again; it has to land where it landed"
    );

    // And again: the published build moved the timestamps on EVERY
    // restart, so one restart is not enough to hold it to this.
    drop(guard);
    std::thread::sleep(Duration::from_millis(300));
    let (child3, addrs3) = build().spawn();
    let _guard3 = common::ChildGuard(child3);
    let mut s = common::connect_to(&addrs3.native);
    assert_eq!(table(&mut s, READ), written, "and on the next restart");
}

#[test]
fn two_servers_do_not_hand_out_the_same_random_values() {
    let one = || {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = common::tmp_base().join(format!("spg-volatile-rng-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let (child, addrs) = common::ServerBuilder::new()
            .arg_path(&dir.join("d.spgdb"))
            .spawn();
        let _guard = common::ChildGuard(child);
        let mut s = common::connect_to(&addrs.native);
        table(&mut s, "SELECT gen_random_uuid()::text, random()::text")
    };
    let a = one();
    let b = one();
    assert!(!a.is_empty(), "the probe read nothing");
    assert_ne!(
        a, b,
        "two servers answered the same uuid and the same random number"
    );
}
