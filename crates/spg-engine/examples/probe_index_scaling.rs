//! v7.38.14 Phase A — EXPLAIN is a known unreliable witness here (the
//! audit records fast-count being displayed as Seq Scan). So ask the
//! clock instead: if an index is used, time is flat in table size; if it
//! is not, time is linear. 10x the rows is the discriminator.
use spg_engine::Engine;
use std::time::Instant;

fn build(n: i64) -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, k INT, s TEXT, j JSONB)")
        .unwrap();
    e.execute(&format!(
        "INSERT INTO t SELECT g, g, 'x' || g::text, ('{{\"k\":' || g::text || '}}')::jsonb \
         FROM generate_series(1,{n}) g"
    ))
    .unwrap();
    for ddl in [
        "CREATE INDEX idx_col  ON t (k)",
        "CREATE INDEX idx_expr ON t (lower(s))",
        "CREATE INDEX idx_gin  ON t USING gin (j)",
    ] {
        e.execute(ddl).unwrap();
    }
    e.execute("ANALYZE t").ok();
    e
}

fn ms(e: &mut Engine, sql: &str) -> f64 {
    e.execute(sql).unwrap();
    let t = Instant::now();
    for _ in 0..20 {
        e.execute(sql).unwrap();
    }
    t.elapsed().as_secs_f64() * 1000.0 / 20.0
}

fn main() {
    for n in [10_000i64, 100_000] {
        let mut e = build(n);
        println!(
            "rows={n:<8} btree-col {:.3} ms   btree-expr {:.3} ms   gin {:.3} ms   seqscan-control {:.3} ms",
            ms(&mut e, "SELECT count(*) FROM t WHERE k = 42"),
            ms(&mut e, "SELECT count(*) FROM t WHERE lower(s) = 'x42'"),
            ms(&mut e, "SELECT count(*) FROM t WHERE j @> '{\"k\":42}'"),
            ms(&mut e, "SELECT count(*) FROM t WHERE k + 0 = 42"),
        );
    }
}
