//! v7.37.6 — samply-targeted bench for Track A /api/conversations FULL SQL.
//!
//! Same as profile_track_a.rs (Track A's SQL was already full prod SQL),
//! just renamed for naming consistency with `profile_content_worker_full`.
//!
//! Run:
//!     CARGO_PROFILE_RELEASE_DEBUG=true \
//!         cargo build --release -p spg-bench-competitor \
//!         --bin profile_track_a_full
//!     samply record --rate 5000 ./target/release/profile_track_a_full

#![allow(clippy::cast_precision_loss)]

use std::time::Instant;

use spg_embedded::Database;

const SQL: &str = "\
SELECT m.thread_id, MAX(m.subject), string_agg(DISTINCT m.sender, ','),
       COUNT(DISTINCT CASE WHEN m.message_id != ''
                           THEN m.message_id
                           ELSE CAST(m.id AS TEXT) END),
       COUNT(DISTINCT CASE WHEN (m.flags & 1) = 0
                           THEN CASE WHEN m.message_id != ''
                                     THEN m.message_id
                                     ELSE CAST(m.id AS TEXT) END
                           END),
       MAX(m.internal_date),
       COALESCE((SELECT ea.category FROM email_analysis ea
                   JOIN messages m2 ON ea.message_id = m2.id
                  WHERE m2.thread_id = m.thread_id
                  ORDER BY m2.internal_date DESC LIMIT 1),
                'general'),
       BOOL_OR((m.flags & 4) != 0),
       COALESCE(
         (SELECT LEFT(ea_snip.summary, 80) FROM email_analysis ea_snip
            JOIN messages m_snip ON ea_snip.message_id = m_snip.id
           WHERE m_snip.thread_id = m.thread_id
             AND ea_snip.summary IS NOT NULL
             AND ea_snip.summary != ''
           ORDER BY m_snip.internal_date DESC LIMIT 1),
         (SELECT LEFT(m3.text_body, 80) FROM messages m3
           WHERE m3.thread_id = m.thread_id
             AND m3.text_body IS NOT NULL
             AND m3.text_body != ''
           ORDER BY m3.internal_date DESC LIMIT 1),
         ''),
       BOOL_OR(m.pinned),
       BOOL_OR(m.archived),
       COALESCE((array_agg(m.importance_level
                           ORDER BY m.importance_score DESC NULLS LAST))[1],
                'normal'),
       COALESCE(MAX(m.importance_score), 0.0),
       COALESCE(BOOL_OR(ea.requires_action), false),
       COALESCE((array_agg(m.sender ORDER BY m.internal_date DESC))[1], ''),
       COUNT(DISTINCT CASE WHEN mb.name = 'Sent' AND m.message_id != ''
                           THEN m.message_id
                           WHEN mb.name = 'Sent'
                           THEN CAST(m.id AS TEXT) END)
  FROM messages m
       JOIN mailboxes mb ON m.mailbox_id = mb.id
       LEFT JOIN email_analysis ea ON ea.message_id = m.id
 WHERE mb.user_address = 'lihao@golia.jp'
   AND thread_id != ''
   AND NOT EXISTS (SELECT 1 FROM snoozed_conversations sc
                    WHERE sc.thread_id = m.thread_id
                      AND sc.account_address = mb.user_address
                      AND sc.snoozed_until > NOW())
 GROUP BY m.thread_id
 HAVING BOOL_OR(m.archived) = false
    AND BOOL_OR(mb.name != 'Sent') = true
 ORDER BY BOOL_OR(m.pinned) DESC, MAX(m.internal_date) DESC
 LIMIT 50";

fn main() {
    let path = std::env::var("SPG_PROD_SNAPSHOT")
        .unwrap_or_else(|_| "/tmp/spg-prod-mailrs/mailrs.spg".to_string());
    eprintln!("opening {path}");
    let mut db = Database::open_path(&path).expect("open snapshot");

    let cold_start = Instant::now();
    let r = db.execute(SQL).expect("cold ok");
    let cold_ms = cold_start.elapsed().as_micros() as f64 / 1000.0;
    let n = match &r {
        spg_engine::QueryResult::Rows { rows, .. } => rows.len(),
        _ => 0,
    };
    eprintln!("cold = {cold_ms:.3} ms (rows = {n})");

    // 5 warmup after cold so caches settle.
    for _ in 0..5 {
        let _ = db.execute(SQL).expect("warmup ok");
    }

    let iters: u32 = 30;
    let start = Instant::now();
    for _ in 0..iters {
        let r = db.execute(std::hint::black_box(SQL)).expect("measure ok");
        std::hint::black_box(r);
    }
    let elapsed = start.elapsed();
    let avg_ms = elapsed.as_micros() as f64 / f64::from(iters) / 1000.0;
    eprintln!("warm avg = {avg_ms:.3} ms over {iters} iters");
}
