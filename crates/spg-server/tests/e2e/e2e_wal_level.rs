//! v6.1.8 — `effective_wal_level` dynamic switch.
//!
//! Fresh cluster boots in `replica`; MAGIC_SUB connections are
//! rejected. `SET effective_wal_level = 'logical'` flips the
//! gate at runtime; subsequent CREATE SUBSCRIPTION succeeds.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

const READ_TIMEOUT: Duration = Duration::from_secs(3);
const CATCHUP_TIMEOUT: Duration = Duration::from_secs(10);

static TMPDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmpdir(tag: &str) -> PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let serial = TMPDIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir =
        crate::common::tmp_base().join(format!("spg-wal-lvl-e2e-{tag}-{pid}-{nanos}-{serial}"));
    std::fs::create_dir_all(&dir).expect("create tmpdir");
    dir
}

fn spawn_replica_publisher(
    db: &std::path::Path,
    wal: &std::path::Path,
) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal)
        .with_repl()
        .spawn()
}

fn spawn_subscriber(
    db: &std::path::Path,
    wal: &std::path::Path,
) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal)
        .spawn()
}

fn read_frame(s: &mut TcpStream) -> Frame {
    let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
    s.read_exact(&mut header).expect("read header");
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).expect("known op");
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        s.read_exact(&mut payload).expect("read payload");
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
                panic!("server rejected SQL {sql:?}: {msg}");
            }
            _ => {}
        }
    }
}

fn select_text(s: &mut TcpStream, sql: &str) -> String {
    send(s, &build_query(sql));
    let rd = read_frame(s);
    assert_eq!(rd.op, Op::RowDescription);
    let mut last = String::new();
    loop {
        let f = read_frame(s);
        match f.op {
            Op::DataRow => {
                if let WireValue::Text(t) = &parse_data_row(&f).unwrap()[0] {
                    last = t.clone();
                }
            }
            Op::DataRowBatch => {
                for r in parse_data_row_batch(&f).unwrap() {
                    if let WireValue::Text(t) = &r[0] {
                        last = t.clone();
                    }
                }
            }
            Op::CommandComplete => return last,
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn select_int(s: &mut TcpStream, sql: &str) -> i64 {
    send(s, &build_query(sql));
    let rd = read_frame(s);
    assert_eq!(rd.op, Op::RowDescription);
    let mut last: i64 = -1;
    loop {
        let f = read_frame(s);
        match f.op {
            Op::DataRow => {
                last = match &parse_data_row(&f).unwrap()[0] {
                    WireValue::Int(n) => i64::from(*n),
                    WireValue::BigInt(n) => *n,
                    WireValue::Text(t) => t.parse().unwrap(),
                    other => panic!("expected int, got {other:?}"),
                };
            }
            Op::DataRowBatch => {
                let rows = parse_data_row_batch(&f).unwrap();
                last = match &rows[0][0] {
                    WireValue::Int(n) => i64::from(*n),
                    WireValue::BigInt(n) => *n,
                    WireValue::Text(t) => t.parse().unwrap(),
                    other => panic!("expected int, got {other:?}"),
                };
            }
            Op::CommandComplete => return last,
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn fresh_cluster_boots_in_replica_mode() {
    let dir = unique_tmpdir("default");
    let (raw, addrs) = spawn_replica_publisher(&dir.join("s.db"), &dir.join("s.wal"));
    let _guard = common::ChildGuard(raw);
    let mut client = common::connect_to(&addrs.native);
    client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let level = select_text(&mut client, "SHOW effective_wal_level");
    assert_eq!(level, "replica");
}

#[test]
fn set_logical_then_show_returns_logical() {
    let dir = unique_tmpdir("flip");
    let (raw, addrs) = spawn_replica_publisher(&dir.join("s.db"), &dir.join("s.wal"));
    let _guard = common::ChildGuard(raw);
    let mut client = common::connect_to(&addrs.native);
    client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    assert_eq!(
        select_text(&mut client, "SHOW effective_wal_level"),
        "replica"
    );
    exec_ok(&mut client, "SET effective_wal_level = 'logical'");
    assert_eq!(
        select_text(&mut client, "SHOW effective_wal_level"),
        "logical"
    );
    exec_ok(&mut client, "SET effective_wal_level = 'replica'");
    assert_eq!(
        select_text(&mut client, "SHOW effective_wal_level"),
        "replica"
    );
}

#[test]
fn replica_mode_rejects_subscription_traffic() {
    // Publisher boots in replica mode (default). Subscriber's
    // CREATE SUBSCRIPTION lands the catalog row + spawns the
    // worker, but the worker's MAGIC_SUB handshake gets refused
    // by the publisher. last_received_pos stays at 0; no records
    // propagate.
    let dir_p = unique_tmpdir("p_replica");
    let dir_s = unique_tmpdir("s_replica");

    let (p_raw, p_addrs) = spawn_replica_publisher(&dir_p.join("p.db"), &dir_p.join("p.wal"));
    let _p_guard = common::ChildGuard(p_raw);
    let mut p_client = common::connect_to(&p_addrs.native);
    p_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    let deadline = Instant::now() + Duration::from_secs(3);
    while TcpStream::connect(p_addrs.repl.as_ref().unwrap()).is_err() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    }

    exec_ok(&mut p_client, "CREATE TABLE t (id INT NOT NULL)");
    exec_ok(&mut p_client, "CREATE PUBLICATION pub_a FOR ALL TABLES");
    exec_ok(&mut p_client, "INSERT INTO t VALUES (1)");

    // Subscriber.
    let (s_raw, s_addrs) = spawn_subscriber(&dir_s.join("s.db"), &dir_s.join("s.wal"));
    let _s_guard = common::ChildGuard(s_raw);
    let mut s_client = common::connect_to(&s_addrs.native);
    s_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(&mut s_client, "CREATE TABLE t (id INT NOT NULL)");
    let repl = p_addrs.repl.as_ref().unwrap();
    let (h, p) = repl.split_once(':').unwrap();
    exec_ok(
        &mut s_client,
        &format!("CREATE SUBSCRIPTION sub_a CONNECTION 'host={h} port={p}' PUBLICATION pub_a"),
    );
    // Give multiple handshake attempts time to fail.
    std::thread::sleep(Duration::from_millis(1500));

    let got = select_int(&mut s_client, "SELECT count(*) FROM t");
    assert_eq!(
        got, 0,
        "replica-mode publisher must not propagate records to MAGIC_SUB subscribers"
    );
}

#[test]
fn flip_to_logical_unblocks_existing_subscription() {
    // The subscription was created against a publisher in replica
    // mode. After the operator flips to logical, the worker's
    // reconnect loop succeeds and records flow.
    let dir_p = unique_tmpdir("p_flip");
    let dir_s = unique_tmpdir("s_flip");

    let (p_raw, p_addrs) = spawn_replica_publisher(&dir_p.join("p.db"), &dir_p.join("p.wal"));
    let _p_guard = common::ChildGuard(p_raw);
    let mut p_client = common::connect_to(&p_addrs.native);
    p_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while TcpStream::connect(p_addrs.repl.as_ref().unwrap()).is_err() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    }
    exec_ok(&mut p_client, "CREATE TABLE t (id INT NOT NULL)");
    exec_ok(&mut p_client, "CREATE PUBLICATION pub_a FOR ALL TABLES");

    let (s_raw, s_addrs) = spawn_subscriber(&dir_s.join("s.db"), &dir_s.join("s.wal"));
    let _s_guard = common::ChildGuard(s_raw);
    let mut s_client = common::connect_to(&s_addrs.native);
    s_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(&mut s_client, "CREATE TABLE t (id INT NOT NULL)");
    let repl = p_addrs.repl.as_ref().unwrap();
    let (h, port) = repl.split_once(':').unwrap();
    exec_ok(
        &mut s_client,
        &format!("CREATE SUBSCRIPTION sub_a CONNECTION 'host={h} port={port}' PUBLICATION pub_a"),
    );
    std::thread::sleep(Duration::from_millis(500));

    // Publisher writes in replica mode — subscriber stays at 0.
    exec_ok(&mut p_client, "INSERT INTO t VALUES (1)");
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(select_int(&mut s_client, "SELECT count(*) FROM t"), 0);

    // Flip the publisher to logical.
    exec_ok(&mut p_client, "SET effective_wal_level = 'logical'");
    // Give the worker's reconnect loop a few cycles to actually
    // re-handshake successfully. v6.1.4 start_offset=0 semantics
    // means a successful reconnect sets effective_start = current
    // WAL end; INSERTs done BEFORE that handshake are skipped,
    // INSERTs done AFTER propagate. So we sleep long enough for at
    // least one reconnect to land, then write.
    std::thread::sleep(Duration::from_millis(1500));

    // New writes after the flip should propagate.
    for i in 2..6 {
        exec_ok(&mut p_client, &format!("INSERT INTO t VALUES ({i})"));
    }

    // Wait for subscriber catchup. Worker retries reconnect; the
    // pre-flip INSERT was filtered as a record the worker never
    // received (the handshake was refused) — but the publisher's
    // start_offset logic means the subscriber resumes from WAL
    // end at first successful handshake, so it sees only the
    // post-flip writes.
    let deadline = Instant::now() + CATCHUP_TIMEOUT;
    loop {
        let got = select_int(&mut s_client, "SELECT count(*) FROM t");
        if got >= 4 {
            break;
        }
        assert!(Instant::now() < deadline, "subscriber stuck at {got}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn set_invalid_value_errors() {
    let dir = unique_tmpdir("bad");
    let (raw, addrs) = spawn_replica_publisher(&dir.join("s.db"), &dir.join("s.wal"));
    let _guard = common::ChildGuard(raw);
    let mut client = common::connect_to(&addrs.native);
    client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send(
        &mut client,
        &build_query("SET effective_wal_level = 'nope'"),
    );
    let f = read_frame(&mut client);
    assert_eq!(f.op, Op::ErrorResponse);
    let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
    assert!(
        msg.contains("nope") || msg.contains("expected"),
        "got: {msg}"
    );
}

#[test]
fn env_var_logical_at_startup() {
    // SPG_WAL_LEVEL=logical at startup should make SHOW report
    // "logical" without any SET.
    let dir = unique_tmpdir("envlog");
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("s.db"))
        .arg("-")
        .arg_path(&dir.join("s.wal"))
        .with_repl()
        .with_logical_wal()
        .spawn();
    let _guard = common::ChildGuard(raw);
    let mut client = common::connect_to(&addrs.native);
    client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    assert_eq!(
        select_text(&mut client, "SHOW effective_wal_level"),
        "logical"
    );
}
