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
    build_inner(true)
}

/// `build_no_analyze` reproduces the `Database::open_path` + fresh-
/// import scenario: catalog is populated by INSERTs but ANALYZE has
/// never been called. This is the mailrs-prod boot shape (no
/// background auto-analyze worker wired in the embed host), and is
/// where the planner falls back to source-order JOIN because the
/// `reorder_joins` pass bails on `stats.is_empty()`. The general fix
/// must work for both `build()` (post-ANALYZE) and `build_no_analyze()`
/// — same query plan latency floor regardless of stats freshness.
fn build_no_analyze() -> Engine {
    build_inner(false)
}

fn build_inner(run_analyze: bool) -> Engine {
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
    // email_analysis — 1:1 with messages, for the conversations
    // correlated subquery shape (`SELECT ea.category FROM
    // email_analysis ea JOIN messages m2 ... WHERE m2.thread_id =
    // m.thread_id ORDER BY m2.internal_date DESC LIMIT 1`).
    eng.execute(
        "CREATE TABLE email_analysis (message_id BIGINT PRIMARY KEY, \
         category TEXT, summary TEXT, requires_action BOOLEAN)",
    )
    .unwrap();
    for b in 0..(N / 500) {
        let mut rows = Vec::with_capacity(500);
        for j in 0..500 {
            let i = b * 500 + j;
            rows.push(format!(
                "({}, 'cat{}', 'summary text {i}', {})",
                i + 1,
                i % 7,
                if i % 11 == 0 { "true" } else { "false" }
            ));
        }
        eng.execute(&format!(
            "INSERT INTO email_analysis (message_id, category, summary, requires_action) VALUES {}",
            rows.join(",")
        ))
        .unwrap();
    }
    // snoozed_conversations — small (one row per ~1000 messages).
    eng.execute(
        "CREATE TABLE snoozed_conversations (thread_id TEXT NOT NULL, \
         account_address TEXT NOT NULL, snoozed_until BIGINT NOT NULL)",
    )
    .unwrap();
    eng.execute("CREATE INDEX idx_snz_thread ON snoozed_conversations(thread_id)")
        .unwrap();
    for i in 0..(N / 1000) {
        let user = i % USERS;
        eng.execute(&format!(
            "INSERT INTO snoozed_conversations (thread_id, account_address, snoozed_until) \
             VALUES ('th-{}', 'u{user}@x', 1)",
            i * 7
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
    if run_analyze {
        eng.execute("ANALYZE").unwrap();
    }
    eng
}

/// Shape (A) — `content_worker.rs:43`. JOIN + NOT EXISTS + ORDER BY
/// pk DESC LIMIT. With LIMIT 64 + indexed `attachment_content
/// (message_id)` this should be O(LIMIT) — the walker fast path
/// (`try_streamed_inner_join_walk_topn` for the joined shape, or
/// `try_pk_walk_top_n` after JOIN reduction) walks `messages` DESC,
/// probes the NOT EXISTS via the index, and stops at 64 survivors.
const SQL_CONTENT_WORKER: &str = "SELECT m.id, m.sender, m.maildir_id, mb.user_address \
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

/// Shape (C) — full mailrs prod conversations SQL from
/// `spg-7.37.3-prod-conversations-real-sql-2026-06-18.md`. 14
/// aggregates per group + 3 correlated `LIMIT 1 ORDER BY internal_date
/// DESC` subqueries on email_analysis + LEFT JOIN email_analysis +
/// NOT EXISTS (snoozed). This is the heavy aggregating shape that
/// goes 2.5s in mailrs prod. Stripped of `string_agg`, `BOOL_OR`,
/// `(array_agg ORDER BY)[1]`, `LEFT(...)`, `COUNT(DISTINCT CASE)` —
/// only the parts SPG can compile at the worktree head (some PG
/// dialect ops aren't all wired here yet); the dominant cost path
/// (small-driver, 3-subquery, GROUP BY + ORDER BY) is preserved.
const SQL_CONVERSATIONS_REAL_SHAPE: &str = "SELECT m.thread_id, MAX(m.subject), \
       MAX(m.internal_date), \
       COALESCE((SELECT ea.category FROM email_analysis ea \
                   JOIN messages m2 ON ea.message_id = m2.id \
                  WHERE m2.thread_id = m.thread_id \
                  ORDER BY m2.internal_date DESC LIMIT 1), \
                'general'), \
       COALESCE((SELECT ea_s.summary FROM email_analysis ea_s \
                   JOIN messages m_s ON ea_s.message_id = m_s.id \
                  WHERE m_s.thread_id = m.thread_id \
                    AND ea_s.summary IS NOT NULL \
                    AND ea_s.summary != '' \
                  ORDER BY m_s.internal_date DESC LIMIT 1), \
                ''), \
       MAX(m.subject) \
  FROM messages m \
       JOIN mailboxes mb ON m.mailbox_id = mb.id \
       LEFT JOIN email_analysis ea ON ea.message_id = m.id \
 WHERE mb.user_address = 'u0@x' \
   AND m.thread_id != '' \
   AND NOT EXISTS (SELECT 1 FROM snoozed_conversations sc \
                    WHERE sc.thread_id = m.thread_id \
                      AND sc.account_address = mb.user_address \
                      AND sc.snoozed_until > 9999999999) \
 GROUP BY m.thread_id \
 ORDER BY MAX(m.internal_date) DESC \
 LIMIT 50";

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
    // at 100k, and the regression it exists to catch is the ~3-5 s
    // prod shape — a hundredfold, not a percentage.
    //
    // v7.38.2 — 100 ms was calibrated on a developer box and assumed
    // ~2x of headroom. The GitHub runner is 3.3x slower than that box
    // (measured: 32 ms here, 105 ms there, on the SAME commit), so the
    // budget was really gating the runner's speed and went red on
    // v7.38.2's release commit. An interleaved A/B against v7.38.1 in
    // its own worktree put both versions at ~32 ms with fully
    // overlapping spread — no regression, just a slower machine. At
    // 500 ms this still catches the 3-5 s shape with 6-10x of margin
    // and sits 15x above the measured value — which is the ~10x this
    // crate's BUDGETS.md asks of every gate, and which 100 ms (3x) had
    // never actually met.
    //
    // The instrument this really wants is a ratio against a reference
    // query timed on the same box, which no amount of hardware drift
    // can move. That needs its own cross-machine calibration before it
    // can gate anything, so it is not smuggled in here.
    let budget = 0.500;
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

/// Real prod conversations shape (Shape C) — LEFT JOIN +
/// NOT EXISTS + 2 correlated `LIMIT 1 ORDER BY DESC` subqueries +
/// GROUP BY + ORDER BY MAX(date) DESC LIMIT. This is the actual
/// shape that hits 2.5s in mailrs prod; the reduced "lite" form
/// above misses the correlated-subquery × per-group cost (the
/// dominant 50× regression vector). Budget 500 ms — the real prod
/// shape has 3 correlated subqueries instead of 2 in this gate
/// (the third uses a `LEFT(m3.text_body, 80)` form not yet wired
/// here), and the gate must not regress past mailrs's "annoying"
/// UX line.
#[test]
fn mailrs_conversations_real_shape_100k_under_budget() {
    let _lock = crate::perf_lock();
    let mut eng = build();
    let mean = run_query(&mut eng, SQL_CONVERSATIONS_REAL_SHAPE, 2);
    let budget = 0.500;
    assert!(
        mean < budget,
        "mailrs conversations real-shape 100k mean {mean:.4}s exceeds budget \
         {budget:.4}s — correlated subqueries on GROUP BY thread_id should batch- \
         evaluate via the per-query memo, not per-group re-execute"
    );
}

#[test]
fn mailrs_conversations_real_shape_100k_no_analyze_under_budget() {
    let _lock = crate::perf_lock();
    let mut eng = build_no_analyze();
    let mean = run_query(&mut eng, SQL_CONVERSATIONS_REAL_SHAPE, 2);
    let budget = 0.500;
    assert!(
        mean < budget,
        "mailrs conversations real-shape 100k no-ANALYZE mean {mean:.4}s exceeds \
         budget {budget:.4}s — the fix must work without ANALYZE (mailrs prod's \
         shape)"
    );
}

/// mailrs prod doesn't wire a background auto-ANALYZE worker into
/// the embed host today — fresh `Database::open_path` + INSERT
/// catalog runs with empty `Statistics`, so `reorder_joins` bails
/// (it gates on `stats.is_empty()`). Source-order JOIN under that
/// shape: `messages` (100k) drives, `mailboxes` (90) joins second.
/// Without WHERE-driven row reduction the running set stays at
/// 100k through the join. The fix must keep this case fast too —
/// the planner needs a stats-free fallback that respects table
/// sizes (mailrs prod's actual entry shape).
#[test]
fn mailrs_content_worker_100k_no_analyze_under_budget() {
    let _lock = crate::perf_lock();
    let mut eng = build_no_analyze();
    let mean = run_query(&mut eng, SQL_CONTENT_WORKER, 3);
    // Same calibration story as the JOIN gate above (v7.38.2): 32 ms
    // measured here, 105 ms on the CI runner, ~3-5 s is the shape being
    // caught.
    let budget = 0.500;
    assert!(
        mean < budget,
        "mailrs content_worker 100k no-ANALYZE mean {mean:.4}s exceeds budget \
         {budget:.4}s — reorder_joins likely bailed on empty stats and source-order \
         JOIN forced a 100k-row materialise instead of the walker fast path"
    );
}

#[test]
fn mailrs_conversations_100k_no_analyze_under_budget() {
    let _lock = crate::perf_lock();
    let mut eng = build_no_analyze();
    let mean = run_query(&mut eng, SQL_CONVERSATIONS_LITE, 3);
    let budget = 0.200;
    assert!(
        mean < budget,
        "mailrs conversations 100k no-ANALYZE mean {mean:.4}s exceeds budget \
         {budget:.4}s — planner needs a stats-free size-aware fallback (mailrs prod \
         runs without ANALYZE; the cost-based reorder bails)"
    );
}

/// Full-tier (ignored) — extended sweep for visibility into the
/// per-shape cost at the 100k seed. Documents headroom against the
/// fast-tier budgets above.
#[test]
#[ignore]
fn mailrs_100k_cascade_p50_p95_p99_sweep() {
    let _lock = crate::perf_lock();
    for (label, mut eng) in [
        ("with-analyze", build()),
        ("no-analyze", build_no_analyze()),
    ] {
        eprintln!("== {label} ==");
        for (name, sql, budget_ms) in [
            ("content_worker", SQL_CONTENT_WORKER, 100.0_f64),
            ("conversations_lite", SQL_CONVERSATIONS_LITE, 200.0),
            ("conversations_real", SQL_CONVERSATIONS_REAL_SHAPE, 500.0),
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
                "  {name}: p50={p50:.2}ms p95={p95:.2}ms p99={p99:.2}ms (budget {budget_ms:.0}ms)"
            );
        }
    }
}
