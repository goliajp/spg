//! v7.16.0 — `sqlx::Connection` for SPG.
//!
//! Wraps [`spg_embedded_tokio::AsyncDatabase`]. The engine is
//! single-writer at the lock level, but read-only statements
//! short-circuit through [`spg_embedded_tokio::AsyncReadHandle`]
//! — each `SpgConnection` lazily attaches a read handle on first
//! readonly statement and refreshes it per-statement so PG
//! read-committed semantics hold (every statement sees the
//! latest committed state). Writes / DDL / TX-control take the
//! writer lock as before.
//!
//! Result: `SpgPoolOptions::new().max_connections(20)` behaves
//! like its `PgPool` analogue — concurrent SELECTs run truly in
//! parallel, transactions serialise. Stock sqlx code drops in
//! unchanged.

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

/// v7.20 P3 — cached per-connection statement: the parse +
/// readonly classification + prepare-time transforms run once
/// per distinct SQL string. Repeated `query!()` call sites (the
/// sqlx norm — fixed SQL, varying binds) hit the cache and pay
/// zero parses.
#[derive(Debug, Clone)]
pub(crate) struct CachedStmt {
    pub(crate) readonly: bool,
    pub(crate) stmt: std::sync::Arc<spg_embedded::Statement>,
}

/// Cap on the per-connection statement cache. Sqlx apps use a
/// finite set of static SQL strings; 256 is far above any real
/// workload. On overflow the cache clears wholesale (simple +
/// predictable; an LRU would buy nothing at this size).
const STMT_CACHE_CAP: usize = 256;

/// One sqlx connection backed by an in-process SPG.
///
/// - `inner: AsyncDatabase` — writer path. Used for DDL / DML /
///   transaction control and statements inside a transaction.
/// - readonly statements run INLINE on the async executor
///   (v7.20 P3): per-statement snapshot via
///   `clone_snapshot_inline` (~0 µs Arc bump) + static
///   `Database::*_on_snapshot` calls (~2 µs CPU). No
///   spawn_blocking on the read path — profile_breakdown
///   measured the 3× thread-hop round-trips at 15-48 µs against
///   2 µs of actual work.
#[derive(Debug)]
pub struct SpgConnection {
    pub(crate) inner: AsyncDatabase,
    /// v7.20 P3 — per-connection statement cache.
    pub(crate) stmt_cache: HashMap<String, CachedStmt>,
    pub(crate) tx_depth: usize,
    pub(crate) pending_rollback: bool,
}

impl Clone for SpgConnection {
    fn clone(&self) -> Self {
        // Fresh cache per clone — Statements are cheap to
        // rebuild and the cache is an optimisation, not state.
        Self {
            inner: self.inner.clone(),
            stmt_cache: HashMap::new(),
            tx_depth: self.tx_depth,
            pending_rollback: self.pending_rollback,
        }
    }
}

impl SpgConnection {
    /// Build a connection from a ready `AsyncDatabase`. Called
    /// internally by [`SpgConnectOptions::connect`] and by
    /// [`crate::SpgPool::connect_in_memory`].
    pub fn new(inner: AsyncDatabase) -> Self {
        Self {
            inner,
            stmt_cache: HashMap::new(),
            tx_depth: 0,
            pending_rollback: false,
        }
    }

    /// Borrow the underlying `AsyncDatabase`. Lets advanced
    /// callers reach for the spg-embedded API directly.
    #[must_use]
    pub const fn engine(&self) -> &AsyncDatabase {
        &self.inner
    }

    /// v7.20 P3 — look up (or build + cache) the parsed
    /// statement and readonly classification for `sql`. One
    /// parse per distinct SQL string per connection.
    pub(crate) async fn cached_stmt(
        &mut self,
        sql: &str,
    ) -> Result<CachedStmt, spg_embedded::EngineError> {
        if let Some(c) = self.stmt_cache.get(sql) {
            return Ok(c.clone());
        }
        let readonly = spg_embedded::Engine::is_readonly_sql(sql);
        // Build the prepared Statement against a current snapshot
        // (prepare-time transforms read the catalog for JOIN
        // reorder). The AST stays valid across snapshots — schema
        // drift surfaces at execute time exactly like PG's
        // "cached plan must not change result type".
        let snap = self.inner.clone_snapshot_inline().await;
        let stmt = spg_embedded::Database::prepare_on_snapshot(&snap, sql)?;
        let cached = CachedStmt {
            readonly,
            stmt: std::sync::Arc::new(stmt),
        };
        if self.stmt_cache.len() >= STMT_CACHE_CAP {
            self.stmt_cache.clear();
        }
        self.stmt_cache.insert(sql.to_string(), cached.clone());
        Ok(cached)
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
//
// v7.18 — fetch_many / fetch_optional take `&mut SpgConnection`
// across the future (allowed by sqlx's `'c: 'e` bound) so the
// run_one routing can lazy-init / refresh the per-connection
// read handle without cloning state. Readonly statements
// outside a transaction fan out through the snapshot path;
// writer statements + everything inside BEGIN keep using the
// writer mutex.
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
        let outcome_fut = async move {
            match arguments {
                // Bind parameters imply exactly one statement (PG
                // rejects multi-statement extended queries too).
                Some(args) => run_one(self, &sql, Some(args)).await.map(|o| vec![o]),
                // No parameters = sqlx's simple-query / `raw_sql`
                // path. PG executes every `;`-separated statement of
                // the message server-side inside ONE implicit
                // transaction; `Database::execute_script` owns both
                // the splitting and the transaction semantics
                // (mailrs embed round-12 + v7.21 polish).
                None => Ok(self
                    .inner
                    .execute_script(&sql)
                    .await
                    .map_err(engine_to_sqlx)?
                    .into_iter()
                    .map(outcome_from)
                    .collect()),
            }
        };
        Box::pin(stream::once(outcome_fut).flat_map(|outcome| {
            let items: Vec<Result<either::Either<SpgQueryResult, SpgRow>, Error>> = match outcome {
                Ok(outcomes) => outcomes
                    .into_iter()
                    .flat_map(|o| match o {
                        Outcome::Affected(qr) => vec![Ok(either::Either::Left(qr))],
                        Outcome::Rows(rows) => rows
                            .into_iter()
                            .map(|r| Ok(either::Either::Right(r)))
                            .collect::<Vec<_>>(),
                    })
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
        Box::pin(async move {
            let args = arguments.map_err(Error::Encode)?;
            match run_one(self, &sql, args).await? {
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
        sql: &'q str,
    ) -> BoxFuture<'e, Result<sqlx_core::describe::Describe<Self::Database>, Error>>
    where
        'c: 'e,
    {
        // v7.17.0 Phase 3.P0-66 — real Describe wired through to
        // `Engine::describe_prepared`. Surfaces column metadata
        // (name / type / nullable) and a parameter count for
        // `sqlx::query!()` compile-time validation. Statement
        // shapes the describe planner can't resolve (JOIN /
        // subquery / unknown table) return an empty `columns`
        // vec — sqlx tolerates this as "no row description
        // available" and the macros fall back to offline mode
        // for those shapes.
        let inner = self.inner.clone();
        let sql_str = sql.to_string();
        Box::pin(async move {
            let (params, cols) = inner.describe(&sql_str).await.map_err(engine_to_sqlx)?;
            let nullable: Vec<Option<bool>> = cols.iter().map(|c| Some(c.nullable)).collect();
            let columns: Vec<SpgColumn> = cols
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let ti = SpgTypeInfo::from_data_type(c.ty);
                    SpgColumn::new(i, c.name.clone(), ti)
                })
                .collect();
            let parameters = if params.is_empty() {
                None
            } else {
                Some(either::Either::Right(params.len()))
            };
            Ok(sqlx_core::describe::Describe {
                columns,
                parameters,
                nullable,
            })
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
    conn: &mut SpgConnection,
    sql: &str,
    arguments: Option<crate::SpgArguments<'_>>,
) -> Result<Outcome, Error> {
    // v7.18 routing + v7.20 P3 inline read path. Inside a
    // transaction the writer lock has to stay held end-to-end so
    // the user's isolation contract holds; we never route to the
    // snapshot path there. Outside a transaction the cached
    // statement's readonly flag decides. A parse error falls
    // through to the writer path so the user sees the same parse
    // error they'd get from a simple-query SELECT.
    let in_tx = conn.tx_depth > 0;
    let cached = if in_tx {
        None
    } else {
        // Parse + classify once per distinct SQL per connection.
        conn.cached_stmt(sql).await.ok()
    };

    let result: EngineQueryResult = if let Some(c) = cached.filter(|c| c.readonly) {
        // v7.20 P3 — readonly statements run INLINE on the async
        // executor: snapshot clone is an Arc bump (~0 µs), the
        // prepared execute is ~2 µs CPU. No spawn_blocking, no
        // thread hop. Per-statement snapshot = PG read-committed.
        //
        // v7.28 (round-22) — INLINE WITH A BUDGET. Inline is perfect
        // at 2 µs and fatal at 60 s: four slow inbox queries
        // saturated mailrs's entire tokio worker pool, starving
        // every other endpoint including /logout. The inline run is
        // bounded (SPG_SQLX_INLINE_BUDGET_MS, default 25 ms); on
        // budget expiry the SAME snapshot + statement re-runs to
        // completion on the blocking pool, off the async workers.
        let snap = conn.inner.clone_snapshot_inline().await;
        let params = arguments.map(crate::SpgArguments::into_engine_values);
        let budget_ms: u64 = std::env::var("SPG_SQLX_INLINE_BUDGET_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(25);
        let started = std::time::Instant::now();
        let inline = spg_embedded::Database::execute_prepared_on_snapshot_with_budget(
            &snap,
            &c.stmt,
            params.as_deref().unwrap_or(&[]),
            budget_ms.saturating_mul(1_000),
        );
        match inline {
            Ok(r) => r,
            Err(spg_embedded::EngineError::Cancelled) => {
                let stmt = c.stmt.clone();
                let params_owned: Vec<spg_embedded::Value> =
                    params.as_deref().unwrap_or(&[]).to_vec();
                let result = tokio::task::spawn_blocking(move || {
                    spg_embedded::Database::execute_prepared_on_snapshot(
                        &snap,
                        &stmt,
                        &params_owned,
                    )
                })
                .await
                .map_err(|e| Error::Protocol(format!("blocking-pool join: {e}")))?
                .map_err(engine_to_sqlx)?;
                // v7.35.0 (mailrs ask #2, 3 reports running) — embed
                // the total elapsed time in the budget-exceeded log
                // line so the consumer can distinguish e.g. an 82 ms
                // NOT-IN form from a 200 ms correlated scan. mailrs
                // greps for "exceeded … inline budget" — the prefix
                // is unchanged, the `elapsed_ms=N` suffix is purely
                // additive.
                let elapsed_ms = started.elapsed().as_millis();
                eprintln!(
                    "spg-sqlx: readonly query exceeded the {budget_ms} ms inline budget; \
                     continuing on the blocking pool: elapsed_ms={elapsed_ms} sql={}",
                    &sql[..sql.len().min(120)]
                );
                result
            }
            Err(e) => return Err(engine_to_sqlx(e)),
        }
    } else {
        let db = &conn.inner;
        if let Some(args) = arguments {
            let stmt = db.prepare(sql).await.map_err(engine_to_sqlx)?;
            db.execute_prepared(&stmt, args.into_engine_values())
                .await
                .map_err(engine_to_sqlx)?
        } else {
            db.execute(sql).await.map_err(engine_to_sqlx)?
        }
    };
    Ok(outcome_from(result))
}

fn outcome_from(result: EngineQueryResult) -> Outcome {
    match result {
        EngineQueryResult::Rows { columns, rows } => {
            let row_values: Vec<Vec<spg_embedded::Value>> =
                rows.into_iter().map(|r| r.values).collect();
            Outcome::Rows(build_rows(&columns, row_values))
        }
        EngineQueryResult::CommandOk { affected, .. } => {
            Outcome::Affected(SpgQueryResult::new(u64::try_from(affected).unwrap_or(0)))
        }
        _ => Outcome::Affected(SpgQueryResult::default()),
    }
}

#[allow(dead_code)]
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
