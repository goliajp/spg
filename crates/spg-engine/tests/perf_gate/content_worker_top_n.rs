//! v7.37.x — perf gate for the mailrs prod `content_worker.rs:43`
//! shape (`spg-7.37.3-prod-slow-queries-worsened-2026-06-20.md`).
//!
//! Bug: `try_pk_walk_top_n` (index_access.rs) drove `ORDER BY pk DESC
//! LIMIT N` directly off the BTree iterator BUT its per-row WHERE
//! eval ran the bare `eval::eval_expr` without a `MemoizeCache`, so
//! a materialised `Expr::InList` (from `subquery_replacement` of an
//! uncorrelated InSubquery — what `try_pull_up_exists_as_in` produces
//! for `NOT EXISTS (... WHERE ac.message_id = m.id)`) ran a linear
//! O(list) scan per outer row. On the 825 MB prod snapshot:
//!     SELECT m.id FROM messages m
//!      WHERE NOT EXISTS (SELECT 1 FROM attachment_content ac
//!                         WHERE ac.message_id = m.id)
//!      ORDER BY m.id DESC LIMIT 64
//! cost ~6.0 s (25 450 outer × 25 521 InList = 650 M apply_binary
//! calls). Fix: walker takes `(engine, cancel)`, calls
//! `engine.eval_expr_with_correlated(..., Some(&mut memo))` instead
//! of `eval::eval_expr`, sharing one cache across the walk so the
//! existing `MemoizeCache.in_sets` HashSet fast path fires.
//!
//! This gate reproduces the shape at a smaller scale (3 000 messages
//! × 3 000 attachments) and budgets the query at 100 ms — large
//! enough that release-build noise is irrelevant, tight enough that
//! a regression to the linear path (~50 ms at this scale, growing
//! quadratically with N) would still trip well before the budget.
//! Budgets in `BUDGETS.md`.

use std::time::Instant;

use spg_engine::Engine;

const N: usize = 3_000;

fn build() -> Engine {
    let mut eng = Engine::new();
    eng.execute(
        "CREATE TABLE messages (id BIGSERIAL PRIMARY KEY, mailbox_id BIGINT, sender TEXT, \
         maildir_id TEXT, size BIGINT)",
    )
    .unwrap();
    eng.execute(
        "CREATE TABLE attachment_content (id BIGSERIAL PRIMARY KEY, message_id BIGINT NOT NULL)",
    )
    .unwrap();
    eng.execute("CREATE INDEX idx_ac_message_id ON attachment_content(message_id)")
        .unwrap();

    for b in 0..(N / 500) {
        let mut rows = Vec::with_capacity(500);
        for j in 0..500 {
            let i = b * 500 + j;
            rows.push(format!(
                "(1, 's{}@x', 'md-{i}', {})",
                i % 100,
                100 + (i % 4096)
            ));
        }
        eng.execute(&format!(
            "INSERT INTO messages (mailbox_id, sender, maildir_id, size) VALUES {}",
            rows.join(",")
        ))
        .unwrap();
    }
    // Every message has an attachment row → NOT EXISTS returns
    // ZERO rows, forcing the walker to visit every outer row (the
    // worst case for the bug: `LIMIT 64` never short-circuits).
    for b in 0..(N / 500) {
        let mut rows = Vec::with_capacity(500);
        for j in 0..500 {
            let i = b * 500 + j;
            rows.push(format!("({})", i + 1));
        }
        eng.execute(&format!(
            "INSERT INTO attachment_content (message_id) VALUES {}",
            rows.join(",")
        ))
        .unwrap();
    }
    eng
}

const SQL: &str = "SELECT m.id FROM messages m \
                    WHERE NOT EXISTS (SELECT 1 FROM attachment_content ac \
                                       WHERE ac.message_id = m.id) \
                    ORDER BY m.id DESC LIMIT 64";

#[test]
fn content_worker_not_exists_top_n_under_budget() {
    let _lock = crate::perf_lock();
    let mut eng = build();
    // Warm once so plan-time `pull_up_exists_as_in` +
    // `subquery_replacement` happen outside the timed window.
    let _ = eng.execute(SQL).expect("warm");

    let iters: u32 = 5;
    let start = Instant::now();
    for _ in 0..iters {
        let r = eng.execute(std::hint::black_box(SQL)).expect("ok");
        std::hint::black_box(r);
    }
    let mean = start.elapsed().as_secs_f64() / f64::from(iters);
    // Pre-fix on this fixture: ~50 ms (linear) growing → seconds as
    // N grows. Post-fix: ~3 ms. 100 ms gives plenty of release-CI
    // headroom while still catching a regression by an order of
    // magnitude.
    let budget = 0.100;
    assert!(
        mean < budget,
        "content_worker NOT EXISTS top-N mean {mean:.4}s exceeds budget {budget:.4}s — \
         try_pk_walk_top_n likely lost MemoizeCache + InList set fast path"
    );
}
