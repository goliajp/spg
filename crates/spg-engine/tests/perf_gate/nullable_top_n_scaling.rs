//! v7.39.11 — `ORDER BY <nullable indexed column> LIMIT n` must not
//! scale with the table.
//!
//! When the walk gate learned to accept a LIMIT this version, it
//! started serving a shape it had always declined — and brought with it
//! a pass it did not need. A NULL key is not in the B-tree, so the rows
//! carrying one are found by walking the whole heap; that is the price
//! r1046 measured and accepted for an UNBOUNDED order. With a LIMIT the
//! walk has usually already produced every row the caller asked for,
//! and scanning the table to add none of them is then the entire cost
//! of the query.
//!
//! The release sweep caught it, on the cell it was built for:
//!
//! ```text
//!   SELECT pad FROM t ORDER BY n LIMIT 10     us        PostgreSQL 18
//!     50,000 rows                           0.237 ms      0.155 ms
//!    400,000 rows                           2.251 ms      0.182 ms
//! ```
//!
//! Ours grew with the table and PostgreSQL's did not, which is the
//! difference between scanning and walking. In-process, the same shape
//! measured 0.065 ms and 1.816 ms with the pass and 0.003 and 0.004
//! without it.
//!
//! This gate asserts the SHAPE rather than a duration: the 400,000-row
//! query may not cost more than five times the 50,000-row one. A walk
//! is flat (measured 1.3x, and the ratio is dominated by the ten rows
//! it returns either way) and the scan is linear (measured 28x), so
//! five separates them with an order of magnitude of margin in both
//! directions and does not care what the machine is doing.

use std::time::Instant;

use spg_engine::{CancelToken, Engine, StreamItem};

use super::perf_lock;

const SMALL: usize = 50_000;
const LARGE: usize = 400_000;
/// Walking measured 1.3x and scanning 28x. Anything under this is a
/// walk on any machine; anything over it is not a walk on any machine.
const MAX_RATIO: f64 = 5.0;

const SQL: &str = "SELECT pad FROM t ORDER BY n LIMIT 10";

/// The sweep's own typed table: `n` is NUMERIC and NULLABLE, which is
/// what makes the NULL pass run at all, and indexed, which is what
/// makes the walk available.
fn build(rows: usize) -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, n NUMERIC, pad TEXT)")
        .unwrap();
    let mut vals = String::with_capacity(rows * 40);
    for i in 0..rows {
        if i > 0 {
            vals.push(',');
        }
        vals.push_str(&format!("({i},{}.5,'xxxxxxxxxxxxxxxx')", i % 997));
    }
    e.execute(&format!("INSERT INTO t VALUES {vals}")).unwrap();
    e.execute("CREATE INDEX t_n ON t (n)").unwrap();
    e
}

/// Best of nine, over the streaming route a wire client takes.
fn best_ms(e: &Engine) -> f64 {
    for _ in 0..3 {
        let _ =
            e.execute_readonly_select_streaming(SQL, CancelToken::none(), |_: StreamItem| Ok(()));
    }
    let mut best = f64::MAX;
    for _ in 0..9 {
        let t0 = Instant::now();
        let n = e
            .execute_readonly_select_streaming(SQL, CancelToken::none(), |_: StreamItem| Ok(()))
            .expect("streaming select");
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(n, 10, "the query must return its ten rows");
        best = best.min(ms);
    }
    best
}

#[test]
fn a_bounded_order_over_a_nullable_indexed_column_does_not_scale() {
    let _g = perf_lock();
    let small = best_ms(&build(SMALL));
    let large = best_ms(&build(LARGE));
    let ratio = large / small;
    println!(
        "nullable top-N: {SMALL} rows {small:.3} ms, {LARGE} rows {large:.3} ms — {ratio:.1}x"
    );
    assert!(
        ratio <= MAX_RATIO,
        "{LARGE} rows cost {ratio:.1}x the {SMALL}-row query ({small:.3} ms -> {large:.3} ms). \
         A walk is flat; this is scanning the table to find NULL-keyed rows the LIMIT \
         does not need."
    );
}
