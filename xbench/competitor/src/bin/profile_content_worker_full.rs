//! v7.37.6 — samply-targeted bench for content_worker prod SQL — FULL
//! variant (JOIN + WHERE), matching the dogfood-replay fixture.
//!
//! A2b's previous `profile_content_worker.rs` used a TRUNCATED SQL
//! (no `JOIN mailboxes` + no `WHERE m.size>0`), so its samply output
//! routed through `try_pk_walk_top_n` and reported 4ms — which doesn't
//! match the prod-shape hot path (88-94ms warm). This binary runs the
//! EXACT fixture SQL from
//! `xtests/dogfood_replay/fixtures/mailrs-2026-06-22-content-worker/
//!  queries.sql` so samply samples the real hot path.
//!
//! Run:
//!     CARGO_PROFILE_RELEASE_DEBUG=true \
//!         cargo build --release -p spg-bench-competitor \
//!         --bin profile_content_worker_full
//!     samply record --rate 5000 \
//!         ./target/release/profile_content_worker_full

#![allow(clippy::cast_precision_loss)]

use std::time::Instant;

use spg_embedded::Database;

const SQL: &str = "\
SELECT m.id, m.sender, m.maildir_id, mb.user_address
  FROM messages m
  JOIN mailboxes mb ON m.mailbox_id = mb.id
 WHERE m.size > 0
   AND NOT EXISTS (
       SELECT 1 FROM attachment_content ac WHERE ac.message_id = m.id
   )
 ORDER BY m.id DESC
 LIMIT 64";

fn main() {
    let path = std::env::var("SPG_PROD_SNAPSHOT")
        .unwrap_or_else(|_| "/tmp/spg-prod-mailrs/mailrs.spg".to_string());
    eprintln!("opening {path}");
    let mut db = Database::open_path(&path).expect("open snapshot");

    // Cold first to surface plan-build cost.
    let cold_start = Instant::now();
    let r = db.execute(SQL).expect("cold ok");
    let cold_ms = cold_start.elapsed().as_micros() as f64 / 1000.0;
    let n = match &r {
        spg_engine::QueryResult::Rows { rows, .. } => rows.len(),
        _ => 0,
    };
    eprintln!("cold = {cold_ms:.3} ms (rows = {n})");

    // Warmup 10 — plan-cache, InListSet, allocator stabilise.
    for _ in 0..10 {
        let _ = db.execute(SQL).expect("warmup ok");
    }

    let iters: u32 = 100;
    let start = Instant::now();
    for _ in 0..iters {
        let r = db.execute(std::hint::black_box(SQL)).expect("measure ok");
        std::hint::black_box(r);
    }
    let elapsed = start.elapsed();
    let avg_ms = elapsed.as_micros() as f64 / f64::from(iters) / 1000.0;
    eprintln!("warm avg = {avg_ms:.3} ms over {iters} iters");
}
