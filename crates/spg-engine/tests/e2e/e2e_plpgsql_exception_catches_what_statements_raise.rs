//! 8.0.3 — an `EXCEPTION` clause catches what the block's statements
//! and expressions raise, not only `RAISE`.
//!
//! Reported by sentori: a unique violation inside
//! `DO $$ BEGIN INSERT … EXCEPTION WHEN OTHERS THEN … END $$` escaped
//! the handler. It was wider than that. The block's writes did not run
//! while the block ran — they were collected and executed after the
//! walk, outside the clause — and a condition was matched against the
//! RAISE message by substring, so nothing but a RAISE could ever be
//! caught: not a unique violation, not `division_by_zero`, not a NOT
//! NULL. A write now runs where it is written, a condition is matched by
//! SQLSTATE, and a handled error rolls the block back to where it began.
//!
//! Every expectation is PostgreSQL 18.6's for the same block.

use spg_engine::{Engine, QueryResult};

fn boot() -> Engine {
    let mut e = Engine::new();
    for s in [
        "CREATE TABLE ex2 (k int UNIQUE, n int)",
        "INSERT INTO ex2 VALUES (1, 1)",
    ] {
        e.execute(s).unwrap();
    }
    e
}

fn rows(e: &mut Engine) -> String {
    match e
        .execute("SELECT string_agg(k || ':' || n, ',' ORDER BY k) FROM ex2")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    }
}

/// A local set inside the handler, read back through a table so the pin
/// sees what the handler saw.
fn handler_saw(e: &mut Engine, block_body: &str, handler: &str) -> String {
    e.execute("CREATE TABLE IF NOT EXISTS seen (v text)")
        .unwrap();
    e.execute("DELETE FROM seen").unwrap();
    e.execute(&format!(
        "DO $$ DECLARE v int := 0; BEGIN {block_body} EXCEPTION {handler} END $$"
    ))
    .unwrap_or_else(|x| panic!("{block_body} / {handler}: {x}"));
    match e
        .execute("SELECT coalesce(string_agg(v, '|'), '') FROM seen")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_unique_violation_is_caught_and_the_block_is_rolled_back() {
    let mut e = boot();
    e.execute(
        "DO $$ BEGIN INSERT INTO ex2 VALUES (2, 2); INSERT INTO ex2 VALUES (1, 9); \
         EXCEPTION WHEN unique_violation THEN INSERT INTO ex2 VALUES (3, 3); END $$",
    )
    .expect("the handler catches 23505");
    assert_eq!(
        rows(&mut e),
        "1:1,3:3",
        "(2,2) was written before the error and goes with the block; the handler's (3,3) stays"
    );
}

#[test]
fn sqlstate_and_sqlerrm_are_the_errors_own() {
    let mut e = boot();
    let seen = handler_saw(
        &mut e,
        "INSERT INTO ex2 VALUES (1, 9);",
        "WHEN OTHERS THEN INSERT INTO seen VALUES (SQLSTATE), (SQLERRM);",
    );
    assert_eq!(
        seen,
        "23505|duplicate key value violates unique constraint \"ex2_k_key\""
    );
}

#[test]
fn a_class_condition_a_literal_sqlstate_and_an_or_list_all_catch() {
    let mut e = boot();
    for handler in [
        "WHEN integrity_constraint_violation THEN INSERT INTO seen VALUES ('ok');",
        "WHEN SQLSTATE '23505' THEN INSERT INTO seen VALUES ('ok');",
        "WHEN division_by_zero OR unique_violation THEN INSERT INTO seen VALUES ('ok');",
    ] {
        assert_eq!(
            handler_saw(&mut e, "INSERT INTO ex2 VALUES (1, 9);", handler),
            "ok",
            "{handler}"
        );
    }
}

#[test]
fn a_handler_for_another_condition_lets_the_error_escape() {
    let mut e = boot();
    let err = e
        .execute(
            "DO $$ BEGIN INSERT INTO ex2 VALUES (1, 9); \
             EXCEPTION WHEN division_by_zero THEN NULL; END $$",
        )
        .expect_err("23505 is not 22012");
    assert!(format!("{err}").contains("duplicate key"), "{err}");
    assert_eq!(rows(&mut e), "1:1");
}

#[test]
fn expression_and_constraint_errors_are_caught_by_their_names() {
    let mut e = boot();
    e.execute("CREATE TABLE ex_nn (a int NOT NULL CHECK (a > 0))")
        .unwrap();
    assert_eq!(
        handler_saw(
            &mut e,
            "v := 1/0;",
            "WHEN division_by_zero THEN INSERT INTO seen VALUES (SQLSTATE);"
        ),
        "22012"
    );
    assert_eq!(
        handler_saw(
            &mut e,
            "INSERT INTO ex_nn VALUES (NULL);",
            "WHEN not_null_violation THEN INSERT INTO seen VALUES (SQLSTATE);"
        ),
        "23502"
    );
    assert_eq!(
        handler_saw(
            &mut e,
            "INSERT INTO ex_nn VALUES (-1);",
            "WHEN check_violation THEN INSERT INTO seen VALUES (SQLSTATE);"
        ),
        "23514"
    );
}

#[test]
fn a_variable_assigned_before_the_error_keeps_its_value() {
    let mut e = boot();
    assert_eq!(
        handler_saw(
            &mut e,
            "v := 5; INSERT INTO ex2 VALUES (1, 9);",
            "WHEN OTHERS THEN INSERT INTO seen VALUES (v::text);"
        ),
        "5"
    );
}

/// PG refuses an unknown condition name before running anything, and
/// sends an uncaught RAISE as P0001 with its own text.
#[test]
fn unknown_names_are_refused_and_an_uncaught_raise_is_p0001() {
    let mut e = boot();
    let err = e
        .execute("DO $$ BEGIN INSERT INTO ex2 VALUES (5, 5); EXCEPTION WHEN foo THEN NULL; END $$")
        .expect_err("unknown condition");
    assert_eq!(
        spg_engine::sqlstate::error_to_wire(&err),
        (
            "42704",
            "unrecognized exception condition \"foo\"".to_string()
        )
    );
    assert_eq!(rows(&mut e), "1:1", "refused before the block ran");
    let err = e
        .execute("DO $$ BEGIN RAISE EXCEPTION 'boom %', 1; END $$")
        .expect_err("RAISE");
    assert_eq!(
        spg_engine::sqlstate::error_to_wire(&err),
        ("P0001", "boom 1".to_string())
    );
}
