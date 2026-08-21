//! Right answers are not the same as a used index — a declined seek
//! gives right answers too. `idx_scan` counts seeks, and the scaling
//! says whether one happened.
use spg_engine::{Engine, QueryResult};

fn idx_scan(e: &mut Engine, t: &str) -> i64 {
    match e
        .execute(&format!(
            "SELECT idx_scan FROM pg_stat_user_tables WHERE relname = '{t}'"
        ))
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => match rows.first().map(|r| r.values[0].clone()) {
            Some(spg_storage::Value::BigInt(n)) => n,
            other => panic!("{other:?}"),
        },
        _ => unreachable!(),
    }
}

fn main() {
    for rows in [2_000usize, 20_000] {
        for with_index in [true, false] {
            let mut e = Engine::new();
            e.execute("CREATE TABLE d (id INT, title TEXT, body TEXT)")
                .unwrap();
            for i in 0..rows {
                e.execute(&format!(
                    "INSERT INTO d VALUES ({i}, 'title{i}', 'body number {i} filler words here')"
                ))
                .unwrap();
            }
            if with_index {
                e.execute(
                    "CREATE INDEX g ON d USING gin (to_tsvector('english', title || ' ' || body))",
                )
                .unwrap();
            }
            let q = "SELECT id FROM d WHERE to_tsvector('english', title || ' ' || body) @@ to_tsquery('english','title42')";
            let before = idx_scan(&mut e, "d");
            let t = std::time::Instant::now();
            for _ in 0..10 {
                e.execute(q).unwrap();
            }
            let ms = t.elapsed().as_secs_f64() * 1e3 / 10.0;
            let n = match e.execute(q).unwrap() {
                QueryResult::Rows { rows, .. } => rows.len(),
                _ => unreachable!(),
            };
            println!(
                "rows={rows:<6} index={with_index:<5} {ms:8.3} ms  {n} row  idx_scan {before} -> {}",
                idx_scan(&mut e, "d")
            );
        }
    }
}
