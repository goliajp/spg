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

//! Regression-catch perf gate for `spg-cli`. Budgets in `BUDGETS.md`.
//! Mirrors the bench: read + deserialize + re-serialize + write.

use std::env::temp_dir;
use std::fs;
use std::time::Instant;

use spg_storage::{Catalog, ColumnSchema, DataType, Row, TableSchema, Value};

fn seed_path() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut p = temp_dir();
    p.push(format!("spg-cli-perfgate-{nanos}-src.spgdb"));
    p
}

fn dst_path() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut p = temp_dir();
    p.push(format!("spg-cli-perfgate-{nanos}-dst.spgdb"));
    p
}

#[test]
fn backup_roundtrip_under_budget() {
    let src = seed_path();
    let dst = dst_path();
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
    fs::write(&src, cat.serialize()).unwrap();

    let iters: u32 = 20;
    let start = Instant::now();
    for _ in 0..iters {
        let bytes = fs::read(&src).unwrap();
        let parsed = Catalog::deserialize(std::hint::black_box(&bytes)).unwrap();
        let out = parsed.serialize();
        fs::write(&dst, out).unwrap();
    }
    let mean_secs = start.elapsed().as_secs_f64() / f64::from(iters);
    let budget_secs = 100e-3;
    assert!(
        mean_secs < budget_secs,
        "backup_roundtrip_100rows mean {mean_secs:.6} s exceeds budget {budget_secs:.6} s"
    );
    let _ = fs::remove_file(&src);
    let _ = fs::remove_file(&dst);
}
