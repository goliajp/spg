//! v7.33 (C1) — the never-die gate. The ceiling-first / never-die
//! axiom (design doc .claude/notes/v7.31-memory-design.md "验收形态"):
//! under a tiny per-query byte budget, a barrage of fat queries must
//!
//!   1. be rejected cleanly — every error is `QueryBytesExceeded`,
//!      never a panic / unbounded alloc / OOM,
//!   2. leave the engine answering — a small in-budget query still
//!      works after the barrage,
//!   3. keep peak allocation bounded by the budget, NOT balloon to the
//!      table size before the ceiling notices. (3) is the load-bearing
//!      one: a query that materialises the whole table *then* reports
//!      QueryBytesExceeded is not never-die — N of them concurrently
//!      still OOMs the host. The budget has to cap the peak.
//!
//! Peak is bracketed with the same LIVE/PEAK allocator shim round26_mem
//! uses. The alloc count is deterministic and machine-independent, so
//! this runs in the fast tier (and the CI perf_gate job).

use crate::{LIVE_BYTES, PEAK_BYTES, perf_lock};
use spg_engine::{Engine, EngineError, QueryResult};
use std::sync::atomic::Ordering;

const ROWS: usize = 400;
const BODY_BYTES: usize = 256 * 1024; // ≈100 MiB of fat bodies total
const BUDGET: usize = 4 * 1024 * 1024; // 4 MiB per-query ceiling
/// A rejected fat query must not first materialise the whole ~100 MiB
/// table. Generous headroom over the 4 MiB budget for transient per-row
/// work, but an order of magnitude below the table size.
const PEAK_BUDGET: usize = 32 * 1024 * 1024;

fn seed(eng: &mut Engine) {
    eng.execute("CREATE TABLE messages (id BIGINT, mailbox_id BIGINT, body TEXT)")
        .unwrap();
    eng.execute("CREATE TABLE mailboxes (id BIGINT, user_address TEXT)")
        .unwrap();
    eng.execute("INSERT INTO mailboxes VALUES (1,'a@x'),(2,'b@x'),(3,'c@x'),(4,'d@x')")
        .unwrap();
    let mut i = 0usize;
    while i < ROWS {
        let mut stmt = String::with_capacity(10 * BODY_BYTES + 1024);
        stmt.push_str("INSERT INTO messages VALUES ");
        for k in 0..10 {
            let id = i + k + 1;
            if k > 0 {
                stmt.push(',');
            }
            let body = format!("{id:08}{}", "x".repeat(BODY_BYTES - 8));
            stmt.push_str(&format!("({id},{},'{body}')", id % 4 + 1));
        }
        eng.execute(&stmt).unwrap();
        i += 10;
    }
}

#[test]
fn tiny_budget_rejects_fat_queries_without_dying() {
    let _g = perf_lock();
    let mut eng = Engine::new().with_max_query_bytes(BUDGET);
    seed(&mut eng);

    // Fat shapes, each of which would materialise far past the 4 MiB
    // budget if the ceiling didn't stop it.
    let fat = [
        "SELECT * FROM messages",
        "SELECT m.body FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id",
        "SELECT * FROM messages ORDER BY body",
    ];

    let floor = LIVE_BYTES.load(Ordering::Relaxed);
    PEAK_BYTES.store(floor, Ordering::Relaxed);

    // Barrage. Every iteration must reject cleanly; the engine must not
    // accumulate state or die across repetitions.
    for round in 0..15 {
        for q in &fat {
            match eng.execute_readonly(q) {
                Err(EngineError::QueryBytesExceeded(_)) => {} // the only acceptable outcome
                Ok(QueryResult::Rows { rows, .. }) => panic!(
                    "round {round}: fat query `{q}` returned {} rows under a {BUDGET}-byte budget \
                     — the ceiling didn't fire",
                    rows.len()
                ),
                Ok(other) => panic!("round {round}: `{q}` unexpected ok {other:?}"),
                Err(other) => panic!(
                    "round {round}: fat query `{q}` must reject with QueryBytesExceeded, got {other:?}"
                ),
            }
        }
    }
    let peak = PEAK_BYTES.load(Ordering::Relaxed);

    // Service continues: a small in-budget query still answers.
    match eng
        .execute_readonly("SELECT id FROM messages WHERE id = 1")
        .expect("engine must still answer a small query after the barrage")
    {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("expected Rows from the survivor query, got {other:?}"),
    }

    let delta = peak.saturating_sub(floor);
    assert!(
        delta < PEAK_BUDGET,
        "fat-query barrage peaked at {delta} bytes (budget {PEAK_BUDGET}) — a query \
         ballooned toward the ~100 MiB table size before the {BUDGET}-byte ceiling \
         rejected it; N concurrent copies would OOM the host (never-die violated)"
    );
}
