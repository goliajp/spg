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

// Diagnostic: equivalent IN forms — uncorrelated subquery vs literal
// InList vs JOIN. If `IN (subquery)` is 50× slower than `IN (lit, …)`
// of the same expansion, materialize-once is failing.
#[test]
fn baseline_in_subquery_three_forms() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_inbox_25k(&mut db);
    // Pre-fetch 6000 message_ids to build the literal InList probe.
    let ids: Vec<i64> = match db
        .execute("SELECT message_id FROM email_analysis ORDER BY message_id")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .filter_map(|r| match r.values.into_iter().next() {
                Some(spg_storage::Value::BigInt(n)) => Some(n),
                Some(spg_storage::Value::Int(n)) => Some(i64::from(n)),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let lit_in_list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let forms: &[(&str, String)] = &[
        (
            "in_subquery_join",
            "SELECT COUNT(*) FROM messages m \
                JOIN mailboxes mb ON m.mailbox_id = mb.id \
                WHERE mb.user_address = 'u@x' \
                  AND m.id IN (SELECT message_id FROM email_analysis)"
                .into(),
        ),
        (
            "in_literal_list_join",
            format!(
                "SELECT COUNT(*) FROM messages m \
                    JOIN mailboxes mb ON m.mailbox_id = mb.id \
                    WHERE mb.user_address = 'u@x' AND m.id IN ({lit_in_list})"
            ),
        ),
        (
            // No JOIN — isolate the IN-list eval on a plain table scan.
            "in_literal_list_no_join",
            format!("SELECT COUNT(*) FROM messages WHERE id IN ({lit_in_list})"),
        ),
        (
            // Non-aggregate JOIN + InList — isolates "is it the
            // aggregate-with-JOIN path that loses InSet, or is it any
            // JOIN with WHERE-InList?"
            "in_literal_list_join_proj",
            format!(
                "SELECT m.id FROM messages m \
                    JOIN mailboxes mb ON m.mailbox_id = mb.id \
                    WHERE mb.user_address = 'u@x' AND m.id IN ({lit_in_list})"
            ),
        ),
    ];
    for (label, sql) in forms {
        time_query(&mut db, sql, 10, label, 1000.0);
    }
}

// Shape 5b: NOT-EXISTS-FILTER — mailrs prod content_worker hot-path
// (`spg-7.34-prod-load-conn-pool-still-degraded-2026-06-17.md`). The
// 7.34.0 EXISTS decorrelation work didn't fire for this NOT EXISTS
// shape and prod stayed `status=degraded` an hour after cutover. After
// the v7.34.2 `pull_up_exists_sublinks` plan-time pass this should
// collapse into a LEFT JOIN + IS NULL semi-anti-join (the same
// `convert_EXISTS_sublink_to_join` rewrite PG does).
#[test]
fn baseline_not_exists_filter() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_inbox_25k(&mut db);
    time_query(
        &mut db,
        "SELECT m.id, m.sender FROM messages m \
            JOIN mailboxes mb ON m.mailbox_id = mb.id \
            WHERE mb.user_address = 'u@x' \
              AND NOT EXISTS (SELECT 1 FROM email_analysis ea WHERE ea.message_id = m.id) \
            ORDER BY m.id DESC LIMIT 200",
        10,
        "not_exists_filter",
        500.0,
    );
}

// v7.34.3 (mailrs prod report #5,
// stables/mailrs/.../spg-7.34.2-not-exists-pullup-not-firing-2026-06-17.md)
// — reproduce the EXACT mailrs prod content_worker SQL shape so we can
// see whether `pull_up_exists_sublinks` actually fires on it.
// mailrs:
//   SELECT m.id, m.sender, m.maildir_id, mb.user_address
//     FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id
//    WHERE m.size > 0
//      AND NOT EXISTS (SELECT 1 FROM attachment_content ac WHERE ac.message_id = m.id)
//    ORDER BY m.id DESC LIMIT $1
#[test]
#[ignore = "250k-row seed runs ~40 s; run via `cargo test ... -- --ignored`"]
fn baseline_mailrs_prod_not_exists_shape() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    // Seed prod-like shape: messages table has a `size` column.
    // attachment_content is sparse (~6 % of messages have an attachment).
    db.execute("CREATE TABLE mailboxes (id BIGSERIAL PRIMARY KEY, name TEXT, user_address TEXT)")
        .unwrap();
    db.execute(
        "CREATE TABLE messages (id BIGSERIAL PRIMARY KEY, mailbox_id BIGINT, sender TEXT, \
            maildir_id TEXT, size BIGINT, internal_date BIGINT)",
    )
    .unwrap();
    db.execute("CREATE TABLE attachment_content (message_id BIGINT PRIMARY KEY, payload TEXT)")
        .unwrap();
    db.execute("CREATE INDEX idx_messages_size ON messages(size)")
        .unwrap();
    for i in 0..25 {
        db.execute(&format!(
            "INSERT INTO mailboxes (name, user_address) VALUES ('mb{i}', 'u@x')"
        ))
        .unwrap();
    }
    // 250 000 messages — matches mailrs prod scale exactly. If the
    // pull-up fires + the resulting plan is efficient (ORDER BY id
    // DESC + LIMIT walk), we should see <25 ms (the mailrs sqlx
    // inline budget). Slow here = the gap mailrs prod #5 hit.
    for batch in 0..500 {
        let mut vals = Vec::new();
        for j in 0..500 {
            let i = batch * 500 + j;
            vals.push(format!(
                "({}, 's{}@x', 'md-{i}', {}, {})",
                (i % 25) + 1,
                i % 100,
                if i % 17 == 0 { 0 } else { 1024 },
                1_700_000_000 + i,
            ));
        }
        db.execute(&format!(
            "INSERT INTO messages (mailbox_id, sender, maildir_id, size, internal_date) VALUES {}",
            vals.join(",")
        ))
        .unwrap();
    }
    // attachment_content covers ~6 % of message ids (every 17th).
    let mut vals = Vec::new();
    let mut n = 0;
    for i in (1..=250_000).step_by(17) {
        vals.push(format!("({}, 'payload-{i}')", i));
        n += 1;
        if n % 500 == 0 {
            db.execute(&format!(
                "INSERT INTO attachment_content (message_id, payload) VALUES {}",
                vals.join(",")
            ))
            .unwrap();
            vals.clear();
        }
    }
    if !vals.is_empty() {
        db.execute(&format!(
            "INSERT INTO attachment_content (message_id, payload) VALUES {}",
            vals.join(",")
        ))
        .unwrap();
    }
    // The NOT EXISTS path: v7.34.2 pulled up via LEFT JOIN + IS NULL
    // but at this scale the LEFT JOIN materialised the whole outer
    // rel; v7.34.3 prefers the NOT IN form (InSet via
    // subquery_replacement) which runs at the same plateau as the
    // plain ORDER BY + LIMIT walk.
    time_query(
        &mut db,
        "SELECT m.id, m.sender, m.maildir_id, mb.user_address \
         FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
         WHERE m.size > 0 \
           AND NOT EXISTS (SELECT 1 FROM attachment_content ac WHERE ac.message_id = m.id) \
         ORDER BY m.id DESC LIMIT 200",
        10,
        "mailrs_prod_not_exists",
        500.0,
    );
    // Equivalent NOT IN — should route through SPG's existing uncorrelated
    // IN-list materialise + InSet membership path. If MUCH faster than
    // NOT EXISTS, the gap is the per-row dispatch the pull-up tries to
    // remove but the resulting LEFT JOIN + IS NULL plan still costs.
    time_query(
        &mut db,
        "SELECT m.id, m.sender, m.maildir_id, mb.user_address \
         FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
         WHERE m.size > 0 \
           AND m.id NOT IN (SELECT message_id FROM attachment_content) \
         ORDER BY m.id DESC LIMIT 200",
        10,
        "mailrs_prod_not_in",
        500.0,
    );
    // Plain primary-table walk with NO subquery — what we'd expect
    // if the executor can early-stop on ORDER BY id DESC + LIMIT.
    time_query(
        &mut db,
        "SELECT m.id, m.sender, m.maildir_id FROM messages m \
         WHERE m.size > 0 \
         ORDER BY m.id DESC LIMIT 200",
        10,
        "mailrs_prod_plain_limit",
        500.0,
    );
}

// Shape 5b: mailrs PROD-SHAPE NOT EXISTS — matches
// `mailrs/scripts/init-schema.sql:217-235` EXACTLY:
//   attachment_content (
//     id BIGSERIAL PRIMARY KEY,
//     message_id BIGINT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
//     ...
//     UNIQUE(message_id, attachment_index)
//   )
// vs Shape 5 which seeded `message_id BIGINT PRIMARY KEY` — that's a
// different shape (single-col PK on message_id vs composite UNIQUE).
// `spg-7.34.5-order-by-limit-walker-not-firing-2026-06-17.md` reports
// 7.34.5 walker did NOT fire on prod — this baseline is the smoking
// gun to either reproduce the miss or rule schema shape out.
#[test]
#[ignore = "250k-row seed runs ~40 s; run via `cargo test ... -- --ignored`"]
fn baseline_mailrs_prod_not_exists_real_schema() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    db.execute("CREATE TABLE mailboxes (id BIGSERIAL PRIMARY KEY, name TEXT, user_address TEXT)")
        .unwrap();
    db.execute(
        "CREATE TABLE messages (id BIGSERIAL PRIMARY KEY, mailbox_id BIGINT, sender TEXT, \
            maildir_id TEXT, size BIGINT, internal_date BIGINT)",
    )
    .unwrap();
    // The prod-shape attachment_content: surrogate `id` PK + `message_id`
    // NOT NULL FK + composite UNIQUE on `(message_id, attachment_index)`.
    db.execute(
        "CREATE TABLE attachment_content (\
            id BIGSERIAL PRIMARY KEY, \
            message_id BIGINT NOT NULL, \
            attachment_index SMALLINT NOT NULL, \
            content_type TEXT NOT NULL, \
            extracted_text TEXT, \
            UNIQUE(message_id, attachment_index))",
    )
    .unwrap();
    db.execute("CREATE INDEX idx_attachment_content_message ON attachment_content(message_id)")
        .unwrap();
    db.execute("CREATE INDEX idx_messages_size ON messages(size)")
        .unwrap();
    for i in 0..25 {
        db.execute(&format!(
            "INSERT INTO mailboxes (name, user_address) VALUES ('mb{i}', 'u@x')"
        ))
        .unwrap();
    }
    for batch in 0..500 {
        let mut vals = Vec::new();
        for j in 0..500 {
            let i = batch * 500 + j;
            vals.push(format!(
                "({}, 's{}@x', 'md-{i}', {}, {})",
                (i % 25) + 1,
                i % 100,
                if i % 17 == 0 { 0 } else { 1024 },
                1_700_000_000 + i,
            ));
        }
        db.execute(&format!(
            "INSERT INTO messages (mailbox_id, sender, maildir_id, size, internal_date) VALUES {}",
            vals.join(",")
        ))
        .unwrap();
    }
    let mut vals = Vec::new();
    let mut n = 0;
    for i in (1..=250_000).step_by(17) {
        vals.push(format!("({i}, 0, 'text/plain', 'payload-{i}')"));
        n += 1;
        if n % 500 == 0 {
            db.execute(&format!(
                "INSERT INTO attachment_content (message_id, attachment_index, content_type, extracted_text) VALUES {}",
                vals.join(",")
            ))
            .unwrap();
            vals.clear();
        }
    }
    if !vals.is_empty() {
        db.execute(&format!(
            "INSERT INTO attachment_content (message_id, attachment_index, content_type, extracted_text) VALUES {}",
            vals.join(",")
        ))
        .unwrap();
    }
    time_query(
        &mut db,
        "SELECT m.id, m.sender, m.maildir_id, mb.user_address \
         FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
         WHERE m.size > 0 \
           AND NOT EXISTS (SELECT 1 FROM attachment_content ac WHERE ac.message_id = m.id) \
         ORDER BY m.id DESC LIMIT 200",
        10,
        "mailrs_prod_real_schema_not_exists",
        500.0,
    );
}

// Shape 7a — mailrs `count_messages` SQL verbatim
// (`crates/mailbox/src/pg/message_ops/read.rs:178`). The simplest
// "stats" sub-query — JOIN messages × mailboxes, COUNT(*). PG p50
// from the staging side-by-side was ~5-10 ms; the 5/5 mailrs
// endpoints all add up via spg-sqlx + handler overhead but each
// sub-query's SPGE p50 is the real ceiling for the wrapped wall-clock.
#[test]
fn baseline_count_messages() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_inbox_25k(&mut db);
    time_query(
        &mut db,
        "SELECT COUNT(*) FROM messages m \
         JOIN mailboxes mb ON m.mailbox_id = mb.id \
         WHERE mb.user_address = 'u@x'",
        10,
        "count_messages",
        100.0,
    );
}

// Shape 7b — mailrs `user_storage_usage`
// (`crates/mailbox/src/pg/usage_ops.rs:6`). SUM size over the
// user's messages. seed_inbox_25k doesn't have a `size` column —
// substitute LENGTH(text_body) to match the work shape (still a
// per-row materialisation + integer accumulator).
#[test]
fn baseline_user_storage_usage() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_inbox_25k(&mut db);
    time_query(
        &mut db,
        "SELECT COALESCE(SUM(LENGTH(m.text_body)), 0) FROM messages m \
         JOIN mailboxes mb ON m.mailbox_id = mb.id \
         WHERE mb.user_address = 'u@x'",
        10,
        "user_storage_usage",
        100.0,
    );
}

// Shape 7c — mailrs `list_conversation_categories`
// (`crates/mailbox/src/pg/search_ops.rs:176`). 3-way JOIN +
// GROUP BY category + COUNT DISTINCT thread_id + ORDER BY count
// DESC. Hot on the stats dashboard.
#[test]
fn baseline_list_categories() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_inbox_25k(&mut db);
    time_query(
        &mut db,
        "SELECT ea.category, COUNT(DISTINCT m.thread_id) \
         FROM email_analysis ea \
         JOIN messages m ON ea.message_id = m.id \
         JOIN mailboxes mb ON m.mailbox_id = mb.id \
         WHERE mb.user_address = 'u@x' AND m.thread_id != '' \
         GROUP BY ea.category \
         ORDER BY COUNT(DISTINCT m.thread_id) DESC",
        10,
        "list_categories",
        500.0,
    );
}

// Shape 7d — mailrs `list_thread_messages` simplified
// (`crates/mailbox/src/pg/thread_ops/mod.rs:43`). Single-thread
// fetch by (user, thread_id). Production form has a MIN(id)
// SubPlan for dedup; we test the simpler shape because the
// SubPlan dedup is mailrs-specific deduplication semantics, not
// a perf-bottleneck shape. PG side this is a textbook indexed
// thread-id seek + JOIN — 5-15 ms wall-clock.
#[test]
fn baseline_list_thread_messages() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_inbox_25k(&mut db);
    time_query(
        &mut db,
        "SELECT m.id, m.mailbox_id, m.sender, m.subject, m.internal_date, \
                m.flags, m.message_id, m.thread_id, mb.user_address, \
                COALESCE(m.importance_level, 'normal'), COALESCE(m.importance_score, 0.0) \
         FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
         WHERE mb.user_address = 'u@x' AND m.thread_id = 'th-1000' \
         ORDER BY m.internal_date ASC",
        10,
        "list_thread_messages",
        100.0,
    );
}

// Shape 7 — GET-CONTACTS — mailrs `spg-7.35.0-perf-asks-2026-06-18.md`
// Ask 1's "cleanest signal" 3.21× ratio (315 ms spg / 98 ms PG, no
// cache effect on either side). The SQL is the exact source from
// `crates/mailbox/src/pg/search_ops.rs:search_contacts`:
//
//   SELECT sender FROM messages m
//   JOIN mailboxes mb ON m.mailbox_id = mb.id
//   WHERE mb.user_address = $1 AND sender ILIKE $2 AND sender != ''
//   GROUP BY sender
//   ORDER BY MAX(internal_date) DESC LIMIT $3
//
// Bound params in the benchmark: $1 = 'u@x', $2 = '%%' (empty-query
// default — ILIKE always true), $3 = 20. PG's measured plan on the
// 24k-row catalog: Seq Scan 33ms → Hash Join 7ms → Sort 25ms →
// GroupAggregate 4ms → top-N 0ms = 70ms.
#[test]
fn baseline_get_contacts() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_inbox_25k(&mut db);
    time_query(
        &mut db,
        "SELECT m.sender FROM messages m \
         JOIN mailboxes mb ON m.mailbox_id = mb.id \
         WHERE mb.user_address = 'u@x' AND m.sender ILIKE '%%' AND m.sender != '' \
         GROUP BY m.sender \
         ORDER BY MAX(m.internal_date) DESC LIMIT 20",
        10,
        "get_contacts",
        500.0,
    );
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
