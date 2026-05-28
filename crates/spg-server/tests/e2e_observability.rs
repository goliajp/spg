#![allow(
    clippy::doc_markdown,
    clippy::cast_possible_truncation,
    clippy::uninlined_format_args
)]

//! v4.13 observability: /healthz + /metrics + structured logging.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Op, build_query, encode};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(3);

fn pick_free_addr() -> String {
    let p = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = p.local_addr().unwrap();
    drop(p);
    a.to_string()
}

fn spawn_server(addr: &str, http_addr: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_spg-server"))
        .arg(addr)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("SPG_HTTP_ADDR", http_addr)
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD")
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
    // Hand-rolled GET. Wait briefly for the HTTP listener to bind
    // (it spawns from the native listener's startup path).
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
    let (status_line, rest) = response.split_once("\r\n").unwrap_or((&response, ""));
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = rest.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (code, body.to_string())
}

#[test]
fn healthz_returns_200() {
    let native = pick_free_addr();
    let http = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&native, &http));
    let _ = wait_for_listener(&native, &mut child.0);
    let (code, body) = http_get(&http, "/healthz");
    assert_eq!(code, 200);
    assert!(body.starts_with("ok"));
}

#[test]
fn metrics_emits_prometheus_text() {
    let native = pick_free_addr();
    let http = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&native, &http));
    let _ = wait_for_listener(&native, &mut child.0);
    let (code, body) = http_get(&http, "/metrics");
    assert_eq!(code, 200);
    assert!(body.contains("# TYPE spg_server_info gauge"));
    assert!(body.contains("spg_server_info{version=\""));
    assert!(body.contains("spg_connections_active "));
    assert!(body.contains("spg_queries_total "));
    assert!(body.contains("spg_errors_total "));
}

#[test]
fn metrics_counter_increments_per_query() {
    let native = pick_free_addr();
    let http = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&native, &http));
    let mut s = wait_for_listener(&native, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // Run 3 queries to bump the counter.
    for _ in 0..3 {
        let q = build_query("CREATE TABLE if_not_exists_table_check (id INT NOT NULL)");
        let mut out = Vec::new();
        encode(&q, &mut out).unwrap();
        s.write_all(&out).unwrap();
        // Drain the response frame so the next query proceeds.
        let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
        if s.read_exact(&mut header).is_err() {
            break;
        }
        let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let _op = Op::from_byte(header[4]).unwrap();
        if payload_len > 0 {
            let mut payload = vec![0u8; payload_len];
            let _ = s.read_exact(&mut payload);
        }
    }
    // Brief settle so the server's atomic increments are visible
    // from the HTTP thread.
    thread::sleep(Duration::from_millis(50));
    let (code, body) = http_get(&http, "/metrics");
    assert_eq!(code, 200);
    let line = body
        .lines()
        .find(|l| l.starts_with("spg_queries_total "))
        .expect("queries_total line present");
    let value: u64 = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    assert!(value >= 3, "expected ≥3 queries counted, got {value}");
}

#[test]
fn unknown_path_returns_404() {
    let native = pick_free_addr();
    let http = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&native, &http));
    let _ = wait_for_listener(&native, &mut child.0);
    let (code, _body) = http_get(&http, "/does-not-exist");
    assert_eq!(code, 404);
}

/// v5.2.1 — `/metrics` exposes `spg_hot_tier_bytes_used` (live counter
/// from `Catalog::hot_tier_bytes()`) and `spg_hot_tier_bytes_budget`
/// (the configured cap from `SPG_HOT_TIER_BYTES`, default 4 GiB).
#[test]
fn metrics_emits_hot_tier_budget_default_4gib() {
    let native = pick_free_addr();
    let http = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&native, &http));
    let _ = wait_for_listener(&native, &mut child.0);
    let (code, body) = http_get(&http, "/metrics");
    assert_eq!(code, 200);
    let budget_line = body
        .lines()
        .find(|l| l.starts_with("spg_hot_tier_bytes_budget "))
        .expect("budget line emitted");
    let budget: u64 = budget_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(budget, 4 * 1024 * 1024 * 1024, "default budget is 4 GiB");
    let used_line = body
        .lines()
        .find(|l| l.starts_with("spg_hot_tier_bytes_used "))
        .expect("used line emitted");
    let used: u64 = used_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    // Fresh server, no user tables — empty catalog → used = 0.
    assert_eq!(used, 0, "fresh server starts with 0 hot bytes");
}

#[test]
fn metrics_hot_tier_budget_honors_env_override() {
    let native = pick_free_addr();
    let http = pick_free_addr();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
    cmd.arg(&native)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("SPG_HTTP_ADDR", &http)
        .env("SPG_HOT_TIER_BYTES", "1048576") // 1 MiB
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD");
    let mut child = ChildGuard(cmd.spawn().unwrap());
    let _ = wait_for_listener(&native, &mut child.0);
    let (code, body) = http_get(&http, "/metrics");
    assert_eq!(code, 200);
    let line = body
        .lines()
        .find(|l| l.starts_with("spg_hot_tier_bytes_budget "))
        .expect("budget line emitted");
    assert!(
        line.ends_with(" 1048576"),
        "budget honors SPG_HOT_TIER_BYTES override; got {line:?}"
    );
}

#[test]
fn metrics_hot_tier_used_grows_after_insert() {
    let native = pick_free_addr();
    let http = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&native, &http));
    let mut s = wait_for_listener(&native, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    // CREATE TABLE + a handful of INSERTs.
    let stmts = [
        "CREATE TABLE hot_growth (id INT NOT NULL, name TEXT NOT NULL)",
        "INSERT INTO hot_growth VALUES (1, 'alice')",
        "INSERT INTO hot_growth VALUES (2, 'bob')",
        "INSERT INTO hot_growth VALUES (3, 'carol-the-longer-name-payload')",
    ];
    for sql in stmts {
        let q = build_query(sql);
        let mut out = Vec::new();
        encode(&q, &mut out).unwrap();
        s.write_all(&out).unwrap();
        let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
        s.read_exact(&mut header).unwrap();
        let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        if payload_len > 0 {
            let mut payload = vec![0u8; payload_len];
            s.read_exact(&mut payload).unwrap();
        }
    }
    thread::sleep(Duration::from_millis(50));
    let (code, body) = http_get(&http, "/metrics");
    assert_eq!(code, 200);
    let line = body
        .lines()
        .find(|l| l.starts_with("spg_hot_tier_bytes_used "))
        .expect("used line emitted");
    let used: u64 = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    // Each row is bitmap(1) + int(4) + text(2 + N). With our payloads
    // that's ~14 + 12 + 36 = ~62 bytes; allow a generous lower bound
    // for schema-driven variation.
    assert!(
        used >= 30,
        "expected used > 30 B after 3 inserts, got {used}"
    );
}

#[test]
fn structured_json_log_format_can_be_enabled() {
    // We can't easily inspect server stderr without piping it; just
    // boot with SPG_LOG_FORMAT=json and verify it still serves
    // requests (no panic on the new code path).
    let native = pick_free_addr();
    let http = pick_free_addr();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
    cmd.arg(&native)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("SPG_HTTP_ADDR", &http)
        .env("SPG_LOG_FORMAT", "json")
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD");
    let mut child = ChildGuard(cmd.spawn().unwrap());
    let _ = wait_for_listener(&native, &mut child.0);
    let (code, _body) = http_get(&http, "/healthz");
    assert_eq!(code, 200);
}
