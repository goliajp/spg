//! read01 round 423 (MySQL differential) — fractional-second precision on a
//! temporal CAST.
//!
//! Two divergences, both measured on MariaDB 11:
//!   * A BARE temporal type has fractional precision ZERO, so
//!     `CAST(x AS DATETIME)` / `AS TIME` DROP the fraction. SPG kept it —
//!     a value divergence, not just a rendering one.
//!   * Reducing precision TRUNCATES toward zero in MySQL
//!     (`CAST('…00.256' AS DATETIME(1))` is `.2`), where PG's
//!     AdjustTimestamp ROUNDS half-away (`.3`). SPG rounded in both.
//!
//! A PostgreSQL session keeps both PG behaviours: `::timestamp` preserves
//! every microsecond and `::timestamp(1)` rounds.
//!
//! SCOPE — the DECLARED precision of a COLUMN (`d DATETIME(3)`) is still
//! parsed and discarded: `ColumnSchema` has no fractional-seconds field, so
//! a `DATETIME` column keeps the fraction MariaDB would truncate, and a
//! `DATETIME(6)` one renders `.25` where MariaDB pads to `.250000`. That
//! needs a storage-format change (a sparse appendix + FILE_VERSION bump, the
//! shape round 386 used for mysql_int_width) and is a round of its own; it
//! is NOT addressed here. The `column_precision_is_not_yet_modelled` test
//! below pins today's behaviour so the follow-up round has a baseline.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::Null => "NULL".to_string(),
            v => spg_engine::eval::value_to_text(v),
        },
        other => panic!("{other:?}"),
    }
}

/// A bare temporal cast target drops the fraction entirely.
#[test]
fn bare_temporal_cast_has_precision_zero() {
    let mut e = mysql();
    assert_eq!(
        one(&mut e, "SELECT CAST('2020-01-01 00:00:00.256' AS DATETIME)"),
        "2020-01-01 00:00:00"
    );
    assert_eq!(one(&mut e, "SELECT CAST('10:00:00.756' AS TIME)"), "10:00:00");
    // The explicit `(0)` spelling agrees.
    assert_eq!(
        one(&mut e, "SELECT CAST('2020-01-01 00:00:00.256' AS DATETIME(0))"),
        "2020-01-01 00:00:00"
    );
}

/// Reducing precision truncates toward zero, it does not round.
#[test]
fn precision_reduction_truncates() {
    let mut e = mysql();
    // .256 -> .2 (a rounding implementation would give .3).
    assert_eq!(
        one(&mut e, "SELECT CAST('2020-01-01 00:00:00.256' AS DATETIME(1))"),
        "2020-01-01 00:00:00.2"
    );
    assert_eq!(
        one(&mut e, "SELECT CAST('2020-01-01 00:00:00.256' AS DATETIME(2))"),
        "2020-01-01 00:00:00.25"
    );
    // .999 -> .9, not 1.0.
    assert_eq!(
        one(&mut e, "SELECT CAST('2020-01-01 00:00:00.999' AS DATETIME(1))"),
        "2020-01-01 00:00:00.9"
    );
    assert_eq!(one(&mut e, "SELECT CAST('10:00:00.756' AS TIME(1))"), "10:00:00.7");
}

/// A precision that keeps every digit is a no-op on the value.
#[test]
fn full_precision_keeps_value() {
    let mut e = mysql();
    assert_eq!(
        one(&mut e, "SELECT CAST('2020-01-01 00:00:00.256' AS DATETIME(6))"),
        // MariaDB renders `.256000`; the INSTANT matches, the zero-padding
        // is the queued column/expression-precision work.
        "2020-01-01 00:00:00.256"
    );
}

/// The `TIMESTAMP(x)` FUNCTION keeps the literal's own fraction — it is not
/// the bare-DATETIME cast, and must not pick up its precision-0 default.
#[test]
fn timestamp_function_keeps_fraction() {
    let mut e = mysql();
    assert_eq!(
        one(&mut e, "SELECT TIMESTAMP('2020-01-01 00:00:00.5')"),
        "2020-01-01 00:00:00.5"
    );
    assert_eq!(
        one(&mut e, "SELECT TIMESTAMP('2020-01-01 00:00:00.25')"),
        "2020-01-01 00:00:00.25"
    );
    // The round-418 two-arg form is unaffected too.
    assert_eq!(
        one(&mut e, "SELECT TIMESTAMP('2020-01-05','01:30:45')"),
        "2020-01-05 01:30:45"
    );
    assert_eq!(
        one(&mut e, "SELECT ADDTIME('2020-01-05 10:00:00','00:00:01.5')"),
        "2020-01-05 10:00:01.500000"
    );
}

/// BASELINE for the follow-up round: a column's declared precision is not
/// modelled yet, so the fraction survives where MariaDB would truncate
/// (`DATETIME`) or pad (`DATETIME(6)` renders `.250000` there).
#[test]
fn column_precision_is_not_yet_modelled() {
    let mut e = mysql();
    e.execute("CREATE TABLE t0(d DATETIME)").unwrap();
    e.execute("CREATE TABLE t6(d DATETIME(6))").unwrap();
    e.execute("INSERT INTO t0 VALUES('2020-01-01 00:00:00.25')").unwrap();
    e.execute("INSERT INTO t6 VALUES('2020-01-01 00:00:00.25')").unwrap();
    // MariaDB: '2020-01-01 00:00:00' — SPG keeps the fraction (queued).
    assert_eq!(one(&mut e, "SELECT d FROM t0"), "2020-01-01 00:00:00.25");
    // MariaDB: '2020-01-01 00:00:00.250000' — SPG trims (queued).
    assert_eq!(one(&mut e, "SELECT d FROM t6"), "2020-01-01 00:00:00.25");
}

/// A PostgreSQL session keeps full microseconds on a bare cast and ROUNDS
/// when a typmod reduces precision.
#[test]
fn postgres_unchanged() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT CAST('2020-01-01 00:00:00.256' AS TIMESTAMP)"),
        "2020-01-01 00:00:00.256"
    );
    assert_eq!(
        one(&mut e, "SELECT '2020-01-01 00:00:00.256'::timestamp"),
        "2020-01-01 00:00:00.256"
    );
    // PG rounds .256 up to .3 at precision 1.
    assert_eq!(
        one(&mut e, "SELECT CAST('2020-01-01 00:00:00.256' AS TIMESTAMP(1))"),
        "2020-01-01 00:00:00.3"
    );
    assert_eq!(one(&mut e, "SELECT CAST('10:00:00.756' AS TIME)"), "10:00:00.756");
}
