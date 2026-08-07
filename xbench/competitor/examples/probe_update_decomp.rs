//! r168 — update_range compute-loss decomposition (counter-first):
//! isolate scan/predicate vs SET-eval vs row-write vs index-maintenance
//! components by differencing table variants on the same UPDATE shape.

use std::fmt::Write as _;
use std::time::Instant;

const N: i64 = 50_000;

fn seed(db: &mut spg_embedded::Database, table: &str, ddl: &str, extra_idx: bool) {
    db.execute(ddl).unwrap();
    if extra_idx {
        db.execute(&format!("CREATE INDEX {table}_v_idx ON {table} (v)"))
            .unwrap();
    }
    let mut i = 1;
    while i <= N {
        let rows = 1000.min(N - i + 1);
        let mut sql = String::with_capacity(rows as usize * 24 + 32);
        let _ = write!(sql, "INSERT INTO {table} VALUES ");
        for k in 0..rows {
            let id = i + k;
            if k > 0 {
                sql.push(',');
            }
            let _ = write!(
                sql,
                "({id}, {}, {})",
                id % 100,
                (id * 2_654_435_761) % 100_000
            );
        }
        db.execute(&sql).unwrap();
        i += 1000;
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn bench(db: &mut spg_embedded::Database, sql: &str) -> f64 {
    for _ in 0..2 {
        db.execute(sql).unwrap();
    }
    median(
        (0..11)
            .map(|_| {
                let t0 = Instant::now();
                db.execute(sql).unwrap();
                t0.elapsed().as_secs_f64() * 1000.0
            })
            .collect(),
    )
}

fn main() {
    let mut db = spg_embedded::Database::open_in_memory();

    // Variant A — the write_heavy shape: PK + v index.
    seed(
        &mut db,
        "wa",
        "CREATE TABLE wa (id INT PRIMARY KEY, g INT NOT NULL, v INT NOT NULL)",
        true,
    );
    // Variant B — PK only, no v index.
    seed(
        &mut db,
        "wb2",
        "CREATE TABLE wb2 (id INT PRIMARY KEY, g INT NOT NULL, v INT NOT NULL)",
        false,
    );
    // Variant C — bare table: no PK, no index at all.
    seed(
        &mut db,
        "wc",
        "CREATE TABLE wc (id INT NOT NULL, g INT NOT NULL, v INT NOT NULL)",
        false,
    );

    let upd = |t: &str| format!("UPDATE {t} SET g = g + 1 WHERE v BETWEEN 20000 AND 40000");
    println!(
        "scan-only  count BETWEEN     : {:8.3} ms",
        bench(
            &mut db,
            "SELECT count(*) FROM wa WHERE v BETWEEN 20000 AND 40000"
        )
    );
    println!(
        "A pk+vidx  update_range_20k  : {:8.3} ms",
        bench(&mut db, &upd("wa"))
    );
    println!(
        "B pk-only  update_range_20k  : {:8.3} ms",
        bench(&mut db, &upd("wb2"))
    );
    println!(
        "C bare     update_range_20k  : {:8.3} ms",
        bench(&mut db, &upd("wc"))
    );
    // SET-eval floor: same rows matched, assignment to a constant.
    println!(
        "C bare     SET g=0 same range: {:8.3} ms",
        bench(
            &mut db,
            "UPDATE wc SET g = 0 WHERE v BETWEEN 20000 AND 40000"
        )
    );
    // Narrow range for linearity.
    println!(
        "A pk+vidx  update 2k range   : {:8.3} ms",
        bench(
            &mut db,
            "UPDATE wa SET g = g + 1 WHERE v BETWEEN 20000 AND 22000"
        )
    );
}
