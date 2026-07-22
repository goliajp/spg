//! read01 round 335 (V61) — a scalar subquery inside a function body.
//!
//! `RETURN (SELECT count(*) FROM t)` answered `subquery reached row eval —
//! engine resolver bug`: an INTERNAL message, reaching the client, for a
//! shape PG runs without comment. `SELECT … INTO` worked, because that arm
//! had a resolver of its own; expressions had none.
//!
//! Found in round 334 while checking whether a plpgsql `SECURITY DEFINER`
//! body switched roles — it failed identically as a superuser, so it was
//! never a privilege question.
//!
//! PG 18.4 measured over a three-row table: all four forms answer 3, 3, 3
//! and 13.

use spg_engine::Engine;
use spg_storage::Value;

fn one(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        spg_engine::QueryResult::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.values.first())
            .cloned()
            .map(Value::into_owned)
            .unwrap_or(Value::Null),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t61 (id INT)").unwrap();
    e.execute("INSERT INTO t61 VALUES (1), (2), (3)").unwrap();
    e
}

#[test]
fn return_of_a_scalar_subquery_works() {
    let mut e = fixture();
    e.execute(
        "CREATE FUNCTION f_ret() RETURNS bigint LANGUAGE plpgsql \
         AS $$ BEGIN RETURN (SELECT count(*) FROM t61); END; $$",
    )
    .unwrap();
    assert_eq!(one(&mut e, "SELECT f_ret()"), Value::BigInt(3));
}

/// A subquery nested inside a larger expression, not just as the whole
/// body — the shape that proved the fix had to reach every node.
#[test]
fn a_subquery_nested_in_an_expression_works() {
    let mut e = fixture();
    e.execute(
        "CREATE FUNCTION f_expr(a INT) RETURNS int LANGUAGE plpgsql \
         AS $$ BEGIN RETURN a + (SELECT max(id) FROM t61); END; $$",
    )
    .unwrap();
    assert_eq!(one(&mut e, "SELECT f_expr(10)"), Value::Int(13));
}

/// The assignment form goes through the interpreter rather than the folded
/// expression path, and needed its own resolver.
#[test]
fn an_assignment_from_a_scalar_subquery_works() {
    let mut e = fixture();
    e.execute(
        "CREATE FUNCTION f_var() RETURNS bigint LANGUAGE plpgsql \
         AS $$ DECLARE n bigint; BEGIN n := (SELECT count(*) FROM t61); RETURN n; END; $$",
    )
    .unwrap();
    assert_eq!(one(&mut e, "SELECT f_var()"), Value::BigInt(3));
}

/// `SELECT … INTO` already worked and must keep working.
#[test]
fn select_into_still_works() {
    let mut e = fixture();
    e.execute(
        "CREATE FUNCTION f_into() RETURNS bigint LANGUAGE plpgsql \
         AS $$ DECLARE n bigint; BEGIN SELECT count(*) INTO n FROM t61; RETURN n; END; $$",
    )
    .unwrap();
    assert_eq!(one(&mut e, "SELECT f_into()"), Value::BigInt(3));
}

/// A subquery that reads the function's own local — the substitution has to
/// happen inside the subquery's statement, not only around it.
#[test]
fn a_subquery_can_read_a_local() {
    let mut e = fixture();
    e.execute(
        "CREATE FUNCTION f_local() RETURNS bigint LANGUAGE plpgsql \
         AS $$ DECLARE lim int; n bigint; BEGIN lim := 2; \
         n := (SELECT count(*) FROM t61 WHERE id <= lim); RETURN n; END; $$",
    )
    .unwrap();
    assert_eq!(one(&mut e, "SELECT f_local()"), Value::BigInt(2));
}
