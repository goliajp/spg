//! v7.39 (read01 utils/adt, date.c anchors) — make_date BC semantics
//! and infinity rendering, oracle-locked.

use spg_engine::{Engine, QueryResult};

fn text_of(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn make_date_bc_and_year_zero() {
    let mut e = Engine::new();
    // A negative year IS the BC year (not the astronomical year).
    assert_eq!(
        text_of(&mut e, "SELECT make_date(-44,3,15)"),
        "0044-03-15 BC"
    );
    // No year zero.
    let err = e.execute("SELECT make_date(0,1,1)").unwrap_err();
    assert!(
        format!("{err}").contains("date field value out of range"),
        "got {err}"
    );
}

#[test]
fn date_infinity_round_trips() {
    let mut e = Engine::new();
    assert_eq!(text_of(&mut e, "SELECT 'infinity'::date"), "infinity");
    assert_eq!(text_of(&mut e, "SELECT '-infinity'::date"), "-infinity");
}
