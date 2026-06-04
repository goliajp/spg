//! v7.7.6 — revert_wal_to_seq.

use spg_embedded::{Database, QueryResult, revert_wal_to_seq};
use std::path::PathBuf;

struct Scratch {
    path: PathBuf,
}
impl Scratch {
    fn new(label: &str) -> Self {
        let mut p = std::env::temp_dir();
        let nanos: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        p.push(format!(
            "spg-embedded-revert-{label}-{nanos}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self { path: p }
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn count(db: &mut Database, sql: &str) -> usize {
    match db.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => 0,
    }
}

#[test]
fn revert_to_seq_one_keeps_only_first_statement() {
    let scratch = Scratch::new("seq1");
    let dir = scratch.path.clone();
    let db_path = dir.join("app.db");
    let wal_path = {
        let mut p = db_path.clone();
        p.set_extension("db.wal");
        p
    };
    {
        let mut db = Database::open_path(&db_path).unwrap();
        // Disable auto-checkpoint so all four ops stay in the WAL.
        db.set_checkpoint_threshold_bytes(u64::MAX);
        db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        db.execute("INSERT INTO t VALUES (1)").unwrap();
        db.execute("INSERT INTO t VALUES (2)").unwrap();
        db.execute("INSERT INTO t VALUES (3)").unwrap();
        // Skip the Drop checkpoint so the WAL survives.
        std::mem::forget(db);
    }
    let out = dir.join("restored.db");
    // Apply only the first record (CREATE TABLE).
    let applied = revert_wal_to_seq(&wal_path, 1, &out).unwrap();
    assert_eq!(applied, 1);
    let mut db = Database::open_path(&out).unwrap();
    // Table exists, but no rows yet.
    assert_eq!(count(&mut db, "SELECT id FROM t"), 0);
}

#[test]
fn revert_to_seq_zero_yields_empty_catalog() {
    let scratch = Scratch::new("seq0");
    let dir = scratch.path.clone();
    let db_path = dir.join("app.db");
    let wal_path = {
        let mut p = db_path.clone();
        p.set_extension("db.wal");
        p
    };
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.set_checkpoint_threshold_bytes(u64::MAX);
        db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        db.execute("INSERT INTO t VALUES (1)").unwrap();
        std::mem::forget(db);
    }
    let out = dir.join("restored.db");
    let applied = revert_wal_to_seq(&wal_path, 0, &out).unwrap();
    assert_eq!(applied, 0);
    let mut db = Database::open_path(&out).unwrap();
    // Querying a nonexistent table should error.
    let r = db.execute("SELECT id FROM t");
    assert!(r.is_err());
}

#[test]
fn revert_to_seq_apply_all_when_budget_exceeds_records() {
    let scratch = Scratch::new("max");
    let dir = scratch.path.clone();
    let db_path = dir.join("app.db");
    let wal_path = {
        let mut p = db_path.clone();
        p.set_extension("db.wal");
        p
    };
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.set_checkpoint_threshold_bytes(u64::MAX);
        db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        for i in 0..5 {
            db.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
        }
        std::mem::forget(db);
    }
    let out = dir.join("restored.db");
    let applied = revert_wal_to_seq(&wal_path, 1_000, &out).unwrap();
    assert_eq!(applied, 6);
    let mut db = Database::open_path(&out).unwrap();
    assert_eq!(count(&mut db, "SELECT id FROM t"), 5);
}
