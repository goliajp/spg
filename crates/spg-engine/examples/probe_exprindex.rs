//! Does an expression index answer `lower(s) = …` now?
//!
//! Scaling, not EXPLAIN: SPG's EXPLAIN has named a Seq Scan while a GIN
//! index was doing the work. Ten times the rows on a seek is flat; on a
//! scan it is ten times the time.
use spg_engine::{Engine, QueryResult};

fn nrows(r: &QueryResult) -> usize {
    match r {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => panic!("not a row set"),
    }
}

fn run(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn arm(rows: usize, with_index: bool) -> (f64, usize) {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE t (k INT, s TEXT)");
    for i in 0..rows {
        run(&mut e, &format!("INSERT INTO t VALUES ({i}, 'X{i}')"));
    }
    if with_index {
        run(&mut e, "CREATE INDEX t_lower ON t (lower(s))");
    }
    let q = "SELECT k FROM t WHERE lower(s) = 'x42'";
    let n = nrows(&e.execute(q).unwrap());
    let t = std::time::Instant::now();
    for _ in 0..20 {
        e.execute(q).unwrap();
    }
    (t.elapsed().as_secs_f64() * 1e3 / 20.0, n)
}

fn main() {
    for rows in [10_000usize, 100_000] {
        let (with, nw) = arm(rows, true);
        let (without, nn) = arm(rows, false);
        println!(
            "rows={rows:<7} with index {with:7.3} ms ({nw} row)   without {without:7.3} ms ({nn} row)   ratio {:.2}x",
            with / without
        );
        assert_eq!(nw, nn, "the index changed the ANSWER");
    }
    // The index must survive inserts made after it was built.
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE u (k INT, s TEXT)");
    run(&mut e, "INSERT INTO u VALUES (1, 'Alpha')");
    run(&mut e, "CREATE INDEX u_lower ON u (lower(s))");
    run(&mut e, "INSERT INTO u VALUES (2, 'ALPHA'), (3, 'beta')");
    let q = "SELECT k FROM u WHERE lower(s) = 'alpha' ORDER BY k";
    println!(
        "after INSERT  -> {} rows (want 2)",
        nrows(&e.execute(q).unwrap())
    );
    run(&mut e, "UPDATE u SET s = 'ALPHA' WHERE k = 3");
    println!(
        "after UPDATE  -> {} rows (want 3)",
        nrows(&e.execute(q).unwrap())
    );
    run(&mut e, "DELETE FROM u WHERE k = 1");
    println!(
        "after DELETE  -> {} rows (want 2)",
        nrows(&e.execute(q).unwrap())
    );

    // And the index must still be doing the work after all of that:
    // rebuild a big table, update one row, and check the seek is flat.
    for rows in [10_000usize, 100_000] {
        let mut e = Engine::new();
        run(&mut e, "CREATE TABLE w (k INT, s TEXT)");
        for i in 0..rows {
            run(&mut e, &format!("INSERT INTO w VALUES ({i}, 'X{i}')"));
        }
        run(&mut e, "CREATE INDEX w_lower ON w (lower(s))");
        run(&mut e, "UPDATE w SET s = 'zz' WHERE k = 0");
        run(&mut e, "DELETE FROM w WHERE k = 1");
        let q = "SELECT k FROM w WHERE lower(s) = 'x42'";
        let t = std::time::Instant::now();
        for _ in 0..20 {
            e.execute(q).unwrap();
        }
        println!(
            "rows={rows:<7} after UPDATE+DELETE, seek {:.3} ms ({} row)",
            t.elapsed().as_secs_f64() * 1e3 / 20.0,
            nrows(&e.execute(q).unwrap())
        );
    }
}
