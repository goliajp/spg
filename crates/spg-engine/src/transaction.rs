//! Transaction-control execution — BEGIN / COMMIT / ROLLBACK and
//! SAVEPOINT / ROLLBACK TO / RELEASE. Lifted out of `lib.rs` (v7.32
//! engine modularisation). These `impl Engine` methods are dispatched
//! from `Engine::execute` (hence pub(crate)) and drive the engine's
//! transaction + savepoint stack.

use alloc::string::String;
use alloc::vec::Vec;

use crate::{Engine, EngineError, QueryResult, TxState};

impl Engine {
    /// v7.38 (read01 P3.19) — replay the `SET LOCAL` undo log down to
    /// `floor`, reverting each transaction-local GUC to the value it held
    /// before it was set (or removing it if it had none). Popping to
    /// `floor = 0` reverts every local change (COMMIT / ROLLBACK); a
    /// non-zero floor reverts only the entries above a savepoint mark
    /// (`ROLLBACK TO`). Restores go through `set_session_param` so its
    /// side effects (foreign_key_checks, escape-mode plan-cache flush) are
    /// replayed too.
    pub(crate) fn restore_local_gucs_to(&mut self, floor: usize) {
        while self.local_guc_saves.len() > floor {
            let (name, prior) = self.local_guc_saves.pop().expect("len checked");
            match prior {
                Some(v) => self.set_session_param(name, spg_sql::ast::SetValue::String(v)),
                None => {
                    self.session_params.remove(&name.to_ascii_lowercase());
                }
            }
        }
    }

    /// Revert every `SET LOCAL` of the current transaction and clear the
    /// savepoint marks — used at COMMIT and ROLLBACK.
    pub(crate) fn restore_all_local_gucs(&mut self) {
        self.restore_local_gucs_to(0);
        self.savepoint_guc_marks.clear();
    }

    pub(crate) fn exec_begin(&mut self) -> Result<QueryResult, EngineError> {
        let tx_id = self.current_tx.ok_or(EngineError::NoActiveTransaction)?;
        if self.tx_catalogs.contains_key(&tx_id) {
            return Err(EngineError::TransactionAlreadyOpen);
        }
        // v7.37.15 Phase C — allocate the tx's writer version FIRST
        // (before caching any snapshot). Concurrent readers that build
        // snapshots between now and COMMIT see this version in
        // `in_progress`, so they don't observe the tx's uncommitted
        // writes. Ordering matters for Phase C.3: `current_snapshot`
        // stamps the reader's own `tx_id` from `tx_writer_versions`, so
        // the version must be registered before the RR/SER snapshot is
        // cached below — otherwise the cached snapshot would carry
        // `tx_id = 0` and the tx would not recognise its own writes.
        let v = self.begin_writer_version();
        self.tx_writer_versions.insert(tx_id, v);
        // v7.37.15 Phase E — cache an MVCC snapshot at BEGIN for
        // REPEATABLE READ / SERIALIZABLE so the tx sees a frozen
        // view across statements. READ COMMITTED (the default)
        // gets None so every statement uses a fresh snapshot.
        let cached_snapshot = match self.current_isolation_level {
            spg_sql::ast::IsolationLevel::RepeatableRead
            | spg_sql::ast::IsolationLevel::Serializable => Some(self.current_snapshot()),
            spg_sql::ast::IsolationLevel::ReadUncommitted
            | spg_sql::ast::IsolationLevel::ReadCommitted => None,
        };
        self.tx_catalogs.insert(
            tx_id,
            TxState {
                catalog: self.catalog.clone(),
                savepoints: Vec::new(),
                cached_snapshot,
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
        // v7.37.15 Phase C — mark the writer version this tx
        // allocated as committed so subsequent reader snapshots
        // observe the tx's writes. No-op if the registry never
        // saw a begin (e.g. autocommit-only paths).
        if let Some(v) = self.tx_writer_versions.remove(&tx_id) {
            self.commit_writer_version(v);
            // v7.37.15 Phase C.4 — release the tx's row locks. No-op
            // until the in-place write path (C.3) starts acquiring.
            self.release_tx_locks(v);
        }
        // All savepoints become permanent at COMMIT and the stack
        // resets for the next TX (`state.savepoints` is discarded with
        // `state`).
        // v7.38 (read01 P3.19) — SET LOCAL settings expire at the
        // transaction boundary, reverting to the pre-transaction values.
        self.restore_all_local_gucs();
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
        // v7.37.15 Phase C.2 — mark the writer version ABORTED. Under
        // today's catalog-COW model the shadow catalog never reached
        // self.catalog so the tx's rows never hit storage, making this
        // observably equivalent to commit_writer_version; but recording
        // the true terminal state is what Phase C.3's in-place write
        // path needs (a rolled-back version's xmin/xmax stamps stay in
        // place until vacuum, and the abort-aware visibility oracle
        // must hide them). Fixes the long-standing "rollback treated as
        // commit" shortcut here.
        if let Some(v) = self.tx_writer_versions.remove(&tx_id) {
            self.abort_writer_version(v);
            // v7.37.15 Phase C.4 — release the tx's row locks on abort
            // too, so a rolled-back FOR UPDATE never leaves a row locked.
            self.release_tx_locks(v);
        }
        // savepoints discarded with the TxState
        // v7.38 (read01 P3.19) — SET LOCAL settings expire at ROLLBACK too.
        self.restore_all_local_gucs();
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    pub(crate) fn exec_savepoint(&mut self, name: String) -> Result<QueryResult, EngineError> {
        let tx_id = self.current_tx.ok_or(EngineError::NoActiveTransaction)?;
        // v7.38 (read01 P3.19) — remember the SET LOCAL undo-log depth at
        // this savepoint so `ROLLBACK TO` can unwind only the later ones.
        let guc_depth = self.local_guc_saves.len();
        let state = self
            .tx_catalogs
            .get_mut(&tx_id)
            .ok_or(EngineError::NoActiveTransaction)?;
        // PG re-uses an existing savepoint name by dropping the older
        // entry and pushing a fresh one — match that behaviour so
        // application code can `SAVEPOINT sp; ...; SAVEPOINT sp` freely.
        state.savepoints.retain(|(n, _)| n != &name);
        let snapshot = state.catalog.clone();
        state.savepoints.push((name.clone(), snapshot));
        self.savepoint_guc_marks.retain(|(n, _)| n != &name);
        self.savepoint_guc_marks.push((name, guc_depth));
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
        // v7.38 (read01 P3.19) — undo any SET LOCAL made after this
        // savepoint (they roll back with the subtransaction), and drop the
        // marks nested under it.
        if let Some(mpos) = self.savepoint_guc_marks.iter().rposition(|(n, _)| n == name) {
            let floor = self.savepoint_guc_marks[mpos].1;
            self.savepoint_guc_marks.truncate(mpos + 1);
            self.restore_local_gucs_to(floor);
        }
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
        // v7.38 (read01 P3.19) — RELEASE keeps the SET LOCAL changes (they
        // survive to the outer transaction), only the bookmarks go.
        if let Some(mpos) = self.savepoint_guc_marks.iter().rposition(|(n, _)| n == name) {
            self.savepoint_guc_marks.truncate(mpos);
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }
}
