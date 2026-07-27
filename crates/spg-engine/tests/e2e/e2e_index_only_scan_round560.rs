//! v7.39 (round 560) — an index-only range scan.
//!
//! Phase 5's B2. Measuring it first found another loss no ledger entry
//! recorded: over pgwire on a 500k table, projecting the indexed column
//! for a 100k-row range —
//!
//!     PG18  Index Only Scan   3.6 ms      SPG  30 ms
//!
//! 8×, widening with the row count (2× at 1k rows). The shape of paying
//! per row for something the index already knows: the range walk holds
//! the KEY, throws it away, keeps the locator, and reads the row for a
//! value it had in hand.
//!
//! Serving the value from the key takes it to 18 ms. PG is still ahead
//! at 3.6, and the remainder is not this path: what is left is one
//! `Row` allocation per output row, which every executor shape pays.
//! Recorded rather than claimed closed.
//!
//! Why PG needs a visibility map for this and SPG does not: a heap
//! tuple carries its own visibility, so an index entry alone cannot say
//! whether the row is live, and PG reads the heap for any page its map
//! does not mark all-visible — a map that vacuum maintains and that is
//! stale between runs. SPG keeps a header array beside the rows, so the
//! locator answers visibility directly.
//!
//! The shape is narrow on purpose: one table, one projected column,
//! that column indexed, the WHERE a range on it. The type must be
//! reconstructible from its key too — `IndexKey::Int` holds an i64
//! whatever the column was declared as, and a date and a timestamp both
//! key as Int, so only the types that map back unambiguously take this
//! path.

use spg_engine::{Engine, QueryResult, TxId};

const IMPLICIT: TxId = TxId(0);

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE i (id INT, k INT, pad TEXT)").unwrap();
    e.execute("INSERT INTO i SELECT g, g, 'x' FROM generate_series(1, 200) g")
        .unwrap();
    e.execute("CREATE INDEX ik ON i (k)").unwrap();
    e
}

/// The values, in index order, and the same answer the ordinary path gives.
#[test]
fn round560_index_only_range_answers() {
    let mut e = engine();
    assert_eq!(
        rows(&mut e, "SELECT k FROM i WHERE k BETWEEN 5 AND 9"),
        vec!["5", "6", "7", "8", "9"]
    );
    // One-sided ranges too.
    assert_eq!(rows(&mut e, "SELECT k FROM i WHERE k > 197"), vec!["198", "199", "200"]);
    assert_eq!(rows(&mut e, "SELECT k FROM i WHERE k < 3"), vec!["1", "2"]);
    // An alias on the output, and a qualifier on the input.
    assert_eq!(
        rows(&mut e, "SELECT k AS n FROM i WHERE k BETWEEN 1 AND 2"),
        vec!["1", "2"]
    );
    assert_eq!(
        rows(&mut e, "SELECT t.k FROM i t WHERE t.k BETWEEN 1 AND 2"),
        vec!["1", "2"]
    );
}

/// The column keeps its declared type — an INT column must not come
/// back as a bigint because the index key is an i64.
#[test]
fn round560_the_declared_type_survives() {
    let mut e = engine();
    assert_eq!(
        rows(&mut e, "SELECT pg_typeof(k) FROM i WHERE k = 5"),
        vec!["integer"]
    );
    e.execute("CREATE TABLE b (v BIGINT, t TEXT)").unwrap();
    e.execute("INSERT INTO b VALUES (10, 'a'), (20, 'b')").unwrap();
    e.execute("CREATE INDEX bv ON b (v)").unwrap();
    e.execute("CREATE INDEX bt ON b (t)").unwrap();
    assert_eq!(
        rows(&mut e, "SELECT pg_typeof(v) FROM b WHERE v > 5 LIMIT 1"),
        vec!["bigint"]
    );
    assert_eq!(rows(&mut e, "SELECT t FROM b WHERE t > 'a'"), vec!["b"]);
}

/// Deleted and updated rows are excluded — visibility is the whole
/// reason PG needs a map here.
#[test]
fn round560_visibility_is_honoured() {
    let mut e = engine();
    e.execute("DELETE FROM i WHERE k IN (6, 7)").unwrap();
    assert_eq!(
        rows(&mut e, "SELECT k FROM i WHERE k BETWEEN 5 AND 9"),
        vec!["5", "8", "9"]
    );
    // An UPDATE appends a new version and tombstones the old; the range
    // must show the new value once, not both.
    e.execute("UPDATE i SET k = 1000 WHERE k = 8").unwrap();
    assert_eq!(
        rows(&mut e, "SELECT k FROM i WHERE k BETWEEN 5 AND 9"),
        vec!["5", "9"]
    );
    assert_eq!(rows(&mut e, "SELECT k FROM i WHERE k > 999"), vec!["1000"]);
}

/// A transaction sees its own writes; another does not.
#[test]
fn round560_mvcc_visibility() {
    let mut e = engine();
    let (t1, t2) = (TxId(91), TxId(92));
    e.execute_in("BEGIN", t1).unwrap();
    e.execute_in("INSERT INTO i VALUES (500, 500, 'z')", t1).unwrap();
    let seen = |e: &mut Engine, tx: TxId| match e
        .execute_in("SELECT k FROM i WHERE k > 400", tx)
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => rows.len(),
        other => panic!("{other:?}"),
    };
    assert_eq!(seen(&mut e, t1), 1, "its own insert");
    e.execute_in("BEGIN", t2).unwrap();
    assert_eq!(seen(&mut e, t2), 0, "not another transaction's");
    e.execute_in("COMMIT", t1).unwrap();
    e.execute_in("COMMIT", t2).unwrap();
    assert_eq!(rows(&mut e, "SELECT k FROM i WHERE k > 400"), vec!["500"]);
}

/// Everything outside the shape keeps the ordinary path and the same
/// answers.
#[test]
fn round560_other_shapes_are_untouched() {
    let mut e = engine();
    // A second projected column.
    assert_eq!(
        rows(&mut e, "SELECT k, id FROM i WHERE k BETWEEN 5 AND 6"),
        vec!["5|5", "6|6"]
    );
    // A column that is not the indexed one.
    assert_eq!(rows(&mut e, "SELECT id FROM i WHERE k = 5"), vec!["5"]);
    // An unindexed predicate column.
    assert_eq!(rows(&mut e, "SELECT id FROM i WHERE id = 5"), vec!["5"]);
    // ORDER BY / LIMIT / DISTINCT keep their own machinery.
    assert_eq!(
        rows(&mut e, "SELECT k FROM i WHERE k BETWEEN 5 AND 9 ORDER BY k DESC LIMIT 2"),
        vec!["9", "8"]
    );
    assert_eq!(
        rows(&mut e, "SELECT DISTINCT k FROM i WHERE k = 5"),
        vec!["5"]
    );
    // An expression over the column, not the bare column.
    assert_eq!(rows(&mut e, "SELECT k + 1 FROM i WHERE k = 5"), vec!["6"]);
}
