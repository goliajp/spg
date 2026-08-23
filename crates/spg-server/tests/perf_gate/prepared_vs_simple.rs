//! v6.1.1 — micro-bench comparing PG-wire Simple Query (Q) vs
//! Extended Query (Parse / Bind / Execute) for a point-SELECT.
//!
//! Ignored by default — invoke with
//!   cargo test --release -p spg-server --test perf_gate \
//!     -- --ignored --nocapture
//!
//! The Extended Query path caches the parsed AST under the
//! prepared-statement name and reuses it per Bind/Execute. Simple
//! Query re-tokenises + re-parses the full SQL text every round
//! trip. This bench measures the realized parse-skip win on a
//! representative SELECT.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::common;

fn local_spawn(db: &std::path::Path) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .with_pgwire()
        .spawn()
}

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_tmpdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir()
        .join("spg-tests")
        .join(format!("spg-perf-prepvssimple-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

struct PgMessage {
    ty: u8,
    #[allow(dead_code)]
    body: Vec<u8>,
}

fn read_message(s: &mut TcpStream) -> PgMessage {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let body_len = len.saturating_sub(4);
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
        s.read_exact(&mut body).expect("body");
    }
    PgMessage { ty, body }
}

fn send_startup(s: &mut TcpStream, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196608u32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn read_until_ready(s: &mut TcpStream) -> Vec<PgMessage> {
    let mut out = Vec::new();
    loop {
        let m = read_message(s);
        let z = m.ty == b'Z';
        out.push(m);
        if z {
            return out;
        }
    }
}

fn send_msg(s: &mut TcpStream, ty: u8, body: &[u8]) {
    let total = (body.len() + 4) as u32;
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(ty);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(body);
    s.write_all(&out).unwrap();
}

fn send_parse(s: &mut TcpStream, name: &str, sql: &str) {
    let mut body = Vec::with_capacity(name.len() + sql.len() + 8);
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    body.extend_from_slice(&0u16.to_be_bytes());
    send_msg(s, b'P', &body);
}

fn send_sync(s: &mut TcpStream) {
    send_msg(s, b'S', &[]);
}

/// Pack Bind + Execute + Sync into a single `write_all` — the
/// pipelined pattern every real PG driver (JDBC, asyncpg, ...)
/// uses to avoid 3× the syscalls per round trip.
fn send_bind_execute_sync(s: &mut TcpStream, stmt: &str, params: &[&str]) {
    let mut out = Vec::with_capacity(64);

    // Bind
    let mut bind_body = Vec::new();
    bind_body.push(0); // empty portal name
    bind_body.extend_from_slice(stmt.as_bytes());
    bind_body.push(0);
    bind_body.extend_from_slice(&0u16.to_be_bytes());
    bind_body.extend_from_slice(&(params.len() as u16).to_be_bytes());
    for p in params {
        bind_body.extend_from_slice(&(p.len() as i32).to_be_bytes());
        bind_body.extend_from_slice(p.as_bytes());
    }
    bind_body.extend_from_slice(&0u16.to_be_bytes());
    out.push(b'B');
    out.extend_from_slice(&((bind_body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&bind_body);

    // Execute
    let mut exec_body = Vec::new();
    exec_body.push(0); // empty portal name
    exec_body.extend_from_slice(&0i32.to_be_bytes());
    out.push(b'E');
    out.extend_from_slice(&((exec_body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&exec_body);

    // Sync (no body)
    out.push(b'S');
    out.extend_from_slice(&4u32.to_be_bytes());

    s.write_all(&out).unwrap();
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "anyone");
    let _ = read_until_ready(&mut s);
    s
}

fn pct(samples: &mut [u128], p: f64) -> u128 {
    samples.sort_unstable();
    let idx = ((samples.len() as f64) * p) as usize;
    samples[idx.min(samples.len() - 1)]
}

/// Render `[v0,v1,…,v_{dim-1}]` as a SPG `VECTOR` literal.
fn vec_literal(rng_seed: u64, dim: usize) -> String {
    let mut out = String::with_capacity(dim * 12);
    out.push('[');
    let mut x = rng_seed;
    for i in 0..dim {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let f = (x >> 40) as f32 / (1u32 << 24) as f32;
        if i > 0 {
            out.push(',');
        }
        // 4 decimals — same shape as a typical embedding row
        out.push_str(&format!("{:.4}", f));
    }
    out.push(']');
    out
}

#[test]
#[ignore]
fn perf_prepared_vs_simple_select_p50() {
    let _lock = crate::perf_lock();
    const N: usize = 5_000;
    const WARMUP: usize = 200;

    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    // Seed: a few hundred rows, predicate matches one.
    send_query(&mut s, "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)");
    let _ = read_until_ready(&mut s);
    for i in 0..256 {
        send_query(&mut s, &format!("INSERT INTO t VALUES ({i}, {})", i * 7));
        let _ = read_until_ready(&mut s);
    }

    // ── Simple Query timings ────────────────────────────────────
    let mut simple = Vec::with_capacity(N);
    for i in 0..(N + WARMUP) {
        let key = (i % 256) as i64;
        let sql = format!("SELECT v FROM t WHERE id = {key}");
        let t0 = Instant::now();
        send_query(&mut s, &sql);
        let _ = read_until_ready(&mut s);
        let dt = t0.elapsed().as_nanos();
        if i >= WARMUP {
            simple.push(dt);
        }
    }

    // ── Extended Query timings (single Parse, many Bind/Execute) ─
    send_parse(&mut s, "ps_v", "SELECT v FROM t WHERE id = $1");
    send_sync(&mut s);
    let _ = read_until_ready(&mut s);

    let mut prep = Vec::with_capacity(N);
    for i in 0..(N + WARMUP) {
        let key = (i % 256) as i64;
        let key_s = key.to_string();
        let t0 = Instant::now();
        send_bind_execute_sync(&mut s, "ps_v", &[&key_s]);
        let _ = read_until_ready(&mut s);
        let dt = t0.elapsed().as_nanos();
        if i >= WARMUP {
            prep.push(dt);
        }
    }

    let simple_p50 = pct(&mut simple.clone(), 0.50);
    let simple_p90 = pct(&mut simple.clone(), 0.90);
    let simple_p99 = pct(&mut simple, 0.99);
    let prep_p50 = pct(&mut prep.clone(), 0.50);
    let prep_p90 = pct(&mut prep.clone(), 0.90);
    let prep_p99 = pct(&mut prep, 0.99);

    println!();
    println!("── v6.1.1 PG-wire Simple Q vs Extended Q  (short SELECT, N={N}) ──");
    println!(
        "  Simple   p50={:>7} ns  p90={:>7} ns  p99={:>7} ns",
        simple_p50, simple_p90, simple_p99
    );
    println!(
        "  Prepared p50={:>7} ns  p90={:>7} ns  p99={:>7} ns",
        prep_p50, prep_p90, prep_p99
    );
    let win = (simple_p50 as f64 - prep_p50 as f64) / simple_p50 as f64 * 100.0;
    println!(
        "  p50 win  = {:.1}%  ({}ns → {}ns)",
        win, simple_p50, prep_p50
    );
    println!();
}

/// Compare simple-vs-prepared on a realistic long query: a 128-dim
/// vector kNN with the embedding rendered into the SQL text. This
/// is the shape where AST caching has actual SQL-parsing work to
/// amortise.
#[test]
#[ignore]
fn perf_prepared_vs_simple_vector_knn_p50() {
    let _lock = crate::perf_lock();
    const N: usize = 1_000;
    const WARMUP: usize = 50;
    const DIM: usize = 128;
    const ROWS: usize = 1_024;

    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    send_query(
        &mut s,
        &format!("CREATE TABLE v (id INT NOT NULL, e VECTOR({DIM}) NOT NULL)"),
    );
    let _ = read_until_ready(&mut s);
    for i in 0..ROWS {
        let lit = vec_literal(i as u64 + 1, DIM);
        send_query(&mut s, &format!("INSERT INTO v VALUES ({i}, {lit})"));
        let _ = read_until_ready(&mut s);
    }

    // Simple-Q timings — full SQL body sent each round.
    let mut simple = Vec::with_capacity(N);
    for i in 0..(N + WARMUP) {
        let q = vec_literal(0xBEEF + i as u64, DIM);
        let sql = format!("SELECT id FROM v ORDER BY e <-> {q} LIMIT 10");
        let t0 = Instant::now();
        send_query(&mut s, &sql);
        let _ = read_until_ready(&mut s);
        let dt = t0.elapsed().as_nanos();
        if i >= WARMUP {
            simple.push(dt);
        }
    }

    // Extended-Q: Parse once, Bind+Execute+Sync per round.
    send_parse(
        &mut s,
        "ps_knn",
        "SELECT id FROM v ORDER BY e <-> $1 LIMIT 10",
    );
    send_sync(&mut s);
    let _ = read_until_ready(&mut s);

    let mut prep = Vec::with_capacity(N);
    for i in 0..(N + WARMUP) {
        let q = vec_literal(0xBEEF + i as u64, DIM);
        let t0 = Instant::now();
        send_bind_execute_sync(&mut s, "ps_knn", &[&q]);
        let _ = read_until_ready(&mut s);
        let dt = t0.elapsed().as_nanos();
        if i >= WARMUP {
            prep.push(dt);
        }
    }

    let simple_p50 = pct(&mut simple.clone(), 0.50);
    let simple_p90 = pct(&mut simple.clone(), 0.90);
    let simple_p99 = pct(&mut simple, 0.99);
    let prep_p50 = pct(&mut prep.clone(), 0.50);
    let prep_p90 = pct(&mut prep.clone(), 0.90);
    let prep_p99 = pct(&mut prep, 0.99);

    println!();
    println!(
        "── v6.1.1 PG-wire Simple Q vs Extended Q  (vector kNN dim={DIM}, rows={ROWS}, N={N}) ──"
    );
    println!(
        "  Simple   p50={:>7} ns  p90={:>7} ns  p99={:>7} ns",
        simple_p50, simple_p90, simple_p99
    );
    println!(
        "  Prepared p50={:>7} ns  p90={:>7} ns  p99={:>7} ns",
        prep_p50, prep_p90, prep_p99
    );
    let win = (simple_p50 as f64 - prep_p50 as f64) / simple_p50 as f64 * 100.0;
    println!(
        "  p50 win  = {:.1}%  ({}ns → {}ns)",
        win, simple_p50, prep_p50
    );
    println!();
}
