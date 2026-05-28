#![allow(
    clippy::doc_markdown,
    clippy::cast_possible_truncation,
    clippy::uninlined_format_args
)]

//! v5.4.1: end-to-end validation that the async-commit flusher
//! thread spawns iff `SPG_SYNCHRONOUS_COMMIT=off` and surfaces
//! its iteration cadence via `/metrics`.
//!
//! These tests pin the *shape* of the v5.4 async-commit env
//! contract (default = sync, opt-in via env var, lifecycle tied
//! to the v4.13 metrics surface). They deliberately do not yet
//! exercise the durability-window guarantee — that's v5.4.3's
//! `chaos_kill_during_async_commit_window` test. v5.4.1's gate
//! is just "the thread shows up exactly when the env var says
//! it should and reports progress."

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(3);

fn pick_free_addr() -> String {
    let p = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = p.local_addr().unwrap();
    drop(p);
    a.to_string()
}

fn wait_for_listener(addr: &str, child: &mut Child) {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        if let Ok(Some(status)) = child.try_wait() {
            panic!("server exited early: {status:?}");
        }
        assert!(Instant::now() < deadline, "server at {addr} never came up");
        thread::sleep(Duration::from_millis(20));
    }
}

fn http_get_body(addr: &str) -> String {
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
    let req = "GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    stream.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();
    let response = String::from_utf8_lossy(&buf).to_string();
    response
        .split_once("\r\n\r\n")
        .map_or(String::new(), |(_, b)| b.to_string())
}

fn flusher_iterations(http: &str) -> u64 {
    let body = http_get_body(http);
    body.lines()
        .find_map(|l| l.strip_prefix("spg_flusher_iterations_total "))
        .map(|s| s.trim().parse::<u64>().unwrap_or(0))
        .unwrap_or(0)
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn flusher_metric_zero_in_default_sync_commit_mode() {
    // Default mode: SPG_SYNCHRONOUS_COMMIT unset → sync semantics
    // → flusher thread is not spawned. After 150 ms of liveness
    // the counter must still read 0; non-zero would mean a spurious
    // spawn that breaks the v5.4 contract (sync mode preserves
    // every v4.42 durability invariant exactly).
    let native = pick_free_addr();
    let http = pick_free_addr();
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_spg-server"))
            .arg(&native)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env("SPG_HTTP_ADDR", &http)
            .env_remove("SPG_PASSWORD")
            .env_remove("SPG_ADMIN_PASSWORD")
            .env_remove("SPG_SYNCHRONOUS_COMMIT")
            .spawn()
            .unwrap(),
    );
    wait_for_listener(&native, &mut child.0);
    thread::sleep(Duration::from_millis(150));
    let v = flusher_iterations(&http);
    assert_eq!(
        v, 0,
        "sync-commit (the default) must not spawn the flusher; got iterations={v}"
    );
}

#[test]
fn flusher_metric_rises_under_async_commit_off() {
    // `SPG_SYNCHRONOUS_COMMIT=off` opts into async-commit mode.
    // Short interval (1 ms) makes the test deterministic without
    // needing a multi-second sleep — at 1 ms cadence, 200 ms
    // wall time should yield ≥ 50 iterations even on a busy CI
    // host. The assertion uses ">= 10" to keep the test green on
    // a heavily-loaded scheduler.
    let native = pick_free_addr();
    let http = pick_free_addr();
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_spg-server"))
            .arg(&native)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env("SPG_HTTP_ADDR", &http)
            .env("SPG_SYNCHRONOUS_COMMIT", "off")
            .env("SPG_FLUSHER_INTERVAL_US", "1000")
            .env_remove("SPG_PASSWORD")
            .env_remove("SPG_ADMIN_PASSWORD")
            .spawn()
            .unwrap(),
    );
    wait_for_listener(&native, &mut child.0);
    thread::sleep(Duration::from_millis(200));
    let v = flusher_iterations(&http);
    assert!(
        v >= 10,
        "expected flusher_iterations_total >= 10 after 200ms at 1ms cadence, got {v}"
    );
}

#[test]
fn flusher_env_var_recognizes_off_false_zero() {
    // The opt-in keyword set is {off, false, 0}. Run three
    // separate spawns to confirm each lights up the flusher.
    for val in ["off", "false", "0"] {
        let native = pick_free_addr();
        let http = pick_free_addr();
        let mut child = ChildGuard(
            Command::new(env!("CARGO_BIN_EXE_spg-server"))
                .arg(&native)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .env("SPG_HTTP_ADDR", &http)
                .env("SPG_SYNCHRONOUS_COMMIT", val)
                .env("SPG_FLUSHER_INTERVAL_US", "500")
                .env_remove("SPG_PASSWORD")
                .env_remove("SPG_ADMIN_PASSWORD")
                .spawn()
                .unwrap(),
        );
        wait_for_listener(&native, &mut child.0);
        thread::sleep(Duration::from_millis(100));
        let v = flusher_iterations(&http);
        assert!(
            v >= 5,
            "SPG_SYNCHRONOUS_COMMIT={val:?} must enable the flusher; got iterations={v}"
        );
    }
}

#[test]
fn flusher_env_var_treats_on_as_sync() {
    // The flip side of the previous test: any value that isn't
    // {off, false, 0} keeps the default sync semantic, including
    // an explicit `on`. This pins the parser so a future tweak
    // doesn't silently widen the opt-in set.
    for val in ["on", "true", "1", "yes", ""] {
        let native = pick_free_addr();
        let http = pick_free_addr();
        let mut child = ChildGuard(
            Command::new(env!("CARGO_BIN_EXE_spg-server"))
                .arg(&native)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .env("SPG_HTTP_ADDR", &http)
                .env("SPG_SYNCHRONOUS_COMMIT", val)
                .env_remove("SPG_PASSWORD")
                .env_remove("SPG_ADMIN_PASSWORD")
                .spawn()
                .unwrap(),
        );
        wait_for_listener(&native, &mut child.0);
        thread::sleep(Duration::from_millis(100));
        let v = flusher_iterations(&http);
        assert_eq!(
            v, 0,
            "SPG_SYNCHRONOUS_COMMIT={val:?} must keep sync semantics; got iterations={v}"
        );
    }
}
