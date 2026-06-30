//! v7.10.1 — AsyncDatabase end-to-end coverage.

use spg_embedded_tokio::{AsyncDatabase, Value};

#[tokio::test]
async fn in_memory_roundtrip() {
    let db = AsyncDatabase::open_in_memory();
    db.execute("CREATE TABLE t (a INT NOT NULL, b TEXT NOT NULL)")
        .await
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 'one'), (2, 'two')")
        .await
        .unwrap();
    let rows = db.query("SELECT a, b FROM t ORDER BY a").await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(matches!(rows[0][0], Value::Int(1)));
    assert!(matches!(rows[1][0], Value::Int(2)));
}

#[tokio::test]
async fn file_open_persists_across_handles() {
    let dir = tempdir_unique("spg-async-persist");
    let path = dir.join("db.spg");
    let db = AsyncDatabase::open_path(&path).await.unwrap();
    db.execute("CREATE TABLE t (a INT NOT NULL)").await.unwrap();
    db.execute("INSERT INTO t VALUES (42)").await.unwrap();
    db.checkpoint().await.unwrap();
    drop(db);
    let db2 = AsyncDatabase::open_path(&path).await.unwrap();
    let rows = db2.query("SELECT a FROM t").await.unwrap();
    assert!(matches!(rows[0][0], Value::Int(42)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn clone_shares_engine() {
    let a = AsyncDatabase::open_in_memory();
    let b = a.clone();
    a.execute("CREATE TABLE t (x INT NOT NULL)").await.unwrap();
    b.execute("INSERT INTO t VALUES (7)").await.unwrap();
    let rows = a.query("SELECT x FROM t").await.unwrap();
    assert!(matches!(rows[0][0], Value::Int(7)));
}

#[tokio::test]
async fn concurrent_inserts_serialise() {
    let db = AsyncDatabase::open_in_memory();
    db.execute("CREATE TABLE t (a INT NOT NULL)").await.unwrap();
    let mut handles = Vec::new();
    for i in 0..32 {
        let db = db.clone();
        handles.push(tokio::spawn(async move {
            db.execute(&format!("INSERT INTO t VALUES ({i})"))
                .await
                .unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let rows = db.query("SELECT COUNT(*) FROM t").await.unwrap();
    assert!(matches!(rows[0][0], Value::BigInt(32) | Value::Int(32)));
}

#[tokio::test]
async fn error_propagates() {
    let db = AsyncDatabase::open_in_memory();
    let err = db
        .execute("SELECT * FROM does_not_exist")
        .await
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("does_not_exist"), "{msg}");
}

#[tokio::test]
async fn execute_does_not_block_runtime() {
    // Tight check that we're actually using spawn_blocking — a
    // sync .execute() inside the runtime would block other tasks.
    // Spawn a competing task that increments a counter on a tokio
    // timer; if execute blocked, the counter wouldn't advance.
    use std::sync::atomic::{AtomicU32, Ordering};
    let db = AsyncDatabase::open_in_memory();
    db.execute("CREATE TABLE t (a INT NOT NULL)").await.unwrap();
    let counter = std::sync::Arc::new(AtomicU32::new(0));
    let c = counter.clone();
    let ticker = tokio::spawn(async move {
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            c.fetch_add(1, Ordering::Relaxed);
        }
    });
    // Hammer the engine while the ticker runs.
    for i in 0..200 {
        db.execute(&format!("INSERT INTO t VALUES ({i})"))
            .await
            .unwrap();
    }
    ticker.await.unwrap();
    assert!(
        counter.load(Ordering::Relaxed) > 30,
        "ticker stalled: only {} ticks completed (runtime probably blocked)",
        counter.load(Ordering::Relaxed)
    );
}

/// v7.37.13 (A1.1 TDD red-then-green) — the CHECKPOINT WORKER must
/// self-wake on its own timer when no SQL is flowing.
///
/// Pre-v7.37.13 model: time-based auto-checkpoint exists (v7.37.10),
/// but it is checked only inside `wal_after_ok` — i.e. the front
/// end's commit path. With ZERO writes (a truly idle process), or
/// with caller-side wal_after_ok bypassed by any new path that
/// forgets to call it, the time trigger is never evaluated, the
/// snapshot never advances, the WAL grows, and the quarantine
/// procedure later costs the customer every in-WAL write since the
/// last byte-threshold fire (mailrs cascade 8, 2026-06-24 prod
/// report: 17 h between base.spg mtime advances).
///
/// Closes AUDIT-3-categories.md A1.1 / Top-6 P0 #1. The fix is a
/// background self-wake task that periodically invokes
/// `trigger_checkpoint` on the live AsyncDatabase without needing
/// any caller-driven SQL.
///
/// The test reads `spg_embedded_tokio::self_wake_fire_count()`
/// before and after a pure-idle window with NO SQL whatsoever and
/// asserts the counter advanced — i.e. the self-wake task ticked.
/// A counter (not base.spg mtime) is used as the witness because
/// `set_checkpoint_time_threshold`'s reset semantics make the
/// caller-side path also fire when the next SQL lands, which would
/// false-pass an mtime-only assertion.
///
/// TDD invariant: this test FAILS before the self-wake task exists
/// (the counter is 0 forever). It PASSES once the task starts
/// ticking on its own timer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v7_37_13_checkpoint_self_wakes_when_idle() {
    let dir = tempdir_unique("spg-v7-37-13-self-wake");
    let path = dir.join("idle.spg");
    let db = AsyncDatabase::open_path(&path).await.expect("open");
    db.execute("CREATE TABLE t (id BIGINT)").await.expect("ddl");
    db.checkpoint().await.expect("seed checkpoint");

    db.set_checkpoint_time_threshold(Some(core::time::Duration::from_millis(150)))
        .await;

    let baseline = spg_embedded_tokio::self_wake_fire_count();

    // PURE IDLE: no SQL at all. Only the self-wake task should run.
    tokio::time::sleep(core::time::Duration::from_millis(600)).await;

    let after = spg_embedded_tokio::self_wake_fire_count();
    let fires = after.saturating_sub(baseline);

    assert!(
        fires >= 2,
        "self-wake task did not tick at least twice during 600 ms idle \
         with threshold=150 ms (saw {fires} fires); the v7.37.10 \
         caller-side time trigger only fires inside wal_after_ok and \
         is bypassed entirely when the application is idle. This is \
         mailrs cascade 8 (2026-06-24 prod report) reproduced as a \
         test for AUDIT-3-categories.md A1.1 / Top-6 P0 #1."
    );

    // Liveness: keep the db alive across the assert so Drop doesn't
    // run mid-window and confuse the cause.
    drop(db);
}

fn tempdir_unique(prefix: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("{prefix}-{pid}-{nanos}-{seq}"));
    std::fs::create_dir_all(&path).expect("mkdir tempdir");
    path
}
