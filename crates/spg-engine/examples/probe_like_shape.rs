//! v7.39 (round 488) — which LIKE spellings reach the fast predicate?
//!
//! The round-488 pins cross-check the fast path against the VM, which is
//! only worth anything if the "general path" spelling really does take
//! the general path. Round 480 was spent acting on an inference about a
//! branch that turned out never to run, so this asks the counter.
//!
//!   cargo run --release --features perf-counters --example probe_like_shape

use spg_engine::Engine;
use std::sync::atomic::Ordering::Relaxed;

fn main() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT, s TEXT, c CHAR(6), n INT)")
        .unwrap();
    for i in 0..20 {
        eng.execute(&format!(
            "INSERT INTO t VALUES ({i}, 'user_{i:04}', 'ab', {i})"
        ))
        .unwrap();
    }
    eng.execute("INSERT INTO t VALUES (99, NULL, NULL, NULL)")
        .unwrap();
    println!("| spelling | fastpred fires | ids |");
    println!("|----------|---------------:|-----|");
    for sql in [
        "SELECT count(*) FROM t WHERE s LIKE '%_05%'",
        "SELECT count(*) FROM t WHERE s LIKE 'user%'",
        "SELECT count(*) FROM t WHERE s NOT LIKE '%05%'",
        "SELECT count(*) FROM t WHERE s ILIKE '%USER%'",
        "SELECT count(*) FROM t WHERE c LIKE 'ab%'",
        "SELECT count(*) FROM t WHERE (s LIKE '%05%') = true",
        "SELECT count(*) FROM t WHERE n LIKE '%5%'",
        "SELECT id FROM t WHERE s NOT LIKE '%' ORDER BY id",
        "SELECT id FROM t WHERE (s NOT LIKE '%') = true ORDER BY id",
        "SELECT id FROM t WHERE s LIKE '%' AND id = 99 ORDER BY id",
    ] {
        let base = spg_engine::eval::compiled::STEP_VM_FASTPRED_FIRE.load(Relaxed);
        let out = eng.execute(sql);
        let fired = spg_engine::eval::compiled::STEP_VM_FASTPRED_FIRE.load(Relaxed) - base;
        let shown = match out {
            Ok(spg_engine::QueryResult::Rows { rows, .. }) => rows
                .iter()
                .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
                .collect::<Vec<_>>()
                .join(";"),
            Ok(other) => format!("{other:?}"),
            Err(e) => format!("ERR {e:?}"),
        };
        println!("| {sql} | {fired} | {shown} |");
    }
}
