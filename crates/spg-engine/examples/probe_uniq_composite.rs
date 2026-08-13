//! r1018 — how many locators does the uniqueness probe walk when the
//! UNIQUE key is COMPOSITE and its leading column is low-cardinality?
//!
//! mailrs (2026-08-13) reported a 98 MB dump that PostgreSQL 18 loads in
//! 10.9 s and `spg import` had not finished after 40 minutes. Their
//! hypothesis was the insert-time `to_tsvector` + GIN index their schema
//! carries. Ablation says otherwise: with the trigger AND the GIN index
//! removed the import is 85 % as slow, and dropping the two composite
//! UNIQUE constraints takes the same load from 14.77 s to 1.87 s.
//!
//! The measured shape is cost per row proportional to rows already in the
//! table. `probe_key_conflict` descends the btree on the LEADING column
//! only and then folds the full key for every locator it finds, so a
//! constraint like `UNIQUE(mailbox_id, uid)` on a single-mailbox table
//! walks the whole table per inserted row — the O(n²) the v7.39 probe was
//! introduced to remove, reappearing wherever the leading column does not
//! discriminate.
//!
//! This counts it, next to a control whose leading column is unique. Both
//! tables carry the same constraint shape and the same row count; only the
//! leading column's cardinality differs.
//!
//!   cargo run --release --features perf-counters --example probe_uniq_composite

use spg_engine::Engine;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

const BATCH: i64 = 500;
const BATCHES: i64 = 20;

/// One 500-tuple INSERT, the shape a dump emits. `lead_divisor` sets how
/// many distinct leading values the batch spreads over: 0 means "one
/// value for the whole table" (mailrs's single mailbox), 1 means "a
/// distinct value per row" (the control).
fn batch(table: &str, lo: i64, hi: i64, per_row_lead: bool) -> String {
    let mut s = String::with_capacity((hi - lo) as usize * 24 + 32);
    s.push_str("INSERT INTO ");
    s.push_str(table);
    s.push_str(" VALUES ");
    for (k, id) in (lo..hi).enumerate() {
        if k > 0 {
            s.push(',');
        }
        let lead = if per_row_lead { id } else { 1 };
        s.push_str(&format!("({id},{lead},{id})"));
    }
    s
}

fn run(label: &str, table: &str, per_row_lead: bool) {
    let mut e = Engine::new();
    e.set_autovacuum(false);
    e.execute(&format!(
        "CREATE TABLE {table}(id BIGINT PRIMARY KEY, lead_col BIGINT, uid BIGINT, \
         UNIQUE(lead_col, uid))"
    ))
    .unwrap();

    println!("\n## {label}");
    println!("| rows before | ms | probes | locators | locators/probe |");
    println!("|------------:|---:|-------:|---------:|---------------:|");
    for b in 0..BATCHES {
        let (lo, hi) = (b * BATCH + 1, (b + 1) * BATCH + 1);
        let sql = batch(table, lo, hi, per_row_lead);
        let base = (
            spg_engine::UNIQ_PROBE_CALLS.load(Relaxed),
            spg_engine::UNIQ_PROBE_LOCATORS.load(Relaxed),
        );
        let t0 = Instant::now();
        e.execute(&sql).unwrap();
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let calls = spg_engine::UNIQ_PROBE_CALLS.load(Relaxed) - base.0;
        let locs = spg_engine::UNIQ_PROBE_LOCATORS.load(Relaxed) - base.1;
        if b % 4 == 0 || b == BATCHES - 1 {
            let per = if calls == 0 {
                0.0
            } else {
                locs as f64 / calls as f64
            };
            println!("| {} | {ms:.1} | {calls} | {locs} | {per:.1} |", b * BATCH);
        }
    }
}

fn main() {
    // The reported shape: every row shares one leading value.
    run(
        "UNIQUE(lead_col, uid), ONE distinct leading value (mailrs)",
        "low_card",
        false,
    );
    // Control: same constraint, same rows, leading value distinct per row.
    run(
        "UNIQUE(lead_col, uid), leading value distinct per row (control)",
        "high_card",
        true,
    );
}
