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

//! Regression-catch perf gate for `spg-engine`. Budgets in `BUDGETS.md`.

use std::time::Instant;

use spg_engine::Engine;

fn fresh_engine() -> Engine {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL, score FLOAT)")
        .unwrap();
    for i in 0..100 {
        let sql = format!(
            "INSERT INTO users VALUES ({i}, 'user-{i}', {})",
            i as f64 * 0.1
        );
        eng.execute(&sql).unwrap();
    }
    eng
}

#[test]
fn execute_select_where_under_budget() {
    let mut eng = fresh_engine();
    let iters: u32 = 200;
    let start = Instant::now();
    for _ in 0..iters {
        let r = eng
            .execute(std::hint::black_box(
                "SELECT id, name FROM users WHERE id = 42",
            ))
            .expect("ok");
        std::hint::black_box(r);
    }
    let mean_secs = start.elapsed().as_secs_f64() / f64::from(iters);
    let budget_secs = 500e-6;
    assert!(
        mean_secs < budget_secs,
        "execute_select_where_n100 mean {mean_secs:.6} s exceeds budget {budget_secs:.6} s"
    );
}
