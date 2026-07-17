//! v7.39 (read01 round 108) — 2-D array concatenation (`||` and array_cat).
//!
//! `ARRAY[[1,2]] || ARRAY[[3,4]]` should append the second matrix's rows to
//! the first (`{{1,2},{3,4}}`). SPG had `||`/array_cat arms only for the 1-D
//! variants, so two matrices fell through to text concatenation — `||` gave the
//! malformed `{{1,2}}{{3,4}}` (typed `text`) and array_cat wrongly reported
//! "element types differ". Added the int/bigint/text/bool 2-D arms to both.
//! Locked byte-identical against live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn concat_operator_appends_matrix_rows() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT (ARRAY[[1,2]] || ARRAY[[3,4]])::text"),
        "{{1,2},{3,4}}"
    );
    assert_eq!(
        text(&mut e, "SELECT (ARRAY[[1,2],[3,4]] || ARRAY[[5,6]])::text"),
        "{{1,2},{3,4},{5,6}}"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT (ARRAY[['a','b']] || ARRAY[['c','d']])::text"
        ),
        "{{a,b},{c,d}}"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT (ARRAY[[true,false]] || ARRAY[[false,true]])::text"
        ),
        "{{t,f},{f,t}}"
    );
}

#[test]
fn array_cat_appends_matrix_rows() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT array_cat(ARRAY[[1,2]], ARRAY[[3,4]])::text"),
        "{{1,2},{3,4}}"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT array_cat(ARRAY[[1::bigint,2]], ARRAY[[3::bigint,4]])::text"
        ),
        "{{1,2},{3,4}}"
    );
}

#[test]
fn concat_keeps_the_array_type_and_dims() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT pg_typeof(ARRAY[[1,2]] || ARRAY[[3,4]])::text"
        ),
        "integer[]"
    );
    assert_eq!(
        text(&mut e, "SELECT array_dims(ARRAY[[1,2]] || ARRAY[[3,4]])"),
        "[1:2][1:2]"
    );
}

#[test]
fn one_dimensional_concat_unaffected() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT (ARRAY[1,2] || ARRAY[3,4])::text"),
        "{1,2,3,4}"
    );
    assert_eq!(text(&mut e, "SELECT (ARRAY[1,2] || 3)::text"), "{1,2,3}");
}
