//! read01 round 349 (MySQL differential, M6) — the clock's precision argument.
//!
//! The checklist recorded `NOW()` / `CURDATE()` / `CURTIME()` as missing.
//! **They are not** — that reading came from a probe built on a bare
//! `Engine::new()`, which has no clock: the engine is `no_std` and cannot
//! read one, so every real host installs it (`with_clock`). With a clock
//! the whole family answers in both dialects. The measurement was wrong,
//! not the engine.
//!
//! What IS missing is the parenthesised precision form, and it is missing
//! on the PG side too: `CURRENT_TIMESTAMP(3)`, `LOCALTIMESTAMP(3)` and
//! `CURRENT_TIME(3)` are PG's own spellings and all three were
//! `function current_timestamp(integer) does not exist`.
//!
//! Measured — MariaDB 11: `NOW(3)` renders `…12:46:41.541`, `NOW(6)` six
//! digits, `NOW(0)` none, and `NOW(7)` is an error. PG 18.4: it has no
//! `now(integer)` at all, so that one spelling is MySQL-only.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

/// 2026-07-22T12:34:56.541528Z
const FIXED: i64 = 1_784_723_696_541_528;

fn engine(mysql: bool) -> Engine {
    let mut e = Engine::new().with_clock(|| FIXED);
    if mysql {
        e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    }
    e
}

fn one(e: &mut Engine, sql: &str) -> Value<'static> {
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

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
        Err(x) => format!("{x}"),
    }
}

/// The family answers once a clock is installed — the thing the bad probe
/// got wrong. Pinned so the claim cannot drift again.
#[test]
fn the_clock_family_answers_in_both_dialects() {
    for mysql in [false, true] {
        let mut e = engine(mysql);
        assert_eq!(one(&mut e, "SELECT NOW()"), Value::Timestamp(FIXED));
        assert_eq!(one(&mut e, "SELECT CURRENT_TIMESTAMP"), Value::Timestamp(FIXED));
        assert_eq!(one(&mut e, "SELECT CURDATE()"), one(&mut e, "SELECT CURRENT_DATE"));
        assert_eq!(one(&mut e, "SELECT CURTIME()"), Value::text("12:34:56"));
    }
}

/// PG's own precision spellings, which did not parse at all.
#[test]
fn pg_precision_spellings_work() {
    let mut e = engine(false);
    assert_eq!(
        one(&mut e, "SELECT CURRENT_TIMESTAMP(3)"),
        Value::Timestamp(1_784_723_696_541_000),
        "three fractional digits kept, the rest truncated"
    );
    assert_eq!(
        one(&mut e, "SELECT LOCALTIMESTAMP(3)"),
        Value::Timestamp(1_784_723_696_541_000)
    );
    assert_eq!(one(&mut e, "SELECT CURRENT_TIME(3)"), Value::text("12:34:56.541"));
    assert_eq!(
        one(&mut e, "SELECT CURRENT_TIMESTAMP(0)"),
        Value::Timestamp(1_784_723_696_000_000)
    );
    assert_eq!(
        one(&mut e, "SELECT CURRENT_TIMESTAMP(6)"),
        Value::Timestamp(FIXED)
    );
}

/// `NOW(n)` is MariaDB's spelling; PG has no `now(integer)` and says so.
#[test]
fn now_with_a_precision_is_mysql_only() {
    let mut m = engine(true);
    assert_eq!(one(&mut m, "SELECT NOW(3)"), Value::Timestamp(1_784_723_696_541_000));
    assert_eq!(one(&mut m, "SELECT NOW(0)"), Value::Timestamp(1_784_723_696_000_000));
    assert_eq!(one(&mut m, "SELECT CURTIME(3)"), Value::text("12:34:56.541"));

    let mut p = engine(false);
    assert_eq!(
        err(&mut p, "SELECT NOW(3)"),
        "eval: type mismatch: function now(integer) does not exist",
        "PG really has no now(integer) — measured"
    );
}

/// Out of range is an error, not a silent full-precision answer.
#[test]
fn too_big_a_precision_is_refused() {
    let mut m = engine(true);
    assert!(
        err(&mut m, "SELECT NOW(7)").contains("does not exist"),
        "MariaDB refuses it too (Too big precision … Maximum is 6)"
    );
    assert!(err(&mut m, "SELECT CURRENT_TIMESTAMP(9)").contains("does not exist"));
}
