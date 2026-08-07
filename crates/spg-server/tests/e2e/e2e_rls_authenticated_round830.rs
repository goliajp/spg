//! r830 — a row-security policy binds the session that authenticated.
//!
//! Measured before this: psql, authenticated over SCRAM as a role with
//! `rolsuper = f`, reading a table with row security enabled and a
//! `USING (owner = current_user)` policy in place, got EVERY row —
//! another owner's included, in all four projection shapes. Two
//! independent reasons, one hiding the other.
//!
//! First, `is_superuser()` answered true for any session without an
//! explicit `SET ROLE`, and every RLS entry point returns early for a
//! superuser, so no policy was injected anywhere. That default was
//! deliberate: in open mode the server accepts any startup packet as
//! admin, so the `user` field is a label, and keying privilege on it
//! would let a client name itself into a role. What changed is that
//! the name is not always unverified — once a credentialed LOGIN role
//! exists the server demands a password, and that is exactly the
//! configuration where policies are supposed to bind. Open mode keeps
//! the old answer, which the first test here holds it to.
//!
//! Second — and only visible once the first was fixed — the streaming
//! executor claims a statement before policy injection happens, so it
//! read the table unfiltered. `SELECT upper(val) FROM sec` returned
//! the policy's two rows while `SELECT val FROM sec` returned all
//! three, same session, same table, differing only in whether the
//! shape gates accepted it. It now declines any table whose policies
//! bind for the session, and the fall-back path enforces.
//!
//! These tests need a real SCRAM client, which this suite's raw-
//! protocol harness is not, so what they can pin is the half that
//! needs no credential: open mode is unchanged, and the enforcement
//! path is reachable through an explicit `SET ROLE` — the same
//! `is_superuser` decision, arrived at the other way. The
//! authenticated matrix (su sees three rows, alice sees two, all four
//! shapes) is measured in the round record via psql.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(30);

struct PgMessage {
    ty: u8,
    body: Vec<u8>,
}

fn read_message(s: &mut TcpStream) -> PgMessage {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len.saturating_sub(4)];
    if !body.is_empty() {
        s.read_exact(&mut body).expect("body");
    }
    PgMessage { ty, body }
}

fn send_startup(s: &mut TcpStream) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0anyone\0\0");
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn read_until_ready(s: &mut TcpStream) -> Vec<PgMessage> {
    let mut out = Vec::new();
    loop {
        let m = read_message(s);
        let z = m.ty == b'Z';
        out.push(m);
        if z {
            return out;
        }
    }
}

fn q(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    send_query(s, sql);
    read_until_ready(s)
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s);
    let _ = read_until_ready(&mut s);
    s
}

fn error_field(msgs: &[PgMessage], want: u8) -> Option<String> {
    let m = msgs.iter().find(|m| m.ty == b'E')?;
    let mut i = 0;
    while i < m.body.len() && m.body[i] != 0 {
        let code = m.body[i];
        let start = i + 1;
        let end = start + m.body[start..].iter().position(|&b| b == 0)?;
        if code == want {
            return Some(String::from_utf8_lossy(&m.body[start..end]).into_owned());
        }
        i = end + 1;
    }
    None
}

fn col0(msgs: &[PgMessage]) -> Vec<String> {
    let mut out = Vec::new();
    for m in msgs.iter().filter(|m| m.ty == b'D') {
        let len = i32::from_be_bytes([m.body[2], m.body[3], m.body[4], m.body[5]]);
        if len < 0 {
            out.push(String::new());
            continue;
        }
        out.push(String::from_utf8_lossy(&m.body[6..6 + len as usize]).into_owned());
    }
    out
}

fn ok(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    let msgs = q(s, sql);
    assert_eq!(
        error_field(&msgs, b'C'),
        None,
        "statement failed: {sql} -> {}",
        error_field(&msgs, b'M').unwrap_or_default()
    );
    msgs
}

fn spawn(label: &str) -> (common::ChildGuard, common::ServerAddrs) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir: PathBuf = std::env::temp_dir().join(format!("spg-e2e-rlsauth-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    (common::ChildGuard(raw), addrs)
}

/// A table with row security on and a policy that admits one owner.
fn seed(s: &mut TcpStream) {
    ok(s, "CREATE TABLE sec (id INT, owner TEXT, val TEXT)");
    ok(
        s,
        "INSERT INTO sec VALUES (1,'alice','a-secret'),(2,'bob','b-secret'),(3,'alice','a2')",
    );
    ok(s, "ALTER TABLE sec ENABLE ROW LEVEL SECURITY");
    ok(s, "CREATE POLICY own ON sec USING (owner = current_user)");
}

/// The four shapes the leak was measured across. Two are claimed by
/// the streaming executor, two are declined by its shape gates and
/// materialise — which is the split that made the second defect
/// visible, so all four stay in the pin.
const SHAPES: [&str; 4] = [
    "SELECT val FROM sec",
    "SELECT sec.val FROM sec",
    "SELECT upper(val) FROM sec",
    "SELECT val FROM sec ORDER BY id",
];

#[test]
fn a_policy_subject_sees_only_its_own_rows_whichever_path_runs() {
    let (_child, addrs) = spawn("subject");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed(&mut s);
    ok(&mut s, "CREATE ROLE alice PASSWORD 'pw'");
    ok(&mut s, "GRANT SELECT ON sec TO alice");
    ok(&mut s, "SET ROLE alice");

    for sql in SHAPES {
        let got = col0(&ok(&mut s, sql));
        assert_eq!(
            got.len(),
            2,
            "`{sql}` returned {got:?} — the policy admits two of the three rows"
        );
        assert!(
            !got.iter().any(|v| v.eq_ignore_ascii_case("b-secret")),
            "`{sql}` returned another owner's row: {got:?}"
        );
    }

    // The aggregate counts what the policy admits, not what the table holds.
    assert_eq!(col0(&ok(&mut s, "SELECT count(*) FROM sec")), vec!["2"]);
}

#[test]
fn a_superuser_is_not_subject_to_the_policy() {
    let (_child, addrs) = spawn("superuser");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed(&mut s);
    ok(&mut s, "CREATE ROLE su SUPERUSER PASSWORD 'pw'");
    ok(&mut s, "SET ROLE su");

    for sql in SHAPES {
        assert_eq!(
            col0(&ok(&mut s, sql)).len(),
            3,
            "`{sql}` — a superuser bypasses row security, as in PG"
        );
    }
}

#[test]
fn open_mode_is_unchanged() {
    // No credentialed LOGIN role exists here, so the server never
    // challenges: the startup name is a label and the session is admin.
    // Keying privilege on an unverified name would let any client name
    // itself into a role, which is what the old unconditional default
    // was protecting against — and it still holds where it applies.
    let (_child, addrs) = spawn("openmode");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed(&mut s);

    for sql in SHAPES {
        assert_eq!(
            col0(&ok(&mut s, sql)).len(),
            3,
            "`{sql}` — an unauthenticated session keeps the admin default"
        );
    }
}
