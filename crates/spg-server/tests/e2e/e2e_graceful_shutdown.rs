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

    // Open the conn that will be "in-flight" when SIGTERM lands. A wide
    // streaming result (about 20 MB) rather than a compute-only shape,
    // so the statement is verifiably mid-delivery when the signal
    // arrives, and stays in flight well past it.
    let mut inflight = TcpStream::connect(&addrs.native).expect("inflight connect");
    inflight.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let slow_sql = "SELECT g, repeat('y', 200) FROM generate_series(1, 100000) g";
    send(&mut inflight, &build_query(slow_sql));

    // v7.37 (round 826) — SIGTERM waits for evidence, not for a timer.
    // This used to sleep 50ms and hope the server thread had entered
    // the engine; when it had not, the drain saw a connection with no
    // statement to drain and closed it, and this side's next read blew
    // up. That is the flake the full-load suite produced twice — a
    // loaded box can hold a thread past any fixed sleep, and with the
    // sleep at zero it is 9 failures in 10 runs. The first frame
    // arriving IS the entered-the-engine event the sleep was guessing
    // at: after it, there is a statement in flight for the drain to
    // honour, by construction.
    let first = read_frame(&mut inflight);
    assert!(
        !matches!(first.op, Op::ErrorResponse | Op::Error),
        "the in-flight query errored before SIGTERM was even sent"
    );

    send_sigterm(&c.0);

    // 1. In-flight query must complete cleanly. Read frames until
    //    CommandComplete; any ErrorResponse fails the test. No frame
    //    cap: the result is deliberately many frames long, and READ_
    //    TIMEOUT already bounds a server that stops mid-result.
    loop {
        let f = read_frame(&mut inflight);
        match f.op {
            Op::CommandComplete => break,
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
                panic!("in-flight query got error during shutdown: {msg}");
            }
            // Pass-through: RowDescription / DataRow / DataRowBatch.
            _ => {}
        }
    }
    drop(inflight);

    // 2. New connections after SIGTERM must be refused. The TCP
    //    listener has stopped accepting; either the connect fails
    //    outright (ECONNREFUSED) or it succeeds at the OS layer
    //    but the server never reads the request and the socket
    //    closes when the process exits. Either is a "refusal".
    let refused = TcpStream::connect(&addrs.native).is_err();
    let drained_by_exit = !refused && {
        // OS-level accept happened (kernel SYN backlog) but the server
        // isn't reading; the child exiting kills that socket, which is
        // also a refusal. v7.37 (round 826) — this gave the child a
        // single 100ms nap to be gone, on a path whose stated budget is
        // the whole shutdown deadline: a drain still finishing at
        // 101ms failed the test with everything behaving correctly.
        // Poll to the same deadline step 3 uses instead.
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if c.0.try_wait().ok().flatten().is_some() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(20));
        }
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
