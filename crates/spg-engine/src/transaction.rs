//! Transaction-control execution — BEGIN / COMMIT / ROLLBACK and
//! SAVEPOINT / ROLLBACK TO / RELEASE. Lifted out of `lib.rs` (v7.32
//! engine modularisation). These `impl Engine` methods are dispatched
//! from `Engine::execute` (hence pub(crate)) and drive the engine's
//! transaction + savepoint stack.

use alloc::string::String;
use alloc::vec::Vec;

use crate::{Engine, EngineError, QueryResult, TxState};

impl Engine {
    pub(crate) fn exec_begin(&mut self) -> Result<QueryResult, EngineError> {
        let tx_id = self.current_tx.ok_or(EngineError::NoActiveTransaction)?;
        if self.tx_catalogs.contains_key(&tx_id) {
            return Err(EngineError::TransactionAlreadyOpen);
        }
        self.tx_catalogs.insert(
            tx_id,
            TxState {
                catalog: self.catalog.clone(),
                savepoints: Vec::new(),
            },
        );
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    pub(crate) fn exec_commit(&mut self) -> Result<QueryResult, EngineError> {
        // v7.38 P0 元机制 A — fires at the commit barrier entry.
        // Represents "this thread is about to take the WAL group
        // commit leader slot" so tests can block here and let a
        // sibling thread arrive (`wal_group_commit_leader_chosen`
        // fires once the slot is taken — see below).
        crate::injection_point!("tx_commit_walgroup_leader_switch", &self.current_tx);
        let tx_id = self.current_tx.ok_or(EngineError::NoActiveTransaction)?;
        let state = self
            .tx_catalogs
            .remove(&tx_id)
            .ok_or(EngineError::NoActiveTransaction)?;
        // v7.38 P0 元机制 A — TX state has been moved off the
        // `tx_catalogs` map; from the WAL group commit point of
        // view, this thread is now the leader.
        crate::injection_point!("wal_group_commit_leader_chosen", &tx_id);
        self.catalog = state.catalog;
        // All savepoints become permanent at COMMIT and the stack
        // resets for the next TX (`state.savepoints` is discarded with
        // `state`).
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: true,
        })
    }

    pub(crate) fn exec_rollback(&mut self) -> Result<QueryResult, EngineError> {
        let tx_id = self.current_tx.ok_or(EngineError::NoActiveTransaction)?;
        if self.tx_catalogs.remove(&tx_id).is_none() {
            return Err(EngineError::NoActiveTransaction);
        }
        // savepoints discarded with the TxState
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    pub(crate) fn exec_savepoint(&mut self, name: String) -> Result<QueryResult, EngineError> {
        let tx_id = self.current_tx.ok_or(EngineError::NoActiveTransaction)?;
        let state = self
            .tx_catalogs
            .get_mut(&tx_id)
            .ok_or(EngineError::NoActiveTransaction)?;
        // PG re-uses an existing savepoint name by dropping the older
        // entry and pushing a fresh one — match that behaviour so
        // application code can `SAVEPOINT sp; ...; SAVEPOINT sp` freely.
        state.savepoints.retain(|(n, _)| n != &name);
        let snapshot = state.catalog.clone();
        state.savepoints.push((name, snapshot));
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    pub(crate) fn exec_rollback_to_savepoint(
        &mut self,
        name: &str,
    ) -> Result<QueryResult, EngineError> {
        let tx_id = self.current_tx.ok_or(EngineError::NoActiveTransaction)?;
        let state = self
            .tx_catalogs
            .get_mut(&tx_id)
            .ok_or(EngineError::NoActiveTransaction)?;
        let pos = state
            .savepoints
            .iter()
            .rposition(|(n, _)| n == name)
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!("savepoint not found: {name}"))
            })?;
        // The savepoint stays on the stack (PG semantics): a later
        // `RELEASE` or further `ROLLBACK TO` is still allowed. Everything
        // after it is discarded.
        let snapshot = state.savepoints[pos].1.clone();
        state.savepoints.truncate(pos + 1);
        state.catalog = snapshot;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    pub(crate) fn exec_release_savepoint(
        &mut self,
        name: &str,
    ) -> Result<QueryResult, EngineError> {
        let tx_id = self.current_tx.ok_or(EngineError::NoActiveTransaction)?;
        let state = self
            .tx_catalogs
            .get_mut(&tx_id)
            .ok_or(EngineError::NoActiveTransaction)?;
        let pos = state
            .savepoints
            .iter()
            .rposition(|(n, _)| n == name)
            .ok_or_else(|| {
                EngineError::Unsupported(alloc::format!("savepoint not found: {name}"))
            })?;
        // RELEASE keeps the work since the savepoint, just discards the
        // bookmark plus everything nested under it.
        state.savepoints.truncate(pos);
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }
}
