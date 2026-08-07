//! v7.39 (round 478) — Phase A decomposition of `like_filter`.
//!
//! The shape is `SELECT count(*) FROM ht WHERE s LIKE '%_05%'` over 50 000
//! rows of `user_NNNN`. At the round-477 baseline SPGE takes 0.75x PG18 —
//! a win, but far short of the "SPGE >> PG" bar, and 1.7 ms over 50 000
//! rows is 34 ns a row.
//!
//! Counter-first, not samply-first: before profiling, split the 34 ns into
//! the parts by measuring shapes that isolate them. Everything here runs on
//! BOTH engines so each stage is a comparison, not just a SPG number.

#![allow(
    clippy::doc_markdown,
    clippy::uninlined_format_args,
    // One linear script: seed both engines, time each stage, print the
    // table. Splitting it would only move the sequence around.
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::format_push_string,
    clippy::print_literal
)]

use spg_bench_competitor::connection_strings;
use spg_engine::Engine;
use sqlx::any::AnyPoolOptions;
use std::time::{Duration, Instant};

const N: i64 = 50_000;

/// (label, sql, what it isolates)
const STAGES: &[(&str, &str, &str)] = &[
    (
        "scan_only",
        "SELECT count(*) FROM ht",
        "row iteration, no predicate",
    ),
    (
        "eq_filter",
        "SELECT count(*) FROM ht WHERE s = 'user_0005'",
        "iteration + one text compare",
    ),
    (
        "like_prefix",
        "SELECT count(*) FROM ht WHERE s LIKE 'user_0%'",
        "anchored, no leading wildcard",
    ),
    (
        "like_nomatch",
        "SELECT count(*) FROM ht WHERE s LIKE '%zzzzzzzz%'",
        "matcher runs, never matches",
    ),
    (
        "like_filter",
        "SELECT count(*) FROM ht WHERE s LIKE '%_05%'",
        "the shape under test",
    ),
    (
        "like_literal",
        "SELECT count(*) FROM ht WHERE s LIKE '%005%'",
        "same shape, no single-char wildcard",
    ),
    // Is the step from scan to predicate about TEXT, or about predicates?
    // `hi` carries the same row count with an INT payload.
    (
        "int_scan",
        "SELECT count(*) FROM hi",
        "int table, no predicate",
    ),
    (
        "int_filter",
        "SELECT count(*) FROM hi WHERE g = 5",
        "int table, one integer compare",
    ),
    (
        "text_notnull",
        "SELECT count(*) FROM ht WHERE s IS NOT NULL",
        "text cell touched, no comparison",
    ),
];

fn runs() -> usize {
    std::env::var("SPG_BENCH_RUNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n >= 3)
        .unwrap_or(301)
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ---- SPGE ----
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE ht (id INT NOT NULL, s TEXT NOT NULL)")
        .expect("create");
    for i in 1..=N {
        let tag = i % 1000;
        eng.execute(&format!("INSERT INTO ht VALUES ({i}, 'user_{tag:04}')"))
            .expect("seed");
    }
    eng.execute("CREATE TABLE hi (id INT NOT NULL, g INT NOT NULL)")
        .expect("create hi");
    for i in 1..=N {
        eng.execute(&format!("INSERT INTO hi VALUES ({i}, {})", i % 1000))
            .expect("seed hi");
    }
    let mut spge = Vec::new();
    for (_, sql, _) in STAGES {
        for _ in 0..5 {
            let _ = eng.execute(sql).expect("warm");
        }
        let mut s = Vec::with_capacity(runs());
        for _ in 0..runs() {
            let t = Instant::now();
            let _ = eng.execute(sql).expect("timed");
            s.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        spge.push(median(s));
    }

    // ---- PG18 ----
    let rt = tokio::runtime::Runtime::new()?;
    let pg: Vec<f64> = rt.block_on(async {
        sqlx::any::install_default_drivers();
        let url = connection_strings()
            .into_iter()
            .find(|(n, _)| *n == "postgres")
            .map(|(_, u)| u)
            .expect("no postgres connection string");
        let pool = AnyPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(10))
            .connect(&url)
            .await
            .expect("connect pg");
        use sqlx::Executor as _;
        pool.execute("DROP TABLE IF EXISTS ht").await.ok();
        pool.execute("CREATE TABLE ht (id INT NOT NULL, s TEXT NOT NULL)")
            .await
            .unwrap();
        let mut ins = String::from("INSERT INTO ht VALUES ");
        for i in 1..=N {
            if i > 1 {
                ins.push(',');
            }
            ins.push_str(&format!("({i}, 'user_{:04}')", i % 1000));
        }
        pool.execute(ins.as_str()).await.unwrap();
        pool.execute("DROP TABLE IF EXISTS hi").await.ok();
        pool.execute("CREATE TABLE hi (id INT NOT NULL, g INT NOT NULL)")
            .await
            .unwrap();
        let mut ins2 = String::from("INSERT INTO hi VALUES ");
        for i in 1..=N {
            if i > 1 {
                ins2.push(',');
            }
            ins2.push_str(&format!("({i}, {})", i % 1000));
        }
        pool.execute(ins2.as_str()).await.unwrap();
        let mut out = Vec::new();
        for (_, sql, _) in STAGES {
            for _ in 0..5 {
                pool.execute(*sql).await.unwrap();
            }
            let mut s = Vec::with_capacity(runs());
            for _ in 0..runs() {
                let t = Instant::now();
                pool.execute(*sql).await.unwrap();
                s.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            out.push(median(s));
        }
        pool.close().await;
        out
    });

    println!(
        "# round 478 — like_filter decomposition, median ms over {} runs, {N} rows",
        runs()
    );
    println!(
        "{:<14} {:>10} {:>10} {:>8}  {:>9}  {}",
        "stage", "SPGE ms", "PG18 ms", "ratio", "SPGE ns/row", "isolates"
    );
    for (i, (label, _, what)) in STAGES.iter().enumerate() {
        println!(
            "{:<14} {:>10.3} {:>10.3} {:>7.2}x  {:>9.1}  {}",
            label,
            spge[i],
            pg[i],
            spge[i] / pg[i],
            spge[i] * 1e6 / N as f64,
            what
        );
    }
    Ok(())
}
