//! v7.39 (read01 round 62) — function OVERLOADING: `f(int)` and `f(text)` are
//! two functions.
//!
//! Round 61 made user functions callable but kept SPG's catalog keyed by NAME.
//! Two consequences, both bad:
//!
//!   - a second `CREATE FUNCTION f(text)` was an "already exists" error, so a
//!     pg_dump carrying an overload set could not restore at all;
//!   - and `f('hi')` — with only `f(int)` defined — silently ran the int
//!     overload and answered `int:hi`. A WRONG ANSWER, not an error.
//!
//! Functions are keyed by SIGNATURE now, with PG's type aliases folded together
//! so `f(integer)` and `f(int)` are the same function (which they are).
//! Byte-locked against live PG18.4.

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
    ok(
        &mut e,
        "CREATE FUNCTION f(x int) RETURNS text AS $$ SELECT 'int:' || x::text $$ LANGUAGE sql",
    );
    ok(
        &mut e,
        "CREATE FUNCTION f(x text) RETURNS text AS $$ SELECT 'text:' || x $$ LANGUAGE sql",
    );
    e
}

#[test]
fn two_overloads_coexist_and_the_right_one_runs() {
    let mut e = seeded();
    assert_eq!(r1(&mut e, "SELECT f(1)"), "int:1");
    assert_eq!(r1(&mut e, "SELECT f('hi')"), "text:hi");
    // Both are in pg_proc, sharing one name.
    assert_eq!(
        r1(&mut e, "SELECT count(*) FROM pg_proc WHERE proname='f'"),
        "2"
    );
}

#[test]
fn a_type_alias_names_the_same_function() {
    let mut e = seeded();
    // `integer` IS `int` — creating f(integer) is a redefinition, not a third
    // overload, so it needs OR REPLACE.
    let msg = err(
        &mut e,
        "CREATE FUNCTION f(x integer) RETURNS text AS $$ SELECT 'x' $$ LANGUAGE sql",
    );
    assert!(msg.contains("already exists"), "{msg}");
    ok(
        &mut e,
        "CREATE OR REPLACE FUNCTION f(x integer) RETURNS text AS $$ SELECT 'replaced' $$ LANGUAGE sql",
    );
    assert_eq!(r1(&mut e, "SELECT f(1)"), "replaced");
    assert_eq!(
        r1(&mut e, "SELECT count(*) FROM pg_proc WHERE proname='f'"),
        "2"
    );
}

#[test]
fn each_overload_carries_its_own_acl() {
    let mut e = seeded();
    ok(&mut e, "CREATE ROLE fred LOGIN PASSWORD 'x'");
    ok(&mut e, "REVOKE EXECUTE ON FUNCTION f(int) FROM PUBLIC");
    assert_eq!(
        r1(
            &mut e,
            "SELECT has_function_privilege('fred','f(int)','EXECUTE')"
        ),
        "false"
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT has_function_privilege('fred','f(text)','EXECUTE')"
        ),
        "true"
    );
    ok(&mut e, "SET ROLE fred");
    assert_eq!(
        err(&mut e, "SELECT f(1)"),
        "eval: type mismatch: permission denied for function f"
    );
    // The other overload is untouched.
    assert_eq!(r1(&mut e, "SELECT f('hi')"), "text:hi");
}

#[test]
fn dropping_needs_a_signature_when_the_name_is_ambiguous() {
    let mut e = seeded();
    let msg = err(&mut e, "DROP FUNCTION f");
    assert!(msg.contains("is not unique"), "{msg}");
    ok(&mut e, "DROP FUNCTION f(int)");
    // The int one is gone; the text one still answers.
    assert_eq!(r1(&mut e, "SELECT f('hi')"), "text:hi");
    assert_eq!(
        r1(&mut e, "SELECT count(*) FROM pg_proc WHERE proname='f'"),
        "1"
    );
    // …and now that the name IS unambiguous, a bare DROP works.
    ok(&mut e, "DROP FUNCTION f");
    assert_eq!(
        r1(&mut e, "SELECT count(*) FROM pg_proc WHERE proname='f'"),
        "0"
    );
}

#[test]
fn a_call_that_matches_no_overload_says_so() {
    let mut e = Engine::new();
    ok(
        &mut e,
        "CREATE FUNCTION only_int(x int) RETURNS int AS $$ SELECT x $$ LANGUAGE sql",
    );
    // One overload of that arity: PG coerces, and so does SPG.
    assert_eq!(r1(&mut e, "SELECT only_int(1)"), "1");
    // Wrong arity names no function that exists.
    assert_eq!(
        err(&mut e, "SELECT only_int(1, 2)"),
        "eval: type mismatch: function only_int(integer, integer) does not exist"
    );
}
