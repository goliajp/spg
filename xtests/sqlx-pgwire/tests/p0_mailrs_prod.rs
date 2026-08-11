//! v7.38 P0 mailrs prod reproducer.
//!
//! Goal: run the exact /api/conversations?limit=50 SQL from
//! `.../mailrs/.claude/notes/spg-7.37.3-prod-conversations-still-2.5s
//!  -user-visible-2026-06-18.md` via the sqlx → spg-server extended
//! protocol path (the path mailrs actually uses in prod), seed N
//! messages, time it on PG18 and SPG side-by-side.
//!
//! The SPG simple-query path rejects this SQL with
//! "unknown table qualifier: m" (we already verified via psql).
//! mailrs prod runs through sqlx extended protocol and the query
//! parses + executes — but on prod's 100 k catalog it takes 1.6 s
//! (50× the staging 30 k baseline). This test is the missing
//! reproducer.
//!
//! Both DSNs come from env so we point at the SPG and PG containers
//! running on the mini testbed:
//!
//!   export SPG_PG_URL='postgres://spg:@127.0.0.1:25490/spg'
//!   export PG_URL='postgres://bench:bench@127.0.0.1:25432/bench'
//!   cargo test -p spg-sqlx-pgwire --test p0_mailrs_prod -- --nocapture --include-ignored
//!
//! Seed size via SPG_P0_SEED_N (default 30000).

use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};
use std::time::{Duration, Instant};

const PROD_SQL: &str = "\
SELECT m.thread_id, \
       MAX(m.subject), \
       string_agg(DISTINCT m.sender, ','), \
       COUNT(DISTINCT CASE WHEN m.message_id != '' \
                           THEN m.message_id \
                           ELSE CAST(m.id AS TEXT) END), \
       COUNT(DISTINCT CASE WHEN (m.flags & 1) = 0 \
                           THEN CASE WHEN m.message_id != '' \
                                     THEN m.message_id \
                                     ELSE CAST(m.id AS TEXT) END \
                           END), \
       MAX(m.internal_date), \
       COALESCE((SELECT ea2.category \
                   FROM email_analysis ea2 \
                  WHERE ea2.message_id = MAX(m.id)), \
                'general') \
  FROM messages m \
  JOIN mailboxes mb ON m.mailbox_id = mb.id \
 WHERE mb.user_address = 'lihao@golia.jp' \
 GROUP BY m.thread_id \
 ORDER BY MAX(m.internal_date) DESC \
 LIMIT 50";

const NO_SUBQ_SQL: &str = "\
SELECT m.thread_id, \
       MAX(m.subject), \
       string_agg(DISTINCT m.sender, ','), \
       MAX(m.internal_date) \
  FROM messages m \
  JOIN mailboxes mb ON m.mailbox_id = mb.id \
 WHERE mb.user_address = 'lihao@golia.jp' \
 GROUP BY m.thread_id \
 ORDER BY MAX(m.internal_date) DESC \
 LIMIT 50";

const MINIMAL_SQL: &str = "\
SELECT m.thread_id, MAX(m.internal_date) \
  FROM messages m \
  JOIN mailboxes mb ON m.mailbox_id = mb.id \
 WHERE mb.user_address = 'lihao@golia.jp' \
 GROUP BY m.thread_id \
 ORDER BY MAX(m.internal_date) DESC \
 LIMIT 50";

async fn connect(env_var: &str) -> PgPool {
    let url = std::env::var(env_var)
        .unwrap_or_else(|_| panic!("{env_var} not set; see crate-level docs"));
    PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(15))
        .connect(&url)
        .await
        .unwrap_or_else(|e| panic!("connect to {env_var}: {e}"))
}

async fn seed(pool: &PgPool, n_messages: usize, label: &str) {
    eprintln!("[{label}] seeding {n_messages} messages…");
    let t = Instant::now();
    for ddl in [
        "DROP TABLE IF EXISTS email_analysis",
        "DROP TABLE IF EXISTS messages",
        "DROP TABLE IF EXISTS mailboxes",
        "CREATE TABLE mailboxes (id BIGSERIAL PRIMARY KEY, name TEXT, user_address TEXT)",
        "CREATE TABLE messages (id BIGSERIAL PRIMARY KEY, mailbox_id BIGINT, thread_id TEXT, \
            subject TEXT, sender TEXT, internal_date BIGINT, flags BIGINT, message_id TEXT)",
        "CREATE TABLE email_analysis (message_id BIGINT PRIMARY KEY, category TEXT)",
        "CREATE INDEX idx_messages_thread ON messages(thread_id)",
        "CREATE INDEX idx_messages_thread_date ON messages(thread_id, internal_date DESC)",
        "CREATE INDEX idx_messages_mailbox ON messages(mailbox_id)",
        "CREATE INDEX idx_mailboxes_user ON mailboxes(user_address, name)",
    ] {
        pool.execute(ddl).await.unwrap();
    }
    for i in 0..10 {
        pool.execute(
            sqlx::query("INSERT INTO mailboxes (name, user_address) VALUES ($1, $2)")
                .bind(format!("mb{i}"))
                .bind("lihao@golia.jp"),
        )
        .await
        .unwrap();
    }

    let msgs_per_thread = 5usize;
    let n_senders = 600usize;
    let body = "lorem ipsum dolor sit amet ".repeat(20);
    // SPG-server stack-overflows past ~200 tuples per INSERT VALUES on
    // some path (seen 2026-06-18 during P0 reproducer setup). Use 100
    // here to stay safe across backends. PG18 handles 500 fine.
    let batch_size = 100usize;
    let mut vals = String::new();
    let mut count = 0usize;
    for i in 0..n_messages {
        if !vals.is_empty() {
            vals.push(',');
        }
        use std::fmt::Write;
        let mailbox_id = (i % 10) + 1;
        let thread_idx = i / msgs_per_thread;
        let sender_idx = i % n_senders;
        let mid = if i % 5 == 0 {
            String::new()
        } else {
            format!("mid-{i}")
        };
        let flags = i32::from(i % 10 >= 3);
        let _ = write!(
            vals,
            "({}, 'th-{}', 'subj{i}', 'sender{}@example.com', {}, {}, '{}')",
            mailbox_id,
            thread_idx,
            sender_idx,
            1_700_000_000_i64 + i as i64,
            flags,
            mid
        );
        // Embed body inline? No — INSERT VALUES syntax is the bottleneck;
        // skip subject body to keep the seed time bounded, matches the
        // SQL columns we GROUP/aggregate over.
        // (Already small: subject, sender, mid, flags, internal_date.)
        let _ = &body; // silence unused
        count += 1;
        if count == batch_size {
            let sql = format!(
                "INSERT INTO messages (mailbox_id, thread_id, subject, sender, internal_date, flags, message_id) VALUES {vals}"
            );
            pool.execute(sql.as_str()).await.unwrap();
            vals.clear();
            count = 0;
        }
    }
    if !vals.is_empty() {
        let sql = format!(
            "INSERT INTO messages (mailbox_id, thread_id, subject, sender, internal_date, flags, message_id) VALUES {vals}"
        );
        pool.execute(sql.as_str()).await.unwrap();
    }

    let ea_rows = n_messages / 4;
    let mut vals = String::new();
    let mut count = 0usize;
    for k in 0..ea_rows {
        let mid = (k * 4 + 1) as i64;
        if !vals.is_empty() {
            vals.push(',');
        }
        use std::fmt::Write;
        let _ = write!(&mut vals, "({mid}, 'cat{}')", k % 5);
        count += 1;
        if count == 500 {
            let sql = format!("INSERT INTO email_analysis (message_id, category) VALUES {vals}");
            pool.execute(sql.as_str()).await.unwrap();
            vals.clear();
            count = 0;
        }
    }
    if !vals.is_empty() {
        let sql = format!("INSERT INTO email_analysis (message_id, category) VALUES {vals}");
        pool.execute(sql.as_str()).await.unwrap();
    }
    eprintln!("[{label}] seed done in {:.2}s", t.elapsed().as_secs_f64());
}

async fn measure(pool: &PgPool, sql: &str, label: &str, iters: usize) {
    // Warm-up.
    match sqlx::query(sql).fetch_all(pool).await {
        Ok(rows) => eprintln!("[{label}] warm-up ok ({} rows)", rows.len()),
        Err(e) => {
            eprintln!("[{label}] warm-up FAILED: {e}");
            return;
        }
    }
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let r = sqlx::query(sql).fetch_all(pool).await;
        let elapsed_ms = t.elapsed().as_secs_f64() * 1000.0;
        if r.is_err() {
            eprintln!("[{label}] iter FAILED: {r:?}");
            return;
        }
        samples.push(elapsed_ms);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = samples[samples.len() / 2];
    let p95 = samples[(samples.len() * 95) / 100];
    let min = samples[0];
    let max = *samples.last().unwrap();
    eprintln!(
        "[{label}] p50={p50:.2}ms  p95={p95:.2}ms  min={min:.2}ms  max={max:.2}ms  (n={})",
        samples.len()
    );
}

fn seed_n() -> usize {
    std::env::var("SPG_P0_SEED_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30_000)
}

// Seed-only tests retained for the smallest-N path (used in CI). For
// the 30 k / 100 k tiers, prefer to seed via psql (simple-query) which
// SPG handles fine — the sqlx INSERT VALUES path stack-overflows the
// CLIENT (sqlx parser) past ~50 tuples per statement, a separate
// finding logged as part of this P0 sweep.
//
// Run the seed-via-psql + measure-via-sqlx path with:
//   SPG_P0_SKIP_SEED=1 cargo test ... p0_mailrs_prod_via_spgs_measure
// (the seeding is done by scripts/p0-mailrs-prod-probe.sh before this
// test fires, so all three tables already exist with N rows.)

async fn measure_or_skip(pool: &PgPool, sql: &str, label: &str, iters: usize) {
    measure(pool, sql, label, iters).await;
}

/// v7.37 (round 1004) — a reproducer that needs live servers SKIPS when it
/// has none, rather than failing.
///
/// Both measurements take their DSN from the environment, and `connect`
/// panics when it is unset. `#[ignore]` used to be enough to keep that off
/// an ordinary run, but `gate.sh --full` passes `--include-ignored`, so the
/// first full-tier run on this branch ended with two panics reading
/// `SPG_PG_URL not set` — a configuration statement, dressed as a failure.
///
/// The perf gate already had the answer for this: it says what is missing,
/// skips, and only fails when the release run demands the comparison
/// (`PERF_REQUIRED=1`). These follow it.
fn dsn_or_skip(env_var: &str) -> Option<String> {
    match std::env::var(env_var) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => {
            eprintln!(
                "skipping: {env_var} is unset, so there is nothing to measure against. \
                 See this file's docs for the two DSNs."
            );
            None
        }
    }
}

#[tokio::test]
#[ignore]
async fn p0_mailrs_prod_via_spgs_measure() {
    if dsn_or_skip("SPG_PG_URL").is_none() {
        return;
    }
    let n = seed_n();
    let spg = connect("SPG_PG_URL").await;
    if std::env::var("SPG_P0_SKIP_SEED").is_err() {
        seed(&spg, n, "SPG").await;
    }
    eprintln!("--- SPGS measurements (N={n}) ---");
    measure_or_skip(&spg, MINIMAL_SQL, "SPG_MINIMAL", 10).await;
    measure_or_skip(&spg, NO_SUBQ_SQL, "SPG_NO_SUBQ", 10).await;
    measure_or_skip(&spg, PROD_SQL, "SPG_PROD", 10).await;
}

#[tokio::test]
#[ignore]
async fn p0_mailrs_prod_via_pg18_measure() {
    if dsn_or_skip("PG_URL").is_none() {
        return;
    }
    let n = seed_n();
    let pg = connect("PG_URL").await;
    if std::env::var("SPG_P0_SKIP_SEED").is_err() {
        seed(&pg, n, "PG").await;
    }
    eprintln!("--- PG18 measurements (N={n}) ---");
    measure_or_skip(&pg, MINIMAL_SQL, "PG_MINIMAL", 10).await;
    measure_or_skip(&pg, NO_SUBQ_SQL, "PG_NO_SUBQ", 10).await;
    measure_or_skip(&pg, PROD_SQL, "PG_PROD", 10).await;
}
