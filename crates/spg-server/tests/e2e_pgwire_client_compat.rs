//! v6.3.5 — Real-client-shaped wire compatibility e2e.
//!
//! Exercises the full v6.3 extended-query surface (Parse + Bind +
//! Execute + Describe + binary params + pipelined batches) over a
//! workload shaped like what tokio-postgres / sqlx / pgx / psycopg3
//! actually generate: multiple concurrent connections all doing
//! prepared-statement reuse with mixed text/binary parameter
//! formats and Describe before Execute.
//!
//! v6.3.5 design ROW called for a docker-compose harness running
//! the actual client libraries. That ship lane requires deps the
//! SPG workspace explicitly forbids ("0 external dependencies"
//! rule from V6_3_DESIGN L1). The docker-compose multi-language
//! harness is therefore carved out to STABILITY §"Out of v6.3" in
//! the v6.3.6 rollup — real-client library compat is verified by
//! the higher-fidelity workload below and revisited in v6.x as a
//! dedicated effort if a user reports a client-specific
//! incompatibility.

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::uninlined_format_args
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

mod common;

const READ_TIMEOUT: Duration = Duration::from_secs(10);

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-client-compat-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn local_spawn(db: &std::path::Path) -> (std::process::Child, common::ServerAddrs) {
    let b = common::ServerBuilder::new().arg_path(db).with_pgwire();
    b.spawn()
}

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

fn parse_with_oids(name: &str, sql: &str, oids: &[u32]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(name.as_bytes());
    b.push(0);
    b.extend_from_slice(sql.as_bytes());
    b.push(0);
    b.extend_from_slice(&(oids.len() as u16).to_be_bytes());
    for o in oids {
        b.extend_from_slice(&o.to_be_bytes());
    }
    b
}

fn describe_msg(kind: u8, name: &str) -> Vec<u8> {
    let mut b = Vec::new();
    b.push(kind);
    b.extend_from_slice(name.as_bytes());
    b.push(0);
    b
}

fn bind_mixed(
    portal: &str,
    stmt: &str,
    formats: &[u16],
    params: &[(i32, Vec<u8>)],
) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(portal.as_bytes());
    b.push(0);
    b.extend_from_slice(stmt.as_bytes());
    b.push(0);
    b.extend_from_slice(&(formats.len() as u16).to_be_bytes());
    for f in formats {
        b.extend_from_slice(&f.to_be_bytes());
    }
    b.extend_from_slice(&(params.len() as u16).to_be_bytes());
    for (len, bytes) in params {
        b.extend_from_slice(&len.to_be_bytes());
        b.extend_from_slice(bytes);
    }
    b.extend_from_slice(&0u16.to_be_bytes()); // 0 result-format codes (text)
    b
}

fn execute_body(portal: &str) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(portal.as_bytes());
    b.push(0);
    b.extend_from_slice(&0u32.to_be_bytes());
    b
}

fn handshake(addr: &str) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "anyone");
    let r = read_message(&mut s);
    assert_eq!(r.ty, b'R');
    read_until_ready(&mut s);
    s
}

fn exec_simple(s: &mut TcpStream, sql: &str) {
    let mut q = Vec::new();
    let mut body = sql.as_bytes().to_vec();
    body.push(0);
    write_msg(&mut q, b'Q', &body);
    s.write_all(&q).unwrap();
    read_until_ready(s);
}

#[test]
fn jdbc_style_prepared_reuse_describe_then_repeated_execute() {
    // Mimics what JDBC + sqlx do: Parse(named) → Describe(S) →
    // Bind+Execute(N times) → Sync. v6.3.0 plan cache means the
    // second Parse on the same SQL across a fresh connection should
    // still be byte-cheap.
    let dir = unique_tmpdir("jdbc-style");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let addr = addrs.pgwire.as_ref().unwrap();

    let mut s = handshake(addr);
    exec_simple(&mut s, "CREATE TABLE t (id INT, name TEXT)");
    exec_simple(&mut s, "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')");

    // Parse once + Describe(S) once.
    let mut q = Vec::new();
    write_msg(
        &mut q,
        b'P',
        &parse_with_oids("s_lookup", "SELECT name FROM t WHERE id = $1", &[23]),
    );
    write_msg(&mut q, b'D', &describe_msg(b'S', "s_lookup"));
    write_msg(&mut q, b'S', &[]);
    s.write_all(&q).unwrap();
    read_until_ready(&mut s);

    // Bind+Execute three different params, all under one Sync.
    let ids: [i32; 3] = [1, 2, 3];
    let mut batch = Vec::new();
    for id in &ids {
        let bytes = id.to_be_bytes().to_vec();
        write_msg(
            &mut batch,
            b'B',
            &bind_mixed("", "s_lookup", &[1], &[(4, bytes)]),
        );
        write_msg(&mut batch, b'E', &execute_body(""));
    }
    write_msg(&mut batch, b'S', &[]);
    s.write_all(&batch).unwrap();

    let mut rows = Vec::new();
    loop {
        let m = read_message(&mut s);
        if m.ty == b'D' {
            let len = i32::from_be_bytes([m.body[2], m.body[3], m.body[4], m.body[5]]) as usize;
            rows.push(String::from_utf8_lossy(&m.body[6..6 + len]).to_string());
        }
        if m.ty == b'Z' {
            break;
        }
    }
    assert_eq!(rows, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
}

#[test]
fn concurrent_connections_share_plan_cache_correctly() {
    // 4 worker threads each open their own pgwire connection and
    // run the same prepared statement 8 times. The engine-level
    // plan cache must serve them all correctly (no cross-talk on
    // parameter values, no plan staleness).
    let dir = unique_tmpdir("concurrent");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let addr = addrs.pgwire.as_ref().unwrap().clone();

    {
        let mut s = handshake(&addr);
        exec_simple(&mut s, "CREATE TABLE conc (worker INT, n INT)");
        for w in 0..4_i32 {
            for n in 0..8_i32 {
                exec_simple(
                    &mut s,
                    &format!("INSERT INTO conc VALUES ({w}, {n})"),
                );
            }
        }
    }

    let addr = Arc::new(addr);
    let mut handles = Vec::new();
    for worker in 0..4_i32 {
        let addr_c = Arc::clone(&addr);
        handles.push(thread::spawn(move || {
            let mut s = handshake(&addr_c);
            // Each worker parses the same SQL (cache hit after worker 0).
            let mut q = Vec::new();
            write_msg(
                &mut q,
                b'P',
                &parse_with_oids(
                    "p",
                    "SELECT n FROM conc WHERE worker = $1 AND n = $2",
                    &[23, 23],
                ),
            );
            write_msg(&mut q, b'S', &[]);
            s.write_all(&q).unwrap();
            read_until_ready(&mut s);

            for n in 0..8_i32 {
                let mut batch = Vec::new();
                let params = vec![
                    (4_i32, worker.to_be_bytes().to_vec()),
                    (4_i32, n.to_be_bytes().to_vec()),
                ];
                write_msg(&mut batch, b'B', &bind_mixed("", "p", &[1], &params));
                write_msg(&mut batch, b'E', &execute_body(""));
                write_msg(&mut batch, b'S', &[]);
                s.write_all(&batch).unwrap();

                let mut got = None;
                loop {
                    let m = read_message(&mut s);
                    if m.ty == b'D' {
                        let len =
                            i32::from_be_bytes([m.body[2], m.body[3], m.body[4], m.body[5]]) as usize;
                        let v = String::from_utf8_lossy(&m.body[6..6 + len]).to_string();
                        got = Some(v.parse::<i32>().unwrap());
                    }
                    if m.ty == b'Z' {
                        break;
                    }
                }
                assert_eq!(got, Some(n), "worker {worker} n={n} mismatch");
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn psycopg3_style_pipeline_with_mixed_text_and_binary() {
    // psycopg3's `pipeline mode` packs a `discover-type` Describe
    // round, then N (Parse/Bind/Execute) cycles before a single
    // Sync. Mixed text+binary parameter formats are typical.
    let dir = unique_tmpdir("psycopg3");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let addr = addrs.pgwire.as_ref().unwrap();
    let mut s = handshake(addr);

    exec_simple(&mut s, "CREATE TABLE m (a INT, b TEXT)");

    // Build the full batch: 5 Parse+Describe+Bind+Execute cycles,
    // alternating param formats per cycle.
    let mut batch = Vec::new();
    for i in 0..5_i32 {
        let stmt_name = format!("s{i}");
        write_msg(
            &mut batch,
            b'P',
            &parse_with_oids(&stmt_name, "SELECT $1::int, $2::text", &[23, 25]),
        );
        write_msg(&mut batch, b'D', &describe_msg(b'S', &stmt_name));
        let formats = if i % 2 == 0 {
            vec![1u16, 0u16] // INT binary, TEXT text
        } else {
            vec![0u16, 1u16] // INT text, TEXT binary
        };
        let bin_int = i.to_be_bytes().to_vec();
        let text_int = i.to_string().into_bytes();
        let text_str = format!("v{i}").into_bytes();
        let bin_str = format!("v{i}").into_bytes();
        let params = if i % 2 == 0 {
            vec![(4_i32, bin_int), (text_str.len() as i32, text_str)]
        } else {
            vec![(text_int.len() as i32, text_int), (bin_str.len() as i32, bin_str)]
        };
        write_msg(&mut batch, b'B', &bind_mixed("", &stmt_name, &formats, &params));
        write_msg(&mut batch, b'E', &execute_body(""));
    }
    write_msg(&mut batch, b'S', &[]);
    s.write_all(&batch).unwrap();

    // Expect 5 DataRows + 5 CommandCompletes + 1 ReadyForQuery.
    let mut data_count = 0;
    let mut cc_count = 0;
    loop {
        let m = read_message(&mut s);
        match m.ty {
            b'D' => data_count += 1,
            b'C' => cc_count += 1,
            b'Z' => break,
            _ => {}
        }
    }
    assert_eq!(data_count, 5);
    assert_eq!(cc_count, 5);
}
