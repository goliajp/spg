//! v7.39 (read01 utils/adt, round 36) — varlena.c part 2: the bytea
//! deep-water functions — overlay(bytea), reverse(bytea), and PG's
//! byte/bit index-out-of-range wording. Byte-locked vs PG18.

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
fn bytea_overlay_and_reverse() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT overlay('\\x1234567890'::bytea placing '\\xffff'::bytea from 2 for 2), \
             reverse('\\x123456'::bytea)"
        ),
        vec!["\\x12ffff7890", "\\x563412"]
    );
}

#[test]
fn bytea_index_range_wordings() {
    let mut e = Engine::new();
    assert!(err_of(&mut e, "SELECT get_byte('\\x1234'::bytea, 9)")
        .contains("index 9 out of valid range, 0..1"));
    assert!(err_of(&mut e, "SELECT set_byte('\\x1234'::bytea, 9, 1)")
        .contains("index 9 out of valid range, 0..1"));
    assert!(err_of(&mut e, "SELECT get_bit('\\x12'::bytea, 99)")
        .contains("index 99 out of valid range, 0..7"));
}
