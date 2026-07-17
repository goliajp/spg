//! r168b — is the update_range loss dominated by MVCC dead-row bloat?
//! Same shape run 11 consecutive times (write_heavy timing) vs the same
//! with a manual VACUUM between runs.

use std::fmt::Write as _;
use std::time::Instant;

const N: i64 = 50_000;

fn seed(db: &mut spg_embedded::Database) {
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
}

fn run(label: &str, vacuum: bool) {
    let mut db = spg_embedded::Database::open_in_memory();
    seed(&mut db);
    let upd = "UPDATE wa SET g = g + 1 WHERE v BETWEEN 20000 AND 40000";
    let mut samples = Vec::new();
    for i in 0..11 {
        let t0 = Instant::now();
        db.execute(upd).unwrap();
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        samples.push(ms);
        if vacuum {
            db.execute("VACUUM wa").unwrap();
        }
        if i == 0 || i == 5 || i == 10 {
            println!("  {label} run{i:2}: {ms:8.3} ms");
        }
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("  {label} median: {:8.3} ms", samples[5]);
}

fn main() {
    println!("consecutive (write_heavy timing):");
    run("bloat  ", false);
    println!("with VACUUM between runs:");
    run("vacuum ", true);
}
