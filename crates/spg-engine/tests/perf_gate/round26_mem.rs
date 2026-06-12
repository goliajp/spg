//! v7.30.3 (mailrs round-26) — the memory gate that would have
//! caught the prod incident. The meili backfill shape (JOIN + ORDER
//! BY id + LIMIT) over fat text bodies must peak at O(answer), not
//! O(table): pre-fix, the general join path materialised ≈2× every
//! body before LIMIT truncated — 100 MiB of seeded bodies peaked
//! well past 200 MiB, and on prod (GBs of mail) walked a 15 GiB
//! no-swap host into reclaim livelock.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;
use std::sync::atomic::Ordering;

use crate::{LIVE_BYTES, PEAK_BYTES, perf_lock};

const ROWS: usize = 400;
const BODY_BYTES: usize = 256 * 1024; // ≈100 MiB of bodies total
const LIMIT: usize = 20;
/// Pre-7.30.3 this query peaked at >200 MiB (2× the table). The
/// bounded path holds LIMIT rows plus the mailboxes hash — single-
/// digit MiB. 64 MiB keeps ~10× headroom without letting an
/// O(table) regression squeak back through.
const PEAK_BUDGET: usize = 64 * 1024 * 1024;

#[test]
fn backfill_join_peak_is_bounded_by_limit_not_table() {
    let _g = perf_lock();
    let mut eng = Engine::new();
    eng.execute(
        "CREATE TABLE messages (id BIGINT, mailbox_id BIGINT, subject TEXT, text_body TEXT)",
    )
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
            stmt.push_str(&format!("({id},{},'s{id}','{body}')", id % 4 + 1));
        }
        eng.execute(&stmt).unwrap();
        i += 10;
    }

    // High-water bracket around the backfill SELECT only.
    let floor = LIVE_BYTES.load(Ordering::Relaxed);
    PEAK_BYTES.store(floor, Ordering::Relaxed);
    let res = eng
        .execute_readonly(&format!(
            "SELECT m.id, m.text_body, mb.user_address FROM messages m \
             JOIN mailboxes mb ON m.mailbox_id = mb.id \
             WHERE m.id > 0 ORDER BY m.id ASC LIMIT {LIMIT}"
        ))
        .unwrap();
    let peak = PEAK_BYTES.load(Ordering::Relaxed);

    match res {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), LIMIT);
            assert!(matches!(
                rows[0].values[0],
                Value::BigInt(1) | Value::Int(1)
            ));
            match &rows[0].values[1] {
                Value::Text(b) => assert_eq!(b.len(), BODY_BYTES),
                other => panic!("expected body text, got {other:?}"),
            }
        }
        other => panic!("expected Rows, got {other:?}"),
    }

    let delta = peak.saturating_sub(floor);
    assert!(
        delta < PEAK_BUDGET,
        "backfill join peaked at {delta} bytes (budget {PEAK_BUDGET}) — \
         bounded execution regressed to O(table) materialisation"
    );
}
