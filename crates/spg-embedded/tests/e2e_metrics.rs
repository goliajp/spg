//! v7.7.5 — Database::metrics() observability hook.

use spg_embedded::Database;

#[test]
fn metrics_in_memory_baseline() {
    let db = Database::open_in_memory();
    let m = db.metrics();
    assert_eq!(m.persistent, false);
    assert_eq!(m.hot_rows, 0);
    assert_eq!(m.hot_bytes, 0);
    assert_eq!(m.cold_segments, 0);
    assert_eq!(m.tables, 0);
    assert_eq!(m.wal_bytes, 0);
}

#[test]
fn metrics_tracks_inserts() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')").unwrap();
    let m = db.metrics();
    assert_eq!(m.tables, 1);
    assert_eq!(m.hot_rows, 3);
    assert!(m.hot_bytes > 0);
}

#[test]
fn metrics_persistent_flag_set_on_open_path() {
    let mut p = std::env::temp_dir();
    let nanos: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    p.push(format!("spg-embedded-metrics-{nanos}-{}.db", std::process::id()));
    let mut db = Database::open_path(&p).unwrap();
    db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    let m = db.metrics();
    assert!(m.persistent);
    assert!(m.wal_bytes > 0, "WAL should have at least one fsynced record");
    drop(db);
    let mut wal = p.clone();
    wal.set_extension("db.wal");
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(&wal);
}

#[test]
fn metrics_cold_segments_after_freeze() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)").unwrap();
    db.execute("CREATE INDEX t_pk ON t (id)").unwrap();
    for i in 0..40 {
        db.execute(&format!("INSERT INTO t VALUES ({i}, 'x')")).unwrap();
    }
    let before = db.metrics();
    assert_eq!(before.cold_segments, 0);
    db.freeze_oldest_to_cold("t", "t_pk", 20).unwrap();
    let after = db.metrics();
    assert_eq!(after.cold_segments, 1);
    // hot_rows shrank by the moved 20.
    assert_eq!(after.hot_rows, 20);
}
