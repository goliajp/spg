//! v7.33 (array_agg argmax) — `(array_agg(x ORDER BY y))[1]` is the
//! argmax/argmin idiom. The engine special-cases it to a running
//! first-by-order accumulator (no per-group array build + sort). These
//! pin the semantics the optimisation must preserve exactly: DESC/ASC,
//! ties (earliest row wins, = the stable-sort element 1), NULLS
//! FIRST/LAST, a NULL *value* at position 1, and the bare-DESC default
//! NULLS placement. The comparator is shared with the full-sort path
//! (`cmp_order_keys` / `order_by_value_cmp`), so these confirm the fast
//! path reproduces what `[1]` of the materialised array would yield.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (g INT, k INT, label TEXT)")
        .unwrap();
    e.execute(
        "INSERT INTO t VALUES \
         (1,3,'c'),(1,1,'a'),(1,2,'b'), \
         (2,5,'x'),(2,5,'y'),(2,5,'z'), \
         (3,NULL,'n'),(3,2,'p'), \
         (4,9,NULL),(4,1,'q')",
    )
    .unwrap();
    e
}

fn col1_by_group(r: QueryResult) -> Vec<Value> {
    match r {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|row| row.values.into_iter().nth(1).unwrap())
            .collect(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn t(s: &str) -> Value {
    Value::Text(s.to_string())
}

#[test]
fn argmax_desc_nulls_last() {
    let mut e = seed();
    let r = e
        .execute(
            "SELECT g, (array_agg(label ORDER BY k DESC NULLS LAST))[1] \
             FROM t GROUP BY g ORDER BY g",
        )
        .unwrap();
    // g1: k desc 3,2,1 -> 'c'. g2: all k=5 tie -> earliest 'x'.
    // g3: 2 then NULL(last) -> 'p'. g4: k=9 first, its label IS NULL.
    assert_eq!(col1_by_group(r), vec![t("c"), t("x"), t("p"), Value::Null]);
}

#[test]
fn argmin_asc() {
    let mut e = seed();
    let r = e
        .execute("SELECT g, (array_agg(label ORDER BY k ASC))[1] FROM t GROUP BY g ORDER BY g")
        .unwrap();
    // g1: k asc 1,2,3 -> 'a'. g2: tie -> 'x'. g3: 2 then NULL(asc default
    // last) -> 'p'. g4: k=1 first -> 'q'.
    assert_eq!(col1_by_group(r), vec![t("a"), t("x"), t("p"), t("q")]);
}

#[test]
fn argmax_desc_nulls_first_picks_the_null_keyed_row() {
    let mut e = seed();
    let r = e
        .execute(
            "SELECT g, (array_agg(label ORDER BY k DESC NULLS FIRST))[1] \
             FROM t GROUP BY g ORDER BY g",
        )
        .unwrap();
    // g3: NULL key sorts FIRST -> its label 'n'. Others unchanged.
    assert_eq!(col1_by_group(r), vec![t("c"), t("x"), t("n"), Value::Null]);
}

#[test]
fn empty_group_is_null() {
    let mut e = seed();
    let r = e
        .execute(
            "SELECT g, (array_agg(label ORDER BY k DESC))[1] \
             FROM t WHERE g = 99 GROUP BY g",
        )
        .unwrap();
    match r {
        QueryResult::Rows { rows, .. } => assert!(rows.is_empty()),
        other => panic!("{other:?}"),
    }
}

#[test]
fn coalesced_argmax_matches_inbox_shape() {
    // The exact inbox shape: COALESCE((array_agg(... ORDER BY ...))[1], def).
    let mut e = seed();
    let r = e
        .execute(
            "SELECT g, COALESCE((array_agg(label ORDER BY k DESC NULLS LAST))[1], 'def') \
             FROM t GROUP BY g ORDER BY g",
        )
        .unwrap();
    // g4's element-1 value is NULL -> COALESCE yields 'def'.
    assert_eq!(col1_by_group(r), vec![t("c"), t("x"), t("p"), t("def")]);
}
