//! v6.5.6 — slow-query log + plan-cache cap env knobs.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use spg_engine::Engine;

// Deterministic clock + capture of the last slow-query event for
// the test thread.
static CLOCK: AtomicI64 = AtomicI64::new(0);
fn clock() -> i64 {
    CLOCK.fetch_add(50_000, Ordering::SeqCst) // 50 ms per call
}

static SLOW_CALL_COUNT: AtomicU64 = AtomicU64::new(0);
fn slow_logger(_sql: &str, _elapsed_us: u64) {
    SLOW_CALL_COUNT.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn slow_query_callback_respects_threshold() {
    // Single combined test to avoid global-state interference under
    // parallel test execution (CLOCK + SLOW_CALL_COUNT are statics).
    CLOCK.store(0, Ordering::SeqCst);
    SLOW_CALL_COUNT.store(0, Ordering::SeqCst);

    // Above threshold: each execute clock-jump = 50ms, threshold
    // 30ms → callback fires.
    let mut eng = Engine::new()
        .with_clock(clock)
        .with_slow_query_log(30_000, slow_logger);
    eng.execute("CREATE TABLE t (id INT)").unwrap();
    eng.execute("INSERT INTO t VALUES (1)").unwrap();
    eng.execute("INSERT INTO t VALUES (2)").unwrap();
    let above = SLOW_CALL_COUNT.load(Ordering::SeqCst);
    assert!(above >= 3, "expected ≥3 fires, got {above}");

    // Below threshold: rebuild engine with a 100ms floor.
    SLOW_CALL_COUNT.store(0, Ordering::SeqCst);
    let mut eng2 = Engine::new()
        .with_clock(clock)
        .with_slow_query_log(100_000, slow_logger);
    eng2.execute("CREATE TABLE u (id INT)").unwrap();
    eng2.execute("INSERT INTO u VALUES (1)").unwrap();
    assert_eq!(SLOW_CALL_COUNT.load(Ordering::SeqCst), 0);
}

#[test]
fn plan_cache_cap_overridable_via_engine_api() {
    let mut eng = Engine::new();
    eng.set_plan_cache_max(8);
    // Fill past the new cap; cache should retain only 8.
    for i in 0..32 {
        eng.execute(&format!("CREATE TABLE t{i} (id INT)")).unwrap();
        eng.execute(&format!("INSERT INTO t{i} VALUES (1)")).unwrap();
    }
    // The plan cache is now bounded at 8. Verify via the introspect
    // accessor.
    assert!(
        eng.plan_cache().len() <= 8,
        "plan cache cap not respected, len = {}",
        eng.plan_cache().len()
    );
}
