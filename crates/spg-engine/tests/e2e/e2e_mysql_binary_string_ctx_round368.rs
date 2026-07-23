//! read01 round 368 (MySQL differential, M20 P3) — a binary string
//! (`0x…` / `X'…'` / `b'…'`, backed by `Value::Bytes`) reads as its raw
//! bytes in a STRING context on the MySQL dialect: `CONCAT(0x41,'B')` is
//! 'AB', not PG's `\x41B`.
//!
//! Rounds 366–367 made the literal a binary string with numeric-context
//! coercion; what stayed wrong was the string context — CONCAT and the
//! other text functions rendered the bytes as PG's `\x…` hex form, so any
//! string built from a hex literal came out as `\x41B` garbage. A
//! PostgreSQL session keeps the `\x…` rendering (a real bytea has no
//! MySQL latin-1 reading).
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match rows
            .first()
            .and_then(|r| r.values.first())
            .cloned()
            .map(Value::into_owned)
        {
            Some(Value::Text(s)) => s.into_owned(),
            other => panic!("`{sql}` was not text: {other:?}"),
        },
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

/// CONCAT reads a hex literal as its bytes, not as `\x…`.
#[test]
fn concat_reads_bytes_as_a_string() {
    let mut e = mysql();
    assert_eq!(text(&mut e, "SELECT CONCAT(0x41, 'B')"), "AB");
    assert_eq!(text(&mut e, "SELECT CONCAT(0x4142, '!')"), "AB!");
    assert_eq!(text(&mut e, "SELECT CONCAT('x', X'4142')"), "xAB");
}

/// `CONCAT_WS` shares the same rendering.
#[test]
fn concat_ws_reads_bytes_as_a_string() {
    let mut e = mysql();
    assert_eq!(text(&mut e, "SELECT CONCAT_WS('-', 0x41, 0x42)"), "A-B");
}

/// A PostgreSQL session keeps PG's `\x…` bytea rendering — the dialect
/// flag must not leak into PG.
#[test]
fn postgres_session_keeps_hex_rendering() {
    let mut p = Engine::new();
    assert_eq!(
        text(&mut p, "SELECT CONCAT('\\x41'::bytea, 'B')"),
        "\\x41B"
    );
}
