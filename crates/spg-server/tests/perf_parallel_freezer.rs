//! v6.7.4 ship-gate #2 — `4_worker_speedup_at_least_2x`.
//!
//! Measures the **prepare phase** of the parallel-freezer path
//! (`Catalog::prepare_freeze_slice` × N workers vs single
//! call) under workers=1 vs workers=4. This is the parallelisable
//! half of the freeze workflow — `commit_freeze_slices`'s
//! `encode_segment` step is, by design (V6_7_DESIGN.md §6), a
//! single-coordinator serial pass that produces one merged
//! segment. Measuring end-to-end would let the serial commit
//! dominate Amdahl, masking the real prepare-phase speedup the
//! v6.7.4 architecture targets.
//!
//! The harness:
//!   1. Builds a populated `Catalog` with a wide payload so
//!      `encode_row_body_dense` (the dominant per-row prepare
//!      cost) shows up in wall-time.
//!   2. Runs `prepare_freeze_slice` × 1 worker, captures
//!      wall-time.
//!   3. Runs `prepare_freeze_slice` × 4 workers via
//!      `std::thread::scope`, captures wall-time.
//!   4. Asserts `t1 / t4 >= 2.0` (the v6.7.4 ship-gate
//!      threshold). Softer 1.5× on hosts with < 4 logical
//!      cores — the gate's intent is "actually scales when
//!      cores exist", and a 2-core CI host structurally can't
//!      hit 2×.
//!   5. Cross-checks correctness: a full freeze through the
//!      parallel commit path produces the same segment bytes
//!      as a single-slice freeze on a fresh clone.
//!
//! Pure storage-layer driver — no spg-server child, no TCP, no
//! WAL, no encode_segment in the measured loop.

use std::ops::Range;
use std::time::Instant;

use spg_storage::{
    Catalog, ColumnSchema, DataType, FreezeSlice, IndexKey, Row, StorageError, TableSchema, Value,
};

/// Big-enough row count that per-worker prepare cost dominates
/// over thread-spawn overhead. 500K rows × 1 KiB payload at
/// ~250 ns/row on a modern x86 core gives ~125 ms single-worker
/// prepare — comfortably above the ~1 ms `std::thread::scope`
/// overhead.
const POPULATE_ROWS: usize = 500_000;
const PAYLOAD_LEN: usize = 1_024;
/// Freeze the whole populated table — keeps prepare and commit
/// phases at their max workload.
const BATCH_ROWS: usize = POPULATE_ROWS;

/// Number of prepare-only repetitions to average out the timing.
/// `prepare_freeze_slice` doesn't mutate the catalog, so we can
/// re-run on the same `&Catalog` for cleaner measurements.
const PREPARE_REPS: usize = 5;

fn wide_users_schema() -> TableSchema {
    TableSchema {
        name: "users".to_string(),
        columns: vec![
            ColumnSchema::new("id".to_string(), DataType::BigInt, false),
            ColumnSchema::new("payload".to_string(), DataType::Text, false),
        ],
        hot_tier_bytes: None,
        foreign_keys: Vec::new(),
        uniqueness_constraints: Vec::new(),
    }
}

fn build_populated_catalog() -> Catalog {
    let mut cat = Catalog::new();
    cat.create_table(wide_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    let payload: String = "x".repeat(PAYLOAD_LEN);
    for id in 0..POPULATE_ROWS as i64 {
        let row = Row::new(vec![Value::BigInt(id), Value::Text(payload.clone())]);
        t.insert(row).unwrap();
    }
    t.add_index("by_id".to_string(), "id").unwrap();
    cat
}

/// Slice `0..n` into `parts` contiguous ranges, mirroring
/// `spg_server::freezer::partition_range`. Kept private here to
/// avoid a dependency on a server-private helper.
fn partition_range(n: usize, parts: usize) -> Vec<Range<usize>> {
    let mut out = Vec::with_capacity(parts);
    let base = n / parts;
    let extra = n % parts;
    let mut start = 0;
    for i in 0..parts {
        let len = base + usize::from(i < extra);
        out.push(start..start + len);
        start += len;
    }
    out
}

fn freeze_once(cat: &mut Catalog, workers: usize) -> Result<(), StorageError> {
    let ranges = partition_range(BATCH_ROWS, workers);
    let prep_t0 = Instant::now();
    let slices: Vec<FreezeSlice> = if workers == 1 {
        ranges
            .into_iter()
            .map(|r| cat.prepare_freeze_slice("users", "by_id", r))
            .collect::<Result<_, _>>()?
    } else {
        let cat_ref: &Catalog = cat;
        std::thread::scope(|s| {
            let handles: Vec<_> = ranges
                .into_iter()
                .map(|r| s.spawn(move || cat_ref.prepare_freeze_slice("users", "by_id", r)))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("worker panicked"))
                .collect::<Result<Vec<_>, _>>()
        })?
    };
    let prep_wall = prep_t0.elapsed();
    let commit_t0 = Instant::now();
    cat.commit_freeze_slices("users", "by_id", slices)?;
    let commit_wall = commit_t0.elapsed();
    println!("  workers={workers}: prepare={prep_wall:?}, commit={commit_wall:?}");
    Ok(())
}

/// Run `Catalog::prepare_freeze_slice` × `workers` times in a
/// `std::thread::scope` (or inline for `workers == 1`) and
/// return the wall-time. Catalog is `&` only — no mutation
/// happens here. Returns the best of `reps` runs to minimise
/// transient jitter (OS scheduler, page-cache warm-up).
fn measure_prepare_wall(cat: &Catalog, workers: usize, reps: usize) -> std::time::Duration {
    let mut best = std::time::Duration::from_secs(u64::MAX);
    for _ in 0..reps {
        let ranges = partition_range(BATCH_ROWS, workers);
        let t0 = Instant::now();
        let _slices: Vec<FreezeSlice> = if workers == 1 {
            ranges
                .into_iter()
                .map(|r| cat.prepare_freeze_slice("users", "by_id", r).unwrap())
                .collect()
        } else {
            std::thread::scope(|s| {
                let handles: Vec<_> = ranges
                    .into_iter()
                    .map(|r| {
                        s.spawn(move || cat.prepare_freeze_slice("users", "by_id", r).unwrap())
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            })
        };
        let elapsed = t0.elapsed();
        if elapsed < best {
            best = elapsed;
        }
    }
    best
}

/// `#[ignore]` because the perf gate is sensitive to CPU
/// contention from other tests running in parallel (cargo test's
/// default thread pool fans out across cores). Run explicitly:
///
/// ```sh
/// cargo test -p spg-server --test perf_parallel_freezer --release -- --ignored
/// ```
///
/// Matches the pattern used by `tests/perf_1b_rows` (v6.7.7).
#[test]
#[ignore]
fn four_worker_speedup_at_least_2x() {
    let base = build_populated_catalog();

    // Prepare-only timing — the parallelisable half of the freeze
    // workflow. Catalog is unchanged across reps.
    let t_single = measure_prepare_wall(&base, 1, PREPARE_REPS);
    let t_quad = measure_prepare_wall(&base, 4, PREPARE_REPS);
    let speedup = t_single.as_secs_f64() / t_quad.as_secs_f64().max(1e-9);
    println!(
        "perf_parallel_freezer prepare phase: \
         t_single={t_single:?}, t_quad={t_quad:?}, speedup={speedup:.2}×"
    );

    // Correctness cross-check: a full freeze through the parallel
    // commit path produces the same segment bytes as a
    // single-slice freeze on a fresh clone.
    let mut c1 = base.clone();
    freeze_once(&mut c1, 1).expect("1-worker freeze");
    let mut c4 = base.clone();
    freeze_once(&mut c4, 4).expect("4-worker freeze");
    let single_seg = c1
        .cold_segment(0)
        .expect("seg 0 on serial freeze")
        .bytes()
        .to_vec();
    let quad_seg = c4
        .cold_segment(0)
        .expect("seg 0 on parallel freeze")
        .bytes()
        .to_vec();
    assert_eq!(
        single_seg, quad_seg,
        "parallel freeze produced different segment bytes than serial freeze"
    );
    for id in [0i64, 1, BATCH_ROWS as i64 / 2, (BATCH_ROWS - 1) as i64] {
        assert_eq!(
            c1.lookup_by_pk("users", "by_id", &IndexKey::Int(id)),
            c4.lookup_by_pk("users", "by_id", &IndexKey::Int(id))
        );
    }

    // Gate. On runners with < 4 logical cores, fall back to a
    // softer 1.5× — the v6.7.4 ship gate's intent is "actually
    // scales when there are cores to scale onto", and a 2-core
    // CI host structurally can't hit 2×.
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    let threshold = if cores >= 4 { 2.0 } else { 1.5 };
    assert!(
        speedup >= threshold,
        "speedup {speedup:.2}× < required {threshold}× on a {cores}-core host \
         (t_single={t_single:?}, t_quad={t_quad:?})"
    );
}
