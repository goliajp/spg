use std::time::Instant;
fn main() {
    let dir = std::env::temp_dir().join(format!(
        "spg-sc-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut db = spg_embedded::Database::open_path(dir.join("d.db")).unwrap();
    db.execute("CREATE TABLE t(id INT PRIMARY KEY, v INT NOT NULL)")
        .unwrap();
    for (label, set) in [
        ("sync=on ", "SET synchronous_commit = on"),
        ("sync=off", "SET synchronous_commit = off"),
    ] {
        db.execute(set).unwrap();
        let base = if label.contains("on") { 0 } else { 1_000_000 };
        let t0 = Instant::now();
        for i in 0..100 {
            db.execute(&format!("INSERT INTO t VALUES ({}, 0)", base + i))
                .unwrap();
        }
        println!(
            "{label}: 100 singles = {:7.2} ms ({:.2} ms/row)",
            t0.elapsed().as_secs_f64() * 1000.0,
            t0.elapsed().as_secs_f64() * 10.0
        );
    }
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
