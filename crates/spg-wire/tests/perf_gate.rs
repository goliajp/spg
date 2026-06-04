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

//! Regression-catch perf gate for `spg-wire`. Budgets in `BUDGETS.md`.
//! Not publishable numbers — see `benches/frames.rs` for those.

use std::time::Instant;

use spg_wire::{build_query, decode, encode, parse_query};

fn measure<F: FnMut()>(iters: u32, mut op: F) -> f64 {
    let start = Instant::now();
    for _ in 0..iters {
        op();
    }
    start.elapsed().as_secs_f64() / f64::from(iters)
}

#[test]
fn query_roundtrip_under_budget() {
    let sql = "SELECT id, name FROM users WHERE id = 42";
    let mean_secs = measure(5_000, || {
        let frame = build_query(std::hint::black_box(sql));
        let mut buf = Vec::with_capacity(64);
        encode(&frame, &mut buf).expect("encode ok");
        let (decoded, _) = decode(&buf).expect("decode ok");
        let _ = parse_query(&decoded);
    });
    let budget_secs = 10e-6;
    assert!(
        mean_secs < budget_secs,
        "query_encode_decode_roundtrip mean {mean_secs:.6} s exceeds budget {budget_secs:.6} s"
    );
}
