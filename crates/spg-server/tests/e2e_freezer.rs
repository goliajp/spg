#![allow(
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args
)]

//! v5.2.2 — background freezer thread end-to-end.
//!
//! The freezer wakes when the catalog-wide hot-tier byte sum exceeds
//! `SPG_HOT_TIER_BYTES`. These tests drive the server with a tiny
//! budget (`HOT_TIER_BYTES=512`) and a fast tick (`TICK_MS=50`) so
//! the freezer fires within a few seconds of startup. Verifies
//! end-to-end:
//!
//! 1. Cold-segment count climbs past zero (`spg_cold_segments_total`).
//! 2. Hot-tier byte counter shrinks after the freeze
//!    (`spg_hot_tier_bytes_used`).
//! 3. The frozen rows still resolve via PK lookup (cold-tier read
//!    path stays consistent with the freezer's atomic swap).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(3);
const FREEZE_DEADLINE: Duration = Duration::from_secs(10);

fn pick_free_addr() -> String {
    let p = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = p.local_addr().unwrap();
    drop(p);
    a.to_string()
}

fn spawn_server_with_tight_budget(addr: &str, http_addr: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_spg-server"))
        .arg(addr)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Budget = 512 B; sweep-style schema rows are ~25-40 B each
        // (id INT + name TEXT), so ~20 rows cross the threshold.
        .env("SPG_HOT_TIER_BYTES", "512")
        // Fast tick so the test doesn't sit waiting a full second.
        .env("SPG_FREEZER_TICK_MS", "50")
        // Single-row batches so the freeze granularity is small;
        // the test inserts in waves and checks the metric climbs.
        .env("SPG_FREEZER_BATCH_ROWS", "8")
        .env("SPG_HTTP_ADDR", http_addr)
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD")
        .env_remove("SPG_FREEZER_DISABLE")
        .spawn()
        .unwrap()
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

fn http_get(addr: &str, path: &str) -> (u16, String) {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut stream = loop {
        if let Ok(s) = TcpStream::connect(addr) {
            break s;
        }
        assert!(
            Instant::now() <= deadline,
            "http listener at {addr} never came up"
        );
        thread::sleep(Duration::from_millis(20));
    };
    stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();
    let response = String::from_utf8_lossy(&buf).to_string();
    let (_status_line, rest) = response.split_once("\r\n").unwrap_or((&response, ""));
    let code: u16 = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = rest.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (code, body.to_string())
}

fn metric_value(body: &str, name: &str) -> Option<u64> {
    body.lines()
        .find(|l| l.starts_with(&format!("{name} ")))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let q = build_query(sql);
    let mut out = Vec::new();
    encode(&q, &mut out).unwrap();
    s.write_all(&out).unwrap();
    drain_response(s);
}

fn drain_response(s: &mut TcpStream) {
    loop {
        let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
        if s.read_exact(&mut header).is_err() {
            return;
        }
        let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let op = Op::from_byte(header[4]).unwrap();
        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            let _ = s.read_exact(&mut payload);
        }
        if matches!(op, Op::CommandComplete | Op::ErrorResponse) {
            return;
        }
    }
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

fn select_one(s: &mut TcpStream, sql: &str) -> WireValue {
    let q = build_query(sql);
    let mut out = Vec::new();
    encode(&q, &mut out).unwrap();
    s.write_all(&out).unwrap();
    let rd = read_frame(s);
    if rd.op == Op::ErrorResponse {
        let msg = spg_wire::parse_error_response(&rd).unwrap_or("<undecodable>");
        panic!("server rejected {sql:?}: {msg}");
    }
    assert_eq!(rd.op, Op::RowDescription);
    let mut got: Option<WireValue> = None;
    loop {
        let f = read_frame(s);
        match f.op {
            Op::DataRow => got = parse_data_row(&f).unwrap().into_iter().next(),
            Op::DataRowBatch => {
                got = parse_data_row_batch(&f)
                    .unwrap()
                    .into_iter()
                    .next()
                    .and_then(|r| r.into_iter().next());
            }
            Op::CommandComplete => return got.expect("no row"),
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn wire_to_i64(v: &WireValue) -> i64 {
    match v {
        WireValue::Int(n) => i64::from(*n),
        WireValue::BigInt(n) => *n,
        WireValue::Text(t) => t.parse().unwrap(),
        WireValue::Float(f) => *f as i64,
        other => panic!("expected integer, got {other:?}"),
    }
}

/// Freezer demotes rows once `hot_tier_bytes_used > budget`. Insert
/// well past the budget and confirm `spg_cold_segments_total` climbs.
#[test]
fn freezer_creates_cold_segment_when_hot_exceeds_budget() {
    let native = pick_free_addr();
    let http = pick_free_addr();
    let mut child = ChildGuard(spawn_server_with_tight_budget(&native, &http));
    let mut s = wait_for_listener(&native, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // Set up a table with an integer PK and a BTree index — the
    // freezer's target-picking requires both.
    send_query(
        &mut s,
        "CREATE TABLE big (id BIGINT NOT NULL, name TEXT NOT NULL)",
    );
    send_query(&mut s, "CREATE INDEX by_id ON big (id)");
    // 50 rows ≫ budget=512 B; each row is ~30 B encoded → ~1500 B.
    for i in 0..50i64 {
        send_query(&mut s, &format!("INSERT INTO big VALUES ({i}, 'name-{i}')"));
    }

    // Wait for the freezer to fire at least once. Tick is 50 ms, so
    // a few hundred ms should be enough; loop until either segment
    // count > 0 or the deadline.
    let deadline = Instant::now() + FREEZE_DEADLINE;
    let mut cold_segs: u64 = 0;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
        let (code, body) = http_get(&http, "/metrics");
        assert_eq!(code, 200);
        if let Some(n) = metric_value(&body, "spg_cold_segments_total")
            && n > 0
        {
            cold_segs = n;
            break;
        }
    }
    assert!(
        cold_segs > 0,
        "spg_cold_segments_total never climbed past 0 within {} s",
        FREEZE_DEADLINE.as_secs()
    );

    // Hot-tier byte sum should be lower than total inserted bytes
    // (we don't know the exact target — could be 0 if everything
    // frozen, could be above budget if freezer hasn't caught up
    // yet). Just verify it's bounded.
    let (_code, body) = http_get(&http, "/metrics");
    let hot_used = metric_value(&body, "spg_hot_tier_bytes_used").expect("hot used reported");
    let budget = metric_value(&body, "spg_hot_tier_bytes_budget").expect("budget reported");
    assert_eq!(budget, 512, "budget honors env override");
    // After freezing, hot should be smaller than the raw insert cost
    // (~1500 B). Loose lower bound — we only need to prove that the
    // freezer made progress, not that it converged to the budget.
    assert!(
        hot_used < 1500,
        "hot bytes should have shrunk after freeze; got {hot_used}"
    );
}

/// After a freeze, the frozen rows still resolve via SQL: PK lookup
/// returns the row body unchanged, regardless of which tier it
/// landed in. Validates that the freezer's atomic swap kept the
/// read path consistent.
#[test]
fn freezer_keeps_frozen_rows_addressable_via_pk_lookup() {
    let native = pick_free_addr();
    let http = pick_free_addr();
    let mut child = ChildGuard(spawn_server_with_tight_budget(&native, &http));
    let mut s = wait_for_listener(&native, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(
        &mut s,
        "CREATE TABLE big (id BIGINT NOT NULL, name TEXT NOT NULL)",
    );
    send_query(&mut s, "CREATE INDEX by_id ON big (id)");
    for i in 0..30i64 {
        send_query(&mut s, &format!("INSERT INTO big VALUES ({i}, 'name-{i}')"));
    }
    // Wait for the freezer to fire.
    let deadline = Instant::now() + FREEZE_DEADLINE;
    loop {
        thread::sleep(Duration::from_millis(100));
        let (_code, body) = http_get(&http, "/metrics");
        if metric_value(&body, "spg_cold_segments_total").unwrap_or(0) > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "freezer did not create a segment within {} s",
            FREEZE_DEADLINE.as_secs()
        );
    }

    // Pick an ID that was inserted early (likely to have been
    // demoted) and confirm it still reads via the index.
    let got = select_one(&mut s, "SELECT count(*) FROM big WHERE id = 3");
    assert_eq!(wire_to_i64(&got), 1, "frozen row still returns from SELECT");
    let got = select_one(&mut s, "SELECT count(*) FROM big WHERE id = 29");
    assert_eq!(
        wire_to_i64(&got),
        1,
        "still-hot row still returns from SELECT"
    );
    // NOTE: unfiltered `SELECT count(*) FROM big` would return only
    // hot-tier rows in v5.2.2 — full-scan paths aren't cold-aware
    // yet (v5.3 manifest + scan-fanout is the next step). The
    // contract this test guards is that the **indexed** read path
    // (PK lookups, the v5.1 → v5.2 gate workload) resolves through
    // both tiers transparently.
}

/// v5.5.3: a vector table that carries BOTH a BTree PK index (the freeze
/// target) AND an NSW index over the vector column is freezable. The frozen
/// vector bytes ride into the cold segment alongside the rest of the row
/// payload (the dense encoder already handles `Value::Vector`), and the row
/// stays addressable via PK lookup. The NSW graph is rebuilt over the rows
/// remaining in the hot tier — kNN search stays on the hot tier (cold vector
/// rows are reachable by PK, not by NSW; that is the v5.5.3 scope).
#[test]
fn freezer_freezes_vector_table_with_nsw_index() {
    let native = pick_free_addr();
    let http = pick_free_addr();
    let mut child = ChildGuard(spawn_server_with_tight_budget(&native, &http));
    let mut s = wait_for_listener(&native, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    send_query(
        &mut s,
        "CREATE TABLE vemb (id BIGINT NOT NULL, v VECTOR(4) NOT NULL)",
    );
    send_query(&mut s, "CREATE INDEX by_id ON vemb (id)");
    send_query(&mut s, "CREATE INDEX vnsw ON vemb USING n (v)");
    for i in 0..50i64 {
        #[allow(clippy::cast_precision_loss)] // 0..50 — exact in f32
        let b = i as f32;
        send_query(
            &mut s,
            &format!(
                "INSERT INTO vemb VALUES ({i}, [{b:.1}, {:.1}, {:.1}, {:.1}])",
                b + 1.0,
                b + 2.0,
                b + 3.0
            ),
        );
    }

    // The freezer must fire — pre-v5.5.3 a table with an NSW index was refused.
    let deadline = Instant::now() + FREEZE_DEADLINE;
    let mut cold_segs: u64 = 0;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
        let (code, body) = http_get(&http, "/metrics");
        assert_eq!(code, 200);
        if let Some(n) = metric_value(&body, "spg_cold_segments_total")
            && n > 0
        {
            cold_segs = n;
            break;
        }
    }
    assert!(
        cold_segs > 0,
        "vector table with an NSW index must be freezable; cold segments stayed 0 within {} s",
        FREEZE_DEADLINE.as_secs()
    );

    // A frozen vector row (early id, likely demoted) still resolves via PK —
    // its vector bytes rode into the cold segment with the rest of the row.
    let got = select_one(&mut s, "SELECT count(*) FROM vemb WHERE id = 3");
    assert_eq!(
        wire_to_i64(&got),
        1,
        "frozen vector row must still resolve via PK lookup"
    );
    // A still-hot row resolves too — and the server survived the freeze of a
    // table whose NSW graph had to be rebuilt over the remaining hot rows.
    let got = select_one(&mut s, "SELECT count(*) FROM vemb WHERE id = 49");
    assert_eq!(wire_to_i64(&got), 1, "still-hot vector row must resolve");
}
