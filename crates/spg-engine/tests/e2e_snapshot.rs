//! v7.11.0/1 — `CatalogSnapshot` + `execute_readonly_on_snapshot`.
//! Epic 1 of v7.11: read-isolation primitive used by
//! `spg-embedded-tokio::AsyncReadHandle`.

use spg_engine::{CatalogSnapshot, Engine, EngineError, QueryResult};
use spg_storage::Value;

fn ok(eng: &mut Engine, sql: &str) -> QueryResult {
    eng.execute(sql)
        .unwrap_or_else(|e| panic!("{sql:?}: {e:?}"))
}

fn read_rows(snap: &CatalogSnapshot, sql: &str) -> Vec<Vec<Value>> {
    match Engine::execute_readonly_on_snapshot(snap, sql)
        .unwrap_or_else(|e| panic!("{sql:?}: {e:?}"))
    {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn snapshot_isolates_from_subsequent_writes() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a INT NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES (1), (2), (3)");
    let snap = eng.clone_snapshot();
    // Write past the snapshot point.
    ok(&mut eng, "INSERT INTO t VALUES (4), (5)");
    // Snapshot still sees only 3 rows.
    let rows = read_rows(&snap, "SELECT a FROM t ORDER BY a");
    assert_eq!(rows.len(), 3);
    // The live engine sees 5.
    let live = match eng.execute_readonly("SELECT COUNT(*) FROM t").unwrap() {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        _ => panic!(),
    };
    assert!(matches!(live, Value::BigInt(5) | Value::Int(5)));
}

#[test]
fn snapshot_survives_row_delete() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a INT NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES (1), (2), (3)");
    let snap = eng.clone_snapshot();
    ok(&mut eng, "DELETE FROM t WHERE a = 2");
    // Live engine sees 2 rows; snapshot still has all 3.
    let rows = read_rows(&snap, "SELECT a FROM t ORDER BY a");
    assert_eq!(rows.len(), 3);
}

#[test]
fn snapshot_rejects_ddl() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a INT NOT NULL)");
    let snap = eng.clone_snapshot();
    let err = Engine::execute_readonly_on_snapshot(&snap, "CREATE TABLE t2 (b INT)").unwrap_err();
    assert!(matches!(err, EngineError::WriteRequired), "{err:?}");
}

#[test]
fn snapshot_rejects_dml() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a INT NOT NULL)");
    let snap = eng.clone_snapshot();
    let err = Engine::execute_readonly_on_snapshot(&snap, "INSERT INTO t VALUES (1)").unwrap_err();
    assert!(matches!(err, EngineError::WriteRequired), "{err:?}");
}

#[test]
fn many_snapshots_concurrent_reads() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a INT NOT NULL)");
    for i in 0..100i64 {
        ok(&mut eng, &format!("INSERT INTO t VALUES ({i})"));
    }
    // Take 8 snapshots and run a parallel-ish read against each.
    let snaps: Vec<_> = (0..8).map(|_| eng.clone_snapshot()).collect();
    let handles: Vec<_> = snaps
        .into_iter()
        .map(|snap| std::thread::spawn(move || read_rows(&snap, "SELECT COUNT(*) FROM t")))
        .collect();
    for h in handles {
        let rows = h.join().expect("thread");
        let v = &rows[0][0];
        assert!(matches!(v, Value::BigInt(100) | Value::Int(100)), "{v:?}");
    }
}

#[test]
fn snapshot_carries_row_limit() {
    let mut eng = Engine::new().with_max_query_rows(5);
    ok(&mut eng, "CREATE TABLE t (a INT NOT NULL)");
    for i in 0..10 {
        ok(&mut eng, &format!("INSERT INTO t VALUES ({i})"));
    }
    let snap = eng.clone_snapshot();
    let err = Engine::execute_readonly_on_snapshot(&snap, "SELECT a FROM t").unwrap_err();
    assert!(matches!(err, EngineError::RowLimitExceeded(5)), "{err:?}");
}

#[test]
fn snapshot_send_across_threads() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a INT NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES (42)");
    let snap = eng.clone_snapshot();
    let v = std::thread::spawn(move || {
        let rows = read_rows(&snap, "SELECT a FROM t");
        rows[0][0].clone()
    })
    .join()
    .expect("thread");
    assert!(matches!(v, Value::Int(42)));
}

#[test]
fn snapshot_refresh_pattern_via_reclone() {
    // No `refresh()` method on the engine — the pattern is
    // "drop the old snapshot, take a fresh one".
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a INT NOT NULL)");
    ok(&mut eng, "INSERT INTO t VALUES (1)");
    let snap_v1 = eng.clone_snapshot();
    assert_eq!(read_rows(&snap_v1, "SELECT a FROM t").len(), 1);
    ok(&mut eng, "INSERT INTO t VALUES (2)");
    let snap_v2 = eng.clone_snapshot();
    assert_eq!(read_rows(&snap_v2, "SELECT a FROM t").len(), 2);
    // Old snapshot still sees the old state.
    assert_eq!(read_rows(&snap_v1, "SELECT a FROM t").len(), 1);
}
