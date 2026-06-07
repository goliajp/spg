//! v7.16.0 — `sqlx::Connection` for SPG.
//!
//! Wraps [`spg_embedded_tokio::AsyncDatabase`]. Single-writer
//! serialisation is the engine's invariant; every Connection
//! shares the same underlying database via `Arc<Mutex<…>>` so
//! `sqlx::Pool<Spg>` is a thin pass-through — the pool's
//! per-connection slot reuses the same engine, and sqlx's
//! `min_connections` / `max_connections` knobs just control
//! how many cheap clones the pool keeps around.

use std::sync::Arc;

use futures_core::future::BoxFuture;
use futures_core::stream::BoxStream;
use sqlx_core::HashMap;
use sqlx_core::connection::Connection;
use sqlx_core::error::Error;
use sqlx_core::executor::Executor;
use sqlx_core::transaction::Transaction;

use spg_embedded::QueryResult as EngineQueryResult;
use spg_embedded_tokio::AsyncDatabase;

use crate::column::SpgColumn;
use crate::database::Spg;
use crate::error::engine_to_sqlx;
use crate::options::SpgConnectOptions;
use crate::query_result::SpgQueryResult;
use crate::row::SpgRow;
use crate::type_info::SpgTypeInfo;

/// One sqlx connection backed by an in-process SPG.
#[derive(Debug, Clone)]
pub struct SpgConnection {
    pub(crate) inner: AsyncDatabase,
    pub(crate) tx_depth: usize,
    pub(crate) pending_rollback: bool,
}

impl SpgConnection {
    /// Build a connection from a ready `AsyncDatabase`. Called
    /// internally by [`SpgConnectOptions::connect`] and by
    /// [`crate::SpgPool::connect_in_memory`].
    pub fn new(inner: AsyncDatabase) -> Self {
        Self {
            inner,
            tx_depth: 0,
            pending_rollback: false,
        }
    }

    /// Borrow the underlying `AsyncDatabase`. Lets advanced
    /// callers reach for the spg-embedded API directly
    /// (e.g. `read_handle()` for fan-out reads).
    #[must_use]
    pub const fn engine(&self) -> &AsyncDatabase {
        &self.inner
    }
}

impl Connection for SpgConnection {
    type Database = Spg;
    type Options = SpgConnectOptions;

    fn close(self) -> BoxFuture<'static, Result<(), Error>> {
        // In-process — dropping the last `AsyncDatabase` clone
        // releases the engine. Nothing to send.
        Box::pin(async move { Ok(()) })
    }

    fn close_hard(self) -> BoxFuture<'static, Result<(), Error>> {
        Box::pin(async move { Ok(()) })
    }

    fn ping(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        // The engine doesn't time-out; a quick no-op query
        // exercises the lock path.
        Box::pin(async move {
            self.inner
                .execute("SELECT 1")
                .await
                .map_err(engine_to_sqlx)?;
            Ok(())
        })
    }

    fn begin(&mut self) -> BoxFuture<'_, Result<Transaction<'_, Self::Database>, Error>>
    where
        Self: Sized,
    {
        Transaction::begin(self, None)
    }

    fn shrink_buffers(&mut self) {
        // No-op — engine doesn't expose per-connection buffers.
    }

    fn flush(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move { Ok(()) })
    }

    fn should_flush(&self) -> bool {
        false
    }
}

// v7.16.0 — Executor on &mut SpgConnection. fetch_many returns
// `Either<QueryResult, Row>` per sqlx-core's stream contract.
// MVP: execute path only — INSERT/UPDATE/DELETE/DDL go through;
// SELECT (fetch_many / fetch / fetch_all) needs Decode coverage
// for the column types, lands in v7.16.x.
impl<'c> Executor<'c> for &'c mut SpgConnection {
    type Database = Spg;

    fn fetch_many<'e, 'q: 'e, E>(
        self,
        mut query: E,
    ) -> BoxStream<
        'e,
        Result<
            either::Either<
                <Self::Database as sqlx_core::database::Database>::QueryResult,
                crate::SpgRow,
            >,
            Error,
        >,
    >
    where
        'c: 'e,
        E: 'q + sqlx_core::executor::Execute<'q, Self::Database>,
    {
        use futures_util::stream::{self, StreamExt};
        let sql = query.sql().to_string();
        let arguments = match query.take_arguments() {
            Ok(args) => args,
            Err(e) => {
                return Box::pin(stream::iter(std::iter::once(Err(Error::Encode(e)))));
            }
        };
        let inner = self.inner.clone();
        let outcome_fut = async move { run_one(&inner, &sql, arguments).await };
        Box::pin(stream::once(outcome_fut).flat_map(|outcome| {
            let items: Vec<Result<either::Either<SpgQueryResult, SpgRow>, Error>> = match outcome {
                Ok(Outcome::Affected(qr)) => vec![Ok(either::Either::Left(qr))],
                Ok(Outcome::Rows(rows)) => rows
                    .into_iter()
                    .map(|r| Ok(either::Either::Right(r)))
                    .collect(),
                Err(e) => vec![Err(e)],
            };
            stream::iter(items)
        }))
    }

    fn fetch_optional<'e, 'q: 'e, E>(
        self,
        mut query: E,
    ) -> BoxFuture<'e, Result<Option<crate::SpgRow>, Error>>
    where
        'c: 'e,
        E: 'q + sqlx_core::executor::Execute<'q, Self::Database>,
    {
        let sql = query.sql().to_string();
        let arguments = query.take_arguments();
        let inner = self.inner.clone();
        Box::pin(async move {
            let args = arguments.map_err(Error::Encode)?;
            match run_one(&inner, &sql, args).await? {
                Outcome::Rows(mut rows) => Ok(rows.drain(..).next()),
                Outcome::Affected(_) => Ok(None),
            }
        })
    }

    fn prepare_with<'e, 'q: 'e>(
        self,
        sql: &'q str,
        _parameters: &'e [<Self::Database as sqlx_core::database::Database>::TypeInfo],
    ) -> BoxFuture<
        'e,
        Result<<Self::Database as sqlx_core::database::Database>::Statement<'q>, Error>,
    >
    where
        'c: 'e,
    {
        let inner = self.inner.clone();
        let sql_str = sql.to_string();
        Box::pin(async move {
            let stmt = inner.prepare(&sql_str).await.map_err(engine_to_sqlx)?;
            // The AsyncStatement wraps the embedded::Statement
            // in Arc — pull it out for the sqlx-side handle.
            // We expose the underlying handle via a tiny adapter
            // method on AsyncStatement (added on the
            // spg-embedded-tokio side).
            let inner_stmt = spg_embedded_tokio::async_statement_inner(&stmt);
            Ok(crate::SpgStatement {
                sql: std::borrow::Cow::Owned(sql_str),
                inner: Some(inner_stmt),
                columns: std::sync::Arc::new(Vec::new()),
                by_name: std::sync::Arc::new(sqlx_core::HashMap::new()),
            })
        })
    }

    fn describe<'e, 'q: 'e>(
        self,
        _sql: &'q str,
    ) -> BoxFuture<'e, Result<sqlx_core::describe::Describe<Self::Database>, Error>>
    where
        'c: 'e,
    {
        Box::pin(async move {
            Err(Error::Protocol(
                "describe is v7.17 — compile-time sqlx::query!() macros need offline mode in the meantime".into(),
            ))
        })
    }
}

/// Outcome of a single dispatch — either rows-affected (DML / DDL)
/// or a row stream (SELECT). The fetch helpers below convert this
/// to sqlx's `Either<QueryResult, Row>` stream shape.
enum Outcome {
    /// DML / DDL statement returning a rows-affected counter.
    Affected(SpgQueryResult),
    /// SELECT result — every row already converted to an
    /// [`SpgRow`].
    Rows(Vec<SpgRow>),
}

async fn run_one(
    db: &AsyncDatabase,
    sql: &str,
    arguments: Option<crate::SpgArguments<'_>>,
) -> Result<Outcome, Error> {
    // Single-dispatch: hit the engine once and inspect the
    // returned QueryResult shape. The prepared path picks up
    // the bind-final SQL via execute_prepared; the bare path
    // reuses execute(). Both surface column metadata for SELECT
    // via the engine's QueryResult::Rows variant directly so we
    // never double-run a statement.
    let result: EngineQueryResult = if let Some(args) = arguments {
        let stmt = db.prepare(sql).await.map_err(engine_to_sqlx)?;
        db.execute_prepared(&stmt, args.into_engine_values())
            .await
            .map_err(engine_to_sqlx)?
    } else {
        db.execute(sql).await.map_err(engine_to_sqlx)?
    };
    match result {
        EngineQueryResult::Rows { columns, rows } => {
            let row_values: Vec<Vec<spg_embedded::Value>> =
                rows.into_iter().map(|r| r.values).collect();
            Ok(Outcome::Rows(build_rows(&columns, row_values)))
        }
        EngineQueryResult::CommandOk { affected, .. } => Ok(Outcome::Affected(
            SpgQueryResult::new(u64::try_from(affected).unwrap_or(0)),
        )),
        _ => Ok(Outcome::Affected(SpgQueryResult::default())),
    }
}

fn affected_from(qr: &EngineQueryResult) -> u64 {
    match qr {
        EngineQueryResult::CommandOk { affected, .. } => u64::try_from(*affected).unwrap_or(0),
        EngineQueryResult::Rows { rows, .. } => u64::try_from(rows.len()).unwrap_or(0),
        _ => 0,
    }
}

fn build_rows(
    cols: &[spg_embedded::ColumnSchema],
    rows: Vec<Vec<spg_embedded::Value>>,
) -> Vec<SpgRow> {
    let columns: Arc<Vec<SpgColumn>> = Arc::new(
        cols.iter()
            .enumerate()
            .map(|(i, c)| SpgColumn::new(i, c.name.clone(), SpgTypeInfo::from_data_type(c.ty)))
            .collect(),
    );
    let mut by_name: HashMap<String, usize> = HashMap::new();
    for (i, c) in cols.iter().enumerate() {
        by_name.insert(c.name.clone(), i);
    }
    let by_name = Arc::new(by_name);
    rows.into_iter()
        .map(|values| SpgRow::new(Arc::clone(&columns), Arc::clone(&by_name), values))
        .collect()
}
