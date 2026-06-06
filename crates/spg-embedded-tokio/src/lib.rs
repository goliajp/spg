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
//! [`AsyncDatabase`] holds a `tokio::sync::Mutex<Database>` and
//! dispatches every engine call through `spawn_blocking`. The
//! Mutex matches the engine's single-writer invariant — there is
//! at most one in-flight engine call at any moment — and
//! `spawn_blocking` insulates the runtime's worker pool from
//! disk stalls.
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

use std::path::Path;
use std::sync::Arc;

pub use spg_embedded::{
    ColumnSchema, Database, DataType, EngineError, ParsedStatement, QueryResult, Statement, Value,
};
pub use spg_engine::CatalogSnapshot;

use tokio::sync::Mutex;

/// Tokio-friendly handle to an embedded SPG database. Clone-cheap
/// (`Arc` inside); every clone shares the same underlying engine.
/// The internal `Mutex` serialises calls so the engine's
/// single-writer invariant holds even under concurrent callers.
#[derive(Debug, Clone)]
pub struct AsyncDatabase {
    inner: Arc<Mutex<Database>>,
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
            inner: Arc::new(Mutex::new(Database::open_in_memory())),
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
        let db = tokio::task::spawn_blocking(move || Database::open_path(path))
            .await
            .expect("spawn_blocking join")?;
        Ok(Self {
            inner: Arc::new(Mutex::new(db)),
        })
    }

    /// Execute a single SQL statement. Acquires the internal lock
    /// (FIFO under Tokio's `Mutex`), then dispatches the engine
    /// call to `spawn_blocking` so a WAL fsync or cold-tier read
    /// can't stall the runtime worker.
    ///
    /// # Errors
    /// Propagates `EngineError` unchanged from the sync engine.
    pub async fn execute(&self, sql: &str) -> Result<QueryResult, EngineError> {
        let inner = Arc::clone(&self.inner);
        let sql = sql.to_string();
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_lock();
            guard.execute(&sql)
        })
        .await
        .expect("spawn_blocking join")
    }

    /// Run a SELECT and return rows as `Vec<Vec<Value>>`. Same
    /// dispatch shape as `execute` — lock + spawn_blocking.
    ///
    /// # Errors
    /// Propagates `EngineError` from the engine.
    pub async fn query(&self, sql: &str) -> Result<Vec<Vec<Value>>, EngineError> {
        let inner = Arc::clone(&self.inner);
        let sql = sql.to_string();
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_lock();
            guard.query(&sql)
        })
        .await
        .expect("spawn_blocking join")
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
            let mut guard = inner.blocking_lock();
            guard.prepare(&sql).map(|stmt| AsyncStatement {
                inner: Arc::new(stmt),
            })
        })
        .await
        .expect("spawn_blocking join")
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
        params: Vec<Value>,
    ) -> Result<QueryResult, EngineError> {
        let inner = Arc::clone(&self.inner);
        let stmt_inner = Arc::clone(&stmt.inner);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_lock();
            guard.execute_prepared(&stmt_inner, &params)
        })
        .await
        .expect("spawn_blocking join")
    }

    /// v7.16.0 — run a prepared SELECT with bound params and
    /// return rows as `Vec<Vec<Value>>`. Errors when the prepared
    /// statement isn't a SELECT.
    ///
    /// # Errors
    /// Propagates `EngineError` from the underlying
    /// `Database::query_prepared`.
    pub async fn query_prepared(
        &self,
        stmt: &AsyncStatement,
        params: Vec<Value>,
    ) -> Result<Vec<Vec<Value>>, EngineError> {
        let inner = Arc::clone(&self.inner);
        let stmt_inner = Arc::clone(&stmt.inner);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_lock();
            guard.query_prepared(&stmt_inner, &params)
        })
        .await
        .expect("spawn_blocking join")
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
    ) -> Result<(Vec<spg_embedded::ColumnSchema>, Vec<Vec<Value>>), EngineError> {
        let inner = Arc::clone(&self.inner);
        let sql = sql.to_string();
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_lock();
            guard.query_with_columns(&sql)
        })
        .await
        .expect("spawn_blocking join")
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
        params: Vec<Value>,
    ) -> Result<(Vec<spg_embedded::ColumnSchema>, Vec<Vec<Value>>), EngineError> {
        let inner = Arc::clone(&self.inner);
        let stmt_inner = Arc::clone(&stmt.inner);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_lock();
            guard.query_prepared_with_columns(&stmt_inner, &params)
        })
        .await
        .expect("spawn_blocking join")
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
            let mut guard = inner.blocking_lock();
            guard.checkpoint()
        })
        .await
        .expect("spawn_blocking join")
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
            let guard = inner.blocking_lock();
            guard.engine().clone_snapshot()
        })
        .await
        .expect("spawn_blocking join");
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
/// Holds a reference to the underlying `AsyncDatabase` (via the
/// shared `Arc<Mutex<Database>>`) only so `refresh()` can briefly
/// re-acquire the lock to take a fresh snapshot. Read paths never
/// touch the Database directly.
#[derive(Debug)]
pub struct AsyncReadHandle {
    db: Arc<Mutex<Database>>,
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
        .expect("spawn_blocking join")
    }

    /// Re-snapshot the underlying engine. Briefly takes the
    /// writer lock; subsequent `query()` calls see the new state.
    /// Idempotent on a quiet engine (clones the same trie roots).
    pub async fn refresh(&mut self) {
        let inner = Arc::clone(&self.db);
        let new_snapshot = tokio::task::spawn_blocking(move || {
            let guard = inner.blocking_lock();
            guard.engine().clone_snapshot()
        })
        .await
        .expect("spawn_blocking join");
        self.snapshot = new_snapshot;
    }
}
