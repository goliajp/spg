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

pub use spg_embedded::{Database, EngineError, QueryResult, Value};

use tokio::sync::Mutex;

/// Tokio-friendly handle to an embedded SPG database. Clone-cheap
/// (`Arc` inside); every clone shares the same underlying engine.
/// The internal `Mutex` serialises calls so the engine's
/// single-writer invariant holds even under concurrent callers.
#[derive(Debug, Clone)]
pub struct AsyncDatabase {
    inner: Arc<Mutex<Database>>,
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
}
