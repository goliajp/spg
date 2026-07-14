//! v7.39 (read01 round 74) — `(f(args)).*` anywhere, and `ROWS FROM` with the
//! SRFs that have no array form.
//!
//! Two residuals from rounds 68/69, and both were closed by REUSING machinery
//! this campaign already built rather than growing a second copy of it:
//!
//!   - `(f(args)).*` beside other items, or over a FROM, lowers to a LATERAL of
//!     the same function plus one item per declared column. `SELECT 'p',
//!     (rows_of(2)).*` IS `SELECT 'p', __rec.id, __rec.v FROM rows_of(2) AS
//!     __rec`. Naming the record's fields takes the catalog, which is why the
//!     lowering lives in the engine and the parser only leaves a marker.
//!   - `ROWS FROM (f(a), g(b))` accepted only SRFs with a scalar ARRAY form (it
//!     lowered them into the unnest-zip channel) — so `generate_series` and every
//!     user function were rejected. They now ride a generic channel that RUNS each
//!     function and zips the rows in LOCKSTEP, padding the shorter with NULLs —
//!     the same rule round 67 established for SRFs in the target list, evaluated
//!     by the same `srf_values`.
//!
//! Byte-locked against live PG18.4, with in-place MVCC on.

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
        "CREATE FUNCTION rows_of(k int) RETURNS TABLE(id int, v text) AS $$ SELECT id, v FROM t WHERE id >= k ORDER BY id $$ LANGUAGE sql",
    );
    ok(
        &mut e,
        "CREATE FUNCTION dbl(k int) RETURNS SETOF int AS $$ SELECT k UNION ALL SELECT k*10 $$ LANGUAGE sql",
    );
    e
}

#[test]
fn record_expansion_beside_other_select_items() {
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(x || ':' || id::text || v, ',') \
             FROM (SELECT 'p' AS x, (rows_of(2)).*) s"
        ),
        "p:2b,p:3c"
    );
}

#[test]
fn rows_from_runs_srfs_that_have_no_array_form() {
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(coalesce(a::text,'-') || '/' || coalesce(b::text,'-'), ',') \
             FROM ROWS FROM (dbl(1), dbl(2)) AS z(a,b)"
        ),
        "1/2,10/20"
    );
}

#[test]
fn rows_from_zips_in_lockstep_and_pads_the_shorter() {
    // Same rule as two SRFs in a target list (round 67): max(len) rows, NULL pad.
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(coalesce(a::text,'-') || '/' || coalesce(b::text,'-'), ',') \
             FROM ROWS FROM (generate_series(1,3), dbl(2)) AS z(a,b)"
        ),
        "1/2,2/20,3/-"
    );
}

#[test]
fn rows_from_still_serves_the_array_srfs() {
    // The all-array case keeps the old, well-trodden unnest-zip lowering.
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(coalesce(a::text,'-') || '/' || coalesce(b,'-'), ',') \
             FROM ROWS FROM (unnest(ARRAY[1,2]), unnest(ARRAY['x'])) AS z(a,b)"
        ),
        "1/x,2/-"
    );
}

#[test]
fn rows_from_takes_with_ordinality() {
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(a::text || '#' || n::text, ',') \
             FROM ROWS FROM (dbl(1)) WITH ORDINALITY AS z(a,n)"
        ),
        "1#1,10#2"
    );
}
