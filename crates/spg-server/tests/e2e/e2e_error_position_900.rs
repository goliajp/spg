//! 9.0.0 — the SQLSTATE and the character position of two errors that
//! carried neither, measured against PostgreSQL 18.6.
//!
//! ```text
//!                                              PG 18.6          SPG 8.0.4
//!   SELECT * FROM t WHERE id                   42804 @ 33       42883 @ none
//!   SELECT * FROM t WHERE 1                    42804 @ 33       42883 @ none
//!   SELECT * FROM t HAVING id                  42804 @ 34       42883 @ none
//!   ALTER … ADD CONSTRAINT c UNIQUE USING …    42809 @ CONSTRAINT  42000 @ index
//! ```
//!
//! The position is the reason two of these are here at all: `WHERE 1`
//! has no name in it to point at, and PostgreSQL still draws its caret
//! on the `1`, so the predicate's own token has to be carried. And the
//! constraint refusals name the INDEX while PostgreSQL points at
//! `CONSTRAINT`, so reading a name back out of the message — which is
//! how the host answers every other position — gives the wrong column.
//!
//! Positions are 1-based character offsets, as PostgreSQL's `P` field is.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
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

fn send_msg(s: &mut TcpStream, ty: u8, body: &[u8]) {
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.push(ty);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(body);
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
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0admin\0\0");
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    read_until_ready(&mut s);
    s
}

fn field(body: &[u8], code: u8) -> Option<String> {
    let mut p = 0;
    while p < body.len() && body[p] != 0 {
        let c = body[p];
        let start = p + 1;
        let mut end = start;
        while end < body.len() && body[end] != 0 {
            end += 1;
        }
        if c == code {
            return Some(String::from_utf8_lossy(&body[start..end]).into_owned());
        }
        p = end + 1;
    }
    None
}

fn query(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    let mut body = Vec::new();
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    send_msg(s, b'Q', &body);
    read_until_ready(s)
}

fn run(s: &mut TcpStream, sql: &str) {
    let msgs = query(s, sql);
    if let Some(e) = msgs.iter().find(|m| m.ty == b'E') {
        panic!("{sql}: {}", String::from_utf8_lossy(&e.body));
    }
}

/// `(SQLSTATE, message, position)` of the error `sql` is refused with.
fn refused(s: &mut TcpStream, sql: &str) -> (String, String, Option<String>) {
    let msgs = query(s, sql);
    let e = msgs
        .iter()
        .find(|m| m.ty == b'E')
        .unwrap_or_else(|| panic!("{sql}: answered instead of refusing"));
    (
        field(&e.body, b'C').unwrap_or_default(),
        field(&e.body, b'M').unwrap_or_default(),
        field(&e.body, b'P'),
    )
}

/// The 1-based position of `needle` in `sql` — how the expectations
/// below are written, so a re-worded statement cannot silently drift
/// away from the token it means.
fn at(sql: &str, needle: &str) -> String {
    (sql.find(needle).expect("needle in the statement") + 1).to_string()
}

#[test]
fn a_predicate_that_is_not_boolean_names_its_type_class_and_place() {
    let dir = crate::common::tmp_base().join(format!("spg-e2e-errpos-p-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    run(&mut s, "CREATE TABLE p9(id int, name text)");

    // PostgreSQL points at where the predicate STARTS: the column, the
    // qualifier of a qualified column, the operand of a cast, and the
    // literal when there is no name at all (all measured on 18.6).
    for (sql, needle, ty) in [
        ("SELECT * FROM p9 WHERE id", "id", "integer"),
        ("SELECT * FROM p9 WHERE name", "name", "text"),
        ("SELECT * FROM p9 WHERE 1", "1", "integer"),
        ("SELECT * FROM p9 WHERE id::int", "id::int", "integer"),
        ("SELECT * FROM p9 WHERE p9.id", "p9.id", "integer"),
    ] {
        let (code, msg, pos) = refused(&mut s, sql);
        assert_eq!(code, "42804", "{sql}: {msg}");
        assert_eq!(
            msg,
            format!("argument of WHERE must be type boolean, not type {ty}"),
            "{sql}"
        );
        assert_eq!(pos.as_deref(), Some(at(sql, needle).as_str()), "{sql}");
    }

    // HAVING is the same rule, and the caret moves with the keyword.
    let sql = "SELECT * FROM p9 HAVING id";
    let (code, msg, pos) = refused(&mut s, sql);
    assert_eq!(code, "42804", "{sql}: {msg}");
    assert_eq!(
        msg, "argument of HAVING must be type boolean, not type integer",
        "{sql}"
    );
    assert_eq!(pos.as_deref(), Some("25"), "{sql}");

    // What PostgreSQL accepts is still accepted — an untyped string
    // literal coerces, and a boolean column is a predicate.
    for sql in [
        "SELECT * FROM p9 WHERE 't'",
        "SELECT * FROM p9 WHERE id = 1",
        "SELECT count(*) FROM p9 HAVING count(*) > 0",
    ] {
        let msgs = query(&mut s, sql);
        assert!(
            msgs.iter().all(|m| m.ty != b'E'),
            "{sql}: refused a statement PostgreSQL accepts"
        );
    }
}

/// 9.0.0 (A4b) — an error raised inside a PL/pgSQL body carried no
/// `CONTEXT:` line.
///
/// Measured 2026-09-19 and again 2026-09-21 against PG 18.6:
///
/// ```text
///   DO $$ BEGIN\n  RAISE EXCEPTION 'boom';\nEND $$
///     PG:  ERROR: boom
///          CONTEXT:  PL/pgSQL function inline_code_block line 2 at RAISE
///     SPG: ERROR: boom          (and nothing else)
/// ```
///
/// A client could see that something failed and not where inside the
/// function. The line is the INNERMOST statement's, which is what
/// PostgreSQL reports.
///
/// **Residual, measured:** PostgreSQL names a `PERFORM` as `at PERFORM`
/// and prints extra `QUERY:` / `SQL statement "…"` lines for an error
/// raised inside an embedded statement. SPG lowers PERFORM to a SELECT,
/// so it says `at SQL statement`, and prints the one CONTEXT line.
#[test]
fn a_plpgsql_error_carries_postgresqls_context_line() {
    let dir = crate::common::tmp_base().join(format!("spg-e2e-errctx-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    // PostgreSQL's own line, byte for byte.
    let msgs = query(&mut s, "DO $$\nBEGIN\n  RAISE EXCEPTION 'boom';\nEND $$");
    let e = msgs.iter().find(|m| m.ty == b'E').expect("refused");
    assert_eq!(field(&e.body, b'M').as_deref(), Some("boom"));
    assert_eq!(
        field(&e.body, b'W').as_deref(),
        Some("PL/pgSQL function inline_code_block line 3 at RAISE")
    );

    // The INNERMOST statement, not the block it is in.
    let msgs = query(
        &mut s,
        "DO $$\nBEGIN\n  IF true THEN\n    RAISE EXCEPTION 'nested';\n  END IF;\nEND $$",
    );
    let e = msgs.iter().find(|m| m.ty == b'E').expect("refused");
    assert_eq!(
        field(&e.body, b'W').as_deref(),
        Some("PL/pgSQL function inline_code_block line 4 at RAISE")
    );

    // An embedded statement that fails names its own line.
    let msgs = query(
        &mut s,
        "DO $$\nDECLARE x int;\nBEGIN\n  x := 1;\n  INSERT INTO nosuch_ctx VALUES (1);\nEND $$",
    );
    let e = msgs.iter().find(|m| m.ty == b'E').expect("refused");
    assert_eq!(
        field(&e.body, b'W').as_deref(),
        Some("PL/pgSQL function inline_code_block line 5 at SQL statement")
    );

    // …and the line does not leak into the NEXT statement's error.
    let msgs = query(&mut s, "SELECT * FROM nosuch_plain");
    let e = msgs.iter().find(|m| m.ty == b'E').expect("refused");
    assert_eq!(
        field(&e.body, b'W'),
        None,
        "a plain statement has no CONTEXT"
    );
}

/// 9.0.0 (N17) — a PL/pgSQL body's parse error carries no POSITION.
///
/// Measured 2026-09-20 on `… END LOOP zz;` inside a DO block: PG 18.6
/// draws `LINE 4:   END LOOP zz;` with the caret under `zz`, SPG sent
/// the sentence alone. The body is lexed in a context of its own — the
/// statement carries it as ONE dollar-quoted token — so the offset the
/// parser knew was the body's, not the statement's. It carries both
/// now, and the wire adds them.
#[test]
fn a_plpgsql_body_error_points_inside_the_body() {
    let dir = crate::common::tmp_base().join(format!("spg-e2e-errpos-pl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    // PostgreSQL's own caret, measured: under the `zz` of `END LOOP zz`.
    let sql = "DO $$ BEGIN\n  FOR i IN 1..2 LOOP\n    NULL;\n  END LOOP zz;\nEND $$";
    let (code, msg, pos) = refused(&mut s, sql);
    assert_eq!(msg, "end label \"zz\" specified for unlabeled block");
    assert_eq!(code, "42601", "{msg}");
    assert_eq!(pos.as_deref(), Some(at(sql, "zz").as_str()), "{sql}");

    // A longer `$tag$` moves the body along, and the caret with it.
    let tagged = "DO $body$ BEGIN\n  FOR i IN 1..2 LOOP\n    NULL;\n  END LOOP zz;\nEND $body$";
    let (_c, _m, pos) = refused(&mut s, tagged);
    assert_eq!(pos.as_deref(), Some(at(tagged, "zz").as_str()), "{tagged}");

    // And the MESSAGE names a token of the BODY. It used to be shaped
    // against the outer statement, where the only token the offset could
    // reach is the dollar-quoted string itself — `syntax error at or
    // near "$$"` for a fault four lines inside it.
    let sql = "DO $$ DECLARE x int := 1; BEGIN RAISE NOTICE '%', ; END $$";
    let (_c, msg, _p) = refused(&mut s, sql);
    assert!(
        !msg.contains("$$"),
        "the message names a token of the body: {msg}"
    );
}

#[test]
fn a_using_index_refusal_points_at_the_constraint_not_the_index() {
    let dir = crate::common::tmp_base().join(format!("spg-e2e-errpos-c-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    run(&mut s, "CREATE TABLE u9(a int, b text)");
    run(&mut s, "CREATE INDEX u9plain ON u9(a)");
    run(&mut s, "CREATE UNIQUE INDEX u9part ON u9(a) WHERE a > 0");
    run(&mut s, "CREATE UNIQUE INDEX u9expr ON u9(lower(b))");

    // All four refusals: the caret sits on the constraint element, and
    // the message names the index (measured on PostgreSQL 18.6).
    for (sql, needle, code, want) in [
        (
            "ALTER TABLE u9 ADD CONSTRAINT c1 UNIQUE USING INDEX nosuch",
            "CONSTRAINT",
            "42704",
            "index \"nosuch\" does not exist",
        ),
        (
            "ALTER TABLE u9 ADD CONSTRAINT c2 UNIQUE USING INDEX u9plain",
            "CONSTRAINT",
            "42809",
            "\"u9plain\" is not a unique index",
        ),
        (
            "ALTER TABLE u9 ADD CONSTRAINT c3 UNIQUE USING INDEX u9part",
            "CONSTRAINT",
            "42809",
            "\"u9part\" is a partial index",
        ),
        (
            "ALTER TABLE u9 ADD CONSTRAINT c4 UNIQUE USING INDEX u9expr",
            "CONSTRAINT",
            "42809",
            "index \"u9expr\" contains expressions",
        ),
        // No name: the element starts at UNIQUE, and so does the caret.
        (
            "ALTER TABLE u9 ADD UNIQUE USING INDEX u9part",
            "UNIQUE",
            "42809",
            "\"u9part\" is a partial index",
        ),
    ] {
        let (got_code, msg, pos) = refused(&mut s, sql);
        assert_eq!(got_code, code, "{sql}: {msg}");
        assert!(msg.starts_with(want), "{sql}: {msg}");
        assert_eq!(pos.as_deref(), Some(at(sql, needle).as_str()), "{sql}");
    }

    // And an index that CAN back the constraint is still adopted.
    run(&mut s, "CREATE UNIQUE INDEX u9ok ON u9(a)");
    run(
        &mut s,
        "ALTER TABLE u9 ADD CONSTRAINT c_ok UNIQUE USING INDEX u9ok",
    );
}

/// 9.0.2 — the extended protocol's Parse, refused: `(SQLSTATE, position)`.
fn refused_at_parse(s: &mut TcpStream, sql: &str) -> (String, Option<String>) {
    let mut body = Vec::new();
    body.push(0); // unnamed statement
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    body.extend_from_slice(&0u16.to_be_bytes()); // no parameter types
    send_msg(s, b'P', &body);
    send_msg(s, b'S', &[]);
    let msgs = read_until_ready(s);
    let e = msgs
        .iter()
        .find(|m| m.ty == b'E')
        .unwrap_or_else(|| panic!("{sql}: Parse answered instead of refusing"));
    (
        field(&e.body, b'C').unwrap_or_default(),
        field(&e.body, b'P'),
    )
}

/// 9.0.2 — Parse carries the position the simple protocol always did.
///
/// sentori §3.27, measured on 9.0.1: over the simple protocol a relation
/// error has its `LINE`/caret, and the same statement through Parse — what
/// every driver sends, and what psql's `\gdesc` and `\bind` send — had
/// none. The engine's error was turned into a sentence and its position
/// dropped with it. PostgreSQL 18.6 puts the caret at the same place both
/// ways.
#[test]
fn an_error_at_parse_carries_its_position() {
    let dir = crate::common::tmp_base().join(format!("spg-e2e-errpos-x-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    for (sql, needle, state) in [
        (
            "SELECT version FROM nosuch_migrations",
            "nosuch_migrations",
            "42P01",
        ),
        ("SELECT nosuchcol FROM pg_class", "nosuchcol", "42703"),
        ("SHOW $1", "$1", "42601"),
    ] {
        assert_eq!(
            refused_at_parse(&mut s, sql),
            (state.to_string(), Some(at(sql, needle))),
            "{sql}"
        );
        // …the same place the simple protocol reports.
        assert_eq!(
            refused(&mut s, sql).2,
            Some(at(sql, needle)),
            "{sql} (simple)"
        );
    }
}
