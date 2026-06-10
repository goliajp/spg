//! v4.33 graceful shutdown — SIGTERM/SIGINT drains in-flight
//! connections up to `SPG_SHUTDOWN_DEADLINE_SEC`, refuses new
//! ones in the meantime, and exits 0.
//!
//! The test sends SIGTERM via `libc::kill` (vs `Child::kill` which
//! issues SIGKILL and would bypass the drain path entirely).

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, build_query, encode};

use std::process::Child;

use std::thread;

fn local_spawn(
    db: &std::path::Path,
    wal: &std::path::Path,
    env: &[(&str, String)],
) -> (std::process::Child, common::ServerAddrs) {
    let mut b = common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal);
    for (k, v) in env {
        b = b.env(*k, v);
    }
    b.spawn()
}

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-shutdown-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
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

fn exec_ok(s: &mut TcpStream, sql: &str) {
    send(s, &build_query(sql));
    let f = read_frame(s);
    if f.op == Op::ErrorResponse {
        let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
        panic!("server rejected SQL {sql:?}: {msg}");
    }
    assert_eq!(f.op, Op::CommandComplete, "expected CC for {sql:?}");
}

#[allow(unsafe_code)]
fn send_sigterm(child: &Child) {
    // SAFETY: `kill(2)` with a valid pid + signal number is the
    // standard POSIX termination primitive. The child is alive
    // (caller asserts this); SIGTERM is the systemd-style stop
    // signal that the server installs a handler for.
    let pid = i32::try_from(child.id()).expect("pid fits in i32");
    let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
    assert_eq!(
        rc,
        0,
        "kill(SIGTERM) failed: {}",
        std::io::Error::last_os_error()
    );
}

/// SIGTERM → in-flight query completes, new connections refused
/// during/after the drain, child exits with status 0 within the
/// shutdown deadline.
#[test]
fn graceful_shutdown_drains_inflight_and_refuses_new_conns_and_exits_zero() {
    let dir = unique_tmpdir("drain");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");
    let (raw, addrs) = local_spawn(
        &db,
        &wal,
        // Generous drain budget; test asserts the child exits well
        // before the budget so we know the drain logic — not the
        // deadline — released the process.
        &[("SPG_SHUTDOWN_DEADLINE_SEC", "10".to_string())],
    );
    let mut c = common::ChildGuard(raw);
    let mut bootstrap = common::connect_to(&addrs.native);
    bootstrap.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(&mut bootstrap, "CREATE TABLE g (id INT NOT NULL)");
    drop(bootstrap);

    // Open the conn that will be "in-flight" when SIGTERM lands.
    // The query is a recursive CTE chosen to take ~100-500ms so
    // SIGTERM has a real window to interrupt; it's also CPU-bound
    // so a non-trivial chunk of work is genuinely mid-flight.
    let mut inflight = TcpStream::connect(&addrs.native).expect("inflight connect");
    inflight.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let slow_sql = "WITH RECURSIVE seq(n) AS (\
                    SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n<20000\
                    ) SELECT count(*) FROM seq";
    send(&mut inflight, &build_query(slow_sql));
    // Yield briefly so the server thread has actually entered the
    // engine before SIGTERM lands.
    thread::sleep(Duration::from_millis(50));

    send_sigterm(&c.0);

    // 1. In-flight query must complete cleanly. Read frames until
    //    CommandComplete; any ErrorResponse fails the test.
    let mut saw_cc = false;
    for _ in 0..16 {
        let f = read_frame(&mut inflight);
        match f.op {
            Op::CommandComplete => {
                saw_cc = true;
                break;
            }
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
                panic!("in-flight query got error during shutdown: {msg}");
            }
            // Pass-through: RowDescription / DataRow / DataRowBatch.
            _ => {}
        }
    }
    assert!(saw_cc, "in-flight query never returned CC");
    drop(inflight);

    // 2. New connections after SIGTERM must be refused. The TCP
    //    listener has stopped accepting; either the connect fails
    //    outright (ECONNREFUSED) or it succeeds at the OS layer
    //    but the server never reads the request and the socket
    //    closes when the process exits. Either is a "refusal".
    let refused = TcpStream::connect(&addrs.native).is_err();
    let drained_by_exit = !refused && {
        // OS-level accept happened (kernel SYN backlog) but the
        // server isn't reading. Wait briefly; if the child exits
        // first the socket dies, which is also acceptable.
        thread::sleep(Duration::from_millis(100));
        c.0.try_wait().ok().flatten().is_some()
    };
    assert!(
        refused || drained_by_exit,
        "new connection wasn't refused and child still running"
    );

    // 3. Child exits with status 0, well within the 10s budget.
    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(s) = c.0.try_wait().expect("try_wait") {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "child did not exit within budget"
        );
        thread::sleep(Duration::from_millis(50));
    };
    assert!(
        status.success(),
        "expected exit 0 after graceful shutdown, got {status:?}"
    );
}
