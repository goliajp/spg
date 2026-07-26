//! v7.39 (round 492) — how many locators does the uniqueness probe walk
//! on a churned table?
//!
//! The round-491 profile of the panel's `delete_reinsert_1k` put
//! `probe_key_conflict` at 8.4 % of the server's connection thread. A
//! BTree index holds one locator per row VERSION, and that shape deletes
//! and re-inserts the same ids over and over — so the suspicion is that
//! each PK probe walks every dead version under its key, two trie
//! descents and a `Vec` allocation apiece.
//!
//! Round 490 fixed exactly that shape of defect on the SEEK side, so this
//! counts before anything moves.
//!
//!   cargo run --release --features perf-counters --example probe_uniq_churn

use spg_engine::Engine;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

const TOTAL: i64 = 50_000;

fn batch(lo: i64, hi: i64) -> String {
    let mut s = String::with_capacity((hi - lo) as usize * 24 + 32);
    s.push_str("INSERT INTO wb VALUES ");
    for (k, id) in (lo..hi).enumerate() {
        if k > 0 {
            s.push(',');
        }
        s.push_str(&format!("({id},{},{})", id % 100, id * 7 % 100_000));
    }
    s
}

fn main() {
    let no_av = std::env::var("SPG_NO_AV").is_ok_and(|v| v != "0");
    let mut e = Engine::new();
    if no_av {
        e.set_autovacuum(false);
    }
    println!("# autovacuum = {}", !no_av);
    e.execute("CREATE TABLE wb(id INT PRIMARY KEY, g INT, v INT)")
        .unwrap();
    for chunk in 0..(TOTAL / 1000) {
        e.execute(&batch(chunk * 1000 + 1, chunk * 1000 + 1001)).unwrap();
    }
    println!("| cycle | reinsert ms | probes | locators | per probe |");
    println!("|------:|------------:|-------:|---------:|----------:|");
    for cycle in 0..=60 {
        let (lo, hi) = (10_001, 11_001);
        e.execute(&format!("DELETE FROM wb WHERE id >= {lo} AND id < {hi}"))
            .unwrap();
        let sql = batch(lo, hi);
        let base = (
            spg_engine::UNIQ_PROBE_CALLS.load(Relaxed),
            spg_engine::UNIQ_PROBE_LOCATORS.load(Relaxed),
        );
        let t0 = Instant::now();
        e.execute(&sql).unwrap();
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let calls = spg_engine::UNIQ_PROBE_CALLS.load(Relaxed) - base.0;
        let locs = spg_engine::UNIQ_PROBE_LOCATORS.load(Relaxed) - base.1;
        if cycle % 10 == 0 {
            let per = if calls == 0 {
                0.0
            } else {
                locs as f64 / calls as f64
            };
            println!("| {cycle} | {ms:.3} | {calls} | {locs} | {per:.1} |");
        }
    }
}
