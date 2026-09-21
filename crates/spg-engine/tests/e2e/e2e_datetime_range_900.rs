//! 9.0.0 — the date/timestamp range and the three sentences around it.
//!
//! Measured on PostgreSQL 18.6:
//!
//! ```text
//!   '4714-11-24 BC'::date          4714-11-24 BC
//!   '4714-11-23 BC'::date          date out of range: "4714-11-23 BC"
//!   '5874897-12-31'::date          5874897-12-31
//!   '5874898-01-01'::date          date out of range: "5874898-01-01"
//!   '4714-11-23 BC'::timestamp     timestamp out of range: "…"
//!   '294277-01-01'::timestamp      timestamp out of range: "…"
//!   '2020-13-01'::date             date/time field value out of range
//!   'abc'::date                    invalid input syntax for type date
//! ```
//!
//! SPG had no range at all on either type — `'5874898-01-01'::date`
//! answered — and reported every failure with one of the other two
//! sentences. Which sentence is decided by PARSING the text with the
//! range lifted, not by reading its year: that is exactly the question
//! "would this be a date if the type were unbounded".
//!
//! RESIDUAL, recorded and decided: the timestamp UPPER bound is
//! `294247-01-10 04:00:54` here against PostgreSQL's `294276-12-31
//! 23:59:59`. SPG counts microseconds from 1970-01-01 and PostgreSQL
//! from 2000-01-01, so the same i64 reaches 30 years less far. Shifting
//! the epoch would touch ~700 sites that read a timestamp or a date,
//! plus a migration over every stored row, the WAL and the audit log —
//! and a single missed site is a silent 30-year error, which this
//! project holds to be the worst outcome there is. A loud refusal 292
//! millennia out is the better trade, and it now carries PostgreSQL's
//! own sentence.

use spg_engine::Engine;

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(spg_engine::QueryResult::Rows { rows, .. }) => rows
            .first()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .unwrap_or_default(),
        Ok(other) => format!("{other:?}"),
        Err(err) => format!("{err:?}"),
    }
}

#[test]
fn the_ends_postgresql_accepts_are_accepted() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT '4714-11-24 BC'::date"), "4714-11-24 BC");
    assert_eq!(one(&mut e, "SELECT '5874897-12-31'::date"), "5874897-12-31");
    assert_eq!(
        one(&mut e, "SELECT '4714-11-24 BC'::timestamp"),
        "4714-11-24 00:00:00 BC"
    );
    assert_eq!(one(&mut e, "SELECT '2020-01-02'::date"), "2020-01-02");
}

#[test]
fn one_day_outside_either_end_is_out_of_range() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT '4714-11-23 BC'::date", "date out of range"),
        ("SELECT '5874898-01-01'::date", "date out of range"),
        (
            "SELECT '4714-11-23 BC'::timestamp",
            "timestamp out of range",
        ),
        ("SELECT '294277-01-01'::timestamp", "timestamp out of range"),
    ] {
        let got = one(&mut e, sql);
        assert!(got.contains(want), "{sql}: {got}");
    }
}

#[test]
fn a_bad_field_and_bad_syntax_keep_their_own_sentences() {
    // The three sentences must stay apart: a FIELD out of range is not a
    // VALUE out of range, and neither is unparseable text.
    let mut e = Engine::new();
    let got = one(&mut e, "SELECT '2020-13-01'::date");
    assert!(got.contains("date/time field value out of range"), "{got}");
    let got = one(&mut e, "SELECT '2020-02-30'::date");
    assert!(got.contains("date/time field value out of range"), "{got}");
    let got = one(&mut e, "SELECT 'abc'::date");
    assert!(got.contains("invalid input syntax for type date"), "{got}");
}
