//! v7.39 (read01 round 63) — a user function whose body has its own FROM.
//!
//! Round 62 left this as the last hole, with a warning attached: the tempting
//! fix is to scan `ctx.catalog`'s rows straight from eval, since the catalog is
//! right there in the context. DON'T. Those are the RAW rows — reading them
//! bypasses the row-header visibility filter, so under in-place MVCC a function
//! body would happily read DEAD rows.
//!
//! So the body runs through the REAL executor: `EvalContext` now carries the
//! engine, the arguments are substituted into the body as literals, and the
//! SELECT goes down the ordinary read path. It sees exactly what a hand-written
//! query would see. `visible_rows_only` below is the test that would have caught
//! the shortcut.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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
        "CREATE FUNCTION lookup(k int) RETURNS text AS $$ SELECT v FROM t WHERE id = k $$ LANGUAGE sql",
    );
    ok(
        &mut e,
        "CREATE FUNCTION cnt() RETURNS bigint AS $$ SELECT count(*) FROM t $$ LANGUAGE sql",
    );
    e
}

#[test]
fn a_body_with_a_from_is_a_real_query() {
    let mut e = seeded();
    assert_eq!(r1(&mut e, "SELECT lookup(2)"), "b");
    // The body may aggregate.
    assert_eq!(r1(&mut e, "SELECT cnt()"), "3");
}

#[test]
fn no_matching_row_is_null_not_an_error() {
    let mut e = seeded();
    assert_eq!(r1(&mut e, "SELECT coalesce(lookup(9),'NULL')"), "NULL");
}

#[test]
fn it_works_per_row_and_inside_an_aggregate() {
    let mut e = seeded();
    assert_eq!(
        r1(&mut e, "SELECT string_agg(lookup(id), ',' ORDER BY id) FROM t"),
        "a,b,c"
    );
}

#[test]
fn visible_rows_only() {
    // THE test. A deleted row must be invisible to the function body, exactly
    // as it is to a hand-written query. Reading the catalog's raw rows from
    // eval would have returned 'b' here — and 3 for the count — under in-place
    // MVCC, where a delete only stamps the row header.
    let mut e = seeded();
    ok(&mut e, "DELETE FROM t WHERE id = 2");
    assert_eq!(r1(&mut e, "SELECT coalesce(lookup(2),'NULL')"), "NULL");
    assert_eq!(r1(&mut e, "SELECT cnt()"), "2");
}

#[test]
fn a_column_of_the_bodys_own_from_shadows_an_argument() {
    // PG resolves `v` inside the body to the TABLE's column, not to the
    // same-named argument. The substitution has to skip it.
    let mut e = seeded();
    ok(&mut e, "CREATE TABLE s (id int, v text)");
    ok(&mut e, "INSERT INTO s VALUES (1,'x')");
    ok(
        &mut e,
        "CREATE FUNCTION shadow(v text) RETURNS text AS $$ SELECT v FROM s WHERE id = 1 $$ LANGUAGE sql",
    );
    assert_eq!(r1(&mut e, "SELECT shadow('arg')"), "x");
}

#[test]
fn a_query_body_may_call_another_function() {
    let mut e = seeded();
    ok(
        &mut e,
        "CREATE FUNCTION shout(k int) RETURNS text AS $$ SELECT upper(lookup(k)) $$ LANGUAGE sql",
    );
    assert_eq!(r1(&mut e, "SELECT shout(3)"), "C");
}
