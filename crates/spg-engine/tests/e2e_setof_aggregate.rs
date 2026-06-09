//! v7.17.0 Phase 3.P0-48 — set-returning sources (`generate_series`,
//! `unnest`) routing through the aggregate executor.
//!
//! Phase 3.2 (`0ffd766`) wired `FROM generate_series(...)` and
//! `FROM unnest(...)` as scan sources but the executor short-
//! circuited straight to the projection / ORDER BY / LIMIT
//! pipeline, never calling `aggregate::run`. So
//! `SELECT COUNT(*) FROM generate_series(1, 10)` either errored
//! at projection time (COUNT(*) isn't a per-row eval shape) or
//! silently returned the wrong row count — a Tier-A silent
//! divergence from PG. The same gap hit `unnest`.
//!
//! P0-48 adds the standard "aggregate dispatch happens before
//! projection" branch to both set-returning executors so the
//! customer's metrics queries (`COUNT`, `SUM`, `MIN`, `MAX`,
//! `AVG`, `string_agg`, …) all land cleanly.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn count_star_over_generate_series() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT COUNT(*) FROM generate_series(1, 100)")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::BigInt(100));
}

#[test]
fn sum_over_generate_series_with_column_alias() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT SUM(g) FROM generate_series(1, 10) AS g")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    // 1 + 2 + ... + 10 = 55.
    assert_eq!(r[0][0], Value::BigInt(55));
}

#[test]
fn min_max_over_generate_series() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT MIN(g), MAX(g) FROM generate_series(5, 12) AS g")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::BigInt(5));
    assert_eq!(r[0][1], Value::BigInt(12));
}

#[test]
fn count_with_where_filter_over_generate_series() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT COUNT(*) FROM generate_series(1, 100) AS g WHERE g > 50")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::BigInt(50));
}

#[test]
fn count_with_predicate_over_unnest() {
    // Phase 5 unnest sources expose a TEXT column. Aggregate
    // routing must work through WHERE-filtered subsets of the
    // unnest output.
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT COUNT(*) FROM unnest(ARRAY['a','b','b','c','c','c']) AS u WHERE u = 'c'")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::BigInt(3));
}

#[test]
fn count_star_over_unnest_keeps_duplicates() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT COUNT(*) FROM unnest(ARRAY['a','b','b','c','c','c']) AS u")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::BigInt(6));
}

#[test]
fn string_agg_over_unnest() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT string_agg(u, ',') FROM unnest(ARRAY['x','y','z']) AS u")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("x,y,z".into()));
}

#[test]
fn projection_path_still_works_no_aggregate() {
    // Regression: existing non-aggregate paths (just projection +
    // ORDER BY / LIMIT) must continue to work.
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT g FROM generate_series(1, 3) AS g ORDER BY g DESC")
            .unwrap(),
    );
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::BigInt(3));
    assert_eq!(r[1][0], Value::BigInt(2));
    assert_eq!(r[2][0], Value::BigInt(1));
}

#[test]
fn group_by_with_generate_series_via_mod_fn() {
    // GROUP BY on a derived value from generate_series.
    // 1..=10 grouped by `mod(g, 2)` → 5 odd, 5 even. (SPG uses
    // the `mod(a, b)` function form; `%` isn't a lexer token.)
    let mut e = Engine::new();
    let mut r = rows(
        e.execute(
            "SELECT mod(g, 2) AS parity, COUNT(*) FROM generate_series(1, 10) AS g \
             GROUP BY mod(g, 2) ORDER BY parity",
        )
        .unwrap(),
    );
    r.sort_by_key(|row| match row[0] {
        Value::BigInt(n) => n,
        Value::Int(n) => n as i64,
        _ => 0,
    });
    assert_eq!(r.len(), 2);
    // 0 (even): 5
    // 1 (odd): 5
    assert_eq!(r[0][1], Value::BigInt(5));
    assert_eq!(r[1][1], Value::BigInt(5));
}
