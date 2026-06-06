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
//! use spg_sqlx::SpgPool;
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
pub use crate::type_info::SpgTypeInfo;
pub use crate::value::{SpgValue, SpgValueRef};

// Re-export the embedded engine's owned-value type so consumers
// don't have to depend on spg-embedded directly to construct or
// pattern-match values returned from the adapter.
pub use spg_embedded::Value as EngineValue;
