//! P0-19 — a workload that does nothing but range-predicated DELETEs, so a
//! profiler's self-time is the answer rather than a haystack.
//!
//! Nine candidates have been counted and set aside on this shape (engine,
//! commit queue, WAL preallocation, the fsync syscall, churn, AST clone, the
//! commit path, the index seek, and — since round 455 wired the counter — a
//! sequential scan). The cost is real and scales with the table: a DELETE
//! whose range predicate matches ONE row costs 0.024 ms at 10k rows and
//! 1.220 ms at 200k, while the same delete by equality stays at 0.006 ms.
//!
//! Run under samply:
//!   samply record -- ./target/release/range_del_profile
use spg_engine::Engine;
use std::fmt::Write as _;

const TOTAL: i64 = 200_000;
const ITERS: usize = 3_000;

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

fn main() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE wb(id INT PRIMARY KEY, g INT, v INT)")
        .unwrap();
    for chunk in 0..(TOTAL / 1000) {
        e.execute(&batch_sql(chunk * 1000, 1000)).unwrap();
    }
    let seg = TOTAL / 2;
    let del = format!("DELETE FROM wb WHERE id >= {seg} AND id < {}", seg + 1);
    let ins = format!("INSERT INTO wb VALUES ({seg},1,1)");
    eprintln!("seeded; profiling {ITERS} range DELETEs");
    for _ in 0..ITERS {
        e.execute(&del).unwrap();
        e.execute(&ins).unwrap();
    }
    eprintln!("done");
}
