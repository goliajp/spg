//! 9.0.0 — arithmetic on an infinite date or timestamp.
//!
//! Recorded delta RD-2 said an interval infinity was not representable
//! here. `IntervalKind::{PosInf, NegInf}` has carried one since it was
//! added, so the reading was stale and the answer was an error:
//!
//! ```text
//!                                              PG 18.6   SPG 8.0.4
//!   'infinity'::ts - '-infinity'::ts           infinity  ERROR: interval out of range
//!   'infinity'::ts - '2020-01-01'::ts          infinity  ERROR
//!   'infinity'::date - '2020-01-01'::date      ERROR     2147465385
//!   'infinity'::date + 1                       infinity  ERROR: DATE + integer overflows
//!   'infinity'::date - '1 day'::interval       infinity  ERROR: DATE → TIMESTAMP lift overflows
//! ```
//!
//! Every expectation below is PostgreSQL 18.6's.

use spg_engine::{Engine, QueryResult};

fn answer(e: &mut Engine, sql: &str) -> Result<String, String> {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => Ok(rows
            .first()
            .and_then(|r| r.values.first())
            .map(spg_engine::eval::value_to_text)
            .unwrap_or_default()),
        Ok(other) => panic!("{sql}: {other:?}"),
        Err(e) => Err(format!("{e}")),
    }
}

#[test]
fn subtracting_timestamps_where_one_is_infinite_answers_that_infinity() {
    let mut e = Engine::new();
    for (sql, want) in [
        (
            "SELECT 'infinity'::timestamp - '-infinity'::timestamp",
            "infinity",
        ),
        (
            "SELECT '-infinity'::timestamp - 'infinity'::timestamp",
            "-infinity",
        ),
        (
            "SELECT 'infinity'::timestamp - '2020-01-01'::timestamp",
            "infinity",
        ),
        (
            "SELECT '2020-01-01'::timestamp - 'infinity'::timestamp",
            "-infinity",
        ),
        (
            "SELECT 'infinity'::timestamptz - '2020-01-01'::timestamptz",
            "infinity",
        ),
    ] {
        assert_eq!(
            answer(&mut e, sql).unwrap_or_else(|e| panic!("{sql}: {e}")),
            want,
            "{sql}"
        );
    }
}

/// Two infinities of the same sign have no difference, and PG says so.
#[test]
fn subtracting_like_signed_infinities_is_out_of_range() {
    let mut e = Engine::new();
    for sql in [
        "SELECT 'infinity'::timestamp - 'infinity'::timestamp",
        "SELECT '-infinity'::timestamp - '-infinity'::timestamp",
    ] {
        let err = answer(&mut e, sql).expect_err(sql);
        assert!(err.contains("interval out of range"), "{sql}: {err}");
    }
}

#[test]
fn an_infinite_date_cannot_be_subtracted_and_stays_infinite_otherwise() {
    let mut e = Engine::new();
    for sql in [
        "SELECT 'infinity'::date - '2020-01-01'::date",
        "SELECT 'infinity'::date - 'infinity'::date",
        "SELECT '2020-01-01'::date - 'infinity'::date",
    ] {
        let err = answer(&mut e, sql).expect_err(sql);
        assert!(
            err.contains("cannot subtract infinite dates"),
            "{sql}: {err}"
        );
    }
    for (sql, want) in [
        ("SELECT 'infinity'::date + 1", "infinity"),
        ("SELECT 'infinity'::date - 1", "infinity"),
        ("SELECT '-infinity'::date + 1", "-infinity"),
        ("SELECT 'infinity'::date - '1 day'::interval", "infinity"),
        ("SELECT '-infinity'::date + '1 day'::interval", "-infinity"),
    ] {
        assert_eq!(
            answer(&mut e, sql).unwrap_or_else(|e| panic!("{sql}: {e}")),
            want,
            "{sql}"
        );
    }
}

/// A finite subtraction is untouched.
#[test]
fn finite_dates_and_timestamps_still_subtract() {
    let mut e = Engine::new();
    assert_eq!(
        answer(
            &mut e,
            "SELECT '2020-01-01'::timestamp - '2019-12-30'::timestamp"
        )
        .unwrap(),
        "2 days"
    );
    assert_eq!(
        answer(&mut e, "SELECT '2020-01-01'::date - '2019-12-30'::date").unwrap(),
        "2"
    );
    assert_eq!(
        answer(&mut e, "SELECT '2020-01-01'::date + 1").unwrap(),
        "2020-01-02"
    );
}
