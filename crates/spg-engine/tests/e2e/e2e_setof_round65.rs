//! v7.39 (read01 round 65) — set-returning functions: `RETURNS SETOF <type>`
//! and `RETURNS TABLE(…)`, called in FROM position.
//!
//! The UDF line's last hole. SPG could not even PARSE `RETURNS SETOF int`,
//! `RETURNS TABLE(id int, v text)`, or `FROM rows_of(2)` — the FROM-position
//! table-function surface existed but was gated to two builtin names.
//!
//! The body is a SELECT: the arguments bind into it as literals (the same
//! binder the scalar path uses since round 63, so an argument shadowed by a
//! column of the body's own FROM resolves the same way), and it runs through the
//! REAL executor. Visibility applies — the round-63 hazard again.
//!
//! Byte-locked against live PG18.4, with in-place MVCC on.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

fn r1(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE t (id int, v text)");
    ok(&mut e, "INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c')");
    ok(
        &mut e,
        "CREATE FUNCTION odds() RETURNS SETOF int AS $$ SELECT id FROM t WHERE id % 2 = 1 ORDER BY id $$ LANGUAGE sql",
    );
    ok(
        &mut e,
        "CREATE FUNCTION rows_of(k int) RETURNS TABLE(id int, v text) AS $$ SELECT id, v FROM t WHERE id >= k ORDER BY id $$ LANGUAGE sql",
    );
    e
}

#[test]
fn setof_scalar_yields_one_column_named_after_the_alias() {
    let mut e = seeded();
    assert_eq!(
        r1(&mut e, "SELECT string_agg(x::text, ',') FROM odds() AS x"),
        "1,3"
    );
    // With no alias, the column takes the function's name (PG).
    assert_eq!(
        r1(&mut e, "SELECT string_agg(odds::text, ',') FROM odds()"),
        "1,3"
    );
}

#[test]
fn returns_table_names_its_columns() {
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(id::text || v, ',') FROM rows_of(2)"
        ),
        "2b,3c"
    );
    assert_eq!(r1(&mut e, "SELECT count(*) FROM rows_of(1)"), "3");
}

#[test]
fn the_enclosing_query_may_filter_the_functions_rows() {
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(v, ',') FROM rows_of(2) WHERE id = 3"
        ),
        "c"
    );
}

#[test]
fn only_visible_rows_reach_the_caller() {
    // The body runs through the read path, so a deleted row is gone for it too.
    let mut e = seeded();
    ok(&mut e, "DELETE FROM t WHERE id = 2");
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(id::text || v, ',') FROM rows_of(1)"
        ),
        "1a,3c"
    );
    assert_eq!(
        r1(&mut e, "SELECT string_agg(x::text, ',') FROM odds() AS x"),
        "1,3"
    );
}

#[test]
fn a_scalar_function_in_from_is_refused() {
    let mut e = seeded();
    ok(
        &mut e,
        "CREATE FUNCTION scalar_one(x int) RETURNS int AS $$ SELECT x + 1 $$ LANGUAGE sql",
    );
    let msg = err(&mut e, "SELECT * FROM scalar_one(1)");
    assert!(msg.contains("does not return a set"), "{msg}");
}
