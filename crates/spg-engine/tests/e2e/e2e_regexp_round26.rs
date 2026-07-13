//! v7.39 (read01 utils/adt, round 26) — regexp.c knives: capture-group
//! semantics in regexp_match, the quantifier caps-rollback off-by-one,
//! regexp_replace's start/N shape, regexp_instr's subexpr argument, and
//! PG's parameter/compile error wordings. Byte-locked vs PG18.

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
fn match_returns_capture_groups() {
    let mut e = Engine::new();
    // With groups PG returns the groups, not the whole match.
    assert_eq!(
        row_of(&mut e, "SELECT regexp_match('foobarbaz', 'b(..)')"),
        vec!["{ar}"]
    );
    // A participating optional group must not report NULL (the
    // caps-rollback off-by-one dropped the final repetition's capture).
    assert_eq!(
        row_of(&mut e, "SELECT regexp_match('foo', 'o(o)?')"),
        vec!["{o}"]
    );
    assert_eq!(
        row_of(&mut e, "SELECT regexp_matches('foo', '(o)(o)?')"),
        vec!["{o,o}"]
    );
    // A genuinely non-participating group IS NULL.
    assert_eq!(
        row_of(&mut e, "SELECT regexp_match('fo', '(o)(o)?')"),
        vec!["{o,NULL}"]
    );
}

#[test]
fn replace_start_n_shape() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT regexp_replace('abcabc', 'b', 'X', 2), \
             regexp_replace('abcabc', 'b', 'X', 1, 2), \
             regexp_replace('AbcAbc', 'b', 'X', 1, 0, 'gi')"
        ),
        vec!["aXcabc", "abcaXc", "AXcAXc"]
    );
    // The 4-arg text form is still the flags overload.
    assert_eq!(
        row_of(&mut e, "SELECT regexp_replace('abcabc', 'b', 'X', 'g')"),
        vec!["aXcaXc"]
    );
    assert!(err_of(&mut e, "SELECT regexp_replace('abc', 'b', 'X', 0, 1)")
        .contains("invalid value for parameter \"start\": 0"));
}

#[test]
fn instr_subexpr_and_wordings() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT regexp_instr('abcdef', 'c(d)(e)', 1, 1, 0, '', 2), \
             regexp_instr('abcabc', 'b', 1, 1, 1)"
        ),
        vec!["5", "3"]
    );
    assert!(err_of(&mut e, "SELECT regexp_instr('abcdef', 'c', 0)")
        .contains("invalid value for parameter \"start\": 0"));
    assert!(err_of(&mut e, "SELECT 'abc' ~ '('")
        .contains("invalid regular expression: parentheses () not balanced"));
}
