//! v7.16.0 — `Database::prepare` + `execute_prepared` /
//! `query_prepared`. mailrs gap-eval E2: in-process callers
//! (and the upcoming `spg-sqlx` adapter) need to bind params
//! without paying the SQL re-parse cost on every call. The
//! engine has plan caching since v6.3.0 + placeholder support
//! since v6.1.1; v7.16.0 surfaces them on the embedded API.

use spg_embedded::{Database, Value};
use std::path::PathBuf;

/// Auto-cleaning scratch dir. Mirrors e2e_chaos.rs's Scratch so
/// the no-tempfile-dep policy stays clean.
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
        p.push(format!("spg-embedded-prepare-{label}-{nanos}"));
        std::fs::create_dir_all(&p).unwrap();
        Self { path: p }
    }
    fn child(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn prepare_then_bind_query() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'alice')").unwrap();
    db.execute("INSERT INTO users VALUES (2, 'bob')").unwrap();
    db.execute("INSERT INTO users VALUES (3, 'carol')").unwrap();

    let stmt = db.prepare("SELECT name FROM users WHERE id = $1").unwrap();
    let rows = db.query_prepared(&stmt, &[Value::Int(2)]).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Text("bob".into()));

    // Same plan, different bind — exercises plan reuse.
    let rows = db.query_prepared(&stmt, &[Value::Int(1)]).unwrap();
    assert_eq!(rows[0][0], Value::Text("alice".into()));

    let rows = db.query_prepared(&stmt, &[Value::Int(3)]).unwrap();
    assert_eq!(rows[0][0], Value::Text("carol".into()));
}

#[test]
fn prepare_then_bind_dml() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE items (id INT NOT NULL, qty INT NOT NULL)")
        .unwrap();

    let insert = db.prepare("INSERT INTO items VALUES ($1, $2)").unwrap();
    db.execute_prepared(&insert, &[Value::Int(1), Value::Int(10)])
        .unwrap();
    db.execute_prepared(&insert, &[Value::Int(2), Value::Int(20)])
        .unwrap();
    db.execute_prepared(&insert, &[Value::Int(3), Value::Int(30)])
        .unwrap();

    let rows = db.query("SELECT id, qty FROM items ORDER BY id").unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], vec![Value::Int(1), Value::Int(10)]);
    assert_eq!(rows[2], vec![Value::Int(3), Value::Int(30)]);
}

#[test]
fn prepare_multi_param_mixed_types() {
    let mut db = Database::open_in_memory();
    db.execute(
        "CREATE TABLE events (id INT NOT NULL, name TEXT NOT NULL, \
         active BOOLEAN NOT NULL, ts TIMESTAMPTZ NOT NULL)",
    )
    .unwrap();

    let insert = db
        .prepare("INSERT INTO events VALUES ($1, $2, $3, $4)")
        .unwrap();
    db.execute_prepared(
        &insert,
        &[
            Value::Int(1),
            Value::Text("signin".into()),
            Value::Bool(true),
            // v7.15.0 TIMESTAMPTZ accepts offset literal —
            // converted to i64 µs UTC at coerce time.
            Value::Text("2026-06-06 12:00:00+00".into()),
        ],
    )
    .unwrap();

    let select = db
        .prepare("SELECT id, name, active FROM events WHERE active = $1")
        .unwrap();
    let rows = db.query_prepared(&select, &[Value::Bool(true)]).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[0][1], Value::Text("signin".into()));
    assert_eq!(rows[0][2], Value::Bool(true));
}

#[test]
fn prepare_query_on_dml_errors() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    let dml = db.prepare("INSERT INTO t VALUES ($1)").unwrap();
    let err = db.query_prepared(&dml, &[Value::Int(1)]).unwrap_err();
    // The handle was prepared as a DML; query_prepared rejects
    // because it can't surface row rows for the caller.
    assert!(err.to_string().contains("SELECT"));
}

#[test]
fn prepare_arity_mismatch_errors() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    let stmt = db.prepare("SELECT id FROM t WHERE id = $1").unwrap();
    // Empty params — placeholder $1 references past the end.
    let err = db.query_prepared(&stmt, &[]).unwrap_err();
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("parameter $1") || msg.contains("placeholder"),
        "expected parameter-arity error, got: {err}"
    );
}

#[test]
fn prepared_dml_persists_via_wal() {
    // Per-connection WAL: bind-final SQL renders into the WAL,
    // so replay sees a simple-query-shaped statement. End-to-end
    // smoke: open path, prepare+bind, drop, reopen, query.
    let dir = Scratch::new("wal-smoke");
    let path = dir.child("db");

    {
        let mut db = Database::open_path(&path).unwrap();
        db.execute("CREATE TABLE kv (k INT NOT NULL, v TEXT NOT NULL)")
            .unwrap();
        let stmt = db.prepare("INSERT INTO kv VALUES ($1, $2)").unwrap();
        db.execute_prepared(&stmt, &[Value::Int(1), Value::Text("one".into())])
            .unwrap();
        db.execute_prepared(&stmt, &[Value::Int(2), Value::Text("two".into())])
            .unwrap();
    }

    let mut db = Database::open_path(&path).unwrap();
    let rows = db.query("SELECT k, v FROM kv ORDER BY k").unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], vec![Value::Int(1), Value::Text("one".into())]);
    assert_eq!(rows[1], vec![Value::Int(2), Value::Text("two".into())]);
}
