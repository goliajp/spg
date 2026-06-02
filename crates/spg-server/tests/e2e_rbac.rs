#![allow(unused_mut, unused_variables)]

//! v4.1 multi-user + RBAC end-to-end.
//!
//! Boots a server with `SPG_ADMIN_PASSWORD` set (auto-creates an admin
//! on first run), restarts it, and verifies:
//! - admin survives restart via the v4.1 envelope snapshot
//! - per-user `AuthUser` works for admin
//! - wrong creds get rejected
//! - legacy `AUTH` is refused once a user table exists
//!
//! v4.1.1 adds the SQL surface (`CREATE USER`, `DROP USER`,
//! `SHOW USERS`) and the readonly / readwrite enforcement tests
//! live below the bootstrap-restart ones.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, build_auth, build_auth_user, build_query, encode, parse_error_response};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(3);

fn unique_tmpdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-rbac-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Spawn an spg-server with an OS-chosen port and return the child +
/// the actual bind address parsed from stderr. See `e2e_wal_binary`'s
/// `spawn_server_on_ephemeral_port` for the race-avoidance rationale.
fn spawn_server(db: &Path, admin_pw: Option<&str>) -> (Child, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
    cmd.arg("127.0.0.1:0")
        .arg(db)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(pw) = admin_pw {
        cmd.env("SPG_ADMIN_PASSWORD", pw);
    } else {
        cmd.env_remove("SPG_ADMIN_PASSWORD");
    }
    cmd.env_remove("SPG_PASSWORD");
    let mut child = cmd.spawn().unwrap();
    let stderr = child.stderr.take().expect("piped stderr");
    let addr = read_listening_addr(&mut child, stderr);
    (child, addr)
}

fn read_listening_addr(child: &mut Child, stderr: ChildStderr) -> String {
    let mut reader = BufReader::new(stderr);
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut line = String::new();
    while Instant::now() < deadline {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                if let Ok(Some(status)) = child.try_wait() {
                    panic!("server exited before printing listen addr: {status:?}");
                }
                thread::sleep(Duration::from_millis(20));
            }
            Ok(_) => {
                // Echo lines so existing test-output behaviour is preserved.
                eprint!("{line}");
                if let Some(addr) = extract_listen_addr(&line) {
                    thread::spawn(move || {
                        // Drain the rest of stderr to the test's stderr so
                        // server logs after startup still surface on
                        // failure-path debugging.
                        let mut buf = String::new();
                        while let Ok(n) = reader.read_line(&mut buf) {
                            if n == 0 {
                                break;
                            }
                            eprint!("{buf}");
                            buf.clear();
                        }
                    });
                    return addr;
                }
            }
            Err(e) => panic!("read stderr: {e}"),
        }
    }
    let _ = child.kill();
    panic!("server didn't print listen addr within {STARTUP_TIMEOUT:?}");
}

fn extract_listen_addr(line: &str) -> Option<String> {
    let after = line.find("listening on ")?;
    let tail = &line[after + "listening on ".len()..];
    let end = tail.find([' ', '\n', '\r']).unwrap_or(tail.len());
    Some(tail[..end].to_string())
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Connect to the already-bound server address. The listener exists
/// (stderr confirmed it) but the OS may need a tick to `accept(2)`.
fn connect_to(addr: &str) -> TcpStream {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match TcpStream::connect(addr) {
            Ok(s) => return s,
            Err(e) => {
                assert!(Instant::now() < deadline, "connect {addr}: {e}");
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Read exactly one frame from `s`. The previous version of this
/// helper allocated a fresh accumulator buffer per call, so any
/// extra bytes the server flushed alongside the frame (common: a
/// single `write_all` carrying `RD` + `DataRow` + `CC` for a SELECT)
/// were silently discarded — the next `read_frame` call would block on
/// data the server already sent. Reading the fixed header + exact
/// payload length avoids the over-read entirely.
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

#[test]
fn admin_bootstrap_survives_restart_and_authuser_works() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");

    // First boot: SPG_ADMIN_PASSWORD creates admin.
    {
        let (raw_child, addr) = spawn_server(&db, Some("hunter2"));
        let _child = ChildGuard(raw_child);
        let mut s = connect_to(&addr);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

        // Unauthed query rejected.
        send(&mut s, &build_query("SELECT 1"));
        assert_eq!(read_frame(&mut s).op, Op::ErrorResponse);

        // Wrong creds rejected.
        send(&mut s, &build_auth_user("admin", "wrong").unwrap());
        let bad = read_frame(&mut s);
        assert_eq!(bad.op, Op::ErrorResponse);
        let msg = parse_error_response(&bad).unwrap();
        assert!(msg.contains("invalid"), "got {msg:?}");

        // Correct AuthUser succeeds.
        send(&mut s, &build_auth_user("admin", "hunter2").unwrap());
        assert_eq!(read_frame(&mut s).op, Op::Pong);

        // Admin can write.
        send(&mut s, &build_query("CREATE TABLE t (id INT NOT NULL)"));
        assert_eq!(read_frame(&mut s).op, Op::CommandComplete);
    }

    // Second boot: snapshot must carry the admin user across restart.
    // Re-spawn without SPG_ADMIN_PASSWORD to prove it's not re-creating
    // — restoration came from the on-disk envelope.
    let (raw_child, addr2) = spawn_server(&db, None);
    let _child = ChildGuard(raw_child);
    let mut s = connect_to(&addr2);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // Connection still gated on auth (user table is non-empty).
    send(&mut s, &build_query("SELECT 1"));
    assert_eq!(read_frame(&mut s).op, Op::ErrorResponse);

    // Original admin password still works after restart.
    send(&mut s, &build_auth_user("admin", "hunter2").unwrap());
    assert_eq!(read_frame(&mut s).op, Op::Pong);
}

#[test]
fn legacy_auth_op_rejected_once_user_table_exists() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");

    let (raw_child, addr) = spawn_server(&db, Some("admin-pw"));
    let _child = ChildGuard(raw_child);
    let mut s = connect_to(&addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // RBAC is active because bootstrap created the admin. Legacy
    // single-password AUTH should be refused even with a value that
    // would otherwise have worked under SPG_PASSWORD mode.
    send(&mut s, &build_auth("admin-pw"));
    let f = read_frame(&mut s);
    assert_eq!(f.op, Op::ErrorResponse);
    let msg = parse_error_response(&f).unwrap();
    assert!(msg.contains("RBAC"), "expected RBAC hint, got {msg:?}");
}

fn login_admin(addr: &str, pw: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send(&mut s, &build_auth_user("admin", pw).unwrap());
    assert_eq!(read_frame(&mut s).op, Op::Pong);
    s
}

fn login_user(addr: &str, user: &str, pw: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send(&mut s, &build_auth_user(user, pw).unwrap());
    assert_eq!(read_frame(&mut s).op, Op::Pong);
    s
}

fn drain_until_complete(s: &mut TcpStream) -> Vec<Frame> {
    let mut out = Vec::new();
    loop {
        let f = read_frame(s);
        let done = matches!(f.op, Op::CommandComplete | Op::ErrorResponse);
        out.push(f);
        if done {
            return out;
        }
    }
}

#[test]
fn readonly_user_cannot_write_but_can_select() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw_child, addr) = spawn_server(&db, Some("admin-pw"));
    let _child = ChildGuard(raw_child);
    // Drain accept(2) latency before tests call login_admin.
    drop(connect_to(&addr));

    // Admin sets up a table + a readonly user.
    {
        let mut s = login_admin(&addr, "admin-pw");
        send(&mut s, &build_query("CREATE TABLE t (id INT NOT NULL)"));
        assert_eq!(read_frame(&mut s).op, Op::CommandComplete);
        send(&mut s, &build_query("INSERT INTO t VALUES (1)"));
        assert_eq!(read_frame(&mut s).op, Op::CommandComplete);
        send(
            &mut s,
            &build_query("CREATE USER 'bi' WITH PASSWORD 'bi-pw' ROLE 'readonly'"),
        );
        assert_eq!(read_frame(&mut s).op, Op::CommandComplete);
    }

    // Readonly user can SELECT.
    let mut s = login_user(&addr, "bi", "bi-pw");
    send(&mut s, &build_query("SELECT * FROM t"));
    let frames = drain_until_complete(&mut s);
    assert!(
        frames.iter().any(|f| f.op == Op::RowDescription),
        "expected RowDescription, got {:?}",
        frames.iter().map(|f| f.op).collect::<Vec<_>>()
    );

    // ...but cannot INSERT.
    send(&mut s, &build_query("INSERT INTO t VALUES (2)"));
    let f = read_frame(&mut s);
    assert_eq!(f.op, Op::ErrorResponse);
    let msg = parse_error_response(&f).unwrap();
    assert!(
        msg.contains("permission denied"),
        "expected permission error, got {msg:?}"
    );

    // ...and cannot CREATE USER (admin-only DDL).
    send(
        &mut s,
        &build_query("CREATE USER 'evil' WITH PASSWORD 'pwn' ROLE 'admin'"),
    );
    let f = read_frame(&mut s);
    assert_eq!(f.op, Op::ErrorResponse);
    let msg = parse_error_response(&f).unwrap();
    assert!(
        msg.contains("admin"),
        "expected admin-only error, got {msg:?}"
    );
}

#[test]
fn readwrite_user_can_write_but_not_manage_users() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw_child, addr) = spawn_server(&db, Some("admin-pw"));
    let _child = ChildGuard(raw_child);
    // Drain accept(2) latency before tests call login_admin.
    drop(connect_to(&addr));

    {
        let mut s = login_admin(&addr, "admin-pw");
        send(&mut s, &build_query("CREATE TABLE t (id INT NOT NULL)"));
        assert_eq!(read_frame(&mut s).op, Op::CommandComplete);
        send(
            &mut s,
            &build_query("CREATE USER 'app' WITH PASSWORD 'app-pw' ROLE 'readwrite'"),
        );
        assert_eq!(read_frame(&mut s).op, Op::CommandComplete);
    }

    let mut s = login_user(&addr, "app", "app-pw");
    send(&mut s, &build_query("INSERT INTO t VALUES (42)"));
    assert_eq!(read_frame(&mut s).op, Op::CommandComplete);

    // ReadWrite is NOT allowed to manage users.
    send(&mut s, &build_query("DROP USER 'app'"));
    let f = read_frame(&mut s);
    assert_eq!(f.op, Op::ErrorResponse);
    let msg = parse_error_response(&f).unwrap();
    assert!(msg.contains("admin"), "got {msg:?}");
}

#[test]
fn show_users_lists_admin_plus_created_users() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw_child, addr) = spawn_server(&db, Some("admin-pw"));
    let _child = ChildGuard(raw_child);
    // Drain accept(2) latency before tests call login_admin.
    drop(connect_to(&addr));

    let mut s = login_admin(&addr, "admin-pw");
    send(
        &mut s,
        &build_query("CREATE USER 'alice' WITH PASSWORD 'p' ROLE 'readonly'"),
    );
    assert_eq!(read_frame(&mut s).op, Op::CommandComplete);

    send(&mut s, &build_query("SHOW USERS"));
    let frames = drain_until_complete(&mut s);
    assert_eq!(frames[0].op, Op::RowDescription);
    let row_count = frames
        .iter()
        .filter(|f| matches!(f.op, Op::DataRow | Op::DataRowBatch))
        .map(|f| match f.op {
            Op::DataRow => 1,
            Op::DataRowBatch => spg_wire::parse_data_row_batch(f).unwrap().len(),
            _ => 0,
        })
        .sum::<usize>();
    assert_eq!(row_count, 2, "expected admin + alice, got {row_count} rows");
}

#[test]
fn drop_user_revokes_access() {
    let dir = unique_tmpdir();
    let db = dir.join("spg.db");
    let (raw_child, addr) = spawn_server(&db, Some("admin-pw"));
    let _child = ChildGuard(raw_child);
    // Drain accept(2) latency before tests call login_admin.
    drop(connect_to(&addr));

    {
        let mut s = login_admin(&addr, "admin-pw");
        send(
            &mut s,
            &build_query("CREATE USER 'temp' WITH PASSWORD 'tp' ROLE 'readwrite'"),
        );
        assert_eq!(read_frame(&mut s).op, Op::CommandComplete);
        send(&mut s, &build_query("DROP USER 'temp'"));
        assert_eq!(read_frame(&mut s).op, Op::CommandComplete);
    }

    // The dropped user can no longer authenticate.
    let mut s = TcpStream::connect(&addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send(&mut s, &build_auth_user("temp", "tp").unwrap());
    let f = read_frame(&mut s);
    assert_eq!(f.op, Op::ErrorResponse);
}
