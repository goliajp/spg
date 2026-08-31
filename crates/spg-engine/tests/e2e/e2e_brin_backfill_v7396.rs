//! v7.39.6 — a BRIN index created on a table that already has rows
//! summarises them.
//!
//! Summaries were only ever widened by an INSERT. An index created
//! afterwards — which is how an index is normally added — held none,
//! and an absent summary is read as "cannot be skipped". The index then
//! cost every write and pruned nothing, for the life of the table.
//! Measured on the published 7.39.5 image, 200,000 rows, a predicate
//! matching none of them:
//!
//! ```text
//!                                        SPG        PG18
//!   WHERE v > 999999999, no index      7.42 ms     4.68 ms
//!   … index created AFTER the rows     7.42 ms     0.34 ms
//!   … index created BEFORE the rows    0.27 ms
//! ```
//!
//! `CHECKPOINT` did not repair it, and neither did a restart.
//!
//! These pins do not time anything and do not read a counter. They ask
//! the storage layer the question the scan asks — which slots may this
//! predicate skip — and check that the answer is the narrow one. A pin
//! on wall clock would be a pin on the machine; a pin on
//! `PROBE_PRUNED` would fire whether or not the summaries said
//! anything.

use spg_engine::Engine;

/// `BRIN_RANGE_ROWS` is 1024, so this is several full slots plus a
/// partial one — enough that "skip everything" and "skip nothing" are
/// different answers.
const ROWS: i64 = 5000;

fn seeded(order: Order) -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE b (id INT PRIMARY KEY, v INT)")
        .unwrap();
    if matches!(order, Order::IndexFirst) {
        e.execute("CREATE INDEX bx ON b USING brin (v)").unwrap();
    }
    e.execute(&format!(
        "INSERT INTO b SELECT g, g FROM generate_series(1, {ROWS}) g"
    ))
    .unwrap();
    if matches!(order, Order::RowsFirst) {
        e.execute("CREATE INDEX bx ON b USING brin (v)").unwrap();
    }
    e
}

enum Order {
    RowsFirst,
    IndexFirst,
}

/// The slots the scan would have to visit for `v > bound`, as the
/// storage layer computes them.
fn slots_above(e: &Engine, bound: i64) -> Vec<core::ops::Range<usize>> {
    let table = e.catalog().get("b").expect("table b");
    let col = table.schema().column_position("v").expect("column v");
    table
        .brin_candidate_slots(col, Some(bound), None)
        .expect("a BRIN index over v exists, so the layer has an opinion")
}

#[test]
fn an_index_created_after_the_rows_can_skip_them() {
    let e = seeded(Order::RowsFirst);
    // Every row is <= ROWS, so nothing can match and every slot is
    // provably disjoint from the predicate.
    let slots = slots_above(&e, ROWS + 1);
    assert!(
        slots.is_empty(),
        "a predicate matching no row left {} slot range(s) to scan — the \
         index created after the rows summarised none of them",
        slots.len()
    );
}

#[test]
fn it_matches_what_creating_it_first_would_have_given() {
    // The same table built the other way round is the control: if the
    // backfill and the per-insert widening disagree, one of them is
    // wrong.
    let after = seeded(Order::RowsFirst);
    let before = seeded(Order::IndexFirst);
    for bound in [-1, 0, 1, ROWS / 2, ROWS - 1, ROWS, ROWS + 1, i64::MAX] {
        assert_eq!(
            slots_above(&after, bound),
            slots_above(&before, bound),
            "the two build orders disagree at v > {bound}"
        );
    }
}

#[test]
fn a_predicate_that_matches_keeps_the_slots_holding_its_rows() {
    // The danger in a skip is never in keeping too much. This is the
    // other direction: the last slot must survive a bound inside it,
    // and a bound below everything must keep every slot.
    let e = seeded(Order::RowsFirst);
    let all = slots_above(&e, -1);
    let covered: usize = all.iter().map(|r| r.end - r.start).sum();
    assert_eq!(
        covered, ROWS as usize,
        "a bound below every value must leave every row to scan"
    );
    let tail = slots_above(&e, ROWS - 1);
    assert!(
        !tail.is_empty() && tail.iter().any(|r| r.end == ROWS as usize),
        "the slot holding the matching rows must be kept, got {tail:?}"
    );
}

#[test]
fn the_answers_are_the_same_either_way() {
    // Correctness, which was never at risk — an absent summary is safe
    // — but the pins above would also pass if pruning became wrong, so
    // this says what the rows are.
    for order in [Order::RowsFirst, Order::IndexFirst] {
        let mut e = seeded(order);
        for (sql, want) in [
            ("SELECT count(*) FROM b WHERE v > 999999999", 0i64),
            ("SELECT count(*) FROM b WHERE v > 4990", 10),
            ("SELECT count(*) FROM b WHERE v BETWEEN 100 AND 199", 100),
            ("SELECT count(*) FROM b WHERE v < 1", 0),
        ] {
            let spg_engine::QueryResult::Rows { rows, .. } = e.execute(sql).unwrap() else {
                panic!("{sql}: expected Rows");
            };
            let got = match &rows[0].values[0] {
                spg_storage::Value::BigInt(n) => *n,
                spg_storage::Value::Int(n) => i64::from(*n),
                other => panic!("{sql}: {other:?}"),
            };
            assert_eq!(got, want, "{sql}");
        }
    }
}
