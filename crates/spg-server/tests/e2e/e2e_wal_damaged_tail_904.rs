//! 9.0.4 — a WAL whose tail cannot be read still starts the database.
//!
//! Measured by `xtests/gates/g5-diskfull.sh` on the published 9.0.3,
//! with the data directory on a 400 MB filesystem that is then filled:
//!
//! ```text
//!   the write with no room      ERROR: durability append failed:
//!                               No space left on device   (as PG does)
//!   still answering             yes
//!   reads                       yes
//!   space freed, writes again   yes
//!   restart                     spg-server: fatal: WAL CRC mismatch at
//!                               offset 879201 — corruption detected,
//!                               refusing to replay
//! ```
//!
//! PostgreSQL 18.6 takes the same filling, the same freeing and the same
//! restart without complaint. SPG never started again.
//!
//! Two things were wrong. `write_all` can return an error after some of
//! its bytes have landed — on ENOSPC it does — and the WAL then held a
//! half-record; the next successful append landed behind it, and reading
//! the half-record's length ran across into it. The fsync path has always
//! rolled its bytes back; the write path now does too.
//!
//! And a record that does not check out ended the boot rather than the
//! WAL. PostgreSQL reads a break in the chain as the end of the usable
//! log: what follows cannot be trusted, and the server comes up on what
//! preceded it. This test is that second half — the one a rollback
//! cannot cover, because a machine can also lose power in the middle of
//! a write.
//!
//! Only when the damage IS the tail. A bit flipped inside one record
//! leaves every record behind it intact, and those are not the
//! operator's to lose silently; that case still refuses to boot, and
//! `e2e_chaos::chaos_wal_bit_flip_caught_by_crc32_refuses_to_replay`
//! has pinned it since v4.37.

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

fn count(stream: &mut std::net::TcpStream, sql: &str) -> String {
    match send(stream, sql).first().and_then(|r| r.first()) {
        Some(WireValue::Int(n)) => n.to_string(),
        Some(WireValue::BigInt(n)) => n.to_string(),
        Some(WireValue::Text(t)) => t.clone(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn a_damaged_tail_ends_recovery_rather_than_the_boot() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = common::tmp_base().join(format!("spg-waltail-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("d.spgdb");
    let wal = dir.join("d.wal");
    let build = || {
        common::ServerBuilder::new()
            .arg_path(&db)
            .arg("-")
            .arg_path(&wal)
    };

    // Killed without a checkpoint: the rows exist only in the WAL.
    let (child, addrs) = build().spawn();
    {
        let _guard = common::ChildGuard(child);
        let mut s = common::connect_to(&addrs.native);
        send(&mut s, "CREATE TABLE t (id int PRIMARY KEY)");
        for i in 1..=5 {
            send(&mut s, &format!("INSERT INTO t VALUES ({i})"));
        }
        assert_eq!(count(&mut s, "SELECT count(*) FROM t"), "5");
    }
    std::thread::sleep(Duration::from_millis(300));

    // Damage the last record, the way a write interrupted by a full disk
    // or a power cut does: the bytes are there and they are wrong.
    let good = std::fs::read(&wal).unwrap();
    assert!(
        good.len() > 40,
        "the WAL holds the statements: {}",
        good.len()
    );
    let mut torn = good.clone();
    let last = torn.len() - 1;
    torn[last] ^= 0xFF;
    std::fs::write(&wal, &torn).unwrap();

    let (child2, addrs2) = build().spawn();
    let guard = common::ChildGuard(child2);
    let mut s = common::connect_to(&addrs2.native);
    // Every statement before the damage is there. The last INSERT is
    // not: what follows a break in the chain is not trusted, which is
    // how PostgreSQL reads one.
    assert_eq!(
        count(&mut s, "SELECT count(*) FROM t"),
        "4",
        "the four statements before the damaged record",
    );
    // And the file was cut there, so the next write does not land behind
    // the damage and vanish on the restart after this one.
    let cut = std::fs::read(&wal).unwrap().len();
    assert!(
        cut < torn.len(),
        "the WAL still ends in bytes recovery cannot read: {cut} of {}",
        torn.len()
    );
    send(&mut s, "INSERT INTO t VALUES (6)");
    assert_eq!(count(&mut s, "SELECT count(*) FROM t"), "5");
    drop(guard);
    std::thread::sleep(Duration::from_millis(300));

    let (child3, addrs3) = build().spawn();
    let _guard3 = common::ChildGuard(child3);
    let mut s = common::connect_to(&addrs3.native);
    assert_eq!(
        count(&mut s, "SELECT count(*) FROM t"),
        "5",
        "the row written after the cut survives the next restart",
    );
}
