//! v7.39 (read01 round 66) — plpgsql set-returning bodies: `RETURN NEXT` and
//! `RETURN QUERY`.
//!
//! Round 65 made `RETURNS SETOF` work for a `LANGUAGE sql` body. Most real
//! set-returning functions are plpgsql, and both of the statements they build
//! their rows with were broken in a way worth spelling out:
//!
//!   - `RETURN NEXT <expr>` did not parse at all. Its comment said "queues with
//!     v7.40 SETOF function infrastructure".
//!   - `RETURN QUERY <select>` DID parse — and desugared to an embedded
//!     side-effect SELECT **whose rows were discarded**. In a set-returning
//!     function that is the entire answer, thrown away.
//!
//! Both are real statements now: they append to the set the function is
//! building and KEEP GOING (neither is a return). The sink only exists inside a
//! set-returning call, so using either one anywhere else is an error — as in PG.
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
        "CREATE FUNCTION pl_next(n int) RETURNS SETOF int AS $$ \
         BEGIN FOR i IN 1..n LOOP RETURN NEXT i * 10; END LOOP; END; $$ LANGUAGE plpgsql",
    );
    ok(
        &mut e,
        "CREATE FUNCTION pl_query(k int) RETURNS SETOF int AS $$ \
         BEGIN RETURN QUERY SELECT id FROM t WHERE id >= k ORDER BY id; END; $$ LANGUAGE plpgsql",
    );
    e
}

#[test]
fn return_next_accumulates_a_row_at_a_time() {
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(x::text, ',') FROM pl_next(3) AS x"
        ),
        "10,20,30"
    );
    // Zero iterations = the empty set, not an error.
    assert_eq!(r1(&mut e, "SELECT count(*) FROM pl_next(0)"), "0");
}

#[test]
fn return_query_appends_the_querys_rows() {
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(x::text, ',') FROM pl_query(2) AS x"
        ),
        "2,3"
    );
    assert_eq!(r1(&mut e, "SELECT count(*) FROM pl_query(1)"), "3");
}

#[test]
fn return_query_sees_only_visible_rows() {
    // It runs through the read path, like every other body since round 63.
    let mut e = seeded();
    ok(&mut e, "DELETE FROM t WHERE id = 2");
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(x::text, ',') FROM pl_query(1) AS x"
        ),
        "1,3"
    );
}

#[test]
fn a_plpgsql_returns_table_names_its_columns() {
    let mut e = seeded();
    ok(
        &mut e,
        "CREATE FUNCTION pl_table(k int) RETURNS TABLE(id int, v text) AS $$ \
         BEGIN RETURN QUERY SELECT t.id, t.v FROM t WHERE t.id >= k ORDER BY t.id; END; $$ LANGUAGE plpgsql",
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(id::text || v, ',') FROM pl_table(2)"
        ),
        "2b,3c"
    );
}

#[test]
fn return_next_outside_a_set_returning_function_is_an_error() {
    // The sink is what makes the statement legal, and a scalar call has none.
    let mut e = seeded();
    ok(
        &mut e,
        "CREATE FUNCTION scalar_next(x int) RETURNS int AS $$ \
         BEGIN RETURN NEXT x; RETURN x; END; $$ LANGUAGE plpgsql",
    );
    let msg = err(&mut e, "SELECT scalar_next(1)");
    assert!(
        msg.contains("cannot use RETURN NEXT in a non-SETOF function"),
        "{msg}"
    );
}
