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

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

fn local_spawn(envs: &[(&str, &str)]) -> (std::process::Child, common::ServerAddrs) {
    let mut b = common::ServerBuilder::new().with_http();
    for (k, v) in envs {
        b = b.env(*k, *v);
    }
    b.spawn()
}

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(3);

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
    metric_u64(http, "spg_flusher_iterations_total")
}

fn metric_u64(http: &str, name: &str) -> u64 {
    let body = http_get_body(http);
    body.lines()
        .find_map(|l| l.strip_prefix(name).and_then(|tail| tail.strip_prefix(' ')))
        .map_or(0, |s| s.trim().parse::<u64>().unwrap_or(0))
}

fn metric_f64(http: &str, name: &str) -> f64 {
    let body = http_get_body(http);
    body.lines()
        .find_map(|l| l.strip_prefix(name).and_then(|tail| tail.strip_prefix(' ')))
        .map_or(0.0, |s| s.trim().parse::<f64>().unwrap_or(0.0))
}

#[test]
fn flusher_metric_zero_in_default_sync_commit_mode() {
    // Default mode: SPG_SYNCHRONOUS_COMMIT unset → sync semantics
    // → flusher thread is not spawned. After 150 ms of liveness
    // the counter must still read 0; non-zero would mean a spurious
    // spawn that breaks the v5.4 contract (sync mode preserves
    // every v4.42 durability invariant exactly).
    let (raw, addrs) = local_spawn(&[]);
    let mut child = common::ChildGuard(raw);
    thread::sleep(Duration::from_millis(150));
    let v = flusher_iterations(addrs.http.as_ref().unwrap());
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
    let (raw, addrs) = local_spawn(&[
        ("SPG_SYNCHRONOUS_COMMIT", "off"),
        ("SPG_FLUSHER_INTERVAL_US", "1000"),
    ]);
    let mut child = common::ChildGuard(raw);
    // v7.37 (round 827) — poll the counter to the floor instead of
    // giving it 200ms and hoping the scheduler ran the flusher enough.
    let mut v = 0;
    common::wait_until(Duration::from_secs(5), || {
        v = flusher_iterations(addrs.http.as_ref().unwrap());
        v >= 10
    });
    assert!(
        v >= 10,
        "expected flusher_iterations_total >= 10 at 1ms cadence, got {v}"
    );
}

#[test]
fn flusher_env_var_recognizes_off_false_zero() {
    // The opt-in keyword set is {off, false, 0}. Run three
    // separate spawns to confirm each lights up the flusher.
    for val in ["off", "false", "0"] {
        let (raw, addrs) = local_spawn(&[
            ("SPG_SYNCHRONOUS_COMMIT", val),
            ("SPG_FLUSHER_INTERVAL_US", "500"),
        ]);
        let mut child = common::ChildGuard(raw);
        // Same conversion as above: poll to the floor.
        let mut v = 0;
        common::wait_until(Duration::from_secs(5), || {
            v = flusher_iterations(addrs.http.as_ref().unwrap());
            v >= 5
        });
        assert!(
            v >= 5,
            "SPG_SYNCHRONOUS_COMMIT={val:?} must enable the flusher; got iterations={v}"
        );
    }
}

#[test]
fn durability_lag_metrics_are_zero_in_sync_mode() {
    // v5.4.3 — `spg_durability_lag_bytes` and
    // `spg_durability_lag_seconds` must both report 0 in sync-
    // commit mode (the default). The flusher isn't spawned;
    // every write is fsynced before the client ack. Render-time
    // logic short-circuits to 0 instead of leaking
    // `last_durable_wal_offset = 0` against a growing WAL.
    let (raw, addrs) = local_spawn(&[]);
    let mut child = common::ChildGuard(raw);
    thread::sleep(Duration::from_millis(100));
    let lag_bytes = metric_u64(addrs.http.as_ref().unwrap(), "spg_durability_lag_bytes");
    let lag_seconds = metric_f64(addrs.http.as_ref().unwrap(), "spg_durability_lag_seconds");
    assert_eq!(lag_bytes, 0, "sync mode must report 0 lag bytes");
    assert!(
        lag_seconds == 0.0,
        "sync mode must report 0 lag seconds, got {lag_seconds}"
    );
}

#[test]
fn durability_lag_seconds_bounded_in_async_mode() {
    // v5.4.3 — under async-commit with a 1 ms cadence, the
    // flusher's most recent `sync_data` is at most a few
    // milliseconds old when `/metrics` is scraped. Allow 1 s of
    // headroom for CI scheduler jitter; tighter than that risks
    // flakes, looser than that defeats the gate's purpose.
    let (raw, addrs) = local_spawn(&[
        ("SPG_SYNCHRONOUS_COMMIT", "off"),
        ("SPG_FLUSHER_INTERVAL_US", "1000"),
    ]);
    let mut child = common::ChildGuard(raw);
    // Wait long enough for at least one flusher tick to land.
    thread::sleep(Duration::from_millis(50));
    let lag_seconds = metric_f64(addrs.http.as_ref().unwrap(), "spg_durability_lag_seconds");
    assert!(
        lag_seconds < 1.0,
        "async-commit lag_seconds should be < 1 s under 1 ms cadence, got {lag_seconds}"
    );
    // Counter-positive sanity: the flusher must have run at
    // least once, otherwise lag_seconds being small is
    // misleading.
    let iters = metric_u64(addrs.http.as_ref().unwrap(), "spg_flusher_iterations_total");
    assert!(
        iters >= 1,
        "expected flusher to have ticked at least once before scrape, got {iters}"
    );
}

#[test]
fn flusher_env_var_treats_on_as_sync() {
    // The flip side of the previous test: any value that isn't
    // {off, false, 0} keeps the default sync semantic, including
    // an explicit `on`. This pins the parser so a future tweak
    // doesn't silently widen the opt-in set.
    for val in ["on", "true", "1", "yes", ""] {
        let (raw, addrs) = local_spawn(&[("SPG_SYNCHRONOUS_COMMIT", val)]);
        let mut child = common::ChildGuard(raw);
        thread::sleep(Duration::from_millis(100));
        let v = flusher_iterations(addrs.http.as_ref().unwrap());
        assert_eq!(
            v, 0,
            "SPG_SYNCHRONOUS_COMMIT={val:?} must keep sync semantics; got iterations={v}"
        );
    }
}
