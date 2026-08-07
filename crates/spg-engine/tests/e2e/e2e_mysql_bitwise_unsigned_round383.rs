//! read01 round 383 (MySQL differential) — the bitwise operators
//! (`~ & | ^ << >>`) are UNSIGNED 64-bit under the MySQL dialect.
//!
//! MariaDB computes every bitwise operator on `BIGINT UNSIGNED`, so a set
//! high bit reads as a large positive value, not PG's signed result:
//!   ~5            = 18446744073709551610   (PG: -6)
//!   -8 >> 1       = 9223372036854775804    (logical, not arithmetic)
//!   1 << 64       = 0                       (no shift-count masking)
//! and `^` is bitwise XOR, not PG's exponentiation:
//!   5 ^ 1         = 4                        (PG: 5.0)
//! Operands round to the nearest integer (`2 & 2.9` = 2, `~2.9` = ~3),
//! and a `0x…` binary literal reads as its value (`~5 & 0xFF` = 250).
//! A PostgreSQL session keeps `^` = power and `~` = signed complement.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

/// The scalar result of `sql` as an i128 (a MySQL bitwise result is a
/// scale-0 NUMERIC that can exceed i64).
fn int(e: &mut Engine, sql: &str) -> i128 {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::Numeric {
                scaled, scale: 0, ..
            } => *scaled,
            Value::Int(n) => i128::from(*n),
            Value::BigInt(n) => i128::from(*n),
            Value::SmallInt(n) => i128::from(*n),
            other => panic!("`{sql}` not an integer: {other:?}"),
        },
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

/// Unary `~` is the unsigned 64-bit complement.
#[test]
fn bit_not_is_unsigned() {
    let mut e = mysql();
    assert_eq!(int(&mut e, "SELECT ~5"), 18_446_744_073_709_551_610);
    assert_eq!(int(&mut e, "SELECT ~0"), 18_446_744_073_709_551_615);
    assert_eq!(int(&mut e, "SELECT ~1"), 18_446_744_073_709_551_614);
    // ~(-1): -1 is all ones, complement is 0.
    assert_eq!(int(&mut e, "SELECT ~(-1)"), 0);
    // Operand rounds to the nearest integer: 2.9 -> 3, ~3.
    assert_eq!(int(&mut e, "SELECT ~2.9"), 18_446_744_073_709_551_612);
}

/// `&` `|` `^` are unsigned; `^` is XOR (not exponentiation).
#[test]
fn and_or_xor() {
    let mut e = mysql();
    assert_eq!(int(&mut e, "SELECT 5 & 3"), 1);
    assert_eq!(int(&mut e, "SELECT 5 | 2"), 7);
    assert_eq!(int(&mut e, "SELECT 5 ^ 1"), 4);
    assert_eq!(int(&mut e, "SELECT 5 ^ 3"), 6);
    // A 0x… binary literal reads as its value in a bitwise op.
    assert_eq!(int(&mut e, "SELECT ~5 & 0xFF"), 250);
    // A float operand rounds to the nearest integer.
    assert_eq!(int(&mut e, "SELECT 2 & 2.9"), 2);
    assert_eq!(int(&mut e, "SELECT 2 & 2.4"), 2);
}

/// `<<` / `>>` shift on the unsigned pattern (logical, no masking).
#[test]
fn shifts() {
    let mut e = mysql();
    assert_eq!(int(&mut e, "SELECT 1 << 4"), 16);
    assert_eq!(int(&mut e, "SELECT 256 >> 2"), 64);
    // -8 as u64 shifted right one is a large positive (logical shift).
    assert_eq!(int(&mut e, "SELECT -8 >> 1"), 9_223_372_036_854_775_804);
    assert_eq!(int(&mut e, "SELECT 1 << 63"), 9_223_372_036_854_775_808);
    // A shift of 64 or more is 0 (MySQL does not mask the count).
    assert_eq!(int(&mut e, "SELECT 1 << 64"), 0);
    assert_eq!(int(&mut e, "SELECT 256 >> 64"), 0);
}

/// `^` binds tighter than `* & |` (MySQL precedence).
#[test]
fn xor_precedence() {
    let mut e = mysql();
    // (2 ^ 3) * 4 = 1 * 4 = 4
    assert_eq!(int(&mut e, "SELECT 2 ^ 3 * 4"), 4);
    // 1 | (2 ^ 3) = 1 | 1 = 1
    assert_eq!(int(&mut e, "SELECT 1 | 2 ^ 3"), 1);
    // 5 & (3 ^ 1) = 5 & 2 = 0
    assert_eq!(int(&mut e, "SELECT 5 & 3 ^ 1"), 0);
}

/// A NULL operand yields NULL.
#[test]
fn null_operand() {
    let mut e = mysql();
    assert!(matches!(
        e.execute("SELECT 3 & NULL").unwrap(),
        QueryResult::Rows { rows, .. } if rows[0].values[0] == Value::Null
    ));
    assert!(matches!(
        e.execute("SELECT ~NULL").unwrap(),
        QueryResult::Rows { rows, .. } if rows[0].values[0] == Value::Null
    ));
}

/// A PostgreSQL session keeps `^` = power and `~` = signed complement.
#[test]
fn postgres_unchanged() {
    let mut p = Engine::new();
    // `^` is exponentiation: 2 ^ 3 = 8.0
    match p.execute("SELECT 2 ^ 3").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0].values[0], Value::Float(8.0));
        }
        other => panic!("2 ^ 3: {other:?}"),
    }
    // `~5` is the signed complement -6
    assert_eq!(int(&mut p, "SELECT ~5"), -6);
}
