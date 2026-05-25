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

//! Regression-catch perf gate for `spg-sql`. Budgets in `BUDGETS.md`.

use std::time::Instant;

use spg_sql::parser::parse_statement;

fn measure<F: FnMut()>(iters: u32, mut op: F) -> f64 {
    let start = Instant::now();
    for _ in 0..iters {
        op();
    }
    start.elapsed().as_secs_f64() / f64::from(iters)
}

#[test]
fn parse_typical_query_under_budget() {
    let sql = "SELECT id, name FROM users WHERE id > 100 ORDER BY id DESC LIMIT 10";
    let mean_secs = measure(2_000, || {
        let _ = parse_statement(std::hint::black_box(sql)).expect("parse ok");
    });
    let budget_secs = 50e-6;
    assert!(
        mean_secs < budget_secs,
        "parse_select_where_order_limit mean {mean_secs:.6} s exceeds budget {budget_secs:.6} s"
    );
}
