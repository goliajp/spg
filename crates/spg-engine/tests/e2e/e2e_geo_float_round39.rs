//! v7.39 (read01 utils/adt, round 39) — geo constructors point(box) /
//! line(point,point), and float→integer coercion (int4()/int8()/int2()
//! and INSERT) rounding + range errors. Byte-locked vs PG18.

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
fn geo_single_arg_constructors() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT point(box '((0,0),(4,4))'), point(circle '<(1,1),3>'), \
             point(lseg '[(0,0),(4,2)]')"
        ),
        vec!["(2,2)", "(1,1)", "(2,1)"]
    );
    assert_eq!(
        row_of(&mut e, "SELECT line(point '(0,0)', point '(1,1)')"),
        vec!["{1,-1,0}"]
    );
    assert!(err_of(&mut e, "SELECT line(point '(1,1)', point '(1,1)')")
        .contains("cannot create line from two identical points"));
}

#[test]
fn float_to_integer_coercion() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(&mut e, "SELECT int4(3.7::float8), int4(2.5::float8), int8(-2.5::float8)"),
        vec!["4", "2", "-2"]
    );
    assert!(err_of(&mut e, "SELECT int4('inf'::float8)")
        .contains("integer out of range"));
    assert!(err_of(&mut e, "SELECT int4(1e20::float8)")
        .contains("integer out of range"));
    assert!(err_of(&mut e, "SELECT int2(1e6::float8)")
        .contains("smallint out of range"));
}
