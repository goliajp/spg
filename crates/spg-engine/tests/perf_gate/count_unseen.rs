//! Empirical root-cause probe for the mailrs P0 `count_unseen`
//! (`spg-conn-pool-exhaustion-slow-queries-2026-06-16.md`): 24k rows
//! 2.4s vs PG 0.25s. Ablation isolates which segment dominates —
//! (B) the two correlated `NOT EXISTS` anti-joins vs (A) the HAVING
//! latest-per-group correlated scalar subquery. `#[ignore]` (full
//! tier, exploratory; not a budget gate). Run:
//!   cargo test --release -p spg-engine --test perf_gate \
//!     -- --ignored --nocapture count_unseen
//!
//! Faithful to the prod shape except `snoozed_until > NOW()` is
//! modelled as `> 1700000000` (a fixed epoch) and the `$1` bind is
//! inlined to 'u@x' — neither changes the decorrelation cost the
//! probe measures.

use std::time::Instant;

use spg_engine::Engine;

const N: usize = 24_000;
const PER: usize = 5; // messages per thread → ~4800 threads

fn build() -> Engine {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE mailboxes (id BIGSERIAL PRIMARY KEY, name TEXT, user_address TEXT)")
        .unwrap();
    eng.execute(
        "CREATE TABLE messages (id BIGSERIAL PRIMARY KEY, mailbox_id BIGINT, thread_id TEXT, \
         subject TEXT, sender TEXT, internal_date BIGINT, flags BIGINT, archived BOOLEAN)",
    )
    .unwrap();
    eng.execute("CREATE TABLE email_analysis (message_id BIGINT PRIMARY KEY, category TEXT)")
        .unwrap();
    eng.execute(
        "CREATE TABLE snoozed_conversations (thread_id TEXT, account_address TEXT, snoozed_until BIGINT)",
    )
    .unwrap();
    eng.execute("CREATE INDEX idx_thread ON messages(thread_id)")
        .unwrap();
    eng.execute("INSERT INTO mailboxes (name, user_address) VALUES ('inbox', 'u@x')")
        .unwrap();

    for b in 0..(N / 500) {
        let mut rows = Vec::with_capacity(500);
        for j in 0..500 {
            let i = b * 500 + j;
            let thread = i / PER;
            let flags = i % 8;
            let archived = if i % 200 == 0 { "true" } else { "false" };
            rows.push(format!(
                "(1, 'th-{thread}', 'subject {i}', 's{}@x', {}, {flags}, {archived})",
                i % 100,
                1_700_000_000 + i
            ));
        }
        eng.execute(&format!(
            "INSERT INTO messages (mailbox_id, thread_id, subject, sender, internal_date, flags, archived) VALUES {}",
            rows.join(",")
        ))
        .unwrap();
    }
    for b in 0..(N / 500) {
        let mut rows = Vec::with_capacity(500);
        for j in 0..500 {
            let i = b * 500 + j;
            let cat = match i % 37 {
                0 => "spam",
                1 => "scam",
                _ => "general",
            };
            rows.push(format!("({}, '{cat}')", i + 1));
        }
        eng.execute(&format!(
            "INSERT INTO email_analysis (message_id, category) VALUES {}",
            rows.join(",")
        ))
        .unwrap();
    }
    // ~300 snoozed threads, snoozed into the future.
    let mut sn = Vec::new();
    for t in 0..300 {
        sn.push(format!("('th-{t}', 'u@x', 1800000000)"));
    }
    eng.execute(&format!(
        "INSERT INTO snoozed_conversations (thread_id, account_address, snoozed_until) VALUES {}",
        sn.join(",")
    ))
    .unwrap();
    eng
}

fn timed(eng: &mut Engine, label: &str, sql: &str, iters: u32) {
    // warmup + row count
    let n = match eng.execute(sql).unwrap() {
        spg_engine::QueryResult::Rows { rows, .. } => rows.len(),
        _ => 0,
    };
    let t = Instant::now();
    for _ in 0..iters {
        std::hint::black_box(eng.execute(std::hint::black_box(sql)).unwrap());
    }
    eprintln!(
        "{label:32} {:>10.2?}/run   ({n} threads)",
        t.elapsed() / iters
    );
}

#[test]
#[ignore = "full-tier exploratory perf probe; run with --ignored --nocapture"]
fn count_unseen_ablation() {
    let _lock = crate::perf_lock();
    let mut eng = build();

    // Inner query of count_unseen (the outer COUNT(*) over the CTE is
    // trivial — all cost is here). Four variants isolate B vs A.
    let base_from = "FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
                     WHERE mb.user_address = 'u@x' AND m.thread_id != ''";
    let anti = " AND NOT EXISTS (SELECT 1 FROM snoozed_conversations sc \
                  WHERE sc.thread_id = m.thread_id AND sc.account_address = mb.user_address \
                  AND sc.snoozed_until > 1700000000) \
                AND NOT EXISTS (SELECT 1 FROM email_analysis ea \
                  WHERE ea.message_id = m.id AND ea.category IN ('spam','scam'))";
    let having_base = " GROUP BY m.thread_id \
                        HAVING BOOL_OR(m.archived) = false \
                        AND COUNT(CASE WHEN (m.flags & 1) = 0 THEN 1 END) > 0";
    let having_subq = " AND LOWER(COALESCE((SELECT m_last.sender FROM messages m_last \
                         WHERE m_last.thread_id = m.thread_id \
                         ORDER BY m_last.internal_date DESC LIMIT 1),'')) \
                        NOT LIKE '%' || LOWER('u@x') || '%'";

    let v0 = format!("SELECT m.thread_id {base_from}{having_base}");
    let v1_b = format!("SELECT m.thread_id {base_from}{anti}{having_base}");
    let v2_a = format!("SELECT m.thread_id {base_from}{having_base}{having_subq}");
    let vfull = format!("SELECT m.thread_id {base_from}{anti}{having_base}{having_subq}");

    eprintln!(
        "-- count_unseen ablation @ {N} msgs / ~{} threads --",
        N / PER
    );
    timed(&mut eng, "V0 base (group+having, no subq)", &v0, 3);
    timed(&mut eng, "V1 +2x NOT EXISTS  (isolate B)", &v1_b, 3);
    timed(&mut eng, "V2 +HAVING m_last  (isolate A)", &v2_a, 3);
    timed(&mut eng, "Vfull (complete count_unseen)", &vfull, 3);
    eprintln!("-- B = V1-V0, A = V2-V0; dominant segment = the larger --");
}
