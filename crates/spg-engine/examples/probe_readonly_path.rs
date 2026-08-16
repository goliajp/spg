use spg_engine::{Engine, QueryResult};
use std::time::Instant;
fn main() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, b BYTEA)")
        .unwrap();
    e.execute("INSERT INTO t SELECT g, decode(lpad(to_hex(g),16,'0'),'hex') FROM generate_series(1,400000) g").unwrap();
    e.execute("CREATE INDEX t_b ON t (b)").unwrap();
    let sqls = [
        "SELECT count(*) FROM t WHERE b = decode(lpad(to_hex(7), 16, '0'), 'hex')",
        "SELECT count(*) FROM t WHERE id = 7::int",
        "SELECT count(*) FROM t WHERE id = 7",
    ];
    for sql in sqls {
        // the &mut path
        e.execute(sql).unwrap();
        let t = Instant::now();
        for _ in 0..5 {
            e.execute(sql).unwrap();
        }
        let mutable = t.elapsed().as_secs_f64() * 1000.0 / 5.0;
        // the read-only path the wire takes for an autocommit SELECT
        e.execute_readonly(sql).unwrap();
        let t = Instant::now();
        for _ in 0..5 {
            match e.execute_readonly(sql) {
                Ok(QueryResult::Rows { .. }) => {}
                other => panic!("{other:?}"),
            }
        }
        let ro = t.elapsed().as_secs_f64() * 1000.0 / 5.0;
        println!("  &mut {mutable:>9.3} ms   readonly {ro:>9.3} ms   {sql}");
    }
}
