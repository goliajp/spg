//! read01 round 320 (V53) — the statements pgwire answered without asking
//! the engine.
//!
//! `canned_response` short-circuits a list of common statements before the
//! parse/execute path, for latency. That is sound only while the canned
//! answer is the one the engine would give. Round 118 found `SHOW
//! transaction_isolation` had drifted; round 319 found `SELECT
//! current_user`. This is the audit of the rest of the table, every
//! expectation read off live PG 18.4.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);
const OID_INT4: u32 = 23;
const OID_INT8: u32 = 20;

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-e2e-canned-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

struct PgMessage {
    ty: u8,
    body: Vec<u8>,
}

fn read_message(s: &mut TcpStream) -> Option<PgMessage> {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).ok()?;
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let body_len = len.saturating_sub(4);
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
        s.read_exact(&mut body).ok()?;
    }
    Some(PgMessage { ty, body })
}

fn send_startup(s: &mut TcpStream, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.push(0);
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(b'Q');
    out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn exchange(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    send_query(s, sql);
    let mut out = Vec::new();
    while let Some(m) = read_message(s) {
        let z = m.ty == b'Z';
        out.push(m);
        if z {
            break;
        }
    }
    out
}

fn datarow_cell(body: &[u8], col_idx: usize) -> Option<String> {
    let cells = u16::from_be_bytes([body[0], body[1]]) as usize;
    if col_idx >= cells {
        return None;
    }
    let mut p = 2;
    for i in 0..cells {
        let len = i32::from_be_bytes([body[p], body[p + 1], body[p + 2], body[p + 3]]);
        p += 4;
        if len < 0 {
            if i == col_idx {
                return None;
            }
            continue;
        }
        let l = len as usize;
        if i == col_idx {
            return Some(std::str::from_utf8(&body[p..p + l]).unwrap().to_string());
        }
        p += l;
    }
    None
}

/// The first column's type OID out of a RowDescription body:
/// u16 field count, then per field: name cstr, table oid (4), column (2),
/// type oid (4), …
fn first_col_type_oid(body: &[u8]) -> u32 {
    let mut p = 2;
    let end = body[p..].iter().position(|&b| b == 0).unwrap() + p;
    p = end + 1 + 4 + 2;
    u32::from_be_bytes([body[p], body[p + 1], body[p + 2], body[p + 3]])
}

fn scalar(s: &mut TcpStream, sql: &str) -> Option<String> {
    exchange(s, sql)
        .iter()
        .find(|m| m.ty == b'D')
        .map(|m| datarow_cell(&m.body, 0))
        .unwrap_or_else(|| panic!("no row for `{sql}`"))
}

fn tag(s: &mut TcpStream, sql: &str) -> String {
    let msgs = exchange(s, sql);
    let c = msgs
        .iter()
        .find(|m| m.ty == b'C')
        .unwrap_or_else(|| panic!("no CommandComplete for `{sql}`: got {:?}", errors(&msgs)));
    let end = c.body.iter().position(|&b| b == 0).unwrap();
    String::from_utf8(c.body[..end].to_vec()).unwrap()
}

fn errors(msgs: &[PgMessage]) -> Vec<String> {
    msgs.iter()
        .filter(|m| m.ty == b'E')
        .map(|m| String::from_utf8_lossy(&m.body).into_owned())
        .collect()
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "alice");
    while let Some(m) = read_message(&mut s) {
        if m.ty == b'Z' {
            break;
        }
    }
    s
}

fn spawn() -> (common::ChildGuard, String) {
    let db = unique_tmpdir("svc").join("spg.db");
    let (child, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .with_pgwire()
        .spawn();
    let addr = addrs.pgwire.expect("pgwire addr");
    (common::ChildGuard(child), addr)
}

/// `SELECT 1` is the most-run query on the wire, and the fast path typed
/// it int8. PG: `pg_typeof(1)` is `integer`, `pg_typeof(2147483648)` is
/// `bigint`.
#[test]
fn a_literal_int_keeps_pgs_width() {
    let (_guard, addr) = spawn();
    let mut s = open(&addr);

    let msgs = exchange(&mut s, "SELECT 1");
    let desc = msgs.iter().find(|m| m.ty == b'T').expect("RowDescription");
    assert_eq!(
        first_col_type_oid(&desc.body),
        OID_INT4,
        "SELECT 1 is int4 in PG"
    );

    let msgs = exchange(&mut s, "SELECT 2147483648");
    let desc = msgs.iter().find(|m| m.ty == b'T').expect("RowDescription");
    assert_eq!(
        first_col_type_oid(&desc.body),
        OID_INT8,
        "a literal past int4 widens"
    );
}

/// A canned `SHOW` answered a fixed value, so the client's own `SET` was
/// invisible.
#[test]
fn show_reports_the_clients_own_setting() {
    let (_guard, addr) = spawn();
    let mut s = open(&addr);

    assert_eq!(tag(&mut s, "SET search_path TO app"), "SET");
    assert_eq!(
        scalar(&mut s, "SHOW search_path").as_deref(),
        Some("app"),
        "SHOW search_path must follow SET"
    );

    assert_eq!(
        scalar(&mut s, "SHOW standard_conforming_strings").as_deref(),
        Some("on"),
        "default"
    );
    assert_eq!(tag(&mut s, "SET standard_conforming_strings = off"), "SET");
    assert_eq!(
        scalar(&mut s, "SHOW standard_conforming_strings").as_deref(),
        Some("off"),
    );
}

/// `SET TRANSACTION ISOLATION LEVEL` was answered `SET` and dropped, so a
/// client that asked for SERIALIZABLE silently stayed on READ COMMITTED.
#[test]
fn set_transaction_isolation_level_takes_effect() {
    let (_guard, addr) = spawn();
    let mut s = open(&addr);

    assert_eq!(tag(&mut s, "BEGIN"), "BEGIN");
    assert_eq!(
        tag(&mut s, "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE"),
        "SET"
    );
    assert_eq!(
        scalar(&mut s, "SHOW transaction_isolation").as_deref(),
        Some("serializable"),
        "the level the client asked for"
    );
    assert_eq!(tag(&mut s, "ROLLBACK"), "ROLLBACK");
}

/// VACUUM and ANALYZE reach the engine. The short-circuit meant the round
/// 169 "manual reclaim is silently ignored" fix never shipped past pgwire.
#[test]
fn vacuum_and_analyze_reach_the_engine() {
    let (_guard, addr) = spawn();
    let mut s = open(&addr);

    assert_eq!(
        tag(&mut s, "CREATE TABLE t (id INT NOT NULL)"),
        "CREATE TABLE"
    );
    for i in 0..50 {
        let _ = tag(&mut s, &format!("INSERT INTO t VALUES ({i})"));
    }
    assert_eq!(tag(&mut s, "DELETE FROM t WHERE id < 40"), "DELETE 40");

    // Dead versions exist; VACUUM reclaims them. `pg_stat_user_tables`
    // is the observable — a canned no-op cannot move it.
    let dead = |s: &mut TcpStream| -> i64 {
        scalar(
            s,
            "SELECT n_dead_tup FROM pg_stat_user_tables WHERE relname = 't'",
        )
        .expect("n_dead_tup")
        .parse()
        .unwrap()
    };
    let dead_before = dead(&mut s);
    assert!(dead_before > 0, "the DELETE left dead versions");

    assert_eq!(tag(&mut s, "VACUUM"), "VACUUM");
    let dead_after = dead(&mut s);
    assert!(
        dead_after < dead_before,
        "VACUUM must reclaim: {dead_before} → {dead_after}"
    );

    assert_eq!(tag(&mut s, "ANALYZE"), "ANALYZE");
}

/// `DISCARD ALL` really discards. pgbouncer runs it between pooled client
/// sessions; a no-op leaked one client's session state to the next.
///
/// PG 18.4 measured: after `SET application_name='x'; PREPARE p …;
/// DISCARD ALL`, application_name is back at its startup value and
/// `EXECUTE p` fails with `prepared statement "p" does not exist`.
#[test]
fn discard_all_wipes_the_session() {
    let (_guard, addr) = spawn();
    let mut s = open(&addr);

    assert_eq!(tag(&mut s, "SET application_name = 'x'"), "SET");
    assert_eq!(tag(&mut s, "PREPARE p AS SELECT 1"), "PREPARE");
    assert_eq!(
        scalar(&mut s, "SHOW application_name").as_deref(),
        Some("x")
    );

    assert_eq!(tag(&mut s, "DISCARD ALL"), "DISCARD ALL");

    assert_eq!(
        scalar(&mut s, "SHOW application_name").as_deref(),
        Some(""),
        "a GUC override must not survive DISCARD ALL"
    );
    let msgs = exchange(&mut s, "EXECUTE p");
    assert!(
        !errors(&msgs).is_empty(),
        "the prepared statement must be gone, got {msgs_len} messages",
        msgs_len = msgs.len()
    );

    // The connection's identity is NOT a GUC and must survive.
    assert_eq!(
        scalar(&mut s, "SELECT current_user").as_deref(),
        Some("alice"),
        "DISCARD ALL must not cost the connection its login identity"
    );
}

/// The non-ALL forms name themselves in the tag, as PG does. SPG answered
/// a bare `DISCARD` for all three.
#[test]
fn discard_subforms_carry_pgs_tag() {
    let (_guard, addr) = spawn();
    let mut s = open(&addr);
    assert_eq!(tag(&mut s, "DISCARD PLANS"), "DISCARD PLANS");
    assert_eq!(tag(&mut s, "DISCARD TEMP"), "DISCARD TEMP");
    assert_eq!(tag(&mut s, "DISCARD SEQUENCES"), "DISCARD SEQUENCES");
}

/// PG refuses DISCARD ALL inside a transaction block.
#[test]
fn discard_all_is_refused_inside_a_block() {
    let (_guard, addr) = spawn();
    let mut s = open(&addr);
    assert_eq!(tag(&mut s, "BEGIN"), "BEGIN");
    let msgs = exchange(&mut s, "DISCARD ALL");
    let errs = errors(&msgs);
    assert!(
        errs.iter()
            .any(|e| e.contains("cannot run inside a transaction block")),
        "expected PG's refusal, got {errs:?}"
    );
    assert_eq!(tag(&mut s, "ROLLBACK"), "ROLLBACK");
}

/// `RESET ALL` resets GUCs, not the connection's identity — those are not
/// GUCs, and clearing the whole map took them with it.
#[test]
fn reset_all_keeps_the_connections_identity() {
    let (_guard, addr) = spawn();
    let mut s = open(&addr);
    assert_eq!(tag(&mut s, "SET application_name = 'x'"), "SET");
    assert_eq!(tag(&mut s, "RESET ALL"), "RESET");
    assert_eq!(scalar(&mut s, "SHOW application_name").as_deref(), Some(""));
    assert_eq!(
        scalar(&mut s, "SELECT current_user").as_deref(),
        Some("alice"),
    );
}

/// `current_schema()` follows the session's search_path. It was a
/// hardcoded "public" in the engine, with pgwire canning the same constant
/// a layer above.
///
/// PG 18.4 measured: with `SET search_path TO app` it reports `app` once
/// that schema exists, and NULL while it does not.
#[test]
fn current_schema_follows_search_path() {
    let (_guard, addr) = spawn();
    let mut s = open(&addr);

    assert_eq!(
        scalar(&mut s, "SELECT current_schema()").as_deref(),
        Some("public"),
        "the default search_path resolves to public"
    );

    assert_eq!(tag(&mut s, "SET search_path TO app"), "SET");
    assert_eq!(
        scalar(&mut s, "SELECT current_schema()"),
        None,
        "a search_path naming no existing schema resolves to NULL"
    );

    assert_eq!(tag(&mut s, "CREATE SCHEMA app"), "CREATE SCHEMA");
    assert_eq!(
        scalar(&mut s, "SELECT current_schema()").as_deref(),
        Some("app"),
    );
}
