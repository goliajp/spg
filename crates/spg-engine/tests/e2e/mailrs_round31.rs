//! mailrs round-31 — aggregate over a *correlated* subquery.
//!
//! `AGG( (SELECT … WHERE … = outer) )` failed at execution with
//! "subquery reached row eval — engine resolver bug": the aggregate's
//! per-row argument loop evaluated its argument with the plain
//! `eval_expr`, which rejects an un-pre-resolved subquery node. A
//! *non*-correlated subquery is folded to a literal before the row loop
//! (so it worked), but a *correlated* one must be evaluated per outer
//! row through the correlated evaluator — exactly the hook the
//! select-list / HAVING / ORDER finalisers already used. The fix routes
//! the aggregate argument / FILTER / arg2 / ORDER key through that hook
//! when they carry a subquery.
//!
//! In mailrs this is the conversation-search aggregation
//! (`BOOL_OR((SELECT ea.requires_action … WHERE ea.message_id = m.id))`);
//! the statement errored and mailrs `.unwrap_or_default()`'d it, so
//! search silently returned empty. Bisect proved the aggregate-wrapping
//! is necessary and sufficient — independent of GROUP BY, the aggregate
//! kind, the correlation target, and the IN-list.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn setup() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE rb_outer (id INT)").unwrap();
    e.execute("CREATE TABLE rb_inner (fk INT, v INT)").unwrap();
    e.execute("INSERT INTO rb_outer (id) VALUES (1),(2),(3)")
        .unwrap();
    e.execute("INSERT INTO rb_inner (fk, v) VALUES (1,10),(2,20),(3,30)")
        .unwrap();
    e
}

fn rows_of(e: &mut Engine, sql: &str) -> Vec<spg_storage::Row> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows from {sql:?}, got {other:?}"),
    }
}

#[test]
fn max_over_correlated_subquery_whole_table() {
    // The reported repro: aggregate wrapping a correlated subquery, no
    // GROUP BY. Previously errored; must fold the per-row scalars to 30.
    let mut e = setup();
    let rows = rows_of(
        &mut e,
        "SELECT MAX((SELECT i.v FROM rb_inner i WHERE i.fk = o.id)) FROM rb_outer o",
    );
    assert_eq!(rows.len(), 1);
    assert!(
        matches!(&rows[0].values[0], Value::Int(30)),
        "expected MAX = 30, got {:?}",
        rows[0].values[0]
    );
}

#[test]
fn count_over_correlated_subquery_with_group_by() {
    // Same class with GROUP BY + a different aggregate. Each outer row
    // resolves its correlated subquery to one non-null scalar, so COUNT
    // is 1 per group.
    let mut e = setup();
    let rows = rows_of(
        &mut e,
        "SELECT o.id, COUNT((SELECT i.v FROM rb_inner i WHERE i.fk = o.id)) \
         FROM rb_outer o GROUP BY o.id ORDER BY o.id",
    );
    assert_eq!(rows.len(), 3);
    for r in &rows {
        assert!(
            matches!(&r.values[1], Value::BigInt(1)),
            "expected COUNT = 1 per group, got {:?}",
            r.values[1]
        );
    }
}

#[test]
fn bool_or_over_correlated_subquery_mailrs_shape() {
    // The production shape: BOOL_OR over a correlated subquery, GROUP BY.
    // rb_inner.v doubles as a truthiness flag here (non-zero → true).
    let mut e = Engine::new();
    e.execute("CREATE TABLE m (id INT)").unwrap();
    e.execute("CREATE TABLE ea (message_id INT, flag BOOL)")
        .unwrap();
    e.execute("INSERT INTO m (id) VALUES (1),(2)").unwrap();
    e.execute("INSERT INTO ea (message_id, flag) VALUES (1, true), (2, false)")
        .unwrap();
    let rows = rows_of(
        &mut e,
        "SELECT m.id, \
            COALESCE(BOOL_OR((SELECT ea.flag FROM ea WHERE ea.message_id = m.id)), false) \
         FROM m GROUP BY m.id ORDER BY m.id",
    );
    assert_eq!(rows.len(), 2);
    assert!(
        matches!(&rows[0].values[1], Value::Bool(true)),
        "m.id=1 flag true"
    );
    assert!(
        matches!(&rows[1].values[1], Value::Bool(false)),
        "m.id=2 flag false"
    );
}

#[test]
fn bare_correlated_subquery_still_works() {
    // Control: a bare correlated subquery (no aggregate) was never
    // broken and must still resolve per outer row.
    let mut e = setup();
    let rows = rows_of(
        &mut e,
        "SELECT o.id, (SELECT i.v FROM rb_inner i WHERE i.fk = o.id) \
         FROM rb_outer o ORDER BY o.id",
    );
    assert_eq!(rows.len(), 3);
    let got: Vec<i32> = rows
        .iter()
        .map(|r| match r.values[1] {
            Value::Int(n) => n,
            ref other => panic!("expected Int, got {other:?}"),
        })
        .collect();
    assert_eq!(got, vec![10, 20, 30]);
}

#[test]
fn aggregate_over_noncorrelated_subquery_still_works() {
    // Control: aggregate over a *non*-correlated subquery is pre-resolved
    // to a literal before the row loop and must keep working.
    let mut e = setup();
    let rows = rows_of(&mut e, "SELECT MAX((SELECT 1)) FROM rb_outer o");
    assert_eq!(rows.len(), 1);
    assert!(
        matches!(&rows[0].values[0], Value::Int(1)),
        "expected MAX = 1, got {:?}",
        rows[0].values[0]
    );
}
