//! r169 — where do the ~35ms of a statement-end autovacuum go?
use std::fmt::Write as _;
use std::time::Instant;

const N: i64 = 50_000;

fn main() {
    let mut db = spg_embedded::Database::open_in_memory();
    db.execute("CREATE TABLE wa (id INT PRIMARY KEY, g INT NOT NULL, v INT NOT NULL)").unwrap();
    db.execute("CREATE INDEX wa_v_idx ON wa (v)").unwrap();
    let mut i = 1;
    while i <= N {
        let rows = 1000.min(N - i + 1);
        let mut sql = String::from("INSERT INTO wa VALUES ");
        for k in 0..rows {
            let id = i + k;
            if k > 0 { sql.push(','); }
            let _ = write!(sql, "({id}, {}, {})", id % 100, (id * 2_654_435_761) % 100_000);
        }
        db.execute(&sql).unwrap();
        i += 1000;
    }
    for round in 0..4 {
        // create 20k dead rows (autovacuum disabled via env in the runner)
        db.execute("UPDATE wa SET g = g + 1 WHERE v BETWEEN 20000 AND 40000").unwrap();
        let t0 = Instant::now();
        db.execute("VACUUM wa").unwrap();
        println!("round {round}: VACUUM after 20k dead: {:.3} ms", t0.elapsed().as_secs_f64() * 1000.0);
    }
    // and a no-op vacuum (0 dead)
    let t0 = Instant::now();
    db.execute("VACUUM wa").unwrap();
    println!("no-op VACUUM (0 dead)   : {:.3} ms", t0.elapsed().as_secs_f64() * 1000.0);
}
