//! v7.39 (read01 round 102) — SQL-standard interval field qualifiers.
//!
//! `INTERVAL '2' YEAR`, `INTERVAL '1-6' YEAR TO MONTH`, `INTERVAL '1 2:03:04'
//! DAY TO SECOND` — the trailing `<FIELD> [TO <FIELD>]` says which unit a bare
//! number means and the leading/trailing precision. SPG's parser rejected the
//! qualifier outright (`expected ')' , got Ident("year")`). It now interprets
//! it: a single field truncates to its precision (SECOND keeps its fraction),
//! `YEAR TO MONTH` reads the `Y-M` form, and the colon-format ranges reuse the
//! existing interval-text parse. Values locked byte-identical against PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn single_field_qualifiers() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT (interval '2' year)::text"), "2 years");
    assert_eq!(text(&mut e, "SELECT (interval '3' month)::text"), "3 mons");
    assert_eq!(text(&mut e, "SELECT (interval '5' day)::text"), "5 days");
    assert_eq!(text(&mut e, "SELECT (interval '4' hour)::text"), "04:00:00");
    assert_eq!(
        text(&mut e, "SELECT (interval '30' minute)::text"),
        "00:30:00"
    );
    assert_eq!(
        text(&mut e, "SELECT (interval '100' second)::text"),
        "00:01:40"
    );
    assert_eq!(
        text(&mut e, "SELECT (interval '-2' year)::text"),
        "-2 years"
    );
}

#[test]
fn single_field_truncates_to_precision_except_second() {
    let mut e = Engine::new();
    // YEAR/HOUR/MINUTE truncate the fraction to the field's whole unit…
    assert_eq!(
        text(&mut e, "SELECT (interval '1.5' hour)::text"),
        "01:00:00"
    );
    assert_eq!(
        text(&mut e, "SELECT (interval '2.5' year)::text"),
        "2 years"
    );
    assert_eq!(
        text(&mut e, "SELECT (interval '1.5' minute)::text"),
        "00:01:00"
    );
    // …but SECOND (the finest unit) keeps the fractional part.
    assert_eq!(
        text(&mut e, "SELECT (interval '90.5' second)::text"),
        "00:01:30.5"
    );
}

#[test]
fn range_qualifiers() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT (interval '1-6' year to month)::text"),
        "1 year 6 mons"
    );
    assert_eq!(
        text(&mut e, "SELECT (interval '-1-6' year to month)::text"),
        "-1 years -6 mons"
    );
    assert_eq!(
        text(&mut e, "SELECT (interval '1 2:03:04' day to second)::text"),
        "1 day 02:03:04"
    );
    assert_eq!(
        text(&mut e, "SELECT (interval '2:03' hour to minute)::text"),
        "02:03:00"
    );
}

#[test]
fn units_bearing_literal_with_qualifier_falls_back() {
    // A literal that already carries its unit isn't a bare number, so the
    // qualifier just validates and the default parse stands.
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT (interval '2 days' day)::text"),
        "2 days"
    );
    // Bare interval (no qualifier) is unchanged — still seconds.
    assert_eq!(text(&mut e, "SELECT (interval '2')::text"), "00:00:02");
}
