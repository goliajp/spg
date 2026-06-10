//! v4.33 slow-query log — `SPG_SLOW_QUERY_LOG_MS` thresholds when
//! the server emits a JSON line on stderr.
//!
//! Test contract: with the threshold set, a query that exceeds it
//! produces exactly one `{"event":"slow_query",...}` line carrying
//! the SQL text, elapsed microseconds, role; a query that comes in
//! under threshold produces nothing.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, build_query, encode};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// v6.0.x — race-free port allocation. Pass `127.0.0.1:0` to the
/// child, parse the actual bound address from the captured stderr
/// buffer. See `tests/common/mod.rs` for the broader rationale.
fn extract_listen_addr_from_buf(buf: &Arc<Mutex<String>>) -> String {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while Instant::now() < deadline {
        let snap = buf.lock().unwrap().clone();
        if let Some(after) = snap.find("listening on ") {
            let tail = &snap[after + "listening on ".len()..];
            let end = tail.find([' ', '\n', '\r']).unwrap_or(tail.len());
            return tail[..end].to_string();
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("server didn't publish listen addr in stderr buffer");
}

fn unique_tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-slowlog-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn_server_capture_stderr(
    addr: &str,
    db: &Path,
    wal: &Path,
    env: &[(&str, String)],
) -> (Child, Arc<Mutex<String>>) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
    cmd.arg(addr)
        .arg(db)
        .arg("-")
        .arg(wal)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD")
        .env_remove("SPG_PG_ADDR");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    let stderr: ChildStderr = child.stderr.take().expect("stderr piped");
    let buf = Arc::new(Mutex::new(String::new()));
    let buf_for_thread = Arc::clone(&buf);
    thread::spawn(move || {
        let mut reader = stderr;
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if let Ok(s) = std::str::from_utf8(&chunk[..n]) {
                        buf_for_thread.lock().unwrap().push_str(s);
                    }
                }
            }
        }
    });
    (child, buf)
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
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

fn send(s: &mut TcpStream, f: &Frame) {
    let mut out = Vec::new();
    encode(f, &mut out).unwrap();
    s.write_all(&out).unwrap();
}

fn drain_to_cc(s: &mut TcpStream) {
    loop {
        let f = read_frame(s);
        match f.op {
            Op::CommandComplete => return,
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
                panic!("server returned error: {msg}");
            }
            _ => {}
        }
    }
}

/// SPG_SLOW_QUERY_LOG_MS thresholded: slow query crosses, fast
/// query stays silent. JSON line carries sql/elapsed_us/role.
#[test]
fn slow_query_log_fires_above_threshold_and_silent_below() {
    let dir = unique_tmpdir("th");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");
    // 5 ms threshold — well above a no-op `SELECT 7` (microseconds
    // in release) and well below the recursive CTE we use below
    // (tens of ms in release at N=90000). The threshold has to
    // straddle the *release-build* timings since CI runs --release.
    let (child, stderr_buf) = spawn_server_capture_stderr(
        "127.0.0.1:0",
        &db,
        &wal,
        &[("SPG_SLOW_QUERY_LOG_MS", "5".to_string())],
    );
    let mut c = ChildGuard(child);
    let addr = extract_listen_addr_from_buf(&stderr_buf);
    let mut s = TcpStream::connect(&addr).expect("connect");
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // Bootstrap (DDL — should NOT count as slow because trivial).
    send(&mut s, &build_query("CREATE TABLE t (id INT NOT NULL)"));
    drain_to_cc(&mut s);

    // Fast probe — must produce no slow-query log line.
    let fast_marker = "SELECT 7 as fast_marker_for_negative_check";
    send(&mut s, &build_query(fast_marker));
    drain_to_cc(&mut s);
    thread::sleep(Duration::from_millis(100));
    {
        let captured = stderr_buf.lock().unwrap().clone();
        assert!(
            !captured.contains(fast_marker),
            "fast query should not appear in slow-query log; stderr:\n{captured}"
        );
    }

    // Slow probe — recursive CTE that takes well above 5 ms even
    // in --release. 90 000 iterations stays under the 100 000-iter
    // runaway cap documented in PROD_READY row 6.8.
    let slow_sql = "WITH RECURSIVE seq(n) AS (\
                    SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n<90000\
                    ) SELECT count(*) FROM seq";
    send(&mut s, &build_query(slow_sql));
    drain_to_cc(&mut s);

    // Give the Drop guard a moment to flush its eprintln onto the
    // captured stderr buffer.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let captured = stderr_buf.lock().unwrap().clone();
        if captured.contains("\"event\":\"slow_query\"") && captured.contains("WITH RECURSIVE") {
            // Sanity: required fields are present and well-formed.
            assert!(
                captured.contains("\"sql\":\""),
                "missing sql field:\n{captured}"
            );
            assert!(
                captured.contains("\"elapsed_us\":"),
                "missing elapsed_us field:\n{captured}"
            );
            assert!(
                captured.contains("\"role\":\""),
                "missing role field:\n{captured}"
            );
            // Threshold echoes back so operators can correlate.
            assert!(
                captured.contains("\"threshold_us\":5000"),
                "missing/wrong threshold_us field:\n{captured}"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "slow-query log never showed up; stderr:\n{captured}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}
