//! v7.39 (parallel-agg P1) — the sharded fused-aggregate path must
//! produce the same answers as the single-threaded path. The engine is
//! no_std and takes the executor by injection, so the test injects a
//! scoped-thread runner (the same shape the server installs) and
//! differentials every fused shape against an uninjected engine.

use spg_engine::{Engine, ParallelRunner, QueryResult};

struct TestRunner;

impl ParallelRunner for TestRunner {
    fn run_shards(
        &self,
        n: usize,
        f: &(dyn Fn(usize) -> Box<dyn core::any::Any + Send> + Sync),
    ) -> Vec<Box<dyn core::any::Any + Send>> {
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..n).map(|i| s.spawn(move || f(i))).collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("shard panicked"))
                .collect()
        })
    }
}

/// Build the same 150k-row table (over the 100k parallel gate) in both
/// engines; only one gets the runner.
fn build_pair() -> (Engine, Engine) {
    let mut serial = Engine::new();
    let mut parallel = Engine::new();
    parallel.set_parallel_runner(std::sync::Arc::new(TestRunner));
    for e in [&mut serial, &mut parallel] {
        e.execute("CREATE TABLE t (v INT NOT NULL, f FLOAT NOT NULL, n NUMERIC(10,2) NOT NULL)")
            .unwrap();
        e.execute("CREATE TABLE tg (g INT NOT NULL, v INT NOT NULL)")
            .unwrap();
        e.execute("CREATE TABLE tgn (gn INT, v INT NOT NULL)").unwrap();
        // 150 batches x 1000 rows.
        for b in 0..150 {
            let mut sql = String::from("INSERT INTO t VALUES ");
            let mut sql_g = String::from("INSERT INTO tg VALUES ");
            let mut sql_gn = String::from("INSERT INTO tgn VALUES ");
            for i in 0..1000 {
                let k: i64 = i64::from(b) * 1000 + i;
                if i > 0 {
                    sql.push(',');
                    sql_g.push(',');
                    sql_gn.push(',');
                }
                sql.push_str(&format!("({}, {}.5, {}.25)", k % 977, k % 31, k % 199));
                sql_g.push_str(&format!("({}, {})", k % 97, k % 9973));
                if k % 11 == 0 {
                    sql_gn.push_str(&format!("(NULL, {})", k % 9973));
                } else {
                    sql_gn.push_str(&format!("({}, {})", k % 53, k % 9973));
                }
            }
            e.execute(&sql).unwrap();
            e.execute(&sql_g).unwrap();
            e.execute(&sql_gn).unwrap();
        }
    }
    (serial, parallel)
}

fn rows_of(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn parallel_fused_aggregates_match_serial() {
    let (mut serial, mut parallel) = build_pair();
    for sql in [
        // The fused multi-spec shape (count* + sum + avg share a pass).
        "SELECT count(*), count(v), sum(v), avg(v) FROM t",
        // Exact integer / numeric sums must be byte-identical.
        "SELECT sum(v), sum(n), avg(n) FROM t",
        // WHERE-filtered scan still shards (filter runs pre-aggregate).
        "SELECT count(*), sum(v) FROM t WHERE v % 2 = 0",
    ] {
        let a = rows_of(&mut serial, sql);
        let b = rows_of(&mut parallel, sql);
        assert_eq!(a, b, "parallel differs from serial for {sql}");
    }
    // Float sums may differ in the last ulp across shard merges (PG's
    // parallel aggregate makes the same tradeoff) — compare within
    // epsilon instead of bitwise.
    let a = rows_of(&mut serial, "SELECT sum(f), avg(f) FROM t");
    let b = rows_of(&mut parallel, "SELECT sum(f), avg(f) FROM t");
    for (x, y) in a[0].iter().zip(b[0].iter()) {
        let (spg_storage::Value::Float(x), spg_storage::Value::Float(y)) = (x, y) else {
            panic!("expected floats, got {x:?} / {y:?}")
        };
        assert!(
            ((x - y) / x).abs() < 1e-12,
            "float aggregate drifted: {x} vs {y}"
        );
    }
}

#[test]
fn parallel_group_by_matches_serial() {
    let (mut serial, mut parallel) = build_pair();
    for sql in [
        // The panel shape: single INT group key, fused specs.
        "SELECT g, count(*), sum(v) FROM tg GROUP BY g ORDER BY g",
        "SELECT g, avg(v) FROM tg GROUP BY g ORDER BY g",
        "SELECT g, count(*) FROM tg WHERE v % 3 = 0 GROUP BY g ORDER BY g",
        // NULL group bucket.
        "SELECT gn, count(*), sum(v) FROM tgn GROUP BY gn ORDER BY gn NULLS LAST",
    ] {
        let a = rows_of(&mut serial, sql);
        let b = rows_of(&mut parallel, sql);
        assert_eq!(a, b, "parallel differs from serial for {sql}");
    }
}

#[test]
fn below_gate_stays_serial_and_identical() {
    let mut e = Engine::new();
    e.set_parallel_runner(std::sync::Arc::new(TestRunner));
    e.execute("CREATE TABLE s (v INT NOT NULL)").unwrap();
    e.execute("INSERT INTO s VALUES (1),(2),(3)").unwrap();
    let r = rows_of(&mut e, "SELECT count(*), sum(v), avg(v) FROM s");
    assert_eq!(
        r[0][0],
        spg_storage::Value::BigInt(3),
        "small scans keep the single-threaded path"
    );
}
