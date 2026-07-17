//! v7.39 (IS-precedence knife) — PG binds the IS family looser than
//! EVERY binary operator (only NOT/AND/OR are looser), and BETWEEN/IN/
//! LIKE at the comparison rung: `1 + 1 IS NULL` is `(1+1) IS NULL`.
//! SPG previously parsed all of these as tight postfixes
//! (`1 + (1 IS NULL)`), silently changing results. All truths
//! differential-locked against PG18.

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

#[test]
fn is_binds_looser_than_every_operator() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT 1 + 1 IS NULL, 1 = 1 IS NULL, 'a' || 'b' IS NULL, 5 > 3 IS TRUE"
        ),
        vec!["false", "false", "false", "true"]
    );
    // ... but tighter than NOT / AND / OR.
    assert_eq!(
        row_of(&mut e, "SELECT NOT 1 IS NULL, true AND false IS NULL"),
        vec!["true", "false"]
    );
    // IS chains left-to-right: (1 IS NULL) = false, then false IS NULL.
    assert_eq!(row_of(&mut e, "SELECT 1 IS NULL IS NULL"), vec!["false"]);
}

#[test]
fn is_distinct_from_takes_arithmetic_rhs() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT 1 + 1 IS DISTINCT FROM 2, 2 * 3 IS DISTINCT FROM 6, \
             3 IS DISTINCT FROM 1 + 1"
        ),
        vec!["false", "false", "true"]
    );
}

#[test]
fn between_in_like_sit_on_the_comparison_rung() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT 1 + 1 IN (2, 5), 1 + 1 BETWEEN 1 AND 3, 'ab' || 'c' LIKE 'ab%', \
             1 BETWEEN 0 AND 2 IS TRUE, 1 IN (1,2) IS TRUE"
        ),
        vec!["true", "true", "true", "true", "true"]
    );
}

#[test]
fn tuple_is_null_still_field_wise() {
    let mut e = Engine::new();
    // The ROW/tuple desugar keeps its dedicated path.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT (2, NULL) IS NULL, (CAST(NULL AS int), NULL) IS NULL, NULL::int IS NOT NULL"
        ),
        vec!["false", "true", "false"]
    );
}
