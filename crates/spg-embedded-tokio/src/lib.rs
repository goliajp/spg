//! Tokio-friendly async wrapper around `spg-embedded`.
//!
//! # Why this crate exists
//!
//! `spg-embedded`'s `Database::execute(&mut self, sql)` is sync
//! and may block on WAL fsync or cold-tier I/O. Called from
//! inside a `tokio::main` runtime that triggers the
//! `block_in_place` warning and ties up a worker thread until the
//! call returns. mailrs's cement (entirely tokio-based) is the
//! load-bearing consumer that surfaced this.
//!
//! v7.18 — [`AsyncDatabase`] holds a `tokio::sync::RwLock<Database>`
//! (upgraded from Mutex). Writer calls take the write lock —
//! the engine is still single-writer, that invariant hasn't
//! changed. Snapshot-taking (`read_handle` init / refresh) only
//! needs read access to `clone_snapshot`, so it takes the read
//! lock and concurrent snapshot refreshes do not serialise.
//! `spawn_blocking` insulates the runtime's worker pool from
//! disk stalls the same way it did under Mutex.
//!
//! # Why a separate crate
//!
//! `spg-embedded` keeps the workspace's "0 external dependencies"
//! policy. `tokio` is the largest external dep we'd ever pull,
//! and gating it behind a Cargo feature flag still surfaces
//! `tokio` in downstream consumers' `Cargo.lock`. A separate
//! adapter crate is the clean answer: anyone who wants the
//! tokio shape opts in by adding `spg-embedded-tokio`; everyone
//! else stays untouched.

#![deny(missing_debug_implementations)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

pub use spg_embedded::{
    ColumnSchema, DataType, Database, EngineError, ParsedStatement, QueryResult, Statement, Value,
};
pub use spg_engine::CatalogSnapshot;

use tokio::sync::{Notify, RwLock};
use tokio::task::JoinError;

/// v7.37.11 (mailrs cascade 7 P0 #2) — process-wide registry of
/// in-flight `AsyncDatabase::open_path` calls, keyed by canonical
/// path. Concurrent open_path callers for the same path share the
/// same `OpenPathShared` so they wait on ONE detached spawn_blocking
/// instead of racing to construct overlapping `Database::open_path`
/// invocations (the race that the v7.37.5 `ACTIVE_OPEN_PATHS`
/// in-process registry was designed to refuse — but mailrs's
/// spg-sqlx Pool kept retrying after each refusal). With this
/// dedup, the second-arrival caller never enters Database::open_path;
/// it just `Notify.notified().await`s for the first caller's result.
struct OpenPathShared {
    notify: Notify,
    result: Mutex<Option<Result<AsyncDatabase, EngineError>>>,
}

static INFLIGHT_OPENS: OnceLock<Mutex<HashMap<PathBuf, Arc<OpenPathShared>>>> = OnceLock::new();

enum InflightLookup {
    First(Arc<OpenPathShared>),
    Existing(Arc<OpenPathShared>),
}

fn inflight_shared(canonical: &Path) -> InflightLookup {
    let map = INFLIGHT_OPENS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = guard.get(canonical) {
        InflightLookup::Existing(Arc::clone(s))
    } else {
        let s = Arc::new(OpenPathShared {
            notify: Notify::new(),
            result: Mutex::new(None),
        });
        guard.insert(canonical.to_path_buf(), Arc::clone(&s));
        InflightLookup::First(s)
    }
}

/// v7.34.1 (mailrs prod report bug B): drop the previous
/// `.expect("spawn_blocking join")` shape that panicked on
/// `JoinError::Cancelled` during runtime shutdown — a SIGKILL with any
/// readonly call in flight reliably reproduced it. Cancelled is the
/// expected state when the tokio runtime is being dropped; map it to
/// `EngineError::Cancelled` so the caller's `?` propagates a clean
/// "shutting down" error instead of a panic. A real panic inside the
/// blocking closure still surfaces — `resume_unwind` re-throws the
/// original payload so backtraces and any `catch_unwind` machinery
/// keeps its semantics.
trait FlattenBlockingExt<T> {
    /// Result-returning closures: flatten `JoinHandle`'s outer error
    /// into `EngineError`; an in-flight cancellation becomes
    /// `Err(EngineError::Cancelled)`, panics propagate verbatim.
    fn flatten_blocking(self) -> Result<T, EngineError>;
}

impl<T> FlattenBlockingExt<T> for Result<Result<T, EngineError>, JoinError> {
    fn flatten_blocking(self) -> Result<T, EngineError> {
        match self {
            Ok(inner) => inner,
            Err(je) if je.is_cancelled() => Err(EngineError::Cancelled),
            Err(je) => std::panic::resume_unwind(je.into_panic()),
        }
    }
}

/// Same idea for `spawn_blocking` closures whose return type is a bare
/// `T` (not a `Result`). Used by `read_handle` / `refresh` where the
/// historical signature is `-> T`. Cancellation here surfaces as a
/// panic with an honest message rather than the misleading
/// "spawn_blocking join" string the old expect produced — a
/// Result-returning rework of those two methods is the API-break that
/// follow-up work would carry.
trait UnwrapBlockingExt<T> {
    fn unwrap_blocking(self) -> T;
}

impl<T> UnwrapBlockingExt<T> for Result<T, JoinError> {
    fn unwrap_blocking(self) -> T {
        match self {
            Ok(v) => v,
            Err(je) if je.is_cancelled() => {
                panic!("spg-embedded-tokio: snapshot helper cancelled during runtime shutdown")
            }
            Err(je) => std::panic::resume_unwind(je.into_panic()),
        }
    }
}

/// Tokio-friendly handle to an embedded SPG database. Clone-cheap
/// (`Arc` inside); every clone shares the same underlying engine.
///
/// v7.18 — backed by a `tokio::sync::RwLock` so writer calls
/// serialise (engine single-writer invariant) but snapshot-only
/// operations (`read_handle` init / refresh, which just clone
/// the catalog trie roots) take the read lock and run
/// concurrently with each other.
#[derive(Debug, Clone)]
pub struct AsyncDatabase {
    inner: Arc<RwLock<Database>>,
}

/// v7.16.0 — Tokio-flavoured prepared-statement handle. Wraps
/// the sync `spg_embedded::Statement` in an `Arc` so the AST is
/// shared (not cloned) across `execute_prepared` /
/// `query_prepared` calls, and so the handle is `Clone + Send`
/// without copying the AST per bind. The engine's per-bind
/// internal clone still happens — that's where placeholder
/// substitution lands — but the spg-embedded-tokio surface
/// avoids the second clone the naive shape would force.
///
/// Holding an `AsyncStatement` does NOT pin the database; drop
/// the last `AsyncDatabase` clone and the handle stops being
/// useful (the next `execute_prepared` call would still find a
/// locked `Database` if any other clone is alive, but bind
/// against a dropped database surfaces as the underlying
/// `EngineError`).
#[derive(Debug, Clone)]
pub struct AsyncStatement {
    inner: Arc<crate::Statement>,
}

/// v7.16.0 — adapter escape hatch: hand back the inner
/// `Arc<Statement>`. Used by the `spg-sqlx` crate to plug the
/// engine-side prepared handle into sqlx's `Statement<'q>` trait
/// without going through another clone. Not intended for
/// application code.
#[doc(hidden)]
#[must_use]
pub fn async_statement_inner(stmt: &AsyncStatement) -> Arc<crate::Statement> {
    Arc::clone(&stmt.inner)
}

impl AsyncDatabase {
    /// In-memory database. No WAL, no catalog snapshot on disk.
    /// `Clone` shares the engine; drop the last clone to release.
    #[must_use]
    pub fn open_in_memory() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Database::open_in_memory())),
        }
    }

    /// Open or create a file-backed database at `path`. The open
    /// itself can stat the file + replay the WAL, so the call is
    /// dispatched via `spawn_blocking` to keep the runtime
    /// responsive. Mirrors `Database::open_path`.
    ///
    /// # Errors
    /// Propagates whatever `Database::open_path` returns on the
    /// sync path (IO errors, format errors, etc.).
    pub async fn open_path<P: AsRef<Path>>(path: P) -> Result<Self, EngineError> {
        let path = path.as_ref().to_path_buf();
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        // v7.37.11 (mailrs 06-23 cascade 7 P0 #2) — process-wide dedup
        // of concurrent open_path calls for the same canonical path.
        //
        // Bug: spg-sqlx Pool spawns N concurrent connections, each
        // calling AsyncDatabase::open_path → Database::open_path →
        // LockRegistryGuard::try_acquire. The first registers in
        // ACTIVE_OPEN_PATHS; the rest hit "in this process" refusal.
        // Worse: tokio::sync::OnceCell in spg-sqlx drops its init
        // future on caller cancel, but the underlying spawn_blocking
        // keeps running + keeps holding the registry slot — so a
        // canceled-then-retried connect() sees the slot held by its
        // OWN previous spawn_blocking generation and lock-hangs.
        //
        // Fix: every concurrent open_path for the SAME path shares
        // ONE detached spawn_blocking. Per-caller awaits are
        // cancellation-safe (Notify-based; the spawn_blocking task
        // is NOT a tokio future — cancel can't kill it). The first
        // caller seeds the shared entry; subsequent callers attach
        // before the result is published. On result publication, all
        // waiters wake; map entry is removed.
        let shared = inflight_shared(&canonical);
        let (is_first, shared) = match shared {
            InflightLookup::First(s) => (true, s),
            InflightLookup::Existing(s) => (false, s),
        };
        if is_first {
            let canonical2 = canonical.clone();
            let shared2 = std::sync::Arc::clone(&shared);
            tokio::task::spawn_blocking(move || {
                let result = Database::open_path(path).map(|db| Self {
                    inner: std::sync::Arc::new(tokio::sync::RwLock::new(db)),
                });
                {
                    let mut g = shared2.result.lock().unwrap_or_else(|e| e.into_inner());
                    *g = Some(result);
                }
                shared2.notify.notify_waiters();
                // Drop the map entry so future open_paths spawn a
                // fresh worker (the AsyncDatabase clones the inner
                // Arc<RwLock>, so the shared entry's purpose ends
                // once results are delivered).
                if let Some(map) = INFLIGHT_OPENS.get() {
                    let _ = map
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&canonical2);
                }
            });
        }
        loop {
            {
                let g = shared.result.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(r) = &*g {
                    return r.clone();
                }
            }
            shared.notify.notified().await;
        }
    }

    /// Execute a single SQL statement.
    ///
    /// v7.20 P2 — group-commit: the engine mutation + WAL enqueue
    /// run under the write lock (~1 µs), then the lock DROPS
    /// before the fsync wait. N concurrent writers' mutations
    /// pipeline behind each other while the WAL leader fsyncs
    /// once for the whole batch — profile_breakdown measured
    /// fsync at 99.2% of the durable write path, so this is
    /// where the concurrency comes back.
    ///
    /// # Errors
    /// Propagates `EngineError` unchanged from the sync engine;
    /// a failed batch flush poisons the WAL loudly for all
    /// waiters.
    pub async fn execute(&self, sql: &str) -> Result<QueryResult, EngineError> {
        let inner = Arc::clone(&self.inner);
        let sql = sql.to_string();
        tokio::task::spawn_blocking(move || {
            let (result, ticket) = {
                let mut guard = inner.blocking_write();
                guard.execute_buffered(&sql)?
            }; // ← write lock released here
            if let Some(t) = ticket {
                t.wait()?; // group-commit: shared fsync
            }
            Ok(result)
        })
        .await
        .flatten_blocking()
    }

    /// v7.21 — run a multi-statement script with PG simple-query
    /// semantics (one implicit transaction; see
    /// `spg_embedded::Database::execute_script`). The write lock is
    /// held across the WHOLE script: the engine's transaction slot
    /// is shared, so releasing the lock mid-script would let another
    /// writer's statements join the script's implicit transaction.
    ///
    /// # Errors
    /// Propagates the first failing statement's `EngineError` after
    /// the implicit rollback.
    pub async fn execute_script(&self, sql: &str) -> Result<Vec<QueryResult>, EngineError> {
        let inner = Arc::clone(&self.inner);
        let sql = sql.to_string();
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_write();
            guard.execute_script(&sql)
        })
        .await
        .flatten_blocking()
    }

    /// Run a SELECT and return rows as `Vec<Vec<Value<'static>>>`. Same
    /// dispatch shape as `execute` — lock + spawn_blocking.
    ///
    /// # Errors
    /// Propagates `EngineError` from the engine.
    pub async fn query(&self, sql: &str) -> Result<Vec<Vec<Value<'static>>>, EngineError> {
        let inner = Arc::clone(&self.inner);
        let sql = sql.to_string();
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_write();
            guard.query(&sql)
        })
        .await
        .flatten_blocking()
    }

    /// v7.16.0 — parse + plan a SQL string once. Returns an
    /// [`AsyncStatement`] handle that subsequent
    /// `execute_prepared` / `query_prepared` calls can re-bind
    /// without re-parsing. Cheap to `Clone` — the underlying AST
    /// sits behind an `Arc`, so the same plan can drive many
    /// concurrent bind calls.
    ///
    /// # Errors
    /// Propagates `EngineError` from the underlying
    /// `Database::prepare`.
    pub async fn prepare(&self, sql: &str) -> Result<AsyncStatement, EngineError> {
        let inner = Arc::clone(&self.inner);
        let sql = sql.to_string();
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_write();
            guard.prepare(&sql).map(|stmt| AsyncStatement {
                inner: Arc::new(stmt),
            })
        })
        .await
        .flatten_blocking()
    }

    /// v7.17.0 Phase 3.P0-66 — async wrapper for
    /// [`Database::describe`]. Returns `(parameter_oids,
    /// output_columns)` for a prepared SQL string without
    /// executing it. Drives the spg-sqlx adapter's
    /// `Executor::describe` so `sqlx::query!()` compile-time
    /// validation can resolve column types.
    ///
    /// # Errors
    /// Propagates `EngineError` from the prepare path
    /// (typically `ParseError`).
    pub async fn describe(
        &self,
        sql: &str,
    ) -> Result<(Vec<u32>, Vec<spg_embedded::ColumnSchema>), EngineError> {
        let inner = Arc::clone(&self.inner);
        let sql = sql.to_string();
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_write();
            guard.describe(&sql)
        })
        .await
        .flatten_blocking()
    }

    /// v7.16.0 — execute a prepared statement with bound params.
    /// `params` is taken by value because the spawn_blocking
    /// closure needs a `'static` capture; the cost is one
    /// `Vec::clone`-equivalent ownership transfer, dwarfed by
    /// the engine's per-bind work.
    ///
    /// # Errors
    /// Propagates engine errors; arity mismatch surfaces as
    /// "parameter \$N referenced but only M bound by client".
    pub async fn execute_prepared(
        &self,
        stmt: &AsyncStatement,
        params: Vec<Value<'static>>,
    ) -> Result<QueryResult, EngineError> {
        let inner = Arc::clone(&self.inner);
        let stmt_inner = Arc::clone(&stmt.inner);
        tokio::task::spawn_blocking(move || {
            // v7.20 P2 — group-commit (see `execute`): mutation
            // under the lock, fsync wait after release.
            let (result, ticket) = {
                let mut guard = inner.blocking_write();
                guard.execute_prepared_buffered(&stmt_inner, &params)?
            };
            if let Some(t) = ticket {
                t.wait()?;
            }
            Ok(result)
        })
        .await
        .flatten_blocking()
    }

    /// v7.16.0 — run a prepared SELECT with bound params and
    /// return rows as `Vec<Vec<Value<'static>>>`. Errors when the prepared
    /// statement isn't a SELECT.
    ///
    /// # Errors
    /// Propagates `EngineError` from the underlying
    /// `Database::query_prepared`.
    pub async fn query_prepared(
        &self,
        stmt: &AsyncStatement,
        params: Vec<Value<'static>>,
    ) -> Result<Vec<Vec<Value<'static>>>, EngineError> {
        let inner = Arc::clone(&self.inner);
        let stmt_inner = Arc::clone(&stmt.inner);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_write();
            guard.query_prepared(&stmt_inner, &params)
        })
        .await
        .flatten_blocking()
    }

    /// v7.16.0 — column-aware variant of `query`. Returns the
    /// SELECT's column schema vec alongside the rows so adapters
    /// (the spg-sqlx fetch path most notably) can drive name +
    /// type-based column lookups.
    ///
    /// # Errors
    /// Same shape as `query` — errors when the SQL isn't a SELECT
    /// or the engine returns one.
    pub async fn query_with_columns(
        &self,
        sql: &str,
    ) -> Result<(Vec<spg_embedded::ColumnSchema>, Vec<Vec<Value<'static>>>), EngineError> {
        let inner = Arc::clone(&self.inner);
        let sql = sql.to_string();
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_write();
            guard.query_with_columns(&sql)
        })
        .await
        .flatten_blocking()
    }

    /// v7.16.0 — column-aware variant of `query_prepared`. Same
    /// shape as `query_with_columns` but driven from a prepared
    /// AsyncStatement + bound params.
    ///
    /// # Errors
    /// Propagates `EngineError`; errors when the prepared
    /// statement isn't a SELECT.
    pub async fn query_prepared_with_columns(
        &self,
        stmt: &AsyncStatement,
        params: Vec<Value<'static>>,
    ) -> Result<(Vec<spg_embedded::ColumnSchema>, Vec<Vec<Value<'static>>>), EngineError> {
        let inner = Arc::clone(&self.inner);
        let stmt_inner = Arc::clone(&stmt.inner);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_write();
            guard.query_prepared_with_columns(&stmt_inner, &params)
        })
        .await
        .flatten_blocking()
    }

    /// Run a checkpoint (flush WAL into the catalog snapshot +
    /// truncate the WAL back to zero). Blocking work — dispatched
    /// the same way as `execute`.
    ///
    /// # Errors
    /// Propagates `EngineError` from the engine / IO layer.
    pub async fn checkpoint(&self) -> Result<(), EngineError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_write();
            guard.checkpoint()
        })
        .await
        .flatten_blocking()
    }

    /// v7.20 P3 — inline snapshot clone for the read fan-out hot
    /// path. Takes the async read lock (not `blocking_read` +
    /// `spawn_blocking` — the clone is an Arc-bump of the catalog
    /// trie roots, ~0 µs per profile_breakdown, far below tokio's
    /// inline-work threshold). spg-sqlx's per-statement
    /// read-committed refresh runs through here; pairing it with
    /// `Database::{prepare,execute_prepared}_on_snapshot` keeps
    /// the whole readonly statement on the async executor with
    /// zero thread hops.
    pub async fn clone_snapshot_inline(&self) -> CatalogSnapshot {
        let guard = self.inner.read().await;
        guard.engine().clone_snapshot()
    }

    /// v7.11.2 — fan-out reader. Clones the engine's committed
    /// catalog under the writer lock, releases the lock, and
    /// hands back an `AsyncReadHandle` that runs SELECTs against
    /// the snapshot **without ever re-acquiring the writer
    /// lock**. Multiple read handles can run concurrently — they
    /// share nothing mutable. mailrs's IMAP fetch pattern lands
    /// here.
    ///
    /// Contract: the snapshot is frozen at the moment this call
    /// returns. Subsequent writes are NOT visible. Call
    /// `AsyncReadHandle::refresh().await` to re-snapshot when
    /// you need fresher data.
    pub async fn read_handle(&self) -> AsyncReadHandle {
        let inner = Arc::clone(&self.inner);
        let snapshot = tokio::task::spawn_blocking(move || {
            let guard = inner.blocking_read();
            guard.engine().clone_snapshot()
        })
        .await
        .unwrap_blocking();
        AsyncReadHandle {
            db: Arc::clone(&self.inner),
            snapshot,
        }
    }
}

/// v7.11.2 — read-only handle backed by a frozen
/// `CatalogSnapshot`. Multiple handles can run concurrently; they
/// don't acquire the writer lock at query time. Refresh-on-demand
/// — the contract is that the handle reflects committed state at
/// the moment of construction or the last `refresh()`.
///
/// v7.18 — holds a reference to the underlying `AsyncDatabase`
/// (via the shared `Arc<RwLock<Database>>`) only so `refresh()`
/// can briefly take the read lock to clone a fresh snapshot.
/// Read paths never touch the Database directly. Snapshot
/// cloning is a trie-root `Arc` copy, so a busy writer barely
/// affects refresh latency.
#[derive(Debug)]
pub struct AsyncReadHandle {
    db: Arc<RwLock<Database>>,
    snapshot: CatalogSnapshot,
}

impl AsyncReadHandle {
    /// Run a read-only SQL statement against the frozen snapshot.
    /// DDL / DML reject with `EngineError::WriteRequired`.
    ///
    /// # Errors
    /// Propagates `EngineError` from the engine's read path.
    pub async fn query(&self, sql: &str) -> Result<QueryResult, EngineError> {
        let snapshot = self.snapshot.clone();
        let sql = sql.to_string();
        tokio::task::spawn_blocking(move || {
            spg_engine::Engine::execute_readonly_on_snapshot(&snapshot, &sql)
        })
        .await
        .flatten_blocking()
    }

    /// v7.18 — parse + plan a SQL string against this handle's
    /// frozen snapshot. Mirror of [`AsyncDatabase::prepare`] for
    /// the readonly fan-out path: clock rewrite + JOIN reorder +
    /// position resolve happen against the snapshot's catalog +
    /// statistics, no writer lock acquired. Multiple read handles
    /// can prepare concurrently; the returned [`AsyncStatement`]
    /// is `Clone + Send`.
    ///
    /// # Errors
    /// Propagates [`EngineError`] from the parser
    /// (`EngineError::Parse`).
    pub async fn prepare(&self, sql: &str) -> Result<AsyncStatement, EngineError> {
        let snapshot = self.snapshot.clone();
        let sql = sql.to_string();
        tokio::task::spawn_blocking(move || {
            Database::prepare_on_snapshot(&snapshot, &sql).map(|stmt| AsyncStatement {
                inner: Arc::new(stmt),
            })
        })
        .await
        .flatten_blocking()
    }

    /// v7.18 — execute a prepared statement against this handle's
    /// frozen snapshot with bound params. Mirror of
    /// [`AsyncDatabase::execute_prepared`] on the readonly path —
    /// writes / DDL hit `EngineError::WriteRequired` so the caller
    /// can route them to the writer mutex. No writer lock
    /// acquired; multiple handles run truly concurrently.
    ///
    /// # Errors
    /// Propagates engine errors (placeholder arity mismatch,
    /// schema drift surfacing as catalog lookups, etc.).
    pub async fn execute_prepared(
        &self,
        stmt: &AsyncStatement,
        params: Vec<Value<'static>>,
    ) -> Result<QueryResult, EngineError> {
        let snapshot = self.snapshot.clone();
        let stmt_inner = Arc::clone(&stmt.inner);
        tokio::task::spawn_blocking(move || {
            Database::execute_prepared_on_snapshot(&snapshot, &stmt_inner, &params)
        })
        .await
        .flatten_blocking()
    }

    /// v7.18 — describe a prepared SQL string against this
    /// handle's frozen snapshot. Returns `(parameter_oids,
    /// output_columns)`. Drives the spg-sqlx adapter's readonly
    /// `Executor::describe` path so `sqlx::query!()` compile-time
    /// validation can resolve column types without touching the
    /// writer engine.
    ///
    /// # Errors
    /// Propagates [`EngineError`] from the parser
    /// (`EngineError::Parse`).
    pub async fn describe(
        &self,
        sql: &str,
    ) -> Result<(Vec<u32>, Vec<spg_embedded::ColumnSchema>), EngineError> {
        let snapshot = self.snapshot.clone();
        let sql = sql.to_string();
        tokio::task::spawn_blocking(move || Database::describe_on_snapshot(&snapshot, &sql))
            .await
            .flatten_blocking()
    }

    /// Re-snapshot the underlying engine. Briefly takes the
    /// writer lock; subsequent `query()` calls see the new state.
    /// Idempotent on a quiet engine (clones the same trie roots).
    pub async fn refresh(&mut self) {
        let inner = Arc::clone(&self.db);
        let new_snapshot = tokio::task::spawn_blocking(move || {
            let guard = inner.blocking_read();
            guard.engine().clone_snapshot()
        })
        .await
        .unwrap_blocking();
        self.snapshot = new_snapshot;
    }
}

#[cfg(test)]
mod dedup_tests {
    use super::*;

    /// v7.37.11 P0 #2 — N concurrent open_path calls for the SAME
    /// path must all succeed without any "in this process" sibling
    /// self-lock error. Pre-v7.37.11 this test would have failed
    /// with N-1 of the calls returning `EngineError::Unsupported(
    /// "database is locked by an in-flight task in this process")`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_open_path_dedup() {
        let tmp = tempfile::tempdir().expect("tmp dir");
        let path = tmp.path().join("dedup.spg");
        // Seed: one synchronous open creates the catalog file. This
        // ensures the per-thread spawn_blocking inside open_path sees
        // a real (rather than fresh-creation) catalog to walk.
        {
            let mut db = AsyncDatabase::open_path(&path).await.expect("seed");
            db.execute("CREATE TABLE t(id BIGINT)").await.expect("ddl");
        }
        // Now race N callers. Each clones the path. ALL must succeed.
        let mut handles = Vec::new();
        for _ in 0..16 {
            let p = path.clone();
            handles.push(tokio::spawn(async move {
                AsyncDatabase::open_path(p).await
            }));
        }
        for (i, h) in handles.into_iter().enumerate() {
            let result = h.await.expect("join");
            assert!(
                result.is_ok(),
                "caller {i} got error (expected dedup to serialize all 16 calls): {:?}",
                result.err()
            );
        }
    }

    /// v7.37.11 — the spg-sqlx cancel-then-retry pattern: a caller
    /// drops its future mid-await, then a fresh caller appears. The
    /// dedup should ensure the second caller waits for the SAME
    /// in-flight task (not spawn its own; the first caller's
    /// spawn_blocking is detached so cancel doesn't kill it).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn open_path_survives_caller_cancel() {
        let tmp = tempfile::tempdir().expect("tmp dir");
        let path = tmp.path().join("cancel.spg");
        // Seed.
        {
            let mut db = AsyncDatabase::open_path(&path).await.expect("seed");
            db.execute("CREATE TABLE u(id BIGINT)").await.expect("ddl");
        }
        // Caller 1: starts open_path then drops the future.
        let p1 = path.clone();
        let task1 = tokio::spawn(async move {
            let _ = AsyncDatabase::open_path(p1).await;
        });
        // Force scheduling so the inflight entry is created.
        tokio::task::yield_now().await;
        task1.abort();
        // The spawn_blocking task is still running; the inflight
        // entry should still exist OR have been cleaned up. Either
        // way, a second caller should succeed.
        let result = AsyncDatabase::open_path(path).await;
        assert!(result.is_ok(), "caller after abort got: {:?}", result.err());
    }
}
