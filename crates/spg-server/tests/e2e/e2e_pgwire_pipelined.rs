//! v6.3.2 — Pipelined query mode e2e + ship gate.
//!
//! Hand-rolled PG extended-query client. Compares wall-clock for N
//! Bind/Execute cycles all batched under one Sync against N cycles
//! each with its own Sync. The ship gate: pipelined ≤ 1.3 × single-
//! cycle RTT.
//!
//! On loopback the kernel coalesces small writes aggressively, so
//! the gate often passes without server-side response buffering.
//! The gate is here to lock that property in so a future refactor
//! that introduces per-message flushes can't silently regress.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const READ_TIMEOUT: Duration = Duration::from_secs(10);
const BATCH_SIZE: usize = 16;
const ITERS: u32 = 32;

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-e2e-pgwire-pipelined-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn local_spawn(db: &std::path::Path) -> (std::process::Child, common::ServerAddrs) {
    let b = common::ServerBuilder::new().arg_path(db).with_pgwire();
    b.spawn()
}

// ── PG wire helpers ───────────────────────────────────────────────

struct PgMessage {
    ty: u8,
    body: Vec<u8>,
}

fn read_message(s: &mut TcpStream) -> PgMessage {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("pg header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let body_len = len.saturating_sub(4);
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
        s.read_exact(&mut body).expect("pg body");
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

fn read_until_ready(s: &mut TcpStream) {
    loop {
        let m = read_message(s);
        if m.ty == b'Z' {
            return;
        }
    }
}

fn write_msg(buf: &mut Vec<u8>, ty: u8, body: &[u8]) {
    buf.push(ty);
    let len = (body.len() + 4) as u32;
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(body);
}

fn parse_msg_body(name: &str, sql: &str) -> Vec<u8> {
    let mut b = Vec::with_capacity(name.len() + sql.len() + 8);
    b.extend_from_slice(name.as_bytes());
    b.push(0);
    b.extend_from_slice(sql.as_bytes());
    b.push(0);
    b.extend_from_slice(&0u16.to_be_bytes()); // zero parameter type OIDs
    b
}

fn bind_msg_body(portal: &str, stmt: &str) -> Vec<u8> {
    let mut b = Vec::with_capacity(portal.len() + stmt.len() + 16);
    b.extend_from_slice(portal.as_bytes());
    b.push(0);
    b.extend_from_slice(stmt.as_bytes());
    b.push(0);
    b.extend_from_slice(&0u16.to_be_bytes()); // 0 format codes
    b.extend_from_slice(&0u16.to_be_bytes()); // 0 parameter values
    b.extend_from_slice(&0u16.to_be_bytes()); // 0 result-format codes
    b
}

fn execute_msg_body(portal: &str, max_rows: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(portal.len() + 8);
    b.extend_from_slice(portal.as_bytes());
    b.push(0);
    b.extend_from_slice(&max_rows.to_be_bytes());
    b
}

fn sync_msg_body() -> Vec<u8> {
    Vec::new()
}

// ── Test scaffolding ──────────────────────────────────────────────

fn setup_session(addr: &str) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "anyone");
    let ok = read_message(&mut s);
    assert_eq!(ok.ty, b'R');
    read_until_ready(&mut s);
    // Create + populate a small table once so every Execute returns
    // a known row set.
    {
        let mut q = Vec::new();
        let sql = "CREATE TABLE pl (id INT, name TEXT)";
        let mut body = sql.as_bytes().to_vec();
        body.push(0);
        write_msg(&mut q, b'Q', &body);
        s.write_all(&q).unwrap();
        read_until_ready(&mut s);
    }
    {
        let mut q = Vec::new();
        let sql = "INSERT INTO pl VALUES (1, 'a'), (2, 'b'), (3, 'c')";
        let mut body = sql.as_bytes().to_vec();
        body.push(0);
        write_msg(&mut q, b'Q', &body);
        s.write_all(&q).unwrap();
        read_until_ready(&mut s);
    }
    // Parse statement once (named).
    {
        let mut q = Vec::new();
        write_msg(
            &mut q,
            b'P',
            &parse_msg_body("stmt", "SELECT id, name FROM pl"),
        );
        write_msg(&mut q, b'S', &sync_msg_body());
        s.write_all(&q).unwrap();
        read_until_ready(&mut s);
    }
    s
}

fn run_single_cycle(s: &mut TcpStream) {
    // One Bind + Execute + Sync. Reads 1+RD+3+CC+ReadyForQuery.
    let mut q = Vec::new();
    write_msg(&mut q, b'B', &bind_msg_body("", "stmt"));
    write_msg(&mut q, b'E', &execute_msg_body("", 0));
    write_msg(&mut q, b'S', &sync_msg_body());
    s.write_all(&q).unwrap();
    read_until_ready(s);
}

fn run_pipelined_batch(s: &mut TcpStream, n: usize) {
    // N (Bind + Execute) cycles followed by a single Sync.
    let mut q = Vec::new();
    for _ in 0..n {
        write_msg(&mut q, b'B', &bind_msg_body("", "stmt"));
        write_msg(&mut q, b'E', &execute_msg_body("", 0));
    }
    write_msg(&mut q, b'S', &sync_msg_body());
    s.write_all(&q).unwrap();
    read_until_ready(s);
}

// ── The gate ──────────────────────────────────────────────────────

#[test]
fn pipelined_batch_under_1_3x_single_rtt() {
    let dir = unique_tmpdir("gate");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let addr = addrs.pgwire.as_ref().expect("pgwire listening");
    let mut s = setup_session(addr);

    // Warm.
    for _ in 0..4 {
        run_single_cycle(&mut s);
    }

    // r1018 — INTERLEAVED, one of each per iteration, rather than all the
    // singles and then all the batches.
    //
    // The ratio is the whole assertion, and measuring its two halves in
    // sequence lets anything that changes the machine BETWEEN them forge
    // one. The r1018 gate run failed here at 1.471 against a 1.3 bound,
    // and the change under test touched only the write path while this
    // measures a prepared SELECT; two further runs of the same binary
    // passed. What moved was the testbed, during the second half — the
    // parallel e2e runner is the load, so the gate supplies its own
    // false positives. Interleaving cancels drift that is slower than
    // one iteration, which is what machine load is.
    //
    // The reading moved as well as steadied: interleaved, three runs give
    // 0.221-0.252 against a 1.3 bound, which is the four-fold gain saving
    // N-1 round trips is supposed to produce. Sequentially it read 1.471.
    // The bound is left where V6_3_DESIGN put it; the headroom is now
    // visible rather than assumed.
    let mut single_total = Duration::ZERO;
    let mut pipelined_total = Duration::ZERO;
    for _ in 0..ITERS {
        let t0 = Instant::now();
        run_single_cycle(&mut s);
        single_total += t0.elapsed();

        // Same total work as BATCH_SIZE single cycles, one Sync for all.
        let t1 = Instant::now();
        run_pipelined_batch(&mut s, BATCH_SIZE);
        pipelined_total += t1.elapsed();
    }
    let single_per_cycle = single_total / ITERS;
    let pipelined_per_batch = pipelined_total / ITERS;
    let pipelined_amortised_per_cycle = pipelined_per_batch / BATCH_SIZE as u32;

    let ratio =
        pipelined_amortised_per_cycle.as_nanos() as f64 / single_per_cycle.as_nanos() as f64;

    eprintln!(
        "v6.3.2 pipeline gate: single = {} µs/cycle, pipelined({BATCH_SIZE}) = {} µs/batch \
         → amortised {} µs/cycle, ratio = {:.3}",
        single_per_cycle.as_micros(),
        pipelined_per_batch.as_micros(),
        pipelined_amortised_per_cycle.as_micros(),
        ratio
    );

    // Strict ship gate from V6_3_DESIGN: amortised ≤ 1.3 × single is
    // the headline, but realistically pipelined should be much
    // faster than single since we save N-1 ReadyForQuery roundtrips.
    // The 1.3× bound checks "no significant regression beyond noise"
    // even on loopback where pipelining gain is muted.
    assert!(
        ratio <= 1.3,
        "pipelined amortised per-cycle must be ≤ 1.3× single per-cycle; ratio = {ratio:.3}"
    );
}

#[test]
fn pipelined_batch_returns_correct_row_count() {
    // Smoke test: a batch of N Bind/Execute still returns N row sets
    // and the connection survives.
    let dir = unique_tmpdir("smoke");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let mut child = common::ChildGuard(raw);
    let addr = addrs.pgwire.as_ref().expect("pgwire listening");
    let mut s = setup_session(addr);

    // Send 8 Bind+Execute pairs then a single Sync, count
    // CommandComplete (`C`) messages.
    let mut q = Vec::new();
    for _ in 0..8 {
        write_msg(&mut q, b'B', &bind_msg_body("", "stmt"));
        write_msg(&mut q, b'E', &execute_msg_body("", 0));
    }
    write_msg(&mut q, b'S', &sync_msg_body());
    s.write_all(&q).unwrap();

    let mut cc_count = 0;
    loop {
        let m = read_message(&mut s);
        if m.ty == b'C' {
            cc_count += 1;
        }
        if m.ty == b'Z' {
            break;
        }
    }
    assert_eq!(cc_count, 8, "expected one CommandComplete per Execute");
}
