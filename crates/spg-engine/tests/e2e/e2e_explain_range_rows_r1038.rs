//! r1035/r1038 — the `rows=` on a range scan, and whose conjunct it is.
//!
//! mailrs measured `est_scan_rows` answering `n / 3` to everything that
//! is not equality: a fixture with 50 matching rows and one with 10,000
//! produced byte-identical estimates and costs, and `ANALYZE` moved
//! neither. r1035 made an indexed range ask the index instead, under the
//! same cap the executor uses so EXPLAIN never costs more than the query.
//!
//! r1038 then had to answer a second question the first change skipped:
//! WHICH conjunct is counted. `WHERE id = 7 AND ts > <x>` seeks the
//! equality and filters the range, and counting the range there printed
//!
//!     Index Cond: (id = 7)   ... rows=200
//!
//! — an estimate for work the plan does not do. The count now comes from
//! the conjunct the node reports as its Index Cond.
//!
//! The round551 note "the row estimate for a range still reads 1 where
//! 150 match" is the defect these close.

use spg_engine::{Engine, QueryResult};

/// 2,000 rows, `ts` ascending with `id`, so `ts > 1800` is exactly 200 of
/// them — inside the quarter-of-the-table cap, and nothing like `n / 3`.
fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE r38 (id INT PRIMARY KEY, ts BIGINT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO r38 SELECT g, g FROM generate_series(1, 2000) g")
        .unwrap();
    e.execute("CREATE INDEX r38ts ON r38 (ts)").unwrap();
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

/// Counted, not guessed — and it moves with selectivity, which is the
/// whole complaint.
#[test]
fn r1038_range_rows_come_from_the_index() {
    let mut e = engine();
    let wide = plan(&mut e, "EXPLAIN SELECT id FROM r38 WHERE ts > 1800");
    assert!(wide[0].contains("rows=200"), "{wide:?}");
    let narrow = plan(&mut e, "EXPLAIN SELECT id FROM r38 WHERE ts > 1990");
    assert!(narrow[0].contains("rows=10"), "{narrow:?}");
    // `n / 3` for this table is 666; neither estimate may be it.
    assert!(!wide[0].contains("rows=666"), "{wide:?}");
}

/// A residual conjunct beside the range — mailrs's reported shape — keeps
/// the range's real count, because the range IS what is seeked.
#[test]
fn r1038_residual_beside_the_range_keeps_the_count() {
    let mut e = engine();
    let p = plan(
        &mut e,
        "EXPLAIN SELECT id FROM r38 WHERE ts > 1800 AND ts IS NOT NULL",
    );
    assert!(p[0].contains("rows=200"), "{p:?}");
    assert!(
        p.iter().any(|l| l.contains("Index Cond: (ts > 1800)")),
        "{p:?}"
    );
    assert!(
        p.iter().any(|l| l.contains("Filter: (ts IS NOT NULL)")),
        "{p:?}"
    );
}

/// An equality beside a range: the plan seeks the equality, so the
/// estimate is the equality's, not the range's.
#[test]
fn r1038_equality_beside_a_range_is_counted_as_the_equality() {
    let mut e = engine();
    let p = plan(
        &mut e,
        "EXPLAIN SELECT id FROM r38 WHERE id = 7 AND ts > 1800",
    );
    assert!(
        p.iter().any(|l| l.contains("Index Cond: (id = 7)")),
        "{p:?}"
    );
    assert!(p[0].contains("rows=1"), "{p:?}");
}

/// Both halves of a BETWEEN are one seek. Each half became seekable on
/// its own in r1035, which made the conjunct split claim one and print
/// the other as a Filter — a re-check that never happens.
#[test]
fn r1038_between_is_one_index_cond_and_one_count() {
    let mut e = engine();
    let p = plan(
        &mut e,
        "EXPLAIN SELECT id FROM r38 WHERE ts >= 100 AND ts <= 199",
    );
    assert!(p[0].contains("rows=100"), "{p:?}");
    assert!(!p.iter().any(|l| l.contains("Filter:")), "{p:?}");
}
