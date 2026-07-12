//! Transaction-control execution — BEGIN / COMMIT / ROLLBACK and
//! SAVEPOINT / ROLLBACK TO / RELEASE. Lifted out of `lib.rs` (v7.32
//! engine modularisation). These `impl Engine` methods are dispatched
//! from `Engine::execute` (hence pub(crate)) and drive the engine's
//! transaction + savepoint stack.

use alloc::string::String;
use alloc::vec::Vec;

use crate::{Engine, EngineError, QueryResult, TxState};

/// v7.37.17 (Phase E2 — RC rebase) — how a statement interacts with an
/// open transaction's shadow catalog, decided BEFORE dispatch (the
/// statement is moved into the executor). Conservative by default:
/// anything unclassified is `Other`, which degrades the tx to its
/// frozen SI view rather than risking a lossy rebase.
pub(crate) enum TxStmtClass {
    /// BEGIN / COMMIT / ROLLBACK / savepoints / SET TRANSACTION —
    /// transaction plumbing; neither rebases nor records.
    TxControl,
    /// Reads and session-local settings: safe to rebase before, no
    /// write-set impact.
    ReadOnly,
    /// Row DML against one named table (writable CTEs surface every
    /// CTE target too).
    Dml(Vec<String>),
    /// Everything else (DDL, COPY, unclassified) — poisons the rebase.
    Other,
}

/// Classify for the RC rebase. SELECTs with data-modifying CTEs count
/// as DML against each CTE target (PG runs them in the same tx).
pub(crate) fn classify_stmt_for_tx(stmt: &spg_sql::ast::Statement) -> TxStmtClass {
    use spg_sql::ast::{CteBody, Statement as S};
    let cte_targets = |ctes: &[spg_sql::ast::Cte]| -> Option<Vec<String>> {
        let mut targets = Vec::new();
        for c in ctes {
            match &c.body {
                CteBody::Select(_) => {}
                CteBody::Insert(i) => targets.push(i.table.clone()),
                CteBody::Update(u) => targets.push(u.table.clone()),
                CteBody::Delete(d) => targets.push(d.table.clone()),
            }
        }
        Some(targets)
    };
    match stmt {
        S::Begin
        | S::Commit
        | S::Rollback
        | S::Savepoint(_)
        | S::RollbackToSavepoint(_)
        | S::ReleaseSavepoint(_)
        | S::SetTransaction { .. } => TxStmtClass::TxControl,
        S::Select(sel) => match cte_targets(&sel.ctes) {
            Some(t) if t.is_empty() => TxStmtClass::ReadOnly,
            Some(t) => TxStmtClass::Dml(t),
            None => TxStmtClass::Other,
        },
        S::Insert(i) => {
            let mut t = alloc::vec![i.table.clone()];
            match cte_targets(&i.ctes) {
                Some(more) => t.extend(more),
                None => return TxStmtClass::Other,
            }
            TxStmtClass::Dml(t)
        }
        S::Update(u) => {
            let mut t = alloc::vec![u.table.clone()];
            match cte_targets(&u.ctes) {
                Some(more) => t.extend(more),
                None => return TxStmtClass::Other,
            }
            TxStmtClass::Dml(t)
        }
        S::Delete(d) => {
            let mut t = alloc::vec![d.table.clone()];
            match cte_targets(&d.ctes) {
                Some(more) => t.extend(more),
                None => return TxStmtClass::Other,
            }
            TxStmtClass::Dml(t)
        }
        // v7.37.17 (E4 r3) — MERGE's DELETE branch still physically
        // removes rows even under the in-place gate (no tombstone), so
        // its write-set is unextractable: a rebase would resurrect the
        // merged-away rows. Poison until MERGE DELETE is inplace-ified
        // (recorded residual).
        S::Merge(_) => TxStmtClass::Other,
        S::ShowTables
        | S::ShowDatabases
        | S::ShowCreateTable { .. }
        | S::ShowIndexes { .. }
        | S::ShowStatus
        | S::ShowVariables { .. }
        | S::ShowProcesslist
        | S::ShowColumns { .. }
        | S::ShowUsers
        | S::Explain { .. }
        | S::Empty => TxStmtClass::ReadOnly,
        _ => TxStmtClass::Other,
    }
}

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
                    self.refresh_render_style();
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

    /// v7.37.17 (Phase E2) — READ COMMITTED per-statement rebase: move
    /// the open RC tx's shadow onto the LATEST committed catalog and
    /// replay the tx's own write-set (identified by its writer version —
    /// see phase-e-read-committed-design.md route α). Runs before every
    /// non-tx-control statement; no-op unless: an explicit tx is open,
    /// its isolation caches no snapshot (RC/RU — RR/SER keep their
    /// frozen view), no DDL poisoned it, and at least one statement
    /// already ran (the BEGIN-time clone IS the latest base for the
    /// first). Replay conflicts (a row this tx tombstoned that another
    /// committed tx already removed) are skipped — PG's RC semantics;
    /// RR/SER never reach here and get serialization_failure at COMMIT
    /// instead (Phase E3). A touched table missing from the fresh base
    /// (concurrent DROP) aborts the rebase and keeps the frozen view.
    /// Returns `Err(SerializationFailure)` when the tx's INSERTed
    /// unique keys collide with a concurrently-committed writer (E4
    /// round 3): the tx's insert already ran against its old view and
    /// can't be un-run, so the statement (or COMMIT) fails with 40001
    /// and the client retries — first-committer-wins, like the FK path.
    pub(crate) fn maybe_rc_rebase(&mut self) -> Result<(), EngineError> {
        // Gate-off (legacy physical-delete) writes carry NO version
        // stamps, so the write-set is unextractable — a rebase would
        // silently lose the tx's own writes. Keep the frozen SI view,
        // which IS the legacy behaviour.
        if !self.mvcc_inplace {
            return Ok(());
        }
        let Some(tx_id) = self.current_tx else {
            return Ok(());
        };
        let Some(state) = self.tx_catalogs.get(&tx_id) else {
            return Ok(());
        };
        if state.cached_snapshot.is_some() || state.rebase_poisoned || state.stmts_run == 0 {
            return Ok(());
        }
        let Some(&v) = self.tx_writer_versions.get(&tx_id) else {
            return Ok(());
        };
        // Extract the write-sets from the OLD shadow first (immutable
        // borrows only), then build + install the fresh catalog.
        let mut wsets: Vec<(String, spg_storage::TxWriteSet)> = Vec::new();
        let mut pairs: alloc::collections::BTreeMap<
            String,
            Vec<(spg_storage::row_header::RowId, spg_storage::row_header::RowId)>,
        > = alloc::collections::BTreeMap::new();
        for tname in &state.touched_tables {
            if let Some(old_t) = state.catalog.get(tname) {
                let ws = old_t.extract_tx_writeset(v);
                if !ws.is_empty() {
                    wsets.push((tname.clone(), ws));
                }
            }
            if let Some(p) = state.update_pairs.get(tname) {
                pairs.insert(tname.clone(), p.clone());
            }
        }
        let mut fresh = self.catalog.clone();
        for (tname, ws) in &mut wsets {
            let Some(new_t) = fresh.get_mut(tname) else {
                // Concurrently dropped — a rebase would lose this tx's
                // writes; keep the frozen shadow for this statement.
                return Ok(());
            };
            // v7.37.17 (Phase E4 fix) — RC conflict semantics with
            // UPDATE atomicity: a tombstone whose target was already
            // updated/deleted by a concurrently-committed tx is
            // SKIPPED, and if it was the old half of one of this tx's
            // UPDATEs, the paired new-version insert is dropped too —
            // otherwise the row duplicates. (First-committer-wins
            // approximation of PG's EvalPlanQual re-check: the tx's
            // UPDATE ends up matching zero rows. Recorded delta: PG
            // re-applies the update to the winner's new version.)
            // A tombstone target absent from the fresh base is NOT a
            // conflict when this same tx inserted it (insert-then-delete
            // in one tx): the replay inserts it first and tombstones it
            // right after, which is exactly PG's "never visible".
            let own_inserted: alloc::collections::BTreeSet<spg_storage::row_header::RowId> =
                ws.inserted.iter().map(|(rid, _)| *rid).collect();
            let conflicted: Vec<spg_storage::row_header::RowId> = new_t
                .tombstone_conflicts(&ws.tombstoned, v)
                .into_iter()
                .filter(|rid| !own_inserted.contains(rid))
                .collect();
            if !conflicted.is_empty() {
                ws.tombstoned.retain(|rid| !conflicted.contains(rid));
                if let Some(tp) = pairs.get(tname.as_str()) {
                    let dropped_new: Vec<spg_storage::row_header::RowId> = tp
                        .iter()
                        .filter(|(old, _)| conflicted.contains(old))
                        .map(|(_, new)| *new)
                        .collect();
                    ws.inserted.retain(|(rid, _)| !dropped_new.contains(rid));
                }
            }
            // v7.37.17 (E4 r3) — TWO-PHASE replay. Tombstones land
            // first so the tx's UPDATE old-versions are dead in
            // `fresh` BEFORE the unique check (the enforcement
            // helpers skip tombstoned rows) — otherwise an UPDATE's
            // new version false-conflicts with its own old version.
            // Own insert-then-delete tombstones ride in phase 2's
            // writeset instead (their targets don't exist yet).
            let (own_cycle, plain_tombs): (Vec<_>, Vec<_>) = ws
                .tombstoned
                .iter()
                .copied()
                .partition(|rid| own_inserted.contains(rid));
            let phase1 = spg_storage::TxWriteSet {
                inserted: Vec::new(),
                tombstoned: plain_tombs,
            };
            let leftover = new_t.replay_tx_writeset(&phase1, v);
            debug_assert!(
                leftover.is_empty(),
                "conflicts were pre-filtered; tombstone replay must be clean"
            );
            let _ = new_t;
            // Unique pre-check for the inserts, against the base with
            // this tx's tombstones applied. A concurrently-taken key
            // fails the statement/COMMIT with 40001.
            {
                let inserted_rows: Vec<Vec<spg_storage::Value<'static>>> =
                    ws.inserted.iter().map(|(_, r)| r.values.clone()).collect();
                if !inserted_rows.is_empty()
                    && let Some(t_ro) = fresh.get(tname.as_str())
                {
                    let ucs = t_ro.schema().uniqueness_constraints.clone();
                    if let Err(e) = crate::constraints::enforce_uniqueness_inserts(
                        &fresh,
                        tname,
                        &ucs,
                        &inserted_rows,
                    ) {
                        return Err(EngineError::SerializationFailure(alloc::format!("{e}")));
                    }
                    if let Err(e) = crate::constraints::enforce_unique_index_inserts(
                        &fresh,
                        tname,
                        &inserted_rows,
                    ) {
                        return Err(EngineError::SerializationFailure(alloc::format!("{e}")));
                    }
                }
            }
            let Some(new_t) = fresh.get_mut(tname) else {
                return Ok(());
            };
            let phase2 = spg_storage::TxWriteSet {
                inserted: core::mem::take(&mut ws.inserted),
                tombstoned: own_cycle,
            };
            let leftover = new_t.replay_tx_writeset(&phase2, v);
            debug_assert!(
                leftover.is_empty(),
                "insert replay must be clean"
            );
        }
        if let Some(st) = self.tx_catalogs.get_mut(&tx_id) {
            st.catalog = fresh;
        }
        Ok(())
    }

    /// v7.37.17 (Phase E4 fix) — record the (old → new) RowId pairs an
    /// in-place UPDATE produced, so the RC rebase can keep the
    /// tombstone+insert halves atomic under write-write conflicts.
    /// No-op outside an explicit transaction.
    pub(crate) fn record_update_pairs(
        &mut self,
        table: &str,
        new_pairs: Vec<(spg_storage::row_header::RowId, spg_storage::row_header::RowId)>,
    ) {
        if new_pairs.is_empty() {
            return;
        }
        let Some(tx_id) = self.current_tx else { return };
        if let Some(st) = self.tx_catalogs.get_mut(&tx_id) {
            st.update_pairs
                .entry(String::from(table))
                .or_default()
                .extend(new_pairs);
        }
    }

    /// v7.37.17 (Phase E2) — post-dispatch bookkeeping for the RC
    /// rebase: count the statement and record its DML targets (or the
    /// DDL poison flag) on the open tx.
    pub(crate) fn record_tx_stmt(&mut self, class: &TxStmtClass) {
        let Some(tx_id) = self.current_tx else { return };
        let Some(st) = self.tx_catalogs.get_mut(&tx_id) else {
            return;
        };
        match class {
            TxStmtClass::TxControl => {}
            TxStmtClass::ReadOnly => st.stmts_run = st.stmts_run.saturating_add(1),
            TxStmtClass::Dml(tables) => {
                st.stmts_run = st.stmts_run.saturating_add(1);
                for t in tables {
                    st.touched_tables.insert(t.clone());
                }
            }
            TxStmtClass::Other => {
                st.stmts_run = st.stmts_run.saturating_add(1);
                st.rebase_poisoned = true;
            }
        }
    }

    /// v7.37.17 (Phase E2) — poison the open tx's rebase from inside an
    /// executor: used by writes whose shadow effect is NOT expressible
    /// as a versioned row write-set (today: the cold-tier locator
    /// shadow a PK-targeted DELETE performs). The tx keeps its frozen
    /// SI view for the rest of its life — never silently loses a write.
    pub(crate) fn poison_tx_rebase(&mut self) {
        let Some(tx_id) = self.current_tx else { return };
        if let Some(st) = self.tx_catalogs.get_mut(&tx_id) {
            st.rebase_poisoned = true;
        }
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
                touched_tables: alloc::collections::BTreeSet::new(),
                rebase_poisoned: false,
                stmts_run: 0,
                update_pairs: alloc::collections::BTreeMap::new(),
            },
        );
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    pub(crate) fn exec_commit(&mut self) -> Result<QueryResult, EngineError> {
        // v7.37.17 (Phase E2) — final rebase before the shadow is
        // installed: COMMIT replaces the whole committed catalog with
        // the shadow, so anything committed concurrently AFTER this
        // tx's last statement would be silently overwritten (lost
        // update). The RC rebase folds those commits in first. RR/SER
        // (cached snapshot) and the legacy gate-off world skip this —
        // their commit-time merge/conflict story is Phase E3; the
        // frozen-view overwrite there is the honest pre-E2 behaviour.
        // v7.37.17 (E4 r3) — a unique-key collision surfacing here
        // fails the COMMIT with 40001, rolling the tx back (PG: a
        // failed COMMIT ends the transaction).
        if let Err(e) = self.maybe_rc_rebase() {
            let tx_id = self.current_tx.ok_or(EngineError::NoActiveTransaction)?;
            self.tx_catalogs.remove(&tx_id);
            if let Some(v) = self.tx_writer_versions.remove(&tx_id) {
                self.abort_writer_version(v);
                self.release_tx_locks(v);
            }
            self.restore_all_local_gucs();
            return Err(e);
        }
        // v7.38 P0 元机制 A — fires at the commit barrier entry.
        // Represents "this thread is about to take the WAL group
        // commit leader slot" so tests can block here and let a
        // sibling thread arrive (`wal_group_commit_leader_chosen`
        // fires once the slot is taken — see below).
        crate::injection_point!("tx_commit_walgroup_leader_switch", &self.current_tx);
        let tx_id = self.current_tx.ok_or(EngineError::NoActiveTransaction)?;
        // v7.37.17 (Phase E3) — RR/SER commit-merge + write-write
        // conflict detection. A frozen-view tx COMMIT used to install
        // its shadow over the committed catalog wholesale, silently
        // overwriting everything committed since BEGIN (lost update).
        // Under the in-place gate the tx's writes are identified by its
        // writer version, so instead: extract the write-set, check it
        // against the LATEST base (a tombstone target already gone /
        // re-tombstoned, or an inserted unique key already taken, means
        // a concurrent writer won — PG's first-committer-wins), and on
        // success replay onto a fresh base clone which then installs.
        // A conflict rolls the tx back and raises
        // SerializationFailure (SQLSTATE 40001), matching PG's retry
        // contract. RC txs were already rebased before this call;
        // poisoned / legacy txs keep the wholesale install (recorded).
        if self.mvcc_inplace
            && let Some(st) = self.tx_catalogs.get(&tx_id)
            && st.cached_snapshot.is_some()
            && !st.rebase_poisoned
            && !st.touched_tables.is_empty()
            && let Some(&v) = self.tx_writer_versions.get(&tx_id)
        {
            let mut wsets: Vec<(String, spg_storage::TxWriteSet)> = Vec::new();
            for tname in &st.touched_tables {
                if let Some(old_t) = st.catalog.get(tname) {
                    let ws = old_t.extract_tx_writeset(v);
                    if !ws.is_empty() {
                        wsets.push((tname.clone(), ws));
                    }
                }
            }
            let mut fresh = self.catalog.clone();
            let mut conflict: Option<String> = None;
            'merge: for (tname, ws) in &wsets {
                let Some(new_t) = fresh.get_mut(tname) else {
                    conflict = Some(alloc::format!("table {tname:?} was dropped concurrently"));
                    break 'merge;
                };
                // v7.37.17 (E4 r3) — TWO-PHASE merge, mirroring the RC
                // rebase: apply the tx's tombstones FIRST (a target
                // already gone / re-tombstoned by another committed
                // writer is a hard 40001 here — RR never skips), then
                // check the inserts' unique keys against a base where
                // the tx's own old versions are already dead (an
                // UPDATE's new version must not false-conflict with
                // its own old version), then land the inserts.
                let own_inserted: alloc::collections::BTreeSet<
                    spg_storage::row_header::RowId,
                > = ws.inserted.iter().map(|(rid, _)| *rid).collect();
                let hard: Vec<spg_storage::row_header::RowId> = new_t
                    .tombstone_conflicts(&ws.tombstoned, v)
                    .into_iter()
                    .filter(|rid| !own_inserted.contains(rid))
                    .collect();
                if !hard.is_empty() {
                    conflict = Some(alloc::format!(
                        "{} row(s) in {tname:?} were deleted or updated by a concurrent transaction",
                        hard.len()
                    ));
                    break 'merge;
                }
                let (own_cycle, plain_tombs): (Vec<_>, Vec<_>) = ws
                    .tombstoned
                    .iter()
                    .copied()
                    .partition(|rid| own_inserted.contains(rid));
                let phase1 = spg_storage::TxWriteSet {
                    inserted: Vec::new(),
                    tombstoned: plain_tombs,
                };
                let leftover = new_t.replay_tx_writeset(&phase1, v);
                debug_assert!(leftover.is_empty(), "hard conflicts pre-checked");
                let _ = new_t;
                let inserted_rows: Vec<Vec<spg_storage::Value<'static>>> =
                    ws.inserted.iter().map(|(_, r)| r.values.clone()).collect();
                if !inserted_rows.is_empty()
                    && let Some(t_ro) = fresh.get(tname)
                {
                    let ucs = t_ro.schema().uniqueness_constraints.clone();
                    if let Err(e) = crate::constraints::enforce_uniqueness_inserts(
                        &fresh,
                        tname,
                        &ucs,
                        &inserted_rows,
                    ) {
                        conflict = Some(alloc::format!("{e}"));
                        break 'merge;
                    }
                    if let Err(e) = crate::constraints::enforce_unique_index_inserts(
                        &fresh,
                        tname,
                        &inserted_rows,
                    ) {
                        conflict = Some(alloc::format!("{e}"));
                        break 'merge;
                    }
                }
                let Some(new_t) = fresh.get_mut(tname) else {
                    conflict = Some(alloc::format!("table {tname:?} was dropped concurrently"));
                    break 'merge;
                };
                let phase2 = spg_storage::TxWriteSet {
                    inserted: ws.inserted.clone(),
                    tombstoned: own_cycle,
                };
                let leftover = new_t.replay_tx_writeset(&phase2, v);
                debug_assert!(leftover.is_empty(), "insert replay must be clean");
            }
            match conflict {
                Some(detail) => {
                    // Roll the tx back (PG: a failed COMMIT ends the tx).
                    self.tx_catalogs.remove(&tx_id);
                    if let Some(v) = self.tx_writer_versions.remove(&tx_id) {
                        self.abort_writer_version(v);
                        self.release_tx_locks(v);
                    }
                    self.restore_all_local_gucs();
                    return Err(EngineError::SerializationFailure(detail));
                }
                None => {
                    if let Some(st) = self.tx_catalogs.get_mut(&tx_id) {
                        st.catalog = fresh;
                    }
                }
            }
        }
        // v7.37.17 (Phase E4) — FK re-validation against the catalog
        // about to install. A child row this tx inserted references a
        // parent that a CONCURRENTLY-COMMITTED tx deleted: the tx's
        // insert-time FK check passed (the parent was alive in its
        // view) and the parent's delete-time reverse check passed (the
        // child was invisible in the base) — so without this final
        // check the commit would install an orphan (caught by the E4
        // matrix). PG serializes via the child's FOR KEY SHARE row
        // lock and fails the DELETE with 23503; SPG's between-
        // statements model can't retroactively fail that committed
        // DELETE, so the LOSER is this tx — first-committer-wins,
        // consistent with the update-update delta — and integrity
        // holds. RC txs are already rebased; RR/SER just merged.
        if self.mvcc_inplace
            && let Some(st) = self.tx_catalogs.get(&tx_id)
            && !st.rebase_poisoned
            && !st.touched_tables.is_empty()
            && let Some(&v) = self.tx_writer_versions.get(&tx_id)
        {
            let mut fk_conflict: Option<String> = None;
            'fk: for tname in &st.touched_tables {
                let Some(t) = st.catalog.get(tname) else {
                    continue;
                };
                let fks = t.schema().foreign_keys.clone();
                if fks.is_empty() {
                    continue;
                }
                let ws = t.extract_tx_writeset(v);
                if ws.inserted.is_empty() {
                    continue;
                }
                let rows: Vec<Vec<spg_storage::Value<'static>>> =
                    ws.inserted.iter().map(|(_, r)| r.values.clone()).collect();
                if let Err(e) =
                    crate::constraints::enforce_fk_inserts(&st.catalog, tname, &fks, &rows)
                {
                    fk_conflict = Some(alloc::format!("{e}"));
                    break 'fk;
                }
            }
            if let Some(detail) = fk_conflict {
                self.tx_catalogs.remove(&tx_id);
                if let Some(v) = self.tx_writer_versions.remove(&tx_id) {
                    self.abort_writer_version(v);
                    self.release_tx_locks(v);
                }
                self.restore_all_local_gucs();
                return Err(EngineError::SerializationFailure(detail));
            }
        }
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
        if let Some(mpos) = self
            .savepoint_guc_marks
            .iter()
            .rposition(|(n, _)| n == name)
        {
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
        if let Some(mpos) = self
            .savepoint_guc_marks
            .iter()
            .rposition(|(n, _)| n == name)
        {
            self.savepoint_guc_marks.truncate(mpos);
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }
}
