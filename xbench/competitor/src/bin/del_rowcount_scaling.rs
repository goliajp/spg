//! P0-20 — how does a DELETE scale with the number of rows it removes?
//!
//! Round 456 removed an O(table) constant from the mutation path, and a
//! one-row range DELETE went 1.220 -> 0.003 ms at 200k rows. The panel's
//! shape deletes 1000 rows at a time, and over the wire that half is still
//! 15.15x PG18 (4.622 ms vs 0.305 ms) — about 4.6 us per row against PG's
//! 0.3 us. So what is left is per-row, not per-table.
use spg_engine::Engine;
use std::fmt::Write as _;
use std::time::Instant;

const TOTAL: i64 = 50_000;

fn batch_sql(base: i64, rows: i64) -> String {
    let mut s = String::with_capacity(rows as usize * 24 + 32);
    s.push_str("INSERT INTO wb VALUES ");
    for k in 0..rows {
        let id = base + k;
        if k > 0 {
            s.push(',');
        }
        let _ = write!(s, "({id},{},{})", id % 100, id * 7 % 100_000);
    }
    s
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    // The server turns in-place MVCC on (v7.37.15 default); a bare
    // `Engine::new()` does not. Round 456's engine numbers and the wire
    // panel's were therefore not the same code path — the engine deletes
    // 1000 rows in 0.184 ms while the same statement over the wire takes
    // 4.622 ms, and this is the first thing to rule out.
    let inplace = std::env::var("SPG_MVCC_INPLACE").is_ok_and(|v| v != "0");
    let mut e = Engine::new();
    e.set_mvcc_inplace(inplace);
    println!("# mvcc_inplace = {inplace}");
    e.execute("CREATE TABLE wb(id INT PRIMARY KEY, g INT, v INT)")
        .unwrap();
    for chunk in 0..(TOTAL / 1000) {
        e.execute(&batch_sql(chunk * 1000, 1000)).unwrap();
    }
    let seg = TOTAL / 2;
    println!("# DELETE of N rows from a {TOTAL}-row table (embedded), median of 15");
    println!("| rows deleted | total ms | per row µs |");
    println!("|-------------:|---------:|-----------:|");
    for n in [1i64, 10, 100, 1000] {
        let del = format!("DELETE FROM wb WHERE id >= {seg} AND id < {}", seg + n);
        let ins = batch_sql(seg, n);
        for _ in 0..3 {
            e.execute(&del).unwrap();
            e.execute(&ins).unwrap();
        }
        let mut v = Vec::with_capacity(15);
        for _ in 0..15 {
            let t = Instant::now();
            e.execute(&del).unwrap();
            v.push(t.elapsed().as_secs_f64() * 1000.0);
            e.execute(&ins).unwrap();
        }
        let ms = median(v);
        println!("| {n:12} | {ms:8.3} | {:10.2} |", ms * 1000.0 / n as f64);
    }
}
