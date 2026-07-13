//! v7.39 (read01 utils/adt, round 34) — varlena.c part 1: split_part's
//! empty-delimiter rule + PG wordings, the text operator support
//! functions by name, and the text literal prefix. Byte-locked vs PG18.

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
fn split_part_contract() {
    let mut e = Engine::new();
    // Empty delimiter: the whole string is field 1 / -1; others ''.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT split_part('a,b,c', '', 1), split_part('a,b,c', '', -1), \
             split_part('a,b,c', '', 2)"
        ),
        vec!["a,b,c", "a,b,c", ""]
    );
    assert!(err_of(&mut e, "SELECT split_part('a,b,c', ',', 0)")
        .contains("field position must not be zero"));
}

#[test]
fn text_support_functions_and_prefix() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT text_le('a', 'b'), texteq('a', 'a'), textcat('a', 'b'), \
             text 'a' > text 'B'"
        ),
        vec!["true", "true", "ab", "true"]
    );
}
