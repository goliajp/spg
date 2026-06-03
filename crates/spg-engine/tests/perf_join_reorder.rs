#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::uninlined_format_args
)]

//! v6.2.3 ship-gate: the 5-table JOIN reorder must produce ≥ 10×
//! speedup vs the source order on a workload where source order
//! materialises an early cross-product.
//!
//! Schema:
//!   t_big1 (1000 rows), t_big2 (1000 rows), t_small (10 rows),
//!   t_big3 (1000 rows), t_big4 (1000 rows)
//!
//! Source order: `t_big1 JOIN t_big2 JOIN t_small JOIN t_big3
//! JOIN t_big4`. With nested-loop semantics, source order
//! produces a cross-product on the very first step (1000 × 1000
//! = 10^6 candidate rows) before the small table can filter.
//!
//! Reordered: the planner sees t_small as the smallest table +
//! every other table joined via an FK edge to t_small. Putting
//! t_small first keeps the running set at ~10 rows, then each
//! subsequent join touches 10 × 1000 = 10^4 candidate rows —
//! orders-of-magnitude cheaper.

use std::time::Instant;

use spg_engine::Engine;

const N_BIG: usize = 40;
const N_FACT: usize = 3;

/// Star workload: 4 "big" dimension tables + 1 small fact table.
/// The fact table holds an FK pointing into each big. Source
/// order joins the bigs first with no usable predicate (literal
/// `1=1` edges), so the executor materialises a 4-way cartesian
/// `40^4 = 2.56M` rows before the fact predicate can filter.
///
/// Reorder finds that putting fact first + chaining each big via
/// its specific predicate keeps every intermediate at ~3 rows
/// (the fact-table cardinality), reducing total work by ≥ 10×.
fn setup_engine(with_stats: bool) -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE fact (id INT NOT NULL, k1 INT NOT NULL, k2 INT NOT NULL, k3 INT NOT NULL, k4 INT NOT NULL)").unwrap();
    for tag in ["big1", "big2", "big3", "big4"] {
        e.execute(&format!("CREATE TABLE {tag} (k INT NOT NULL)")).unwrap();
    }
    for i in 0..N_FACT {
        e.execute(&format!(
            "INSERT INTO fact VALUES ({i}, {i}, {i}, {i}, {i})"
        ))
        .unwrap();
    }
    for tag in ["big1", "big2", "big3", "big4"] {
        for i in 0..N_BIG {
            e.execute(&format!("INSERT INTO {tag} VALUES ({i})")).unwrap();
        }
    }
    if with_stats {
        e.execute("ANALYZE").unwrap();
    }
    e
}

/// Source order: bigs first connected only by trivial `1=1`
/// predicates, fact joined LAST with the real edges. A naive
/// planner runs a 4-way cartesian of bigs (40⁴ ≈ 2.5M) then
/// filters via fact (5 rows × 2.5M ≈ 12.5M iterations).
fn five_table_join_sql() -> &'static str {
    "SELECT fact.id FROM big1 \
     INNER JOIN big2 ON 1 = 1 \
     INNER JOIN big3 ON 1 = 1 \
     INNER JOIN big4 ON 1 = 1 \
     INNER JOIN fact \
       ON fact.k1 = big1.k \
       AND fact.k2 = big2.k \
       AND fact.k3 = big3.k \
       AND fact.k4 = big4.k"
}

#[test]
fn five_table_join_speedup_vs_source_order() {
    let mut eng_with_stats = setup_engine(true);
    let mut eng_no_stats = setup_engine(false);
    let sql = five_table_join_sql();
    // Warm both engines once each (allocator warmup) before the
    // timed run.
    let _ = eng_with_stats.execute(sql).expect("warmup reordered");
    let _ = eng_no_stats.execute(sql).expect("warmup baseline");

    let t0 = Instant::now();
    let r = eng_with_stats.execute(sql).expect("reordered SELECT");
    let reordered_ns = t0.elapsed().as_nanos();
    std::hint::black_box(r);

    let t0 = Instant::now();
    let r = eng_no_stats.execute(sql).expect("baseline SELECT");
    let baseline_ns = t0.elapsed().as_nanos();
    std::hint::black_box(r);

    let speedup = baseline_ns as f64 / reordered_ns.max(1) as f64;
    eprintln!(
        "five_table_join: baseline={baseline_ns} ns, reordered={reordered_ns} ns, speedup={speedup:.1}×"
    );
    assert!(
        speedup >= 10.0,
        "v6.2.3 ship-gate: 5-table JOIN reorder speedup must be ≥ 10×; got {speedup:.1}×"
    );
}
