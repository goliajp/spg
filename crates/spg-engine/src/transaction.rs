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
        // v7.37.15 Phase C — allocate a writer version for the
        // duration of this explicit transaction. Concurrent
        // readers that build snapshots between now and COMMIT
        // see this version in `in_progress`, so they don't
        // observe the tx's uncommitted writes (when the tx's
        // INSERT stamps `xmin = V`, snapshots taken before the
        // exec_commit hides those rows). The version stays in
        // `active_writer_versions` until exec_commit or
        // exec_rollback fires.
        let v = self.begin_writer_version();
        // Stash the allocated version on the TxState so
        // exec_commit / exec_rollback know which version to
        // commit/discard. Reuse `next_tx_id` as a cheap registry —
        // actual storage is on TxState but the lookup key is
        // `tx_id`.
        self.tx_writer_versions.insert(tx_id, v);
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
        // v7.37.15 Phase C — mark the writer version this tx
        // allocated as committed so subsequent reader snapshots
        // observe the tx's writes. No-op if the registry never
        // saw a begin (e.g. autocommit-only paths).
        if let Some(v) = self.tx_writer_versions.remove(&tx_id) {
            self.commit_writer_version(v);
        }
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
        // v7.37.15 Phase C — release the writer version (treated
        // as a commit for the in_progress set's purposes; the
        // shadow catalog never made it into self.catalog so the
        // rows the tx stamped never reached storage).
        if let Some(v) = self.tx_writer_versions.remove(&tx_id) {
            self.commit_writer_version(v);
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
