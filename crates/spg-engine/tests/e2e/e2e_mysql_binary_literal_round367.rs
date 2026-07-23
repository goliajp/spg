//! read01 round 367 (MySQL differential, M20 P1+P2) — `0x…` / `X'…'` /
//! `b'…'` are BINARY STRINGS in the MySQL dialect, with numeric-context
//! coercion.
//!
//! MySQL's `0x41` is the one-byte string 'A', not the integer 65 — and
//! mysqldump emits `0x…` for every BINARY / BLOB column, so reading it as
//! an integer silently corrupts a restored dump. In a numeric context the
//! literal is its bytes' big-endian value (`0x4142 + 0` = 16706,
//! `0x10 = 16`), and it compares byte-wise against a string (`0x61 = 'a'`
//! is true). On INSERT it lands in each column the MariaDB way: bytes into
//! BINARY / BLOB, its number into a numeric column, its bytes-as-string
//! into a text column.
//!
//! A PostgreSQL session is unaffected: there `0x10` stays a radix-16
//! integer and `X'…'` stays a bit string.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn scalar(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.values.first())
            .cloned()
            .map(Value::into_owned)
            .unwrap_or(Value::Null),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

fn bytes<'a>(v: &'a Value<'a>) -> &'a [u8] {
    match v {
        Value::Bytes(b) => b.as_ref(),
        other => panic!("expected Bytes, got {other:?}"),
    }
}

/// The bare literal is a binary string carrying its bytes.
#[test]
fn hex_literal_is_a_binary_string() {
    let mut e = mysql();
    assert_eq!(bytes(&scalar(&mut e, "SELECT 0x41")), &[0x41]);
    assert_eq!(bytes(&scalar(&mut e, "SELECT 0x4142")), &[0x41, 0x42]);
    // Odd digit count left-pads (MariaDB): 0x123 -> 01 23.
    assert_eq!(bytes(&scalar(&mut e, "SELECT 0x123")), &[0x01, 0x23]);
    // X'…' is the same byte string.
    assert_eq!(bytes(&scalar(&mut e, "SELECT X'41'")), &[0x41]);
    // b'…' packs bits big-endian, left-padded to a byte.
    assert_eq!(bytes(&scalar(&mut e, "SELECT b'1000001'")), &[0x41]);
    assert_eq!(bytes(&scalar(&mut e, "SELECT b'1010'")), &[0x0A]);
}

/// In a numeric context the literal is its bytes' big-endian value.
#[test]
fn numeric_context_is_big_endian() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT 0x41 + 0"), Value::BigInt(65));
    assert_eq!(scalar(&mut e, "SELECT 0x4142 + 0"), Value::BigInt(16706));
    assert_eq!(scalar(&mut e, "SELECT X'41' + 0"), Value::BigInt(65));
    assert_eq!(scalar(&mut e, "SELECT b'1000001' + 0"), Value::BigInt(65));
    // Comparison against a number coerces the same way.
    assert_eq!(scalar(&mut e, "SELECT 0x10 = 16"), Value::Bool(true));
    assert_eq!(scalar(&mut e, "SELECT 0x10 = 17"), Value::Bool(false));
}

/// Against a string operand the literal compares byte-wise.
#[test]
fn compares_byte_wise_against_a_string() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT 0x61 = 'a'"), Value::Bool(true));
    assert_eq!(scalar(&mut e, "SELECT 0x62 = 'a'"), Value::Bool(false));
}

/// INSERT routes the literal into each column type the MariaDB way.
#[test]
fn insert_coerces_per_column_type() {
    let mut e = mysql();
    e.execute("CREATE TABLE t (b BLOB, v VARBINARY(10), s VARCHAR(10), n INT)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (0x4142, 0x4344, 0x4546, 0x10)")
        .unwrap();
    // BLOB / VARBINARY keep the raw bytes (the mysqldump path).
    assert_eq!(bytes(&scalar(&mut e, "SELECT b FROM t")), &[0x41, 0x42]);
    assert_eq!(bytes(&scalar(&mut e, "SELECT v FROM t")), &[0x43, 0x44]);
    // VARCHAR takes the bytes as a string; INT takes the big-endian number.
    assert_eq!(scalar(&mut e, "SELECT s FROM t"), Value::text("EF"));
    assert_eq!(scalar(&mut e, "SELECT n FROM t"), Value::Int(16));
}

/// `X'…'` with an odd number of hex digits is a syntax error (MariaDB).
#[test]
fn odd_length_x_literal_is_rejected() {
    let mut e = mysql();
    assert!(e.execute("SELECT X'123'").is_err());
}

/// A PostgreSQL session keeps the PG readings — `0x10` is the integer 16,
/// so it inserts into an INT column and does arithmetic as an integer.
#[test]
fn postgres_session_reads_0x_as_integer() {
    let mut p = Engine::new();
    assert_eq!(scalar(&mut p, "SELECT 0x10 + 0"), Value::Int(16));
    p.execute("CREATE TABLE t (n INT)").unwrap();
    p.execute("INSERT INTO t VALUES (0x10)").unwrap();
    assert_eq!(scalar(&mut p, "SELECT n FROM t"), Value::Int(16));
}
