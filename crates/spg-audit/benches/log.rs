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

//! Stone-level criterion bench for `spg-audit`. Measures the two
//! call shapes that show up under SQL load:
//!   * append_one  — single AuditLog::append (per executed SQL)
//!   * verify_100  — full hash-chain verify over a 100-entry log

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use spg_audit::AuditLog;

fn make_log(n: usize) -> AuditLog {
    let mut log = AuditLog::new();
    for i in 0..n {
        log.append(format!("SELECT {i}"), 1_700_000_000_000 + i as u64);
    }
    log
}

fn bench_append_one(c: &mut Criterion) {
    let sql = "SELECT id, name FROM users WHERE id = 42".to_string();
    c.bench_function("append_one", |b| {
        b.iter_batched(
            AuditLog::new,
            |mut log| {
                log.append(black_box(sql.clone()), black_box(1_700_000_000_000));
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

fn bench_verify_100(c: &mut Criterion) {
    let log = make_log(100);
    c.bench_function("verify_100entries", |b| {
        b.iter(|| {
            log.verify().expect("verify ok");
            black_box(&log);
        });
    });
}

fn bench_serialize_100(c: &mut Criterion) {
    let log = make_log(100);
    c.bench_function("serialize_100entries", |b| {
        b.iter(|| {
            let bytes = log.serialize();
            black_box(bytes);
        });
    });
}

criterion_group!(
    benches,
    bench_append_one,
    bench_verify_100,
    bench_serialize_100
);
criterion_main!(benches);
