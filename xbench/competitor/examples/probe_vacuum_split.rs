//! r170 — split VACUUM cost: physical compact vs per-index rebuild,
//! by differencing tables with 0 / 1 / 2 indexes.
use std::fmt::Write as _;
use std::time::Instant;

const N: i64 = 50_000;

fn seed(db: &mut spg_embedded::Database, t: &str, ddl: &str, vidx: bool) {
    db.execute(ddl).unwrap();
    if vidx {
        db.execute(&format!("CREATE INDEX {t}_v ON {t} (v)"))
            .unwrap();
    }
    let mut i = 1;
    while i <= N {
        let rows = 1000.min(N - i + 1);
        let mut sql = String::new();
        let _ = write!(sql, "INSERT INTO {t} VALUES ");
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

fn vac_ms(db: &mut spg_embedded::Database, t: &str) -> f64 {
    db.execute(&format!(
        "UPDATE {t} SET g = g + 1 WHERE v BETWEEN 20000 AND 40000"
    ))
    .unwrap();
    let t0 = Instant::now();
    db.execute(&format!("VACUUM {t}")).unwrap();
    t0.elapsed().as_secs_f64() * 1000.0
}

fn main() {
    let mut db = spg_embedded::Database::open_in_memory();
    seed(
        &mut db,
        "t0",
        "CREATE TABLE t0 (id INT NOT NULL, g INT NOT NULL, v INT NOT NULL)",
        false,
    );
    seed(
        &mut db,
        "t1",
        "CREATE TABLE t1 (id INT PRIMARY KEY, g INT NOT NULL, v INT NOT NULL)",
        false,
    );
    seed(
        &mut db,
        "t2",
        "CREATE TABLE t2 (id INT PRIMARY KEY, g INT NOT NULL, v INT NOT NULL)",
        true,
    );
    for round in 0..3 {
        let a = vac_ms(&mut db, "t0");
        let b = vac_ms(&mut db, "t1");
        let c = vac_ms(&mut db, "t2");
        println!(
            "round {round}: 0-idx {a:7.3} ms | 1-idx {b:7.3} ms | 2-idx {c:7.3} ms  (per-index ≈ {:.3})",
            c - b
        );
    }
}
