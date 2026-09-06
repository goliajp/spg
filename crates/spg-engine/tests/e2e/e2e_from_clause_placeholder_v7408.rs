//! v7.40.8 — `$N` inside a FROM-clause set-returning function.
//!
//! Reported by a customer against 7.40.7 as a live 500 on one of their
//! endpoints, traced by them from the failing request to the server's
//! own log to this one line:
//!
//! ```text
//!   PREPARE q (int[]) AS SELECT count(*) FROM unnest($1::int[]) AS t(x);
//!   EXECUTE q (ARRAY[1,2]);
//!     ERROR:  parameter $1 referenced but only 0 bound by client
//! ```
//!
//! The parameter arrives — `= ANY($1)` reads it in the same statement
//! shape — and `unnest` does not see it. The substitution walk visits a
//! FROM item's LATERAL subquery and its ON clause, and neither of the
//! two expression slots a set-returning FROM item actually carries:
//! `unnest_expr` and `generate_series_args`. A `$N` in either reaches
//! evaluation still a placeholder, against an empty parameter buffer.
//!
//! Both slots are pinned here, in the primary position and on the join
//! side, because they are one defect and fixing the reported half would
//! leave the other.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn engine_with(sqls: &[&str]) -> Engine {
    let mut eng = Engine::new();
    for sql in sqls {
        eng.execute(sql)
            .unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
    }
    eng
}

fn fetch_with_params(
    eng: &mut Engine,
    sql: &str,
    params: &[Value<'static>],
) -> Vec<Vec<Value<'static>>> {
    let stmt = eng.prepare(sql).expect("parses");
    match eng
        .execute_prepared(stmt, params)
        .unwrap_or_else(|e| panic!("{sql:?}: {e:?}"))
    {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("expected Rows from {sql:?}, got {other:?}"),
    }
}

fn ints(v: &[i32]) -> Value<'static> {
    Value::IntArray(v.iter().map(|n| Some(*n)).collect())
}

#[test]
fn a_bound_array_reaches_unnest_in_the_primary_position() {
    let mut eng = Engine::new();
    let rows = fetch_with_params(
        &mut eng,
        "SELECT count(*) FROM unnest($1::int[]) AS t(x)",
        &[ints(&[1, 2])],
    );
    assert_eq!(rows, vec![vec![Value::BigInt(2)]]);
}

/// The customer's own boundary: the SAME parameter, read two ways in
/// one statement shape. `= ANY` worked while `unnest` raised, which is
/// what told them the parameter had arrived.
#[test]
fn the_same_parameter_reads_the_same_through_any_and_through_unnest() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (x INT NOT NULL)",
        "INSERT INTO t VALUES (1), (2), (3)",
    ]);
    let via_any = fetch_with_params(
        &mut eng,
        "SELECT count(*) FROM t WHERE x = ANY($1::int[])",
        &[ints(&[1, 2])],
    );
    let via_unnest = fetch_with_params(
        &mut eng,
        "SELECT count(*) FROM unnest($1::int[]) AS u(x)",
        &[ints(&[1, 2])],
    );
    assert_eq!(via_any, via_unnest, "one parameter, one answer");
}

#[test]
fn arity_is_not_part_of_it() {
    let mut eng = Engine::new();
    let rows = fetch_with_params(
        &mut eng,
        "SELECT count(*) FROM unnest($1::int[], $2::int[]) AS t(a, b)",
        &[ints(&[1, 2]), ints(&[3, 4])],
    );
    assert_eq!(rows, vec![vec![Value::BigInt(2)]]);
}

#[test]
fn a_bound_array_reaches_unnest_on_the_join_side() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (x INT NOT NULL)",
        "INSERT INTO t VALUES (1), (2), (3)",
    ]);
    let rows = fetch_with_params(
        &mut eng,
        "SELECT count(*) FROM t JOIN unnest($1::int[]) AS u(x) ON u.x = t.x",
        &[ints(&[1, 2])],
    );
    assert_eq!(rows, vec![vec![Value::BigInt(2)]]);
}

/// The other expression slot on the same FROM item. Nobody reported
/// this one; it is the same walk missing the same kind of field.
#[test]
fn a_bound_bound_reaches_generate_series() {
    let mut eng = Engine::new();
    let rows = fetch_with_params(
        &mut eng,
        "SELECT count(*) FROM generate_series($1, $2) AS g(n)",
        &[Value::Int(1), Value::Int(4)],
    );
    assert_eq!(rows, vec![vec![Value::BigInt(4)]]);
}

#[test]
fn generate_series_on_the_join_side_too() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (x INT NOT NULL)",
        "INSERT INTO t VALUES (1), (2), (3)",
    ]);
    let rows = fetch_with_params(
        &mut eng,
        "SELECT count(*) FROM t JOIN generate_series($1, $2) AS g(n) ON g.n = t.x",
        &[Value::Int(2), Value::Int(9)],
    );
    assert_eq!(rows, vec![vec![Value::BigInt(2)]]);
}

/// A literal array in the same position kept working throughout, which
/// is why the defect survived: every fixture used one.
#[test]
fn a_literal_array_still_works() {
    let mut eng = Engine::new();
    let rows = fetch_with_params(
        &mut eng,
        "SELECT count(*) FROM unnest(ARRAY[1,2,3]) AS t(x)",
        &[],
    );
    assert_eq!(rows, vec![vec![Value::BigInt(3)]]);
}

/// The customer's own repro spelling. SQL-level `PREPARE`/`EXECUTE` is
/// a different entry point from the wire's Bind/Execute, and it is the
/// one they could reduce the failure to.
#[test]
fn sql_level_prepare_and_execute_reach_it_too() {
    let mut eng = Engine::new();
    eng.execute("PREPARE q3 (int[]) AS SELECT count(*) FROM unnest($1::int[]) AS t(x)")
        .expect("prepare");
    match eng.execute("EXECUTE q3 (ARRAY[1,2])").expect("execute") {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(
                rows.into_iter().map(|r| r.values).collect::<Vec<_>>(),
                vec![vec![Value::BigInt(2)]]
            );
        }
        other => panic!("expected Rows, got {other:?}"),
    }
}
