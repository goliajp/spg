//! v7.39 (read01 round 68) — the UDF line's three residuals, closed:
//! `WITH ORDINALITY` on a user function, a multi-column function in the SELECT
//! list (it is a RECORD), and `RETURN QUERY EXECUTE`.
//!
//! The record one is the satisfying part: `Value::Composite` has existed since
//! round 56 (the composite-type epic) — `SELECT rows_of(2)` yielding `(2,b)` is
//! exactly what it is for. Round 67 had to error there ("call it in FROM"),
//! because the value did not get built. It does now.
//!
//! `RETURN QUERY EXECUTE` was the last place still taking the discard path that
//! round 66 killed for the static form: it ran the dynamic SQL and threw the
//! rows away.
//!
//! Byte-locked against live PG18.4, with in-place MVCC on.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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
fn with_ordinality_appends_a_counter() {
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(x::text || '#' || n::text, ',') FROM odds() WITH ORDINALITY AS a(x, n)"
        ),
        "1#1,3#2"
    );
}

#[test]
fn with_ordinality_on_a_returns_table_counts_after_its_columns() {
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(id::text || v || '#' || n::text, ',') \
             FROM rows_of(2) WITH ORDINALITY AS a(id, v, n)"
        ),
        "2b#1,3c#2"
    );
}

#[test]
fn a_multi_column_function_in_the_select_list_is_a_record() {
    // Value::Composite, from the round-56 composite epic. Round 67 errored here.
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(r::text, ',') FROM (SELECT rows_of(2) AS r) s"
        ),
        "(2,b),(3,c)"
    );
}

#[test]
fn return_query_execute_appends_its_rows() {
    let mut e = seeded();
    ok(
        &mut e,
        "CREATE FUNCTION dyn(k int) RETURNS SETOF int AS $$ \
         BEGIN RETURN QUERY EXECUTE 'SELECT id FROM t WHERE id >= ' || k::text || ' ORDER BY id'; END; \
         $$ LANGUAGE plpgsql",
    );
    assert_eq!(
        r1(&mut e, "SELECT string_agg(x::text, ',') FROM dyn(2) AS x"),
        "2,3"
    );
    // …and its rows obey visibility, like every body since round 63.
    ok(&mut e, "DELETE FROM t WHERE id = 2");
    assert_eq!(
        r1(&mut e, "SELECT string_agg(x::text, ',') FROM dyn(1) AS x"),
        "1,3"
    );
}
