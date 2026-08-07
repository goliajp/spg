//! v7.39 (round 562) — the index-only walk keeps the header leaf it is
//! standing on.
//!
//! Round 561 left the wire-side residual unnamed, so this round profiled
//! the server serving the 100k-row range instead of guessing. The
//! connection thread's CPU went 63-65% SPG, 16-18% allocator, 7-8%
//! memcpy, 10-13% kernel — and inside SPG the index walk dominated while
//! the row ENCODING, the thing round 561 attacked, was 7%.
//!
//! Two things came out of that and both are here:
//!
//!   * the range collected 100k `(key, locator)` pairs into a `Vec` to
//!     walk once and drop — it hands back the walk now;
//!   * the headers are a `PersistentVec`, a 32-way trie, so testing
//!     visibility BY POSITION is four dependent loads per row. A
//!     sequential scan never pays that (it walks rows and headers in
//!     lockstep); an index walk cannot, but its positions arrive
//!     ascending and a leaf holds 32, so the leaf is kept between rows.
//!
//! Measured over pgwire, six paired batches (three at 100k rows out,
//! three at 400k), alternating binaries on one data directory:
//!
//!     100k   before 18.42 / 18.70 / 17.96 ms    after 18.15 / 17.81 / 17.21
//!     400k   before 59.71 / 56.62 / 56.72       after 56.90 / 55.12 / 55.47
//!
//! Lower in 6 of 6 — real, and about 3%. Which is an order of magnitude
//! LESS than the profile line appeared to promise, and that gap is the
//! lesson: with line-tables-only debug info the collect's line was
//! absorbing the whole B-tree traversal inlined into it. Removing the
//! collect moved those samples into the loop body rather than removing
//! them. The traversal of the persistent trie is the cost, and naming it
//! is the next round's job, not a fourth attack on its neighbours.
//!
//! What the pins below are for: the round-560 visibility pins run on a
//! 200-row table, which fits in ONE trie leaf, so they would pass
//! whether the cursor handled a leaf boundary or not. These cross
//! leaves, and read them out of position order.

use spg_engine::{Engine, QueryResult, TxId};

const IMPLICIT: TxId = TxId(0);

fn vals(e: &mut Engine, sql: &str) -> Vec<i64> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                spg_engine::eval::value_to_text(&r.values[0])
                    .parse()
                    .expect("integer")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// Without an ORDER BY no ordering is promised — PG does not promise one
/// either — so a whole-table check compares the SET of visible values.
fn sorted(e: &mut Engine, sql: &str) -> Vec<i64> {
    let mut v = vals(e, sql);
    v.sort_unstable();
    v
}

/// 2000 rows — 63 leaves of 32 — with a scattered third deleted, so the
/// visible set changes inside leaves and across their boundaries.
#[test]
fn round562_visibility_across_leaf_boundaries() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE r (id INT, k INT)").unwrap();
    e.execute("INSERT INTO r SELECT g, g FROM generate_series(1, 2000) g")
        .unwrap();
    e.execute("CREATE INDEX rk ON r (k)").unwrap();
    e.execute("DELETE FROM r WHERE k % 3 = 0").unwrap();

    let expected: Vec<i64> = (1..=2000).filter(|k| k % 3 != 0).collect();
    assert_eq!(sorted(&mut e, "SELECT k FROM r WHERE k > 0"), expected);

    // A range that starts and ends mid-leaf, and one that spans exactly
    // a boundary.
    assert_eq!(
        vals(&mut e, "SELECT k FROM r WHERE k BETWEEN 30 AND 40"),
        vec![31, 32, 34, 35, 37, 38, 40]
    );
    assert_eq!(
        vals(&mut e, "SELECT k FROM r WHERE k BETWEEN 1 AND 5"),
        vec![1, 2, 4, 5]
    );
    // The same answer the ordinary path gives, on the same range.
    assert_eq!(
        vals(&mut e, "SELECT id FROM r WHERE k BETWEEN 30 AND 40"),
        vec![31, 32, 34, 35, 37, 38, 40]
    );
}

/// When the index order runs OPPOSITE to insertion order, every step is
/// a leaf miss — the cursor must answer the same as descending per row.
#[test]
fn round562_positions_out_of_order() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d (id INT, k INT)").unwrap();
    // k descends as position ascends, so the B-tree hands back positions
    // 999, 998, … — the run held from the last row never contains the
    // next one until the walk crosses back into it.
    e.execute("INSERT INTO d SELECT g, 1000 - g FROM generate_series(1, 999) g")
        .unwrap();
    e.execute("CREATE INDEX dk ON d (k)").unwrap();
    e.execute("DELETE FROM d WHERE k % 7 = 0").unwrap();

    let expected: Vec<i64> = (1..=999).filter(|k| k % 7 != 0).collect();
    assert_eq!(sorted(&mut e, "SELECT k FROM d WHERE k > 0"), expected);
    assert_eq!(
        vals(&mut e, "SELECT k FROM d WHERE k BETWEEN 40 AND 50"),
        vec![40, 41, 43, 44, 45, 46, 47, 48, 50]
    );
}

/// An UPDATE appends a new version and tombstones the old, so the same
/// range must cross leaves whose visible rows moved to the end.
#[test]
fn round562_updates_move_positions() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT, k INT)").unwrap();
    e.execute("INSERT INTO u SELECT g, g FROM generate_series(1, 500) g")
        .unwrap();
    e.execute("CREATE INDEX uk ON u (k)").unwrap();
    // Move a scattered set past the end of the range; the new versions
    // live at positions 500.. while the tombstones stay where they were.
    e.execute("UPDATE u SET k = k + 10000 WHERE k % 5 = 0")
        .unwrap();

    let expected: Vec<i64> = (1..=500).filter(|k| k % 5 != 0).collect();
    assert_eq!(
        sorted(&mut e, "SELECT k FROM u WHERE k BETWEEN 1 AND 9999"),
        expected
    );
    let moved: Vec<i64> = (1..=500)
        .filter(|k| k % 5 == 0)
        .map(|k| k + 10000)
        .collect();
    assert_eq!(sorted(&mut e, "SELECT k FROM u WHERE k > 9999"), moved);
}

/// A second transaction's uncommitted rows stay invisible across leaves.
#[test]
fn round562_mvcc_across_leaves() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE m (id INT, k INT)").unwrap();
    e.execute("INSERT INTO m SELECT g, g FROM generate_series(1, 800) g")
        .unwrap();
    e.execute("CREATE INDEX mk ON m (k)").unwrap();
    let (t1, t2) = (TxId(61), TxId(62));
    e.execute_in("BEGIN", t1).unwrap();
    e.execute_in("DELETE FROM m WHERE k % 2 = 0", t1).unwrap();

    let seen =
        |e: &mut Engine, tx: TxId| match e.execute_in("SELECT k FROM m WHERE k > 0", tx).unwrap() {
            QueryResult::Rows { rows, .. } => rows.len(),
            other => panic!("{other:?}"),
        };
    assert_eq!(seen(&mut e, t1), 400, "its own delete");
    e.execute_in("BEGIN", t2).unwrap();
    assert_eq!(seen(&mut e, t2), 800, "not another transaction's");
    e.execute_in("COMMIT", t1).unwrap();
    e.execute_in("COMMIT", t2).unwrap();
    assert_eq!(sorted(&mut e, "SELECT k FROM m WHERE k > 0").len(), 400);
}
