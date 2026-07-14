//! v7.39 (read01 round 67) — set-returning functions in the SELECT LIST, and
//! the lockstep rule when there is more than one.
//!
//! PG's ProjectSet semantics (PG10+): several SRFs in one target list run in
//! **LOCKSTEP, not as a cross product**. The output has as many rows as the
//! LONGEST of them, and the shorter ones are padded with NULLs:
//!
//!     SELECT generate_series(1,3), generate_series(10,11);
//!     1 | 10
//!     2 | 11
//!     3 |            ← NULL, not a second pass over 10,11
//!
//! SPG only ever handled ONE: the parser lifted the first SRF item into a FROM
//! item, and a second came back as "unknown function `generate_series`". A user
//! `RETURNS SETOF` function in the list was never handled at all.
//!
//! The lift now steps aside when the projection holds more than one function
//! call, and the engine expands the whole list together. Byte-locked against
//! live PG18.4, with in-place MVCC on.

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
        "CREATE FUNCTION odds() RETURNS SETOF int AS $$ SELECT id FROM t WHERE id % 2 = 1 ORDER BY id $$ LANGUAGE sql",
    );
    e
}

#[test]
fn two_srfs_run_in_lockstep_and_the_shorter_pads_with_nulls() {
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(coalesce(a::text,'-') || '/' || coalesce(b::text,'-'), ',') \
             FROM (SELECT generate_series(1,3) AS a, generate_series(10,11) AS b) s"
        ),
        "1/10,2/11,3/-"
    );
    // Different kinds zip the same way.
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(coalesce(u::text,'-') || '/' || coalesce(g::text,'-'), ',') \
             FROM (SELECT unnest(ARRAY[7,8]) AS u, generate_series(1,3) AS g) s"
        ),
        "7/1,8/2,-/3"
    );
}

#[test]
fn a_user_setof_function_expands_in_the_target_list() {
    let mut e = seeded();
    assert_eq!(
        r1(&mut e, "SELECT string_agg(x::text, ',') FROM (SELECT odds() AS x) s"),
        "1,3"
    );
}

#[test]
fn it_expands_per_input_row_when_there_is_a_from() {
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(id::text || ':' || o::text, ',') \
             FROM (SELECT id, odds() AS o FROM t WHERE id = 1) s"
        ),
        "1:1,1:3"
    );
}

#[test]
fn an_empty_set_contributes_no_rows() {
    // Not one NULL row — zero rows (PG).
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT count(*) FROM (SELECT unnest('{}'::int[]) AS x) s"
        ),
        "0"
    );
}

#[test]
fn the_functions_rows_obey_visibility() {
    let mut e = seeded();
    ok(&mut e, "DELETE FROM t WHERE id = 3");
    assert_eq!(
        r1(&mut e, "SELECT string_agg(x::text, ',') FROM (SELECT odds() AS x) s"),
        "1"
    );
}

#[test]
fn a_single_srf_still_works_the_way_it_did() {
    // The parser's lift still owns this shape; the change must not disturb it.
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(x::text, ',') FROM (SELECT generate_series(1,3) AS x) s"
        ),
        "1,2,3"
    );
}
