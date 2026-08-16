//! r1040 — what does the exact NUMERIC sort key cost?
//!
//! The key went from a bare `f64` to a canonical decimal one, which owns
//! a digit vector. That is a heap allocation per key where there was
//! none, and `ORDER BY <numeric column>` builds one per row. Correctness
//! is not negotiable here — the f64 projection returned rows in the wrong
//! order — but the price has to be known rather than assumed.
//!
//! Ordered by an INT column in the same table as the control: it shares
//! every part of the path except the key, so a change that shows up in
//! both is the machine, not this.
//!
//!   cargo run --release --example probe_numeric_sort_cost

use spg_engine::Engine;
use std::time::Instant;

const ROWS: i64 = 200_000;

fn build() -> Engine {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE s (id INT PRIMARY KEY, n NUMERIC, k INT)")
        .unwrap();
    let mut values = String::new();
    for i in 0..ROWS {
        if !values.is_empty() {
            values.push(',');
        }
        // Two decimal places, so the keys are short but not degenerate.
        values.push_str(&format!("({i},{}.{:02},{i})", i / 7, i % 100));
        if values.len() > 60_000 || i == ROWS - 1 {
            eng.execute(&format!("INSERT INTO s (id, n, k) VALUES {values}"))
                .unwrap();
            values.clear();
        }
    }
    eng
}

fn time(eng: &mut Engine, sql: &str, reps: u32) -> f64 {
    eng.execute(sql).unwrap();
    let t = Instant::now();
    for _ in 0..reps {
        eng.execute(sql).unwrap();
    }
    t.elapsed().as_secs_f64() * 1000.0 / f64::from(reps)
}

fn main() {
    let mut eng = build();
    // Interleaved, and the order flipped between rounds, because the
    // machine drifts over the length of a run.
    let mut num = Vec::new();
    let mut int = Vec::new();
    for round in 0..6 {
        if round % 2 == 0 {
            num.push(time(&mut eng, "SELECT id FROM s ORDER BY n", 3));
            int.push(time(&mut eng, "SELECT id FROM s ORDER BY k", 3));
        } else {
            int.push(time(&mut eng, "SELECT id FROM s ORDER BY k", 3));
            num.push(time(&mut eng, "SELECT id FROM s ORDER BY n", 3));
        }
    }
    let med = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let spread = |v: &[f64]| {
        let (lo, hi) = v
            .iter()
            .fold((f64::MAX, f64::MIN), |(l, h), x| (l.min(*x), h.max(*x)));
        format!("{lo:.1}-{hi:.1}")
    };
    println!("{ROWS} rows, 6 interleaved rounds, 3 reps each");
    println!(
        "  ORDER BY n (numeric): median {:.1} ms   spread {}",
        med(num.clone()),
        spread(&num)
    );
    println!(
        "  ORDER BY k (int, control): median {:.1} ms   spread {}",
        med(int.clone()),
        spread(&int)
    );
}
