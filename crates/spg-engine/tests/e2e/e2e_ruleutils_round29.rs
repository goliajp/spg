//! v7.39 (read01 utils/adt, round 29) — ruleutils.c part 1:
//! pg_get_viewdef's multi-line shape (+ pretty parens), the partial-index
//! WHERE in pg_get_indexdef, and pg_get_serial_sequence's real contract.
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

#[test]
fn viewdef_multiline_shape() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ru_t (id int, v text)").unwrap();
    e.execute("CREATE VIEW ru_v AS SELECT id, v FROM ru_t WHERE id > 1")
        .unwrap();
    assert_eq!(
        row_of(&mut e, "SELECT pg_get_viewdef('ru_v')"),
        vec![" SELECT id,\n    v\n   FROM ru_t\n  WHERE (id > 1);"]
    );
    // Pretty mode drops the redundant top-level WHERE parens.
    assert_eq!(
        row_of(&mut e, "SELECT pg_get_viewdef('ru_v', true)"),
        vec![" SELECT id,\n    v\n   FROM ru_t\n  WHERE id > 1;"]
    );
}

#[test]
fn indexdef_carries_partial_predicate() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ru_i (k int)").unwrap();
    e.execute("CREATE UNIQUE INDEX ru_uidx ON ru_i (k) WHERE k > 5")
        .unwrap();
    assert_eq!(
        row_of(&mut e, "SELECT pg_get_indexdef('ru_uidx'::regclass)"),
        vec!["CREATE UNIQUE INDEX ru_uidx ON public.ru_i USING btree (k) WHERE (k > 5)"]
    );
}
