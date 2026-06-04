#![allow(unused_mut, unused_variables, clippy::uninlined_format_args)]
//! v6.2.0 — end-to-end wire-protocol round-trip for ANALYZE +
//! `SELECT * FROM spg_statistic`. Engine-level invariants
//! (histogram bound shape, n_distinct, envelope v5 round-trip)
//! are covered in `spg-engine`'s lib tests; this file exercises
//! the server boundary.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

mod common;

const READ_TIMEOUT: Duration = Duration::from_secs(3);

static TMPDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmpdir() -> PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let serial = TMPDIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("spg-stat-e2e-{pid}-{nanos}-{serial}"));
    fs::create_dir_all(&dir).expect("create tmpdir");
    dir
}

fn local_spawn(db: &std::path::Path) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new().arg_path(db).spawn()
}

fn read_frame(s: &mut TcpStream) -> Frame {
    let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
    s.read_exact(&mut header).unwrap();
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).unwrap();
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        s.read_exact(&mut payload).unwrap();
    }
    Frame { op, payload }
}

fn send(s: &mut TcpStream, f: &Frame) {
    let mut out = Vec::new();
    encode(f, &mut out).unwrap();
    s.write_all(&out).unwrap();
}

fn exec_ok(s: &mut TcpStream, sql: &str) {
    send(s, &build_query(sql));
    loop {
        let f = read_frame(s);
        match f.op {
            Op::CommandComplete => return,
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
                panic!("server rejected {sql:?}: {msg}");
            }
            _ => {}
        }
    }
}

fn exec_err(s: &mut TcpStream, sql: &str) -> String {
    send(s, &build_query(sql));
    let f = read_frame(s);
    assert_eq!(f.op, Op::ErrorResponse, "expected error, got {:?}", f.op);
    spg_wire::parse_error_response(&f)
        .unwrap_or("<undecodable>")
        .to_string()
}

fn select_rows(s: &mut TcpStream, sql: &str) -> Vec<Vec<WireValue>> {
    send(s, &build_query(sql));
    let rd = read_frame(s);
    assert_eq!(rd.op, Op::RowDescription, "got {:?}", rd.op);
    let mut rows = Vec::new();
    loop {
        let f = read_frame(s);
        match f.op {
            Op::DataRow => rows.push(parse_data_row(&f).unwrap()),
            Op::DataRowBatch => rows.extend(parse_data_row_batch(&f).unwrap()),
            Op::CommandComplete => return rows,
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn analyze_populates_spg_statistic_rows() {
    let dir = unique_tmpdir();
    let (raw, addrs) = local_spawn(&dir.join("s.db"));
    let _g = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(
        &mut s,
        "CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL)",
    );
    for i in 0..30 {
        exec_ok(
            &mut s,
            &format!("INSERT INTO users VALUES ({i}, 'name{i}')"),
        );
    }
    exec_ok(&mut s, "ANALYZE users");

    let rows = select_rows(&mut s, "SELECT * FROM spg_statistic");
    assert_eq!(rows.len(), 2, "one row per column of users");
    // Row 0: (users, id, …), Row 1: (users, name, …) — alphabetical
    // by (table, column) so id < name.
    match (&rows[0][0], &rows[0][1]) {
        (WireValue::Text(t), WireValue::Text(c)) => {
            assert_eq!(t, "users");
            assert_eq!(c, "id");
        }
        _ => panic!(),
    }
    // null_frac = 0 for NOT NULL columns.
    if let WireValue::Float(f) = &rows[0][2] {
        assert!(f.abs() < 1e-6);
    } else {
        panic!("expected Float, got {:?}", rows[0][2]);
    }
    // n_distinct = 30 unique ids.
    match &rows[0][3] {
        WireValue::Int(n) => assert_eq!(*n, 30),
        WireValue::BigInt(n) => assert_eq!(*n, 30),
        WireValue::Text(t) => assert_eq!(t.parse::<i64>().unwrap(), 30),
        other => panic!("expected int, got {other:?}"),
    }
    // histogram_bounds — non-empty + sorted numerically.
    if let WireValue::Text(bounds) = &rows[0][4] {
        assert!(bounds.starts_with('['));
        assert!(bounds.contains("0,") || bounds.contains("0]"));
        assert!(bounds.contains("29"));
    } else {
        panic!("expected Text histogram_bounds, got {:?}", rows[0][4]);
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn bare_analyze_covers_all_user_tables() {
    let dir = unique_tmpdir();
    let (raw, addrs) = local_spawn(&dir.join("s.db"));
    let _g = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(&mut s, "CREATE TABLE t1 (id INT NOT NULL)");
    exec_ok(&mut s, "CREATE TABLE t2 (label TEXT NOT NULL)");
    exec_ok(&mut s, "INSERT INTO t1 VALUES (1)");
    exec_ok(&mut s, "INSERT INTO t2 VALUES ('hello')");

    exec_ok(&mut s, "ANALYZE");
    let rows = select_rows(&mut s, "SELECT * FROM spg_statistic");
    let table_names: Vec<&str> = rows
        .iter()
        .filter_map(|r| {
            if let WireValue::Text(t) = &r[0] {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect();
    assert!(table_names.contains(&"t1"));
    assert!(table_names.contains(&"t2"));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn analyze_unknown_table_errors() {
    let dir = unique_tmpdir();
    let (raw, addrs) = local_spawn(&dir.join("s.db"));
    let _g = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    let msg = exec_err(&mut s, "ANALYZE no_such_table");
    assert!(
        msg.contains("table not found") || msg.contains("no_such_table"),
        "got: {msg}"
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn analyze_persists_across_restart_via_envelope_v5() {
    let dir = unique_tmpdir();
    let db = dir.join("s.db");

    {
        let (raw, addrs) = local_spawn(&db);
        let _g = common::ChildGuard(raw);
        let mut s = common::connect_to(&addrs.native);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        exec_ok(&mut s, "CREATE TABLE t (id INT NOT NULL)");
        for i in 0..15 {
            exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i})"));
        }
        exec_ok(&mut s, "ANALYZE");
    }

    let (raw, addrs) = local_spawn(&db);
    let _g = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let rows = select_rows(&mut s, "SELECT * FROM spg_statistic");
    assert_eq!(rows.len(), 1, "stats must survive the restart");
    match &rows[0][1] {
        WireValue::Text(c) => assert_eq!(c, "id"),
        other => panic!("expected Text col name, got {other:?}"),
    }
}

#[test]
fn select_star_from_spg_statistic_on_empty_engine() {
    let dir = unique_tmpdir();
    let (raw, addrs) = local_spawn(&dir.join("s.db"));
    let _g = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let rows = select_rows(&mut s, "SELECT * FROM spg_statistic");
    assert!(rows.is_empty());
}

#[test]
fn reanalyze_after_inserts_updates_n_distinct() {
    let dir = unique_tmpdir();
    let (raw, addrs) = local_spawn(&dir.join("s.db"));
    let _g = common::ChildGuard(raw);
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    exec_ok(&mut s, "CREATE TABLE t (id INT NOT NULL)");
    for i in 0..10 {
        exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i})"));
    }
    exec_ok(&mut s, "ANALYZE t");
    for i in 10..40 {
        exec_ok(&mut s, &format!("INSERT INTO t VALUES ({i})"));
    }
    exec_ok(&mut s, "ANALYZE t");
    let rows = select_rows(&mut s, "SELECT * FROM spg_statistic");
    assert_eq!(rows.len(), 1);
    let n = match &rows[0][3] {
        WireValue::Int(n) => i64::from(*n),
        WireValue::BigInt(n) => *n,
        WireValue::Text(t) => t.parse().unwrap(),
        other => panic!("expected int, got {other:?}"),
    };
    assert_eq!(n, 40);
}
