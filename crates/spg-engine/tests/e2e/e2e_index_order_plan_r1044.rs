//! r1044 — `EXPLAIN` names the index-order walk that the executor runs.
//!
//! It did not. `SELECT pad FROM t ORDER BY id` on a 400,000-row table
//! planned as `Sort` over `Seq Scan` while the executor walked the
//! primary key: 34.9 ms against 147.0 for the same query ordered by an
//! unindexed column, so the walk was plainly running and the plan named
//! the wrong access path.
//!
//! Round 551 fixed a different case of exactly this and wrote the reason
//! down: EXPLAIN is the first thing any performance question opens, and
//! an instrument that misnames the access path is worse than one that
//! says nothing. This session hit the plan-versus-executor split three
//! times; the answer each time is that the decision has to live in ONE
//! place, and here that is `Engine::index_order_walk_target`.
//!
//! What is pinned is agreement, not a duration: the plan says walk
//! exactly when the gate says walk, and says sort otherwise.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    // r1046 — `k` is nullable AND carries NULLs. It walks now: the
    // walk emits the NULL rows itself, at the end SQL puts them.
    // `m` has no index — the control for a shape that must still sort.
    e.execute("CREATE TABLE io (id INT PRIMARY KEY, k INT, j INT NOT NULL, m INT)")
        .unwrap();
    e.execute(
        "INSERT INTO io SELECT g, CASE WHEN g % 7 = 0 THEN NULL ELSE g % 100 END, \
         g % 50, g % 13 FROM generate_series(1, 500) g",
    )
    .unwrap();
    e.execute("CREATE INDEX io_k ON io (k)").unwrap();
    e.execute("CREATE INDEX io_j ON io (j)").unwrap();
    e
}

fn plan(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The walk is named, and no Sort is claimed above it.
#[test]
fn r1044_a_walked_order_is_planned_as_an_index_scan() {
    let mut e = engine();
    for (sql, idx) in [
        ("EXPLAIN SELECT id FROM io ORDER BY id", "io_pkey"),
        ("EXPLAIN SELECT id FROM io ORDER BY id DESC", "io_pkey"),
        ("EXPLAIN SELECT id FROM io ORDER BY j", "io_j"),
        // r1046 — nullable, and carrying NULLs.
        ("EXPLAIN SELECT id FROM io ORDER BY k", "io_k"),
    ] {
        let p = plan(&mut e, sql);
        assert!(
            p[0].contains(&alloc_fmt(idx)),
            "the walk was not named: {p:?}"
        );
        assert!(
            !p.iter().any(|l| l.starts_with("Sort")),
            "a walked order still claimed a Sort: {p:?}"
        );
    }
}

fn alloc_fmt(idx: &str) -> String {
    format!("Index Scan using {idx}")
}

/// And a sort is still a sort. Each of these fails the gate for a
/// different stated reason, so a plan that called any of them a walk
/// would be describing something the executor does not do.
#[test]
fn r1044_shapes_outside_the_gate_still_sort() {
    let mut e = engine();
    for sql in [
        // No index on the column at all.
        "EXPLAIN SELECT id FROM io ORDER BY m",
        // LIMIT has its own top-N path.
        "EXPLAIN SELECT id FROM io ORDER BY id LIMIT 10",
        // DISTINCT is not part of the walk.
        "EXPLAIN SELECT DISTINCT j FROM io ORDER BY j",
        // Two keys: the index orders one.
        "EXPLAIN SELECT id FROM io ORDER BY j, id",
    ] {
        let p = plan(&mut e, sql);
        assert!(
            p.iter().any(|l| l.trim_start().starts_with("Sort")),
            "expected a Sort for a shape outside the gate: {sql}\n{p:?}"
        );
    }
}

/// A walk costs less than the sort it replaces, which is the whole point
/// of naming it — a reader compares plans by their costs.
#[test]
fn r1044_the_walk_is_cheaper_than_the_sort_it_replaces() {
    let mut e = engine();
    let walk = plan(&mut e, "EXPLAIN SELECT id FROM io ORDER BY j");
    let sort = plan(&mut e, "EXPLAIN SELECT id FROM io ORDER BY m");
    let total = |line: &str| -> f64 {
        line.split("..")
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .and_then(|n| n.parse::<f64>().ok())
            .unwrap_or_else(|| panic!("no cost in {line}"))
    };
    let (w, s) = (total(&walk[0]), total(&sort[0]));
    assert!(w < s, "walk {w} should cost less than sort {s}");
    // And the total is above the startup — the first version of this
    // node read its cost off a child it did not have and printed
    // `cost=0.15..0.00`, a total below its own startup.
    let startup = walk[0]
        .split("cost=")
        .nth(1)
        .and_then(|r| r.split("..").next())
        .and_then(|n| n.parse::<f64>().ok())
        .expect("startup");
    assert!(w >= startup, "total {w} below startup {startup}: {walk:?}");
}
