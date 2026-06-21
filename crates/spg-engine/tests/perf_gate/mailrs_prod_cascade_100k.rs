//! v7.37.x — T3 mailrs prod 100k cascade general-shape perf gate.
//!
//! Reproduces the two prod query shapes from
//! `stables/mailrs/.claude/notes/spg-7.37.3-prod-pool-cascade-restarted-2026-06-19.md`
//! and `spg-7.37.3-prod-slow-queries-worsened-2026-06-20.md` at a
//! synthetic 100k seed, on a SPG-generic two-table schema with
//! the same JOIN + NOT EXISTS + ORDER BY pk DESC LIMIT N shape.
//!
//! Two shapes, both must come in well under 200 ms on the fixture:
//!
//!   (A) content_worker shape (`crates/server/src/content_worker.rs:43`):
//!     ```
//!     SELECT m.id, m.sender, m.maildir_id, mb.user_address
//!       FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id
//!      WHERE m.size > 0
//!        AND NOT EXISTS (SELECT 1 FROM attachment_content ac
//!                         WHERE ac.message_id = m.id)
//!      ORDER BY m.id DESC LIMIT 64
//!     ```
//!     prod (100k catalog, every msg has attachment) = 3-5 s steady,
//!     19 s tail. With LIMIT 64 + ORDER BY pk DESC + indexed NOT EXISTS
//!     this should be O(LIMIT) — gate budgets 50 ms.
//!
//!   (B) conversations shape ([the GROUP BY + correlated subq one]):
//!     reduced here to the minimal "small driving table forces tiny
//!     intermediate" pattern — `mailboxes` filtered to one user reduces
//!     all downstream cardinality. The full prod shape has 14
//!     aggregates per group + 3 correlated `LIMIT 1 ORDER BY DESC`
//!     subqueries on indexed columns; the planner must pick `mailboxes`
//!     as driver. The minimal gate is `JOIN + WHERE small-table-filter
//!     + GROUP BY thread_id + ORDER BY MAX(internal_date) DESC LIMIT 50`
//!     on the same seed — budgets 200 ms.
//!
//! Both shapes are common across PG-shaped apps; the fix must live in
//! SPG's planner / executor, not in a per-table guard. See plan §T3.

use std::time::Instant;

use spg_engine::Engine;

const N: usize = 100_000;
/// PER_THREAD: messages per `thread_id`. Drives the conversations
/// shape's GROUP-BY cardinality (100k / 5 = 20k threads).
const PER_THREAD: usize = 5;
/// USERS: distinct mailbox owners. Selecting one user (≈3 mailboxes /
/// ≈3.3k messages of the 100k seed) lets the planner exploit the
/// small-driver fast path; reproduces the prod-shape skew.
const USERS: usize = 30;

fn build() -> Engine {
    let mut eng = Engine::new();
    // Mailboxes — small (USERS × ~3 = 90 rows). Same shape as the
    // prod schema (`crates/server/src/pg.rs` + `crates/mailbox`).
    eng.execute(
        "CREATE TABLE mailboxes (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL, \
         user_address TEXT NOT NULL)",
    )
    .unwrap();
    eng.execute("CREATE INDEX idx_mb_user ON mailboxes(user_address)")
        .unwrap();
    for u in 0..USERS {
        let user = format!("u{u}@x");
        for mb_name in ["Inbox", "Sent", "Archive"] {
            eng.execute(&format!(
                "INSERT INTO mailboxes (name, user_address) VALUES ('{mb_name}', '{user}')"
            ))
            .unwrap();
        }
    }
    // Messages — 100k rows. mailbox_id maps round-robin across the
    // ~90 mailboxes; `internal_date` increases monotonically with
    // `id` so ORDER BY id DESC ≈ ORDER BY internal_date DESC.
    eng.execute(
        "CREATE TABLE messages (id BIGSERIAL PRIMARY KEY, mailbox_id BIGINT NOT NULL, \
         thread_id TEXT NOT NULL, subject TEXT, sender TEXT, maildir_id TEXT, \
         internal_date BIGINT, flags BIGINT NOT NULL DEFAULT 0, size BIGINT NOT NULL DEFAULT 0, \
         archived BOOLEAN NOT NULL DEFAULT false, pinned BOOLEAN NOT NULL DEFAULT false)",
    )
    .unwrap();
    eng.execute("CREATE INDEX idx_msg_mailbox ON messages(mailbox_id)")
        .unwrap();
    eng.execute("CREATE INDEX idx_msg_thread ON messages(thread_id)")
        .unwrap();
    let total_mb = USERS * 3;
    for b in 0..(N / 500) {
        let mut rows = Vec::with_capacity(500);
        for j in 0..500 {
            let i = b * 500 + j;
            let mb_id = (i % total_mb) + 1;
            let thread = i / PER_THREAD;
            let archived = if i % 200 == 0 { "true" } else { "false" };
            rows.push(format!(
                "({mb_id}, 'th-{thread}', 'subj {i}', 's{}@x', 'md-{i}', {}, 0, {}, {archived}, false)",
                i % 100,
                1_700_000_000_i64 + i as i64,
                100 + (i % 4096),
            ));
        }
        eng.execute(&format!(
            "INSERT INTO messages (mailbox_id, thread_id, subject, sender, maildir_id, \
             internal_date, flags, size, archived, pinned) VALUES {}",
            rows.join(",")
        ))
        .unwrap();
    }
    // attachment_content — every message gets one. Worst case for
    // the content_worker NOT EXISTS shape: LIMIT 64 never short-
    // circuits, so the planner must rely on the indexed NOT EXISTS
    // probe instead of full scan + sort.
    eng.execute(
        "CREATE TABLE attachment_content (id BIGSERIAL PRIMARY KEY, \
         message_id BIGINT NOT NULL)",
    )
    .unwrap();
    eng.execute("CREATE INDEX idx_ac_msg ON attachment_content(message_id)")
        .unwrap();
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
    // ANALYZE — let the planner read per-column stats. PG's policy:
    // ANALYZE drives the optimiser. SPG's reorder pass also gates on
    // it; without stats the join-order pass is a no-op (the bug the
    // 100k cascade exercise hits in mailrs prod). Background auto-
    // analyze (`tables_needing_analyze`) covers this when the embed
    // host wires it, but the perf gate runs ANALYZE explicitly so
    // the timed shape is the planner's best plan, not its fallback.
    eng.execute("ANALYZE").unwrap();
    eng
}

/// Shape (A) — `content_worker.rs:43`. JOIN + NOT EXISTS + ORDER BY
/// pk DESC LIMIT. With LIMIT 64 + indexed `attachment_content
/// (message_id)` this should be O(LIMIT) — the walker fast path
/// (`try_streamed_inner_join_walk_topn` for the joined shape, or
/// `try_pk_walk_top_n` after JOIN reduction) walks `messages` DESC,
/// probes the NOT EXISTS via the index, and stops at 64 survivors.
const SQL_CONTENT_WORKER: &str =
    "SELECT m.id, m.sender, m.maildir_id, mb.user_address \
       FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
      WHERE m.size > 0 \
        AND NOT EXISTS (SELECT 1 FROM attachment_content ac \
                         WHERE ac.message_id = m.id) \
      ORDER BY m.id DESC LIMIT 64";

/// Shape (B) — reduced conversations form: small driving filter on
/// `mailboxes.user_address` then GROUP BY + ORDER BY + LIMIT. The
/// planner must pick `mailboxes` as driver (USERS=30 → 3 rows match
/// the WHERE) so the join intermediate stays at ~3.3k rows instead
/// of scanning the full 100k. With the right plan + indexed
/// `(mailbox_id)` this is well under 200 ms.
const SQL_CONVERSATIONS_LITE: &str = "SELECT m.thread_id, MAX(m.internal_date), \
       MAX(m.subject), COUNT(*) \
       FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
      WHERE mb.user_address = 'u0@x' \
   GROUP BY m.thread_id \
   ORDER BY MAX(m.internal_date) DESC LIMIT 50";

fn run_query(eng: &mut Engine, sql: &str, iters: u32) -> f64 {
    // Warm once so plan rewrite / cache / cold-tier promotion are
    // outside the timed window.
    let _ = eng.execute(sql).expect("warm");
    let start = Instant::now();
    for _ in 0..iters {
        let r = eng.execute(std::hint::black_box(sql)).expect("ok");
        std::hint::black_box(r);
    }
    start.elapsed().as_secs_f64() / f64::from(iters)
}

#[test]
fn mailrs_content_worker_100k_join_under_budget() {
    let _lock = crate::perf_lock();
    let mut eng = build();
    let mean = run_query(&mut eng, SQL_CONTENT_WORKER, 3);
    // mailrs prod budget: 200 ms (the user-visible UX line). With the
    // walker fast path + indexed NOT EXISTS this should sit at ≤ 50 ms
    // at 100k. Budget 100 ms gives CI noise headroom while still
    // catching the catastrophic ~3-5 s prod-shape regression.
    let budget = 0.100;
    assert!(
        mean < budget,
        "mailrs content_worker 100k JOIN+NOT EXISTS+ORDER BY pk DESC LIMIT \
         mean {mean:.4}s exceeds budget {budget:.4}s — likely the walker fast \
         path bailed (general join executor materialised the full set then sorted)"
    );
}

#[test]
fn mailrs_conversations_100k_small_driver_under_budget() {
    let _lock = crate::perf_lock();
    let mut eng = build();
    let mean = run_query(&mut eng, SQL_CONVERSATIONS_LITE, 3);
    // The right plan picks `mailboxes` as driver (3 rows after the
    // user filter) and the intermediate stays at ~3.3k join rows.
    // Wrong plan (messages first) materialises 100k rows pre-filter
    // → seconds. Budget 200 ms reflects mailrs's UX expectation.
    let budget = 0.200;
    assert!(
        mean < budget,
        "mailrs conversations 100k small-driver-filter GROUP BY mean {mean:.4}s \
         exceeds budget {budget:.4}s — planner likely picked messages-first instead \
         of mailboxes-first (WHERE-filter selectivity not weighed in cost)"
    );
}

/// Full-tier (ignored) — extended sweep for visibility into the
/// per-shape cost at the 100k seed. Documents headroom against the
/// fast-tier budgets above.
#[test]
#[ignore]
fn mailrs_100k_cascade_p50_p95_p99_sweep() {
    let _lock = crate::perf_lock();
    let mut eng = build();
    for (name, sql, budget_ms) in [
        ("content_worker", SQL_CONTENT_WORKER, 100.0_f64),
        ("conversations", SQL_CONVERSATIONS_LITE, 200.0),
    ] {
        let _ = eng.execute(sql).expect("warm");
        let iters: usize = 20;
        let mut samples: Vec<f64> = Vec::with_capacity(iters);
        for _ in 0..iters {
            let t0 = Instant::now();
            let r = eng.execute(std::hint::black_box(sql)).expect("ok");
            samples.push(t0.elapsed().as_secs_f64() * 1000.0);
            std::hint::black_box(r);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p50 = samples[iters / 2];
        let p95 = samples[(iters * 19) / 20];
        let p99 = samples[(iters * 99) / 100];
        eprintln!(
            "{name}: p50={p50:.2}ms p95={p95:.2}ms p99={p99:.2}ms (budget {budget_ms:.0}ms)"
        );
    }
}
