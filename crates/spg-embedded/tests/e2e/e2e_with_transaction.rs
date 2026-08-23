//! v7.2.0 — `Database::with_transaction(|tx| …)`.

use spg_embedded::{Database, EngineError};
use spg_storage::Value;

#[test]
fn with_transaction_commits_on_ok() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    db.with_transaction(|tx| {
        tx.execute("INSERT INTO t VALUES (1)")?;
        tx.execute("INSERT INTO t VALUES (2)")?;
        Ok::<_, EngineError>(())
    })
    .unwrap();
    let got = db.query("SELECT id FROM t WHERE id = 1").unwrap();
    assert_eq!(got.len(), 1);
    let got = db.query("SELECT id FROM t WHERE id = 2").unwrap();
    assert_eq!(got.len(), 1);
}

#[test]
fn with_transaction_rolls_back_on_err() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    db.execute("INSERT INTO t VALUES (100)").unwrap();
    let result: Result<(), EngineError> = db.with_transaction(|tx| {
        tx.execute("INSERT INTO t VALUES (1)")?;
        tx.execute("INSERT INTO t VALUES (2)")?;
        // Surface a deliberate failure.
        Err(EngineError::Unsupported("rollback me".into()))
    });
    assert!(result.is_err());
    // Pre-TX row survives.
    let got = db.query("SELECT id FROM t WHERE id = 100").unwrap();
    assert_eq!(got.len(), 1);
    // In-TX rows are gone.
    for id in [1, 2] {
        let got = db
            .query(&format!("SELECT id FROM t WHERE id = {id}"))
            .unwrap();
        assert!(got.is_empty(), "row {id} survived rollback");
    }
}

#[test]
fn with_transaction_returns_body_value() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    let count = db
        .with_transaction(|tx| {
            tx.execute("INSERT INTO t VALUES (1)")?;
            tx.execute("INSERT INTO t VALUES (2)")?;
            tx.execute("INSERT INTO t VALUES (3)")?;
            Ok::<_, EngineError>(3usize)
        })
        .unwrap();
    assert_eq!(count, 3);
}

#[test]
fn with_transaction_works_on_persistent_db() {
    let dir = std::env::temp_dir().join("spg-tests").join(format!(
        "spg-embed-tx-persist-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("spg.db");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        db.with_transaction(|tx| {
            tx.execute("INSERT INTO t VALUES (1)")?;
            tx.execute("INSERT INTO t VALUES (2)")?;
            Ok::<_, EngineError>(())
        })
        .unwrap();
    }
    // Reopen — committed rows survive.
    let mut db = Database::open_path(&db_path).unwrap();
    let got = db.query("SELECT id FROM t WHERE id = 1").unwrap();
    match &got[0][0] {
        Value::Int(1) => {}
        other => panic!("expected Int(1), got {other:?}"),
    }
}

#[test]
fn rolled_back_transaction_stays_rolled_back_after_reopen() {
    // The WAL must not resurrect a rolled-back transaction's writes
    // on replay. mailrs embed round-12 polish: the in-memory rollback
    // path was covered, the durable one (snapshot + WAL replay after
    // a reopen) was not — and the file-backed pool is exactly what a
    // cutover production box runs.
    let dir = std::env::temp_dir().join("spg-tests").join(format!(
        "spg-embed-tx-rollback-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("spg.db");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        let r: Result<(), EngineError> = db.with_transaction(|tx| {
            tx.execute("INSERT INTO t VALUES (1)")?;
            Err(EngineError::Unsupported("force rollback".into()))
        });
        assert!(r.is_err());
        // Explicit statement-level form too (the sqlx adapter path).
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO t VALUES (2)").unwrap();
        db.execute("ROLLBACK").unwrap();
        assert!(
            db.query("SELECT id FROM t").unwrap().is_empty(),
            "rolled-back rows visible before reopen"
        );
    }
    let mut db = Database::open_path(&db_path).unwrap();
    let got = db.query("SELECT id FROM t").unwrap();
    assert!(
        got.is_empty(),
        "WAL replay resurrected rolled-back rows: {got:?}"
    );
}

#[test]
fn committed_transaction_survives_process_crash() {
    // Counterpart of the rollback test: a COMMITted transaction must
    // be durable the moment commit returns — not only after a
    // graceful Drop/checkpoint. `mem::forget` skips Drop, simulating
    // SIGKILL; the reopen must replay the committed writes from WAL.
    let dir = std::env::temp_dir().join("spg-tests").join(format!(
        "spg-embed-tx-crash-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("spg.db");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO t VALUES (1)").unwrap();
        db.execute("COMMIT").unwrap();
        std::mem::forget(db); // crash: no Drop, no checkpoint
    }
    // The leaked lock names a live pid (ours); clear it the way an
    // operator would.
    Database::force_unlock(&db_path).unwrap();
    let mut db = Database::open_path(&db_path).unwrap();
    let got = db.query("SELECT id FROM t").unwrap();
    assert_eq!(got.len(), 1, "committed tx lost across crash: {got:?}");
}

#[test]
fn savepoint_rollback_shapes_the_replayed_transaction() {
    // ROLLBACK TO SAVEPOINT must truncate the tx WAL buffer so the
    // replayed transaction matches what the engine committed. The
    // surviving row carries a ';' inside a string literal to pin the
    // record's script-splitting on replay.
    let dir = std::env::temp_dir().join("spg-tests").join(format!(
        "spg-embed-tx-savepoint-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("spg.db");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL, note TEXT)")
            .unwrap();
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO t VALUES (1, 'a;b')").unwrap();
        db.execute("SAVEPOINT s1").unwrap();
        db.execute("INSERT INTO t VALUES (2, 'discarded')").unwrap();
        db.execute("ROLLBACK TO SAVEPOINT s1").unwrap();
        db.execute("INSERT INTO t VALUES (3, 'kept')").unwrap();
        db.execute("COMMIT").unwrap();
        std::mem::forget(db); // crash — recovery must come from WAL
    }
    Database::force_unlock(&db_path).unwrap();
    let mut db = Database::open_path(&db_path).unwrap();
    let got = db.query("SELECT id FROM t ORDER BY id").unwrap();
    let ids: Vec<_> = got.iter().map(|r| r[0].clone()).collect();
    assert_eq!(
        ids,
        vec![Value::Int(1), Value::Int(3)],
        "replayed tx must reflect the savepoint rollback"
    );
}

/// v7.39 (round 622) — this asserted that nesting ERRORS, which was SPG's
/// own behaviour written down as a rule. PG does not error: a `BEGIN` inside
/// a transaction warns "there is already a transaction in progress" and is a
/// no-op, and the later extra `COMMIT` warns "there is no transaction in
/// progress". The engine matches that now, byte for byte —
///
/// ```text
///     BEGIN; INSERT 1; BEGIN; INSERT 2; COMMIT; COMMIT; SELECT count(*)
///     PG18  WARNING, WARNING, 2        SPG  WARNING, WARNING, 2
/// ```
///
/// — so the rule this test encoded is gone, and what replaces it is the
/// consequence a caller has to know: the INNER commit ends the OUTER
/// transaction, so work after it is no longer covered.
#[test]
fn nested_with_transaction_follows_pg_and_does_not_error() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    let result: Result<(), EngineError> = db.with_transaction(|tx| {
        tx.execute("INSERT INTO t VALUES (1)")?;
        tx.with_transaction(|inner| {
            inner.execute("INSERT INTO t VALUES (2)")?;
            Ok::<_, EngineError>(())
        })?;
        Ok(())
    });
    assert!(
        result.is_ok(),
        "PG does not reject a nested BEGIN: {result:?}"
    );
    let got = db.query("SELECT id FROM t ORDER BY id").unwrap();
    assert_eq!(got.len(), 2, "both rows are committed, as in PG");
}
