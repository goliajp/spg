//! v7.38 (read01) — DISTINCT over a synthetic / derived source (a subquery,
//! VALUES, unnest, or generate_series in FROM) now dedups, like PG; previously
//! those executors applied ORDER BY / OFFSET / LIMIT but silently dropped
//! DISTINCT. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn nrows(e: &mut Engine, sql: &str) -> usize {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => panic!("rows"),
    }
}

#[test]
fn distinct_over_derived_sources() {
    let mut e = Engine::new();
    // Derived table (VALUES / subquery).
    assert_eq!(nrows(&mut e, "SELECT DISTINCT x FROM (VALUES (1),(1),(2)) v(x)"), 2);
    assert_eq!(nrows(&mut e, "SELECT DISTINCT x FROM (SELECT 1 AS x UNION ALL SELECT 1) t"), 1);
    // unnest.
    assert_eq!(nrows(&mut e, "SELECT DISTINCT u FROM unnest(ARRAY[1,1,2,2]) u"), 2);
    // Composes with numeric-scale dedup.
    assert_eq!(nrows(&mut e, "SELECT DISTINCT x FROM (VALUES (1.0),(1.00),(1.000)) v(x)"), 1);
    // DISTINCT is applied before LIMIT and preserves ORDER BY.
    assert_eq!(nrows(&mut e, "SELECT DISTINCT x FROM (VALUES (1),(1),(2),(3)) v(x) ORDER BY x LIMIT 2"), 2);
    // Non-DISTINCT keeps duplicates.
    assert_eq!(nrows(&mut e, "SELECT x FROM (VALUES (1),(1),(2)) v(x)"), 3);
}
