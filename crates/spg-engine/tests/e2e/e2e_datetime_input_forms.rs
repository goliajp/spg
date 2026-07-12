//! v7.39 (read01 utils/adt, datetime.c anchors) — input-parser forms,
//! every expected value the live PG18 oracle's output.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    Engine::new().with_clock(|| 1_700_000_000_000_000) // 2023-11-14 22:13:20 UTC
}

fn text_of(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => {
            spg_engine::eval::value_to_text(&rows[0].values[0])
        }
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn datetime_input_forms_match_pg() {
    let mut e = engine();
    // Text month with a two-digit year disambiguates by DateOrder
    // (default MDY: 23 = day, 24 = year 2024).
    assert_eq!(text_of(&mut e, "SELECT 'Jan-23-24'::date"), "2024-01-23");
    // Day-of-year ordinal form.
    assert_eq!(text_of(&mut e, "SELECT '2024-060'::date"), "2024-02-29");
    // A 3+-digit first field is unambiguously the year.
    assert_eq!(text_of(&mut e, "SELECT '123-4-5'::date"), "0123-04-05");
    // Julian day number (JD 2451545 = 2000-01-01).
    assert_eq!(text_of(&mut e, "SELECT 'J2451545'::date"), "2000-01-01");
    // Relative reserved words resolve against the injected clock.
    assert_eq!(text_of(&mut e, "SELECT 'today'::date"), "2023-11-14");
    assert_eq!(text_of(&mut e, "SELECT 'yesterday'::date"), "2023-11-13");
    assert_eq!(text_of(&mut e, "SELECT 'tomorrow'::date"), "2023-11-15");
    assert_eq!(
        text_of(&mut e, "SELECT 'now'::timestamp"),
        "2023-11-14 22:13:20"
    );
}
