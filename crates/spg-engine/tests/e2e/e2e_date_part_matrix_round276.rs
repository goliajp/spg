//! v7.39 (round 276) — date_part()'s field/type matrix, the half that
//! needed the argument's static type.
//!
//! date_part() and EXTRACT do NOT share a matrix. Round 274 fixed the
//! DATE side by reproducing PG's promotion (a date becomes a timestamp
//! first, which is why PG's error for a DATE argument names TIMESTAMP).
//! This closes the other half: the timezone family must be REJECTED on
//! a plain timestamp and must still ANSWER on a timestamptz — and SPG
//! stores both in the same Value::Timestamp, so the judgement needs the
//! argument's declared type, exactly as the EXTRACT arm already does.
//!
//! Every expectation was read off live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from {sql}");
    };
    assert_eq!(rows.len(), 1, "{sql}");
    spg_engine::eval::value_to_text(&rows[0].values[0])
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
        Err(x) => format!("{x}").replace("eval: type mismatch: ", ""),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE dpt (ts timestamp, tz timestamptz, d date, t time)")
        .unwrap();
    e.execute(
        "INSERT INTO dpt VALUES ('2020-06-15 10:20:30', '2020-06-15 10:20:30+02', \
         '2020-06-15', '10:20:30')",
    )
    .unwrap();
    e
}

#[test]
fn the_timezone_family_is_rejected_on_a_plain_timestamp() {
    let mut e = fixture();
    // SPG answered 0 here — silently answering a question PG refuses.
    for unit in ["timezone", "timezone_hour", "timezone_minute"] {
        assert_eq!(
            err(&mut e, &format!("SELECT date_part('{unit}', ts) FROM dpt")),
            format!("unit \"{unit}\" not supported for type timestamp without time zone"),
        );
    }
    // A literal timestamp, with no column to read a declared type from,
    // is rejected too.
    assert_eq!(
        err(
            &mut e,
            "SELECT date_part('timezone', TIMESTAMP '2020-06-15 10:20:30')",
        ),
        "unit \"timezone\" not supported for type timestamp without time zone",
    );
}

#[test]
fn the_timezone_family_still_answers_on_a_timestamptz() {
    let mut e = fixture();
    // The distinction the value alone cannot make: SPG stores a
    // timestamptz in the same Value::Timestamp. A first attempt that
    // rejected on the value's shape broke exactly these three.
    for unit in ["timezone", "timezone_hour", "timezone_minute"] {
        assert_eq!(
            one(&mut e, &format!("SELECT date_part('{unit}', tz) FROM dpt")),
            "0",
            "{unit}",
        );
    }
}

#[test]
fn a_date_argument_reports_the_type_it_is_promoted_to() {
    let mut e = fixture();
    // PG names TIMESTAMP for a DATE argument — the tell that date_part
    // promotes a date to timestamp-at-midnight before doing anything.
    assert_eq!(
        err(&mut e, "SELECT date_part('timezone', d) FROM dpt"),
        "unit \"timezone\" not supported for type timestamp without time zone",
    );
    // And the same promotion is why the time-of-day fields ANSWER on a
    // date, where EXTRACT rejects them.
    assert_eq!(one(&mut e, "SELECT date_part('hour', d) FROM dpt"), "0");
}

#[test]
fn a_time_argument_reports_its_own_type() {
    let mut e = fixture();
    // TIME is not promoted, so PG names time here, not timestamp.
    assert_eq!(
        err(&mut e, "SELECT date_part('timezone', t) FROM dpt"),
        "unit \"timezone\" not supported for type time without time zone",
    );
}

#[test]
fn the_ordinary_fields_are_untouched() {
    let mut e = fixture();
    assert_eq!(one(&mut e, "SELECT date_part('year', ts) FROM dpt"), "2020");
    assert_eq!(one(&mut e, "SELECT date_part('hour', ts) FROM dpt"), "10");
    assert_eq!(one(&mut e, "SELECT date_part('month', d) FROM dpt"), "6");
}

#[test]
fn extract_keeps_its_own_matrix() {
    let mut e = fixture();
    // The two forms are pinned as DIFFERENT on purpose: EXTRACT rejects
    // the time-of-day fields on a date where date_part answers 0.
    assert!(
        e.execute("SELECT EXTRACT(HOUR FROM d) FROM dpt").is_err(),
        "EXTRACT(HOUR FROM date) must stay rejected",
    );
    assert_eq!(one(&mut e, "SELECT date_part('hour', d) FROM dpt"), "0");
}
