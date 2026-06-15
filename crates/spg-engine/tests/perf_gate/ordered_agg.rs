//! v7.33 (array_agg perf) — the ordered-aggregate borrow-channel gate.
//! `array_agg(x ORDER BY y)` over join/grouped input must read its sort
//! key (and value arg) by reference, NOT materialise the whole combined
//! row per input row. Pre-fix, the bound aggregate fast path called
//! `RowRef::as_row()` once PER input row PER ordered/filtered sub-eval
//! just to read one bound column — on the inbox shape that cloned a
//! ~1 KB combined row 24k times (≈ half the array_agg segment cost).
//!
//! The aggregate input MUST be a join (RowRef::Tuple): for a single
//! table the rows are RowRef::Owned and `as_row()` is already a free
//! borrow, so only the join path exercised the regression. This gate
//! joins a fat driving table to a small dim table, puts a FAT body
//! column in the driver but keeps it out of the aggregate, and GROUPs +
//! array_aggs over thin columns. A per-row full-row materialisation
//! would clone the fat combined tuple every input row; the borrow path
//! touches only the thin sort key + value, so allocs stay ~O(rows)
//! independent of the fat column.

use crate::{ALLOC_CALLS, perf_lock};
use spg_engine::{Engine, QueryResult};
use std::sync::atomic::Ordering;

const ROWS: usize = 4_000;
const GROUPS: usize = 200;
const BODY_BYTES: usize = 2 * 1024; // fat column NOT touched by the agg
/// Borrow path: ~O(rows) small allocs for the thin key/value + per-group
/// arrays. A per-row full-row materialisation would add a ~2 KB clone per
/// row (∝ BODY_BYTES) and blow past this. 12/row leaves headroom over the
/// measured borrow-path count while tripping on the materialise regression.
const ALLOC_PER_ROW_BUDGET: usize = 12;

#[test]
fn ordered_aggregate_reads_sort_key_by_reference() {
    let _g = perf_lock();
    let mut eng = Engine::new();
    // Driver `t` carries the fat body; dim `d` is tiny. The join makes
    // the aggregate input a RowRef::Tuple, so a per-row as_row() would
    // materialise the fat combined tuple.
    eng.execute("CREATE TABLE t (g BIGINT, rank BIGINT, label TEXT, dim BIGINT, body TEXT)")
        .unwrap();
    eng.execute("CREATE TABLE d (id BIGINT, name TEXT)")
        .unwrap();
    eng.execute("INSERT INTO d VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d')")
        .unwrap();
    let body = "x".repeat(BODY_BYTES);
    let mut i = 0usize;
    while i < ROWS {
        let mut stmt = String::with_capacity(40 * BODY_BYTES);
        stmt.push_str("INSERT INTO t VALUES ");
        for k in 0..40 {
            let id = i + k;
            if k > 0 {
                stmt.push(',');
            }
            // g = group key, rank = sort key, label = agg value, dim = join
            // key, body = fat column present in the row but never referenced.
            stmt.push_str(&format!(
                "({}, {}, 'L{}', {}, '{body}')",
                id % GROUPS,
                id,
                id % 7,
                id % 4 + 1
            ));
        }
        eng.execute(&stmt).unwrap();
        i += 40;
    }

    // array_agg with an internal ORDER BY over a JOIN — the ordered-
    // aggregate path on RowRef::Tuple. `body` is NOT referenced, so a
    // correct borrow path never clones it.
    let sql = "SELECT t.g, array_agg(t.label ORDER BY t.rank DESC) \
               FROM t JOIN d ON t.dim = d.id GROUP BY t.g";
    eng.execute(sql).unwrap(); // warm

    let a0 = ALLOC_CALLS.load(Ordering::Relaxed);
    let r = eng.execute(sql).unwrap();
    let allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a0;
    match r {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), GROUPS),
        other => panic!("unexpected result: {other:?}"),
    }
    let per_row = allocs as f64 / ROWS as f64;
    assert!(
        per_row <= ALLOC_PER_ROW_BUDGET as f64,
        "ordered aggregate allocated {per_row:.1}/row ({allocs} for {ROWS} rows) — \
         budget {ALLOC_PER_ROW_BUDGET}/row; the per-row full-row materialisation \
         (cloning the untouched {BODY_BYTES}-byte body) regressed back in"
    );
}
