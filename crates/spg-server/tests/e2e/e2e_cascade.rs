//! v6.1.6 — cascading replication + cluster-id cycle detection.
//!
//! Three-node chain:
//!     A (publisher)                                            ← INSERTs
//!       │  MAGIC_V2 follower stream
//!       ▼
//!     B (follower of A AND publisher)                          ← receives + re-publishes
//!       │  MAGIC_SUB subscriber stream
//!       ▼
//!     C (subscriber of B)                                      ← receives via cascade
//!
//! Plus a direct self-loop case: a server subscribes to its own
//! replication endpoint. The MAGIC_SUB cluster_id reply matches
//! the subscriber's own and the link is aborted before any record
//! flows. The catalog row stays (so SHOW SUBSCRIPTIONS still
//! lists it), but `last_received_pos` never advances.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

const READ_TIMEOUT: Duration = Duration::from_secs(3);
const CATCHUP_TIMEOUT: Duration = Duration::from_secs(15);

static TMPDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmpdir(tag: &str) -> PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let serial = TMPDIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("spg-cascade-e2e-{tag}-{pid}-{nanos}-{serial}"));
    std::fs::create_dir_all(&dir).expect("create tmpdir");
    dir
}

fn spawn_with_repl(
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

fn spawn_plain(
    db: &std::path::Path,
    wal: &std::path::Path,
) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal)
        .spawn()
}

fn spawn_follower_with_repl(
    db: &std::path::Path,
    wal: &std::path::Path,
    follow_of: &str,
) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal)
        .env("SPG_FOLLOW_OF", follow_of)
        .with_repl()
        .with_logical_wal()
        .spawn()
}

fn wait_for_addr(addr: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while TcpStream::connect(addr).is_err() {
        assert!(Instant::now() < deadline, "addr never came up: {addr}");
        std::thread::sleep(Duration::from_millis(50));
    }
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
fn three_node_chain_replays_correctly() {
    // A → B → C cascade. A writes; B (follower of A AND publisher)
    // forwards; C subscribes to B and observes A's writes.
    //
    // DDL doesn't propagate through logical replication (v6.1.x
    // policy), so the operator-side schema setup is explicit on
    // each node. The publication has to be declared on B as well
    // since C is subscribing to B, not A.
    let dir_a = unique_tmpdir("A");
    let dir_b = unique_tmpdir("B");
    let dir_c = unique_tmpdir("C");

    // Spawn A with replication listener.
    let (a_raw, a_addrs) = spawn_with_repl(&dir_a.join("a.db"), &dir_a.join("a.wal"));
    let _a_guard = common::ChildGuard(a_raw);
    let mut a_client = common::connect_to(&a_addrs.native);
    a_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    wait_for_addr(a_addrs.repl.as_ref().unwrap());

    exec_ok(&mut a_client, "CREATE TABLE t (id INT NOT NULL)");
    exec_ok(&mut a_client, "CREATE PUBLICATION pub_t FOR ALL TABLES");

    // Spawn B as A's follower AND a publisher. B will receive A's
    // CREATE TABLE via the v2 follower path (which DOES replicate
    // DDL — it's a byte-stream of A's WAL).
    let (b_raw, b_addrs) = spawn_follower_with_repl(
        &dir_b.join("b.db"),
        &dir_b.join("b.wal"),
        a_addrs.repl.as_ref().unwrap(),
    );
    let _b_guard = common::ChildGuard(b_raw);
    let mut b_client = common::connect_to(&b_addrs.native);
    b_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    wait_for_addr(b_addrs.repl.as_ref().unwrap());

    // Wait for B's follower side to receive A's setup (table
    // creation propagates via the v2 byte-stream follower path).
    let deadline = Instant::now() + CATCHUP_TIMEOUT;
    loop {
        let mut probe = TcpStream::connect(&b_addrs.native).unwrap();
        probe.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        send(&mut probe, &build_query("SHOW TABLES"));
        assert_eq!(read_frame(&mut probe).op, Op::RowDescription);
        let mut saw_t = false;
        loop {
            let f = read_frame(&mut probe);
            match f.op {
                Op::DataRow => {
                    if let WireValue::Text(n) = &parse_data_row(&f).unwrap()[0]
                        && n == "t"
                    {
                        saw_t = true;
                    }
                }
                Op::DataRowBatch => {
                    for row in parse_data_row_batch(&f).unwrap() {
                        if let WireValue::Text(n) = &row[0]
                            && n == "t"
                        {
                            saw_t = true;
                        }
                    }
                }
                Op::CommandComplete => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        if saw_t {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "B never received A's CREATE TABLE"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // On B, declare the publication too (DDL doesn't auto-flow via
    // logical replication; v2 byte-stream did create the table but
    // CREATE PUBLICATION is a separate catalog mutation B needs to
    // pick up via its own DDL).
    //
    // Actually — wait. B IS receiving A's WAL bytes raw (v2
    // protocol), so A's CREATE PUBLICATION command, which goes
    // through A's WAL just like any other auto-commit, lands on
    // B's WAL too. So B should already have pub_t.
    //
    // Test this assumption: query B's SHOW PUBLICATIONS.
    send(&mut b_client, &build_query("SHOW PUBLICATIONS"));
    let rd = read_frame(&mut b_client);
    assert_eq!(rd.op, Op::RowDescription);
    let mut b_pubs: Vec<String> = Vec::new();
    loop {
        let f = read_frame(&mut b_client);
        match f.op {
            Op::DataRow => {
                let row = parse_data_row(&f).unwrap();
                if let WireValue::Text(n) = &row[0] {
                    b_pubs.push(n.clone());
                }
            }
            Op::DataRowBatch => {
                for row in parse_data_row_batch(&f).unwrap() {
                    if let WireValue::Text(n) = &row[0] {
                        b_pubs.push(n.clone());
                    }
                }
            }
            Op::CommandComplete => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(
        b_pubs.iter().any(|n| n == "pub_t"),
        "B should have inherited pub_t via the v2 byte-stream follower path; got {b_pubs:?}"
    );

    // Spawn C as a plain server (no follower, no repl listener).
    // C subscribes to B.
    let (c_raw, c_addrs) = spawn_plain(&dir_c.join("c.db"), &dir_c.join("c.wal"));
    let _c_guard = common::ChildGuard(c_raw);
    let mut c_client = common::connect_to(&c_addrs.native);
    c_client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(&mut c_client, "CREATE TABLE t (id INT NOT NULL)");
    let b_repl = b_addrs.repl.as_ref().unwrap();
    let (b_host, b_port) = b_repl.split_once(':').unwrap();
    exec_ok(
        &mut c_client,
        &format!(
            "CREATE SUBSCRIPTION sub_to_b CONNECTION 'host={b_host} port={b_port}' PUBLICATION pub_t"
        ),
    );
    std::thread::sleep(Duration::from_millis(500));

    // Write on A. Cascade: A's WAL → B's WAL (v2 byte-stream) →
    // B's MAGIC_SUB tail → C applies.
    for i in 0..5 {
        exec_ok(&mut a_client, &format!("INSERT INTO t VALUES ({i})"));
    }

    // C must converge.
    let got = wait_for_count(
        &c_addrs.native,
        "SELECT count(*) FROM t",
        5,
        Instant::now() + CATCHUP_TIMEOUT,
    );
    assert_eq!(got, 5, "cascade C must see all 5 rows");
}

#[test]
fn cycle_detection_aborts_loop() {
    // A subscribes to its own MAGIC_SUB endpoint. The cluster_id
    // sidecar makes A's `state.cluster_id` stable; the subscription
    // worker handshakes, receives the master's cluster_id back, sees
    // it matches its own, and aborts the link with REPLICATION_LOOP.
    //
    // We can't directly observe the abort signal from the client
    // (it's an internal worker), but the side-effects are visible:
    //   - last_received_pos stays at 0 forever (worker never
    //     applied a record)
    //   - INSERTs land in the table once (publisher path) — never
    //     twice (the would-be cascade is broken at handshake)
    let dir = unique_tmpdir("loop");
    let (raw, addrs) = spawn_with_repl(&dir.join("s.db"), &dir.join("s.wal"));
    let _guard = common::ChildGuard(raw);
    let mut client = common::connect_to(&addrs.native);
    client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    wait_for_addr(addrs.repl.as_ref().unwrap());

    exec_ok(&mut client, "CREATE TABLE t (id INT NOT NULL)");
    exec_ok(&mut client, "CREATE PUBLICATION pub_t FOR ALL TABLES");

    // Subscribe to OURSELF.
    let repl = addrs.repl.as_ref().unwrap();
    let (host, port) = repl.split_once(':').unwrap();
    exec_ok(
        &mut client,
        &format!(
            "CREATE SUBSCRIPTION sub_self CONNECTION 'host={host} port={port}' PUBLICATION pub_t"
        ),
    );
    // Give the worker time to attempt + fail multiple handshakes.
    //
    // v7.37 (round 827) — deliberately a fixed sleep, not a poll. This
    // waits for something NOT to happen (a buggy cascade re-applying
    // rows), and there is no event to poll for when the correct
    // behaviour is silence. Too short a window can only weaken the
    // test toward a false pass; it cannot produce a spurious failure,
    // because the rows asserted on were CC'd synchronously above.
    std::thread::sleep(Duration::from_millis(1500));

    // Insert 3 rows. Publisher-path INSERT lands them in our table.
    // The buggy cascade would re-apply them, giving 6, 9, etc.
    for i in 0..3 {
        exec_ok(&mut client, &format!("INSERT INTO t VALUES ({i})"));
    }
    // Same deliberate negative window as above.
    std::thread::sleep(Duration::from_millis(1500));

    // Row count must be exactly 3.
    let got = select_int(&mut client, "SELECT count(*) FROM t");
    assert_eq!(
        got, 3,
        "cycle detection must prevent the subscription's would-be re-apply"
    );

    // SHOW SUBSCRIPTIONS should report last_received_pos = 0
    // (worker never got past the handshake).
    send(&mut client, &build_query("SHOW SUBSCRIPTIONS"));
    assert_eq!(read_frame(&mut client).op, Op::RowDescription);
    let mut last_pos: i64 = -1;
    loop {
        let f = read_frame(&mut client);
        match f.op {
            Op::DataRow => {
                let row = parse_data_row(&f).unwrap();
                if let WireValue::Text(n) = &row[0]
                    && n == "sub_self"
                {
                    last_pos = wire_to_i64(&row[4]);
                }
            }
            Op::DataRowBatch => {
                for row in parse_data_row_batch(&f).unwrap() {
                    if let WireValue::Text(n) = &row[0]
                        && n == "sub_self"
                    {
                        last_pos = wire_to_i64(&row[4]);
                    }
                }
            }
            Op::CommandComplete => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(last_pos, 0, "self-subscription must never advance past 0");
}

#[test]
fn cluster_id_persists_across_restart() {
    // Sanity: the cluster_id sidecar must survive a restart so that
    // cycle detection holds after the bounce. We can't read the
    // cluster_id directly through SQL (no SHOW for it), so we
    // verify indirectly: a self-subscription created BEFORE the
    // restart must STILL be rejected AFTER the restart (proving
    // cluster_id didn't change).
    let dir = unique_tmpdir("persist");

    {
        let (raw, addrs) = spawn_with_repl(&dir.join("s.db"), &dir.join("s.wal"));
        let _guard = common::ChildGuard(raw);
        let mut client = common::connect_to(&addrs.native);
        client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        wait_for_addr(addrs.repl.as_ref().unwrap());
        exec_ok(&mut client, "CREATE TABLE t (id INT NOT NULL)");
        exec_ok(&mut client, "CREATE PUBLICATION pub_t FOR ALL TABLES");
        let repl = addrs.repl.as_ref().unwrap();
        let (h, p) = repl.split_once(':').unwrap();
        exec_ok(
            &mut client,
            &format!(
                "CREATE SUBSCRIPTION sub_self CONNECTION 'host={h} port={p}' PUBLICATION pub_t"
            ),
        );
    }

    // The sidecar file is at <wal_path>.cluster_id — must exist.
    // v7.37 (round 827) — poll for it instead of sleeping a fixed
    // 500ms proxy for "the first boot has written it by now".
    let sidecar = dir.join("s.wal.cluster_id");
    common::wait_until(Duration::from_secs(5), || sidecar.exists());
    assert!(
        sidecar.exists(),
        "cluster_id sidecar missing after first boot"
    );
    let sidecar_bytes = std::fs::read(&sidecar).unwrap();
    assert_eq!(sidecar_bytes.len(), 8, "cluster_id sidecar must be 8 bytes");

    // Restart and verify the sidecar bytes are unchanged (proving
    // it was loaded rather than regenerated).
    let (raw, addrs) = spawn_with_repl(&dir.join("s.db"), &dir.join("s.wal"));
    let _guard = common::ChildGuard(raw);
    let mut client = common::connect_to(&addrs.native);
    client.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    wait_for_addr(addrs.repl.as_ref().unwrap());
    let sidecar_bytes_after = std::fs::read(&sidecar).unwrap();
    assert_eq!(
        sidecar_bytes, sidecar_bytes_after,
        "cluster_id must be stable across restart"
    );

    // And inserts on the restarted server still don't double-apply
    // through the self-loop subscription (proving cycle detection
    // survived the bounce).
    for i in 0..3 {
        exec_ok(&mut client, &format!("INSERT INTO t VALUES ({i})"));
    }
    // Same deliberate negative window as above.
    std::thread::sleep(Duration::from_millis(1500));
    let got = select_int(&mut client, "SELECT count(*) FROM t");
    assert_eq!(got, 3, "post-restart self-loop must still be detected");
}
