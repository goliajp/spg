//! Ground-truth: does the sharded aggregate path fire through the
//! embedded stack, and what does it cost vs the serial path?
use std::time::Instant;

fn main() {
    let mut db = spg_embedded::Database::open_in_memory();
    // Mirror heavy.rs's h5 exactly: 3 columns + the v index.
    db.execute("CREATE TABLE h5 (id INT NOT NULL, g INT NOT NULL, v INT NOT NULL)")
        .unwrap();
    db.execute("CREATE INDEX h5_v_idx ON h5 (v)").unwrap();
    for b in 0..500 {
        let mut sql = String::from("INSERT INTO h5 VALUES ");
        for i in 0..1000 {
            let k: i64 = i64::from(b) * 1000 + i;
            if i > 0 { sql.push(','); }
            sql.push_str(&format!("({k}, {}, {})", k % 100, k % 9973));
        }
        db.execute(&sql).unwrap();
    }
    let sql = std::env::var("PROBE_SQL")
        .unwrap_or_else(|_| "SELECT count(*), sum(v), avg(v) FROM h5".into());
    let sql = sql.as_str();
    // warmup
    for _ in 0..3 { db.execute(sql).unwrap(); }
    let before = spg_engine::PARALLEL_AGG_FIRED.load(std::sync::atomic::Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..31 { db.execute(sql).unwrap(); }
    let elapsed = t.elapsed();
    let after = spg_engine::PARALLEL_AGG_FIRED.load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "parallel_fired={} runs=31 total={:?} per-run={:?}",
        after - before,
        elapsed,
        elapsed / 31
    );
}
