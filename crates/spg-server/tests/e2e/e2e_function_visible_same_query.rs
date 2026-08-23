//! v7.38.19 — a function created earlier in the SAME query string was
//! invisible to the statements after it.
//!
//! ```text
//! CREATE FUNCTION f() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$; SELECT f()
//! ```
//!
//! sent as ONE simple query answered `function f() does not exist` while
//! the CREATE in that same string had just succeeded. PostgreSQL 18.4
//! answers `1`.
//!
//! A multi-statement simple query is an implicit transaction, so the new
//! function lives in the transaction's shadow catalog; `ev_ctx` threaded
//! the COMMITTED one. `CREATE TABLE t(…); SELECT count(*) FROM t` in the
//! same position was already right — the ninth member of the family
//! v7.38.18 recorded for `ANALYZE`, where eight statement kinds read the
//! active catalog and one did not.
//!
//! **How it was found.** sentori's status ledger lists `RETURNS
//! bigint[]` as fixed in v7.37.25, its status column reading "on
//! 7.38.1" — eighteen releases behind. Re-verifying that row rather than
//! believing it produced `function fb() does not exist`, which looked
//! like an array-return regression and was not: on its own the array
//! return is fine. The ledger's probe had been the two-statement form,
//! and what it caught was a different defect neither side knew about.
//!
//! **It is a wire test for a measured reason.** A corpus file cannot
//! express it — the runner sends each statement separately, and the
//! negative control there stayed green. Nor can an engine test:
//! `Engine::execute` takes one statement and does not split on `;`. Only
//! the simple-query path forms the string this defect needs. Fifth time
//! in this release that the first instrument reached for could not see
//! what it was pointed at.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(10);

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

/// One Q frame carrying the whole string — the shape `psql -c` sends,
/// and the only shape that exercises the multi-statement path.
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

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s);
    let _ = read_until_ready(&mut s);
    s
}

fn q(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    send_query(s, sql);
    read_until_ready(s)
}

fn sqlstate(msgs: &[PgMessage]) -> Option<String> {
    let m = msgs.iter().find(|m| m.ty == b'E')?;
    let mut i = 0;
    while i < m.body.len() && m.body[i] != 0 {
        let code = m.body[i];
        let start = i + 1;
        let end = start + m.body[start..].iter().position(|&b| b == 0)?;
        if code == b'C' {
            return Some(String::from_utf8_lossy(&m.body[start..end]).into_owned());
        }
        i = end + 1;
    }
    None
}

fn one_value(s: &mut TcpStream, sql: &str) -> String {
    let msgs = q(s, sql);
    let d = msgs.iter().find(|m| m.ty == b'D').expect("a DataRow");
    let len = i32::from_be_bytes([d.body[2], d.body[3], d.body[4], d.body[5]]);
    String::from_utf8_lossy(&d.body[6..6 + len as usize]).into_owned()
}

fn spawn(label: &str) -> (common::ChildGuard, common::ServerAddrs) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir: PathBuf =
        crate::common::tmp_base().join(format!("spg-e2e-implicittx-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    (common::ChildGuard(raw), addrs)
}

#[test]
fn a_function_created_in_the_same_query_string_is_visible() {
    let (_child, addrs) = spawn("fnvis");
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    assert_eq!(
        one_value(
            &mut s,
            "CREATE FUNCTION vf() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$; SELECT vf()"
        ),
        "1"
    );
    assert_eq!(
        one_value(
            &mut s,
            "CREATE FUNCTION vb() RETURNS bigint LANGUAGE sql AS $$ SELECT 7::bigint $$; SELECT vb()"
        ),
        "7"
    );
    // Two of them, in one expression.
    assert_eq!(
        one_value(
            &mut s,
            "CREATE FUNCTION va() RETURNS int LANGUAGE sql AS $$ SELECT 10 $$; \
             CREATE FUNCTION vc() RETURNS int LANGUAGE sql AS $$ SELECT 32 $$; SELECT va() + vc()"
        ),
        "42"
    );
    // The customer's ledger entry, in the form their probe used.
    assert_eq!(
        one_value(
            &mut s,
            "CREATE FUNCTION vr() RETURNS bigint[] LANGUAGE sql AS $$ SELECT ARRAY[1,2]::bigint[] $$; SELECT vr()"
        ),
        "{1,2}"
    );
}

/// v7.38.19 — the whole family, enumerated rather than waited for.
///
/// The function case was the NINTH thing found this way: v7.38.18 fixed
/// `ANALYZE` and recorded that seven other statement kinds in the same
/// position were already right. Each of those was found by something
/// being wrong, one at a time.
///
/// So the rest of the family is listed here instead. Every one of these
/// creates an object and uses it inside ONE simple query, and every one
/// was measured against PostgreSQL 18.4 before being written down. They
/// all passed on the day this was written — which is the point: the next
/// one to break has a test rather than a customer.
#[test]
fn every_object_created_in_one_query_string_is_visible_to_the_rest() {
    let (_child, addrs) = spawn("famvis");
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    // Two tables and a join — this one reads the catalog through the
    // join reorderer, which is a different path from the scan.
    assert_eq!(
        one_value(
            &mut s,
            "CREATE TABLE j1(i int); CREATE TABLE j2(i int); \
             INSERT INTO j1 VALUES (1),(2); INSERT INTO j2 VALUES (2),(3); \
             SELECT count(*) FROM j1 JOIN j2 ON j1.i=j2.i"
        ),
        "1"
    );
    assert_eq!(
        one_value(
            &mut s,
            "CREATE TABLE v1(i int); INSERT INTO v1 VALUES (1); \
             CREATE VIEW vv AS SELECT i FROM v1; SELECT count(*) FROM vv"
        ),
        "1"
    );
    assert_eq!(
        one_value(
            &mut s,
            "CREATE TABLE ix2(i int); INSERT INTO ix2 VALUES (1),(2); \
             CREATE INDEX ix2_i ON ix2(i); SELECT count(*) FROM ix2 WHERE i=2"
        ),
        "1"
    );
    assert_eq!(
        one_value(
            &mut s,
            "CREATE TYPE mood2 AS ENUM ('a','b'); CREATE TABLE mt(m mood2); \
             INSERT INTO mt VALUES ('a'); SELECT count(*) FROM mt"
        ),
        "1"
    );
    assert_eq!(
        one_value(&mut s, "CREATE SEQUENCE sq9; SELECT nextval('sq9')"),
        "1"
    );
    assert_eq!(
        one_value(
            &mut s,
            "CREATE DOMAIN pos AS int CHECK (VALUE > 0); CREATE TABLE dt(a pos); \
             INSERT INTO dt VALUES (1); SELECT count(*) FROM dt"
        ),
        "1"
    );
}

/// The control: a TABLE in the same position was always visible and must
/// stay so. It is what made this look like an array defect rather than a
/// visibility one.
#[test]
fn a_table_created_in_the_same_query_string_is_still_visible() {
    let (_child, addrs) = spawn("tblvis");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    assert_eq!(
        one_value(
            &mut s,
            "CREATE TABLE vt(i int); INSERT INTO vt VALUES (1),(2); SELECT count(*) FROM vt"
        ),
        "2"
    );
}
