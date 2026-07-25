//! P0-13 — is a range DELETE doing a full scan? (yes)
//!
//! Run: cargo run --release -p spg-bench-competitor --bin del_range_scaling
//!
//! `DELETE FROM wb WHERE id >= a AND id < a+1000` on the PK costs SPGS
//! 4.967 ms against PG18's 0.292 ms — 17x, and the whole of
//! `delete_reinsert_1k`'s loss. If the predicate is not reaching the index,
//! the cost scales with the TABLE, not with the 1000 rows removed. Embedded,
//! so the wire is out of it.
use spg_engine::Engine;
use std::time::Instant;

fn batch_sql(base: i64, rows: i64) -> String {
    let mut s = String::with_capacity(rows as usize * 24 + 32);
    s.push_str("INSERT INTO wb VALUES ");
    for k in 0..rows {
        let id = base + k;
        if k > 0 {
            s.push(',');
        }
        s.push_str(&format!("({id},{},{})", id % 100, id * 7 % 100_000));
    }
    s
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn run(total: i64) -> (f64, f64, f64) {
    let mut e = Engine::new();
    e.execute("CREATE TABLE wb(id INT PRIMARY KEY, g INT, v INT)")
        .unwrap();
    for chunk in 0..(total / 1000) {
        e.execute(&batch_sql(chunk * 1000, 1000)).unwrap();
    }
    let seg = total / 2;
    let del = format!("DELETE FROM wb WHERE id >= {seg} AND id < {}", seg + 1000);
    let ins = batch_sql(seg, 1000);
    let eq = format!("DELETE FROM wb WHERE id = {}", seg + 500);
    let eq_ins = format!("INSERT INTO wb VALUES ({},1,1)", seg + 500);
    // A RANGE predicate matching exactly one row: separates "does the range
    // seek fire" from "does applying N deletes cost O(table)".
    let r1 = format!(
        "DELETE FROM wb WHERE id >= {} AND id < {}",
        seg + 700,
        seg + 701
    );
    let r1_ins = format!("INSERT INTO wb VALUES ({},1,1)", seg + 700);
    for _ in 0..3 {
        e.execute(&del).unwrap();
        e.execute(&ins).unwrap();
    }
    let mut rv = Vec::new();
    let mut ev = Vec::new();
    let mut r1v = Vec::new();
    for _ in 0..21 {
        let t = Instant::now();
        e.execute(&del).unwrap();
        rv.push(t.elapsed().as_secs_f64() * 1000.0);
        e.execute(&ins).unwrap();
        let t = Instant::now();
        e.execute(&eq).unwrap();
        ev.push(t.elapsed().as_secs_f64() * 1000.0);
        e.execute(&eq_ins).unwrap();
        let t = Instant::now();
        e.execute(&r1).unwrap();
        r1v.push(t.elapsed().as_secs_f64() * 1000.0);
        e.execute(&r1_ins).unwrap();
    }
    (median(rv), median(ev), median(r1v))
}

fn main() {
    println!("# DELETE cost vs table size (embedded), median of 21");
    println!("| table rows | range DEL 1000 | range DEL 1 | equality DEL 1 |");
    println!("|-----------:|---------------:|------------:|---------------:|");
    for total in [10_000i64, 50_000, 200_000] {
        let (r, eq, r1) = run(total);
        println!("| {total:10} | {r:11.3} ms | {r1:8.3} ms | {eq:11.3} ms |");
    }
}
