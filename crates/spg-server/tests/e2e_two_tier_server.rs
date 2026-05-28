//! v5.1: end-to-end validation that spg-server's lazy cold-tier
//! preload hook actually wires a hand-baked segment into the
//! catalog and routes PK SELECTs through it. Sanity-tests the
//! same `SPG_PRELOAD_COLD_SEGMENT` env path the sweep validation
//! uses — just at small scale, in CI.
//!
//! The test bakes a 256-row segment in-process, drops it on
//! disk, starts a `spg-server` child with the env var pointing
//! at it, drives the same CREATE TABLE / CREATE INDEX dance the
//! sweep uses, and asserts that PK lookups on keys that exist
//! *only* in the cold segment come back with the right row.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use spg_storage::{
    Row, SEGMENT_PAGE_BYTES, TableSchema, Value, encode_row_body_dense, encode_segment,
};
use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_command_complete, parse_data_row};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

static TMPDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmpdir() -> PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let serial = TMPDIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("spg-coldtier-e2e-{pid}-{nanos}-{serial}"));
    fs::create_dir_all(&dir).expect("create tmpdir");
    dir
}

fn pick_free_addr() -> String {
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = probe.local_addr().unwrap();
    drop(probe);
    a.to_string()
}

fn spawn_server_with_preload(addr: &str, preload_spec: &str) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
    cmd.arg(addr);
    cmd.env("SPG_PRELOAD_COLD_SEGMENT", preload_spec);
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn spg-server")
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_listener(addr: &str, child: &mut Child) -> TcpStream {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match TcpStream::connect(addr) {
            Ok(s) => return s,
            Err(e) => {
                if let Ok(Some(status)) = child.try_wait() {
                    panic!("server exited early: {status:?} ({e})");
                }
                assert!(Instant::now() < deadline, "server never came up: {e}");
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn send_query(stream: &mut TcpStream, sql: &str) {
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).unwrap();
    stream.write_all(&out).unwrap();
}

fn read_frame(stream: &mut TcpStream) -> Frame {
    let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
    stream.read_exact(&mut header).expect("read header");
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).expect("known op");
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream.read_exact(&mut payload).expect("read payload");
    }
    Frame { op, payload }
}

fn expect_cc(stream: &mut TcpStream) {
    let f = read_frame(stream);
    if f.op != Op::CommandComplete {
        let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
        panic!("expected CC, got {:?}: {msg}", f.op);
    }
    parse_command_complete(&f).unwrap();
}

fn run_select(stream: &mut TcpStream, sql: &str) -> Vec<Vec<WireValue>> {
    send_query(stream, sql);
    assert_eq!(read_frame(stream).op, Op::RowDescription);
    let mut rows = Vec::new();
    loop {
        let f = read_frame(stream);
        match f.op {
            Op::DataRow => rows.push(parse_data_row(&f).unwrap()),
            Op::CommandComplete => return rows,
            other => panic!("unexpected: {other:?}"),
        }
    }
}

/// Build a small cold-tier segment matching the `users` schema
/// the test creates server-side. Each row is dense-encoded so
/// the server's `Catalog::resolve_cold_locator` decode path
/// round-trips to the right `Row`.
fn bake_users_segment(rows: &[(i64, &str)]) -> Vec<u8> {
    use spg_storage::{ColumnSchema, DataType};
    let schema = TableSchema::new(
        "users",
        vec![
            ColumnSchema::new("id", DataType::BigInt, false),
            ColumnSchema::new("name", DataType::Text, false),
        ],
    );
    let seg_rows: Vec<(u64, Vec<u8>)> = rows
        .iter()
        .map(|(id, name)| {
            let row = Row::new(vec![Value::BigInt(*id), Value::Text((*name).into())]);
            (*id as u64, encode_row_body_dense(&row, &schema))
        })
        .collect();
    let (bytes, _) =
        encode_segment(seg_rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).expect("encode segment");
    bytes
}

#[test]
fn preload_loads_cold_segment_on_first_query_after_index_creation() {
    let tmpdir = unique_tmpdir();
    let seg_path = tmpdir.join("users.spg");

    // Cold rows whose PKs are deliberately above what we INSERT
    // into the hot tier — so any hit on `id >= 100` must come
    // from the cold segment.
    let cold: Vec<(i64, &str)> = (100..356).map(|i| (i, "cold-row")).collect();
    fs::write(&seg_path, bake_users_segment(&cold)).expect("write segment");

    let addr = pick_free_addr();
    let spec = format!("users:by_id:{}", seg_path.display());
    let mut child = ChildGuard(spawn_server_with_preload(&addr, &spec));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // CREATE TABLE + INDEX. The preload spec sits idle until both
    // exist; the very next Op::Query after CREATE INDEX triggers
    // `try_lazy_preload_cold`.
    send_query(
        &mut s,
        "CREATE TABLE users (id BIGINT NOT NULL, name TEXT NOT NULL)",
    );
    expect_cc(&mut s);
    send_query(&mut s, "CREATE INDEX by_id ON users (id)");
    expect_cc(&mut s);

    // INSERT one hot row so the table has a non-empty hot tier;
    // the cold rows are the bulk.
    send_query(&mut s, "INSERT INTO users VALUES (1, 'hot-alice')");
    expect_cc(&mut s);

    // First SELECT triggers the preload (the INSERT above also
    // does, but a SELECT immediately afterwards verifies the
    // already-loaded short-circuit). Hot tier hit first:
    let rows = run_select(&mut s, "SELECT name FROM users WHERE id = 1");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], WireValue::Text("hot-alice".into()));

    // Cold tier hits — every key 100..356 must come back as
    // "cold-row".
    for id in [100i64, 150, 200, 300, 355] {
        let rows = run_select(&mut s, &format!("SELECT name FROM users WHERE id = {id}"));
        assert_eq!(rows.len(), 1, "id={id} should hit cold tier");
        assert_eq!(
            rows[0][0],
            WireValue::Text("cold-row".into()),
            "id={id} cold-tier row mismatch"
        );
    }

    // Out-of-segment-range key must miss.
    let rows = run_select(&mut s, "SELECT name FROM users WHERE id = 999");
    assert!(rows.is_empty(), "id=999 must miss both tiers");
    // Inside [min_pk, max_pk] but never inserted at all into the
    // cold segment (we used contiguous 100..356, so gap key 50 is
    // outside; any key in that range that we *did* skip would
    // miss). Test the just-below-min key.
    let rows = run_select(&mut s, "SELECT name FROM users WHERE id = 50");
    assert!(rows.is_empty(), "id=50 must miss (between hot id=1 and cold range)");

    drop(s);
    drop(child);
    let _ = fs::remove_dir_all(&tmpdir);
}
