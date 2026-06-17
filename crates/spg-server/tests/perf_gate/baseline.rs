//! v7.34 (SPGS/SPGE perf bar) — SPGE-side baseline harness.
//!
//! Cover the six query shapes the SPGS-side `scripts/wire-latency-probe.sh`
//! exercises, but go straight through `Engine::execute` (no pgwire, no
//! docker psql) so the eprintln below carries the wire-free baseline.
//! Pair with the wire-probe SPGS + PG18 numbers and the per-query red-line
//! evaluation in `.claude/notes/perf-baseline-vX.Y.Z.md`.
//!
//! Naming discipline (memory: feedback-spgs-spge-perf-bar):
//!   * SPGS = server wire (pgwire, docker psql in the probe).
//!   * SPGE = embedded (this file, `Engine::execute` direct).
//!   Never report "SPG" — collapses two cost structures into one number.
//!
//! Red lines (user, 2026-06-17/18):
//!   * SPGS must > PG18 wire (any margin; losing = regression).
//!   * SPGE must be measurably faster than SPGS by more than wire overhead
//!     alone — otherwise SPGE only wins by skipping TCP+encode, which
//!     means SPGE internal is no faster than PG internal (just hidden by
//!     wire savings).
//!
//! Each shape: one warm-up + ten measured iterations, p50/min/max via
//! eprintln so cool-host CI runs (`--nocapture` or `gates --full`) carry
//! the authoritative number. The hard assertion is loose — only fails the
//! gate when the SPGE p50 blows past a generous budget — because the
//! point here is the baseline, not the SLO. Hard red-line gating moves to
//! the assertion sweep once we trust the cool baseline.

use spg_engine::{Engine, QueryResult};
use std::time::Instant;

fn seed_inbox_25k(db: &mut Engine) {
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
    let body = "lorem ipsum dolor sit amet ".repeat(40);
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

/// 1 warm + n measured; print p50/min/max and assert against `budget_ms`.
fn time_query(db: &mut Engine, sql: &str, n: usize, label: &str, budget_ms: f64) {
    db.execute(sql).unwrap(); // warm
    let mut samples = Vec::with_capacity(n);
    for _ in 0..n {
        let t = Instant::now();
        let r = db.execute(sql).unwrap();
        let elapsed = t.elapsed();
        if !matches!(r, QueryResult::Rows { .. } | QueryResult::CommandOk { .. }) {
            panic!("unexpected result for {label}: {r:?}");
        }
        samples.push(elapsed.as_secs_f64() * 1000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = samples[samples.len() / 2];
    let min = samples[0];
    let max = *samples.last().unwrap();
    eprintln!("baseline {label} SPGE: p50={p50:.2}ms min={min:.2}ms max={max:.2}ms (n={n})");
    assert!(
        p50 < budget_ms,
        "{label} SPGE p50={p50:.2}ms exceeded baseline budget {budget_ms} ms"
    );
}

// Shape 1: SELECT-1 — tiny single-row pluck. SPGS baseline is "near-pure
// wire RT"; SPGE drops both the wire and most of the executor.
#[test]
fn baseline_select_1() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_inbox_25k(&mut db);
    time_query(
        &mut db,
        "SELECT id FROM messages WHERE id = 1",
        10,
        "select_1",
        50.0,
    );
}

// Shape 2: SELECT-COUNT-STAR — full scan, single scalar out. No wire
// encode of any consequence; isolates aggregate plan + table walk.
#[test]
fn baseline_select_count_star() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_inbox_25k(&mut db);
    time_query(
        &mut db,
        "SELECT COUNT(*) FROM messages",
        10,
        "select_count_star",
        100.0,
    );
}

// Shape 3: PROJ-25k — 25k-row non-aggregate JOIN projection (5 cells/
// row). Same SQL the wire-probe sends as `PROJ`. SPGS pays cell encode +
// DataRow framing + TCP 2 MB push on top of this; SPGE does not.
#[test]
fn baseline_proj_25k() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_inbox_25k(&mut db);
    time_query(
        &mut db,
        "SELECT m.id, m.subject, m.sender, m.internal_date, mb.user_address \
         FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
         WHERE mb.user_address = 'u@x'",
        10,
        "proj_25k",
        500.0,
    );
}

// Shape 4: INBOX-25k — 17-col / 3 correlated subqueries / GROUP BY
// thread_id / LIMIT 50. Same SQL the wire-probe sends as `INBOX`. Output
// is only 50 rows so wire encode ≈ 0 — SPGS/SPGE diff here isolates pure
// SQL execution overhead (= wire vs no wire essentially indistinguishable).
#[test]
fn baseline_inbox_25k() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_inbox_25k(&mut db);
    time_query(
        &mut db,
        "SELECT m.thread_id, MAX(m.subject), COUNT(DISTINCT m.id), MAX(m.internal_date), \
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
            ORDER BY MAX(m.internal_date) DESC LIMIT 50",
        10,
        "inbox_25k",
        4000.0,
    );
}

// Shape 5: EXISTS-FILTER — mailrs `count_unseen` shape. The classic
// "EXISTS over JOIN" decorrelation target — pre-7.33.0 was 1.39 s, the
// 7.33-era treatment dropped SPGE to ~130 ms (n=1 informal). This is the
// repeatable measurement.
#[test]
fn baseline_exists_in_60() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_inbox_25k(&mut db);
    time_query(
        &mut db,
        "SELECT COUNT(*) FROM messages m \
            JOIN mailboxes mb ON m.mailbox_id = mb.id \
            WHERE mb.user_address = 'u@x' \
              AND EXISTS (SELECT 1 FROM email_analysis ea WHERE ea.message_id = m.id)",
        10,
        "exists_in_60",
        500.0,
    );
}

// Diagnostic: three equivalent shapes for the EXISTS-FILTER baseline,
// to isolate whether the 48 ms SPGE cost is the EXISTS decorr not
// firing or whether a deeper semi-join plan is missing. PG handles all
// three identically (hash semi-join under the covers).
#[test]
fn baseline_exists_filter_three_forms() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_inbox_25k(&mut db);
    let forms: &[(&str, &str)] = &[
        (
            "exists_subquery",
            "SELECT COUNT(*) FROM messages m \
                JOIN mailboxes mb ON m.mailbox_id = mb.id \
                WHERE mb.user_address = 'u@x' \
                  AND EXISTS (SELECT 1 FROM email_analysis ea WHERE ea.message_id = m.id)",
        ),
        (
            "in_subquery",
            "SELECT COUNT(*) FROM messages m \
                JOIN mailboxes mb ON m.mailbox_id = mb.id \
                WHERE mb.user_address = 'u@x' \
                  AND m.id IN (SELECT message_id FROM email_analysis)",
        ),
        (
            "manual_join",
            "SELECT COUNT(*) FROM messages m \
                JOIN mailboxes mb ON m.mailbox_id = mb.id \
                JOIN email_analysis ea ON ea.message_id = m.id \
                WHERE mb.user_address = 'u@x'",
        ),
    ];
    for (label, sql) in forms {
        time_query(&mut db, sql, 10, label, 1000.0);
    }
}

// Shape 6: GET-CONVERSATIONS-IN(60) — the mailrs prod hot path 169ef66
// fixed (snippet subquery JOIN, IN-list of 60 thread ids). Picks 60
// stable thread ids from the seed deterministically.
#[test]
fn baseline_get_conversations_in_60() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_inbox_25k(&mut db);
    // 60 deterministic thread ids from the seed (mod 25 k so they hit).
    let ids: String = (0..60)
        .map(|i| format!("'th-{}'", i * 400))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT m.thread_id, MAX(m.internal_date), \
                COALESCE((SELECT LEFT(ea.summary, 80) FROM email_analysis ea \
                          JOIN messages m_snip ON ea.message_id = m_snip.id \
                          WHERE m_snip.thread_id = m.thread_id \
                            AND ea.summary IS NOT NULL AND ea.summary != '' \
                          ORDER BY m_snip.internal_date DESC LIMIT 1), '') \
         FROM messages m \
         WHERE m.thread_id IN ({ids}) \
         GROUP BY m.thread_id"
    );
    time_query(&mut db, &sql, 10, "get_conversations_in_60", 1000.0);
}
