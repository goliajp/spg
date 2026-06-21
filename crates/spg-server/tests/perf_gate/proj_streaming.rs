//! v7.37 — PROJ extended-protocol streaming probe.
//!
//! `cargo test --release -p spg-server --test perf_gate \
//!    perf_proj_streaming -- --ignored --nocapture`
//!
//! Seeds the inbox_25k shape locally, then drives the PROJ query
//! (5-cell × 25 k-row projection) through both pgwire paths against
//! the same SPG server:
//!   * Simple-query (`b'Q'`): the legacy materialising path.
//!   * Extended-query (Parse / Bind / Execute / Sync): the v7.37
//!     streaming path — joined-projection streamer skips
//!     `Vec<Row<'static>>` + per-cell `.cloned()`.
//!
//! Reports p50 / p90 / p99 for both and the simple → extended win.
//! Ignored by default; meant for ad-hoc verification on mini.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::common;

const READ_TIMEOUT: Duration = Duration::from_secs(60);

fn unique_tmpdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-perf-projstream-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn local_spawn(db: &std::path::Path) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .with_pgwire()
        .spawn()
}

fn read_message(s: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let body_len = len.saturating_sub(4);
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
        s.read_exact(&mut body).expect("body");
    }
    (ty, body)
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

fn send_bind_execute_sync(s: &mut TcpStream, stmt: &str) {
    let mut out = Vec::with_capacity(64);
    // Bind — empty portal, no params.
    let mut bind_body = Vec::new();
    bind_body.push(0);
    bind_body.extend_from_slice(stmt.as_bytes());
    bind_body.push(0);
    bind_body.extend_from_slice(&0u16.to_be_bytes());
    bind_body.extend_from_slice(&0u16.to_be_bytes());
    bind_body.extend_from_slice(&0u16.to_be_bytes());
    out.push(b'B');
    out.extend_from_slice(&((bind_body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&bind_body);
    // Execute
    let mut exec_body = Vec::new();
    exec_body.push(0);
    exec_body.extend_from_slice(&0i32.to_be_bytes());
    out.push(b'E');
    out.extend_from_slice(&((exec_body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&exec_body);
    // Sync
    out.push(b'S');
    out.extend_from_slice(&4u32.to_be_bytes());
    s.write_all(&out).unwrap();
}

fn read_until_ready(s: &mut TcpStream) {
    loop {
        let (ty, _) = read_message(s);
        if ty == b'Z' {
            return;
        }
    }
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "anyone");
    read_until_ready(&mut s);
    s
}

fn pct(samples: &mut [u128], p: f64) -> u128 {
    samples.sort_unstable();
    let idx = ((samples.len() as f64) * p) as usize;
    samples[idx.min(samples.len() - 1)]
}

const PROJ_SQL: &str = "SELECT m.id, m.subject, m.sender, m.internal_date, mb.user_address \
                       FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
                       WHERE mb.user_address = 'u@x'";

fn seed_inbox(s: &mut TcpStream) {
    send_query(
        s,
        "CREATE TABLE mailboxes (id BIGSERIAL PRIMARY KEY, name TEXT, user_address TEXT)",
    );
    read_until_ready(s);
    send_query(
        s,
        "CREATE TABLE messages (id BIGSERIAL PRIMARY KEY, mailbox_id BIGINT, thread_id TEXT, \
         subject TEXT, sender TEXT, internal_date BIGINT, flags BIGINT, pinned BOOLEAN, \
         archived BOOLEAN, importance_level TEXT, importance_score REAL, message_id TEXT, \
         text_body TEXT)",
    );
    read_until_ready(s);
    for i in 0..25 {
        send_query(
            s,
            &format!("INSERT INTO mailboxes (name, user_address) VALUES ('mb{i}', 'u@x')"),
        );
        read_until_ready(s);
    }
    // Insert 25 k messages in batches of 500.
    let mut vals = String::new();
    let mut count = 0;
    for i in 0..25_000 {
        if !vals.is_empty() {
            vals.push(',');
        }
        use std::fmt::Write;
        let mb_id = (i % 25) + 1;
        let _ = write!(
            vals,
            "({mb_id}, 'thr{}', 'subject {i}', 'sender{}@x', {}, 0, false, false, 'normal', 0.5, 'mid{i}', 'body text body text {i}')",
            i % 1000,
            i % 100,
            1_700_000_000_i64 + i as i64
        );
        count += 1;
        if count == 500 {
            let sql = format!(
                "INSERT INTO messages (mailbox_id, thread_id, subject, sender, internal_date, flags, pinned, archived, importance_level, importance_score, message_id, text_body) VALUES {vals}"
            );
            send_query(s, &sql);
            read_until_ready(s);
            vals.clear();
            count = 0;
        }
    }
    if !vals.is_empty() {
        let sql = format!(
            "INSERT INTO messages (mailbox_id, thread_id, subject, sender, internal_date, flags, pinned, archived, importance_level, importance_score, message_id, text_body) VALUES {vals}"
        );
        send_query(s, &sql);
        read_until_ready(s);
    }
}

#[test]
#[ignore]
fn perf_proj_streaming() {
    let _lock = crate::perf_lock();
    const N: usize = 30;
    const WARMUP: usize = 5;
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    eprintln!("seeding 25 k inbox shape…");
    seed_inbox(&mut s);
    eprintln!("seeded");

    // Warm up + measure simple-query PROJ.
    let mut simple: Vec<u128> = Vec::with_capacity(N);
    for i in 0..(N + WARMUP) {
        let t0 = Instant::now();
        send_query(&mut s, PROJ_SQL);
        read_until_ready(&mut s);
        let dt = t0.elapsed().as_nanos();
        if i >= WARMUP {
            simple.push(dt);
        }
    }

    // Parse a no-param prepared form.
    send_parse(&mut s, "proj", PROJ_SQL);
    send_sync(&mut s);
    read_until_ready(&mut s);

    // Warm up + measure extended-query PROJ.
    let mut prep: Vec<u128> = Vec::with_capacity(N);
    for i in 0..(N + WARMUP) {
        let t0 = Instant::now();
        send_bind_execute_sync(&mut s, "proj");
        read_until_ready(&mut s);
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

    eprintln!();
    eprintln!("── v7.37 PROJ streaming probe ({N} iters, 25 k-row shape) ──");
    eprintln!(
        "  Simple-Q   p50={:>9} ns  p90={:>9} ns  p99={:>9} ns",
        simple_p50, simple_p90, simple_p99
    );
    eprintln!(
        "  Extended-Q p50={:>9} ns  p90={:>9} ns  p99={:>9} ns  (streaming path)",
        prep_p50, prep_p90, prep_p99
    );
    let p50_ms_simple = simple_p50 as f64 / 1_000_000.0;
    let p50_ms_prep = prep_p50 as f64 / 1_000_000.0;
    eprintln!(
        "  p50 simple = {:.2} ms / extended = {:.2} ms  (Δ = {:+.2} ms,  {:+.1}%)",
        p50_ms_simple,
        p50_ms_prep,
        p50_ms_prep - p50_ms_simple,
        (p50_ms_prep - p50_ms_simple) / p50_ms_simple * 100.0
    );
}
