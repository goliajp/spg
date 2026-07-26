//! v7.39 (round 486) — which `IN` spellings actually reach the fast
//! predicate, and which fall to the VM?
//!
//! The round-486 pins cross-check the two paths against each other, which
//! is only worth anything if the "general path" spelling really does take
//! the general path. Round 480 was spent acting on an inference about a
//! branch that turned out never to run, so this asks the counter instead.
//!
//!   cargo run --release --features perf-counters --example probe_in_set_shape

use spg_engine::Engine;
use std::sync::atomic::Ordering::Relaxed;

fn main() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (g INT, s TEXT)").unwrap();
    for i in 0..20 {
        eng.execute(&format!("INSERT INTO t VALUES ({i}, 'v{i}')"))
            .unwrap();
    }
    println!("| spelling | fastpred fires |");
    println!("|----------|---------------:|");
    for sql in [
        "SELECT count(*) FROM t WHERE g IN (1,3,5)",
        "SELECT count(*) FROM t WHERE g NOT IN (1,3,5)",
        "SELECT count(*) FROM t WHERE (g IN (1,3,5)) = true",
        "SELECT count(*) FROM t WHERE NOT (g NOT IN (1,3,5))",
        "SELECT count(*) FROM t WHERE s IN ('v1','v3')",
        "SELECT count(*) FROM t WHERE s IN (1,3)",
    ] {
        let base = spg_engine::eval::compiled::STEP_VM_FASTPRED_FIRE.load(Relaxed);
        let _ = eng.execute(sql);
        let fired = spg_engine::eval::compiled::STEP_VM_FASTPRED_FIRE.load(Relaxed) - base;
        println!("| {sql} | {fired} |");
    }
}
