//! v7.16.0 — `sqlx::TransactionManager` for SPG.
//!
//! SPG-engine has single-writer BEGIN/COMMIT/ROLLBACK; the
//! adapter wraps that. Nested transactions are NOT supported —
//! same restriction as `spg-embedded::Database::with_transaction`.
//! Save-points map to PG's SAVEPOINT semantics in v7.16.x.

use futures_core::future::BoxFuture;
use sqlx_core::error::Error;
use sqlx_core::transaction::TransactionManager;

use crate::connection::SpgConnection;
use crate::database::Spg;
use crate::error::engine_to_sqlx;

/// Wires `Connection::begin` / `Transaction::commit` /
/// `Transaction::rollback` to engine-side
/// BEGIN/COMMIT/ROLLBACK statements.
#[derive(Debug)]
pub struct SpgTransactionManager;

impl TransactionManager for SpgTransactionManager {
    type Database = Spg;

    fn begin<'conn>(
        conn: &'conn mut SpgConnection,
        statement: Option<std::borrow::Cow<'static, str>>,
    ) -> BoxFuture<'conn, Result<(), Error>> {
        Box::pin(async move {
            let sql = statement
                .as_deref()
                .map(std::string::ToString::to_string)
                .unwrap_or_else(|| "BEGIN".to_string());
            conn.inner.execute(&sql).await.map_err(engine_to_sqlx)?;
            conn.tx_depth = conn.tx_depth.saturating_add(1);
            Ok(())
        })
    }

    fn commit(conn: &mut SpgConnection) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            if conn.tx_depth == 0 {
                return Err(engine_to_sqlx(
                    spg_embedded::EngineError::NoActiveTransaction,
                ));
            }
            conn.inner.execute("COMMIT").await.map_err(engine_to_sqlx)?;
            conn.tx_depth = conn.tx_depth.saturating_sub(1);
            Ok(())
        })
    }

    fn rollback(conn: &mut SpgConnection) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            if conn.tx_depth == 0 {
                return Err(engine_to_sqlx(
                    spg_embedded::EngineError::NoActiveTransaction,
                ));
            }
            conn.inner
                .execute("ROLLBACK")
                .await
                .map_err(engine_to_sqlx)?;
            conn.tx_depth = conn.tx_depth.saturating_sub(1);
            Ok(())
        })
    }

    fn start_rollback(conn: &mut SpgConnection) {
        // v7.16.0 — best-effort sync rollback for the Drop
        // path. We can't await here so the rollback is
        // queued by clearing the depth; the next async-aware
        // entry-point on the connection re-issues ROLLBACK if
        // needed. mailrs's transaction code always reaches
        // explicit commit/rollback under normal control flow.
        if conn.tx_depth > 0 {
            conn.tx_depth = conn.tx_depth.saturating_sub(1);
            conn.pending_rollback = true;
        }
    }

    fn get_transaction_depth(conn: &SpgConnection) -> usize {
        conn.tx_depth
    }
}
