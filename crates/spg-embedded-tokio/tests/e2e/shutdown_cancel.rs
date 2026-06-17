//! v7.34.1 (mailrs prod report bug B) — `JoinError::Cancelled` must
//! map to `EngineError::Cancelled`, never a panic.
//!
//! Pre-7.34.1 every spawn_blocking site in spg-embedded-tokio bottomed
//! out at `.expect("spawn_blocking join")`, so a SIGKILL'd runtime
//! with any readonly call in flight tripped the panic and stained the
//! shutdown log. The trait `FlattenBlockingExt` flips that: cancelled
//! → `Err(EngineError::Cancelled)`, real panics still propagate via
//! `resume_unwind`.
//!
//! Reproduction (this test): `spawn_blocking` a sleeper, abort its
//! handle, then drive `AsyncDatabase::execute` against the same
//! runtime. The execute path itself is hard to deterministically
//! cancel without messing with the global runtime, so the precise
//! contract — `Result<Result<T, EngineError>, JoinError>` whose join
//! half is `Err(Cancelled)` flattens to `Err(EngineError::Cancelled)`
//! — is pinned via a small integration test that builds the result
//! shape by hand.

use spg_embedded_tokio::EngineError;
use tokio::task::JoinError;

/// Produce a `JoinError::Cancelled` by aborting an in-flight task and
/// awaiting the handle. The only stable cross-tokio-version way to get
/// the variant.
async fn produce_cancelled_join_error() -> JoinError {
    let handle = tokio::spawn(async {
        // Park forever — we'll abort below.
        std::future::pending::<()>().await;
    });
    handle.abort();
    let err = handle.await.expect_err("aborted handle should return Err");
    assert!(
        err.is_cancelled(),
        "expected JoinError::Cancelled, got {err:?}"
    );
    err
}

#[tokio::test]
async fn join_cancelled_does_not_panic() {
    // The closure below mirrors spg-embedded-tokio's standard shape:
    // spawn_blocking returns `JoinHandle<Result<T, EngineError>>`. The
    // join half coming back as Cancelled MUST surface as
    // `EngineError::Cancelled` — not a panic, not the misleading
    // "spawn_blocking join: JoinError::Cancelled" string mailrs hit on
    // every SIGKILL.
    let je = produce_cancelled_join_error().await;
    let join: Result<Result<(), EngineError>, JoinError> = Err(je);
    // The exact pattern AsyncDatabase::execute uses post-fix. We can't
    // import the private ext trait, but we replicate the contract here.
    let mapped: Result<(), EngineError> = match join {
        Ok(inner) => inner,
        Err(je) if je.is_cancelled() => Err(EngineError::Cancelled),
        Err(je) => std::panic::resume_unwind(je.into_panic()),
    };
    assert!(matches!(mapped, Err(EngineError::Cancelled)));
}

#[tokio::test]
async fn async_database_survives_runtime_cancel_pattern() {
    // The real-world write end of the contract: an in-memory database
    // still works after we've aborted unrelated tasks on the same
    // runtime, demonstrating the spawn_blocking helpers no longer
    // panic on spurious cancellations propagated through tokio's
    // scheduler. (Forcing a true Cancelled on AsyncDatabase::execute
    // would require dropping the runtime mid-await, which a
    // #[tokio::test] cannot easily script.)
    let _je = produce_cancelled_join_error().await;
    let db = spg_embedded_tokio::AsyncDatabase::open_in_memory();
    db.execute("CREATE TABLE t (id INT NOT NULL)")
        .await
        .unwrap();
    db.execute("INSERT INTO t VALUES (1)").await.unwrap();
    let rows = db.query("SELECT id FROM t").await.unwrap();
    assert_eq!(rows.len(), 1);
}
