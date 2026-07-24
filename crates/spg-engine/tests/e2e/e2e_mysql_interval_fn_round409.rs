//! read01 round 409 (MySQL differential) — the `INTERVAL(N, N1, N2, …)`
//! function.
//!
//! Distinct from the interval literal (`INTERVAL 1 DAY`), MySQL's
//! `INTERVAL(N, N1, N2, …)` returns the count of leading list values that are
//! ≤ N — a binary-search index into an ascending list: 0 when N < N1, k when
//! Nk ≤ N < N(k+1). A NULL search value returns -1; a NULL list element sorts
//! below N. Numbers compare as doubles (no rounding), strings via their
//! leading-numeric prefix. SPG had no such function — `INTERVAL(5,1,3,7)` was
//! a parse error. The interval literal forms are unchanged, and a PostgreSQL
//! session (no INTERVAL function) still rejects the call.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn scalar(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::Null => "NULL".to_string(),
            // Render dates / intervals as their canonical text (as psql does).
            v => spg_engine::eval::value_to_text(v),
        },
        other => panic!("{other:?}"),
    }
}

/// Binary-search index into the ascending list.
#[test]
fn index_into_list() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT INTERVAL(5,1,3,7)"), "2");
    assert_eq!(scalar(&mut e, "SELECT INTERVAL(0,1,3,7)"), "0");
    assert_eq!(scalar(&mut e, "SELECT INTERVAL(1,1,3,7)"), "1");
    assert_eq!(scalar(&mut e, "SELECT INTERVAL(3,1,3,7)"), "2");
    assert_eq!(scalar(&mut e, "SELECT INTERVAL(8,1,3,7)"), "3");
    assert_eq!(scalar(&mut e, "SELECT INTERVAL(-1,1,3,7)"), "0");
    assert_eq!(scalar(&mut e, "SELECT INTERVAL(23,1,15,17,30,44)"), "3");
}

/// A fractional N compares directly (no rounding, unlike ELT).
#[test]
fn fractional_no_rounding() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT INTERVAL(1.5,1,2,3)"), "1");
    assert_eq!(scalar(&mut e, "SELECT INTERVAL(5.9,1,6,7)"), "1");
}

/// NULL search value returns -1; a NULL list element sorts below N.
#[test]
fn null_handling() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT INTERVAL(NULL,1,3,7)"), "-1");
    assert_eq!(scalar(&mut e, "SELECT INTERVAL(5,1,NULL,7)"), "2");
}

/// A string search value is read via its leading-numeric prefix.
#[test]
fn string_coercion() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT INTERVAL('5',1,3,7)"), "2");
}

/// The interval LITERAL forms are unaffected by the function addition.
#[test]
fn interval_literal_unchanged() {
    let mut e = mysql();
    assert_eq!(
        scalar(&mut e, "SELECT DATE_ADD('2020-01-01', INTERVAL 2 DAY)"),
        "2020-01-03"
    );
    assert_eq!(
        scalar(&mut e, "SELECT '2020-01-01' + INTERVAL 1 MONTH"),
        "2020-02-01"
    );
}

/// A PostgreSQL session has no INTERVAL function and rejects the call, but
/// its interval literal still parses.
#[test]
fn postgres_rejects_function() {
    let mut e = Engine::new();
    assert!(
        e.execute("SELECT INTERVAL(5,1,3,7)").is_err(),
        "PG has no INTERVAL() function"
    );
    assert_eq!(scalar(&mut e, "SELECT INTERVAL '2 days'"), "2 days");
}
