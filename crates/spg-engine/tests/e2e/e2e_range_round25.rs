//! v7.39 (read01 utils/adt, round 25) — rangetypes.c knives: misordered
//! bound rejection (constructor + text input), the `&<` / `&>`
//! operators, tstzrange offset I/O, and the malformed-literal wording.
//! Byte-locked vs PG18.

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
fn misordered_bounds_rejected() {
    let mut e = Engine::new();
    for sql in [
        "SELECT int4range(5, 1)",
        "SELECT int4range(5, 1, '(]')",
        "SELECT '[3,1]'::int4range",
        "SELECT numrange(2.5, 1.5)",
    ] {
        assert!(
            err_of(&mut e, sql)
                .contains("range lower bound must be less than or equal to range upper bound"),
            "{sql}"
        );
    }
    assert!(
        err_of(&mut e, "SELECT '[1,2'::int4range").contains("malformed range literal: \"[1,2\"")
    );
    // Equal bounds still collapse to empty (not an error).
    assert_eq!(row_of(&mut e, "SELECT int4range(3, 3)"), vec!["empty"]);
}

#[test]
fn overleft_overright_operators() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT int4range(1,5) &< int4range(3,8), int4range(3,8) &< int4range(1,5), \
             int4range(3,8) &> int4range(1,5), int4range(1,5) &> int4range(3,8)"
        ),
        vec!["true", "false", "true", "false"]
    );
    // Empty on either side is neither.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT 'empty'::int4range &< int4range(1,5), int4range(1,5) &> 'empty'::int4range"
        ),
        vec!["false", "false"]
    );
    // Equal upper bounds: &< holds.
    assert_eq!(
        row_of(&mut e, "SELECT int4range(1,5) &< int4range(3,5)"),
        vec!["true"]
    );
}

#[test]
fn tstzrange_offset_io() {
    let mut e = Engine::new();
    // Bounds render with the UTC offset suffix; a date+offset bound is
    // legal tstz input.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT tstzrange('2024-01-01 10:00+00', '2024-01-02 00:00+00')"
        ),
        vec!["[\"2024-01-01 10:00:00+00\",\"2024-01-02 00:00:00+00\")"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT tstzrange('2024-01-01 10:00+00', '2024-01-02+00')"
        ),
        vec!["[\"2024-01-01 10:00:00+00\",\"2024-01-02 00:00:00+00\")"]
    );
    // tsrange stays offset-free.
    assert_eq!(
        row_of(&mut e, "SELECT tsrange('2024-01-01 10:00', '2024-01-02')"),
        vec!["[\"2024-01-01 10:00:00\",\"2024-01-02 00:00:00\")"]
    );
}
