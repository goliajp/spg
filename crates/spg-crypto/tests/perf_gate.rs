#![cfg(not(debug_assertions))]
// Bench/test-gate code allow-list (cast precision, doc-markdown, etc.)
// — these files are dev-only, the gate's job is regression catching.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::useless_conversion,
    clippy::similar_names
)]

//! Regression-catch perf gate. Budgets and methodology live in
//! `crates/spg-crypto/BUDGETS.md`. Run via `cargo test -p spg-crypto
//! --test perf_gate`.
//!
//! These are *not* publishable numbers. The criterion bench medians in
//! `crates/spg-crypto/benches/hash.rs` are; quote those instead.

use std::time::Instant;

use spg_crypto::hash;

/// Run `op` `iters` times in release mode (the test harness compiles
/// with `--release` only when invoked via `cargo test --release`; the
/// budgets here are sized so debug-mode timings still fit comfortably).
fn measure<F: FnMut()>(iters: u32, mut op: F) -> f64 {
    let start = Instant::now();
    for _ in 0..iters {
        op();
    }
    start.elapsed().as_secs_f64() / f64::from(iters)
}

#[test]
fn hash_64b_under_budget() {
    let input = [0u8; 64];
    // 5_000 iters keeps the test under 1 s even at the budget ceiling.
    let mean_secs = measure(5_000, || {
        let _ = hash(std::hint::black_box(&input));
    });
    let budget_secs = 10e-6; // 10 µs
    assert!(
        mean_secs < budget_secs,
        "hash_64b mean {mean_secs:.6} s exceeds budget {budget_secs:.6} s"
    );
}

#[test]
// CI shared runners flake on this micro-benchmark; run locally
// with `cargo test --release -- --ignored hash_1kib`.
#[ignore]
fn hash_1kib_under_budget() {
    let input = [0u8; 1024];
    let mean_secs = measure(2_000, || {
        let _ = hash(std::hint::black_box(&input));
    });
    let budget_secs = 50e-6; // 50 µs
    assert!(
        mean_secs < budget_secs,
        "hash_1kib mean {mean_secs:.6} s exceeds budget {budget_secs:.6} s"
    );
}
