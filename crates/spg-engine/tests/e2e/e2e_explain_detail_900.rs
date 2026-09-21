//! 9.0.0 — four EXPLAIN details, three closed and one narrowed.
//!
//! Measured on PostgreSQL 18.6 with
//! `(ANALYZE, BUFFERS, COSTS OFF, TIMING OFF, SUMMARY OFF)` over 500
//! rows:
//!
//! ```text
//!   Limit (actual rows=10.00 loops=1)
//!     Buffers: shared hit=6
//!     ->  Sort (actual rows=10.00 loops=1)
//!           Sort Key: v
//!           Sort Method: top-N heapsort  Memory: 17kB
//!           ->  Seq Scan on c5t s (actual rows=500.00 loops=1)
//! ```
//!
//! SPG answered `-> Sort` with no actuals, `Sort Key: s.v`, a
//! `Sort Method` with no memory, and a `Buffers:` line of its own
//! vocabulary. What is fixed here:
//!
//!   * a Sort under a Limit reports the rows the Limit took — which is
//!     exactly what it emitted, and the Limit had already counted them;
//!   * a sort key is qualified only when more than one relation is in
//!     scope, as PG's is (`ORDER BY s.v` over one table is `Sort Key: v`);
//!   * an in-memory sort reports the bytes it held, from the sorter's own
//!     high-water mark.
//!
//! What is NOT: per-node `Buffers: shared hit=N` and the `Planning:`
//! block, which count 8 kB blocks in a buffer pool SPG does not have.
//! The line it used to print instead is gone (see `e2e_explain_options`).

use spg_engine::{Engine, QueryResult};

fn plan(e: &mut Engine, sql: &str) -> String {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.iter()
        .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
        .collect::<Vec<_>>()
        .join("\n")
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE c5t (id int, v text, n int)")
        .expect("table");
    for i in 1..=500 {
        e.execute(&format!("INSERT INTO c5t VALUES ({i}, 'x{i}', {})", i % 7))
            .expect("insert");
    }
    e
}

#[test]
fn a_sort_under_a_limit_reports_the_rows_the_limit_took() {
    let mut e = seeded();
    let p = plan(
        &mut e,
        "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF) \
         SELECT v FROM c5t s ORDER BY s.v LIMIT 10",
    );
    let sort = p
        .lines()
        .find(|l| l.trim_start().starts_with("->  Sort"))
        .unwrap_or_else(|| panic!("no Sort node:\n{p}"));
    assert!(
        sort.contains("(actual rows=10.00 loops=1)"),
        "the Sort carries no actuals: {sort}"
    );
}

#[test]
fn a_sort_key_is_qualified_only_when_it_has_to_be() {
    let mut e = seeded();
    let p = plan(
        &mut e,
        "EXPLAIN (COSTS OFF) SELECT v FROM c5t s ORDER BY s.v",
    );
    assert!(p.contains("Sort Key: v"), "{p}");
    assert!(!p.contains("Sort Key: s.v"), "{p}");
    // Two relations in scope: PG qualifies, and so must SPG.
    e.execute("CREATE TABLE c5u (id int, w text)").expect("t");
    let j = plan(
        &mut e,
        "EXPLAIN (COSTS OFF) SELECT s.v FROM c5t s JOIN c5u u ON u.id = s.id ORDER BY s.v",
    );
    assert!(j.contains("Sort Key: s.v"), "{j}");
}

#[test]
fn an_in_memory_sort_reports_the_memory_it_held() {
    let mut e = seeded();
    let p = plan(
        &mut e,
        "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF) \
         SELECT v FROM c5t s ORDER BY s.v",
    );
    let line = p
        .lines()
        .find(|l| l.trim_start().starts_with("Sort Method:"))
        .unwrap_or_else(|| panic!("no Sort Method line:\n{p}"))
        .trim()
        .to_string();
    assert!(
        line.starts_with("Sort Method: quicksort  Memory: ") && line.ends_with("kB"),
        "PG's shape is `Sort Method: quicksort  Memory: 25kB`, got: {line}"
    );
    // A measurement, not a placeholder.
    let kb: u64 = line
        .rsplit_once("Memory: ")
        .and_then(|(_, m)| m.trim_end_matches("kB").parse().ok())
        .unwrap_or_else(|| panic!("unreadable memory figure: {line}"));
    assert!(kb > 0, "a sort that held nothing: {line}");
}

#[test]
fn buffers_prints_no_line_postgresql_does_not_write() {
    let mut e = seeded();
    let p = plan(
        &mut e,
        "EXPLAIN (ANALYZE, BUFFERS, COSTS OFF, TIMING OFF, SUMMARY OFF) \
         SELECT v FROM c5t s ORDER BY s.v LIMIT 10",
    );
    assert!(
        !p.contains("hot_rows") && !p.contains("cache_hit_ratio"),
        "SPG's own buffer vocabulary: {p}"
    );
}
