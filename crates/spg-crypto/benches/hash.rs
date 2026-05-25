// Bench code intentionally trades pedantic lint ceremony (cast precision,
// useless conversions, doc-markdown) for readability — these files are
// dev-only and the perf numbers are what matters.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::useless_conversion,
    clippy::similar_names
)]

//! Stone-level criterion bench for `spg-crypto`. Measures the BLAKE3
//! single-block path (audit-log entry hash) and the multi-chunk path
//! (catalog snapshot hash). Medians here roll up into the workspace
//! `PERFORMANCE.md`.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use spg_crypto::hash;

fn bench_hash(c: &mut Criterion) {
    // 64 B: single BLAKE3 block — the size band an audit-log entry hash
    // (a serialized AuditEntry header) sits in.
    let input64 = vec![0u8; 64];
    c.bench_function("hash_64b", |b| {
        b.iter(|| {
            let _ = hash(black_box(&input64));
        });
    });
    // 1 KiB: single BLAKE3 chunk — covers a small catalog row.
    let input1k = vec![0u8; 1024];
    c.bench_function("hash_1kib", |b| {
        b.iter(|| {
            let _ = hash(black_box(&input1k));
        });
    });
    // 16 KiB: multi-chunk path — small catalog snapshot.
    let input16k = vec![0u8; 16 * 1024];
    c.bench_function("hash_16kib", |b| {
        b.iter(|| {
            let _ = hash(black_box(&input16k));
        });
    });
}

criterion_group!(benches, bench_hash);
criterion_main!(benches);
