#![allow(unused_mut, unused_variables, clippy::uninlined_format_args)]
//! v6.1.5 — publisher-side WAL filtering by publication.
//!
//! The publisher's MAGIC_SUB tail walks each WAL record's owner
//! (the table the SQL touches) and only forwards records that
//! satisfy the requested publication's scope. Filtered-out records
//! get a FRAME_TYPE_SKIP frame so the subscriber's
//! `last_received_pos` still advances byte-for-byte with the
//! master — reconnect from the same offset doesn't re-stream
//! filtered records.
//!
//! Ship-gate (V6_1_DESIGN.md L2 row 5):
//!   - `only_published_tables_replicated`
//!   - publisher filter overhead ≤ 200 ns/record (covered by the
//!     stand-alone unit test in `replication.rs`)

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

mod common;

const READ_TIMEOUT: Duration = Duration::from_secs(3);
const CATCHUP_TIMEOUT: Duration = Duration::from_secs(10);

static TMPDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmpdir(tag: &str) -> PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let serial = TMPDIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("spg-filt-e2e-{tag}-{pid}-{nanos}-{serial}"));
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

fn wait_until_pos(sub_client: &mut TcpStream, sub_name: &str, target_pos: u64, deadline: Instant) {
    // Poll SHOW SUBSCRIPTIONS for last_received_pos ≥ target. Used
    // when we expect the subscriber to advance the offset (via
    // SKIP) without ANY new rows landing in the target table.
    loop {
        send(sub_client, &build_query("SHOW SUBSCRIPTIONS"));
        assert_eq!(read_frame(sub_client).op, Op::RowDescription);
        let mut last_pos: i64 = -1;
        loop {
            let f = read_frame(sub_client);
            match f.op {
                Op::DataRow => {
                    let row = parse_data_row(&f).unwrap();
                    // row[0] = name, row[4] = last_received_pos
                    if let WireValue::Text(n) = &row[0]
                        && n == sub_name
                    {
                        last_pos = wire_to_i64(&row[4]);
                    }
                }
                Op::DataRowBatch => {
                    for row in parse_data_row_batch(&f).unwrap() {
                        if let WireValue::Text(n) = &row[0]
                            && n == sub_name
                        {
                            last_pos = wire_to_i64(&row[4]);
                        }
                    }
                }
                Op::CommandComplete => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        if last_pos >= 0 && last_pos as u64 >= target_pos {
            return;
        }
        if Instant::now() >= deadline {
            panic!("subscription {sub_name:?} last_pos {last_pos} never reached {target_pos}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn for_table_filter_propagates_only_published_tables() {
    let dir_p = unique_tmpdir("pub");
    let dir_s = unique_tmpdir("sub");

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

    // Publisher: two tables, publication for ONLY t1.
    exec_ok(&mut pub_client, "CREATE TABLE t1 (id INT NOT NULL)");
    exec_ok(&mut pub_client, "CREATE TABLE t2 (id INT NOT NULL)");
    exec_ok(&mut pub_client, "CREATE PUBLICATION pub_t1 FOR TABLE t1");

    let (sub_raw, sub_addrs) = spawn_subscriber(&dir_s.join("s.db"), &dir_s.join("s.wal"));
    let mut sub_guard = common::ChildGuard(sub_raw);
    let mut sub_client = common::connect_to(&sub_addrs.native);
    sub_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    // Subscriber-side schema (both tables, since we expect SOME
    // rows of t1 to arrive — but none of t2).
    exec_ok(&mut sub_client, "CREATE TABLE t1 (id INT NOT NULL)");
    exec_ok(&mut sub_client, "CREATE TABLE t2 (id INT NOT NULL)");
    let (host, port) = repl_addr.split_once(':').unwrap();
    exec_ok(
        &mut sub_client,
        &format!(
            "CREATE SUBSCRIPTION sub_a CONNECTION 'host={host} port={port}' PUBLICATION pub_t1"
        ),
    );
    std::thread::sleep(Duration::from_millis(500));

    // Interleave 5 inserts into t1 and 5 into t2.
    for i in 0..5 {
        exec_ok(&mut pub_client, &format!("INSERT INTO t1 VALUES ({})", i));
        exec_ok(
            &mut pub_client,
            &format!("INSERT INTO t2 VALUES ({})", 100 + i),
        );
    }

    // Wait for sub_a to converge on t1 = 5.
    let deadline = Instant::now() + CATCHUP_TIMEOUT;
    loop {
        let n = select_int(&mut sub_client, "SELECT count(*) FROM t1");
        if n == 5 {
            break;
        }
        assert!(Instant::now() < deadline, "t1 never reached 5 (saw {n})");
        std::thread::sleep(Duration::from_millis(100));
    }

    // Crucial assertion: t2 must stay empty on the subscriber.
    let t2_count = select_int(&mut sub_client, "SELECT count(*) FROM t2");
    assert_eq!(t2_count, 0, "subscriber must NOT see t2 inserts");
}

#[test]
fn for_all_tables_except_blocks_only_excepted() {
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

    exec_ok(&mut pub_client, "CREATE TABLE keep_a (id INT NOT NULL)");
    exec_ok(&mut pub_client, "CREATE TABLE drop_me (id INT NOT NULL)");
    exec_ok(&mut pub_client, "CREATE TABLE keep_b (id INT NOT NULL)");
    exec_ok(
        &mut pub_client,
        "CREATE PUBLICATION pub_x FOR ALL TABLES EXCEPT drop_me",
    );

    let (sub_raw, sub_addrs) = spawn_subscriber(&dir_s.join("s.db"), &dir_s.join("s.wal"));
    let mut sub_guard = common::ChildGuard(sub_raw);
    let mut sub_client = common::connect_to(&sub_addrs.native);
    sub_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(&mut sub_client, "CREATE TABLE keep_a (id INT NOT NULL)");
    exec_ok(&mut sub_client, "CREATE TABLE drop_me (id INT NOT NULL)");
    exec_ok(&mut sub_client, "CREATE TABLE keep_b (id INT NOT NULL)");
    let (host, port) = repl_addr.split_once(':').unwrap();
    exec_ok(
        &mut sub_client,
        &format!(
            "CREATE SUBSCRIPTION sub_x CONNECTION 'host={host} port={port}' PUBLICATION pub_x"
        ),
    );
    std::thread::sleep(Duration::from_millis(500));

    for i in 0..3 {
        exec_ok(
            &mut pub_client,
            &format!("INSERT INTO keep_a VALUES ({})", i),
        );
        exec_ok(
            &mut pub_client,
            &format!("INSERT INTO drop_me VALUES ({})", i),
        );
        exec_ok(
            &mut pub_client,
            &format!("INSERT INTO keep_b VALUES ({})", i),
        );
    }

    let deadline = Instant::now() + CATCHUP_TIMEOUT;
    loop {
        let a = select_int(&mut sub_client, "SELECT count(*) FROM keep_a");
        let b = select_int(&mut sub_client, "SELECT count(*) FROM keep_b");
        if a == 3 && b == 3 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "keep_a={a} keep_b={b} (need both = 3)"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let dm = select_int(&mut sub_client, "SELECT count(*) FROM drop_me");
    assert_eq!(dm, 0, "drop_me must NOT propagate under ALL TABLES EXCEPT");
}

#[test]
fn skip_frame_advances_subscriber_offset() {
    // Verify that records the master filters out cause the
    // subscriber's `last_received_pos` to advance by the skipped
    // byte count — proving the SKIP frame is wired end-to-end.
    let dir_p = unique_tmpdir("pub3");
    let dir_s = unique_tmpdir("sub3");

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
    exec_ok(&mut pub_client, "CREATE TABLE t_only (id INT NOT NULL)");
    exec_ok(&mut pub_client, "CREATE TABLE t_filtered (id INT NOT NULL)");
    exec_ok(
        &mut pub_client,
        "CREATE PUBLICATION pub_only FOR TABLE t_only",
    );

    let (sub_raw, sub_addrs) = spawn_subscriber(&dir_s.join("s.db"), &dir_s.join("s.wal"));
    let mut sub_guard = common::ChildGuard(sub_raw);
    let mut sub_client = common::connect_to(&sub_addrs.native);
    sub_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(&mut sub_client, "CREATE TABLE t_only (id INT NOT NULL)");
    exec_ok(&mut sub_client, "CREATE TABLE t_filtered (id INT NOT NULL)");
    let (host, port) = repl_addr.split_once(':').unwrap();
    exec_ok(
        &mut sub_client,
        &format!(
            "CREATE SUBSCRIPTION sub_a CONNECTION 'host={host} port={port}' PUBLICATION pub_only"
        ),
    );
    std::thread::sleep(Duration::from_millis(500));

    // Write only to the filtered-out table. The subscriber must
    // see ZERO row in t_filtered but its last_received_pos must
    // advance (proving SKIP frames flow).
    for i in 0..10 {
        exec_ok(
            &mut pub_client,
            &format!("INSERT INTO t_filtered VALUES ({})", i),
        );
    }
    // Each INSERT produces one auto-commit SQL record. The exact
    // byte count is implementation-dependent; just assert pos > 0.
    let deadline = Instant::now() + CATCHUP_TIMEOUT;
    wait_until_pos(&mut sub_client, "sub_a", 1, deadline);
    let filtered_count = select_int(&mut sub_client, "SELECT count(*) FROM t_filtered");
    assert_eq!(filtered_count, 0, "filtered table must remain empty");
}
