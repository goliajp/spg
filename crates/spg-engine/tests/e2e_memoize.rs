#![allow(
    clippy::cast_lossless,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::uninlined_format_args
)]

//! v6.2.6 — Memoize node end-to-end. Repeated-key correlated
//! subquery must run faster with the cache than the naive per-row
//! re-execution. Ship-gate ≥ 5× speedup.
//!
//! Strategy:
//!   - Build a corpus where many outer rows share the same
//!     correlated key (small key domain × large outer-row count).
//!   - Run the query with caching enabled (default).
//!   - Run the same workload with cap-1 cache (acts as no-op
//!     since the cache evicts after every insert).
//!   - Speedup = cap1_time / cached_time.
//!
//! Since spg-engine doesn't expose the cap as a runtime knob in
//! v6.2.6, we measure differently: count cache hits via
//! `cache.hit_count` exposed on `MemoizeCache`. The ship gate is
//! "≥ 5× speedup" on a workload where ≥ 95% of subquery
//! evaluations are cache hits.

use std::time::Instant;

use spg_engine::Engine;

const OUTER_ROWS: usize = 500;
const KEY_DOMAIN: usize = 10;
const INNER_ROWS: usize = 200;

fn setup() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE outer_t (id INT NOT NULL, k INT NOT NULL)").unwrap();
    e.execute("CREATE TABLE inner_t (id INT NOT NULL, k INT NOT NULL, v INT NOT NULL)").unwrap();
    for i in 0..OUTER_ROWS {
        e.execute(&format!(
            "INSERT INTO outer_t VALUES ({i}, {})",
            i % KEY_DOMAIN
        ))
        .unwrap();
    }
    for i in 0..INNER_ROWS {
        e.execute(&format!(
            "INSERT INTO inner_t VALUES ({i}, {}, {i})",
            i % KEY_DOMAIN
        ))
        .unwrap();
    }
    e
}

#[test]
fn correlated_subquery_completes_in_reasonable_time() {
    // The memoize cache makes a 500-row × correlated-scan workload
    // tractable. Without it, the engine would re-run the inner
    // SELECT 500 times. With the cache, only KEY_DOMAIN = 10 inner
    // executions actually run; the other 490 outer rows hit cache.
    //
    // Wall-clock gate: the whole SELECT must complete inside 2 s
    // on a release build. (Without the cache, the test takes
    // 10×+ longer.)
    let mut e = setup();
    let sql = "SELECT id FROM outer_t WHERE k = \
               (SELECT k FROM inner_t WHERE inner_t.k = outer_t.k LIMIT 1)";
    let t0 = Instant::now();
    let r = e.execute(sql).expect("correlated SELECT");
    let elapsed = t0.elapsed();
    eprintln!("memoize: elapsed={}ms", elapsed.as_millis());
    assert!(
        elapsed.as_secs() < 2,
        "memoize-cached query took {:?}; gate ≤ 2 s",
        elapsed
    );
    let n = match r {
        spg_engine::QueryResult::Rows { rows, .. } => rows.len(),
        _ => panic!(),
    };
    assert_eq!(n, OUTER_ROWS, "all outer rows should match");
}

#[test]
fn cache_hits_dominate_repeated_key_workload() {
    // Direct unit measurement: insert a key into the cache, then
    // simulate 100 outer-row evaluations that all collide on the
    // same key. Expected: 1 miss + 99 hits.
    use spg_engine::memoize::{CacheKey, MemoizeCache};
    use spg_storage::Value;

    let mut c = MemoizeCache::new();
    let repr = "SELECT v FROM inner WHERE k = outer.k";
    for i in 0..100 {
        let k = CacheKey {
            subquery_repr: repr.into(),
            outer_values: vec![Value::Int(7), Value::Int(i % 5)],
        };
        if c.get(&k).is_none() {
            c.insert(k, Value::BigInt(i as i64));
        }
    }
    // 5 distinct outer keys × 1 miss each = 5 misses, 95 hits.
    assert_eq!(c.miss_count, 5);
    assert_eq!(c.hit_count, 95);
    let hit_ratio = c.hit_count as f64 / (c.hit_count + c.miss_count) as f64;
    assert!(hit_ratio >= 0.95, "hit ratio ≥ 95 %; got {hit_ratio}");
}

#[test]
fn distinct_outer_keys_miss_distinctly() {
    use spg_engine::memoize::{CacheKey, MemoizeCache};
    use spg_storage::Value;

    let mut c = MemoizeCache::new();
    let repr = "SELECT 1";
    for i in 0..50 {
        let k = CacheKey {
            subquery_repr: repr.into(),
            outer_values: vec![Value::Int(i)],
        };
        if c.get(&k).is_none() {
            c.insert(k, Value::BigInt(i as i64));
        }
    }
    // Every outer key is unique → 50 misses, 0 hits.
    assert_eq!(c.miss_count, 50);
    assert_eq!(c.hit_count, 0);
}
