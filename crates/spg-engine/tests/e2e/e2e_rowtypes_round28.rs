//! v7.39 (read01 utils/adt, round 28) — rowtypes.c: the composite-type
//! text cast (record_in), field re-labeling from ROW values, and the
//! runtime composite comparison. Byte-locked vs PG18.

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
fn composite_text_cast_and_field_access() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE rt_pair AS (a int, b text)").unwrap();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT '(1,hello)'::rt_pair, '(2,\"with space\")'::rt_pair, '(3,)'::rt_pair"
        ),
        vec!["(1,hello)", "(2,\"with space\")", "(3,)"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT ('(1,hello)'::rt_pair).a, ('(1,hello)'::rt_pair).b"
        ),
        vec!["1", "hello"]
    );
    // ROW value re-labels into the composite; runtime comparison works.
    assert_eq!(
        row_of(&mut e, "SELECT ROW(1,'x')::rt_pair = '(1,x)'::rt_pair"),
        vec!["true"]
    );
    let err = e.execute("SELECT '(1,hello'::rt_pair").unwrap_err();
    assert!(
        format!("{err}").contains("malformed record literal"),
        "{err}"
    );
}
