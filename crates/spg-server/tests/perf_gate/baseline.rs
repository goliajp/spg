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

// v7.37 probe — same shape as user_storage_usage but with the JOIN
// folded to a direct mailbox_id filter. Measures the structural
// ceiling: how fast is the SUM(LENGTH(text_body)) path WITHOUT the
// JOIN executor walk? If this lands at ~PG18 (2.3 ms), the gap is
// purely the JOIN walk; if it's still well above, the bottleneck
// is per-row PV indirection + Value enum.
#[test]
fn baseline_user_storage_usage_fold_probe() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_inbox_25k(&mut db);
    time_query(
        &mut db,
        "SELECT COALESCE(SUM(LENGTH(text_body)), 0) FROM messages \
         WHERE mailbox_id = 1",
        10,
        "user_storage_usage_fold_probe",
        100.0,
    );
}

// v7.37 — does qualifying the column slow the path?
#[test]
fn baseline_count_messages_fold_eq_qualified() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_inbox_25k(&mut db);
    time_query(
        &mut db,
        "SELECT COUNT(*) FROM messages m WHERE m.mailbox_id = 1",
        10,
        "fold_eq_qualified",
        100.0,
    );
}

// v7.37 — does IN-list (size 1) match `=`?
#[test]
fn baseline_count_messages_fold_in1() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_inbox_25k(&mut db);
    time_query(
        &mut db,
        "SELECT COUNT(*) FROM messages m WHERE m.mailbox_id IN (1)",
        10,
        "fold_in1",
        100.0,
    );
}

// v7.37 — same shape my fold actually produces: 25 mailbox ids
// in the IN list.
#[test]
fn baseline_count_messages_fold_in25() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_inbox_25k(&mut db);
    time_query(
        &mut db,
        "SELECT COUNT(*) FROM messages m WHERE m.mailbox_id IN \
         (1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25)",
        10,
        "fold_in25",
        100.0,
    );
}

// v7.37 probe — same as count_messages but with the JOIN folded.
#[test]
fn baseline_count_messages_fold_probe() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_inbox_25k(&mut db);
    time_query(
        &mut db,
        "SELECT COUNT(*) FROM messages WHERE mailbox_id = 1",
        10,
        "count_messages_fold_probe",
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
// v7.37.3 — general SPG perf bar covering the inbox-listing UI
// canonical shape: `GROUP BY <text column> + per-group aggregate +
// ORDER BY <aggregate> [DESC] LIMIT k`. Hit by every "show top N by
// recent activity" surface across all SPG clients, not just mailrs.
// Three cardinalities so the regression isn't masked by a single
// distinct-group count: 100 / 1 000 / 5 000 distinct senders over
// the same 25 k-row seed (sender pool is `sender{i % N}@example`,
// so high-cardinality cases are more groups per same input). LIMIT
// fixed at 20 (typical "top contacts" UI). Tests the structural
// top-K LIMIT path inside `aggregate::sort_synth_by_order_by`.
// v7.37.3 — same as `seed_with_sender_cardinality` but text_body is a
// `body_len`-byte ASCII payload (random-ish per message). Probes
// SPG's row-major storage sensitivity to row width — the contacts /
// list_conversations SQL doesn't SELECT text_body, but PV<Row> still
// pulls the whole row's bytes into cache when iterating, so wide
// rows can cost much more than narrow ones.
fn seed_with_sender_cardinality_wide(
    db: &mut Engine,
    n_messages: usize,
    distinct_senders: usize,
    body_len: usize,
) {
    db.execute("CREATE TABLE mailboxes (id BIGSERIAL PRIMARY KEY, name TEXT, user_address TEXT)")
        .unwrap();
    db.execute(
        "CREATE TABLE messages (id BIGSERIAL PRIMARY KEY, mailbox_id BIGINT, thread_id TEXT, \
         subject TEXT, sender TEXT, internal_date BIGINT, flags BIGINT, pinned BOOLEAN, \
         archived BOOLEAN, importance_level TEXT, importance_score REAL, message_id TEXT, \
         text_body TEXT)",
    )
    .unwrap();
    db.execute("INSERT INTO mailboxes (name, user_address) VALUES ('default', 'u@x')")
        .unwrap();
    // Deterministic ASCII body so the seed is stable across runs.
    let body_template: String = (0..body_len)
        .map(|i| char::from((b'a' + (i % 26) as u8) as u8))
        .collect();
    let body_sql_safe = body_template.replace('\'', "''");
    let mut vals = String::new();
    let mut count = 0;
    for i in 0..n_messages {
        if !vals.is_empty() {
            vals.push(',');
        }
        use std::fmt::Write;
        let sender_idx = i % distinct_senders;
        let _ = write!(
            vals,
            "(1, 'thr{}', 'subj{i}', 'sender{sender_idx}@example.com', {}, 0, false, false, 'normal', 0.5, 'm{i}', '{body_sql_safe}')",
            i % 1000,
            1_700_000_000_i64 + i as i64
        );
        count += 1;
        if count == 100 {
            let sql = format!(
                "INSERT INTO messages (mailbox_id, thread_id, subject, sender, internal_date, flags, pinned, archived, importance_level, importance_score, message_id, text_body) VALUES {vals}"
            );
            db.execute(&sql).unwrap();
            vals.clear();
            count = 0;
        }
    }
    if !vals.is_empty() {
        let sql = format!(
            "INSERT INTO messages (mailbox_id, thread_id, subject, sender, internal_date, flags, pinned, archived, importance_level, importance_score, message_id, text_body) VALUES {vals}"
        );
        db.execute(&sql).unwrap();
    }
}

fn seed_with_sender_cardinality(db: &mut Engine, n_messages: usize, distinct_senders: usize) {
    db.execute("CREATE TABLE mailboxes (id BIGSERIAL PRIMARY KEY, name TEXT, user_address TEXT)")
        .unwrap();
    db.execute(
        "CREATE TABLE messages (id BIGSERIAL PRIMARY KEY, mailbox_id BIGINT, thread_id TEXT, \
         subject TEXT, sender TEXT, internal_date BIGINT, flags BIGINT, pinned BOOLEAN, \
         archived BOOLEAN, importance_level TEXT, importance_score REAL, message_id TEXT, \
         text_body TEXT)",
    )
    .unwrap();
    db.execute("INSERT INTO mailboxes (name, user_address) VALUES ('default', 'u@x')")
        .unwrap();
    let mut vals = String::new();
    let mut count = 0;
    for i in 0..n_messages {
        if !vals.is_empty() {
            vals.push(',');
        }
        use std::fmt::Write;
        let sender_idx = i % distinct_senders;
        let _ = write!(
            vals,
            "(1, 'thr{}', 'subj{i}', 'sender{sender_idx}@example.com', {}, 0, false, false, 'normal', 0.5, 'm{i}', 'body {i}')",
            i % 1000,
            1_700_000_000_i64 + i as i64
        );
        count += 1;
        if count == 500 {
            let sql = format!(
                "INSERT INTO messages (mailbox_id, thread_id, subject, sender, internal_date, flags, pinned, archived, importance_level, importance_score, message_id, text_body) VALUES {vals}"
            );
            db.execute(&sql).unwrap();
            vals.clear();
            count = 0;
        }
    }
    if !vals.is_empty() {
        let sql = format!(
            "INSERT INTO messages (mailbox_id, thread_id, subject, sender, internal_date, flags, pinned, archived, importance_level, importance_score, message_id, text_body) VALUES {vals}"
        );
        db.execute(&sql).unwrap();
    }
}

const CONTACTS_TOPN_SQL: &str = "SELECT m.sender FROM messages m \
                                 JOIN mailboxes mb ON m.mailbox_id = mb.id \
                                 WHERE mb.user_address = 'u@x' AND m.sender ILIKE '%%' \
                                 AND m.sender != '' \
                                 GROUP BY m.sender \
                                 ORDER BY MAX(m.internal_date) DESC LIMIT 20";

#[test]
fn baseline_grouped_textkey_topn_100() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_with_sender_cardinality(&mut db, 25_000, 100);
    time_query(
        &mut db,
        CONTACTS_TOPN_SQL,
        10,
        "grouped_textkey_topn_100",
        500.0,
    );
}

#[test]
fn baseline_grouped_textkey_topn_1k() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_with_sender_cardinality(&mut db, 25_000, 1_000);
    time_query(
        &mut db,
        CONTACTS_TOPN_SQL,
        10,
        "grouped_textkey_topn_1k",
        500.0,
    );
}

#[test]
fn baseline_grouped_textkey_topn_5k() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_with_sender_cardinality(&mut db, 25_000, 5_000);
    time_query(
        &mut db,
        CONTACTS_TOPN_SQL,
        10,
        "grouped_textkey_topn_5k",
        500.0,
    );
}

// v7.37.3 — row-width sensitivity probe for the inbox-listing shape.
// Same SQL + cardinality as `grouped_textkey_topn_5k`, but with a
// realistic ~ 1 KiB / 5 KiB `text_body` per message (real emails sit
// in that range). Tests SPG's PV<Row> sequential-scan cost as a
// function of row width — every row read pulls the full Row into
// cache, so wide rows hurt even when the SQL doesn't SELECT the wide
// column.
#[test]
fn baseline_grouped_textkey_topn_5k_body1k() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_with_sender_cardinality_wide(&mut db, 25_000, 5_000, 1024);
    time_query(
        &mut db,
        CONTACTS_TOPN_SQL,
        10,
        "grouped_textkey_topn_5k_body1k",
        1500.0,
    );
}

#[test]
fn baseline_grouped_textkey_topn_5k_body5k() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_with_sender_cardinality_wide(&mut db, 25_000, 5_000, 5120);
    time_query(
        &mut db,
        CONTACTS_TOPN_SQL,
        10,
        "grouped_textkey_topn_5k_body5k",
        2000.0,
    );
}

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

// --- v7.38 P0 reproducer: mailrs prod /api/conversations?limit=50 ---
//
// Source: mailrs note `spg-7.37.3-prod-conversations-still-2.5s-user-
// visible-2026-06-18.md`. Staging 29k msgs warm p50 = 46 ms; prod 80–100k
// msgs warm = 2.5–2.7 s = ~50× regression at ~3× row count. Strongly
// suggests plan-shape switch past a catalog-size threshold (no stats /
// correlated subquery not decorrelated / DISTINCT agg cost blowup /
// 7.37.3 top-K LIMIT path silently disabled by this shape).
//
// The SQL below is verbatim from the prod note. Seed mimics mailrs's
// realistic distribution: 10 mailboxes per user, ~5 messages per thread
// (so thread_id GROUP BY has real cardinality, not the trivial 1-row-
// per-group `seed_inbox_25k` shape). Three cardinalities so we can see
// the switch point in plan choice.
fn seed_mailrs_inbox(db: &mut Engine, n_messages: usize) {
    db.execute("CREATE TABLE mailboxes (id BIGSERIAL PRIMARY KEY, name TEXT, user_address TEXT)")
        .unwrap();
    db.execute(
        "CREATE TABLE messages (id BIGSERIAL PRIMARY KEY, mailbox_id BIGINT, thread_id TEXT, \
         subject TEXT, sender TEXT, internal_date BIGINT, flags BIGINT, pinned BOOLEAN, \
         archived BOOLEAN, importance_level TEXT, importance_score REAL, message_id TEXT, \
         text_body TEXT)",
    )
    .unwrap();
    db.execute(
        "CREATE TABLE email_analysis (message_id BIGINT PRIMARY KEY, category TEXT, \
         summary TEXT, requires_action BOOLEAN)",
    )
    .unwrap();
    // Match mailrs prod indexes that matter for this shape.
    db.execute("CREATE INDEX idx_messages_thread ON messages(thread_id)")
        .unwrap();
    db.execute("CREATE INDEX idx_messages_thread_date ON messages(thread_id, internal_date DESC)")
        .unwrap();
    db.execute("CREATE INDEX idx_messages_mailbox ON messages(mailbox_id)")
        .unwrap();
    db.execute("CREATE INDEX idx_mailboxes_user ON mailboxes(user_address, name)")
        .unwrap();
    // mailrs prod /api/conversations references snoozed_conversations
    // via NOT EXISTS — schema needed even if rows are absent.
    db.execute(
        "CREATE TABLE snoozed_conversations (\
         thread_id TEXT NOT NULL, account_address TEXT NOT NULL, \
         snoozed_until BIGINT NOT NULL, \
         PRIMARY KEY (thread_id, account_address))",
    )
    .unwrap();

    // 10 mailboxes (Inbox, Sent, Archive, …), single user.
    for i in 0..10 {
        db.execute(&format!(
            "INSERT INTO mailboxes (name, user_address) VALUES ('mb{i}', 'lihao@golia.jp')"
        ))
        .unwrap();
    }

    // ~5 msgs per thread → n_messages/5 threads. ~600 distinct senders.
    let msgs_per_thread: usize = 5;
    let n_senders: usize = 600;
    let body = "lorem ipsum dolor sit amet ".repeat(20);
    let batch_size: usize = 500;
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
        // Roughly half of messages have a non-empty message_id (RFC822
        // Message-ID coverage in real corpora is ~70–90 %; pick 80 % to
        // exercise the CASE-WHEN-message_id branch in the SQL below).
        let mid = if i % 5 == 0 {
            String::new()
        } else {
            format!("mid-{i}")
        };
        // flags: roughly 30 % unread (bit 0 = 0).
        let flags = if i % 10 < 3 { 0 } else { 1 };
        let _ = write!(
            vals,
            "({}, 'th-{}', 'subj{i}', 'sender{}@example.com', {}, {}, false, false, 'normal', 0.5, '{}', '{body} {i}')",
            mailbox_id,
            thread_idx,
            sender_idx,
            1_700_000_000_i64 + i as i64,
            flags,
            mid
        );
        count += 1;
        if count == batch_size {
            db.execute(&format!(
                "INSERT INTO messages (mailbox_id, thread_id, subject, sender, internal_date, flags, pinned, archived, importance_level, importance_score, message_id, text_body) VALUES {vals}"
            )).unwrap();
            vals.clear();
            count = 0;
        }
    }
    if !vals.is_empty() {
        db.execute(&format!(
            "INSERT INTO messages (mailbox_id, thread_id, subject, sender, internal_date, flags, pinned, archived, importance_level, importance_score, message_id, text_body) VALUES {vals}"
        )).unwrap();
    }

    // email_analysis populated for ~25 % of messages (matches mailrs's
    // background analysis coverage rate).
    let ea_rows = n_messages / 4;
    let mut vals = String::new();
    let mut count = 0usize;
    for k in 0..ea_rows {
        let mid = (k * 4 + 1) as i64; // every 4th message id
        if !vals.is_empty() {
            vals.push(',');
        }
        use std::fmt::Write;
        let _ = write!(
            vals,
            "({mid}, 'cat{}', 'summary {k}', {})",
            k % 5,
            k % 2 == 0
        );
        count += 1;
        if count == 500 {
            db.execute(&format!(
                "INSERT INTO email_analysis (message_id, category, summary, requires_action) VALUES {vals}"
            )).unwrap();
            vals.clear();
            count = 0;
        }
    }
    if !vals.is_empty() {
        db.execute(&format!(
            "INSERT INTO email_analysis (message_id, category, summary, requires_action) VALUES {vals}"
        )).unwrap();
    }
    // v7.38 — run ANALYZE so the reorder pass has statistics. The
    // mailrs prod path triggers this fix via the LEFT-JOIN split-
    // point change; without ANALYZE, the secondary stats gate in
    // reorder_joins still bails. Mirrors what we ask customers to
    // do after a bulk import.
    db.execute("ANALYZE").unwrap();
}

// Stripped-down "minimal" shape: GROUP BY thread_id + MAX(internal_date)
// + ORDER BY MAX LIMIT 50. No DISTINCT aggs, no correlated subquery.
// Tests whether the 7.37.3 top-K LIMIT path triggers for this shape on
// a real grouping cardinality, and whether the join order picks mailboxes
// as driving table when the user_address filter is small.
const MAILRS_MINIMAL_SQL: &str = "\
SELECT m.thread_id, MAX(m.internal_date) \
  FROM messages m \
  JOIN mailboxes mb ON m.mailbox_id = mb.id \
 WHERE mb.user_address = 'lihao@golia.jp' \
 GROUP BY m.thread_id \
 ORDER BY MAX(m.internal_date) DESC \
 LIMIT 50";

// + DISTINCT aggs from the prod SQL (string_agg DISTINCT + 2 COUNT DISTINCT
// with nested CASE). No correlated subquery yet (SPGE binder rejects
// outer `MAX(m.id)` reference; that variant runs via SPGS only).
const MAILRS_DISTINCT_AGGS_SQL: &str = "\
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
       MAX(m.internal_date) \
  FROM messages m \
  JOIN mailboxes mb ON m.mailbox_id = mb.id \
 WHERE mb.user_address = 'lihao@golia.jp' \
 GROUP BY m.thread_id \
 ORDER BY MAX(m.internal_date) DESC \
 LIMIT 50";

fn dump_explain(db: &mut Engine, label: &str, sql: &str) {
    if let Ok(QueryResult::Rows { rows, .. }) = db.execute(&format!("EXPLAIN {sql}")) {
        eprintln!("--- EXPLAIN {label} ---");
        for row in rows {
            for cell in &row.values {
                eprintln!("{cell:?}");
            }
        }
        eprintln!("--- end EXPLAIN ---");
    }
}

#[test]
#[ignore = "P0 mailrs-shape reproducer; --include-ignored to run"]
fn baseline_mailrs_minimal_30k() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_mailrs_inbox(&mut db, 30_000);
    dump_explain(&mut db, "mailrs_minimal_30k", MAILRS_MINIMAL_SQL);
    time_query(
        &mut db,
        MAILRS_MINIMAL_SQL,
        10,
        "mailrs_minimal_30k",
        1500.0,
    );
}

#[test]
#[ignore = "P0 mailrs-shape reproducer; --include-ignored to run"]
fn baseline_mailrs_minimal_100k() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_mailrs_inbox(&mut db, 100_000);
    dump_explain(&mut db, "mailrs_minimal_100k", MAILRS_MINIMAL_SQL);
    time_query(
        &mut db,
        MAILRS_MINIMAL_SQL,
        10,
        "mailrs_minimal_100k",
        10_000.0,
    );
}

#[test]
#[ignore = "P0 mailrs-shape reproducer; --include-ignored to run"]
fn baseline_mailrs_distinct_aggs_30k() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_mailrs_inbox(&mut db, 30_000);
    dump_explain(
        &mut db,
        "mailrs_distinct_aggs_30k",
        MAILRS_DISTINCT_AGGS_SQL,
    );
    time_query(
        &mut db,
        MAILRS_DISTINCT_AGGS_SQL,
        10,
        "mailrs_distinct_aggs_30k",
        2_000.0,
    );
}

#[test]
#[ignore = "P0 mailrs-shape reproducer; --include-ignored to run"]
fn baseline_mailrs_distinct_aggs_100k() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_mailrs_inbox(&mut db, 100_000);
    dump_explain(
        &mut db,
        "mailrs_distinct_aggs_100k",
        MAILRS_DISTINCT_AGGS_SQL,
    );
    time_query(
        &mut db,
        MAILRS_DISTINCT_AGGS_SQL,
        10,
        "mailrs_distinct_aggs_100k",
        20_000.0,
    );
}

// === mailrs prod /api/conversations?limit=50 verbatim SQL ===
//
// Source: `mailrs/crates/mailbox/src/pg/thread_ops/query.rs:135–165`
// `PgMailboxStore::list_conversations` — confirmed via
// `mailrs/.claude/notes/spg-7.37.3-prod-conversations-real-sql-2026-06-18.md`.
//
// Client stack is `spg-sqlx::SpgPool` (in-process) when mailrs is built
// with `--features spg` (prod docker image). That = SPG `Engine::execute`
// = this perf_gate's path. Reproduces the prod path bit-for-bit.
//
// Only divergence from prod: `sc.snoozed_until > NOW()` replaced with
// `> 0` (the snoozed_conversations table is empty in seed, so NOT EXISTS
// short-circuits without entering the inner WHERE, but SPG still type-
// checks the predicate — and seeding TIMESTAMPTZ + NOW() adds
// non-determinism unrelated to the perf shape we're chasing). Semantics
// preserved on empty table.
const MAILRS_PROD_REAL_SQL: &str = "\
SELECT m.thread_id, MAX(m.subject), string_agg(DISTINCT m.sender, ','), \
       COUNT(DISTINCT CASE WHEN m.message_id != '' \
                           THEN m.message_id \
                           ELSE CAST(m.id AS TEXT) END), \
       COUNT(DISTINCT CASE WHEN (m.flags & 1) = 0 \
                           THEN CASE WHEN m.message_id != '' \
                                     THEN m.message_id \
                                     ELSE CAST(m.id AS TEXT) END \
                           END), \
       MAX(m.internal_date), \
       COALESCE((SELECT ea.category FROM email_analysis ea \
                   JOIN messages m2 ON ea.message_id = m2.id \
                  WHERE m2.thread_id = m.thread_id \
                  ORDER BY m2.internal_date DESC LIMIT 1), \
                'general'), \
       BOOL_OR((m.flags & 4) != 0), \
       COALESCE( \
         (SELECT LEFT(ea_snip.summary, 80) FROM email_analysis ea_snip \
            JOIN messages m_snip ON ea_snip.message_id = m_snip.id \
           WHERE m_snip.thread_id = m.thread_id \
             AND ea_snip.summary IS NOT NULL \
             AND ea_snip.summary != '' \
           ORDER BY m_snip.internal_date DESC LIMIT 1), \
         (SELECT LEFT(m3.text_body, 80) FROM messages m3 \
           WHERE m3.thread_id = m.thread_id \
             AND m3.text_body IS NOT NULL \
             AND m3.text_body != '' \
           ORDER BY m3.internal_date DESC LIMIT 1), \
         ''), \
       BOOL_OR(m.pinned), \
       BOOL_OR(m.archived), \
       COALESCE((array_agg(m.importance_level \
                           ORDER BY m.importance_score DESC NULLS LAST))[1], \
                'normal'), \
       COALESCE(MAX(m.importance_score), 0.0), \
       COALESCE(BOOL_OR(ea.requires_action), false), \
       COALESCE((array_agg(m.sender ORDER BY m.internal_date DESC))[1], ''), \
       COUNT(DISTINCT CASE WHEN mb.name = 'Sent' AND m.message_id != '' \
                           THEN m.message_id \
                           WHEN mb.name = 'Sent' \
                           THEN CAST(m.id AS TEXT) END) \
  FROM messages m \
       JOIN mailboxes mb ON m.mailbox_id = mb.id \
       LEFT JOIN email_analysis ea ON ea.message_id = m.id \
 WHERE mb.user_address = 'lihao@golia.jp' \
   AND thread_id != '' \
   AND NOT EXISTS (SELECT 1 FROM snoozed_conversations sc \
                    WHERE sc.thread_id = m.thread_id \
                      AND sc.account_address = mb.user_address \
                      AND sc.snoozed_until > 0) \
 GROUP BY m.thread_id \
 HAVING BOOL_OR(m.archived) = false \
    AND BOOL_OR(mb.name != 'Sent') = true \
 ORDER BY BOOL_OR(m.pinned) DESC, MAX(m.internal_date) DESC \
 LIMIT 50";

#[test]
#[ignore = "P0 mailrs prod-real reproducer; --include-ignored to run"]
fn baseline_mailrs_prod_real_30k() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_mailrs_inbox(&mut db, 30_000);
    dump_explain(&mut db, "mailrs_prod_real_30k", MAILRS_PROD_REAL_SQL);
    time_query(
        &mut db,
        MAILRS_PROD_REAL_SQL,
        10,
        "mailrs_prod_real_30k",
        5_000.0,
    );
}

#[test]
#[ignore = "P0 mailrs prod-real reproducer; --include-ignored to run"]
fn baseline_mailrs_prod_real_100k() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_mailrs_inbox(&mut db, 100_000);
    dump_explain(&mut db, "mailrs_prod_real_100k", MAILRS_PROD_REAL_SQL);
    // v7.37.4 A' instrumentation — snapshot batched-scalar fire/probe
    // counters before/after the 10-iteration timing loop to confirm
    // whether the LIMIT 1 + ORDER BY 1 subqueries actually take the
    // keyed-restriction path (or fall through to all-keys batch).
    use core::sync::atomic::Ordering;
    use spg_engine::{
        BATCHED_SCALAR_FALL_THROUGH_COUNT, BATCHED_SCALAR_KEYED_FIRE_COUNT,
        BATCHED_SCALAR_KEYED_PROBE_COUNT, EXISTS_BATCH_FALL_THROUGH_COUNT, EXISTS_BATCH_FIRE_COUNT,
        EXISTS_PULLUP_FIRE_COUNT,
    };
    let fb = BATCHED_SCALAR_KEYED_FIRE_COUNT.load(Ordering::Relaxed);
    let pb = BATCHED_SCALAR_KEYED_PROBE_COUNT.load(Ordering::Relaxed);
    let tb = BATCHED_SCALAR_FALL_THROUGH_COUNT.load(Ordering::Relaxed);
    let epb = EXISTS_PULLUP_FIRE_COUNT.load(Ordering::Relaxed);
    let ebb = EXISTS_BATCH_FIRE_COUNT.load(Ordering::Relaxed);
    let etb = EXISTS_BATCH_FALL_THROUGH_COUNT.load(Ordering::Relaxed);
    time_query(
        &mut db,
        MAILRS_PROD_REAL_SQL,
        10,
        "mailrs_prod_real_100k",
        20_000.0,
    );
    let fa = BATCHED_SCALAR_KEYED_FIRE_COUNT.load(Ordering::Relaxed);
    let pa = BATCHED_SCALAR_KEYED_PROBE_COUNT.load(Ordering::Relaxed);
    let ta = BATCHED_SCALAR_FALL_THROUGH_COUNT.load(Ordering::Relaxed);
    let epa = EXISTS_PULLUP_FIRE_COUNT.load(Ordering::Relaxed);
    let eba = EXISTS_BATCH_FIRE_COUNT.load(Ordering::Relaxed);
    let eta = EXISTS_BATCH_FALL_THROUGH_COUNT.load(Ordering::Relaxed);
    eprintln!(
        "[A'] mailrs_prod_real_100k batched-scalar: keyed_fires={} keyed_probes={} fall_through={}",
        fa - fb,
        pa - pb,
        ta - tb,
    );
    eprintln!(
        "[A'] mailrs_prod_real_100k EXISTS: pullup_fires={} batch_fires={} batch_fall_through={}",
        epa - epb,
        eba - ebb,
        eta - etb,
    );
}

// Ablation: same as MAILRS_PROD_REAL_SQL but the 3 correlated subqueries
// (category / summary / text_body snippet) replaced with constants.
// Delta to MAILRS_PROD_REAL_SQL on the same N isolates the subqueries'
// share of the prod cost. If big = SPG planner not using
// idx_messages_thread_date on the inner LIMIT-1 lookups.
const MAILRS_PROD_NO_SUBQ_SQL: &str = "\
SELECT m.thread_id, MAX(m.subject), string_agg(DISTINCT m.sender, ','), \
       COUNT(DISTINCT CASE WHEN m.message_id != '' \
                           THEN m.message_id \
                           ELSE CAST(m.id AS TEXT) END), \
       COUNT(DISTINCT CASE WHEN (m.flags & 1) = 0 \
                           THEN CASE WHEN m.message_id != '' \
                                     THEN m.message_id \
                                     ELSE CAST(m.id AS TEXT) END \
                           END), \
       MAX(m.internal_date), \
       'general' AS category_stub, \
       BOOL_OR((m.flags & 4) != 0), \
       '' AS snippet_stub, \
       BOOL_OR(m.pinned), \
       BOOL_OR(m.archived), \
       COALESCE((array_agg(m.importance_level \
                           ORDER BY m.importance_score DESC NULLS LAST))[1], \
                'normal'), \
       COALESCE(MAX(m.importance_score), 0.0), \
       COALESCE(BOOL_OR(ea.requires_action), false), \
       COALESCE((array_agg(m.sender ORDER BY m.internal_date DESC))[1], ''), \
       COUNT(DISTINCT CASE WHEN mb.name = 'Sent' AND m.message_id != '' \
                           THEN m.message_id \
                           WHEN mb.name = 'Sent' \
                           THEN CAST(m.id AS TEXT) END) \
  FROM messages m \
       JOIN mailboxes mb ON m.mailbox_id = mb.id \
       LEFT JOIN email_analysis ea ON ea.message_id = m.id \
 WHERE mb.user_address = 'lihao@golia.jp' \
   AND thread_id != '' \
   AND NOT EXISTS (SELECT 1 FROM snoozed_conversations sc \
                    WHERE sc.thread_id = m.thread_id \
                      AND sc.account_address = mb.user_address \
                      AND sc.snoozed_until > 0) \
 GROUP BY m.thread_id \
 HAVING BOOL_OR(m.archived) = false \
    AND BOOL_OR(mb.name != 'Sent') = true \
 ORDER BY BOOL_OR(m.pinned) DESC, MAX(m.internal_date) DESC \
 LIMIT 50";

// Ablation 2: same as PROD_REAL but drop LEFT JOIN ea entirely + the
// outer ea.requires_action ref. Isolates the LEFT-JOIN-by-PK cost.
const MAILRS_PROD_NO_LEFTJOIN_SQL: &str = "\
SELECT m.thread_id, MAX(m.subject), string_agg(DISTINCT m.sender, ','), \
       COUNT(DISTINCT CASE WHEN m.message_id != '' \
                           THEN m.message_id \
                           ELSE CAST(m.id AS TEXT) END), \
       COUNT(DISTINCT CASE WHEN (m.flags & 1) = 0 \
                           THEN CASE WHEN m.message_id != '' \
                                     THEN m.message_id \
                                     ELSE CAST(m.id AS TEXT) END \
                           END), \
       MAX(m.internal_date), \
       COALESCE((SELECT ea.category FROM email_analysis ea \
                   JOIN messages m2 ON ea.message_id = m2.id \
                  WHERE m2.thread_id = m.thread_id \
                  ORDER BY m2.internal_date DESC LIMIT 1), \
                'general'), \
       BOOL_OR((m.flags & 4) != 0), \
       '' AS snippet_stub, \
       BOOL_OR(m.pinned), \
       BOOL_OR(m.archived), \
       COALESCE((array_agg(m.importance_level \
                           ORDER BY m.importance_score DESC NULLS LAST))[1], \
                'normal'), \
       COALESCE(MAX(m.importance_score), 0.0), \
       false AS requires_action_stub, \
       COALESCE((array_agg(m.sender ORDER BY m.internal_date DESC))[1], ''), \
       COUNT(DISTINCT CASE WHEN mb.name = 'Sent' AND m.message_id != '' \
                           THEN m.message_id \
                           WHEN mb.name = 'Sent' \
                           THEN CAST(m.id AS TEXT) END) \
  FROM messages m \
       JOIN mailboxes mb ON m.mailbox_id = mb.id \
 WHERE mb.user_address = 'lihao@golia.jp' \
   AND thread_id != '' \
   AND NOT EXISTS (SELECT 1 FROM snoozed_conversations sc \
                    WHERE sc.thread_id = m.thread_id \
                      AND sc.account_address = mb.user_address \
                      AND sc.snoozed_until > 0) \
 GROUP BY m.thread_id \
 HAVING BOOL_OR(m.archived) = false \
    AND BOOL_OR(mb.name != 'Sent') = true \
 ORDER BY BOOL_OR(m.pinned) DESC, MAX(m.internal_date) DESC \
 LIMIT 50";

// Ablation 3: same as PROD_REAL but drop the 2 (array_agg ORDER BY...)[1]
// ordered-aggregates (replace with constants). Isolates the ordered-agg
// per-group sort cost.
const MAILRS_PROD_NO_ORDERED_AGG_SQL: &str = "\
SELECT m.thread_id, MAX(m.subject), string_agg(DISTINCT m.sender, ','), \
       COUNT(DISTINCT CASE WHEN m.message_id != '' \
                           THEN m.message_id \
                           ELSE CAST(m.id AS TEXT) END), \
       COUNT(DISTINCT CASE WHEN (m.flags & 1) = 0 \
                           THEN CASE WHEN m.message_id != '' \
                                     THEN m.message_id \
                                     ELSE CAST(m.id AS TEXT) END \
                           END), \
       MAX(m.internal_date), \
       'general' AS category_stub, \
       BOOL_OR((m.flags & 4) != 0), \
       '' AS snippet_stub, \
       BOOL_OR(m.pinned), \
       BOOL_OR(m.archived), \
       'normal' AS importance_stub, \
       COALESCE(MAX(m.importance_score), 0.0), \
       COALESCE(BOOL_OR(ea.requires_action), false), \
       '' AS sender_stub, \
       COUNT(DISTINCT CASE WHEN mb.name = 'Sent' AND m.message_id != '' \
                           THEN m.message_id \
                           WHEN mb.name = 'Sent' \
                           THEN CAST(m.id AS TEXT) END) \
  FROM messages m \
       JOIN mailboxes mb ON m.mailbox_id = mb.id \
       LEFT JOIN email_analysis ea ON ea.message_id = m.id \
 WHERE mb.user_address = 'lihao@golia.jp' \
   AND thread_id != '' \
   AND NOT EXISTS (SELECT 1 FROM snoozed_conversations sc \
                    WHERE sc.thread_id = m.thread_id \
                      AND sc.account_address = mb.user_address \
                      AND sc.snoozed_until > 0) \
 GROUP BY m.thread_id \
 HAVING BOOL_OR(m.archived) = false \
    AND BOOL_OR(mb.name != 'Sent') = true \
 ORDER BY BOOL_OR(m.pinned) DESC, MAX(m.internal_date) DESC \
 LIMIT 50";

#[test]
#[ignore = "P0 mailrs ablation: no LEFT JOIN ea; --include-ignored"]
fn baseline_mailrs_prod_no_leftjoin_100k() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_mailrs_inbox(&mut db, 100_000);
    dump_explain(
        &mut db,
        "mailrs_prod_no_leftjoin_100k",
        MAILRS_PROD_NO_LEFTJOIN_SQL,
    );
    time_query(
        &mut db,
        MAILRS_PROD_NO_LEFTJOIN_SQL,
        10,
        "mailrs_prod_no_leftjoin_100k",
        10_000.0,
    );
}

#[test]
#[ignore = "P0 mailrs ablation: no ordered_agg; --include-ignored"]
fn baseline_mailrs_prod_no_ordered_agg_100k() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_mailrs_inbox(&mut db, 100_000);
    dump_explain(
        &mut db,
        "mailrs_prod_no_ordered_agg_100k",
        MAILRS_PROD_NO_ORDERED_AGG_SQL,
    );
    time_query(
        &mut db,
        MAILRS_PROD_NO_ORDERED_AGG_SQL,
        10,
        "mailrs_prod_no_ordered_agg_100k",
        10_000.0,
    );
}

#[test]
#[ignore = "P0 mailrs prod-real ablation; --include-ignored to run"]
fn baseline_mailrs_prod_no_subq_100k() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_mailrs_inbox(&mut db, 100_000);
    dump_explain(&mut db, "mailrs_prod_no_subq_100k", MAILRS_PROD_NO_SUBQ_SQL);
    time_query(
        &mut db,
        MAILRS_PROD_NO_SUBQ_SQL,
        10,
        "mailrs_prod_no_subq_100k",
        10_000.0,
    );
}

// v7.37.4 A' ablation panel — finer-grained than the prod_no_* family.
// Each constant drops ONE structural element from MAILRS_PROD_REAL_SQL
// so the per-element cost lands as a clean delta on the same 100k seed.

// prod_real - NOT EXISTS (snoozed_conversations anti-join). The
// pull_up_exists_sublinks rewrite (v7.34.2) should already turn this
// into a hash anti-join, but per-row vs amortised cost matters when
// outer width is 14 aggs.
const MAILRS_PROD_NO_NOT_EXISTS_SQL: &str = "\
SELECT m.thread_id, MAX(m.subject), string_agg(DISTINCT m.sender, ','), \
       COUNT(DISTINCT CASE WHEN m.message_id != '' \
                           THEN m.message_id \
                           ELSE CAST(m.id AS TEXT) END), \
       COUNT(DISTINCT CASE WHEN (m.flags & 1) = 0 \
                           THEN CASE WHEN m.message_id != '' \
                                     THEN m.message_id \
                                     ELSE CAST(m.id AS TEXT) END \
                           END), \
       MAX(m.internal_date), \
       COALESCE((SELECT ea.category FROM email_analysis ea \
                   JOIN messages m2 ON ea.message_id = m2.id \
                  WHERE m2.thread_id = m.thread_id \
                  ORDER BY m2.internal_date DESC LIMIT 1), \
                'general'), \
       BOOL_OR((m.flags & 4) != 0), \
       COALESCE( \
         (SELECT LEFT(ea_snip.summary, 80) FROM email_analysis ea_snip \
            JOIN messages m_snip ON ea_snip.message_id = m_snip.id \
           WHERE m_snip.thread_id = m.thread_id \
             AND ea_snip.summary IS NOT NULL \
             AND ea_snip.summary != '' \
           ORDER BY m_snip.internal_date DESC LIMIT 1), \
         (SELECT LEFT(m3.text_body, 80) FROM messages m3 \
           WHERE m3.thread_id = m.thread_id \
             AND m3.text_body IS NOT NULL \
             AND m3.text_body != '' \
           ORDER BY m3.internal_date DESC LIMIT 1), \
         ''), \
       BOOL_OR(m.pinned), \
       BOOL_OR(m.archived), \
       COALESCE((array_agg(m.importance_level \
                           ORDER BY m.importance_score DESC NULLS LAST))[1], \
                'normal'), \
       COALESCE(MAX(m.importance_score), 0.0), \
       COALESCE(BOOL_OR(ea.requires_action), false), \
       COALESCE((array_agg(m.sender ORDER BY m.internal_date DESC))[1], ''), \
       COUNT(DISTINCT CASE WHEN mb.name = 'Sent' AND m.message_id != '' \
                           THEN m.message_id \
                           WHEN mb.name = 'Sent' \
                           THEN CAST(m.id AS TEXT) END) \
  FROM messages m \
       JOIN mailboxes mb ON m.mailbox_id = mb.id \
       LEFT JOIN email_analysis ea ON ea.message_id = m.id \
 WHERE mb.user_address = 'lihao@golia.jp' \
   AND thread_id != '' \
 GROUP BY m.thread_id \
 HAVING BOOL_OR(m.archived) = false \
    AND BOOL_OR(mb.name != 'Sent') = true \
 ORDER BY BOOL_OR(m.pinned) DESC, MAX(m.internal_date) DESC \
 LIMIT 50";

// prod_real - HAVING. Replace the two bool_or HAVING preds with
// constant TRUE so every group passes; isolates the per-group HAVING
// eval cost from the rest of the post-aggregate path.
const MAILRS_PROD_NO_HAVING_SQL: &str = "\
SELECT m.thread_id, MAX(m.subject), string_agg(DISTINCT m.sender, ','), \
       COUNT(DISTINCT CASE WHEN m.message_id != '' \
                           THEN m.message_id \
                           ELSE CAST(m.id AS TEXT) END), \
       COUNT(DISTINCT CASE WHEN (m.flags & 1) = 0 \
                           THEN CASE WHEN m.message_id != '' \
                                     THEN m.message_id \
                                     ELSE CAST(m.id AS TEXT) END \
                           END), \
       MAX(m.internal_date), \
       COALESCE((SELECT ea.category FROM email_analysis ea \
                   JOIN messages m2 ON ea.message_id = m2.id \
                  WHERE m2.thread_id = m.thread_id \
                  ORDER BY m2.internal_date DESC LIMIT 1), \
                'general'), \
       BOOL_OR((m.flags & 4) != 0), \
       COALESCE( \
         (SELECT LEFT(ea_snip.summary, 80) FROM email_analysis ea_snip \
            JOIN messages m_snip ON ea_snip.message_id = m_snip.id \
           WHERE m_snip.thread_id = m.thread_id \
             AND ea_snip.summary IS NOT NULL \
             AND ea_snip.summary != '' \
           ORDER BY m_snip.internal_date DESC LIMIT 1), \
         (SELECT LEFT(m3.text_body, 80) FROM messages m3 \
           WHERE m3.thread_id = m.thread_id \
             AND m3.text_body IS NOT NULL \
             AND m3.text_body != '' \
           ORDER BY m3.internal_date DESC LIMIT 1), \
         ''), \
       BOOL_OR(m.pinned), \
       BOOL_OR(m.archived), \
       COALESCE((array_agg(m.importance_level \
                           ORDER BY m.importance_score DESC NULLS LAST))[1], \
                'normal'), \
       COALESCE(MAX(m.importance_score), 0.0), \
       COALESCE(BOOL_OR(ea.requires_action), false), \
       COALESCE((array_agg(m.sender ORDER BY m.internal_date DESC))[1], ''), \
       COUNT(DISTINCT CASE WHEN mb.name = 'Sent' AND m.message_id != '' \
                           THEN m.message_id \
                           WHEN mb.name = 'Sent' \
                           THEN CAST(m.id AS TEXT) END) \
  FROM messages m \
       JOIN mailboxes mb ON m.mailbox_id = mb.id \
       LEFT JOIN email_analysis ea ON ea.message_id = m.id \
 WHERE mb.user_address = 'lihao@golia.jp' \
   AND thread_id != '' \
   AND NOT EXISTS (SELECT 1 FROM snoozed_conversations sc \
                    WHERE sc.thread_id = m.thread_id \
                      AND sc.account_address = mb.user_address \
                      AND sc.snoozed_until > 0) \
 GROUP BY m.thread_id \
 ORDER BY BOOL_OR(m.pinned) DESC, MAX(m.internal_date) DESC \
 LIMIT 50";

// prod_real - DISTINCT family. Replace the 4 COUNT(DISTINCT CASE …)
// + string_agg(DISTINCT …) with their non-distinct counterparts.
// Confirms (or refutes) the 95 ms DISTINCT estimate from the
// minimal → minimal+DISTINCT gap.
const MAILRS_PROD_NO_DISTINCT_SQL: &str = "\
SELECT m.thread_id, MAX(m.subject), string_agg(m.sender, ','), \
       COUNT(CASE WHEN m.message_id != '' \
                  THEN m.message_id \
                  ELSE CAST(m.id AS TEXT) END), \
       COUNT(CASE WHEN (m.flags & 1) = 0 \
                  THEN CASE WHEN m.message_id != '' \
                            THEN m.message_id \
                            ELSE CAST(m.id AS TEXT) END \
                  END), \
       MAX(m.internal_date), \
       COALESCE((SELECT ea.category FROM email_analysis ea \
                   JOIN messages m2 ON ea.message_id = m2.id \
                  WHERE m2.thread_id = m.thread_id \
                  ORDER BY m2.internal_date DESC LIMIT 1), \
                'general'), \
       BOOL_OR((m.flags & 4) != 0), \
       COALESCE( \
         (SELECT LEFT(ea_snip.summary, 80) FROM email_analysis ea_snip \
            JOIN messages m_snip ON ea_snip.message_id = m_snip.id \
           WHERE m_snip.thread_id = m.thread_id \
             AND ea_snip.summary IS NOT NULL \
             AND ea_snip.summary != '' \
           ORDER BY m_snip.internal_date DESC LIMIT 1), \
         (SELECT LEFT(m3.text_body, 80) FROM messages m3 \
           WHERE m3.thread_id = m.thread_id \
             AND m3.text_body IS NOT NULL \
             AND m3.text_body != '' \
           ORDER BY m3.internal_date DESC LIMIT 1), \
         ''), \
       BOOL_OR(m.pinned), \
       BOOL_OR(m.archived), \
       COALESCE((array_agg(m.importance_level \
                           ORDER BY m.importance_score DESC NULLS LAST))[1], \
                'normal'), \
       COALESCE(MAX(m.importance_score), 0.0), \
       COALESCE(BOOL_OR(ea.requires_action), false), \
       COALESCE((array_agg(m.sender ORDER BY m.internal_date DESC))[1], ''), \
       COUNT(CASE WHEN mb.name = 'Sent' AND m.message_id != '' \
                  THEN m.message_id \
                  WHEN mb.name = 'Sent' \
                  THEN CAST(m.id AS TEXT) END) \
  FROM messages m \
       JOIN mailboxes mb ON m.mailbox_id = mb.id \
       LEFT JOIN email_analysis ea ON ea.message_id = m.id \
 WHERE mb.user_address = 'lihao@golia.jp' \
   AND thread_id != '' \
   AND NOT EXISTS (SELECT 1 FROM snoozed_conversations sc \
                    WHERE sc.thread_id = m.thread_id \
                      AND sc.account_address = mb.user_address \
                      AND sc.snoozed_until > 0) \
 GROUP BY m.thread_id \
 HAVING BOOL_OR(m.archived) = false \
    AND BOOL_OR(mb.name != 'Sent') = true \
 ORDER BY BOOL_OR(m.pinned) DESC, MAX(m.internal_date) DESC \
 LIMIT 50";

#[test]
#[ignore = "P0 mailrs A' ablation; --include-ignored to run"]
fn baseline_mailrs_prod_no_not_exists_100k() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_mailrs_inbox(&mut db, 100_000);
    time_query(
        &mut db,
        MAILRS_PROD_NO_NOT_EXISTS_SQL,
        10,
        "mailrs_prod_no_not_exists_100k",
        20_000.0,
    );
}

#[test]
#[ignore = "P0 mailrs A' ablation; --include-ignored to run"]
fn baseline_mailrs_prod_no_having_100k() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_mailrs_inbox(&mut db, 100_000);
    time_query(
        &mut db,
        MAILRS_PROD_NO_HAVING_SQL,
        10,
        "mailrs_prod_no_having_100k",
        20_000.0,
    );
}

#[test]
#[ignore = "P0 mailrs A' ablation; --include-ignored to run"]
fn baseline_mailrs_prod_no_distinct_100k() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_mailrs_inbox(&mut db, 100_000);
    time_query(
        &mut db,
        MAILRS_PROD_NO_DISTINCT_SQL,
        10,
        "mailrs_prod_no_distinct_100k",
        20_000.0,
    );
}

#[test]
#[ignore = "P0 mailrs prod-real reproducer; --include-ignored to run"]
fn baseline_mailrs_prod_real_300k() {
    let _g = crate::perf_lock();
    let mut db = Engine::new();
    seed_mailrs_inbox(&mut db, 300_000);
    dump_explain(&mut db, "mailrs_prod_real_300k", MAILRS_PROD_REAL_SQL);
    time_query(
        &mut db,
        MAILRS_PROD_REAL_SQL,
        10,
        "mailrs_prod_real_300k",
        120_000.0,
    );
}
