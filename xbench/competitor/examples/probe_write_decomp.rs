//! r167 write-path decomposition micro-probe (counter-first):
//! split the residual write-shape costs into parse / pure-execute /
//! WAL+fsync components by differencing in-memory vs durable runs.

use std::fmt::Write as _;
use std::time::Instant;

const N: i64 = 50_000;

fn batch_sql(base: i64, rows: i64) -> String {
    let mut sql = String::with_capacity(rows as usize * 24 + 32);
    sql.push_str("INSERT INTO wb VALUES ");
    for k in 0..rows {
        let id = base + k;
        if k > 0 {
            sql.push(',');
        }
        let _ = write!(sql, "({id}, {}, {})", id % 100, (id * 2_654_435_761) % 100_000);
    }
    sql
}

fn seed(db: &mut spg_embedded::Database) {
    db.execute("CREATE TABLE wb (id INT PRIMARY KEY, g INT NOT NULL, v INT NOT NULL)")
        .unwrap();
    db.execute("CREATE INDEX wb_v_idx ON wb (v)").unwrap();
    let mut i = 1;
    while i <= N {
        db.execute(&batch_sql(i, 1000.min(N - i + 1))).unwrap();
        i += 1000;
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn time_ms(mut f: impl FnMut()) -> f64 {
    let t0 = Instant::now();
    f();
    t0.elapsed().as_secs_f64() * 1000.0
}

fn main() {
    // ---- parse-only: the 1000-row VALUES text ----
    let sql = batch_sql(10_000_000, 1000);
    let parse_ms = median(
        (0..11)
            .map(|_| time_ms(|| {
                let _ = spg_sql::parser::parse_statement(&sql).unwrap();
            }))
            .collect(),
    );
    println!("parse_1k_values      : {parse_ms:8.3} ms");

    // ---- in-memory engine (no WAL, no fsync) ----
    let mut mem = spg_embedded::Database::open_in_memory();
    seed(&mut mem);
    let mut base = 10_000_000_i64;
    let mem_batch = median(
        (0..11)
            .map(|_| {
                let sql = batch_sql(base, 1000);
                let ms = time_ms(|| {
                    mem.execute(&sql).unwrap();
                });
                mem.execute(&format!("DELETE FROM wb WHERE id >= {base} AND id < {}", base + 1000)).unwrap();
                base += 10_000;
                ms
            })
            .collect(),
    );
    println!("mem_insert_batch_1k  : {mem_batch:8.3} ms");
    let mem_update_range = median(
        (0..11)
            .map(|_| time_ms(|| {
                mem.execute("UPDATE wb SET g = g + 1 WHERE v BETWEEN 20000 AND 40000").unwrap();
            }))
            .collect(),
    );
    println!("mem_update_range_20k : {mem_update_range:8.3} ms");
    let mut lo = 1_i64;
    let mem_delete_1k = median(
        (0..11)
            .map(|_| {
                let del = format!("DELETE FROM wb WHERE id >= {lo} AND id < {}", lo + 1000);
                let ms = time_ms(|| {
                    mem.execute(&del).unwrap();
                });
                mem.execute(&batch_sql(lo, 1000)).unwrap();
                lo += 1000;
                ms
            })
            .collect(),
    );
    println!("mem_delete_1k        : {mem_delete_1k:8.3} ms");

    // ---- durable engine (WAL + fsync) ----
    let dir = std::env::temp_dir().join(format!(
        "spg-probe-wd-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut dur = spg_embedded::Database::open_path(dir.join("p.db")).unwrap();
    seed(&mut dur);
    let mut base = 10_000_000_i64;
    let dur_batch = median(
        (0..11)
            .map(|_| {
                let sql = batch_sql(base, 1000);
                let ms = time_ms(|| {
                    dur.execute(&sql).unwrap();
                });
                dur.execute(&format!("DELETE FROM wb WHERE id >= {base} AND id < {}", base + 1000)).unwrap();
                base += 10_000;
                ms
            })
            .collect(),
    );
    println!("dur_insert_batch_1k  : {dur_batch:8.3} ms  (fsync component ≈ {:.3} ms)", dur_batch - mem_batch);
    // single-statement no-op-ish write for the pure fsync floor
    let mut k = 20_000_000_i64;
    let dur_single = median(
        (0..11)
            .map(|_| {
                let ins = format!("INSERT INTO wb VALUES ({k}, 0, 0)");
                let ms = time_ms(|| {
                    dur.execute(&ins).unwrap();
                });
                k += 1;
                ms
            })
            .collect(),
    );
    println!("dur_single_insert    : {dur_single:8.3} ms  (durability floor per stmt)");
    drop(dur);
    let _ = std::fs::remove_dir_all(&dir);
}
