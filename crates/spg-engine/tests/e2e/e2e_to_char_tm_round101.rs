//! v7.39 (read01 round 101) — `to_char` `TM` (translation-mode) prefix.
//!
//! `to_char(date, 'TMDay')` should emit the localized day name at its natural
//! width (`Monday`); SPG didn't recognise the `TM` modifier and emitted a
//! literal `TM` in front of the padded name (`TMMonday   `). SPG's locale is C,
//! where the localized names are the English ones the formatter already uses,
//! so `TM`'s observable effect is the natural-width trim — now applied.
//! Locked byte-identical against live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn tm_emits_name_at_natural_width() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT to_char(date '2024-01-01','TMDay')"),
        "Monday"
    );
    assert_eq!(
        text(&mut e, "SELECT to_char(date '2024-01-01','TMDy')"),
        "Mon"
    );
    assert_eq!(
        text(&mut e, "SELECT to_char(date '2024-03-05','TMMonth')"),
        "March"
    );
    assert_eq!(
        text(&mut e, "SELECT to_char(date '2024-03-05','TMMon')"),
        "Mar"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT to_char(date '2024-03-05','TMDay, TMMonth DD')"
        ),
        "Tuesday, March 05"
    );
}

#[test]
fn non_tm_still_blank_pads_and_numeric_unaffected() {
    // Regression guard: without TM a name field keeps its blank padding to the
    // longest member (9), and TM on a name field doesn't strip a following
    // numeric field's zero pad.
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT to_char(date '2024-03-05','Day')"),
        "Tuesday  "
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT to_char(timestamp '2024-03-05 14:00','TMDay HH24')"
        ),
        "Tuesday 14"
    );
}
