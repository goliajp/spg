//! v7.38 元机制 D unit pin — `SPG_TEST_EXPLAIN_NO_COSTS=1` strips the
//! nondeterministic `elapsed=…us` annotation off the EXPLAIN ANALYZE
//! Total line so test diffs are byte-equal across runs.
//!
//! Acceptor: `crates/spg-engine/src/lib.rs` `exec_explain` (Total line).
//! Index   : `xtests/sigil/test-mode-gucs.md`.

use spg_engine::testkit::EnvConfig;
use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn fake_clock() -> i64 {
    // Deterministic strictly-monotonic stub so EXPLAIN ANALYZE has a
    // wall clock to subtract against. Using a real wall clock would
    // make the production-mode assertion brittle (elapsed varies);
    // the GUC-off path only needs *some* elapsed digit to assert
    // against.
    use std::sync::atomic::{AtomicI64, Ordering};
    static TICK: AtomicI64 = AtomicI64::new(0);
    TICK.fetch_add(17, Ordering::Relaxed) + 17
}

fn explain_lines(engine: &mut Engine) -> Vec<String> {
    engine.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    for i in 0..10 {
        engine
            .execute(&format!("INSERT INTO t VALUES ({i})"))
            .unwrap();
    }
    let r = engine.execute("EXPLAIN ANALYZE SELECT * FROM t").unwrap();
    match r {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|row| match row.values.into_iter().next().unwrap() {
                Value::Text(s) => s.into_owned(),
                _ => panic!("EXPLAIN line not text"),
            })
            .collect(),
        _ => panic!("EXPLAIN returned non-Rows"),
    }
}

#[test]
fn explain_no_costs_strips_elapsed_from_total_line() {
    // v7.39 (round 227) — PG shape: production EXPLAIN ANALYZE emits
    // `Execution Time: … ms` (replaced SPG's `Total: … elapsed=…us`).
    let mut on = Engine::new().with_clock(fake_clock);
    let on_lines = explain_lines(&mut on);
    assert!(
        on_lines.iter().any(|l| l.starts_with("Execution Time: ")),
        "production EXPLAIN must include Execution Time; got {on_lines:?}"
    );

    // GUC on: same query, Total line has no elapsed annotation.
    let mut off = Engine::new()
        .with_clock(fake_clock)
        .with_env_cfg(EnvConfig::builder().explain_no_costs(true).build());
    let off_lines = explain_lines(&mut off);
    assert!(
        !off_lines.iter().any(|l| l.starts_with("Execution Time: ")),
        "EXPLAIN_NO_COSTS must strip the timing summary; got {off_lines:?}"
    );
}

#[test]
fn explain_no_costs_is_byte_equal_across_runs() {
    // Two independent engines with the GUC on must emit byte-equal
    // EXPLAIN ANALYZE output for the same query — the whole point
    // of EXPLAIN_NO_COSTS.
    let cfg = EnvConfig::builder().explain_no_costs(true).build();
    let mut a = Engine::new()
        .with_clock(fake_clock)
        .with_env_cfg(cfg.clone());
    let mut b = Engine::new().with_clock(fake_clock).with_env_cfg(cfg);
    let la = explain_lines(&mut a);
    let lb = explain_lines(&mut b);
    assert_eq!(
        la, lb,
        "EXPLAIN_NO_COSTS output must be byte-equal across runs"
    );
}
