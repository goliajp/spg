//! Correlated select-list subquery: all-keys batch vs keyed-seek must
//! agree. v7.33 (mailrs 7.32.1) added a cost guard to
//! `try_batch_correlated_scalar`: when an outer query has NO LIMIT, every
//! group survives, so the deferred-subquery seeder hands the call a
//! restrict set the size of the whole group set. Doing one index seek per
//! key then dwarfs a single all-keys grouped scan, so the guard falls
//! through to the batch. This pins the invariant the guard relies on —
//! batch result ≡ keyed result for every covered key — so the perf switch
//! can never change an answer.
//!
//! Reproduced shape: mailrs `get_conversations_by_thread_ids`, a
//! `(SELECT … WHERE inner = outer ORDER BY … DESC LIMIT 1)` correlated
//! scalar in the select list of a GROUP BY with no outer LIMIT.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn setup() -> Engine {
    let mut e = Engine::new();
    // 8 threads, 3 messages each = 24 rows. With no outer LIMIT the
    // subquery seeder sees 8 surviving keys against a 24-row driver
    // (8*4 >= 24 → batch path). With LIMIT 2 only 2 keys survive
    // (2*4 < 24 → keyed path). One table exercises both.
    e.execute("CREATE TABLE msg (id INT, thread INT, sender TEXT, ts INT)")
        .unwrap();
    let mut vals = Vec::new();
    let mut id = 1;
    for thread in 1..=8 {
        for k in 0..3 {
            let ts = thread * 10 + k; // newest per thread is k=2
            vals.push(format!("({id}, {thread}, 's{thread}_{k}', {ts})"));
            id += 1;
        }
    }
    e.execute(&format!(
        "INSERT INTO msg (id, thread, sender, ts) VALUES {}",
        vals.join(",")
    ))
    .unwrap();
    e
}

fn rows_of(e: &mut Engine, sql: &str) -> Vec<spg_storage::Row<'static>> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows from {sql:?}, got {other:?}"),
    }
}

fn text<'a>(v: &'a Value<'_>) -> &'a str {
    match v {
        Value::Text(s) => s.as_ref(),
        other => panic!("expected text, got {other:?}"),
    }
}

#[test]
fn no_limit_batch_picks_correct_top1_per_group() {
    // The all-keys batch path (no outer LIMIT). The newest message per
    // thread is k=2 (ts = thread*10+2), sender 's{thread}_2'.
    let mut e = setup();
    let rows = rows_of(
        &mut e,
        "SELECT m.thread, (SELECT i.sender FROM msg i WHERE i.thread = m.thread \
             ORDER BY i.ts DESC LIMIT 1) \
         FROM msg m GROUP BY m.thread ORDER BY m.thread",
    );
    assert_eq!(rows.len(), 8);
    for (idx, r) in rows.iter().enumerate() {
        let thread = idx + 1;
        assert_eq!(
            text(&r.values[1]),
            format!("s{thread}_2"),
            "thread {thread} newest sender"
        );
    }
}

#[test]
fn keyed_limit_path_matches_batch_path() {
    // The keyed-seek path (tight LIMIT leaves few survivors) must return
    // the SAME per-group answers as the no-LIMIT batch, just fewer rows.
    let mut e = setup();
    // Order by the group's own newest ts so the surviving 2 groups are
    // deterministic (threads 8 and 7).
    let rows = rows_of(
        &mut e,
        "SELECT m.thread, (SELECT i.sender FROM msg i WHERE i.thread = m.thread \
             ORDER BY i.ts DESC LIMIT 1) \
         FROM msg m GROUP BY m.thread ORDER BY MAX(m.ts) DESC LIMIT 2",
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(text(&rows[0].values[1]), "s8_2");
    assert_eq!(text(&rows[1].values[1]), "s7_2");
}

#[test]
fn no_limit_batch_aggregate_wrapped_correlated_subquery() {
    // The aggregate-wrapped correlated subquery (R31 shape) also routes
    // through the batch (restrict = None) and must stay correct with no
    // outer LIMIT. MAX over a per-row correlated lookup.
    let mut e = setup();
    let rows = rows_of(
        &mut e,
        "SELECT m.thread, MAX((SELECT i.ts FROM msg i WHERE i.id = m.id)) \
         FROM msg m GROUP BY m.thread ORDER BY m.thread",
    );
    assert_eq!(rows.len(), 8);
    for (idx, r) in rows.iter().enumerate() {
        let thread = (idx + 1) as i32;
        // MAX of the per-row ts lookups for this thread = newest (k=2).
        assert!(
            matches!(&r.values[1], Value::Int(n) if *n == thread * 10 + 2),
            "thread {thread} max ts, got {:?}",
            r.values[1]
        );
    }
}
