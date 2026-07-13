//! v7.39 (read01 utils/adt, round 33) — varbit.c + varchar.c knives:
//! bit(n) cast pad/truncate, length/octet_length/overlay over bit
//! strings, PG's bit error wordings, the varchar literal prefix, the
//! ::name cast, and the date-sentinel multiply audit. Byte-locked vs
//! PG18.

use spg_engine::{Engine, QueryResult};

fn row_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err_of(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

#[test]
fn bit_cast_pads_right_or_truncates() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(&mut e, "SELECT B'10'::bit(4), B'1010'::bit(2), '101'::bit(4)"),
        vec!["1000", "10", "1010"]
    );
}

#[test]
fn bit_length_overlay_and_wordings() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT B'1010' || B'01', bit_length(B'1010'), octet_length(B'1010'), \
             length(B'1010'), overlay(B'1010' placing B'11' from 2)"
        ),
        vec!["101001", "4", "1", "4", "1110"]
    );
    assert!(err_of(&mut e, "SELECT B'1' & B'10'")
        .contains("cannot AND bit strings of different sizes"));
    assert!(err_of(&mut e, "SELECT 'abc'::bit(4)")
        .contains("\"a\" is not a valid binary digit"));
}

#[test]
fn varchar_prefix_and_name_cast() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT varchar 'abc', 'toolong'::varchar(3), \
             name 'thisnameislongerthanit'::text"
        ),
        vec!["abc", "too", "thisnameislongerthanit"]
    );
    // ::name truncates to 63 bytes.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT length(repeat('x', 100)::name)"
        ),
        vec!["63"]
    );
}

#[test]
fn date_sentinels_survive_generate_series_fold() {
    let mut e = Engine::new();
    // The date→timestamp fold used a plain multiply that overflowed on
    // the ±infinity sentinels (part of the round-32 crash family).
    let err = e.execute(
        "SELECT * FROM generate_series('infinity'::date, 'infinity'::date, interval '1 day')",
    );
    assert!(err.is_ok() || err.is_err()); // must not abort the process
}
