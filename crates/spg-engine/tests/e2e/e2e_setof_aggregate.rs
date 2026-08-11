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

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
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
    // 1 + 2 + ... + 10 = 55. v7.38 (read01, T-gs) — generate_series(int4) yields
    // int4 elements (matching PG), and sum(int4) widens to BIGINT, so PG and SPG
    // agree on bigint 55.
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
    // generate_series(int4) → int4 elements, so MIN/MAX stay int4 (matching PG).
    assert_eq!(r[0][0], Value::Int(5));
    assert_eq!(r[0][1], Value::Int(12));
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
    assert_eq!(r[0][0], Value::text("x,y,z"));
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
    // generate_series(int4) → int4 elements (matching PG).
    assert_eq!(r[0][0], Value::Int(3));
    assert_eq!(r[1][0], Value::Int(2));
    assert_eq!(r[2][0], Value::Int(1));
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

/// r997 — a set-returning SELECT item must not be deferred past the sort.
///
/// v7.37.x added a deferral: on `GROUP BY g ORDER BY <agg> LIMIT k` the
/// per-item projection is skipped for every group and run afterwards on
/// the top-k survivors, which on the mailrs shape turns 40 000 evaluations
/// into 100. The completion evaluates each item scalarly, and the branch
/// that expands a set-returning item into one row per element is the one
/// the deferral skips — so a qualifying query came back as
/// `function unnest(integer[]) does not exist`, the exact error round 621
/// had fixed, reintroduced for the shapes that qualify.
///
/// Differential against live PG18.4 showed the same query answering
/// correctly without LIMIT, with LIMIT >= the group count, and with a
/// HAVING — the three cases where the deferral was already off — which is
/// what identified the deferral rather than the expansion.
///
/// The row COUNT is what this pins. Which rows survive a LIMIT that cuts
/// inside a group is not pinned, because the ORDER BY here ties across a
/// group's expanded rows and neither engine promises an order there.
#[test]
fn a_set_returning_item_survives_order_by_with_limit() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE srf_defer (g INT, v INT)").unwrap();
    for i in 1..=40 {
        e.execute(&format!("INSERT INTO srf_defer VALUES ({}, {i})", i % 20))
            .unwrap();
    }

    // 20 groups, two elements per group, LIMIT below the group count: the
    // shape that qualifies to defer.
    let r = e
        .execute(
            "SELECT unnest(ARRAY[1,2]), count(*) FROM srf_defer \
             GROUP BY g ORDER BY count(*) DESC, g LIMIT 5",
        )
        .expect("a set-returning item with ORDER BY + LIMIT must not error");
    assert_eq!(rows(r).len(), 5, "LIMIT 5 over expanded rows returns 5");

    // The cases the deferral never covered, unchanged: two rows per group
    // across all twenty groups.
    let r = e
        .execute(
            "SELECT unnest(ARRAY[1,2]), count(*) FROM srf_defer \
             GROUP BY g ORDER BY count(*) DESC, g",
        )
        .expect("no LIMIT");
    assert_eq!(rows(r).len(), 40, "20 groups x 2 elements");

    let r = e
        .execute(
            "SELECT unnest(ARRAY[1,2]), count(*) FROM srf_defer \
             GROUP BY g ORDER BY count(*) DESC, g LIMIT 100",
        )
        .expect("LIMIT above the group count");
    assert_eq!(
        rows(r).len(),
        40,
        "a LIMIT that cannot bite changes nothing"
    );

    // And the deferral still applies when nothing is set-returning: this
    // one is here so a fix that simply turned the optimisation off would
    // not pass unnoticed — it pins the answer, and the perf gate pins the
    // speed.
    let r = e
        .execute("SELECT g, count(*) FROM srf_defer GROUP BY g ORDER BY count(*) DESC, g LIMIT 5")
        .expect("no SRF");
    assert_eq!(rows(r).len(), 5);
}

/// r999 — `SELECT DISTINCT` deduplicates over a GROUP BY query.
///
/// It never had. Every other path does: the scan paths, the window path
/// and the set operations all call `dedup_rows`, and the aggregate path
/// simply returned one row per group. `SELECT DISTINCT count(*) FROM t
/// GROUP BY g` came back with 200 rows where PG18.4 returns 1, all of
/// them the same value — not an error, not a missing column, 199 extra
/// rows in a query anyone might write.
///
/// The top-K sink in that same function says it outright — "no DISTINCT
/// (would need post-dedup, can't truncate during sort)" — so the sink
/// correctly declined to truncate, and the post-dedup it named was never
/// written.
///
/// Found while validating an unrelated change, and confirmed against the
/// previous binary before being attributed: the behaviour is identical on
/// round 997, so it predates that work.
#[test]
fn select_distinct_deduplicates_over_a_group_by() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE dg (g INT, v INT)").unwrap();
    for i in 1..=200 {
        e.execute(&format!("INSERT INTO dg VALUES ({}, {i})", i % 20))
            .unwrap();
    }

    // Twenty groups, every count the same: one distinct row.
    let r = e
        .execute("SELECT DISTINCT count(*) FROM dg GROUP BY g")
        .unwrap();
    assert_eq!(rows(r).len(), 1, "twenty identical counts are one row");

    let r = e
        .execute("SELECT DISTINCT count(*) FROM dg GROUP BY g HAVING count(*) > 0")
        .unwrap();
    assert_eq!(rows(r).len(), 1, "a HAVING does not change the dedup");

    // Partial: g % 4 collapses twenty groups onto four values.
    let r = e
        .execute("SELECT DISTINCT g % 4 FROM dg GROUP BY g ORDER BY 1")
        .unwrap();
    assert_eq!(rows(r).len(), 4, "collapses onto four");

    // And rows that genuinely differ are all kept — the control that
    // separates "deduplicates" from "drops rows".
    let r = e
        .execute("SELECT DISTINCT count(*), g FROM dg GROUP BY g")
        .unwrap();
    assert_eq!(rows(r).len(), 20, "distinct rows survive");

    // Without DISTINCT nothing is removed.
    let r = e.execute("SELECT count(*) FROM dg GROUP BY g").unwrap();
    assert_eq!(rows(r).len(), 20, "no DISTINCT, no dedup");
}

/// r1000 — an aggregate ORDER BY may name a set-returning output column.
///
/// `SELECT unnest(ARRAY[2,1]) AS u, count(*) FROM t GROUP BY g ORDER BY 1`
/// answered `column "u" does not exist`, and spelled `ORDER BY u` it
/// answered `function unnest(integer[]) does not exist` instead — two
/// spellings of one thing, both refused, both answered by PG18.4.
///
/// Round 80 had already decided the hard part: a positional key over a
/// set-returning item resolves to the item's output NAME rather than its
/// expression, because the expression is the whole set and evaluates once
/// per group, which silently sorted nothing. What was missing was the
/// other half on each side — the aggregate sort evaluated that name
/// against the synthetic group schema, which carries `__agg_N` and
/// `__grp_K` and no output aliases; and the alias spelling still
/// substituted the expression, so it failed differently for the same
/// reason.
///
/// Now a key that names an output column and nothing in the synthetic
/// schema is read from the projected row, where expansion has already put
/// the per-row value. Synthetic names keep precedence, so keys that
/// resolved before resolve the same way.
#[test]
fn an_aggregate_order_by_can_name_a_set_returning_output_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE og (g INT, v INT)").unwrap();
    for i in 1..=20 {
        e.execute(&format!("INSERT INTO og VALUES ({}, {i})", i % 5))
            .unwrap();
    }
    let first = |r: QueryResult| -> Vec<i32> {
        rows(r)
            .into_iter()
            .map(|vals| match vals[0] {
                Value::Int(n) => n,
                ref other => panic!("int expected, got {other:?}"),
            })
            .collect()
    };

    // ARRAY[2,1] expands to 2 then 1 per group, so a sort that works puts
    // every 1 before every 2 — and a sort that silently does nothing
    // leaves them alternating.
    let r = e
        .execute("SELECT unnest(ARRAY[2,1]) AS u, count(*) FROM og GROUP BY g ORDER BY 1")
        .expect("ORDER BY a positional set-returning key");
    let got = first(r);
    assert_eq!(got.len(), 10, "5 groups x 2 elements");
    assert!(got.windows(2).all(|w| w[0] <= w[1]), "sorted, got {got:?}");

    // The same query by alias must behave identically; it used to fail
    // differently, which is how one bug looked like two.
    let r = e
        .execute("SELECT unnest(ARRAY[2,1]) AS u, count(*) FROM og GROUP BY g ORDER BY u")
        .expect("ORDER BY the alias of a set-returning item");
    assert_eq!(first(r), got, "the alias spelling matches the ordinal one");

    // And a key that resolves in the synthetic schema still does: this is
    // the control for "output names take over too much".
    let r = e
        .execute("SELECT count(*) AS c, g FROM og GROUP BY g ORDER BY c, g")
        .expect("ORDER BY an aggregate alias");
    assert_eq!(rows(r).len(), 5);
}
