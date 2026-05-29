//! v5.5.1 per-query memory budget (`SPG_MAX_QUERY_BYTES`) end-to-end:
//! - a SELECT whose result materialises past the ceiling is cancelled with a
//!   clear error instead of driving the process toward the OOM killer;
//! - a normal sub-budget query is unaffected (no false positives);
//! - the per-query reset means a long load of small INSERTs never trips, even
//!   though their cumulative bytes dwarf the ceiling.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, build_query, encode, parse_error_response};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// 1 MiB ceiling: small enough that a few thousand padded rows blow past it,
/// large enough that parsing/planning a normal query stays well under.
const BUDGET: &str = "1048576";

fn pick_free_addr() -> String {
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = probe.local_addr().unwrap();
    drop(probe);
    a.to_string()
}

fn spawn_server(addr: &str, envs: &[(&str, &str)]) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
    cmd.arg(addr).stdout(Stdio::null()).stderr(Stdio::null());
    cmd.env_remove("SPG_PASSWORD");
    cmd.env_remove("SPG_ADMIN_PASSWORD");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.spawn().unwrap()
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

fn read_frame(s: &mut TcpStream) -> Frame {
    let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
    s.read_exact(&mut header).expect("read header");
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).expect("known op");
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        s.read_exact(&mut payload).expect("read payload");
    }
    Frame { op, payload }
}

fn send(s: &mut TcpStream, f: &Frame) {
    let mut buf = Vec::new();
    encode(f, &mut buf).unwrap();
    s.write_all(&buf).unwrap();
}

/// CREATE the table and bulk-load `batches * 100` padded rows. Each INSERT
/// batch is ~40 KB — far under the ceiling — and the budget resets per query,
/// so the load never trips it even though the total is multiples of the cap.
fn create_and_load(s: &mut TcpStream, batches: usize) {
    send(
        s,
        &build_query("CREATE TABLE t (id INT NOT NULL, payload TEXT NOT NULL)"),
    );
    assert_eq!(read_frame(s).op, Op::CommandComplete);
    let pad = "x".repeat(400);
    for batch in 0..batches {
        let mut sql = String::from("INSERT INTO t VALUES ");
        for row in 0..100 {
            let id = batch * 100 + row + 1;
            if row > 0 {
                sql.push(',');
            }
            sql.push_str(&format!("({id},'{pad}')"));
        }
        send(s, &build_query(&sql));
        assert_eq!(
            read_frame(s).op,
            Op::CommandComplete,
            "load batch {batch} should commit under the per-query budget"
        );
    }
}

#[test]
fn over_budget_select_is_cancelled() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr, &[("SPG_MAX_QUERY_BYTES", BUDGET)]));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // 8000 rows × 400-char payload ≈ 3.2 MB of result data — ~3× the 1 MiB
    // ceiling, so the SELECT's materialisation crosses it with ample margin.
    create_and_load(&mut s, 80);

    send(&mut s, &build_query("SELECT * FROM t"));
    let f = read_frame(&mut s);
    assert_eq!(
        f.op,
        Op::ErrorResponse,
        "over-budget SELECT must error, not stream the whole table"
    );
    let msg = parse_error_response(&f).unwrap();
    assert!(
        msg.to_lowercase().contains("cancel"),
        "expected a cancellation error from the memory budget, got {msg:?}"
    );
}

#[test]
fn under_budget_select_succeeds() {
    let addr = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr, &[("SPG_MAX_QUERY_BYTES", BUDGET)]));
    let mut s = wait_for_listener(&addr, &mut child.0);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // 200 rows × 400 char ≈ 80 KB ≪ 1 MiB — comfortably under budget.
    create_and_load(&mut s, 2);

    send(&mut s, &build_query("SELECT * FROM t"));
    let f = read_frame(&mut s);
    assert_eq!(
        f.op,
        Op::RowDescription,
        "under-budget SELECT should stream normally, not get cancelled"
    );
}
