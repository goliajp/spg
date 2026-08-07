//! v7.39 (read01 round 180) — embedded RETURNING / writable-CTE
//! durability across a simulated crash.
//!
//! The server twin of this bug shipped in r178; the embedded gates
//! had the same two holes (and one of their own):
//!   * autocommit DML with RETURNING answers `Rows` — the
//!     CommandOk-only `modified` gate skipped its WAL record;
//!   * `sql_is_read_only` head-words `WITH` as a read, so a writable
//!     CTE never reached the WAL on the autocommit path NOR the
//!     in-tx statement buffer;
//!   * the prepared path shared the CommandOk-only gate.
//! Crash simulation: `mem::forget(db)` skips Drop's checkpoint, so
//! recovery must come from the WAL alone.

use spg_embedded::Database;
use std::path::PathBuf;

fn tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-ret-dur-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn count(db: &mut Database, sql: &str) -> usize {
    db.query(sql).unwrap().len()
}

#[test]
fn autocommit_returning_survives_crash() {
    let dir = tmpdir("ret");
    let db_path = dir.join("spg.db");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT NOT NULL)")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 0) RETURNING id")
            .unwrap();
        db.execute("INSERT INTO t VALUES (2, 0), (3, 0) RETURNING id")
            .unwrap();
        db.execute("UPDATE t SET v = 9 WHERE id <= 2 RETURNING id")
            .unwrap();
        db.execute("DELETE FROM t WHERE id = 3 RETURNING id")
            .unwrap();
        std::mem::forget(db);
    }
    Database::force_unlock(&db_path).unwrap();
    let mut db = Database::open_path(&db_path).unwrap();
    assert_eq!(count(&mut db, "SELECT id FROM t"), 2, "rows 1,2 survive");
    assert_eq!(
        count(&mut db, "SELECT id FROM t WHERE v = 9"),
        2,
        "RETURNING UPDATE must replay"
    );
}

#[test]
fn writable_cte_survives_crash_autocommit_and_tx() {
    let dir = tmpdir("cte");
    let db_path = dir.join("spg.db");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT NOT NULL)")
            .unwrap();
        // Autocommit writable CTE, with and without RETURNING.
        db.execute("WITH s AS (SELECT 1 AS x) INSERT INTO t SELECT x, 0 FROM s")
            .unwrap();
        db.execute("WITH s AS (SELECT 2 AS x) INSERT INTO t SELECT x, 0 FROM s RETURNING id")
            .unwrap();
        // In-tx writable CTE — must land in the tx WAL buffer.
        db.execute("BEGIN").unwrap();
        db.execute("WITH s AS (SELECT 3 AS x) INSERT INTO t SELECT x, 0 FROM s")
            .unwrap();
        db.execute("INSERT INTO t VALUES (4, 0) RETURNING id")
            .unwrap();
        db.execute("COMMIT").unwrap();
        assert_eq!(count(&mut db, "SELECT id FROM t"), 4);
        std::mem::forget(db);
    }
    Database::force_unlock(&db_path).unwrap();
    let mut db = Database::open_path(&db_path).unwrap();
    assert_eq!(
        count(&mut db, "SELECT id FROM t"),
        4,
        "writable-CTE + in-tx RETURNING rows must all replay"
    );
}

#[test]
fn prepared_returning_survives_crash() {
    let dir = tmpdir("prep");
    let db_path = dir.join("spg.db");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT NOT NULL)")
            .unwrap();
        let stmt = db
            .prepare("INSERT INTO t VALUES ($1, 0) RETURNING id")
            .unwrap();
        for i in 1..=3_i32 {
            db.execute_prepared(&stmt, &[spg_storage::Value::Int(i)])
                .unwrap();
        }
        std::mem::forget(db);
    }
    Database::force_unlock(&db_path).unwrap();
    let mut db = Database::open_path(&db_path).unwrap();
    assert_eq!(
        count(&mut db, "SELECT id FROM t"),
        3,
        "prepared RETURNING inserts must replay"
    );
}
