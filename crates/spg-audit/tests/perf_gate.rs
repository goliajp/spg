#![cfg(not(debug_assertions))]
// Test-gate allow-list — see crates/spg-crypto/tests/perf_gate.rs.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::useless_conversion,
    clippy::similar_names
)]

//! Regression-catch perf gate for `spg-audit`. Budgets in `BUDGETS.md`.

use std::time::Instant;

use spg_audit::AuditLog;

#[test]
fn append_one_under_budget() {
    // Build a 100-entry log so append's prev-hash lookup is on its
    // typical path (not the initial empty-log fast lane).
    let mut log = AuditLog::new();
    for i in 0..100 {
        log.append(format!("SELECT {i}"), 1_700_000_000_000 + i as u64);
    }
    let iters: u32 = 1_000;
    let start = Instant::now();
    for i in 0..iters {
        log.append(
            std::hint::black_box(format!("SELECT id = {i}")),
            std::hint::black_box(1_700_000_000_000),
        );
    }
    let mean_secs = start.elapsed().as_secs_f64() / f64::from(iters);
    let budget_secs = 50e-6;
    assert!(
        mean_secs < budget_secs,
        "append_one mean {mean_secs:.6} s exceeds budget {budget_secs:.6} s"
    );
}
