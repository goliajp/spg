//! v4.1 multi-user + RBAC end-to-end.
//!
//! Boots a server with `SPG_ADMIN_PASSWORD` set (auto-creates an admin
//! on first run), restarts it, and verifies:
//! - admin survives restart via the v4.1 envelope snapshot
//! - per-user `AuthUser` works for admin
//! - wrong creds get rejected
//! - legacy `AUTH` is refused once a user table exists
//!
//! `CREATE USER` / `DROP USER` SQL lands in v4.1.1, so the readonly
//! / readwrite role enforcement gets its own e2e test then.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{
    Frame, FrameError, Op, build_auth, build_auth_user, build_query, decode, encode,
    parse_error_response,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(3);

fn pick_free_addr() -> String {
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = probe.local_addr().unwrap();
    drop(probe);
    a.to_string()
}

fn unique_tmpdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-rbac-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn_server(addr: &str, db: &PathBuf, admin_pw: Option<&str>) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spg-server"));
    cmd.arg(addr)
        .arg(db)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(pw) = admin_pw {
        cmd.env("SPG_ADMIN_PASSWORD", pw);
    } else {
        cmd.env_remove("SPG_ADMIN_PASSWORD");
    }
    cmd.env_remove("SPG_PASSWORD");
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
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        match decode(&buf) {
            Ok((f, _)) => return f,
            Err(FrameError::ShortHeader | FrameError::ShortPayload) => {
                let n = s.read(&mut chunk).unwrap();
                assert!(n > 0, "server closed mid-frame");
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(e) => panic!("decode: {e}"),
        }
    }
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
    let addr = pick_free_addr();

    // First boot: SPG_ADMIN_PASSWORD creates admin.
    {
        let mut child = ChildGuard(spawn_server(&addr, &db, Some("hunter2")));
        let mut s = wait_for_listener(&addr, &mut child.0);
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
    let addr2 = pick_free_addr();
    let mut child = ChildGuard(spawn_server(&addr2, &db, None));
    let mut s = wait_for_listener(&addr2, &mut child.0);
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
    let addr = pick_free_addr();

    let mut child = ChildGuard(spawn_server(&addr, &db, Some("admin-pw")));
    let mut s = wait_for_listener(&addr, &mut child.0);
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
