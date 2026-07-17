//! v7.39 (read01 round 69) — a user function on a JOIN's right side (LATERAL),
//! and `(f(args)).*`.
//!
//! LATERAL over a set-returning function is the whole point of LATERAL: the
//! call's ARGUMENTS reference the outer row, so it runs once per outer row.
//! Four things had to line up, and each was a separate silent failure:
//!
//!   1. the parser absorbed `LATERAL` only for four builtin SRF names;
//!   2. a call with an outer reference has to be marked CORRELATED, or it runs
//!      once with an unresolved column;
//!   3. the join's per-outer-row substitution reached `unnest_expr` and
//!      `generate_series_args` but NOT a table function's arguments;
//!   4. the lateral column-name probe saw `SELECT *` and answered `col0`, so
//!      `AS d` resolved to nothing.
//!
//! And one older bug fell out: the argument binder did not walk a body's UNION
//! peers, so `SELECT k UNION ALL SELECT k*10` left the second `k` unbound —
//! reported as "column not found: k".
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
        "CREATE FUNCTION rows_of(k int) RETURNS TABLE(id int, v text) AS $$ SELECT id, v FROM t WHERE id >= k ORDER BY id $$ LANGUAGE sql",
    );
    ok(
        &mut e,
        "CREATE FUNCTION dbl(k int) RETURNS SETOF int AS $$ SELECT k UNION ALL SELECT k*10 $$ LANGUAGE sql",
    );
    e
}

#[test]
fn a_join_lateral_runs_the_function_per_outer_row() {
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(t.id::text || ':' || d::text, ',') \
             FROM t JOIN LATERAL dbl(t.id) AS d ON true WHERE t.id <= 2"
        ),
        "1:1,1:10,2:2,2:20"
    );
}

#[test]
fn a_left_join_lateral_works_the_same() {
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(t.id::text || ':' || coalesce(d::text,'-'), ',') \
             FROM t LEFT JOIN LATERAL dbl(t.id) AS d ON true WHERE t.id = 1"
        ),
        "1:1,1:10"
    );
}

#[test]
fn the_comma_form_is_a_lateral_too() {
    // `FROM t, LATERAL f(t.c)` — and a RETURNS TABLE keeps its column names.
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(x.id::text || x.v, ',') \
             FROM t, LATERAL rows_of(t.id) AS x WHERE t.id = 3"
        ),
        "3c"
    );
}

#[test]
fn a_union_body_binds_its_arguments_in_every_peer() {
    // `SELECT k UNION ALL SELECT k*10` — the binder used to walk only the first
    // half, so the second `k` came back as "column not found".
    let mut e = seeded();
    assert_eq!(
        r1(&mut e, "SELECT string_agg(x::text, ',') FROM dbl(5) AS x"),
        "5,50"
    );
}

#[test]
fn record_expansion_gives_the_functions_columns() {
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(id::text || v, ',') FROM (SELECT (rows_of(2)).*) s"
        ),
        "2b,3c"
    );
}

#[test]
fn the_lateral_rows_obey_visibility() {
    let mut e = seeded();
    ok(&mut e, "DELETE FROM t WHERE id = 3");
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(x.id::text || x.v, ',') \
             FROM t, LATERAL rows_of(t.id) AS x WHERE t.id = 2"
        ),
        "2b"
    );
}
