//! Does an expression index cost anything to MAINTAIN?
//!
//! `ddl.rs:2586` says the engine "falls through to the bare-column-reference
//! path", which would mean the index IS built and maintained — on the wrong
//! keys. If so the write side pays for an index nothing can ever read.
//! Three arms, same inserts: no index / index on the column / index on
//! `lower(col)`.
use spg_engine::Engine;

fn run(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn arm(label: &str, ddl: Option<&str>, rows: usize) -> f64 {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE t (k INT, s TEXT)");
    if let Some(d) = ddl {
        run(&mut e, d);
    }
    for i in 0..rows {
        run(&mut e, &format!("INSERT INTO t VALUES ({i}, 'x{i}')"));
    }
    // time a further 200 inserts on the loaded table
    let t = std::time::Instant::now();
    for i in rows..rows + 200 {
        run(&mut e, &format!("INSERT INTO t VALUES ({i}, 'x{i}')"));
    }
    let us = t.elapsed().as_secs_f64() * 1e6 / 200.0;
    println!("  {label:<28} {us:8.2} us/insert");
    us
}

fn main() {
    for rows in [2_000usize, 20_000] {
        println!("rows={rows}");
        let none = arm("no index", None, rows);
        let col = arm("index on (s)", Some("CREATE INDEX i ON t (s)"), rows);
        let expr = arm(
            "index on (lower(s))",
            Some("CREATE INDEX i ON t (lower(s))"),
            rows,
        );
        println!(
            "  -> expr/none {:.2}x   expr/col {:.2}x\n",
            expr / none,
            expr / col
        );
    }
}
