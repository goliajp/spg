// Bench code allow-list — see crates/spg-crypto/benches/hash.rs for rationale.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::useless_conversion,
    clippy::similar_names
)]

//! Stone-level criterion bench for `spg-engine`. End-to-end execute
//! cost from raw SQL bytes to QueryResult, over a pre-populated
//! catalog. Covers:
//!   * tiny constant SELECT  (parser-only-ish)
//!   * SELECT with WHERE     (full filter path)
//!   * SELECT with aggregate (group_by + count)
//!   * INSERT one row        (catalog mutate hot path)

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use spg_engine::Engine;

fn fresh_engine() -> Engine {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL, score FLOAT)")
        .unwrap();
    // Populate with 100 rows so the hot path actually walks data.
    for i in 0..100 {
        let sql = format!(
            "INSERT INTO users VALUES ({i}, 'user-{i}', {})",
            i as f64 * 0.1
        );
        eng.execute(&sql).unwrap();
    }
    eng
}

fn bench_select_one(c: &mut Criterion) {
    let mut eng = Engine::new();
    c.bench_function("execute_select_const", |b| {
        b.iter(|| {
            let r = eng.execute(black_box("SELECT 1")).expect("ok");
            black_box(r);
        });
    });
}

fn bench_select_where(c: &mut Criterion) {
    let mut eng = fresh_engine();
    c.bench_function("execute_select_where_n100", |b| {
        b.iter(|| {
            let r = eng
                .execute(black_box("SELECT id, name FROM users WHERE id = 42"))
                .expect("ok");
            black_box(r);
        });
    });
}

fn bench_select_aggregate(c: &mut Criterion) {
    let mut eng = fresh_engine();
    c.bench_function("execute_select_count_group_n100", |b| {
        b.iter(|| {
            let r = eng
                .execute(black_box("SELECT COUNT(*) FROM users WHERE id < 50"))
                .expect("ok");
            black_box(r);
        });
    });
}

fn bench_insert_one(c: &mut Criterion) {
    c.bench_function("execute_insert_one", |b| {
        b.iter_batched(
            fresh_engine,
            |mut eng| {
                let r = eng
                    .execute(black_box("INSERT INTO users VALUES (999, 'nine', 9.9)"))
                    .expect("ok");
                black_box(r);
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

criterion_group!(
    benches,
    bench_select_one,
    bench_select_where,
    bench_select_aggregate,
    bench_insert_one
);
criterion_main!(benches);
