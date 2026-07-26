//! v7.39 (round 481) — how many values does the Step VM leave on the stack?
//!
//! Round 480 left `drop_glue<Value>` at 16 % of self time with the drops
//! attributed to the predicate closure — the stack, not the returned value
//! (round 479 removed that one). Whether the ops leave operands behind for
//! the next call's `clear()` to drop is a question with a number.
//!
//! Build with the counters on:
//!   cargo run --release --features perf-counters --example probe_step_leftover

use spg_engine::Engine;
use std::sync::atomic::Ordering::Relaxed;

fn main() {
    let n: i64 = 50_000;
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE hi (id INT NOT NULL, g INT NOT NULL)")
        .expect("create");
    for i in 1..=n {
        eng.execute(&format!("INSERT INTO hi VALUES ({i}, {})", i % 1000))
            .expect("seed");
    }
    for (label, sql) in [
        ("int_filter", "SELECT count(*) FROM hi WHERE g = 5"),
        ("scan_only", "SELECT count(*) FROM hi"),
    ] {
        use spg_engine::eval::compiled as c;
        let base = (
            c::STEP_VM_CALL_COUNT.load(Relaxed),
            c::STEP_VM_STEPS_TOTAL.load(Relaxed),
            c::STEP_VM_STACK_LEFTOVER.load(Relaxed),
            c::STEP_VM_STACK_LEFTOVER_HEAP.load(Relaxed),
        );
        let _ = eng.execute(sql).expect("q");
        let now = (
            c::STEP_VM_CALL_COUNT.load(Relaxed),
            c::STEP_VM_STEPS_TOTAL.load(Relaxed),
            c::STEP_VM_STACK_LEFTOVER.load(Relaxed),
            c::STEP_VM_STACK_LEFTOVER_HEAP.load(Relaxed),
        );
        let calls = now.0 - base.0;
        println!(
            "{label:<12} vm_calls={calls:<8} steps={:<8} leftover={:<8} leftover_heap={:<6} \
             steps/call={:.2} leftover/call={:.2}",
            now.1 - base.1,
            now.2 - base.2,
            now.3 - base.3,
            if calls == 0 {
                0.0
            } else {
                (now.1 - base.1) as f64 / calls as f64
            },
            if calls == 0 {
                0.0
            } else {
                (now.2 - base.2) as f64 / calls as f64
            },
        );
    }
}
