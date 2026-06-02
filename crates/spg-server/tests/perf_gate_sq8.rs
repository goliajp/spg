//! v6.0.1 ship-gate perf tests for `VECTOR(N) USING SQ8` columns.
//!
//! Both tests are `#[ignore]` by default — they take minutes and
//! ~200 MiB of memory, so they're for explicit perf verification,
//! not the routine `cargo test` pass. Run with:
//!
//! ```sh
//! cargo test --release -p spg-server --test perf_gate_sq8 -- --ignored --nocapture
//! ```
//!
//! Gate budgets come from `V6_DESIGN.md` L1 goal-numbers table:
//!
//! - **`sq8_knn_1m_dim128_p50_under_50us_server`** — full server
//!   path (pgwire client → engine → HNSW SQ8 + rerank) over a 1M
//!   dim-128 corpus. p50 of 1024 random queries ≤ 50 µs server-
//!   side. (pgvector clocks ~1500 µs for the same shape.)
//!
//! - **`sq8_rss_1m_dim128_under_200mib`** — resident-set size of
//!   the server process after ingest + warmup ≤ 200 MiB. (Pure
//!   f32 storage of the same corpus would land at ~488 MiB just
//!   for the row payload; the 4× SQ8 compression is what makes
//!   this gate possible.)
//!
//! The corpus is built deterministically via splitmix64 so reruns
//! are reproducible. Both tests share `seed_corpus()` so changing
//! the seed re-points both gates simultaneously.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, build_query, encode, parse_command_complete};

mod common;
use common::{ChildGuard, ServerBuilder, connect_to, rss_kib_of};

// 1M-scale workloads — give the server plenty of head room before
// the test harness considers it stuck.
#[allow(clippy::unreadable_literal)]
const READ_TIMEOUT_SECS: u64 = 120;
const DIM: usize = 128;
const CORPUS_SIZE: usize = 1_000_000;
const QUERY_COUNT: usize = 1024;

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn next_unit_f32(state: &mut u64) -> f32 {
    let u = (splitmix64(state) >> 32) as u32;
    #[allow(clippy::cast_precision_loss)]
    let r = (u as f32) / (u32::MAX as f32);
    2.0 * r - 1.0
}

fn seed_corpus(n: usize) -> Vec<Vec<f32>> {
    let mut seed: u64 = 0xCAFE_BABE_DEAD_BEEFu64;
    (0..n)
        .map(|_| (0..DIM).map(|_| next_unit_f32(&mut seed)).collect())
        .collect()
}

fn seed_queries(n: usize) -> Vec<Vec<f32>> {
    let mut seed: u64 = 0x5EED_5EED_5EED_5EEDu64;
    (0..n)
        .map(|_| (0..DIM).map(|_| next_unit_f32(&mut seed)).collect())
        .collect()
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).unwrap();
    s.write_all(&out).unwrap();
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

fn expect_cc(s: &mut TcpStream) {
    let f = read_frame(s);
    if f.op != Op::CommandComplete {
        let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
        panic!("expected CC, got {:?}: {msg}", f.op);
    }
    parse_command_complete(&f).unwrap();
}

fn drain_select(s: &mut TcpStream) {
    let rd = read_frame(s);
    if rd.op != Op::RowDescription {
        let msg = spg_wire::parse_error_response(&rd).unwrap_or("<undecodable>");
        panic!("expected RD, got {:?}: {msg}", rd.op);
    }
    loop {
        let f = read_frame(s);
        match f.op {
            Op::DataRow | Op::DataRowBatch => {} // drained, keep reading
            Op::CommandComplete => return,
            Op::ErrorResponse => {
                let msg = spg_wire::parse_error_response(&f).unwrap();
                panic!("server error mid-row-stream: {msg}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}

fn format_vec(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 12);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&x.to_string());
    }
    s.push(']');
    s
}

fn ingest_corpus(s: &mut TcpStream, corpus: &[Vec<f32>]) {
    send_query(
        s,
        "CREATE TABLE vecs (id INT NOT NULL, v VECTOR(128) USING SQ8 NOT NULL)",
    );
    expect_cc(s);
    // Bulk-INSERT via batched single-row statements. Faster paths
    // (multi-row INSERT, COPY) would land in v6.0.5 sweep work; the
    // gate today only measures kNN p50, not ingest throughput.
    for (i, v) in corpus.iter().enumerate() {
        send_query(
            s,
            &format!("INSERT INTO vecs VALUES ({}, {})", i, format_vec(v)),
        );
        expect_cc(s);
        if i % 50_000 == 0 {
            eprintln!("ingest progress: {i} / {}", corpus.len());
        }
    }
    send_query(s, "CREATE INDEX vecs_idx ON vecs USING hnsw (v)");
    expect_cc(s);
}

#[test]
#[ignore = "1M-scale; run via `cargo test --release -p spg-server --test perf_gate_sq8 -- --ignored`"]
fn sq8_knn_1m_dim128_p50_under_50us_server() {
    let (raw, addrs) = ServerBuilder::new()
        .startup_timeout(Duration::from_secs(30))
        .spawn();
    let pid = raw.id();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)))
        .unwrap();

    eprintln!("seeding {CORPUS_SIZE} dim-{DIM} corpus");
    let corpus = seed_corpus(CORPUS_SIZE);
    let queries = seed_queries(QUERY_COUNT);
    let ingest_start = Instant::now();
    ingest_corpus(&mut s, &corpus);
    eprintln!("ingest: {:.1}s", ingest_start.elapsed().as_secs_f64());

    // Warmup — first few queries pay any caching cost we don't
    // want to mix into the p50 sample.
    for q in queries.iter().take(8) {
        send_query(
            &mut s,
            &format!(
                "SELECT id FROM vecs ORDER BY v <-> {} LIMIT 10",
                format_vec(q)
            ),
        );
        drain_select(&mut s);
    }

    // Measured pass.
    let mut latencies_us: Vec<u128> = Vec::with_capacity(queries.len());
    for q in &queries {
        let t = Instant::now();
        send_query(
            &mut s,
            &format!(
                "SELECT id FROM vecs ORDER BY v <-> {} LIMIT 10",
                format_vec(q)
            ),
        );
        drain_select(&mut s);
        latencies_us.push(t.elapsed().as_micros());
    }
    latencies_us.sort_unstable();
    let p50 = latencies_us[latencies_us.len() / 2];
    let p99 = latencies_us[(latencies_us.len() * 99) / 100];
    eprintln!("sq8 knn 1M dim128 p50 = {p50} µs, p99 = {p99} µs (pid {pid})");
    let budget_us: u128 = 50;
    assert!(
        p50 <= budget_us,
        "sq8_knn_1m_dim128 p50 {p50} µs exceeds budget {budget_us} µs \
         (p99 {p99} µs over {} queries)",
        latencies_us.len()
    );
}

#[test]
#[ignore = "1M-scale; run via `cargo test --release -p spg-server --test perf_gate_sq8 -- --ignored`"]
fn sq8_rss_1m_dim128_under_200mib() {
    let (raw, addrs) = ServerBuilder::new()
        .startup_timeout(Duration::from_secs(30))
        .spawn();
    let pid = raw.id();
    let _child = ChildGuard(raw);
    let mut s = connect_to(&addrs.native);
    s.set_read_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)))
        .unwrap();

    eprintln!("seeding {CORPUS_SIZE} dim-{DIM} corpus");
    let corpus = seed_corpus(CORPUS_SIZE);
    let queries = seed_queries(8);
    ingest_corpus(&mut s, &corpus);

    // Warmup so any lazy allocations land before we sample RSS.
    for q in &queries {
        send_query(
            &mut s,
            &format!(
                "SELECT id FROM vecs ORDER BY v <-> {} LIMIT 10",
                format_vec(q)
            ),
        );
        drain_select(&mut s);
    }

    // Sample RSS 5× over 5 s — peak is what the budget checks.
    let mut peak_kib: u64 = 0;
    for _ in 0..5 {
        let rss = rss_kib_of(pid);
        peak_kib = peak_kib.max(rss);
        thread::sleep(Duration::from_secs(1));
    }
    let ceiling_kib: u64 = 200 * 1024;
    eprintln!("sq8 1M dim128 RSS peak = {peak_kib} KiB (budget {ceiling_kib} KiB)");
    assert!(
        peak_kib > 0,
        "rss_kib_of(pid={pid}) returned 0 — `ps` parse failure"
    );
    assert!(
        peak_kib <= ceiling_kib,
        "sq8_rss_1m_dim128 peak {peak_kib} KiB exceeds 200 MiB ceiling \
         — F32 baseline would be ~488 MiB so the 4× compression is \
         the load-bearing budget here"
    );
}
