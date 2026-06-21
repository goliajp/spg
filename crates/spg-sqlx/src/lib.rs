// v7.16.0 — every public item carries a doc-comment.
#![deny(missing_docs)]

//! # spg-sqlx
//!
//! sqlx 0.8 Database driver for [`spg-embedded`]. Lets
//! in-process callers swap `sqlx::PgPool` for `SpgPool` and keep
//! the rest of their `sqlx::query` / `sqlx::query_as` /
//! `pool.begin` cement unchanged — backs mailrs's drop-in
//! "PgPool → SpgPool" goal from the gap evaluation (E1).
//!
//! ## v7.16.0 MVP scope
//!
//! - [`Spg`] marker type + the 11 associated types `sqlx::Database`
//!   requires, all wired up to compile.
//! - [`SpgPool`] / [`SpgConnection`] wrap [`spg_embedded_tokio::AsyncDatabase`]
//!   so a single in-process database is the "pool". No real
//!   pooling — every "connection" handle is a cheap clone of
//!   the underlying `Arc<Mutex<Database>>`.
//! - Bind-time [`Value`][SpgValue] encoding for the basic scalar
//!   surface: `i32`, `i64`, `bool`, `String`, `Vec<u8>`. Round-trip
//!   verified end-to-end against `sqlx::query("INSERT …").bind(…)`
//!   in the test suite.
//! - Transactions via the engine's BEGIN/COMMIT/ROLLBACK; the
//!   [`SpgTransactionManager`] wraps that for `pool.begin()`.
//!
//! ## v7.16.x / v7.17 follow-up
//!
//! - Encode/Decode for the remaining mailrs-side types:
//!   TIMESTAMPTZ (`chrono::DateTime<Utc>`), JSON / JSONB
//!   (`serde_json::Value`), `tsvector`, `VECTOR(N)`,
//!   `INT[]` / `TEXT[]`, `BYTEA` (Vec<u8> beyond the basic path),
//!   numeric.
//! - `FromRow` derive support — the macro's generated impl reads
//!   columns by index/name via the [`Row`][sqlx_core::row::Row]
//!   trait, so wiring `SpgRow::try_get` is enough for the derive
//!   to "just work" once the per-type Decode lands.
//! - `sqlx::query!()` compile-time validation via sqlx's offline
//!   mode (`SQLX_OFFLINE=true` + a checked-in `.sqlx/` dir). The
//!   adapter itself doesn't need a DESCRIBE protocol —
//!   `Spg`-shaped offline cache mirrors what mailrs ships
//!   against PG today.
//!
//! ## Quick start
//!
//! ```no_run
//! use spg_sqlx::{SpgPool, SpgPoolExt};
//!
//! # async fn _f() -> Result<(), Box<dyn std::error::Error>> {
//! let pool = SpgPool::connect_in_memory().await?;
//! sqlx::query("CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL)")
//!     .execute(&pool)
//!     .await?;
//! sqlx::query("INSERT INTO users VALUES ($1, $2)")
//!     .bind(1_i32)
//!     .bind("alice")
//!     .execute(&pool)
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Concurrency, durability, and `Send + Sync` (mailrs round-9 B.4 + v7.18)
//!
//! ### `SpgPool: Send + Sync + 'static`
//!
//! [`SpgPool`] is `Pool<Spg>` from sqlx-core, which is
//! `Send + Sync + 'static` by construction. Holding it inside
//! `Arc<WebState>` for sharing across Axum/Tower handlers,
//! background workers, and long-lived spawn tasks works the
//! same as `sqlx::PgPool`. Clones are cheap (Arc bumps).
//!
//! ### Pool semantics (v7.18 — drop-in PG-shape)
//!
//! `SpgPoolOptions::new().max_connections(N).connect_with(...)`
//! behaves like its `PgPool` analogue:
//!
//! - **Concurrent SELECTs scale**. Every [`SpgConnection`]
//!   lazily attaches an `AsyncReadHandle` on first read-only
//!   statement outside a transaction, then refreshes its
//!   snapshot per statement so each SELECT sees the latest
//!   committed state (PG read-committed default). N pool
//!   connections → N concurrent reads, no writer-lock
//!   serialisation.
//! - **Writes serialise**. INSERT / UPDATE / DELETE / DDL take
//!   the writer lock — the engine is single-writer at the
//!   storage level and that invariant stays.
//! - **Transactions stay on the writer path**. Everything
//!   between `BEGIN` and `COMMIT/ROLLBACK` routes to the writer
//!   even for the same-TX SELECTs — that's how the user's
//!   uncommitted writes become visible to subsequent same-TX
//!   reads (the snapshot path would not see them).
//! - **Cross-connection read-committed**. After one connection
//!   commits, another connection's next SELECT picks up the new
//!   state via its per-statement snapshot refresh — same
//!   visibility window PG users expect.
//! - **`SpgConnectOptions` shares the underlying engine across
//!   every `connect()`**: one `Arc<RwLock<Database>>` behind a
//!   `tokio::sync::OnceCell` on the options. That's how
//!   `let mut tx = pool.begin().await?;` and a separate
//!   `pool.acquire().await?` reach the same in-process state.
//!
//! ### Escape hatch — `read_handle` for SPG-aware code
//!
//! For code paths that want to bypass sqlx entirely and hold
//! an explicit snapshot lifetime (e.g. an IMAP fetch that
//! shouldn't see writes mid-stream), reach for
//! [`spg_embedded_tokio::AsyncReadHandle`] directly via
//! [`SpgConnection::engine()`]. Stock sqlx users do not need
//! this — the routing inside [`SpgConnection`] already fans
//! out reads through that exact path under the hood.
//!
//! ### Cross-process write semantics
//!
//! **Two coexisting processes opening the same `open_path(p)`
//! are NOT serialised by SPG.** SPG-embedded is single-writer
//! at the process level: each process gets its own
//! `Arc<Mutex<Database>>`, and the WAL on disk is not
//! flock-coordinated across them. If a second process opens
//! the path while the first is running:
//!
//! - the second process replays the WAL as of its open
//!   moment and sees a snapshot of state through the last
//!   completed checkpoint + the WAL it read,
//! - subsequent writes from the second process land in its
//!   own in-memory catalog and its own WAL append,
//! - whichever process flushes last wins for the catalog
//!   snapshot on the next checkpoint, and the other process's
//!   writes are silently lost on reopen.
//!
//! For an admin-tool + server use case (mailrs round-9 B.4
//! question 1), the safe pattern is to STOP the server,
//! run the admin tool, then START the server. The
//! cross-process locking story (file lock, lease, advisory
//! lock) is a v7.17+ ask; today the contract is
//! "single-process owner per database file."
//!
//! ### WAL durability under crash
//!
//! [`spg_embedded::Database::execute`] fsyncs the WAL append
//! before returning `Ok`. So at the moment a successful
//! `execute()` returns, the write is durable across a process
//! crash AND a host power loss. On reopen,
//! [`spg_embedded::Database::open_path`] replays every
//! WAL record produced since the last checkpoint — the
//! `ZERO-CHANGE CUTOVER VERIFIED` gate (mailrs-spg-embedded
//! validation harness) covers this end to end.
//!
//! What's NOT durable:
//!
//! - A `BEGIN`-but-not-yet-`COMMIT` transaction at crash time
//!   rolls back on reopen — the in-tx WAL records aren't replayed.
//!   This is the desired behaviour: SPG's transaction model is
//!   single-writer with explicit COMMIT.
//! - The catalog snapshot file (the periodic checkpoint output)
//!   is rewritten atomically via temp-file + rename; a crash
//!   during checkpoint leaves the previous snapshot intact.
//!
//! The checkpoint threshold defaults to 4 MiB of WAL growth and
//! is configurable via
//! [`spg_embedded::Database::set_checkpoint_threshold_bytes`].
//! Lower thresholds make recovery faster (less WAL to replay)
//! at the cost of more frequent IO; higher thresholds amortise
//! IO but extend recovery time.

mod arguments;
mod column;
mod connection;
mod database;
mod error;
mod options;
mod pool;
mod query_result;
mod row;
mod statement;
mod transaction;
mod type_info;
mod types;
mod value;

pub use crate::arguments::{SpgArgumentValue, SpgArguments};
pub use crate::column::SpgColumn;
pub use crate::connection::SpgConnection;
pub use crate::database::Spg;
pub use crate::options::SpgConnectOptions;
pub use crate::pool::{SpgPool, SpgPoolExt, SpgPoolOptions};
pub use crate::query_result::SpgQueryResult;
pub use crate::row::SpgRow;
pub use crate::statement::SpgStatement;
pub use crate::transaction::SpgTransactionManager;
pub use crate::type_info::{Kind, SpgTypeInfo};
pub use crate::value::{SpgValue, SpgValueRef};

// Re-export the embedded engine's owned-value type so consumers
// don't have to depend on spg-embedded directly to construct or
// pattern-match values returned from the adapter.
pub use spg_embedded::ValueOwned as EngineValue;
