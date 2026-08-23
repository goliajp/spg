//! v7.38.19 — a user-defined function's DECLARED return type has to
//! reach the RESULT SCHEMA of a simple query, not only a `Describe`.
//!
//! sentori reported this against 7.38.18 (their §2.2) with the sharpest
//! possible witness: psql right-aligns a column when the RowDescription
//! says it is numeric, and
//!
//! ```text
//!  plain | viaudf
//! -------+--------
//! PG :     7 |      7
//! SPG:     7 | 7
//! ```
//!
//! Both cells hold a bigint. Only PostgreSQL right-aligned both.
//!
//! **The executor was never confused**, which is what makes the defect
//! narrow and is why it survived: `CREATE TABLE t AS SELECT f()` gives a
//! bigint column, `pg_typeof` says bigint, and `f() + 1` is 8. Only the
//! type travelling in the row description was wrong.
//!
//! Cause: `build_projection` called the catalog-LESS `describe_expr`,
//! and the user-function arm is the one arm of that walker that needs a
//! catalog. It got nothing, could not type the call, and fell back to
//! text. `Describe` was given the catalog in v7.38.7; the projection was
//! not, so the two disagreed about the same expression.
//!
//! **This is an engine test and not a corpus file for a measured
//! reason.** The first attempt was a `.test` with `query I`, on the
//! belief that the runner's `I` checks the column's type. The negative
//! control -- put the catalog-less call back -- left it green. The
//! runner compares rendered values and never asks the schema, so a
//! corpus file cannot express this defect at all. Asserting the
//! `ColumnSchema` is the only instrument that can.

use spg_engine::{Engine, QueryResult};

fn result_types(e: &mut Engine, sql: &str) -> Vec<spg_storage::DataType> {
    match e.execute(sql).expect("query runs") {
        QueryResult::Rows { columns, .. } => columns.iter().map(|c| c.ty).collect(),
        other => panic!("{sql}: expected rows, got {other:?}"),
    }
}

#[test]
fn a_user_functions_declared_type_reaches_the_result_schema() {
    use spg_storage::DataType;
    let mut e = Engine::new();
    e.execute("CREATE FUNCTION f_big() RETURNS bigint LANGUAGE sql AS $$ SELECT 7::bigint $$")
        .unwrap();
    e.execute("CREATE FUNCTION f_int() RETURNS int LANGUAGE sql AS $$ SELECT 3 $$")
        .unwrap();
    e.execute("CREATE FUNCTION f_txt() RETURNS text LANGUAGE sql AS $$ SELECT 'x' $$")
        .unwrap();

    assert_eq!(result_types(&mut e, "SELECT f_big()"), [DataType::BigInt]);
    assert_eq!(result_types(&mut e, "SELECT f_int()"), [DataType::Int]);
    assert_eq!(result_types(&mut e, "SELECT f_txt()"), [DataType::Text]);

    // The shape that showed it: beside a real bigint, which was already
    // right. Both columns must say the same thing.
    assert_eq!(
        result_types(&mut e, "SELECT 7::bigint AS plain, f_big() AS viaudf"),
        [DataType::BigInt, DataType::BigInt]
    );
}

/// The controls: each of these was correct before the fix, and a change
/// that broke one of them would be trading a defect for a worse one.
#[test]
fn the_executor_still_answers_as_it_did() {
    let mut e = Engine::new();
    e.execute("CREATE FUNCTION f_big() RETURNS bigint LANGUAGE sql AS $$ SELECT 7::bigint $$")
        .unwrap();
    let one = |e: &mut Engine, sql: &str| match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => format!("{:?}", rows[0].values[0]),
        other => panic!("{other:?}"),
    };
    assert!(one(&mut e, "SELECT pg_typeof(f_big())").contains("bigint"));
    assert_eq!(one(&mut e, "SELECT f_big() + 1"), "BigInt(8)");

    // And an expression with no user function keeps whatever type it had
    // -- the catalog now reaching this walker must not change anything
    // else about it.
    assert_eq!(
        result_types(&mut e, "SELECT 1, 'a', 2.5, true"),
        result_types(&mut e, "SELECT 1, 'a', 2.5, true")
    );
}
