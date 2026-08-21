//! v7.38.13 — `SELECT DISTINCT k FROM t ORDER BY k`, the sweep cell that
//! has been ~6 % behind PostgreSQL 18 for three releases and trips the
//! gate inconsistently because the gap fits inside its variance.
//!
//! The table is the sweep's OWN (`perf-endpoint-sweep.sh`), not one
//! invented here: 400k rows, `k` taking 400k distinct values through a
//! multiplicative hash, and a 200-byte `pad` the query never reads. An
//! earlier A/B on an invented table measured 21 ms where this measures
//! 133 — same SQL text, different work.
//!
//! Controls, so the profile can be attributed:
//!   distinct  — the shape itself
//!   order     — `SELECT k FROM t ORDER BY k`, no DISTINCT
//!   groupby   — `SELECT k FROM t GROUP BY k ORDER BY k`, PG's other
//!               spelling of the same answer
use spg_engine::Engine;
use std::time::Instant;

fn build(n: i64) -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT PRIMARY KEY, k INT, pad TEXT)")
        .unwrap();
    e.execute(&format!(
        "INSERT INTO big SELECT g, ((g::bigint*7919)%{n})::int, \
         repeat(chr(97+(g%26)),200) FROM generate_series(1,{n}) g"
    ))
    .unwrap();
    e
}

fn time(e: &mut Engine, sql: &str, reps: usize) -> f64 {
    e.execute(sql).unwrap();
    let t = Instant::now();
    for _ in 0..reps {
        e.execute(sql).unwrap();
    }
    t.elapsed().as_secs_f64() * 1000.0 / reps as f64
}

fn main() {
    let n: i64 = std::env::var("SPG_PROBE_ROWS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(400_000);
    let reps: usize = std::env::var("SPG_PROBE_REPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let only = std::env::var("SPG_PROBE_SHAPE").unwrap_or_else(|_| "all".into());
    let shapes: [(&str, &str); 3] = [
        ("distinct", "SELECT DISTINCT k FROM big ORDER BY k"),
        ("order", "SELECT k FROM big ORDER BY k"),
        ("groupby", "SELECT k FROM big GROUP BY k ORDER BY k"),
    ];
    let mut e = build(n);
    for (name, sql) in shapes {
        if only != "all" && only != name {
            continue;
        }
        let mut v: Vec<f64> = (0..5).map(|_| time(&mut e, sql, reps)).collect();
        v.sort_by(f64::total_cmp);
        println!(
            "{name:9} {:.1} ms  [{:.1}..{:.1}]",
            v[v.len() / 2],
            v[0],
            v[v.len() - 1]
        );
    }
}
