//! v7.37.5-A2b — samply-targeted micro-bench for content_worker prod SQL.
//!
//! Opens the prod snapshot at /tmp/spg-prod-mailrs/mailrs.spg and runs the
//! content_worker SQL 100 times in-process via spg-embedded `Database`.
//! No wire overhead — engine direct.
//!
//! Run via:
//!     CARGO_PROFILE_RELEASE_DEBUG=true \
//!         cargo build --release -p spg-bench-competitor --bin profile_content_worker
//!     samply record --rate 5000 \
//!         ./target/release/profile_content_worker
//!
//! Outputs avg-ms to stderr after 100 measured iterations (10 warmup).

#![allow(clippy::cast_precision_loss)]

use std::time::Instant;

use spg_embedded::Database;

const SQL: &str = "SELECT m.id FROM messages m \
                    WHERE NOT EXISTS (SELECT 1 FROM attachment_content ac \
                                       WHERE ac.message_id = m.id) \
                    ORDER BY m.id DESC LIMIT 64";

fn main() {
    let path = std::env::var("SPG_PROD_SNAPSHOT")
        .unwrap_or_else(|_| "/tmp/spg-prod-mailrs/mailrs.spg".to_string());
    eprintln!("opening {path}");
    let mut db = Database::open_path(&path).expect("open snapshot");

    // Warmup 10 — plan-cache, InListSet, allocator stabilise.
    for i in 0..10 {
        let r = db.execute(SQL).expect("warmup ok");
        if i == 0 {
            let n = match &r {
                spg_engine::QueryResult::Rows { rows, .. } => rows.len(),
                _ => 0,
            };
            eprintln!("first warmup ok; rows={n}");
        }
    }

    let iters: u32 = 100;
    let start = Instant::now();
    for _ in 0..iters {
        let r = db.execute(std::hint::black_box(SQL)).expect("measure ok");
        std::hint::black_box(r);
    }
    let elapsed = start.elapsed();
    let avg_ms = elapsed.as_micros() as f64 / f64::from(iters) / 1000.0;
    eprintln!("avg = {avg_ms:.3} ms over {iters} iters");
}
