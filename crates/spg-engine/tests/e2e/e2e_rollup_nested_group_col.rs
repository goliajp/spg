//! v7.38 (read01) — a grouping column referenced *inside an expression* in a
//! ROLLUP / CUBE / GROUPING SETS query (`COALESCE(g,'TOTAL')`,
//! `CASE WHEN g IS NULL …`) now resolves: in a grouping set where the key is
//! dropped it evaluates to NULL, at any depth. Previously only a bare
//! top-level select item equal to the key was nullified, so a nested
//! reference failed with "column not found". Row multisets are PG18.4-exact
//! (order is implementation-defined without ORDER BY, so we compare as sets).

use spg_engine::{Engine, QueryResult};
use std::collections::BTreeSet;

fn rows_set(e: &mut Engine, sql: &str) -> BTreeSet<String> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        spg_storage::Value::Text(s) => s.to_string(),
                        spg_storage::Value::Null => "∅".to_string(),
                        v @ spg_storage::Value::SmallIntArray(_) => {
                            spg_engine::eval::value_to_text(v)
                        }
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE rl(g text, h text, v int)").unwrap();
    e.execute("INSERT INTO rl VALUES ('a','x',1),('a','y',2),('b','x',3)")
        .unwrap();
    e
}

#[test]
fn coalesce_of_group_key_in_rollup() {
    let mut e = seed();
    assert_eq!(
        rows_set(
            &mut e,
            "SELECT COALESCE(g,'TOTAL')::text, sum(v)::text FROM rl GROUP BY ROLLUP(g)"
        ),
        set(&["a|3", "b|3", "TOTAL|6"])
    );
    // Nested inside a `||` alongside the aggregate.
    assert_eq!(
        rows_set(
            &mut e,
            "SELECT (COALESCE(g,'TOTAL')||':'||sum(v))::text FROM rl GROUP BY ROLLUP(g)"
        ),
        set(&["a:3", "b:3", "TOTAL:6"])
    );
}

#[test]
fn case_when_group_key_is_null_in_rollup() {
    // The canonical rollup-total label idiom.
    let mut e = seed();
    assert_eq!(
        rows_set(
            &mut e,
            "SELECT (CASE WHEN g IS NULL THEN 'ALL' ELSE g END)::text, sum(v)::text \
             FROM rl GROUP BY ROLLUP(g)"
        ),
        set(&["a|3", "b|3", "ALL|6"])
    );
    // Two keys, nested IS NULL AND IS NULL.
    assert_eq!(
        rows_set(
            &mut e,
            "SELECT (CASE WHEN g IS NULL AND h IS NULL THEN 'GRAND' WHEN h IS NULL THEN g||'-sub' \
             ELSE g||'/'||h END)::text, sum(v)::text FROM rl GROUP BY ROLLUP(g,h)"
        ),
        set(&["a/x|1", "a/y|2", "b/x|3", "a-sub|3", "b-sub|3", "GRAND|6"])
    );
}

#[test]
fn cube_and_grouping_sets_nested_keys() {
    let mut e = seed();
    assert_eq!(
        rows_set(
            &mut e,
            "SELECT COALESCE(g,'-')::text, COALESCE(h,'-')::text, sum(v)::text FROM rl GROUP BY CUBE(g,h)"
        ),
        set(&[
            "a|x|1", "a|y|2", "b|x|3", "a|-|3", "b|-|3", "-|x|4", "-|y|2", "-|-|6"
        ])
    );
    // grouping() still works alongside the nested key.
    assert_eq!(
        rows_set(
            &mut e,
            "SELECT COALESCE(g,'x')::text, count(*)::text, grouping(g)::text FROM rl GROUP BY ROLLUP(g)"
        ),
        set(&["a|2|0", "b|1|0", "x|3|1"])
    );
}

// ── v7.39 — multi-column GROUPING SETS with top-level ORDER BY ──

#[test]
fn grouping_sets_multi_column_order_by_resolves() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE gs (g INT, x INT)").unwrap();
    e.execute("INSERT INTO gs VALUES (1,10),(1,20),(2,5)")
        .unwrap();
    // The head set drops `x` (NULL literal); its column name must
    // survive so the ORDER BY on the union output still resolves.
    let QueryResult::Rows { rows, .. } = e
        .execute(
            "SELECT g, x, count(*) FROM gs GROUP BY GROUPING SETS ((g),(x)) \
             ORDER BY g NULLS LAST, x NULLS LAST",
        )
        .unwrap()
    else {
        panic!("rows")
    };
    // (g)-set: g=1(n=2), g=2(n=1) with x NULL; (x)-set: x=5,10,20 with g NULL.
    assert_eq!(rows.len(), 5, "two g-groups + three x-groups");
}
