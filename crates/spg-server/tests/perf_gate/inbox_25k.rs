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
    // Warm-up once (plan cache, allocator), then measure.
    db.execute(INBOX).unwrap();
    let t = Instant::now();
    let r = db.execute(INBOX).unwrap();
    let elapsed = t.elapsed();
    match r {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 50),
        other => panic!("{other:?}"),
    }
    // Measured ~1.1 s on an M-series laptop (release); 4 s budget
    // gives shared-runner headroom while still failing the
    // pre-round-22 executor (which never returned at all).
    assert!(
        elapsed.as_secs_f64() < 4.0,
        "inbox query took {elapsed:?} on the 25k dataset (budget 4 s)"
    );
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
