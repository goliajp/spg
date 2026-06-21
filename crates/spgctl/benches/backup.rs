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

//! Stone-level criterion bench for `spg-cli`. The CLI is a binary crate
//! so there's no library API to call into; this bench re-implements the
//! `fn backup` body inline (read → `Catalog::deserialize` →
//! `Catalog::serialize` → write) so the perf number tracks what the
//! subcommand actually does.
//!
//! Measures: 100-row catalog backup over an existing on-disk file.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use spg_storage::{Catalog, ColumnSchema, DataType, Row, TableSchema, Value};
use std::env::temp_dir;
use std::fs;

fn write_seed_catalog(path: &std::path::Path) {
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "users",
        vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new("name", DataType::Text, false),
        ],
    ))
    .unwrap();
    let t = cat.get_mut("users").unwrap();
    for i in 0..100 {
        t.insert(Row::new(vec![
            Value::Int(i),
            Value::text(format!("user-{i}")),
        ]))
        .unwrap();
    }
    fs::write(path, cat.serialize()).unwrap();
}

fn bench_backup_roundtrip(c: &mut Criterion) {
    let src = temp_dir().join("spg-cli-bench-src.spgdb");
    let dst = temp_dir().join("spg-cli-bench-dst.spgdb");
    write_seed_catalog(&src);
    c.bench_function("backup_roundtrip_100rows", |b| {
        b.iter(|| {
            // Mirror `fn backup`: read, deserialize, re-serialize, write.
            let bytes = fs::read(black_box(&src)).expect("read");
            let cat = Catalog::deserialize(&bytes).expect("deserialize");
            let out = cat.serialize();
            fs::write(black_box(&dst), out).expect("write");
        });
    });
    let _ = fs::remove_file(&src);
    let _ = fs::remove_file(&dst);
}

/// v3.1.4: in-memory variant. Bypasses the filesystem entirely so the
/// median reflects only deserialize + re-serialize cost; the
/// fs-backed `backup_roundtrip_100rows` above swings 300× depending
/// on OS page-cache state, which makes its number useless as a perf
/// signal. This one is the real CPU-side cost.
fn bench_backup_inmemory(c: &mut Criterion) {
    // Build the seed catalog once and capture its serialized form;
    // the bench loop then operates on a `Vec<u8>` source, with the
    // destination going to a freshly allocated `Vec<u8>` each iter.
    let mut cat = Catalog::new();
    cat.create_table(spg_storage::TableSchema::new(
        "users",
        vec![
            spg_storage::ColumnSchema::new("id", spg_storage::DataType::Int, false),
            spg_storage::ColumnSchema::new("name", spg_storage::DataType::Text, false),
        ],
    ))
    .unwrap();
    let t = cat.get_mut("users").unwrap();
    for i in 0..100 {
        t.insert(spg_storage::Row::new(vec![
            spg_storage::Value::Int(i),
            spg_storage::Value::text(format!("user-{i}")),
        ]))
        .unwrap();
    }
    let src_bytes = cat.serialize();
    c.bench_function("backup_inmemory_100rows", |b| {
        b.iter(|| {
            let parsed = Catalog::deserialize(black_box(&src_bytes)).expect("deserialize");
            let out = parsed.serialize();
            black_box(out);
        });
    });
}

criterion_group!(benches, bench_backup_roundtrip, bench_backup_inmemory);
criterion_main!(benches);
