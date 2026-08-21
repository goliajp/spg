//! Does UNIQUE on an expression still rescan the table per insert?
//!
//! Before v7.38.16 it did: the index held the leading column's values, so
//! the only way to know whether `lower(email)` repeated was to read every
//! row. Cost measured then: 0.43 ms per insert at 2,000 rows, 3.9 ms at
//! 20,000 — growing with the table.
use spg_engine::Engine;

fn run(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn arm(rows: usize, ddl: &str, label: &str) -> f64 {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE t (k INT, s TEXT)");
    run(&mut e, ddl);
    for i in 0..rows {
        run(&mut e, &format!("INSERT INTO t VALUES ({i}, 'X{i}')"));
    }
    let t = std::time::Instant::now();
    for i in rows..rows + 100 {
        run(&mut e, &format!("INSERT INTO t VALUES ({i}, 'X{i}')"));
    }
    let us = t.elapsed().as_secs_f64() * 1e6 / 100.0;
    println!("  {label:<34} {us:9.2} us/insert");
    us
}

fn main() {
    for rows in [2_000usize, 20_000] {
        println!("rows={rows}");
        arm(
            rows,
            "CREATE UNIQUE INDEX ux ON t (lower(s))",
            "UNIQUE on lower(s)",
        );
        arm(
            rows,
            "CREATE UNIQUE INDEX ux ON t (s)",
            "UNIQUE on s (reference)",
        );
        println!();
    }
    // It must still REJECT.
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE u (k INT, s TEXT)");
    run(&mut e, "CREATE UNIQUE INDEX ux ON u (lower(s))");
    run(&mut e, "INSERT INTO u VALUES (1, 'Alpha')");
    match e.execute("INSERT INTO u VALUES (2, 'ALPHA')") {
        Err(err) => println!("duplicate rejected: {err:?}"),
        Ok(_) => panic!("DUPLICATE ACCEPTED — uniqueness lost"),
    }
    run(&mut e, "INSERT INTO u VALUES (3, 'beta')");
    run(&mut e, "INSERT INTO u VALUES (4, NULL)");
    run(&mut e, "INSERT INTO u VALUES (5, NULL)");
    println!("distinct values and two NULLs accepted");
}
