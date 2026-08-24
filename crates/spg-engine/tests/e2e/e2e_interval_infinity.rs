//! v7.38.19 — `interval` has the two infinities PostgreSQL gave it.
//!
//! The ledger described this as a subtraction edge case. It was not:
//! SPG's `Interval` had no infinite value at all, and the subtraction
//! error was one symptom of that.
//!
//! Every expectation below is PostgreSQL 18.4's, run against the same
//! statement rather than reasoned about — forty-six of them across the
//! parse, render, compare, arithmetic, array, aggregate and storage
//! paths. Three that are easy to get wrong from first principles:
//!
//!   * `'inf'::interval` is *invalid input syntax*, though `'inf'` IS
//!     accepted for `float8`. Interval takes the whole word.
//!   * `inf - inf` and `inf * 0` are refused as *interval out of
//!     range* — an indeterminate form, not an overflow.
//!   * `timestamp + inf` is the infinite TIMESTAMP, a value this build
//!     already had.
//!
//! The representation is an `IntervalKind` beside the numbers, and the
//! numbers it writes are the ones PostgreSQL puts on the wire — all
//! three fields at their extreme, measured with `COPY … (FORMAT
//! binary)`. That is why nothing needed a new file version: no finite
//! interval reaches the triple, so a file written before this version
//! cannot contain one.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        spg_storage::Value::Text(t) => t.to_string(),
                        spg_storage::Value::Bool(b) => String::from(if *b { "t" } else { "f" }),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .unwrap_or_default(),
        other => panic!("expected rows from {sql}, got {other:?}"),
    }
}

fn rendered(e: &mut Engine, expr: &str) -> String {
    one(e, &format!("SELECT ({expr})::text"))
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(other) => panic!("expected {sql} to be refused, got {other:?}"),
    }
}

#[test]
fn the_two_infinities_parse_and_render() {
    let mut e = Engine::new();
    for (input, want) in [
        ("'infinity'::interval", "infinity"),
        ("'-infinity'::interval", "-infinity"),
        ("'+infinity'::interval", "infinity"),
        ("'Infinity'::interval", "infinity"),
        ("'INFINITY'::interval", "infinity"),
        ("'@ infinity'::interval", "infinity"),
        ("'infinity '::interval", "infinity"),
        ("'infinity'::text::interval", "infinity"),
    ] {
        assert_eq!(rendered(&mut e, input), want, "{input}");
    }
}

/// `'inf'` is accepted for float and refused for interval. PG 18.4:
/// *invalid input syntax for type interval: "inf"*.
#[test]
fn the_abbreviation_is_not_accepted() {
    let mut e = Engine::new();
    let got = err(&mut e, "SELECT 'inf'::interval");
    assert!(
        got.contains("invalid input syntax for type interval"),
        "{got}"
    );
}

#[test]
fn they_sit_outside_every_finite_interval() {
    let mut e = Engine::new();
    for (expr, want) in [
        ("'infinity'::interval = 'infinity'::interval", "t"),
        ("'infinity'::interval > '100 years'::interval", "t"),
        ("'-infinity'::interval < '-100 years'::interval", "t"),
        ("'infinity'::interval >= 'infinity'::interval", "t"),
        ("'infinity'::interval <> '1 day'::interval", "t"),
        ("isfinite('infinity'::interval)", "f"),
        ("isfinite('-infinity'::interval)", "f"),
        ("isfinite('1 day'::interval)", "t"),
    ] {
        assert_eq!(one(&mut e, &format!("SELECT {expr}")), want, "{expr}");
    }
}

#[test]
fn arithmetic_answers_what_postgresql_answers() {
    let mut e = Engine::new();
    for (expr, want) in [
        ("-('infinity'::interval)", "-infinity"),
        ("-('-infinity'::interval)", "infinity"),
        ("'infinity'::interval * 2", "infinity"),
        ("'infinity'::interval * -1", "-infinity"),
        ("'-infinity'::interval * -2", "infinity"),
        ("'infinity'::interval / 2", "infinity"),
        ("'infinity'::interval + '1 day'::interval", "infinity"),
        ("'1 day'::interval + 'infinity'::interval", "infinity"),
        ("'1 day'::interval - 'infinity'::interval", "-infinity"),
        ("'-infinity'::interval + '-infinity'::interval", "-infinity"),
        ("justify_days('infinity'::interval)", "infinity"),
        ("justify_hours('infinity'::interval)", "infinity"),
        ("justify_interval('-infinity'::interval)", "-infinity"),
    ] {
        assert_eq!(rendered(&mut e, expr), want, "{expr}");
    }
}

/// Two infinities that cancel have no answer, and PostgreSQL says so
/// with the same wording it uses for a scaling that cancels.
#[test]
fn an_indeterminate_form_is_refused() {
    let mut e = Engine::new();
    for expr in [
        "SELECT 'infinity'::interval - 'infinity'::interval",
        "SELECT '-infinity'::interval + 'infinity'::interval",
        "SELECT 'infinity'::interval * 0",
    ] {
        let got = err(&mut e, expr);
        assert!(got.contains("interval out of range"), "{expr}: {got}");
    }
}

/// A timestamp shifted by an infinite interval is the infinite
/// timestamp — a value this build already had, which is why the two
/// have to agree.
#[test]
fn a_timestamp_shifted_by_one_becomes_infinite() {
    let mut e = Engine::new();
    assert_eq!(
        rendered(&mut e, "'2020-01-01'::timestamp + 'infinity'::interval"),
        "infinity"
    );
    assert_eq!(
        rendered(&mut e, "'2020-01-01'::timestamp - 'infinity'::interval"),
        "-infinity"
    );
    assert_eq!(
        rendered(&mut e, "'2020-01-01'::date + 'infinity'::interval"),
        "infinity"
    );
    assert_eq!(one(&mut e, "SELECT isfinite('infinity'::timestamp)"), "f");
}

/// Stored, read back, ordered, aggregated and put in an array — the
/// paths where a kind carried beside the numbers could be dropped.
#[test]
fn it_survives_storage_and_every_collection() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ivt (id int, iv interval, arr interval[])")
        .unwrap();
    e.execute(
        "INSERT INTO ivt VALUES (1,'infinity','{infinity,1 day}'),\
         (2,'-infinity','{-infinity}'),(3,'1 day','{1 day}')",
    )
    .unwrap();
    assert_eq!(
        one(&mut e, "SELECT iv::text FROM ivt WHERE id = 1"),
        "infinity"
    );
    assert_eq!(
        one(&mut e, "SELECT iv::text FROM ivt WHERE id = 2"),
        "-infinity"
    );
    assert_eq!(
        one(&mut e, "SELECT arr::text FROM ivt WHERE id = 1"),
        "{infinity,\"1 day\"}"
    );
    assert_eq!(
        one(&mut e, "SELECT (arr)[1]::text FROM ivt WHERE id = 1"),
        "infinity"
    );
    // The row the ORDER BY puts first is the one holding -infinity.
    assert_eq!(one(&mut e, "SELECT id FROM ivt ORDER BY iv"), "Int(2)");
    assert_eq!(
        one(&mut e, "SELECT min(iv)::text, max(iv)::text FROM ivt"),
        "-infinity|infinity"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM ivt WHERE isfinite(iv)"),
        "BigInt(1)"
    );
    assert_eq!(
        one(&mut e, "SELECT id FROM ivt WHERE iv = 'infinity'"),
        "Int(1)"
    );
    assert_eq!(
        one(&mut e, "SELECT id FROM ivt WHERE iv > '1 year'"),
        "Int(1)"
    );
}
