use spg_engine::{Engine, QueryResult};
use std::time::Instant;
fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect::<Vec<_>>()
            .join(" | "),
        Ok(o) => format!("{o:?}"),
        Err(err) => format!("ERR {err}"),
    }
}
fn time(e: &mut Engine, sql: &str) -> f64 {
    e.execute(sql).unwrap();
    let t = Instant::now();
    for _ in 0..5 {
        e.execute(sql).unwrap();
    }
    t.elapsed().as_secs_f64() * 1000.0 / 5.0
}
fn main() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, b BYTEA)")
        .unwrap();
    e.execute("INSERT INTO t SELECT g, decode(lpad(to_hex(g),16,'0'),'hex') FROM generate_series(1,400000) g").unwrap();
    e.execute("CREATE INDEX t_b ON t (b)").unwrap();
    for sql in [
        "SELECT count(*) FROM t WHERE b = decode(lpad(to_hex(7), 16, '0'), 'hex')",
        "SELECT count(*) FROM t WHERE b = '\\x0000000000000007'",
    ] {
        println!("{:>9.3} ms   {}", time(&mut e, sql), sql);
    }
    println!(
        "plan: {}",
        one(
            &mut e,
            "EXPLAIN SELECT count(*) FROM t WHERE b = decode(lpad(to_hex(7), 16, '0'), 'hex')"
        )
    );
    println!(
        "prepared: {}",
        e.prepare("SELECT count(*) FROM t WHERE b = decode(lpad(to_hex(7), 16, '0'), 'hex')")
            .map(|s| format!("{s}"))
            .unwrap()
    );
}
