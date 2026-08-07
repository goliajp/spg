//! v7.39 (round 565) — EXPLAIN names the scan that actually runs.
//!
//! Round 560 taught the executor to answer a range out of the index
//! alone; round 564 measured the two scans 2x apart at 50k rows out.
//! EXPLAIN called both of them `Index Scan`, so the two plans read
//! identically to anyone comparing them — and EXPLAIN is the first thing
//! a performance question opens. Round 551 closed the same class of
//! defect (an index named that could not serve the predicate) and this
//! one was self-inflicted three rounds later: a path was added to the
//! executor and EXPLAIN was not told.
//!
//! PG18, measured:
//!
//!     EXPLAIN          Index Only Scan using i560k on i560
//!                        Index Cond: ((k >= 1) AND (k <= 100))
//!     EXPLAIN ANALYZE  … the same, plus
//!                        Heap Fetches: 0
//!
//! So the name changes under a plain EXPLAIN and `Heap Fetches` appears
//! only under ANALYZE, where it is a counter rather than a plan
//! property. Zero is the measured truth for SPG rather than a
//! placeholder: this path never reads a row, which is the whole reason
//! the node has its own name.
//!
//! The question EXPLAIN asks is the executor's own — the statement shape
//! test and the pre-walk checks, both called rather than restated. A
//! plan that names a path the executor would not take is the same defect
//! wearing the other face, so the pins below include every shape that
//! must still say `Index Scan`.
//!
//! Recorded, not done at the time: PG says `Index Only Scan` for
//! `WHERE k = 5` too, and SPG's fast path was range-only, so its plan
//! said `Index Scan` there — honest about what ran, and a gap in the
//! path rather than in EXPLAIN. Round 566 closed the path, so the pin
//! below now expects the same node PG names.

use spg_engine::{Engine, QueryResult};

fn plan(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p565 (id INT, k INT, pad TEXT)")
        .unwrap();
    e.execute("INSERT INTO p565 SELECT g, g, 'x' FROM generate_series(1, 300) g")
        .unwrap();
    e.execute("CREATE INDEX p565k ON p565 (k)").unwrap();
    e
}

#[test]
fn round565_index_only_range_is_named() {
    let mut e = engine();
    let p = plan(
        &mut e,
        "EXPLAIN SELECT k FROM p565 WHERE k BETWEEN 1 AND 100",
    );
    assert!(
        p[0].starts_with("Index Only Scan using p565k on p565"),
        "{p:?}"
    );
    assert!(p.iter().any(|l| l.contains("Index Cond:")), "{p:?}");
    // A counter, not a plan property — PG does not print it here.
    assert!(
        !p.iter().any(|l| l.contains("Heap Fetches")),
        "a plain EXPLAIN has no Heap Fetches line in PG: {p:?}"
    );
}

#[test]
fn round565_analyze_adds_heap_fetches() {
    let mut e = engine();
    let p = plan(
        &mut e,
        "EXPLAIN ANALYZE SELECT k FROM p565 WHERE k BETWEEN 1 AND 100",
    );
    assert!(
        p[0].starts_with("Index Only Scan using p565k on p565"),
        "{p:?}"
    );
    assert!(p.iter().any(|l| l.trim() == "Heap Fetches: 0"), "{p:?}");
}

/// Everything the executor would NOT answer from the index alone keeps
/// the old name. A plan naming a path that does not run is the same
/// defect from the other side.
#[test]
fn round565_other_shapes_stay_index_scan() {
    let mut e = engine();
    let head = |e: &mut Engine, sql: &str| plan(e, sql).join("\n");

    // Projecting a different column reads the row. On a narrow range the
    // seek is worth taking and the plan says Index Scan; either way it
    // must never claim to answer out of the index alone.
    let fetch = head(
        &mut e,
        "EXPLAIN SELECT id FROM p565 WHERE k BETWEEN 1 AND 2",
    );
    assert!(fetch.contains("Index Scan using p565k on p565"), "{fetch}");
    assert!(!fetch.contains("Index Only Scan"), "{fetch}");
    assert!(
        !head(
            &mut e,
            "EXPLAIN SELECT id FROM p565 WHERE k BETWEEN 1 AND 100"
        )
        .contains("Index Only Scan")
    );
    // Equality reached the fast path in round 566 — the degenerate
    // range — so it names what PG names.
    let eq = head(&mut e, "EXPLAIN SELECT k FROM p565 WHERE k = 5");
    assert!(eq.contains("Index Only Scan using p565k on p565"), "{eq}");
    // …but reading a different column still fetches the row.
    let eq_fetch = head(&mut e, "EXPLAIN SELECT id FROM p565 WHERE k = 5");
    assert!(!eq_fetch.contains("Index Only Scan"), "{eq_fetch}");
    // ORDER BY / LIMIT / DISTINCT are outside the shape.
    for sql in [
        "EXPLAIN SELECT k FROM p565 WHERE k BETWEEN 1 AND 100 ORDER BY k",
        "EXPLAIN SELECT k FROM p565 WHERE k BETWEEN 1 AND 100 LIMIT 5",
        "EXPLAIN SELECT DISTINCT k FROM p565 WHERE k BETWEEN 1 AND 100",
    ] {
        let text = head(&mut e, sql);
        assert!(
            !text.contains("Index Only Scan"),
            "{sql} must not claim an index-only scan: {text}"
        );
    }
    // Two projected columns.
    assert!(
        !head(
            &mut e,
            "EXPLAIN SELECT k, id FROM p565 WHERE k BETWEEN 1 AND 100"
        )
        .contains("Index Only Scan")
    );
    // An expression over the column is not the bare column.
    assert!(
        !head(
            &mut e,
            "EXPLAIN SELECT k + 1 FROM p565 WHERE k BETWEEN 1 AND 100"
        )
        .contains("Index Only Scan")
    );
}

/// A type whose key does not restore it takes the ordinary path, and the
/// plan says so — the same rule the executor applies, not a second copy.
#[test]
fn round565_unrestorable_type_is_not_named_index_only() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d565 (d DATE, t TEXT)").unwrap();
    e.execute("INSERT INTO d565 VALUES ('2026-01-01', 'a'), ('2026-01-02', 'b')")
        .unwrap();
    e.execute("CREATE INDEX d565d ON d565 (d)").unwrap();
    let p = plan(
        &mut e,
        "EXPLAIN SELECT d FROM d565 WHERE d BETWEEN '2026-01-01' AND '2026-01-02'",
    );
    assert!(!p.join("\n").contains("Index Only Scan"), "{p:?}");

    // Text does restore.
    e.execute("CREATE INDEX d565t ON d565 (t)").unwrap();
    let p = plan(
        &mut e,
        "EXPLAIN SELECT t FROM d565 WHERE t BETWEEN 'a' AND 'b'",
    );
    assert!(
        p[0].starts_with("Index Only Scan using d565t on d565"),
        "{p:?}"
    );
}

/// The JSON form carries the node type too — a visualiser reading it
/// must see the same distinction the text form makes.
#[test]
fn round565_json_carries_the_node_type() {
    let mut e = engine();
    let j = plan(
        &mut e,
        "EXPLAIN (FORMAT JSON) SELECT k FROM p565 WHERE k BETWEEN 1 AND 100",
    )
    .join("");
    assert!(j.contains("\"Node Type\": \"Index Only Scan\""), "{j}");
    assert!(j.contains("\"Index Name\": \"p565k\""), "{j}");
    let j = plan(
        &mut e,
        "EXPLAIN (FORMAT JSON) SELECT id FROM p565 WHERE k BETWEEN 1 AND 2",
    )
    .join("");
    assert!(j.contains("\"Node Type\": \"Index Scan\""), "{j}");
}
