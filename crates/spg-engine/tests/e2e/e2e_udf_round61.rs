//! v7.39 (read01 round 61) — user-defined functions are CALLABLE, and they have
//! an ACL.
//!
//! `CREATE FUNCTION` has stored a function since v7.12.4 — but only TRIGGERS
//! ever invoked one. Calling `f1(1)` from an expression answered "unknown
//! function `f1`". The scalar call surface that `ReturnTarget::Expr`'s own doc
//! comment promised ("reserved for the scalar UDF surface in v7.12.5+") was
//! never built, so `CREATE FUNCTION` succeeded and the function was unusable.
//!
//! The call lives in EVAL, not the engine, because `EvalContext` already carries
//! the catalog: a body that calls another function recurses through the same
//! path, and a per-row call (`SELECT f1(a) FROM t`) needs no engine round-trip.
//!
//! Byte-locked against live PG18.4.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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
        "CREATE FUNCTION f1(x int) RETURNS int AS $$ SELECT x + 1 $$ LANGUAGE sql",
    );
    ok(&mut e, "CREATE TABLE t (id int)");
    ok(&mut e, "INSERT INTO t VALUES (1),(2),(3)");
    e
}

#[test]
fn a_sql_function_can_finally_be_called() {
    let mut e = seeded();
    assert_eq!(r1(&mut e, "SELECT f1(1)"), "2");
    // The declared return type is applied: a body yielding int through a
    // `RETURNS text` function comes back as text.
    ok(
        &mut e,
        "CREATE FUNCTION as_text(x int) RETURNS text AS $$ SELECT x + 1 $$ LANGUAGE sql",
    );
    assert_eq!(r1(&mut e, "SELECT as_text(1) || '!'"), "2!");
}

#[test]
fn a_function_may_call_another_function() {
    let mut e = seeded();
    ok(
        &mut e,
        "CREATE FUNCTION g(a int, b int) RETURNS int AS $$ SELECT a * b + f1(a) $$ LANGUAGE sql",
    );
    assert_eq!(r1(&mut e, "SELECT g(3,4)"), "16");
}

#[test]
fn a_plpgsql_body_of_a_single_return_works() {
    let mut e = seeded();
    ok(
        &mut e,
        "CREATE FUNCTION p1(x int) RETURNS text AS $$ BEGIN RETURN 'v' || x::text; END; $$ LANGUAGE plpgsql",
    );
    assert_eq!(r1(&mut e, "SELECT p1(7)"), "v7");
}

#[test]
fn a_function_is_callable_per_row_and_inside_an_aggregate() {
    let mut e = seeded();
    assert_eq!(r1(&mut e, "SELECT count(*) FROM t WHERE f1(id) > 2"), "2");
    // The aggregate stages each built a BARE EvalContext and dropped the
    // catalog `run` was already carrying — so a user function inside an
    // aggregate's argument answered "unknown function". Same family as the
    // catalog-less-context bugs of rounds 49/53/54/55/56.
    assert_eq!(
        r1(
            &mut e,
            "SELECT string_agg(f1(id)::text, ',' ORDER BY id) FROM t"
        ),
        "2,3,4"
    );
}

#[test]
fn a_wrong_arity_call_reports_pgs_no_such_signature() {
    let mut e = seeded();
    assert_eq!(
        err(&mut e, "SELECT f1(1,2)"),
        "eval: type mismatch: function f1(integer, integer) does not exist"
    );
}

#[test]
fn execute_is_granted_to_public_by_default() {
    let mut e = seeded();
    ok(&mut e, "CREATE ROLE fred LOGIN PASSWORD 'x'");
    // PG's default is not "nobody may call it" — proacl stays NULL to say so.
    assert_eq!(
        r1(&mut e, "SELECT has_function_privilege('fred','f1(int)','EXECUTE')"),
        "true"
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT coalesce(proacl,'NULL') FROM pg_proc WHERE proname='f1'"
        ),
        "NULL"
    );
    ok(&mut e, "SET ROLE fred");
    assert_eq!(r1(&mut e, "SELECT f1(1)"), "2");
}

#[test]
fn revoking_execute_from_public_actually_stops_the_call() {
    let mut e = seeded();
    ok(&mut e, "CREATE ROLE fred LOGIN PASSWORD 'x'");
    ok(&mut e, "REVOKE EXECUTE ON FUNCTION f1(int) FROM PUBLIC");
    // The ACL materialises, owner's entry included.
    assert_eq!(
        r1(&mut e, "SELECT proacl FROM pg_proc WHERE proname='f1'"),
        "{admin=X/admin}"
    );
    assert_eq!(
        r1(&mut e, "SELECT has_function_privilege('fred','f1(int)','EXECUTE')"),
        "false"
    );
    ok(&mut e, "SET ROLE fred");
    assert_eq!(
        err(&mut e, "SELECT f1(1)"),
        "eval: type mismatch: permission denied for function f1"
    );
    // …and a grant brings it back.
    ok(&mut e, "RESET ROLE");
    ok(&mut e, "GRANT EXECUTE ON FUNCTION f1(int) TO fred");
    ok(&mut e, "SET ROLE fred");
    assert_eq!(r1(&mut e, "SELECT f1(1)"), "2");
}

#[test]
fn grant_on_all_tables_in_schema_expands() {
    // It used to report success and do nothing — which tells a DBA the grant
    // landed when it did not.
    let mut e = seeded();
    ok(&mut e, "CREATE ROLE fred LOGIN PASSWORD 'x'");
    ok(&mut e, "CREATE TABLE t2 (a int)");
    ok(&mut e, "GRANT SELECT ON ALL TABLES IN SCHEMA public TO fred");
    assert_eq!(
        r1(&mut e, "SELECT has_table_privilege('fred','t','SELECT')"),
        "true"
    );
    assert_eq!(
        r1(&mut e, "SELECT has_table_privilege('fred','t2','SELECT')"),
        "true"
    );
    ok(&mut e, "REVOKE SELECT ON ALL TABLES IN SCHEMA public FROM fred");
    assert_eq!(
        r1(&mut e, "SELECT has_table_privilege('fred','t','SELECT')"),
        "false"
    );
}

#[test]
fn an_unsupported_body_says_so_instead_of_answering_wrongly() {
    let mut e = seeded();
    // v7.39 (read01 round 63) — a body with its own FROM IS invocable now: it
    // runs through the real executor (see e2e_udf_query_round63). What is still
    // unsupported says so instead of answering wrongly — a multi-statement
    // plpgsql body is the remaining shape.
    ok(
        &mut e,
        "CREATE FUNCTION reads(x int) RETURNS int AS $$ SELECT id FROM t WHERE id = x $$ LANGUAGE sql",
    );
    assert_eq!(r1(&mut e, "SELECT reads(1)"), "1");

    ok(
        &mut e,
        "CREATE FUNCTION multi(x int) RETURNS int AS $$ BEGIN x := x + 1; RETURN x; END; $$ LANGUAGE plpgsql",
    );
    let msg = err(&mut e, "SELECT multi(1)");
    assert!(msg.contains("single `RETURN <expr>;`"), "{msg}");
}
