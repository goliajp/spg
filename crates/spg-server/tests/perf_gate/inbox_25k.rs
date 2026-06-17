//! v7.28 (mailrs round-22) — the seeded 25k-row inbox panel. The
//! production inbox query (17 columns, 3 correlated scalar
//! subqueries, GROUP BY over ~24k threads) never returned on
//! 7.27; the join-executor work (hash equi-join, predicate
//! pushdown + index seek, table-order swap, index-nested-loop,
//! unreferenced-column elision) brought it to ~1 s on this
//! dataset. Budgets carry ~3× headroom over the measured M-series
//! numbers; absolute budgets only (ratio gates are full-tier
//! elsewhere).

use spg_engine::{Engine, QueryResult};
use std::time::Instant;

fn seed(db: &mut Engine) {
    db.execute("CREATE TABLE mailboxes (id BIGSERIAL PRIMARY KEY, name TEXT, user_address TEXT)")
        .unwrap();
    db.execute("CREATE TABLE messages (id BIGSERIAL PRIMARY KEY, mailbox_id BIGINT, thread_id TEXT, subject TEXT, sender TEXT, internal_date BIGINT, flags BIGINT, pinned BOOLEAN, archived BOOLEAN, importance_level TEXT, importance_score REAL, message_id TEXT, text_body TEXT)").unwrap();
    db.execute("CREATE TABLE email_analysis (message_id BIGINT PRIMARY KEY, category TEXT, summary TEXT, requires_action BOOLEAN)").unwrap();
    db.execute("CREATE INDEX idx_thread ON messages(thread_id)")
        .unwrap();
    for i in 0..30 {
        db.execute(&format!(
            "INSERT INTO mailboxes (name, user_address) VALUES ('mb{i}', 'u@x')"
        ))
        .unwrap();
    }
    let body = "lorem ipsum dolor sit amet ".repeat(40); // ~1 KB
    for batch in 0..50 {
        let mut vals = Vec::new();
        for j in 0..500 {
            let i = batch * 500 + j;
            vals.push(format!(
                "({}, 'th-{i}', 'subject {i}', 's{}@x', {}, {}, false, false, 'normal', 0.5, 'mid-{i}', '{body} {i}')",
                (i % 30) + 1,
                i % 100,
                1_700_000_000 + i,
                i % 8
            ));
        }
        db.execute(&format!(
            "INSERT INTO messages (mailbox_id, thread_id, subject, sender, internal_date, flags, pinned, archived, importance_level, importance_score, message_id, text_body) VALUES {}",
            vals.join(",")
        ))
        .unwrap();
    }
    let mut vals = Vec::new();
    for i in 0..6000 {
        vals.push(format!(
            "({}, 'cat{}', 'summary {i}', {})",
            i * 4 + 1,
            i % 5,
            i % 2 == 0
        ));
        if vals.len() == 500 {
            db.execute(&format!(
                "INSERT INTO email_analysis (message_id, category, summary, requires_action) VALUES {}",
                vals.join(",")
            ))
            .unwrap();
            vals.clear();
        }
    }
}

const INBOX: &str = "SELECT m.thread_id, MAX(m.subject), COUNT(DISTINCT m.id), MAX(m.internal_date), \
    COALESCE((SELECT e2.category FROM email_analysis e2 JOIN messages m2 ON e2.message_id = m2.id \
              WHERE m2.thread_id = m.thread_id ORDER BY m2.internal_date DESC LIMIT 1), 'general'), \
    COALESCE((SELECT e3.summary FROM email_analysis e3 JOIN messages m3 ON e3.message_id = m3.id \
              WHERE m3.thread_id = m.thread_id ORDER BY m3.internal_date DESC LIMIT 1), ''), \
    COALESCE((SELECT LEFT(m4.text_body, 120) FROM messages m4 WHERE m4.thread_id = m.thread_id \
              ORDER BY m4.internal_date DESC LIMIT 1), ''), \
    BOOL_OR(m.pinned), BOOL_OR(m.archived), \
    COALESCE((array_agg(m.importance_level ORDER BY m.importance_score DESC NULLS LAST))[1], 'normal'), \
    COALESCE(MAX(m.importance_score), 0.0), COALESCE(BOOL_OR(ea.requires_action), false) \
    FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
    LEFT JOIN email_analysis ea ON ea.message_id = m.id \
    WHERE mb.user_address = 'u@x' AND m.thread_id != '' \
    GROUP BY m.thread_id HAVING BOOL_OR(m.archived) = false \
    ORDER BY MAX(m.internal_date) DESC LIMIT 50";

#[test]
fn inbox_query_on_25k_rows_stays_under_budget() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed(&mut db);
    // Warm-up once (plan cache, allocator), then 10 measured iterations
    // so the eprintln below carries a stable median for the mailrs perf
    // ledger — single-shot timing pingponged ±20 % between runs.
    db.execute(INBOX).unwrap();
    let mut samples = Vec::with_capacity(10);
    for _ in 0..10 {
        let t = Instant::now();
        let r = db.execute(INBOX).unwrap();
        let elapsed = t.elapsed();
        match r {
            QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 50),
            other => panic!("{other:?}"),
        }
        samples.push(elapsed.as_secs_f64() * 1000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = samples[samples.len() / 2];
    let min = samples[0];
    let max = *samples.last().unwrap();
    eprintln!("inbox_25k embed: p50={p50:.1}ms min={min:.1}ms max={max:.1}ms (n=10)");
    // Measured ~38 ms on a cold M-series laptop (release) post-7.32
    // P4/argmax; 4 s budget gives shared-runner headroom while still
    // failing the pre-round-22 executor (which never returned at all).
    assert!(p50 < 4000.0, "inbox query p50={p50}ms (budget 4 s)");
}

#[test]
fn join_limit_on_25k_rows_stays_under_budget() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed(&mut db);
    let t = Instant::now();
    let r = db
        .execute("SELECT m.id FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id LIMIT 5")
        .unwrap();
    let elapsed = t.elapsed();
    match r {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 5),
        other => panic!("{other:?}"),
    }
    // Measured ~75 ms; budget 1 s (was >10 s on 7.27).
    assert!(
        elapsed.as_secs_f64() < 1.0,
        "JOIN … LIMIT 5 took {elapsed:?} on the 25k dataset (budget 1 s)"
    );
}

/// v7.34 (SPGS/SPGE perf bar) — SPGE-side PROJ p50 probe. Mirrors the
/// SPGS-side `scripts/wire-latency-probe.sh` PROJ shape (25 k-row
/// non-aggregate JOIN projection, 5 cells per row) but runs straight
/// through `Engine::execute` so the eprintln below carries the wire-
/// free baseline. Pair this with the wire-probe SPGS number to decide
/// whether (SPGS - SPGE) gap is wire-side encode/framing or whether
/// SPGE itself is already on the same plateau — the latter would mean
/// "SPGE > PG18" is just TCP arbitrage, not real internal speed.
#[test]
fn proj_query_on_25k_rows_embed_p50() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed(&mut db);
    const PROJ: &str = "SELECT m.id, m.subject, m.sender, m.internal_date, mb.user_address \
        FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
        WHERE mb.user_address = 'u@x'";
    db.execute(PROJ).unwrap();
    let mut samples = Vec::with_capacity(10);
    for _ in 0..10 {
        let t = Instant::now();
        let r = db.execute(PROJ).unwrap();
        let elapsed = t.elapsed();
        match r {
            QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 25_000),
            other => panic!("{other:?}"),
        }
        samples.push(elapsed.as_secs_f64() * 1000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = samples[samples.len() / 2];
    let min = samples[0];
    let max = *samples.last().unwrap();
    eprintln!("proj_25k SPGE: p50={p50:.1}ms min={min:.1}ms max={max:.1}ms (n=10)");
    assert!(p50 < 1000.0, "PROJ SPGE p50={p50}ms (budget 1 s)");
}
