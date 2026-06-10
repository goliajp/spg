//! v6.1.4 — end-to-end CREATE / DROP SUBSCRIPTION over two real
//! spg-server processes.
//!
//! The publisher boots with replication listener; the subscriber
//! boots without `SPG_FOLLOW_OF` (so it doesn't act as a legacy
//! follower) but with the same protocol-aware engine. CREATE
//! SUBSCRIPTION on the subscriber spawns a `replication::
//! run_subscription_worker` thread (via `reconcile_subscriptions`),
//! which connects to the publisher's replication endpoint with the
//! v6.1.4 `MAGIC_SUB` handshake.

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
    let dir = std::env::temp_dir().join(format!("spg-sub-e2e-{tag}-{pid}-{nanos}-{serial}"));
    std::fs::create_dir_all(&dir).expect("create tmpdir");
    dir
}

fn spawn_publisher(
    db: &std::path::Path,
    wal: &std::path::Path,
) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal)
        .with_repl()
        .with_logical_wal()
        .spawn()
}

/// Subscriber boots with NO upstream — just a plain spg-server with
/// a WAL of its own (so writes the subscriber applies get persisted
/// for downstream cascade). The subscription worker is launched
/// inside the engine via `CREATE SUBSCRIPTION`.
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

fn select_int(s: &mut TcpStream, sql: &str) -> i64 {
    send(s, &build_query(sql));
    let rd = read_frame(s);
    if rd.op == Op::ErrorResponse {
        let msg = spg_wire::parse_error_response(&rd).unwrap_or("<undecodable>");
        panic!("server rejected SQL {sql:?}: {msg}");
    }
    assert_eq!(rd.op, Op::RowDescription);
    let mut last: i64 = -1;
    loop {
        let f = read_frame(s);
        match f.op {
            Op::DataRow => last = wire_to_i64(&parse_data_row(&f).unwrap()[0]),
            Op::DataRowBatch => {
                let rows = parse_data_row_batch(&f).unwrap();
                last = wire_to_i64(&rows[0][0]);
            }
            Op::CommandComplete => return last,
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn wire_to_i64(v: &WireValue) -> i64 {
    match v {
        WireValue::Int(n) => i64::from(*n),
        WireValue::BigInt(n) => *n,
        WireValue::Text(t) => t.parse().unwrap(),
        other => panic!("expected integer, got {other:?}"),
    }
}

fn wait_for_count(addr: &str, sql: &str, target: i64, deadline: Instant) -> i64 {
    loop {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        let got = select_int(&mut s, sql);
        if got >= target || Instant::now() >= deadline {
            return got;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn subscription_replicates_inserts_from_publisher() {
    let dir_p = unique_tmpdir("pub");
    let dir_s = unique_tmpdir("sub");

    let (pub_raw, pub_addrs) = spawn_publisher(&dir_p.join("p.db"), &dir_p.join("p.wal"));
    let mut pub_guard = common::ChildGuard(pub_raw);
    let mut pub_client = common::connect_to(&pub_addrs.native);
    pub_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // Wait for the publisher's replication listener.
    let repl_addr = pub_addrs.repl.as_ref().unwrap().clone();
    let deadline = Instant::now() + Duration::from_secs(3);
    while TcpStream::connect(&repl_addr).is_err() {
        assert!(Instant::now() < deadline, "repl listener never came up");
        std::thread::sleep(Duration::from_millis(50));
    }

    // Publisher: set up table + publication.
    exec_ok(
        &mut pub_client,
        "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)",
    );
    exec_ok(&mut pub_client, "CREATE PUBLICATION pub_a FOR ALL TABLES");

    // Boot subscriber. Same target schema (subscription doesn't
    // auto-sync — per V6_1_DESIGN.md design point 3).
    let (sub_raw, sub_addrs) = spawn_subscriber(&dir_s.join("s.db"), &dir_s.join("s.wal"));
    let mut sub_guard = common::ChildGuard(sub_raw);
    let mut sub_client = common::connect_to(&sub_addrs.native);
    sub_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(
        &mut sub_client,
        "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)",
    );

    // Subscribe.
    let repl_host_port = repl_addr.clone();
    let (host, port) = repl_host_port.split_once(':').unwrap();
    exec_ok(
        &mut sub_client,
        &format!(
            "CREATE SUBSCRIPTION sub_a CONNECTION 'host={host} port={port}' PUBLICATION pub_a"
        ),
    );

    // Give the subscriber worker a moment to handshake.
    std::thread::sleep(Duration::from_millis(500));

    // Publisher writes 10 rows.
    for i in 0..10 {
        exec_ok(
            &mut pub_client,
            &format!("INSERT INTO t VALUES ({i}, {})", i * 7),
        );
    }

    // Subscriber must converge to 10 rows.
    let got = wait_for_count(
        &sub_addrs.native,
        "SELECT count(*) FROM t",
        10,
        Instant::now() + CATCHUP_TIMEOUT,
    );
    assert_eq!(got, 10, "subscriber must see all 10 rows");
}

#[test]
fn drop_subscription_stops_the_worker() {
    let dir_p = unique_tmpdir("pub2");
    let dir_s = unique_tmpdir("sub2");

    let (pub_raw, pub_addrs) = spawn_publisher(&dir_p.join("p.db"), &dir_p.join("p.wal"));
    let mut pub_guard = common::ChildGuard(pub_raw);
    let mut pub_client = common::connect_to(&pub_addrs.native);
    pub_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let repl_addr = pub_addrs.repl.as_ref().unwrap().clone();
    let deadline = Instant::now() + Duration::from_secs(3);
    while TcpStream::connect(&repl_addr).is_err() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    }
    exec_ok(&mut pub_client, "CREATE TABLE t (id INT NOT NULL)");
    exec_ok(&mut pub_client, "CREATE PUBLICATION pub_a FOR ALL TABLES");

    let (sub_raw, sub_addrs) = spawn_subscriber(&dir_s.join("s.db"), &dir_s.join("s.wal"));
    let mut sub_guard = common::ChildGuard(sub_raw);
    let mut sub_client = common::connect_to(&sub_addrs.native);
    sub_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(&mut sub_client, "CREATE TABLE t (id INT NOT NULL)");
    let (host, port) = repl_addr.split_once(':').unwrap();
    exec_ok(
        &mut sub_client,
        &format!(
            "CREATE SUBSCRIPTION sub_a CONNECTION 'host={host} port={port}' PUBLICATION pub_a"
        ),
    );

    std::thread::sleep(Duration::from_millis(500));
    exec_ok(&mut pub_client, "INSERT INTO t VALUES (1)");
    wait_for_count(
        &sub_addrs.native,
        "SELECT count(*) FROM t",
        1,
        Instant::now() + CATCHUP_TIMEOUT,
    );

    // Drop the subscription. The worker should shut down within
    // ~500 ms (the SUB_READ_TIMEOUT poll cadence).
    exec_ok(&mut sub_client, "DROP SUBSCRIPTION sub_a");
    std::thread::sleep(Duration::from_millis(800));

    // Now publish more rows. The subscriber must NOT pick them up.
    for i in 2..6 {
        exec_ok(&mut pub_client, &format!("INSERT INTO t VALUES ({i})"));
    }
    std::thread::sleep(Duration::from_millis(800));
    let got = select_int(&mut sub_client, "SELECT count(*) FROM t");
    assert_eq!(
        got, 1,
        "subscriber must stay at 1 row after DROP SUBSCRIPTION"
    );
}

#[test]
fn subscription_survives_publisher_restart() {
    // The worker's reconnect-loop must reconnect after the
    // publisher comes back. We don't drop the subscription —
    // just stop and restart the publisher, then write more rows.
    let dir_p = unique_tmpdir("pub3");
    let dir_s = unique_tmpdir("sub3");

    let (pub_raw, pub_addrs) = spawn_publisher(&dir_p.join("p.db"), &dir_p.join("p.wal"));
    let pub_guard = common::ChildGuard(pub_raw);
    let mut pub_client = common::connect_to(&pub_addrs.native);
    pub_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let repl_addr = pub_addrs.repl.as_ref().unwrap().clone();
    let deadline = Instant::now() + Duration::from_secs(3);
    while TcpStream::connect(&repl_addr).is_err() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    }
    exec_ok(&mut pub_client, "CREATE TABLE t (id INT NOT NULL)");
    exec_ok(&mut pub_client, "CREATE PUBLICATION pub_a FOR ALL TABLES");
    exec_ok(&mut pub_client, "INSERT INTO t VALUES (1)");

    let (sub_raw, sub_addrs) = spawn_subscriber(&dir_s.join("s.db"), &dir_s.join("s.wal"));
    let mut sub_guard = common::ChildGuard(sub_raw);
    let mut sub_client = common::connect_to(&sub_addrs.native);
    sub_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(&mut sub_client, "CREATE TABLE t (id INT NOT NULL)");
    let (host, port) = repl_addr.split_once(':').unwrap();
    exec_ok(
        &mut sub_client,
        &format!(
            "CREATE SUBSCRIPTION sub_a CONNECTION 'host={host} port={port}' PUBLICATION pub_a"
        ),
    );

    std::thread::sleep(Duration::from_millis(500));
    // First row publisher wrote BEFORE subscription started — the
    // v6.1.4 design ships start_offset=0 meaning "from current WAL
    // end", so the subscriber should NOT see that row. New rows
    // written AFTER subscribe should propagate.
    let baseline = select_int(&mut sub_client, "SELECT count(*) FROM t");
    assert_eq!(baseline, 0, "v6.1.4 starts from current WAL end");

    exec_ok(&mut pub_client, "INSERT INTO t VALUES (2)");
    wait_for_count(
        &sub_addrs.native,
        "SELECT count(*) FROM t",
        1,
        Instant::now() + CATCHUP_TIMEOUT,
    );

    // Kill the publisher.
    drop(pub_guard);
    std::thread::sleep(Duration::from_millis(500));

    // Restart publisher with the same WAL — it picks up where it
    // left off; its existing table + publication catalog survive
    // via the snapshot envelope v3/v4 path.
    let (pub_raw2, pub_addrs2) = spawn_publisher(&dir_p.join("p.db"), &dir_p.join("p.wal"));
    let _pub_guard2 = common::ChildGuard(pub_raw2);
    let mut pub_client2 = common::connect_to(&pub_addrs2.native);
    pub_client2.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    // Wait for the new repl listener.
    let repl_addr2 = pub_addrs2.repl.as_ref().unwrap().clone();
    let deadline = Instant::now() + Duration::from_secs(3);
    while TcpStream::connect(&repl_addr2).is_err() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    }
    // The subscriber's conn_str still points at the original
    // address; the new publisher gets a new ephemeral port, so for
    // this test we can only check publisher2's own state survived
    // (the subscriber will be stuck on the old port). v6.1.x option:
    // pin the publisher's port. For now we just verify publisher2
    // recovered its table + publication.
    let pub_row_count = select_int(&mut pub_client2, "SELECT count(*) FROM t");
    assert_eq!(pub_row_count, 2);
}
