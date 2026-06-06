//! v7.16.0 — `SpgPool` type alias + convenience constructors.

use sqlx_core::error::Error;
use sqlx_core::pool::{Pool, PoolOptions};

use crate::database::Spg;
use crate::options::SpgConnectOptions;

/// Drop-in replacement for `sqlx::PgPool` over an in-process
/// SPG. Same `Pool<Spg>` shape — every sqlx-core API generic
/// over `Pool<DB>` works against this alias.
pub type SpgPool = Pool<Spg>;

/// Pool builder hooks — re-exported for ergonomic
/// `SpgPoolOptions::new()` calls in mailrs-shape code.
pub type SpgPoolOptions = PoolOptions<Spg>;

/// Convenience constructors that mirror sqlx's pool-construction
/// shape (`PgPool::connect(url)` style). Implemented as an
/// extension trait on [`SpgPool`] so consumers can write
/// `SpgPool::connect_in_memory().await` directly.
pub trait SpgPoolExt: Sized {
    /// In-memory pool — single connection, no persistence.
    fn connect_in_memory() -> futures_core::future::BoxFuture<'static, Result<SpgPool, Error>>;
    /// File-backed pool at `path`.
    fn connect_path(
        path: std::path::PathBuf,
    ) -> futures_core::future::BoxFuture<'static, Result<SpgPool, Error>>;
}

impl SpgPoolExt for SpgPool {
    fn connect_in_memory() -> futures_core::future::BoxFuture<'static, Result<SpgPool, Error>> {
        Box::pin(async {
            SpgPoolOptions::new()
                .max_connections(1)
                .connect_with(SpgConnectOptions::in_memory())
                .await
        })
    }

    fn connect_path(
        path: std::path::PathBuf,
    ) -> futures_core::future::BoxFuture<'static, Result<SpgPool, Error>> {
        Box::pin(async move {
            SpgPoolOptions::new()
                .max_connections(1)
                .connect_with(SpgConnectOptions::file(path))
                .await
        })
    }
}
