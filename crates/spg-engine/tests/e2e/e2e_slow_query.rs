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

/// 9.0.4 — `SET log_min_duration_statement` is the documented way to
/// turn slow-query logging on, and it did nothing.
///
/// The floor was read from the environment at boot and nowhere else.
/// The GUC was accepted, `SHOW` reflected it, and no statement was ever
/// logged — measured against PostgreSQL 18.6 through
/// `xtests/gates/g5-observe.sh`, which logs the next statement.
///
/// A server that starts with the log OFF used to clear the callback as
/// well as the floor, so there was nothing for a session to turn on.
/// That is the shape the shipped images run in: their default is PG's
/// `-1`.
///
/// This pins the engine's half. The wire's half — a STREAMED SELECT
/// never reaching this check at all — is pinned by that harness, which
/// drives a real server; an in-process engine cannot see it.
// Its own clock and its own counter: the pair above is process-global
// and the test that uses it runs beside this one.
static CLOCK_GUC: AtomicI64 = AtomicI64::new(0);
fn clock_guc() -> i64 {
    CLOCK_GUC.fetch_add(50_000, Ordering::SeqCst)
}
static GUC_CALL_COUNT: AtomicU64 = AtomicU64::new(0);
fn guc_logger(_sql: &str, _elapsed_us: u64) {
    GUC_CALL_COUNT.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn a_session_can_turn_the_slow_query_log_on() {
    // Started with the log off, as the images are.
    let mut eng = Engine::new()
        .with_clock(clock_guc)
        .with_slow_query_logger(guc_logger);
    eng.execute("CREATE TABLE q (id INT)").unwrap();
    eng.execute("INSERT INTO q VALUES (1)").unwrap();
    assert_eq!(
        GUC_CALL_COUNT.load(Ordering::SeqCst),
        0,
        "nothing is logged until a session asks",
    );

    // PG's scale: milliseconds, and this clock jumps 50 ms per reading.
    eng.execute("SET log_min_duration_statement = 1").unwrap();
    eng.execute("INSERT INTO q VALUES (2)").unwrap();
    let on = GUC_CALL_COUNT.load(Ordering::SeqCst);
    assert!(on >= 1, "the session turned it on, got {on} fires");

    // And off again, which is what `-1` means.
    eng.execute("SET log_min_duration_statement = -1").unwrap();
    GUC_CALL_COUNT.store(0, Ordering::SeqCst);
    eng.execute("INSERT INTO q VALUES (3)").unwrap();
    assert_eq!(
        GUC_CALL_COUNT.load(Ordering::SeqCst),
        0,
        "-1 is off, as it is in PostgreSQL",
    );
}

#[test]
fn plan_cache_cap_overridable_via_engine_api() {
    let mut eng = Engine::new();
    eng.set_plan_cache_max(8);
    // Fill past the new cap; cache should retain only 8.
    for i in 0..32 {
        eng.execute(&format!("CREATE TABLE t{i} (id INT)")).unwrap();
        eng.execute(&format!("INSERT INTO t{i} VALUES (1)"))
            .unwrap();
    }
    // The plan cache is now bounded at 8. Verify via the introspect
    // accessor.
    assert!(
        eng.plan_cache().len() <= 8,
        "plan cache cap not respected, len = {}",
        eng.plan_cache().len()
    );
}
