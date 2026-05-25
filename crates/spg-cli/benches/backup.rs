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
            Value::Text(format!("user-{i}")),
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

criterion_group!(benches, bench_backup_roundtrip);
criterion_main!(benches);
