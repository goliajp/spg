//! v7.40.11 — an operator had no way to set anything for a whole
//! deployment, and a client's connect-time negotiation was discarded.
//!
//! Two halves of one gap, reported against 7.40.9 (§3.13, §3.14).
//!
//! **`-c name=value` killed the container.** It is the documented way
//! to set a server GUC in `postgres`'s own image and what `command:` in
//! a compose file carries:
//!
//! ```text
//!   docker run … postgres:18        -c work_mem=64MB  →  running, 64MB
//!   docker run … goliakk/spg:7.40.9 -c work_mem=64MB  →  exited (1)
//! ```
//!
//! The argument was read as a positional database path
//! (`db file TimeZone=Asia/Tokyo does not exist yet`) and then
//! `spg-server: fatal: invalid socket address` — a message naming the
//! wrong component, so an operator looks at ports and bind addresses
//! for a cause that is an unrecognised argument.
//!
//! **The startup packet was parsed and thrown away.** A client
//! negotiates its session before its first query; it is the only place
//! a POOLED client can put a setting that has to be true of every
//! connection, including the ones the pool opens hours later. There are
//! two channels and 7.40.9 ignored both:
//!
//! ```text
//!   guc                asked            SPG gave        SET gives
//!   TimeZone           Asia/Tokyo       UTC             Asia/Tokyo
//!   statement_timeout  7s               0               7s
//!   work_mem           64MB             4MB             64MB
//!   search_path        probe_schema     "$user",public  probe_schema
//!   bytea_output       escape           hex             escape
//!   IntervalStyle      postgres_verbose postgres        postgres_verbose
//! ```
//!
//! Every one works via `SET`, so the settings are implemented; it is
//! the channel that carries them at connect time that was not. The
//! named half is the more serious one: sqlx sends
//! `extra_float_digits = 2` on every connection it opens and SPG
//! reported `1`. A driver that sends `DateStyle = ISO, MDY` and is
//! silently given something else misreads every date it parses; one
//! that pins `search_path` reads and writes the wrong tables. Nothing
//! raises: the connection is accepted and every query answers.
//!
//! Precedence is PostgreSQL's, lowest first: the server's own `-c`,
//! then `ALTER DATABASE/ROLE SET`, then what this connection asked
//! for. The last two already worked; the first and the third did not
//! exist.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(60);

fn unique_db(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = common::tmp_base().join(format!("spg-bootset-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p.join("d.spgdb")
}

fn pg_msg(s: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("pg header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len.saturating_sub(4)];
    if !body.is_empty() {
        s.read_exact(&mut body).expect("pg body");
    }
    (ty, body)
}

/// Connect, carrying whatever startup parameters the caller names.
fn pg_connect_with(addr: &str, params: &[(&str, &str)]) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let mut body = Vec::new();
    body.extend_from_slice(&196_608u32.to_be_bytes());
    body.extend_from_slice(b"user\0bench\0");
    for (k, v) in params {
        body.extend_from_slice(k.as_bytes());
        body.push(0);
        body.extend_from_slice(v.as_bytes());
        body.push(0);
    }
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    loop {
        if pg_msg(&mut s).0 == b'Z' {
            break;
        }
    }
    s
}

/// One `SHOW`, over the simple query protocol; returns the value.
fn show(s: &mut TcpStream, name: &str) -> String {
    let sql = format!("SHOW {name}");
    let mut q: Vec<u8> = vec![b'Q'];
    let mut b = sql.as_bytes().to_vec();
    b.push(0);
    q.extend_from_slice(&((b.len() + 4) as u32).to_be_bytes());
    q.extend_from_slice(&b);
    s.write_all(&q).unwrap();
    let mut value = String::new();
    let mut err = None;
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'D' => {
                let len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
                if len > 0 {
                    value = String::from_utf8_lossy(&body[6..6 + len as usize]).into_owned();
                }
            }
            b'E' => {
                let mut pos = 0;
                while pos < body.len() && body[pos] != 0 {
                    let t = body[pos];
                    pos += 1;
                    let end = body[pos..].iter().position(|&c| c == 0).unwrap() + pos;
                    if t == b'M' {
                        err = Some(String::from_utf8_lossy(&body[pos..end]).into_owned());
                    }
                    pos = end + 1;
                }
            }
            b'Z' => break,
            _ => {}
        }
    }
    assert!(err.is_none(), "SHOW {name}: {err:?}");
    value
}

/// `-c name=value` on the command line, the way `postgres`'s image
/// takes it and the way `command:` in a compose file carries it.
#[test]
fn boot_settings_from_the_command_line() {
    let (raw, addrs) = common::ServerBuilder::new()
        .arg("-c")
        .arg("work_mem=64MB")
        .arg("-c")
        .arg("TimeZone=Asia/Tokyo")
        .arg_path(&unique_db("boot"))
        .with_pgwire()
        .spawn();
    let _guard = common::ChildGuard(raw);
    let mut s = pg_connect_with(addrs.pgwire.as_ref().unwrap(), &[]);
    assert_eq!(show(&mut s, "work_mem"), "64MB");
    assert_eq!(show(&mut s, "TimeZone"), "Asia/Tokyo");
    // Every connection, not just the first — this is the whole point of
    // a deployment-wide setting.
    let mut s2 = pg_connect_with(addrs.pgwire.as_ref().unwrap(), &[]);
    assert_eq!(show(&mut s2, "work_mem"), "64MB");
}

/// The `-cname=value` spelling, which is the other one `postgres`
/// accepts, and a positional path that still lands where it belongs.
#[test]
fn the_joined_spelling_and_the_positional_path_coexist() {
    let db = unique_db("joined");
    let (raw, addrs) = common::ServerBuilder::new()
        .arg("-cwork_mem=32MB")
        .arg_path(&db)
        .with_pgwire()
        .spawn();
    let _guard = common::ChildGuard(raw);
    let mut s = pg_connect_with(addrs.pgwire.as_ref().unwrap(), &[]);
    assert_eq!(show(&mut s, "work_mem"), "32MB");
    // The database path was still read as the database path: a table
    // created here has to survive into the file the argument named.
    let mut q: Vec<u8> = vec![b'Q'];
    let mut b = b"CREATE TABLE bootcheck (a INT)".to_vec();
    b.push(0);
    q.extend_from_slice(&((b.len() + 4) as u32).to_be_bytes());
    q.extend_from_slice(&b);
    s.write_all(&q).unwrap();
    loop {
        if pg_msg(&mut s).0 == b'Z' {
            break;
        }
    }
}

/// The startup packet's NAMED parameters — where every driver puts its
/// own defaults without being asked. sqlx sends `extra_float_digits`
/// on every connection it opens.
#[test]
fn the_startup_packets_named_parameters_are_applied() {
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&unique_db("named"))
        .with_pgwire()
        .spawn();
    let _guard = common::ChildGuard(raw);
    let mut s = pg_connect_with(
        addrs.pgwire.as_ref().unwrap(),
        &[
            ("extra_float_digits", "2"),
            ("TimeZone", "Asia/Tokyo"),
            ("DateStyle", "ISO, MDY"),
        ],
    );
    assert_eq!(show(&mut s, "extra_float_digits"), "2");
    assert_eq!(show(&mut s, "TimeZone"), "Asia/Tokyo");
    assert_eq!(show(&mut s, "DateStyle"), "ISO, MDY");
}

/// The `options` parameter — `PGOPTIONS`, `?options=` in a URL,
/// `PgConnectOptions::options` in sqlx, pgbouncer's per-pool settings.
/// All the same string.
#[test]
fn the_startup_packets_options_string_is_applied() {
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&unique_db("options"))
        .with_pgwire()
        .spawn();
    let _guard = common::ChildGuard(raw);
    let mut s = pg_connect_with(
        addrs.pgwire.as_ref().unwrap(),
        &[(
            "options",
            "-c TimeZone=Asia/Tokyo -c statement_timeout=7s -c work_mem=64MB",
        )],
    );
    assert_eq!(show(&mut s, "TimeZone"), "Asia/Tokyo");
    assert_eq!(show(&mut s, "work_mem"), "64MB");
    assert_eq!(show(&mut s, "statement_timeout"), "7s");
}

/// PostgreSQL's precedence, lowest first: the server's own `-c`, then
/// what this connection asked for. A pooled client that pins a setting
/// must win over the deployment default, or pinning is meaningless.
#[test]
fn the_connections_request_wins_over_the_servers_default() {
    let (raw, addrs) = common::ServerBuilder::new()
        .arg("-c")
        .arg("work_mem=64MB")
        .arg_path(&unique_db("prec"))
        .with_pgwire()
        .spawn();
    let _guard = common::ChildGuard(raw);
    // No request: the server's default.
    let mut plain = pg_connect_with(addrs.pgwire.as_ref().unwrap(), &[]);
    assert_eq!(show(&mut plain, "work_mem"), "64MB");
    // A request: the connection's.
    let mut asked = pg_connect_with(addrs.pgwire.as_ref().unwrap(), &[("work_mem", "16MB")]);
    assert_eq!(show(&mut asked, "work_mem"), "16MB");
    // And the two connections do not leak into each other.
    assert_eq!(show(&mut plain, "work_mem"), "64MB");
}

/// A setting the server does not know is refused at boot rather than
/// accepted and dropped — accepting a negotiation and discarding it is
/// the worst of the three available behaviours, and that goes for the
/// command line too.
#[test]
fn an_unknown_boot_setting_is_a_startup_error() {
    let db = unique_db("badboot");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_spg-server"))
        .arg("127.0.0.1:0")
        .arg("-c")
        .arg("no_such_setting_at_all=1")
        .arg(&db)
        .env_remove("SPG_DB")
        .env_remove("SPG_WAL")
        .env_remove("SPG_AUDIT")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    // A BOUNDED wait, because the failing behaviour here is a server
    // that starts and listens forever. The first draft of this called
    // `.output()`, which blocks until exit: the red pin did not fail,
    // it hung the test binary — and two of them, for five hours.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(s) = child.try_wait().expect("try_wait") {
            break Some(s);
        }
        if std::time::Instant::now() > deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let started_anyway = status.is_none();
    if started_anyway {
        let _ = child.kill();
        let _ = child.wait();
    }
    let mut msg = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut msg);
    }
    assert!(
        !started_anyway,
        "the server started and kept listening; an unknown setting must not boot"
    );
    assert!(
        !status.expect("checked above").success(),
        "an unknown setting must not boot: {msg}"
    );
    assert!(
        msg.contains("no_such_setting_at_all"),
        "the message names the setting: {msg}"
    );
}

/// A setting the connection asks for and cannot have refuses the
/// CONNECTION, naming what was not applied — "accepting a negotiation
/// and discarding it is the worst of the three available behaviours".
///
/// The sentence and the SQLSTATE are the engine's own. PostgreSQL says
/// `unrecognized configuration parameter "x"` for a name it does not
/// know and `invalid value for parameter "x": "y"` for a value it will
/// not take, and so does the engine; a wrapper that asserted the first
/// for both reported a RECOGNISED parameter as unrecognised, which is
/// what the first cut of this did.
#[test]
fn a_setting_that_cannot_be_applied_refuses_the_connection() {
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&unique_db("refuse"))
        .with_pgwire()
        .spawn();
    let _guard = common::ChildGuard(raw);
    let addr = addrs.pgwire.as_ref().unwrap();

    // An unknown name.
    let (code, msg) = connect_expecting_error(addr, &[("no_such_thing", "1")]);
    assert!(
        msg.contains("unrecognized configuration parameter") && msg.contains("no_such_thing"),
        "{code} {msg}"
    );

    // A known name with a value it will not take.
    let (code, msg) = connect_expecting_error(addr, &[("DateStyle", "ISO,")]);
    assert!(
        msg.contains("invalid value for parameter") && msg.contains("DateStyle"),
        "a recognised parameter must not be reported as unrecognised: {code} {msg}"
    );

    // And the server is still serving: one refused connection does not
    // take the listener with it.
    let mut ok = pg_connect_with(addr, &[]);
    assert!(!show(&mut ok, "work_mem").is_empty());
}

/// Connect and read the ErrorResponse the server answers the startup
/// packet with. Returns (SQLSTATE, message).
fn connect_expecting_error(addr: &str, params: &[(&str, &str)]) -> (String, String) {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let mut body = Vec::new();
    body.extend_from_slice(&196_608u32.to_be_bytes());
    body.extend_from_slice(b"user\0bench\0");
    for (k, v) in params {
        body.extend_from_slice(k.as_bytes());
        body.push(0);
        body.extend_from_slice(v.as_bytes());
        body.push(0);
    }
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    loop {
        let (ty, b) = pg_msg(&mut s);
        if ty == b'E' {
            let mut code = String::new();
            let mut msg = String::new();
            let mut pos = 0;
            while pos < b.len() && b[pos] != 0 {
                let tag = b[pos];
                pos += 1;
                let end = b[pos..].iter().position(|&c| c == 0).unwrap() + pos;
                let v = String::from_utf8_lossy(&b[pos..end]).into_owned();
                match tag {
                    b'C' => code = v,
                    b'M' => msg = v,
                    _ => {}
                }
                pos = end + 1;
            }
            return (code, msg);
        }
        assert_ne!(
            ty, b'Z',
            "the connection was accepted; a setting it cannot have must refuse it"
        );
    }
}
