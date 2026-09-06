//! v7.40.10 — `LIMIT $n` inside a derived table was ignored, and the
//! statement returned every row.
//!
//! Reported against 7.40.9. No error, both the SQL-level
//! `PREPARE`/`EXECUTE` path and an extended-protocol Bind.
//!
//! The boundary, measured on the published 7.40.9 image before the fix
//! — five rows in the table, `LIMIT $1` bound to 2:
//!
//! ```text
//!   top level                                    2 rows   correct
//!   top level, LIMIT $1 OFFSET $2                2 rows   correct
//!   top level, FOR UPDATE SKIP LOCKED            2 rows   correct
//!   inside a CTE                                 2 rows   correct
//!   inside IN (SELECT … LIMIT $1)                2 rows   correct
//!   a scalar subquery's own LIMIT $1             1 row    correct
//!   inside a DERIVED TABLE                       5 rows   WRONG
//!   inside a LATERAL                             5 rows   WRONG
//! ```
//!
//! `substitute_select` resolves its own LIMIT/OFFSET and recurses into
//! CTE bodies and UNION peers. A FROM item's subquery is neither, and a
//! `LimitExpr` is not an `Expr`, so the expression walk that does reach
//! those subqueries never sees it.
//!
//! The reporter's own repro wrapped the statement in
//! `SELECT count(*) FROM (… LIMIT $1) s` to make counting easy, and
//! that wrapper IS the trigger: nine of the ten statements they listed
//! are top-level and were never affected. The tenth shape they did not
//! list is, and it is the one that matters most — an
//! `INSERT … SELECT … FROM (… ORDER BY … LIMIT $n) x`, which caps a
//! push send's audience. It inserted every matching row.

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

fn rows_of(eng: &mut Engine, sql: &str, params: &[Value<'static>]) -> Vec<Vec<Value<'static>>> {
    let stmt = eng.prepare(sql).expect("parses");
    match eng
        .execute_prepared(stmt, params)
        .unwrap_or_else(|e| panic!("{sql:?}: {e:?}"))
    {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("{sql:?}: expected Rows, got {other:?}"),
    }
}

fn five() -> Engine {
    engine_with(&[
        "CREATE TABLE lim (id INT, t TEXT)",
        "INSERT INTO lim VALUES (1,'row1'),(2,'row2'),(3,'row3'),(4,'row4'),(5,'row5')",
    ])
}

#[test]
fn a_derived_table_honours_its_bound_limit() {
    let mut eng = five();
    let got = rows_of(
        &mut eng,
        "SELECT t FROM (SELECT id, t FROM lim ORDER BY id LIMIT $1) z",
        &[Value::BigInt(2)],
    );
    assert_eq!(got.len(), 2, "two rows, not the whole table: {got:?}");
}

/// The reporter's own case E: the rows themselves rather than a count,
/// because a count over a fixture smaller than the limit cannot tell
/// the two states apart — which is why their suite went green on this
/// for as long as it has existed.
#[test]
fn the_rows_themselves_are_the_first_two() {
    let mut eng = five();
    let got = rows_of(
        &mut eng,
        "SELECT t FROM (SELECT id, t FROM lim ORDER BY id LIMIT $1) z",
        &[Value::BigInt(2)],
    );
    let texts: Vec<String> = got
        .iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.to_string(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(texts, vec!["row1".to_string(), "row2".to_string()]);
}

#[test]
fn a_derived_table_honours_a_bound_offset_too() {
    let mut eng = five();
    let got = rows_of(
        &mut eng,
        "SELECT t FROM (SELECT id, t FROM lim ORDER BY id LIMIT $1 OFFSET $2) z",
        &[Value::BigInt(2), Value::BigInt(1)],
    );
    let texts: Vec<String> = got
        .iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.to_string(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(texts, vec!["row2".to_string(), "row3".to_string()]);
}

#[test]
fn a_lateral_honours_it_as_well() {
    let mut eng = five();
    let got = rows_of(
        &mut eng,
        "SELECT b.t FROM lim a CROSS JOIN LATERAL \
         (SELECT t FROM lim ORDER BY id LIMIT $1) b WHERE a.id = 1",
        &[Value::BigInt(2)],
    );
    assert_eq!(got.len(), 2, "{got:?}");
}

/// The shape that costs a real deployment something: a push send caps
/// its audience with a `LIMIT $n` inside the derived table it selects
/// from. Ignoring it sends to every device rather than the capped set.
#[test]
fn an_insert_select_from_a_capped_derived_table_inserts_the_cap() {
    let mut eng = engine_with(&[
        "CREATE TABLE dtok (id INT, provider TEXT)",
        "INSERT INTO dtok VALUES (1,'apns'),(2,'apns'),(3,'apns'),(4,'apns'),(5,'apns')",
        "CREATE TABLE sends (id INT, provider TEXT)",
    ]);
    let stmt = eng
        .prepare(
            "INSERT INTO sends (id, provider) \
             SELECT dt.id, dt.provider \
             FROM (SELECT id, provider FROM dtok ORDER BY id LIMIT $1) dt",
        )
        .expect("parses");
    eng.execute_prepared(stmt, &[Value::BigInt(2)])
        .expect("insert");
    let got = match eng.execute("SELECT count(*) FROM sends").expect("count") {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        got,
        Value::BigInt(2),
        "a capped fan-out must insert the cap, not the whole audience"
    );
}

/// The shapes that were already right. They are here so a fix that
/// reaches too far — resolving a limit twice, or in a position where
/// the parameter means something else — cannot pass unnoticed.
#[test]
fn the_shapes_that_already_worked_still_do() {
    let mut eng = five();
    assert_eq!(
        rows_of(
            &mut eng,
            "SELECT t FROM lim ORDER BY id LIMIT $1",
            &[Value::BigInt(2)]
        )
        .len(),
        2,
        "top level"
    );
    assert_eq!(
        rows_of(
            &mut eng,
            "WITH c AS (SELECT id, t FROM lim ORDER BY id LIMIT $1) SELECT t FROM c",
            &[Value::BigInt(2)]
        )
        .len(),
        2,
        "inside a CTE"
    );
    assert_eq!(
        rows_of(
            &mut eng,
            "SELECT t FROM lim WHERE id IN (SELECT id FROM lim ORDER BY id LIMIT $1)",
            &[Value::BigInt(2)]
        )
        .len(),
        2,
        "inside an IN subquery"
    );
}
