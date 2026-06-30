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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

/// v7.37.13 (A1.1) — count of times the background self-wake task
/// successfully invoked the checkpoint trigger on an idle AsyncDatabase.
///
/// Process-wide because the task is per-`AsyncDatabase::open_path`
/// and there can be many. Tests use [`self_wake_fire_count`] as a
/// witness that the timer actually ticked — relying on base.spg
/// mtime alone would false-pass when the caller-side time trigger
/// (v7.37.10) fires on the test's setup SQL.
static SELF_WAKE_FIRE_COUNT: AtomicU64 = AtomicU64::new(0);

/// v7.37.13 (A1.1) — snapshot of the process-wide self-wake fire
/// counter. Tests read this before / after a quiescent window and
/// assert the delta is positive, witnessing that the timer ticked.
///
/// Production code has no reason to read this; it's an observability
/// hook for tests (and `spgctl`-style diagnostics, future).
#[must_use]
pub fn self_wake_fire_count() -> u64 {
    SELF_WAKE_FIRE_COUNT.load(AtomicOrdering::Relaxed)
}

/// v7.37.13 (A1.1) — minimum self-wake tick interval. The task
/// sleeps at most this long between checks; if the configured
/// `checkpoint_time_threshold` is smaller, the tick uses half the
/// threshold so it can fire near the deadline rather than after.
const SELF_WAKE_MAX_TICK: core::time::Duration = core::time::Duration::from_millis(500);

/// v7.37.13 (A1.1) — background self-wake checkpoint loop. Spawned
/// once per successful `AsyncDatabase::open_path`. Holds a Weak to
/// the inner `RwLock<Database>` so it exits the moment the last
/// `AsyncDatabase` clone drops (no leak, no shutdown channel).
///
/// On each tick:
///   1. Upgrade the Weak. None → all `AsyncDatabase` clones dropped → exit.
///   2. `blocking_read` the Database (snapshot path needs `&self`).
///   3. Read the configured threshold. None / Some(ZERO) → exit
///      (operator opted out via `SPG_EMBEDDED_CHECKPOINT_SECONDS=0`).
///   4. Invoke `maybe_trigger_checkpoint` — the helper is idempotent
///      (skips if a checkpoint is already pending / in flight, and
///      the engine's own dedup gate skips if there's nothing new
///      to flush since the last snapshot).
///   5. Bump `SELF_WAKE_FIRE_COUNT` (witness for tests / diagnostics).
///   6. Sleep min(threshold/2, SELF_WAKE_MAX_TICK).
///
/// The whole loop runs on tokio's worker pool (`spawn`), not
/// `spawn_blocking` — the read-lock + Arc-bump-and-enqueue path is
/// microseconds, well under the inline-work threshold. The heavy
/// snapshot serialization happens in the checkpoint worker
/// (separate std::thread already), not here.
fn spawn_self_wake_checkpoint_task(weak: std::sync::Weak<tokio::sync::RwLock<Database>>) {
    tokio::spawn(async move {
        let mut tick = SELF_WAKE_MAX_TICK;
        loop {
            tokio::time::sleep(tick).await;
            let Some(arc) = weak.upgrade() else {
                // All AsyncDatabase clones dropped → exit cleanly.
                return;
            };
            // Hold the read lock for the briefest possible window —
            // snapshot_checkpoint_job is an Arc bump + cheap clones,
            // the actual disk work runs on the checkpoint worker
            // thread off this borrow. Use async read() so we don't
            // block the tokio worker (blocking_read would panic with
            // "Cannot block the current thread from within a runtime").
            let threshold_opt = {
                let g = arc.read().await;
                let t = g.checkpoint_time_threshold();
                // Skip the trigger when the operator disabled the
                // path (Some(ZERO) means "fire ASAP"; truly disabled
                // is None).
                if t.is_some() {
                    let _ = g.maybe_trigger_checkpoint();
                    SELF_WAKE_FIRE_COUNT.fetch_add(1, AtomicOrdering::Relaxed);
                }
                t
            };
            drop(arc);
            // Adapt the next tick to the configured threshold so a
            // small threshold doesn't have to wait the 500 ms cap.
            tick = match threshold_opt {
                None => SELF_WAKE_MAX_TICK,
                Some(t) if t.is_zero() => SELF_WAKE_MAX_TICK,
                Some(t) => (t / 2)
                    .max(core::time::Duration::from_millis(10))
                    .min(SELF_WAKE_MAX_TICK),
            };
        }
    });
}

pub use spg_embedded::{
    ColumnSchema, DataType, Database, EngineError, ParsedStatement, QueryResult, Statement, Value,
};
pub use spg_engine::CatalogSnapshot;

// v7.37.14 (B2.4) — generic race-dedup primitive. Originally
// inlined in this file for the v7.37.11 INFLIGHT_OPENS path;
// extracted so future race shapes (index rebuild, partition
// attach, ...) reuse the same watch-channel race-fix from
// v7.37.12 instead of re-deriving it per site.
pub mod race_guard;
pub use race_guard::{RaceGuard, RaceLookup, RaceShared};

use tokio::sync::RwLock;
use tokio::task::JoinError;

/// v7.37.11 (mailrs cascade 7 P0 #2) — process-wide deduplication
/// of in-flight `AsyncDatabase::open_path` calls, keyed by canonical
/// path. Concurrent open_path callers for the same path share the
/// same shared inflight handle so they wait on ONE detached
/// spawn_blocking instead of racing to construct overlapping
/// `Database::open_path` invocations (the race that the v7.37.5
/// `ACTIVE_OPEN_PATHS` in-process registry was designed to refuse —
/// but mailrs's spg-sqlx Pool kept retrying after each refusal).
/// With this dedup, the second-arrival caller never enters
/// Database::open_path; it just awaits the first caller's result.
///
/// v7.37.12 — replaced the original Notify+Mutex pair with a
/// `tokio::sync::watch` channel to close a subscribe-after-publish
/// race (the Notify variant didn't store a permit; a late receiver
/// could miss `notify_waiters()`).
///
/// v7.37.14 (B2.4) — extracted to the generic [`RaceGuard`]
/// primitive in `race_guard.rs`. The on-the-wire behaviour is
/// identical; the bespoke `OpenPathShared` / `InflightLookup`
/// types are gone. Future race-dedup shapes (index rebuild,
/// partition attach, etc.) reuse the same primitive.
static INFLIGHT_OPENS: RaceGuard<PathBuf, Result<AsyncDatabase, EngineError>> = RaceGuard::new();

/// v7.37.12 — observability counters for the dedup path. Forwarded
/// from the underlying `RaceGuard` counters so existing callers
/// (`spgctl`, test harnesses) read the same atomic addresses they
/// did before v7.37.14 — i.e. this is a 0-source-change
/// observability migration, not just a refactor.
pub fn open_path_dedup_first_count() -> u64 {
    INFLIGHT_OPENS
        .first_count
        .load(std::sync::atomic::Ordering::Relaxed)
}

pub fn open_path_dedup_existing_count() -> u64 {
    INFLIGHT_OPENS
        .existing_count
        .load(std::sync::atomic::Ordering::Relaxed)
}

/// v7.37.12 — when `SPG_OPEN_PATH_LOG=1` is set, every open_path
/// call emits a single stderr line covering the dedup decision +
/// total elapsed wait. Off-by-default so existing log scrapers
/// don't break; on-by-env-var so an operator hitting a prod
/// recurrence of v7.37.5 RAII-claim-vs-reality drift (mailrs
/// pattern) can flip the flag without redeploying.
fn open_path_log_enabled() -> bool {
    std::env::var_os("SPG_OPEN_PATH_LOG").is_some()
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
        let log_enabled = open_path_log_enabled();
        let start = std::time::Instant::now();
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
        let lookup = INFLIGHT_OPENS.lookup(&canonical);
        let (is_first, shared) = match lookup {
            RaceLookup::First(s) => (true, s),
            RaceLookup::Existing(s) => (false, s),
        };
        if log_enabled {
            eprintln!(
                "[spg open_path] path={} role={}",
                canonical.display(),
                if is_first { "first" } else { "existing" }
            );
        }
        if is_first {
            let canonical2 = canonical.clone();
            let shared2 = std::sync::Arc::clone(&shared);
            tokio::task::spawn_blocking(move || {
                let result = Database::open_path(path).map(|db| Self {
                    inner: std::sync::Arc::new(tokio::sync::RwLock::new(db)),
                });
                // v7.37.14 — RaceGuard handles publish + map removal
                // atomically; followers wake from the watch channel.
                INFLIGHT_OPENS.publish_and_remove(&canonical2, &shared2, result);
            });
        }
        // Both leader and followers await via the shared. The leader
        // ends up reading its own published result (no special path).
        if let Some(result) = shared.subscribe_done().await {
            if log_enabled {
                eprintln!(
                    "[spg open_path] path={} role={} elapsed={:.3}s status={}",
                    canonical.display(),
                    if is_first { "first" } else { "existing" },
                    start.elapsed().as_secs_f64(),
                    if result.is_ok() { "ok" } else { "err" }
                );
            }
            // v7.37.13 (A1.1) — start the background self-wake
            // checkpoint timer on every successful open_path so the
            // on-disk snapshot advances on its own schedule even if
            // the calling app goes fully idle. The task holds a
            // Weak<RwLock<Database>>, so it auto-exits when the last
            // AsyncDatabase clone drops.
            if let Ok(ref db) = result {
                spawn_self_wake_checkpoint_task(std::sync::Arc::downgrade(&db.inner));
            }
            return result;
        }
        // Subscribe returned None → channel sender dropped without
        // publishing Done. Should not happen in practice (the
        // spawn_blocking always sends), but treat as cancellation.
        Err(EngineError::Cancelled)
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

    /// v7.37.13 — async façade over the sync
    /// [`spg_embedded::Database::checkpoint_wait`]. Drains the
    /// background checkpoint worker (waits until any pending /
    /// in-flight async checkpoint completes), surfaces sticky
    /// errors, returns. Used by the v7.37.13 self-wake timer tests
    /// to assert that the worker actually wrote a new snapshot.
    pub async fn checkpoint_wait(&self) -> Result<(), EngineError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let guard = inner.blocking_read();
            guard.checkpoint_wait()
        })
        .await
        .flatten_blocking()
    }

    /// v7.37.13 — async façade over
    /// [`spg_embedded::Database::set_checkpoint_time_threshold`]
    /// for tests. Setting `None` disables the time path; setting
    /// `Some(d)` overrides the env-var default
    /// (`SPG_EMBEDDED_CHECKPOINT_SECONDS`). Production callers
    /// normally don't need this — the env var is the supported
    /// configuration channel; the setter exists so tests can drop
    /// the 60 s default to sub-second values without waiting a
    /// real minute per case.
    pub async fn set_checkpoint_time_threshold(&self, threshold: Option<core::time::Duration>) {
        let inner = Arc::clone(&self.inner);
        let _ = tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_write();
            guard.set_checkpoint_time_threshold(threshold);
        })
        .await;
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
            handles.push(tokio::spawn(
                async move { AsyncDatabase::open_path(p).await },
            ));
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
