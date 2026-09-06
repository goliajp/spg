//! v7.40.10 — `now()` did not exist inside an `INSERT … SELECT`.
//!
//! Reported against 7.40.9; 7.40.7 and 7.40.8 refuse it too, so not a
//! regression.
//!
//! ```text
//!   INSERT INTO d SELECT g, now() FROM src
//!     ERROR:  function now() does not exist
//!   INSERT INTO d SELECT g, current_timestamp FROM src
//!     ERROR:  function current_timestamp() does not exist
//! ```
//!
//! The second message names the shape: the statement says
//! `current_timestamp` with no parentheses and the error says
//! `current_timestamp()` with them. The clock rewrite turns the keyword
//! into a call and folds it to a literal; where the rewrite does not
//! reach, the call it made survives and no such function exists.
//!
//! `rewrite_clock_calls`'s INSERT arm walked `ins.rows` and the
//! ON CONFLICT clause, and not `ins.select_source`.
//! `substitute_placeholders`'s INSERT arm learned about that field in
//! v7.33; this walk never did. It is the fourth instance in one day of
//! a per-statement walk that knows some of the places a statement can
//! nest — after `unnest_expr` and `generate_series_args` in the
//! substitution walk (7.40.8), the same pair in `describe` (7.40.9),
//! and a FROM subquery's LIMIT (7.40.10).
//!
//! It is the time functions specifically, in that one position:
//! `gen_random_uuid()` is zero-argument and works there, as do `upper`,
//! `md5` and `abs`, because none of them is folded by the clock pass.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

/// A fixed clock, because the rewrite this file pins is SKIPPED
/// entirely when the engine has none — `rewrite_clock_calls` returns on
/// `now_micros == None` and the evaluator answers at runtime instead.
/// A test on a clockless engine exercises the other path and would have
/// gone green against a fix that did nothing, which is what the first
/// draft of this file did.
fn fixed_clock() -> i64 {
    1_767_225_600_000_000
}

fn engine_with(sqls: &[&str]) -> Engine {
    let mut eng = Engine::new().with_clock(fixed_clock);
    for sql in sqls {
        eng.execute(sql)
            .unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
    }
    eng
}

fn count(eng: &mut Engine, table: &str) -> i64 {
    match eng
        .execute(&format!("SELECT count(*) FROM {table}"))
        .expect("count")
    {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            Value::BigInt(n) => n,
            ref other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn fixture() -> Engine {
    engine_with(&[
        "CREATE TABLE src (g INT)",
        "INSERT INTO src VALUES (1), (2), (3)",
        "CREATE TABLE d (g INT, ts TIMESTAMPTZ)",
    ])
}

#[test]
fn now_exists_inside_an_insert_select() {
    let mut eng = fixture();
    eng.execute("INSERT INTO d SELECT g, now() FROM src")
        .expect("now() in an INSERT … SELECT");
    assert_eq!(count(&mut eng, "d"), 3);
}

/// The keyword spelling, which is the one whose error named the shape.
#[test]
fn current_timestamp_the_keyword_works_there_too() {
    let mut eng = fixture();
    eng.execute("INSERT INTO d SELECT g, current_timestamp FROM src")
        .expect("current_timestamp in an INSERT … SELECT");
    assert_eq!(count(&mut eng, "d"), 3);
}

/// The whole family, since one of them reaching the rewrite says
/// nothing about the others.
#[test]
fn the_rest_of_the_clock_family_reaches_it() {
    for f in [
        "now()",
        "current_timestamp",
        "localtimestamp",
        "statement_timestamp()",
        "transaction_timestamp()",
        "clock_timestamp()",
    ] {
        let mut eng = fixture();
        eng.execute(&format!("INSERT INTO d SELECT g, {f} FROM src"))
            .unwrap_or_else(|e| panic!("{f} in an INSERT … SELECT: {e:?}"));
        assert_eq!(count(&mut eng, "d"), 3, "{f}");
    }
}

/// One statement, one clock. The whole reason the rewrite folds these
/// to a literal is that every row must carry the same instant.
#[test]
fn every_row_carries_the_same_instant() {
    let mut eng = fixture();
    eng.execute("INSERT INTO d SELECT g, now() FROM src")
        .expect("insert");
    let distinct = match eng
        .execute("SELECT count(DISTINCT ts) FROM d")
        .expect("count distinct")
    {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(distinct, Value::BigInt(1), "one statement, one clock");
}

/// The positions that already worked, so a fix that reaches too far
/// cannot pass unnoticed.
#[test]
fn the_positions_that_already_worked_still_do() {
    let mut eng = fixture();
    eng.execute("INSERT INTO d VALUES (1, now())")
        .expect("VALUES");
    eng.execute("UPDATE d SET ts = now()").expect("UPDATE SET");
    match eng.execute("SELECT now()").expect("bare SELECT") {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("{other:?}"),
    }
    // A zero-argument function the clock pass does not fold has always
    // worked in the reported position; it is here as the control that
    // says the position itself was never the problem.
    eng.execute("CREATE TABLE u (g INT, id UUID)").expect("ddl");
    eng.execute("INSERT INTO u SELECT g, gen_random_uuid() FROM src")
        .expect("gen_random_uuid in an INSERT … SELECT");
    assert_eq!(count(&mut eng, "u"), 3);
}
