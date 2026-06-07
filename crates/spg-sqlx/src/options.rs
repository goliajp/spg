//! v7.16.0 — `sqlx::ConnectOptions` for SPG.
//!
//! URL scheme: `spg:` followed by either `memory` (or empty
//! path → in-memory) or a file path:
//!
//!   `spg:memory`            — in-memory database
//!   `spg:///tmp/app.db`     — file-backed, absolute path
//!   `spg:./relative.db`     — file-backed, relative path
//!
//! Future v7.16.x: TCP fallback for cases where a process
//! wants to talk to an existing spg-server via sqlx (the
//! adapter currently always opens an in-process Database, so
//! the URL is effectively a `Database::open_path` shortcut).

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use futures_core::future::BoxFuture;
use log::LevelFilter;
use sqlx_core::connection::{ConnectOptions, LogSettings};
use sqlx_core::error::Error;

use crate::connection::SpgConnection;

/// Options for opening an [`SpgConnection`].
///
/// v7.16.0 — every clone of an `SpgConnectOptions` shares the
/// same underlying `AsyncDatabase` once the first `connect()`
/// resolves. That's the key to making `sqlx::Pool<Spg>` behave
/// the way mailrs expects: `pool.begin()` and a separate
/// `pool.acquire()` on the same pool reach the same in-process
/// engine, so transaction visibility works.
#[derive(Debug, Clone)]
pub struct SpgConnectOptions {
    /// Where to open the database. `None` → in-memory.
    pub path: Option<PathBuf>,
    /// sqlx log settings — adapter-level no-op for v7.16.0 but
    /// preserved so the `log_statements` / `log_slow_statements`
    /// builders compile.
    pub log_settings: LogSettings,
    /// Lazily-initialised shared engine. Constructed on the
    /// first `connect()` call; every subsequent `connect()` on
    /// a clone of these options returns a fresh `SpgConnection`
    /// that points at the same engine. Tokio's `OnceCell` keeps
    /// concurrent initialisation safe.
    pub(crate) shared: std::sync::Arc<tokio::sync::OnceCell<spg_embedded_tokio::AsyncDatabase>>,
}

impl Default for SpgConnectOptions {
    fn default() -> Self {
        Self {
            path: None,
            log_settings: LogSettings::default(),
            shared: std::sync::Arc::new(tokio::sync::OnceCell::new()),
        }
    }
}

impl SpgConnectOptions {
    /// Construct an in-memory database options handle.
    #[must_use]
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Construct a file-backed options handle.
    #[must_use]
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
            log_settings: LogSettings::default(),
            shared: std::sync::Arc::new(tokio::sync::OnceCell::new()),
        }
    }
}

impl FromStr for SpgConnectOptions {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        // Strip `spg:` / `spg://` prefix. Anything that remains
        // is either `memory` (case-insensitive) or a file path.
        let rest = s
            .strip_prefix("spg://")
            .or_else(|| s.strip_prefix("spg:"))
            .unwrap_or(s);
        if rest.is_empty() || rest.eq_ignore_ascii_case("memory") {
            return Ok(Self::in_memory());
        }
        Ok(Self::file(rest))
    }
}

impl ConnectOptions for SpgConnectOptions {
    type Connection = SpgConnection;

    fn from_url(url: &sqlx_core::Url) -> Result<Self, Error> {
        // sqlx::Url drops the scheme; the path lives in
        // `url.path()` (with a leading `/` for absolute paths).
        // `host()` resolves to None for in-memory `spg:memory`.
        if url.scheme() != "spg" {
            return Err(Error::Configuration(
                format!("expected spg:// scheme, got {:?}", url.scheme()).into(),
            ));
        }
        let host = url.host_str().unwrap_or("");
        let path = url.path();
        let combined = match (host, path) {
            ("", "") | ("", "/") => String::new(),
            ("", p) => p.to_string(),
            (h, "") | (h, "/") => h.to_string(),
            (h, p) => format!("{h}{p}"),
        };
        SpgConnectOptions::from_str(&combined)
    }

    fn connect(&self) -> BoxFuture<'_, Result<SpgConnection, Error>> {
        let path = self.path.clone();
        let shared = std::sync::Arc::clone(&self.shared);
        Box::pin(async move {
            let inner = shared
                .get_or_try_init(|| async {
                    match path {
                        None => Ok::<_, Error>(spg_embedded_tokio::AsyncDatabase::open_in_memory()),
                        Some(p) => spg_embedded_tokio::AsyncDatabase::open_path(p)
                            .await
                            .map_err(crate::error::engine_to_sqlx),
                    }
                })
                .await?
                .clone();
            Ok(SpgConnection::new(inner))
        })
    }

    fn log_statements(mut self, level: LevelFilter) -> Self {
        self.log_settings.log_statements(level);
        self
    }

    fn log_slow_statements(mut self, level: LevelFilter, duration: Duration) -> Self {
        self.log_settings.log_slow_statements(level, duration);
        self
    }
}
