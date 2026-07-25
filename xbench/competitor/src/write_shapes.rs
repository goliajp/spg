//! r175 — the write-heavy shape suite, shared between the embedded
//! panel (`write_heavy`) and the wire panel (`wire_heavy`). The
//! driver is protocol-agnostic: every shape runs through a
//! run-one-statement closure, so the same timed bodies drive
//! spg-embedded, SPGS-over-pgwire and PG18 identically.

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::fmt::Write as _;
use std::time::Instant;

pub const N: i64 = 50_000;
pub const WARMUP: usize = 5;
/// v7.39 (round 445) — 11 was not enough to call a winner.
///
/// Measured: five back-to-back panel runs put `insert_singles_100` anywhere
/// between 0.90x and 3.68x and `delete_reinsert_1k` between 1.31x and 3.17x.
/// Every single-run verdict on those shapes was inside its own noise. The
/// shapes are dominated by fsync latency on a virtualised disk, which is
/// jittery by nature, so the only fix is more samples: the methodology's
/// "n 100 -> 1000, runs 3 -> 10" applied to this panel.
///
/// `SPG_BENCH_RUNS` overrides it, so a variance sweep can push higher still
/// without a rebuild.
pub const RUNS: usize = 51; // odd → clean median

/// Effective run count: `SPG_BENCH_RUNS` when set and sane, else [`RUNS`].
#[must_use]
pub fn runs() -> usize {
    std::env::var("SPG_BENCH_RUNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n >= 3)
        .unwrap_or(RUNS)
}

/// One write shape: (name, setup-free timed body). The body receives a
/// per-run id base so inserts never collide; the harness deletes the
/// inserted segment AFTER the timing window.
#[derive(Clone, Copy)]
pub enum Shape {
    /// One statement, 1000-row VALUES list.
    InsertBatch1k,
    /// 100 independent single-row INSERTs (autocommit each).
    InsertSingles100,
    /// BEGIN; 100 single-row INSERTs; COMMIT (one durability point).
    TxBatch100,
    /// One UPDATE touching ~20k rows (g is unindexed; v-window fixed).
    UpdateRange,
    /// 100 point UPDATEs by PK.
    UpdatePk100,
    /// DELETE a 1000-row segment then re-insert it (both timed).
    DeleteReinsert1k,
}

pub const SHAPES: &[(&str, Shape)] = &[
    ("insert_batch_1k", Shape::InsertBatch1k),
    ("insert_singles_100", Shape::InsertSingles100),
    ("tx_batch_100", Shape::TxBatch100),
    ("update_range_20k", Shape::UpdateRange),
    ("update_pk_100", Shape::UpdatePk100),
    ("delete_reinsert_1k", Shape::DeleteReinsert1k),
];

pub fn val_for(i: i64) -> i64 {
    (i * 2_654_435_761) % 100_000
}

pub fn batch_insert_sql(base: i64, rows: i64) -> String {
    let mut sql = String::with_capacity(rows as usize * 24 + 32);
    sql.push_str("INSERT INTO wb VALUES ");
    for k in 0..rows {
        let id = base + k;
        if k > 0 {
            sql.push(',');
        }
        let _ = write!(sql, "({id}, {}, {})", id % 100, val_for(id));
    }
    sql
}

pub fn median_ms(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    samples[samples.len() / 2]
}

#[must_use]
pub fn verdict(ratio: f64) -> &'static str {
    if ratio <= 1.0 {
        "WIN"
    } else if ratio < 1.2 {
        "~tied"
    } else {
        "LOSS"
    }
}

/// Drive one timed run of `shape` through `exec` (a run-one-statement
/// closure). Returns elapsed ms of the timed window only; cleanup
/// happens outside the window via `exec` too.
pub fn run_shape(shape: Shape, base: i64, exec: &mut dyn FnMut(&str)) -> f64 {
    match shape {
        Shape::InsertBatch1k => {
            let sql = batch_insert_sql(base, 1000);
            let t0 = Instant::now();
            exec(&sql);
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            exec(&format!(
                "DELETE FROM wb WHERE id >= {base} AND id < {}",
                base + 1000
            ));
            ms
        }
        Shape::InsertSingles100 => {
            let stmts: Vec<String> = (0..100)
                .map(|k| {
                    let id = base + k;
                    format!(
                        "INSERT INTO wb VALUES ({id}, {}, {})",
                        id % 100,
                        val_for(id)
                    )
                })
                .collect();
            let t0 = Instant::now();
            for s in &stmts {
                exec(s);
            }
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            exec(&format!(
                "DELETE FROM wb WHERE id >= {base} AND id < {}",
                base + 100
            ));
            ms
        }
        Shape::TxBatch100 => {
            let stmts: Vec<String> = (0..100)
                .map(|k| {
                    let id = base + k;
                    format!(
                        "INSERT INTO wb VALUES ({id}, {}, {})",
                        id % 100,
                        val_for(id)
                    )
                })
                .collect();
            let t0 = Instant::now();
            exec("BEGIN");
            for s in &stmts {
                exec(s);
            }
            exec("COMMIT");
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            exec(&format!(
                "DELETE FROM wb WHERE id >= {base} AND id < {}",
                base + 100
            ));
            ms
        }
        Shape::UpdateRange => {
            // g is unindexed, v stays untouched → the matching window is
            // stable across runs and no index maintenance noise on g.
            let t0 = Instant::now();
            exec("UPDATE wb SET g = g + 1 WHERE v BETWEEN 20000 AND 40000");
            t0.elapsed().as_secs_f64() * 1000.0
        }
        Shape::UpdatePk100 => {
            let stmts: Vec<String> = (0..100)
                .map(|k| {
                    let id = (base + k * 37) % N + 1;
                    format!("UPDATE wb SET g = g + 1 WHERE id = {id}")
                })
                .collect();
            let t0 = Instant::now();
            for s in &stmts {
                exec(s);
            }
            t0.elapsed().as_secs_f64() * 1000.0
        }
        Shape::DeleteReinsert1k => {
            // Delete a 1000-row slice of the base table, then restore it.
            let lo = 1 + (base % 40_000);
            let hi = lo + 1000;
            let del = format!("DELETE FROM wb WHERE id >= {lo} AND id < {hi}");
            let mut ins = String::with_capacity(1000 * 24 + 32);
            ins.push_str("INSERT INTO wb VALUES ");
            for (k, id) in (lo..hi).enumerate() {
                if k > 0 {
                    ins.push(',');
                }
                let _ = write!(ins, "({id}, {}, {})", id % 100, val_for(id));
            }
            let t0 = Instant::now();
            exec(&del);
            exec(&ins);
            t0.elapsed().as_secs_f64() * 1000.0
        }
    }
}

/// Full suite against one engine: create schema, seed N rows, run every
/// shape (warm-ups + RUNS timed), return per-shape median ms.
pub fn bench_engine(exec: &mut dyn FnMut(&str)) -> Vec<f64> {
    exec("CREATE TABLE wb (id INT PRIMARY KEY, g INT NOT NULL, v INT NOT NULL)");
    exec("CREATE INDEX wb_v_idx ON wb (v)");
    // Seed in 1k-row batches (setup, untimed).
    let mut i = 1;
    while i <= N {
        exec(&batch_insert_sql(i, 1000.min(N - i + 1)));
        i += 1000;
    }
    let mut out = Vec::with_capacity(SHAPES.len());
    // Fresh id space above the seeded rows; each run gets its own slice.
    let mut next_base = N + 1_000_000;
    for (_, shape) in SHAPES {
        for _ in 0..WARMUP {
            run_shape(*shape, next_base, exec);
            next_base += 10_000;
        }
        let n_runs = runs();
        let mut samples = Vec::with_capacity(n_runs);
        for _ in 0..n_runs {
            samples.push(run_shape(*shape, next_base, exec));
            next_base += 10_000;
        }
        out.push(median_ms(samples));
    }
    out
}
