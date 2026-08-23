//! v7.39 (read01 round 171) — `SET synchronous_commit = off` is a real
//! session-level durability control on the embedded durable path (A4).
//! The group-commit machinery and the process-level
//! SPG_SYNCHRONOUS_COMMIT env existed since v7.20; the SQL GUC was
//! stored/SHOWable but silently ignored by the execute durability wait,
//! so a PG application's `SET synchronous_commit = off` changed
//! nothing. Now: off skips the per-statement fsync wait (the record is
//! still enqueued; the next synchronous commit / checkpoint / clean
//! shutdown flushes it — PG's documented trade), and data survives a
//! clean close+reopen in either mode.

use spg_embedded::Database;

fn tmp() -> std::path::PathBuf {
    let p = std::env::temp_dir().join("spg-tests").join(format!(
        "spg-sync-commit-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn set_off_accepted_and_data_survives_clean_close() {
    let dir = tmp();
    let path = dir.join("d.db");
    {
        let mut db = Database::open_path(&path).unwrap();
        db.execute("CREATE TABLE t(id INT PRIMARY KEY, v INT NOT NULL)")
            .unwrap();
        db.execute("SET synchronous_commit = off").unwrap();
        for i in 0..200 {
            db.execute(&format!("INSERT INTO t VALUES ({i}, {i})"))
                .unwrap();
        }
        // Flip back on mid-session: the next write waits again.
        db.execute("SET synchronous_commit = on").unwrap();
        db.execute("INSERT INTO t VALUES (1000, 1)").unwrap();
    } // Drop = clean shutdown flush.
    {
        let mut db = Database::open_path(&path).unwrap();
        let r = db.execute("SELECT count(*) FROM t").unwrap();
        match r {
            spg_engine::QueryResult::Rows { rows, .. } => {
                assert_eq!(rows[0].values[0], spg_storage::Value::BigInt(201));
            }
            other => panic!("{other:?}"),
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn invalid_value_rejected() {
    let mut db = Database::open_in_memory();
    assert!(db.execute("SET synchronous_commit = sometimes").is_err());
    db.execute("SET synchronous_commit = local").unwrap(); // waits locally, valid
    db.execute("SET synchronous_commit = remote_apply").unwrap();
}
