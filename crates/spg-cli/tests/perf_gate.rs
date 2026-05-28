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
//!
//! v5.3.5 split: the original `backup_roundtrip_under_budget` measured
//! `fs::read + Catalog::deserialize + Catalog::serialize + fs::write`,
//! but on macOS Tahoe APFS the `fs::write` truncate-then-rewrite path
//! for a tiny file dominates the timing (~120 ms / iter measured on a
//! 100-row file) — it does not reflect any spg-storage codec change.
//!
//! Gates after the split:
//!   * `backup_codec_under_budget` — codec-only, no file I/O. Measures
//!     `Catalog::deserialize + Catalog::serialize`. **This is the spg-
//!     storage regression gate** and stays on the CI hot path.
//!   * `backup_disk_roundtrip_under_budget` — full read/deserialize/
//!     serialize/write loop, `#[ignore]`-marked because it benchmarks
//!     the host OS file system far more than the spg code. Release
//!     process invokes it manually for a cold-cache sanity number.

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

fn build_seed_catalog() -> Catalog {
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
    cat
}

/// CI gate: spg-storage `Catalog::deserialize + serialize` round-trip
/// on a 100-row in-memory snapshot. No file I/O — file system noise
/// belongs in the `#[ignore]`-marked disk benchmark below, not in the
/// spg codec regression gate.
#[test]
fn backup_codec_under_budget() {
    let bytes = build_seed_catalog().serialize();
    let iters: u32 = 100;
    let start = Instant::now();
    for _ in 0..iters {
        let parsed = Catalog::deserialize(std::hint::black_box(&bytes)).unwrap();
        let out = parsed.serialize();
        std::hint::black_box(out);
    }
    let mean_secs = start.elapsed().as_secs_f64() / f64::from(iters);
    let budget_secs = 5e-3;
    assert!(
        mean_secs < budget_secs,
        "backup_codec_100rows mean {mean_secs:.6} s exceeds budget {budget_secs:.6} s"
    );
}

/// Release-process gate (kept `#[ignore]` because it benchmarks the
/// host OS file system more than the spg code). On macOS Tahoe APFS
/// the per-iter `fs::write` truncate dominates and can reach ~120 ms
/// for a < 1 KB file; on Linux ext4/xfs the same call is sub-ms. The
/// generous 400 ms budget keeps the gate as a smoke test for "is the
/// disk path catastrophically slow" without making CI host-dependent.
#[test]
#[ignore = "release-process: OS-dependent disk timing; codec gate covers regressions"]
fn backup_disk_roundtrip_under_budget() {
    let src = seed_path();
    let dst = dst_path();
    fs::write(&src, build_seed_catalog().serialize()).unwrap();

    let iters: u32 = 20;
    let start = Instant::now();
    for _ in 0..iters {
        let bytes = fs::read(&src).unwrap();
        let parsed = Catalog::deserialize(std::hint::black_box(&bytes)).unwrap();
        let out = parsed.serialize();
        fs::write(&dst, out).unwrap();
    }
    let mean_secs = start.elapsed().as_secs_f64() / f64::from(iters);
    let budget_secs = 400e-3;
    assert!(
        mean_secs < budget_secs,
        "backup_disk_roundtrip_100rows mean {mean_secs:.6} s exceeds budget {budget_secs:.6} s"
    );
    let _ = fs::remove_file(&src);
    let _ = fs::remove_file(&dst);
}
