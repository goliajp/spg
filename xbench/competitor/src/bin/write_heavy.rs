//! Write-heavy latency — SPG (embedded, DURABLE file backend) vs PostgreSQL 18.
//!
//! Phase-0 loss-hunt extension (r164): the read panel (`heavy`) shows 19/19
//! WINs, so the remaining loss frontier is writes. SPG runs
//! `spg_embedded::Database::open_path` — the real durable customer path (WAL
//! + fsync semantics), NOT the bare in-memory Engine the read panel uses.
//!
//! r174 — both engines now run BOTH commit modes, compared like-for-like:
//!
//!  * `sync=on` (each engine's durable default): the honest durability
//!    column — every commit waits for its fsync point.
//!  * `sync=off` (the identical customer SQL `SET synchronous_commit = off`
//!    on both engines — real on SPG since r171/r172): PG's documented
//!    latency trade, commit returns before flush.
//!
//! The r164 original compared SPG-durable against PG-off only (tilted
//! against SPG so a WIN was unarguable); with the session GUC real on SPG
//! the panel can finally show both fair pairings.
//!
//! Run: `cargo run --release -p spg-bench-competitor --bin write_heavy`
//! Needs PostgreSQL reachable at the `connection_strings()` postgres URL.

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::uninlined_format_args
)]

use spg_bench_competitor::connection_strings;
use sqlx::any::AnyPoolOptions;
use std::fmt::Write as _;
use std::time::Instant;

const N: i64 = 50_000;
const WARMUP: usize = 2;
const RUNS: usize = 11; // odd → clean median

/// One write shape: (name, setup-free timed body). The body receives a
/// per-run id base so inserts never collide; the harness deletes the
/// inserted segment AFTER the timing window.
#[derive(Clone, Copy)]
enum Shape {
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

const SHAPES: &[(&str, Shape)] = &[
    ("insert_batch_1k", Shape::InsertBatch1k),
    ("insert_singles_100", Shape::InsertSingles100),
    ("tx_batch_100", Shape::TxBatch100),
    ("update_range_20k", Shape::UpdateRange),
    ("update_pk_100", Shape::UpdatePk100),
    ("delete_reinsert_1k", Shape::DeleteReinsert1k),
];

fn val_for(i: i64) -> i64 {
    (i * 2_654_435_761) % 100_000
}

fn batch_insert_sql(base: i64, rows: i64) -> String {
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

fn median_ms(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    samples[samples.len() / 2]
}

/// Drive one timed run of `shape` through `exec` (a run-one-statement
/// closure). Returns elapsed ms of the timed window only; cleanup
/// happens outside the window via `exec` too.
fn run_shape(shape: Shape, base: i64, exec: &mut dyn FnMut(&str)) -> f64 {
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

fn bench_engine(exec: &mut dyn FnMut(&str)) -> Vec<f64> {
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
        let mut samples = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            samples.push(run_shape(*shape, next_base, exec));
            next_base += 10_000;
        }
        out.push(median_ms(samples));
    }
    out
}

/// One full SPG pass on a fresh durable database. `sync_off` issues the
/// same customer SQL PG gets — the r171 session GUC, not an env knob.
fn bench_spg(sync_off: bool) -> Result<Vec<f64>, Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join(format!(
        "spg-write-heavy-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir)?;
    let mut db = spg_embedded::Database::open_path(dir.join("bench.db"))
        .map_err(|e| format!("open_path: {e}"))?;
    if sync_off {
        db.execute("SET synchronous_commit = off")
            .map_err(|e| format!("SET synchronous_commit: {e}"))?;
    }
    let out = bench_engine(&mut |sql| {
        db.execute(sql).unwrap();
    });
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(out)
}

fn verdict(ratio: f64) -> &'static str {
    if ratio <= 1.0 {
        "WIN"
    } else if ratio < 1.2 {
        "~tied"
    } else {
        "LOSS"
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();

    // ---- SPG embedded, durable file backend, both commit modes ----
    let spg_on = bench_spg(false)?;
    let spg_off = bench_spg(true)?;

    // ---- PostgreSQL 18, both commit modes ----
    let pg_url = connection_strings()
        .into_iter()
        .find(|(n, _)| *n == "postgres")
        .map(|(_, u)| u)
        .ok_or("no postgres connection string")?;
    let pool = AnyPoolOptions::new()
        .max_connections(1)
        .connect(&pg_url)
        .await?;
    let rt = tokio::runtime::Handle::current();
    let bench_pg = |mode: &'static str| {
        let pool = pool.clone();
        let rt = rt.clone();
        tokio::task::block_in_place(move || {
            rt.block_on(async {
                sqlx::query("DROP TABLE IF EXISTS wb")
                    .execute(&pool)
                    .await
                    .unwrap();
                sqlx::query(&format!("SET synchronous_commit = {mode}"))
                    .execute(&pool)
                    .await
                    .unwrap();
            });
            let rt2 = rt.clone();
            let pool2 = pool.clone();
            let out = bench_engine(&mut |sql| {
                rt2.block_on(async {
                    sqlx::query(sql).execute(&pool2).await.unwrap();
                });
            });
            rt.block_on(async {
                sqlx::query("DROP TABLE IF EXISTS wb")
                    .execute(&pool)
                    .await
                    .unwrap();
            });
            out
        })
    };
    let pg_on = bench_pg("on");
    let pg_off = bench_pg("off");

    println!("# write-heavy shapes — median ms over {RUNS} runs, {N}-row seeded table");
    println!("# SPG = spg-embedded DURABLE (open_path); PG18 = postgres:18-alpine");
    println!("# like-for-like: sync=on both sides | sync=off both sides (same customer SQL)");
    println!(
        "| shape             | SPGon ms |  PGon ms |  on-ratio | SPGoff ms | PGoff ms | off-ratio |"
    );
    println!(
        "|-------------------|---------:|---------:|----------:|----------:|---------:|----------:|"
    );
    for (i, (name, _)) in SHAPES.iter().enumerate() {
        let r_on = spg_on[i] / pg_on[i];
        let r_off = spg_off[i] / pg_off[i];
        println!(
            "| {:<17} | {:>8.3} | {:>8.3} | {:>4.2}× {:<5} | {:>9.3} | {:>8.3} | {:>4.2}× {:<5} |",
            name,
            spg_on[i],
            pg_on[i],
            r_on,
            verdict(r_on),
            spg_off[i],
            pg_off[i],
            r_off,
            verdict(r_off)
        );
    }
    Ok(())
}
