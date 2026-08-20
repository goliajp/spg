//! The `Table` storage object: row insert/update/delete, index
//! construction + rebuild (BTree / BRIN / GIN / GIN-trgm /
//! GIN-fulltext / NSW), cold-locator registration, and schema
//! mutation (add/drop/rename column). Split out of lib.rs (monster
//! tier-3 cut 4). The `Table` struct itself stays in lib.rs as
//! storage vocabulary; this module is the inherent `impl` over it.
//! `Table`'s private fields are reachable here because `table` is a
//! descendant module of the crate root where the struct is declared.

use super::*;

impl Table {
    pub fn new(schema: TableSchema) -> Self {
        Self {
            schema,
            rel_id: crate::row_header::RelId::UNASSIGNED,
            rows: PersistentVec::new(),
            headers: PersistentVec::new(),
            rowids: PersistentVec::new(),
            next_rowid: alloc::sync::Arc::new(core::sync::atomic::AtomicU64::new(1)),
            dead_rows: 0,
            stat_tup_ins: 0,
            stat_tup_upd: 0,
            stat_tup_del: 0,
            scan_stats: crate::ScanStats::default(),
            last_autovacuum_us: None,
            last_analyze_us: None,
            indices: Vec::new(),
            hot_bytes: 0,
            cold_row_count: 0,
            cold_row_count_stale: false,
            redo_log: None,
            excl_indexes: Vec::new(),
            tx_write_track: None,
            prune_horizon: 0,
        }
    }

    /// v7.37.15 (Phase C.1) — allocate the next stable [`RowId`] for
    /// this relation. Monotonic, never reused. Callers push the
    /// returned id onto `rowids` in lock-step with the `rows` /
    /// `headers` append so `rowids[i]` names the row at slot `i`.
    fn alloc_rowid(&mut self) -> crate::row_header::RowId {
        // fetch_add on the lineage-shared counter: clones (transaction
        // shadows, snapshots) mint from the SAME sequence, so ids stay
        // unique across concurrent shadows. Relaxed suffices — all
        // minting happens under the engine's single writer guard; the
        // atomic is for clone-shared identity, not for racing threads.
        let id = crate::row_header::RowId(
            self.next_rowid
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed),
        );
        id
    }

    /// v7.37.15 (Phase C.1) — read-only access to the stable row ids
    /// parallel to `rows()`. `rowids().len() == rows().len()` is the
    /// load-bearing lock-step invariant (asserted in debug builds at
    /// every mutation boundary alongside `headers`).
    #[must_use]
    pub fn rowids(&self) -> &PersistentVec<crate::row_header::RowId> {
        &self.rowids
    }

    /// v7.37.15 (Phase C.1) — this relation's stable identity.
    /// [`RelId::UNASSIGNED`](crate::row_header::RelId::UNASSIGNED) for
    /// a bare `Table::new`; a real id once the catalog stamps it.
    #[must_use]
    pub fn rel_id(&self) -> crate::row_header::RelId {
        self.rel_id
    }

    /// v7.37.15 (Phase C.1) — stamp this relation's stable identity.
    /// Called by `Catalog::create_table` and the deserialize
    /// dense-assign pass; idempotent overwrite.
    pub(crate) fn set_rel_id(&mut self, id: crate::row_header::RelId) {
        self.rel_id = id;
    }

    /// v7.37.15 (Phase C.1) — rebuild the `rowids` vec so it is dense
    /// `1..=rows.len()` and reset the allocator above it. Used on the
    /// load / snapshot-restore path where rows arrive without ids
    /// (pre-V6 envelope): every row gets a fresh id, sufficient while
    /// ids are process-local bookkeeping. Keeps the lock-step
    /// invariant against the freshly-loaded `rows`.
    pub fn assign_dense_rowids(&mut self) {
        let n = self.rows.len();
        let mut fresh: PersistentVec<crate::row_header::RowId> = PersistentVec::new();
        for i in 0..n {
            fresh.push_mut(crate::row_header::RowId((i + 1) as u64));
        }
        self.rowids = fresh;
        self.next_rowid
            .store((n as u64) + 1, core::sync::atomic::Ordering::Relaxed);
        debug_assert_eq!(
            self.rows.len(),
            self.rowids.len(),
            "rowids must stay in lock-step with rows after assign_dense_rowids"
        );
    }

    /// v7.37.16 (autovacuum) — number of tombstoned-but-present hot rows.
    /// Incrementally maintained; drives the engine's autovacuum threshold.
    #[must_use]
    pub fn dead_rows(&self) -> u64 {
        self.dead_rows
    }

    /// v7.37.16 (autovacuum) — loader-side rebase of the dead-row
    /// counter (the v53 MVCC appendix restores headers verbatim).
    pub(crate) fn set_dead_rows_on_load(&mut self, dead: u64) {
        self.dead_rows = dead;
    }

    /// v7.39 (pg_stat knife A) — bump the volatile write counters the
    /// engine's DML dispatcher reports per statement.
    pub fn bump_write_stats(&mut self, ins: u64, upd: u64, del: u64) {
        self.stat_tup_ins = self.stat_tup_ins.saturating_add(ins);
        self.stat_tup_upd = self.stat_tup_upd.saturating_add(upd);
        self.stat_tup_del = self.stat_tup_del.saturating_add(del);
    }

    /// `(n_tup_ins, n_tup_upd, n_tup_del)` for pg_stat_user_tables.
    #[must_use]
    pub fn write_stats(&self) -> (u64, u64, u64) {
        (self.stat_tup_ins, self.stat_tup_upd, self.stat_tup_del)
    }

    /// v7.39 (pg_stat knife C) — maintenance stamps for
    /// pg_stat_user_tables (`(last_autovacuum_us, last_analyze_us)`).
    #[must_use]
    pub fn maintenance_stamps(&self) -> (Option<i64>, Option<i64>) {
        (self.last_autovacuum_us, self.last_analyze_us)
    }

    pub fn stamp_autovacuum(&mut self, unix_us: i64) {
        self.last_autovacuum_us = Some(unix_us);
    }

    pub fn stamp_analyze(&mut self, unix_us: i64) {
        self.last_analyze_us = Some(unix_us);
    }

    /// v7.39 (pg_stat knife B) — the scan counters (read side of
    /// pg_stat_user_tables).
    #[must_use]
    pub fn scan_stats(&self) -> &crate::ScanStats {
        &self.scan_stats
    }

    /// v7.39 (pg_stat knife B) — one sequential scan over the visible
    /// rows, reported by engine scan loops that walk headers directly
    /// (parallel shards, the aggregate full scan) instead of
    /// `scan_visible`.
    pub fn note_seq_scan(&self) {
        use core::sync::atomic::Ordering;
        self.scan_stats.seq_scan.fetch_add(1, Ordering::Relaxed);
        let visible = (self.rows.len() as u64).saturating_sub(self.dead_rows);
        self.scan_stats
            .seq_tup_read
            .fetch_add(visible, Ordering::Relaxed);
    }

    /// v7.39 (pg_stat knife B) — one index scan returning `fetched`
    /// rows (the engine's index-seek paths report here).
    pub fn note_index_scan(&self, fetched: u64) {
        use core::sync::atomic::Ordering;
        self.scan_stats.idx_scan.fetch_add(1, Ordering::Relaxed);
        self.scan_stats
            .idx_tup_fetch
            .fetch_add(fetched, Ordering::Relaxed);
    }

    /// v7.37.15 (Phase A.2) — read-only access to the per-row
    /// MVCC visibility headers. `headers().len() == rows().len()`
    /// is the load-bearing invariant; Phase B scan paths consult
    /// `headers()[idx]` to decide visibility.
    #[must_use]
    pub fn headers(&self) -> &PersistentVec<crate::row_header::RowHeader> {
        &self.headers
    }

    /// v7.37.15 (Phase B TDD) — `#[cfg(test)]`-only mutable header
    /// access for tests that need to simulate Phase C semantics
    /// (writer-side xmin/xmax stamping) before the real stamping
    /// API lands. Phase C will provide a writer-aware setter that
    /// keeps headers + xact bookkeeping consistent.
    #[cfg(test)]
    pub(crate) fn headers_mut_for_test(
        &mut self,
    ) -> &mut PersistentVec<crate::row_header::RowHeader> {
        &mut self.headers
    }

    /// v7.37.16 (Epic W) — `#[cfg(test)]`-only read of the relation's
    /// next-RowId allocator cursor, so the snapshot round-trip tests can
    /// assert it is restored correctly (strictly above every persisted
    /// id) without a public accessor on the hot path.
    #[cfg(test)]
    pub(crate) fn next_rowid_for_test(&self) -> u64 {
        self.next_rowid.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// v7.37.15 (Phase C) — engine writer path. Same as [`insert`]
    /// but stamps `xmin` on the new row's header with the writing
    /// transaction's id (caller-supplied; obtained from the engine's
    /// monotonic version counter). The fresh insert is alive
    /// (`xmax = XMAX_ALIVE`); a later UPDATE / DELETE will set
    /// `xmax` to a later version, leaving the row physically
    /// present until vacuum reclaims it (Phase D).
    ///
    /// Callers in [`crate::row_header::next_version`] order:
    ///   1. allocate version V via `next_version()`
    ///   2. call `insert_with_xmin(row, V)`
    ///   3. update any indexes (as `insert` does)
    ///
    /// `xmin = XMIN_FROZEN` short-circuits to plain `insert`
    /// behaviour so the legacy in-memory / WAL-replay paths keep
    /// returning identical results when they end up here.
    pub fn insert_with_xmin(&mut self, row: Row<'static>, xmin: u64) -> Result<(), StorageError> {
        if xmin == crate::row_header::XMIN_FROZEN {
            return self.insert(row);
        }
        self.insert(row)?;
        // Insert appended `RowHeader::frozen()`; overwrite with the
        // alive-xmin header so visibility scans against snapshots
        // taken before the writer's commit hide this row. Subsequent
        // commit is recorded by the WAL; replay re-applies via the
        // plain `insert_no_index` path and stamps frozen — but a
        // snapshot taken AFTER commit sees `xmin = V <= snapshot.version`
        // and the in_progress bitset no longer contains V, so the
        // row passes the visibility predicate identically.
        let last = self
            .headers
            .len()
            .checked_sub(1)
            .expect("insert appended a header");
        if let Some(new_headers) = self
            .headers
            .set(last, crate::row_header::RowHeader::alive(xmin))
        {
            self.headers = new_headers;
        }
        // v7.38.2 (R2) — record the versioned insert for the rebase's
        // incremental write-set (see `Table::tx_write_track`).
        if xmin != 0 {
            let rid = self
                .rowids
                .get(last)
                .copied()
                .unwrap_or(crate::row_header::RowId::UNASSIGNED);
            self.write_track_for(xmin).inserted.push((last, rid));
        }
        debug_assert_eq!(
            self.rows.len(),
            self.headers.len(),
            "headers must stay in lock-step with rows after insert_with_xmin"
        );
        Ok(())
    }

    /// v7.38.2 (R2) — the per-table write track, claimed by `v`. A
    /// different version taking the table replaces the track (one
    /// writer per shadow; on the base this bounds memory to the last
    /// writer's footprint). See `Table::tx_write_track`.
    fn write_track_for(&mut self, v: u64) -> &mut TxWriteTrack {
        let replace = self.tx_write_track.as_ref().is_none_or(|t| t.version != v);
        if replace {
            self.tx_write_track = Some(TxWriteTrack {
                version: v,
                ..TxWriteTrack::default()
            });
        }
        self.tx_write_track.as_mut().expect("just ensured")
    }

    /// v7.39 (round 493) — publish the snapshot floor the insert path may
    /// prune dead index entries under. See `prune_horizon`.
    ///
    /// The engine sets this from `vacuum_oldest_active()` before a
    /// statement's inserts. `0` disables pruning, which is the default and
    /// is always safe.
    pub fn set_prune_horizon(&mut self, horizon: u64) {
        self.prune_horizon = horizon;
    }

    /// v7.37.15 (Phase D) — single-table vacuum pass. Walks the
    /// header vec and physically removes any row whose delete
    /// commit is older than `oldest_active_snapshot`. Returns the
    /// number of reclaimable rows (with `dry_run == true`) or the
    /// number actually reclaimed.
    ///
    /// `oldest_active_snapshot` is the floor of every live
    /// snapshot's `version` — the engine maintains this; hosts
    /// pass it through.
    ///
    /// Phase D ships the storage primitive. Hosts (spg-embedded /
    /// spg-server) schedule the pass on their own thread.
    pub fn vacuum(
        &mut self,
        oldest_active_snapshot: u64,
        dry_run: bool,
    ) -> crate::vacuum::VacuumReport {
        let examined = self.headers.len() as u64;
        // Collect the reclaimable positions in a first pass so the
        // mutation can rebuild both the rows and the headers vec
        // together (their lock-step invariant survives).
        let to_reclaim: alloc::vec::Vec<usize> = (0..self.headers.len())
            .filter(|&i| {
                self.headers
                    .get(i)
                    .map(|h| crate::vacuum::is_reclaimable(h.xmax, oldest_active_snapshot))
                    .unwrap_or(false)
            })
            .collect();
        if to_reclaim.is_empty() || dry_run {
            return crate::vacuum::VacuumReport {
                rows_reclaimed: to_reclaim.len() as u64,
                rows_examined: examined,
                per_table: alloc::vec::Vec::new(),
            };
        }
        // Drive the existing per-position delete path so both rows
        // and headers shrink together (it's the only mutator that
        // already maintains the lock-step invariant).
        let removed = self.delete_rows_no_index(&to_reclaim);
        self.rebuild_indices();
        crate::vacuum::VacuumReport {
            rows_reclaimed: removed as u64,
            rows_examined: examined,
            per_table: alloc::vec::Vec::new(),
        }
    }

    /// v7.37.15 (Phase C) — mark the row at `position` as deleted
    /// by version `xmax`. The row stays physically present; later
    /// vacuum (Phase D) reclaims it once no live snapshot can
    /// still see it.
    ///
    /// Returns `Err(Corrupt)` on out-of-bounds and silently no-ops
    /// when the row is already tombstoned (a later DELETE on an
    /// already-deleted row should not change xmax — the original
    /// deletion wins).
    pub fn mark_row_deleted(&mut self, position: usize, xmax: u64) -> Result<(), StorageError> {
        if position >= self.headers.len() {
            return Err(StorageError::Corrupt(alloc::format!(
                "mark_row_deleted: position {position} out of bounds (headers={})",
                self.headers.len()
            )));
        }
        let mut h = *self.headers.get(position).expect("position bounds-checked");
        if h.xmax != crate::row_header::XMAX_ALIVE {
            // Already tombstoned by an earlier delete. Keep the
            // original xmax — first-deleter-wins.
            return Ok(());
        }
        h.xmax = xmax;
        if let Some(new_headers) = self.headers.set(position, h) {
            self.headers = new_headers;
        }
        self.dead_rows += 1;
        // v7.38.2 (R2) — record the versioned tombstone (stable RowId).
        if xmax != 0 && xmax != crate::row_header::XMAX_ALIVE {
            let rid = self
                .rowids
                .get(position)
                .copied()
                .unwrap_or(crate::row_header::RowId::UNASSIGNED);
            self.write_track_for(xmax).tombstoned.push(rid);
        }
        // v7.37.15 (Epic W durable-tombstone slice) — capture the
        // in-place tombstone as row-level redo so a gate-on
        // (`SPG_MVCC_INPLACE`) DELETE / UPDATE-old-version /
        // ON-CONFLICT survives crash recovery. Unlike `delete_rows`
        // (which records `RowChange::Delete` with physical positions),
        // the tombstone keeps the slot, so it is named by the row's
        // stable `RowId` — read from `self.rowids()[position]` here,
        // before any later compaction shifts the slot. `xmax` is the
        // deleting statement's writer version (the engine passes
        // `writer_version_for_current_stmt`), so no post-drain stamp is
        // needed. Only paid for when redo capture is on; a no-op
        // (already-tombstoned / out-of-bounds) returned above and
        // records nothing.
        if self.redo_log.is_some() {
            let rowid = self
                .rowids()
                .get(position)
                .copied()
                .unwrap_or(crate::row_header::RowId::UNASSIGNED);
            self.record_redo(move |table| RowChange::Tombstone {
                table,
                rowids: alloc::vec![rowid],
                xmax,
            });
        }
        Ok(())
    }

    /// v7.37.16 — batch form of [`Table::mark_row_deleted`]: stamp `xmax`
    /// on every alive, in-bounds position and record ONE
    /// `RowChange::Tombstone` carrying all affected `RowId`s (the codec
    /// and replay already handle multi-rowid records). The per-row form
    /// paid one redo record — a Vec alloc plus a log push — PER ROW,
    /// ~800 ns/row on a 10k-row gate-on DELETE (heavy_write del_10k).
    /// Semantics match the single-row form: already-tombstoned keeps its
    /// original xmax (first-deleter-wins), out-of-bounds is skipped.
    /// Returns the number of rows NEWLY tombstoned.
    pub fn mark_rows_deleted(&mut self, positions: &[usize], xmax: u64) -> usize {
        let mut rowids: alloc::vec::Vec<crate::row_header::RowId> = alloc::vec::Vec::new();
        let capture = self.redo_log.is_some();
        let mut newly = 0usize;
        for &position in positions {
            // v7.37.16 — `get_mut` (transient in-place edit when the
            // headers trie is uniquely owned) instead of the `set`
            // path-copy: a 10k-row tombstone pass was spending ~3 ms in
            // per-row spine copies.
            match self.headers.get_mut(position) {
                Some(h) if h.xmax == crate::row_header::XMAX_ALIVE => {
                    h.xmax = xmax;
                }
                _ => continue, // out-of-bounds or already tombstoned
            }
            self.dead_rows += 1;
            newly += 1;
            // v7.38.2 (R2) — record the versioned tombstone.
            if xmax != 0 && xmax != crate::row_header::XMAX_ALIVE {
                let rid = self
                    .rowids
                    .get(position)
                    .copied()
                    .unwrap_or(crate::row_header::RowId::UNASSIGNED);
                self.write_track_for(xmax).tombstoned.push(rid);
            }
            if capture {
                rowids.push(
                    self.rowids()
                        .get(position)
                        .copied()
                        .unwrap_or(crate::row_header::RowId::UNASSIGNED),
                );
            }
        }
        if capture && !rowids.is_empty() {
            self.record_redo(move |table| RowChange::Tombstone {
                table,
                rowids,
                xmax,
            });
        }
        newly
    }

    /// v7.37.17 (Phase E RC rebase) — extract the write-set one writer
    /// version left on this table, expressed against stable [`RowId`]s
    /// so it can be replayed onto a FRESHER catalog clone whose
    /// physical slots differ. `inserted` carries INSERT rows and the
    /// new versions of UPDATEs (`xmin == v`); `tombstoned` carries the
    /// ids DELETE / UPDATE-old-version stamped (`xmax == v`). A row
    /// both inserted and tombstoned by the same version appears in
    /// both lists; replay applies inserts first, tombstones second —
    /// net effect identical.
    #[must_use]
    pub fn extract_tx_writeset(&self, v: u64) -> crate::TxWriteSet {
        // v7.38.2 (R2) — incremental fast path: the funnels recorded
        // exactly which slots `v` marked, so extraction is O(writes).
        // Every recorded insert is re-verified against the live header
        // and rowid; one mismatch (slot shifted, inherited track,
        // rows written before tracking) abandons the fast path for the
        // scan below — the track can be incomplete for a version it
        // does not name, never for the one it does, because claiming a
        // version resets it and every marker for that version appends.
        if let Some(track) = &self.tx_write_track
            && track.version == v
        {
            let mut inserted: alloc::vec::Vec<(crate::row_header::RowId, Row<'static>)> =
                alloc::vec::Vec::with_capacity(track.inserted.len());
            let mut ok = true;
            for &(pos, rid) in &track.inserted {
                let verified = self.headers.get(pos).is_some_and(|h| h.xmin == v)
                    && self.rowids.get(pos).copied() == Some(rid);
                if !verified {
                    ok = false;
                    break;
                }
                match self.rows.get(pos) {
                    Some(row) => inserted.push((rid, row.clone())),
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                return crate::TxWriteSet {
                    inserted,
                    tombstoned: track.tombstoned.clone(),
                };
            }
        }
        let mut inserted: alloc::vec::Vec<(crate::row_header::RowId, Row<'static>)> =
            alloc::vec::Vec::new();
        let mut tombstoned: alloc::vec::Vec<crate::row_header::RowId> = alloc::vec::Vec::new();
        for (i, h) in self.headers.iter().enumerate() {
            let rid = self
                .rowids
                .get(i)
                .copied()
                .unwrap_or(crate::row_header::RowId::UNASSIGNED);
            if h.xmin == v
                && let Some(row) = self.rows.get(i)
            {
                inserted.push((rid, row.clone()));
            }
            if h.xmax == v {
                tombstoned.push(rid);
            }
        }
        crate::TxWriteSet {
            inserted,
            tombstoned,
        }
    }

    /// v7.38.2 (R2 round 4) — the slot a RowId lives in, in O(log n).
    ///
    /// RowIds are allocated monotonically and pushed in lock-step with
    /// rows, so `rowids` is ascending everywhere except the handful of
    /// slots the rebase replay rewrote with a restored original id.
    /// Binary search answers the ascending majority and only ever
    /// returns a slot it has just VERIFIED names `rid`; anything it
    /// cannot answer falls through to the linear scan, so the fast path
    /// can only be slow, never wrong. The slot naming `rid` is THE slot:
    /// `next_rowid` is a lineage-shared `Arc<AtomicU64>`, so a shadow
    /// and the live table it rebases onto mint from ONE sequence and
    /// cannot collide.
    ///
    /// What it replaces: both rebase-path lookups walked every row per
    /// tombstone. At pgbench scale 5 (500k accounts) that alone cost
    /// 2.4x throughput at c=4 while PG18 got FASTER on the same widening
    /// — the O(rows) signature that named this attack.
    fn rowid_position(&self, rid: crate::row_header::RowId) -> Option<usize> {
        let n = self.rowids.len();
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.rowids.get(mid) {
                Some(m) if *m == rid => return Some(mid),
                Some(m) if *m < rid => lo = mid + 1,
                Some(_) => hi = mid,
                None => break,
            }
        }
        (0..n).find(|&i| self.rowids.get(i) == Some(&rid))
    }

    /// v7.37.17 (Phase E4 fix) — read-only conflict probe for a
    /// write-set's tombstones against THIS (fresher) relation: a target
    /// RowId that is gone, or already tombstoned by a DIFFERENT
    /// version, is a write-write conflict. Callers use this BEFORE
    /// `replay_tx_writeset` so a conflicting UPDATE can drop its
    /// paired insert too (atomicity of tombstone+insert pairs).
    #[must_use]
    pub fn tombstone_conflicts(
        &self,
        rids: &[crate::row_header::RowId],
        v: u64,
    ) -> alloc::vec::Vec<crate::row_header::RowId> {
        rids.iter()
            .filter(|rid| match self.rowid_position(**rid) {
                Some(i) => self
                    .headers
                    .get(i)
                    .is_some_and(|h| h.xmax != crate::row_header::XMAX_ALIVE && h.xmax != v),
                None => true,
            })
            .copied()
            .collect()
    }

    /// v7.37.17 (Phase E RC rebase) — replay a write-set extracted from
    /// an OLDER clone of this relation onto this (fresher) one, keeping
    /// the original RowIds. Deliberately does NOT capture redo: a
    /// replay re-expresses writes the transaction already made, it is
    /// not a new mutation (the redo story rides the eventual COMMIT).
    /// Returns the ids whose tombstone could not be applied because the
    /// row is gone or already tombstoned by a DIFFERENT version — the
    /// write-write conflict surface (RC skips them per PG semantics;
    /// RR/SER turn them into serialization_failure — Phase E3).
    pub fn replay_tx_writeset(
        &mut self,
        ws: &crate::TxWriteSet,
        v: u64,
    ) -> alloc::vec::Vec<crate::row_header::RowId> {
        for (rid, row) in &ws.inserted {
            // Full insert (validation + index maintenance + fresh
            // header/rowid), then re-stamp the header's xmin and put
            // the ORIGINAL RowId back. The allocator id the insert
            // burned is simply never used — ids are never recycled, so
            // a gap is harmless. Insert can only fail on schema
            // mismatch, impossible for a row this same relation
            // already accepted; a debug_assert documents that.
            let res = self.insert(row.clone());
            debug_assert!(res.is_ok(), "writeset replay re-inserts a validated row");
            if res.is_err() {
                continue;
            }
            let last = self.rows.len() - 1;
            if let Some(h) = self.headers.get_mut(last) {
                h.xmin = v;
            }
            if let Some(slot) = self.rowids.get_mut(last) {
                *slot = *rid;
            }
            // v7.38.2 (R2) — the replay marks xmin=v OUTSIDE the
            // insert funnel, so record it here too: without this a
            // SECOND rebase's fast extraction would miss every row the
            // first rebase replayed — a lost write.
            self.write_track_for(v).inserted.push((last, *rid));
        }
        let mut conflicts: alloc::vec::Vec<crate::row_header::RowId> = alloc::vec::Vec::new();
        for rid in &ws.tombstoned {
            let pos = self.rowid_position(*rid);
            match pos {
                Some(i) => match self.headers.get_mut(i) {
                    Some(h) if h.xmax == crate::row_header::XMAX_ALIVE => {
                        h.xmax = v;
                        self.dead_rows += 1;
                        // v7.38.2 (R2) — same funnel-bypass recording
                        // as the insert replay above.
                        self.write_track_for(v).tombstoned.push(*rid);
                    }
                    Some(h) if h.xmax == v => {} // already ours (idempotent)
                    _ => conflicts.push(*rid),
                },
                None => conflicts.push(*rid),
            }
        }
        conflicts
    }

    /// v7.34 (crash-recovery P0 #2) — start capturing row-level redo into
    /// this table (engine call before a mutating statement when
    /// persistence is on). Idempotent; existing captured changes are kept.
    pub fn enable_redo(&mut self) {
        if self.redo_log.is_none() {
            self.redo_log = Some(Vec::new());
        }
    }

    /// v7.34 — drain the captured redo changes and stop capturing.
    /// Returns the physical [`RowChange`]s applied since `enable_redo`,
    /// in apply order (empty when capture was off or nothing changed).
    pub fn take_redo(&mut self) -> Vec<RowChange> {
        self.redo_log.take().unwrap_or_default()
    }

    /// Record one captured change when redo capture is on. The table name
    /// rides on the change (taken from the schema) so a drained log is
    /// self-describing against the whole catalog.
    fn record_redo(&mut self, make: impl FnOnce(String) -> RowChange) {
        if self.redo_log.is_some() {
            let change = make(self.schema.name.clone());
            if let Some(log) = self.redo_log.as_mut() {
                log.push(change);
            }
        }
    }

    /// Total encoded byte size of every row currently in the hot tier
    /// (`self.rows`). See struct docs for the maintenance contract.
    /// Returns 0 for an empty table.
    #[must_use]
    pub const fn hot_bytes(&self) -> u64 {
        self.hot_bytes
    }

    /// v6.7.0 — cached count of cold-tier rows. See struct field
    /// docs for the staleness contract.
    #[must_use]
    pub const fn cold_row_count(&self) -> u64 {
        self.cold_row_count
    }

    /// v6.7.0 — overwrite the cached count. Called by the engine's
    /// `analyze_one_table` after walking the indices.
    pub fn set_cold_row_count(&mut self, n: u64) {
        self.cold_row_count = n;
        self.cold_row_count_stale = false;
    }

    /// v6.7.0 — mark the cached count as potentially out of date.
    /// Called by freezer / promote / DELETE paths so a subsequent
    /// `spg_statistic` read knows the number may not reflect the
    /// current state.
    pub fn mark_cold_row_count_stale(&mut self) {
        self.cold_row_count_stale = true;
    }

    /// v6.7.0 — report whether the cached count is known to be out
    /// of date. Exposed for completeness; the virtual table surface
    /// returns the cached value regardless.
    #[must_use]
    pub const fn cold_row_count_stale(&self) -> bool {
        self.cold_row_count_stale
    }

    /// v7.36 — O(1) "could this table possibly have cold rows?"
    /// predicate, intended for perf-critical executor hot paths
    /// that just need to skip the cold-tier branch when there's
    /// definitely nothing there. Reads the cached `cold_row_count`:
    ///   - cache fresh + cache == 0 → return false (fast path)
    ///   - cache stale → return true (conservative; the executor
    ///     pays the cold-aware path's `iter_cold_rows_*` cost but
    ///     stays correct)
    ///   - cache fresh + cache > 0 → return true
    /// `count_cold_locators` remains the right call for the EXACT
    /// count (ANALYZE etc.) — its O(N) walk is unsuitable per join
    /// stage.
    #[must_use]
    pub const fn has_cold_rows_fast(&self) -> bool {
        self.cold_row_count_stale || self.cold_row_count > 0
    }

    /// r944 — every BTree index a cold row could have been filed under.
    ///
    /// The freeze writes a row's locator into exactly ONE index
    /// (`register_cold_locators` takes a single index name) and the
    /// freezer picks that index by its own rule, so a reader that guesses
    /// a different one finds nothing. Round 943 is that bug: the freezer
    /// chose the first BTree index over any integer column, the scan
    /// looked at the first index on the primary key's column, and 15
    /// frozen rows of 40 vanished from a plain `SELECT`.
    ///
    /// Union over all of them rather than guessing one. Because each
    /// row's locator exists in exactly one index, the union yields every
    /// row once and needs no visited-set.
    ///
    /// Deliberately NOT filtered to declared-unique indices. Freezing
    /// through an index whose keys repeat is a real limitation —
    /// `resolve_cold_locator` resolves BY KEY and cannot say which of two
    /// rows sharing one was meant — but that limit belongs to the freeze,
    /// which builds the segment keyed that way. Filtering it here only
    /// hides rows that were frozen anyway, which is the bug rather than a
    /// guard against it; the freezer's own tests freeze tables whose
    /// integer index carries no uniqueness constraint.
    pub fn cold_capable_indices(&self) -> impl Iterator<Item = &Index> {
        self.indices
            .iter()
            .filter(|i| matches!(i.kind, IndexKind::BTree(_)))
    }

    /// v6.7.0 — walk every BTree index and count `RowLocator::Cold`
    /// entries; return the MAX across indices. The freeze path
    /// (`freeze_oldest_to_cold`) writes cold locators to ONE
    /// designated index — that index ends up with the full per-row
    /// count. MAX-across-indices yields the precise count when a
    /// PK-style index exists; for multi-index tables without a
    /// covering index it's a lower bound (rare in practice).
    /// Caller responsibility: only invoke under `engine.write()`
    /// or after taking ownership; the walk is O(N) over every
    /// (key, locator) pair.
    #[must_use]
    pub fn count_cold_locators(&self) -> u64 {
        let mut best: u64 = 0;
        for idx in &self.indices {
            if let IndexKind::BTree(map) = &idx.kind {
                let n: u64 = map
                    .iter()
                    .map(|(_, locs)| locs.iter().filter(|l| l.is_cold()).count() as u64)
                    .sum();
                if n > best {
                    best = n;
                }
            }
        }
        best
    }

    pub const fn schema(&self) -> &TableSchema {
        &self.schema
    }

    /// v6.7.2 — mutable schema accessor for ALTER TABLE paths.
    /// Used by `Engine::exec_alter_table` to flip per-table
    /// settings like `hot_tier_bytes`.
    pub const fn schema_mut(&mut self) -> &mut TableSchema {
        &mut self.schema
    }

    /// v4.39: returns the persistent row vector by reference. Callers that
    /// used to take `&[Row]` should switch to `.iter()` (via
    /// `IntoIterator for &PersistentVec`) or `.get(i)` for indexing.
    /// v7.38.11 — the column positions this table has BRIN indexes on.
    pub fn brin_columns(&self) -> alloc::vec::Vec<usize> {
        self.indices
            .iter()
            .filter(|i| matches!(i.kind, crate::IndexKind::Brin { .. }))
            .map(|i| i.column_position)
            .collect()
    }

    /// v7.38.11 — the slot ranges a BRIN index cannot rule out for
    /// `col_pos` under `lo <= x` / `x <= hi`, or `None` when there is
    /// no BRIN index on that column.
    ///
    /// `None` and "every slot" are deliberately different answers:
    /// `None` means this table has nothing to say, so a caller that
    /// does not understand BRIN keeps scanning exactly as before.
    ///
    /// A range is skipped only when its summary PROVES no row in it can
    /// match. A range with no summary — never written, or written only
    /// with values this index cannot order — is always kept. The
    /// predicate still runs on every row that survives: the summary
    /// decides what to skip, never what to return.
    #[must_use]
    pub fn brin_candidate_slots(
        &self,
        col_pos: usize,
        lo: Option<i64>,
        hi: Option<i64>,
    ) -> Option<alloc::vec::Vec<core::ops::Range<usize>>> {
        let summaries = self.indices.iter().find_map(|idx| match &idx.kind {
            crate::IndexKind::Brin { summaries, .. } if idx.column_position == col_pos => {
                Some(summaries)
            }
            _ => None,
        })?;
        let n = self.rows.len();
        let mut out: alloc::vec::Vec<core::ops::Range<usize>> = alloc::vec::Vec::new();
        let mut start = 0usize;
        while start < n {
            let end = (start + crate::BRIN_RANGE_ROWS).min(n);
            let keep = match summaries.get(start / crate::BRIN_RANGE_ROWS) {
                // Proven disjoint from the predicate's interval.
                Some(Some((rmin, rmax))) => {
                    !(lo.is_some_and(|l| *rmax < l) || hi.is_some_and(|h| *rmin > h))
                }
                // No summary: nothing is proven, so nothing is skipped.
                _ => true,
            };
            if keep {
                match out.last_mut() {
                    Some(last) if last.end == start => last.end = end,
                    _ => out.push(start..end),
                }
            }
            start = end;
        }
        Some(out)
    }

    pub const fn rows(&self) -> &PersistentVec<Row<'static>> {
        &self.rows
    }

    pub const fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// v7.37.15 (Phase B) — answer "is row at `idx` visible under
    /// `snapshot`?" without exposing the header internals to the
    /// engine. Callers in scan paths consult this BEFORE yielding
    /// the row.
    ///
    /// Defensive: out-of-bounds `idx` and the (impossible, asserted)
    /// length mismatch return `false`, mirroring "row is not there
    /// so it's not visible." Production scans never see either.
    ///
    /// Phase A always returns `true` because every header is
    /// `RowHeader::frozen()` and `Snapshot::unbounded()` accepts
    /// every header. The full visibility behaviour engages once
    /// Phase C writers start stamping real `xmin`/`xmax`.
    #[must_use]
    pub fn is_row_visible(&self, idx: usize, snapshot: &crate::snapshot::Snapshot) -> bool {
        match self.headers.get(idx) {
            Some(h) => self.header_visible(idx, h, snapshot),
            None => false,
        }
    }

    /// v7.39 (round 486) — the visibility decision once the header is
    /// already in hand. `scan_visible` walks rows and headers in
    /// lockstep, so it has the header without paying a second trie
    /// descent to look it up by index.
    fn header_visible(
        &self,
        idx: usize,
        h: &crate::row_header::RowHeader,
        snapshot: &crate::snapshot::Snapshot,
    ) -> bool {
        // v7.39 (round 297, E3 Phase 1b) — `SKIP LOCKED` rides here so
        // that every row source honours it; see `Snapshot::locked_out`.
        if let Some((rel, set)) = &snapshot.locked_out
            && *rel == self.rel_id
            && set.contains(&idx)
        {
            return false;
        }
        snapshot.visible(h)
    }

    /// v7.37.15 (Phase D) — true iff every row in this table is
    /// known-all-visible to every snapshot (frozen xmin + alive
    /// xmax). When true, `scan_visible` skips the per-row check
    /// entirely — the scan degenerates to a plain `rows().iter()`.
    ///
    /// Maintained lazily: any insert/update that stamps a non-
    /// frozen xmin / xmax clears the cached flag; the next call to
    /// this method recomputes by walking the header vec. The walk
    /// is O(n) in the rare case (only when an MVCC writer ran on
    /// this table); steady-state legacy workloads hit the cached
    /// `true` and scan at pre-v7.37.15 speed.
    ///
    /// Phase D wires this into the engine's hot-tier scan
    /// optimisation; the bit also serves the per-segment all-
    /// visible bitmap (each cold segment is a separately tracked
    /// `all_visible` bit, but cold segments are frozen wholesale
    /// so they're trivially `true`).
    #[must_use]
    pub fn is_all_visible(&self) -> bool {
        // Compute on the fly. Caching is a follow-up optimisation
        // (would require &mut self or a Cell); the v7.37.15
        // initial ship favours correctness + simplicity over the
        // amortised constant.
        self.headers
            .iter()
            .all(crate::row_header::RowHeader::is_all_visible_fast)
    }

    /// v7.37.15 (Phase B / D) — iterate over `(idx, row)` pairs whose
    /// header is visible under `snapshot`. This is the engine-side
    /// drop-in replacement for `for (i, r) in t.rows().iter().enumerate()`
    /// at scan sites. The check is a single branch + atomic
    /// register read inside the snapshot path; with `Snapshot::unbounded`
    /// the optimiser folds the gate away.
    ///
    /// `'a` lifetime on `snapshot` keeps the helper zero-cost in
    /// the hot loop — no Arc bump, no allocation.
    /// v7.39 (round 560) — is the row at this position visible to the
    /// snapshot? Exposed so an index-only walk can decide without
    /// fetching the row it is deciding about.
    #[must_use]
    pub fn position_visible(&self, idx: usize, snapshot: &crate::snapshot::Snapshot) -> bool {
        self.headers
            .get(idx)
            .is_some_and(|h| self.header_visible(idx, h, snapshot))
    }

    /// v7.39 (round 562) — the same question asked many times over
    /// ascending positions, without descending the header trie for each
    /// one.
    ///
    /// A profile of the server serving a 100k-row index-only range put
    /// 27% of the connection thread's CPU on the per-row visibility test.
    /// The headers are a `PersistentVec` — a 32-way trie — so
    /// `position_visible` is four dependent pointer loads per row. A
    /// sequential scan never pays that: it walks rows and headers in
    /// lockstep. An index walk cannot, but its positions arrive in
    /// ascending order and a leaf holds 32 of them, so keeping the run
    /// between calls turns 32 descents into one.
    ///
    /// A position outside the held run just descends, so an index whose
    /// order is uncorrelated with position costs what it costs today.
    #[must_use]
    pub fn header_runs(&self) -> HeaderRuns<'_> {
        HeaderRuns {
            table: self,
            run: None,
        }
    }

    /// v7.39 (round 559) — how many rows a snapshot sees, without
    /// touching a single one of them.
    ///
    /// `count(*)` already short-circuits to `rows.len()` in the
    /// aggregate layer, so the O(1) part was never the problem: the cost
    /// is UPSTREAM, materialising every visible row so that layer can
    /// take its length. `scan_visible` zips the row trie with the
    /// headers, and a count needs only the headers.
    ///
    /// Measured over pgwire on 500k rows, `SELECT count(*)`:
    ///
    /// ```text
    ///     PG18 (2 parallel workers)   8.2 ms
    ///     PG18 (parallelism off)     10.3 ms
    ///     SPG                        16.5 ms   = 33 ns/row
    /// ```
    ///
    /// — 1.6x slower than a single-threaded PG on the commonest
    /// aggregate there is, which no ledger entry recorded.
    pub fn count_visible(&self, snapshot: &crate::snapshot::Snapshot) -> usize {
        self.note_seq_scan();
        self.headers
            .iter()
            .enumerate()
            .filter(|(i, h)| self.header_visible(*i, h, snapshot))
            .count()
    }

    pub fn scan_visible<'a, 'b>(
        &'a self,
        snapshot: &'b crate::snapshot::Snapshot,
    ) -> impl Iterator<Item = (usize, &'a Row<'static>)> + 'b
    where
        'a: 'b,
    {
        // v7.39 (pg_stat knife B) — one sequential scan; tup_read is
        // the visible-row estimate (an early-terminating consumer —
        // LIMIT — reads fewer; the lazy iterator can't report back).
        // Two relaxed atomic adds per SCAN (not per row).
        self.note_seq_scan();
        // v7.39 (round 486) — headers ride alongside the rows instead of
        // being looked up by index. `headers.len() == rows.len()` is an
        // asserted invariant, so the zip drops nothing; an index lookup
        // costs a trie descent per row, and the walk costs one per leaf.
        self.rows
            .iter()
            .zip(self.headers.iter())
            .enumerate()
            .filter(move |(i, (_, h))| self.header_visible(*i, h, snapshot))
            .map(|(i, (r, _))| (i, r))
    }

    /// The hot-tier slot a scan should resume at, given the last
    /// [`RowId`](crate::row_header::RowId) it consumed and where that row
    /// used to sit.
    ///
    /// Slots move. `vacuum` reclaims tombstones by rebuilding the row
    /// vector, so every position after the first reclaimed one shifts
    /// down — a reader that remembered a bare index would silently skip
    /// or repeat rows. Row ids do not move: they are allocated
    /// monotonically and never reused, which makes them the only stable
    /// way to say "carry on after this row".
    ///
    /// `hint` is the position that row occupied when it was read. It is
    /// still right whenever nothing was reclaimed under the reader, so
    /// the check costs one lookup; the binary search is the fallback for
    /// when it is not, and it works because appends only ever push
    /// larger ids and reclaiming preserves their order.
    pub fn resume_slot_after(&self, last: crate::row_header::RowId, hint: usize) -> usize {
        if hint > 0 && self.rowids.get(hint - 1).is_some_and(|&r| r == last) {
            return hint;
        }
        let (mut lo, mut hi) = (0usize, self.rowids.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.rowids.get(mid) {
                Some(&r) if r <= last => lo = mid + 1,
                _ => hi = mid,
            }
        }
        lo
    }

    /// The same visibility-gated walk as [`Table::scan_visible`], resuming
    /// at hot-tier index `start`.
    ///
    /// A server-side cursor hands out its result in batches and has to
    /// continue where the previous batch stopped. Restarting the walk per
    /// batch and discarding a growing prefix would make an N-batch drain
    /// quadratic in the row count, so the resume point is a parameter
    /// rather than something the caller skips over.
    ///
    /// `start` is a hot-tier position, not a [`RowId`](crate::row_header::RowId):
    /// callers that resume across a compaction must re-derive it, which is
    /// why the cursor path only resumes tables with no cold segments.
    ///
    /// `note_seq_scan` fires only for `start == 0`. One cursor drained in
    /// 300 batches is one sequential scan of the table, and counting it
    /// 300 times would misreport `pg_stat_user_tables.seq_scan`.
    pub fn scan_visible_from<'a, 'b>(
        &'a self,
        start: usize,
        snapshot: &'b crate::snapshot::Snapshot,
    ) -> impl Iterator<Item = (usize, &'a Row<'static>)> + 'b
    where
        'a: 'b,
    {
        if start == 0 {
            self.note_seq_scan();
        }
        self.rows
            .iter()
            .zip(self.headers.iter())
            .enumerate()
            .skip(start)
            .filter(move |(i, (_, h))| self.header_visible(*i, h, snapshot))
            .map(|(i, (r, _))| (i, r))
    }

    /// v7.38.11 — like [`Table::scan_visible_from`] but visiting only
    /// the slots a BRIN summary could not rule out.
    ///
    /// The caller passes ranges it got from
    /// [`Table::brin_candidate_slots`]; passing `0..len` gives exactly
    /// the same rows in the same order as the unpruned scan, which is
    /// how the callers that have no BRIN index keep their behaviour.
    pub fn scan_visible_slots<'a, 'b>(
        &'a self,
        slots: alloc::vec::Vec<core::ops::Range<usize>>,
        snapshot: &'b crate::snapshot::Snapshot,
    ) -> impl Iterator<Item = (usize, &'a Row<'static>)> + 'b
    where
        'a: 'b,
    {
        self.note_seq_scan();
        slots.into_iter().flatten().filter_map(move |i| {
            let h = self.headers.get(i)?;
            if !self.header_visible(i, h, snapshot) {
                return None;
            }
            self.rows.get(i).map(|r| (i, r))
        })
    }

    /// v6.8.0 — exposed for the engine layer to patch
    /// `Index::included_columns` post-creation. Could fold into
    /// `add_index` once the engine's IF-NOT-EXISTS guard moves up,
    /// but the patch shape is the minimal change for v6.8.0.
    pub fn indices_mut(&mut self) -> &mut [Index] {
        &mut self.indices
    }

    pub fn indices(&self) -> &[Index] {
        &self.indices
    }

    /// Compute the next `AUTO_INCREMENT` value for the column at
    /// `col_pos`. Defined as `max(existing) + 1`, falling back to `1`
    /// when the column currently holds no integer values. NULL / non-
    /// integer cells are skipped. Returns `None` when the column isn't
    /// an integer type.
    pub fn next_auto_value(&self, col_pos: usize) -> Option<i64> {
        let ty = self.schema.columns.get(col_pos)?.ty;
        if !matches!(ty, DataType::SmallInt | DataType::Int | DataType::BigInt) {
            return None;
        }
        let mut max: Option<i64> = None;
        for row in &self.rows {
            match row.values.get(col_pos) {
                Some(Value::SmallInt(n)) => {
                    let v = i64::from(*n);
                    max = Some(max.map_or(v, |m| m.max(v)));
                }
                Some(Value::Int(n)) => {
                    let v = i64::from(*n);
                    max = Some(max.map_or(v, |m| m.max(v)));
                }
                Some(Value::BigInt(n)) => {
                    max = Some(max.map_or(*n, |m| m.max(*n)));
                }
                _ => {}
            }
        }
        // v7.39 (round 220) — `ALTER … ALTER COLUMN … RESTART [WITH n]`
        // lifts the next allocated value to at least n (a floor over the
        // max+1 scan). A dump-restore RESTART lands exactly on n; a
        // backward RESTART is safely ignored (no duplicate-key landmine,
        // unlike PG).
        let base = max.map_or(1, |m| m + 1);
        let floor = self
            .schema
            .columns
            .get(col_pos)
            .and_then(|c| c.auto_restart)
            .unwrap_or(i64::MIN);
        Some(base.max(floor))
    }

    /// Return the first index defined over `column_position`, if any.
    /// (`v0.8` supports at most one index per column logically; the search
    /// just picks the first match.)
    pub fn index_on(&self, column_position: usize) -> Option<&Index> {
        // v6.7.1 — prefer BTree (has the key→locator map needed
        // for `lookup_eq`) over BRIN (metadata-only). When only a
        // BRIN exists on the column, return None so the executor
        // falls back to the hot-tier row scan instead of trying
        // to use BRIN for an equality lookup (which would always
        // return an empty slice and look like "no rows matched").
        self.indices
            .iter()
            .find(|i| i.column_position == column_position && matches!(i.kind, IndexKind::BTree(_)))
            .or_else(|| {
                self.indices.iter().find(|i| {
                    i.column_position == column_position && matches!(i.kind, IndexKind::Nsw(_))
                })
            })
    }

    /// Insert one row after validating it matches the schema (length + type).
    /// Returns `StorageError` on mismatch — the table is left unchanged.
    /// Updates every defined index with the new row's key.
    pub fn insert(&mut self, row: Row<'static>) -> Result<(), StorageError> {
        if row.len() != self.schema.columns.len() {
            return Err(StorageError::ArityMismatch {
                expected: self.schema.columns.len(),
                actual: row.len(),
            });
        }
        for (i, (val, col)) in row.values.iter().zip(&self.schema.columns).enumerate() {
            if val.is_null() {
                if !col.nullable {
                    return Err(StorageError::NullInNotNull {
                        column: col.name.clone(),
                    });
                }
                continue;
            }
            // v7.39 (read01 round 54) — `data_type()` is None for the
            // eval-only variants that carry no DataType (RegClass, Composite).
            // They are NOT NULL, so `.expect("non-null")` PANICKED on them —
            // materialising a CTE like `WITH w AS (SELECT 't'::regclass)` blew
            // up the query with an "internal error". Report a clean type
            // mismatch instead; the engine coerces these before they get here
            // on every path that knows how.
            let Some(actual) = val.data_type() else {
                // An eval-only value (RegClass carries oid + name, Composite a
                // field tuple) has no DataType in the storage lattice. It is
                // NOT NULL, so the old `.expect("non-null")` PANICKED — which
                // is how `WITH w AS (SELECT 't'::regclass)` blew up with an
                // "internal error". Accept it: the value keeps its dual shape
                // and downstream comparisons (RegClass vs BigInt oid) handle it.
                continue;
            };
            // A Vector column needs the variant AND the dimension to
            // agree, which the equality inside `column_accepts` already
            // encodes because DataType::Vector carries the dim.
            let compatible = column_accepts(actual, col.ty);
            if !compatible {
                return Err(StorageError::TypeMismatch {
                    column: col.name.clone(),
                    expected: col.ty,
                    actual,
                    position: i,
                });
            }
        }
        let new_row_idx = self.rows.len();
        // v7.39 (round 493) — disjoint borrows: the BTree arm below reads
        // headers to decide which of this key's locators are dead while
        // holding `indices` mutably.
        let horizon = self.prune_horizon;
        let headers = &self.headers;
        // Pre-validate before mutating: ensure indices receive an IndexKey.
        // For NSW we defer the graph update to *after* the row is pushed
        // so the kNN search can see it in `self.rows`.
        for idx in &mut self.indices {
            match &mut idx.kind {
                IndexKind::BTree(map) => {
                    if let Some(key) = IndexKey::from_value(&row.values[idx.column_position]) {
                        // v4.40: PersistentBTreeMap has no in-place entry-or-default.
                        // Clone-then-insert keeps the same semantics — for typical
                        // unique-key schemas the Vec is 1-element so the clone is
                        // O(1). For dup-heavy columns it's O(M) per insert, traded
                        // for the structural-sharing win at clone time.
                        //
                        // v7.39 (round 558) — TAKE the list instead of cloning it.
                        // `insert_mut` returns the previous value by MOVE, so the
                        // O(M) copy the note above accepted is avoidable, and the
                        // retain below still gets the list in hand. What that
                        // trade cost, measured on a 50k table:
                        //
                        //   UPDATE h SET v = 1 WHERE v <= 10000   (10k -> ONE key)
                        //     v indexed 150.6 ms   v unindexed 31.2 ms
                        //   UPDATE h SET v = v + 1 WHERE v <= 10000 (distinct keys)
                        //     v indexed  34.7 ms   v unindexed 32.0 ms
                        //
                        // 11.9 µs/row when the new keys collide against 0.27 when
                        // they do not — 44x for the same row count, because the
                        // k-th insert under one key copied a k-element list. Under
                        // in-place MVCC an UPDATE appends a new row VERSION, so an
                        // ordinary `SET flag = 'done'` over a batch lands every
                        // one of them on the same key.
                        let mut entries = map
                            .insert_mut(key.clone(), crate::posting::PostingList::new())
                            .unwrap_or_default();
                        // v7.39 (round 493) — drop this key's dead versions while
                        // the list is already in hand.
                        //
                        // "The Vec is 1-element for unique-key schemas" is what
                        // churn breaks: a posting list carries one locator per row
                        // VERSION, so deleting and re-inserting the same id grows
                        // it without bound between vacuums. Round 492 counted 61
                        // locators under one PK by cycle 60, each costing the
                        // uniqueness probe a header lookup, and round 490 found the
                        // range seek walking the same versions.
                        //
                        // Vacuum already prunes them — by rebuilding every index,
                        // which is why it runs rarely enough for this to matter.
                        // Here the work is free: the list is cloned on this path
                        // anyway and is about to be written back.
                        //
                        // Safety is vacuum's own argument: `prune_horizon` is the
                        // floor of every live snapshot, so a version reclaimable
                        // under it is invisible to every reader that exists or can
                        // yet begin (a later snapshot's version is >= the floor).
                        // A horizon of 0 keeps everything.
                        // v7.39 (round 558) — AMORTISE it.
                        //
                        // The retain walks the whole list, so running it on
                        // every insert is O(M) per insert and O(n²) over a
                        // statement that puts n row versions under one key.
                        // Measured on a 50k table, 10k rows updated:
                        //
                        //                       retain every insert   off
                        //   SET v = 1  (dupes)        135.9 ms       11.7
                        //   SET v = v+1 (distinct)     31.4 ms       13.4
                        //
                        // and the second line has no colliding key at all —
                        // the OTHER index (g, 100 distinct values over 50k
                        // rows) supplies lists long enough on its own. Every
                        // insert on every index was paying it.
                        //
                        // Pruning only when the list has DOUBLED keeps round
                        // 493's bound — the list stays within 2x its pruned
                        // size, so the seek still never walks an unbounded
                        // version chain — while the total work over n inserts
                        // becomes n + n/2 + n/4 + … = O(n). Skipping a prune
                        // can only delay reclamation; it never drops a live
                        // locator, so the safety argument in the note above is
                        // untouched.
                        if horizon > 0 && entries.len() > 1 && entries.len().is_power_of_two() {
                            entries.retain(|loc| match loc {
                                RowLocator::Hot(i) => headers.get(i).is_none_or(|h| {
                                    !crate::vacuum::is_reclaimable(h.xmax, horizon)
                                }),
                                RowLocator::Cold { .. } => true,
                            });
                        }
                        entries.push(RowLocator::Hot(new_row_idx));
                        map.insert_mut(key, entries);
                    }
                }
                // v7.38.1 (L12) — multi-column key: every component must
                // key, or the row is not entered (a `=` probe can never
                // select the NULL it would stand for). The take/prune/
                // push dance is the BTree arm's, for the same churn
                // reasons.
                IndexKind::BTreeMulti(map) => {
                    if let Some(key) = crate::compose_multi_key(
                        &row.values,
                        idx.column_position,
                        &idx.extra_column_positions,
                    ) {
                        let mut entries = map
                            .insert_mut(key.clone(), crate::posting::PostingList::new())
                            .unwrap_or_default();
                        if horizon > 0 && entries.len() > 1 && entries.len().is_power_of_two() {
                            entries.retain(|loc| match loc {
                                RowLocator::Hot(i) => headers.get(i).is_none_or(|h| {
                                    !crate::vacuum::is_reclaimable(h.xmax, horizon)
                                }),
                                RowLocator::Cold { .. } => true,
                            });
                        }
                        entries.push(RowLocator::Hot(new_row_idx));
                        map.insert_mut(key, entries);
                    }
                }
                IndexKind::Gin(map) => {
                    // v7.12.3 — extend posting list per lexeme word.
                    // NULL or non-TsVector cell → no-op (cell carries
                    // no lexemes to index).
                    if let Value::TsVector(lexemes) = &row.values[idx.column_position] {
                        for lex in lexemes {
                            if let Some(entries) = map.get_mut(&lex.word) {
                                entries.push(RowLocator::Hot(new_row_idx));
                            } else {
                                map.insert_mut(
                                    lex.word.clone(),
                                    crate::posting::PostingList::single(RowLocator::Hot(
                                        new_row_idx,
                                    )),
                                );
                            }
                        }
                    }
                }
                IndexKind::GinTrgm(map) => {
                    // v7.15.0 — trigram GIN. Shingle the TEXT cell
                    // into PG-compatible 3-byte trigrams and extend
                    // each trigram's posting list.
                    if let Value::Text(s) = &row.values[idx.column_position] {
                        for tri in trgm::extract_trigrams(s) {
                            // r1019 — address the String-keyed map with the borrowed
                            // trigram; allocate one only for a key the map has never
                            // seen, which after the first rows is rare.
                            let key = trgm::trigram_str(&tri);
                            if let Some(entries) = map.get_mut_by(key) {
                                entries.push(RowLocator::Hot(new_row_idx));
                            } else {
                                map.insert_mut(
                                    alloc::string::ToString::to_string(key),
                                    crate::posting::PostingList::single(RowLocator::Hot(
                                        new_row_idx,
                                    )),
                                );
                            }
                        }
                    }
                }
                IndexKind::GinFulltext(map) => {
                    // v7.17.0 Phase 2.2 — MySQL FULLTEXT-shape
                    // GIN over a TEXT / VARCHAR cell. Tokenise
                    // via the storage-local `simple_lex` (same
                    // rule as `to_tsvector('simple', text)`) and
                    // extend each lexeme's posting list.
                    let text_cell = match &row.values[idx.column_position] {
                        Value::Text(s) => Some(s.as_ref()),
                        // mysqldump-style mediumtext / longtext
                        // land as Value::Text on insert; varchar
                        // cells likewise. Anything else (NULL,
                        // integer, …) contributes no lexemes.
                        _ => None,
                    };
                    if let Some(s) = text_cell {
                        for lex in fts_simple::simple_lex(s) {
                            if let Some(entries) = map.get_mut(&lex) {
                                entries.push(RowLocator::Hot(new_row_idx));
                            } else {
                                map.insert_mut(
                                    lex,
                                    crate::posting::PostingList::single(RowLocator::Hot(
                                        new_row_idx,
                                    )),
                                );
                            }
                        }
                    }
                }
                IndexKind::GinJsonb(map) => {
                    // v7.37.8(sentori Epic 5 P2)— real JSONB-GIN.
                    // Extract canonical `(path, leaf)` tokens from
                    // the cell text and extend each token's posting
                    // list. NULL or non-Json cell contributes no
                    // tokens(`labels @> '...'` against a NULL row
                    // is always false so absence here is correct).
                    let json_cell = match &row.values[idx.column_position] {
                        Value::Json(s) => Some(s.as_ref()),
                        _ => None,
                    };
                    if let Some(s) = json_cell {
                        for tok in jsonb_gin::extract_tokens(s) {
                            if let Some(entries) = map.get_mut(&tok) {
                                entries.push(RowLocator::Hot(new_row_idx));
                            } else {
                                map.insert_mut(
                                    tok,
                                    crate::posting::PostingList::single(RowLocator::Hot(
                                        new_row_idx,
                                    )),
                                );
                            }
                        }
                    }
                }
                // v7.38.11 — widen the summary covering this slot.
                //
                // Widen-only: this can make a range less selective and
                // never makes it skip a row. A value with no BRIN
                // ordering leaves the range as it was, and a range that
                // has never seen one stays `None`, which the scan reads
                // as "cannot be skipped".
                IndexKind::Brin { summaries, .. } => {
                    let r = new_row_idx / crate::BRIN_RANGE_ROWS;
                    if summaries.len() <= r {
                        summaries.resize(r + 1, None);
                    }
                    if let Some(n) = crate::brin_scalar(&row.values[idx.column_position]) {
                        summaries[r] = Some(match summaries[r] {
                            Some((lo, hi)) => (lo.min(n), hi.max(n)),
                            None => (n, n),
                        });
                    }
                }
                // NSW handled below after the row push (so the new row
                // is visible to the kNN-graph connect step).
                IndexKind::Nsw(_) => {}
            }
        }
        // v7.39 (round 215) — maintain the range-exclusion indexes for the
        // freshly-inserted row (before the move; `new_row_idx` is the slot it
        // will occupy). Mirrors the BTree maintenance above.
        if !self.excl_indexes.is_empty() {
            self.excl_indexes_on_insert(&row, new_row_idx);
        }
        // v5.2.1: maintain incremental hot-tier byte counter. Computed
        // before the move so we don't need to borrow `row` after push.
        self.hot_bytes = self
            .hot_bytes
            .saturating_add(row_body_encoded_len(&row, &self.schema) as u64);
        // v7.34 — capture the row-level redo before the row is moved in.
        // v7.37.15 (Epic W slice 1) — carry the stable RowId this insert
        // will receive. `alloc_rowid` below hands out `RowId(next_rowid)`
        // and bumps the counter unconditionally, so the id read here is
        // exactly the one the row ends up with. `writer_version` (xmin)
        // is 0: the writing TxId is not threaded to this layer yet (the
        // header pushed below is `RowHeader::frozen()`).
        let redo_rowid =
            crate::row_header::RowId(self.next_rowid.load(core::sync::atomic::Ordering::Relaxed));
        self.record_redo(|table| RowChange::Insert {
            table,
            row: row.clone(),
            rowid: redo_rowid,
            writer_version: 0,
        });
        // v4.39.1: push_mut keeps streaming inserts at Vec::push speed when
        // the table is uniquely owned (the spg-embedded path); inside a TX
        // wrap where a Catalog snapshot exists, push_mut path-copies the
        // tail just like push() and the snapshot stays valid.
        self.rows.push_mut(row);
        // v7.37.15 (Phase A.2) — keep `headers` lock-step with `rows`.
        // Phase A defaults every new insert to RowHeader::frozen() so
        // visibility checks against any snapshot return true; Phase C
        // upgrades the inserter to stamp the writing tx's xmin.
        self.headers
            .push_mut(crate::row_header::RowHeader::frozen());
        // v7.37.15 (Phase C.1) — allocate + push the stable RowId in
        // lock-step with rows/headers. Index locators still address
        // by physical slot at this commit; the id is additive
        // bookkeeping the lock table / HOT chains / WAL migrate to.
        let rid = self.alloc_rowid();
        self.rowids.push_mut(rid);
        // v7.37.15 (Epic W slice 1) — the id captured for the redo log
        // above must be the one actually assigned to the row.
        debug_assert_eq!(
            rid, redo_rowid,
            "redo-captured RowId must match the allocated RowId"
        );
        debug_assert_eq!(
            self.rows.len(),
            self.headers.len(),
            "headers must stay in lock-step with rows after insert"
        );
        debug_assert_eq!(
            self.rows.len(),
            self.rowids.len(),
            "rowids must stay in lock-step with rows after insert"
        );
        // NSW updates after the push so the new row is visible to the
        // greedy search used during connect.
        let new_row_idx = self.rows.len() - 1;
        let nsw_targets: Vec<usize> = self
            .indices
            .iter()
            .enumerate()
            .filter_map(|(i, idx)| {
                if matches!(idx.kind, IndexKind::Nsw(_)) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        for idx_pos in nsw_targets {
            nsw_insert_at(self, idx_pos, new_row_idx);
        }
        Ok(())
    }

    /// Build a new B-tree index over the named column. Rebuilds from
    /// existing rows. Errors if `column_name` doesn't exist or the index
    /// name is taken.
    pub fn add_index(&mut self, name: String, column_name: &str) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        let mut idx = Index::new_btree(name, column_position);
        if let IndexKind::BTree(map) = &mut idx.kind {
            for (i, row) in self.rows.iter().enumerate() {
                if let Some(key) = IndexKey::from_value(&row.values[column_position]) {
                    if let Some(entries) = map.get_mut(&key) {
                        entries.push(RowLocator::Hot(i));
                    } else {
                        map.insert_mut(
                            key,
                            crate::posting::PostingList::single(RowLocator::Hot(i)),
                        );
                    }
                }
            }
        }
        self.indices.push(idx);
        Ok(())
    }

    /// v7.39 (round 215) — ensure a range-exclusion index exists on
    /// `column_position`, building it from the current rows. Idempotent: a
    /// second call for the same column is a no-op. Called at CREATE TABLE /
    /// ALTER ADD EXCLUDE and on catalog load (rebuild-from-constraints).
    /// Tombstoned rows are indexed too (they are filtered by the consumer via
    /// `is_deleted()` at query time — the established index pattern).
    pub fn ensure_excl_range_index(&mut self, column_position: usize) {
        if self
            .excl_indexes
            .iter()
            .any(|e| e.column_position == column_position)
        {
            return;
        }
        let mut map: crate::PersistentBTreeMap<(i128, u8), crate::posting::PostingList> =
            crate::PersistentBTreeMap::new();
        for (i, row) in self.rows.iter().enumerate() {
            if let Some(v) = row.values.get(column_position)
                && let Some(key) = crate::range_excl_index_key(v)
            {
                if let Some(entries) = map.get_mut(&key) {
                    entries.push(RowLocator::Hot(i));
                } else {
                    map.insert_mut(key, crate::posting::PostingList::single(RowLocator::Hot(i)));
                }
            }
        }
        self.excl_indexes.push(crate::ExclRangeIndex {
            column_position,
            map,
        });
    }

    /// v7.39 (round 215) — the range-exclusion index on `column_position`, if
    /// one was built. The EXCLUDE enforcement path probes its
    /// [`predecessor`](crate::PersistentBTreeMap::predecessor) + successors to
    /// find candidate overlaps in O(log n).
    #[must_use]
    pub fn excl_range_index(
        &self,
        column_position: usize,
    ) -> Option<&crate::PersistentBTreeMap<(i128, u8), crate::posting::PostingList>> {
        self.excl_indexes
            .iter()
            .find(|e| e.column_position == column_position)
            .map(|e| &e.map)
    }

    /// v7.39 (round 215) — add a freshly-appended row at `row_idx` to every
    /// range-exclusion index. Called from `insert` after the row is pushed,
    /// mirroring the BTree secondary-index maintenance.
    fn excl_indexes_on_insert(&mut self, row: &Row<'static>, row_idx: usize) {
        for ex in &mut self.excl_indexes {
            if let Some(v) = row.values.get(ex.column_position)
                && let Some(key) = crate::range_excl_index_key(v)
            {
                if let Some(entries) = ex.map.get_mut(&key) {
                    entries.push(RowLocator::Hot(row_idx));
                } else {
                    ex.map.insert_mut(
                        key,
                        crate::posting::PostingList::single(RowLocator::Hot(row_idx)),
                    );
                }
            }
        }
    }

    /// v7.39 (round 215) — rebuild every range-exclusion index from the
    /// current rows (called from `rebuild_indices`, i.e. after a physical
    /// compaction/delete that shifted slots). Preserves which columns are
    /// indexed; re-emits all `Hot` locators.
    fn rebuild_excl_indexes(&mut self) {
        let cols: Vec<usize> = self
            .excl_indexes
            .iter()
            .map(|e| e.column_position)
            .collect();
        self.excl_indexes.clear();
        for c in cols {
            self.ensure_excl_range_index(c);
        }
    }

    /// Build a new NSW (HNSW-flavoured) index over the named column.
    /// Required for `ORDER BY col <-> literal LIMIT k` to plan as a
    /// graph traversal instead of a full scan. Column must be a Vector
    /// type. `m` is the maximum number of neighbours per node.
    pub fn add_nsw_index(
        &mut self,
        name: String,
        column_name: &str,
        m: usize,
    ) -> Result<(), StorageError> {
        self.add_nsw_index_inner(name, column_name, m, None)
    }

    /// v6.0.4 — synchronous rebuild of the named NSW index. If
    /// `new_encoding` is `Some(target)` and differs from the column's
    /// current encoding, every stored cell at the indexed column is
    /// re-coded into the target encoding before the new graph
    /// builds. Returns `IndexNotFound` if no index by that name exists
    /// and `Unsupported` for non-NSW indexes (`BTree` REBUILD is a no-op
    /// the engine layer rejects, not a storage-level concept).
    ///
    /// Holds the caller's `&mut self` for the duration — no
    /// concurrency / staging / WAL-replay machinery in v6.0.4. The
    /// "live" optimisation lands as v6.0.4.1.
    pub fn rebuild_nsw_index(
        &mut self,
        name: &str,
        new_encoding: Option<VecEncoding>,
    ) -> Result<(), StorageError> {
        let idx_pos = self
            .indices
            .iter()
            .position(|i| i.name == name)
            .ok_or_else(|| StorageError::IndexNotFound {
                name: String::from(name),
            })?;
        let col_pos = self.indices[idx_pos].column_position;
        let m = match &self.indices[idx_pos].kind {
            IndexKind::Nsw(g) => g.m,
            IndexKind::BTree(_)
            | IndexKind::Brin { .. }
            | IndexKind::Gin(_)
            | IndexKind::GinTrgm(_)
            | IndexKind::GinFulltext(_)
            | IndexKind::GinJsonb(_)
            | IndexKind::BTreeMulti(_) => {
                return Err(StorageError::Unsupported(format!(
                    "ALTER INDEX REBUILD on non-NSW index {name:?} — only NSW indexes can rebuild"
                )));
            }
        };
        let col_name = self.schema.columns[col_pos].name.clone();
        // 1. Optional re-encoding pass. Done first so the cells
        //    match the schema before the graph rebuild walks them.
        if let Some(target) = new_encoding {
            let current = match self.schema.columns[col_pos].ty {
                DataType::Vector { encoding, .. } => encoding,
                ref other => {
                    return Err(StorageError::Unsupported(format!(
                        "ALTER INDEX REBUILD WITH (encoding=…) on non-vector column type {other:?}"
                    )));
                }
            };
            if target != current {
                let DataType::Vector { dim, .. } = self.schema.columns[col_pos].ty else {
                    unreachable!("checked above")
                };
                let n = self.rows.len();
                for i in 0..n {
                    let row = self
                        .rows
                        .get_mut(i)
                        .expect("row index in bounds (we iterated up to len())");
                    let cell = core::mem::replace(&mut row.values[col_pos], Value::Null);
                    let recoded = recode_vector_cell(cell, target)?;
                    row.values[col_pos] = recoded;
                }
                self.schema.columns[col_pos].ty = DataType::Vector {
                    dim,
                    encoding: target,
                };
            }
        }
        // 2. Drop the existing index slot + rebuild from row payload.
        self.indices.remove(idx_pos);
        self.add_nsw_index_inner(String::from(name), &col_name, m, None)?;
        Ok(())
    }

    /// Restore an NSW index from a pre-built graph (used on
    /// deserialize). Skips the bulk-build pass since the topology is
    /// already known. Returns `DuplicateIndex` or `ColumnNotFound` on
    /// schema mismatch as usual.
    pub fn restore_nsw_index(
        &mut self,
        name: String,
        column_name: &str,
        graph: NswGraph,
    ) -> Result<(), StorageError> {
        self.add_nsw_index_inner(name, column_name, graph.m, Some(graph))
    }

    /// Restore a `BTree` index from a pre-built `(IndexKey, Vec<RowLocator>)`
    /// map. Used by [`Catalog::deserialize`] when reading a v9 (or later)
    /// catalog snapshot — the map travels on disk so cold-tier locators
    /// survive a round-trip, instead of being rebuilt from `self.rows`
    /// (which would lose every Cold entry). Same error contract as
    /// [`Table::add_index`].
    pub fn restore_btree_index(
        &mut self,
        name: String,
        column_name: &str,
        map: PersistentBTreeMap<IndexKey, crate::posting::PostingList>,
    ) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        self.indices.push(Index {
            name,
            column_position,
            kind: IndexKind::BTree(map),
            included_columns: Vec::new(),
            partial_predicate: None,
            expression: None,
            is_unique: false,
            nulls_not_distinct: false,
            descending: false,
            nulls_first: None,
            collation: None,
            extra_column_positions: Vec::new(),
        });
        Ok(())
    }

    /// v7.38.1 (L12) — snapshot-restore counterpart for a tag-7
    /// multi-column B-tree. The extras arrive via the per-index
    /// appendix, which `Catalog::deserialize` applies after this call —
    /// exactly as it does for every other restored kind.
    pub fn restore_btree_multi_index(
        &mut self,
        name: String,
        column_name: &str,
        map: PersistentBTreeMap<alloc::boxed::Box<[IndexKey]>, crate::posting::PostingList>,
    ) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        self.indices.push(Index {
            kind: IndexKind::BTreeMulti(map),
            ..Index::new_btree(name, column_position)
        });
        Ok(())
    }

    /// v7.38.1 (L12) — upgrade a leading-column B-tree that carries
    /// `extra_column_positions` into a real multi-column B-tree, in
    /// place, keeping every piece of index metadata. Returns `false`
    /// (untouched) when the index is not a plain BTree, has no extras,
    /// or keys on an expression (whose value is not a column's own).
    ///
    /// Cold locators block the conversion too: a composite key cannot
    /// be derived for a row whose body lives in a cold segment, and
    /// silently dropping the entry would drop the row from every seek.
    pub fn convert_index_to_multi(&mut self, name: &str) -> Result<bool, StorageError> {
        let Some(pos) = self.indices.iter().position(|i| i.name == name) else {
            return Ok(false);
        };
        {
            let idx = &self.indices[pos];
            if idx.extra_column_positions.is_empty()
                || idx.expression.is_some()
                || idx.partial_predicate.is_some()
            {
                return Ok(false);
            }
            match &idx.kind {
                IndexKind::BTree(map) => {
                    if map.iter().any(|(_, locs)| locs.iter().any(|l| l.is_cold())) {
                        return Ok(false);
                    }
                }
                _ => return Ok(false),
            }
        }
        let column_position = self.indices[pos].column_position;
        let extras = self.indices[pos].extra_column_positions.clone();
        // Component-type gate: every non-null value of every component
        // must key, or rows could silently vanish from the index.
        for p in core::iter::once(column_position).chain(extras.iter().copied()) {
            match self.schema.columns.get(p) {
                Some(col) if crate::multi_component_type_ok(col.ty) => {}
                _ => return Ok(false),
            }
        }
        let mut pairs: Vec<(alloc::boxed::Box<[IndexKey]>, usize)> =
            Vec::with_capacity(self.rows.len());
        for (i, row) in self.rows.iter().enumerate() {
            if let Some(key) = crate::compose_multi_key(&row.values, column_position, &extras) {
                pairs.push((key, i));
            }
        }
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        let mut grouped: Vec<(alloc::boxed::Box<[IndexKey]>, crate::posting::PostingList)> =
            Vec::new();
        for (key, i) in pairs {
            match grouped.last_mut() {
                Some((k, locs)) if *k == key => locs.push(RowLocator::Hot(i)),
                _ => grouped.push((key, crate::posting::PostingList::single(RowLocator::Hot(i)))),
            }
        }
        self.indices[pos].kind = IndexKind::BTreeMulti(PersistentBTreeMap::from_sorted(grouped));
        Ok(true)
    }

    /// v7.38.1 (L12) — build a real multi-column B-tree over
    /// `[leading, extras…]` from the current rows. The caller supplies
    /// resolved column positions; uniqueness and the rest of the
    /// index's metadata are applied by the caller afterwards, exactly
    /// as `add_index` callers do today.
    pub fn add_multi_index(
        &mut self,
        name: &str,
        column_position: usize,
        extra_column_positions: Vec<usize>,
    ) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name: name.into() });
        }
        if extra_column_positions.is_empty() {
            return Err(StorageError::Unsupported(
                "add_multi_index: needs at least two columns; use add_index for one".into(),
            ));
        }
        for pos in core::iter::once(column_position).chain(extra_column_positions.iter().copied()) {
            match self.schema.columns.get(pos) {
                Some(col) if crate::multi_component_type_ok(col.ty) => {}
                _ => {
                    return Err(StorageError::Unsupported(format!(
                        "add_multi_index: component column {pos} has no total key form"
                    )));
                }
            }
        }
        let mut idx = Index {
            extra_column_positions: extra_column_positions.clone(),
            ..Index::new_btree_multi(String::from(name), column_position)
        };
        let mut pairs: Vec<(alloc::boxed::Box<[IndexKey]>, usize)> =
            Vec::with_capacity(self.rows.len());
        for (i, row) in self.rows.iter().enumerate() {
            if let Some(key) =
                crate::compose_multi_key(&row.values, column_position, &extra_column_positions)
            {
                pairs.push((key, i));
            }
        }
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        let mut grouped: Vec<(alloc::boxed::Box<[IndexKey]>, crate::posting::PostingList)> =
            Vec::new();
        for (key, i) in pairs {
            match grouped.last_mut() {
                Some((k, locs)) if *k == key => locs.push(RowLocator::Hot(i)),
                _ => grouped.push((key, crate::posting::PostingList::single(RowLocator::Hot(i)))),
            }
        }
        idx.kind = IndexKind::BTreeMulti(PersistentBTreeMap::from_sorted(grouped));
        self.indices.push(idx);
        Ok(())
    }

    /// v6.7.1 — public restore counterpart for BRIN indices. Used
    /// by `Catalog::deserialize` when a v10 snapshot carries a
    /// BRIN index entry. BRIN carries no in-memory data — only the
    /// `column_type` snapshot is restored.
    pub fn restore_brin_index(
        &mut self,
        name: String,
        column_name: &str,
        column_type: DataType,
    ) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        self.indices
            .push(Index::new_brin(name, column_position, column_type));
        Ok(())
    }

    /// v6.7.1 — public CREATE INDEX counterpart for BRIN. Creates
    /// the index entry with a snapshot of the indexed column's
    /// current `DataType`.
    pub fn add_brin_index(&mut self, name: String, column_name: &str) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        let column_type = self.schema.columns[column_position].ty;
        self.indices
            .push(Index::new_brin(name, column_position, column_type));
        Ok(())
    }

    /// v7.12.3 — Build a new GIN inverted index over a `tsvector`
    /// column. Populates posting lists from existing rows. Errors
    /// if the column doesn't exist, isn't `TsVector`, or the index
    /// name is taken.
    pub fn add_gin_index(&mut self, name: String, column_name: &str) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        if self.schema.columns[column_position].ty != DataType::TsVector {
            return Err(StorageError::Corrupt(format!(
                "GIN index {name:?} requires a tsvector column; \
                 {column_name:?} is {:?}",
                self.schema.columns[column_position].ty
            )));
        }
        let mut idx = Index::new_gin(name, column_position);
        if let IndexKind::Gin(map) = &mut idx.kind {
            for (i, row) in self.rows.iter().enumerate() {
                if let Value::TsVector(lexemes) = &row.values[column_position] {
                    for lex in lexemes {
                        if let Some(entries) = map.get_mut(&lex.word) {
                            entries.push(RowLocator::Hot(i));
                        } else {
                            map.insert_mut(
                                lex.word.clone(),
                                crate::posting::PostingList::single(RowLocator::Hot(i)),
                            );
                        }
                    }
                }
            }
        }
        self.indices.push(idx);
        Ok(())
    }

    /// v7.12.3 — Restore a GIN index from a deserialised snapshot.
    /// Mirrors [`Self::restore_btree_index`] but takes the GIN's
    /// `word → Vec<RowLocator>` posting-list map (already populated
    /// from the catalog stream) instead of an `IndexKey` map.
    pub fn restore_gin_index(
        &mut self,
        name: String,
        column_name: &str,
        map: PersistentBTreeMap<String, crate::posting::PostingList>,
    ) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        let mut idx = Index::new_gin(name, column_position);
        idx.kind = IndexKind::Gin(map);
        self.indices.push(idx);
        Ok(())
    }

    /// v7.15.0 — `gin_trgm_ops` GIN over a TEXT column. Walks
    /// every row, shingles the cell into PG-compatible trigrams,
    /// and builds the posting-list map. NULL / non-TEXT cells
    /// contribute nothing (no trigrams).
    pub fn add_gin_trgm_index(
        &mut self,
        name: String,
        column_name: &str,
    ) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        if !matches!(
            self.schema.columns[column_position].ty,
            DataType::Text | DataType::Varchar(_)
        ) {
            return Err(StorageError::Corrupt(format!(
                "trigram-GIN index {name:?} requires a TEXT/VARCHAR column; \
                 {column_name:?} is {:?}",
                self.schema.columns[column_position].ty
            )));
        }
        let mut idx = Index::new_gin_trgm(name, column_position);
        if let IndexKind::GinTrgm(map) = &mut idx.kind {
            for (i, row) in self.rows.iter().enumerate() {
                if let Value::Text(s) = &row.values[column_position] {
                    for tri in trgm::extract_trigrams(s) {
                        // r1019 — address the String-keyed map with the borrowed
                        // trigram; allocate one only for a key the map has never
                        // seen, which after the first rows is rare.
                        let key = trgm::trigram_str(&tri);
                        if let Some(entries) = map.get_mut_by(key) {
                            entries.push(RowLocator::Hot(i));
                        } else {
                            map.insert_mut(
                                alloc::string::ToString::to_string(key),
                                crate::posting::PostingList::single(RowLocator::Hot(i)),
                            );
                        }
                    }
                }
            }
        }
        self.indices.push(idx);
        Ok(())
    }

    /// v7.15.0 — restore a trigram-GIN from its catalog snapshot
    /// payload. Mirrors [`Self::restore_gin_index`].
    pub fn restore_gin_trgm_index(
        &mut self,
        name: String,
        column_name: &str,
        map: PersistentBTreeMap<String, crate::posting::PostingList>,
    ) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        let mut idx = Index::new_gin_trgm(name, column_position);
        idx.kind = IndexKind::GinTrgm(map);
        self.indices.push(idx);
        Ok(())
    }

    /// v7.17.0 Phase 2.2 — MySQL `FULLTEXT KEY` GIN over a TEXT
    /// column. Walks every row, tokenises the cell into lower-
    /// cased word lexemes (`fts_simple::simple_lex` — same rule
    /// as `to_tsvector('simple', text)`), and builds the
    /// posting-list map. NULL / non-TEXT cells contribute
    /// nothing (no lexemes).
    pub fn add_gin_fulltext_index(
        &mut self,
        name: String,
        column_name: &str,
    ) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        if !matches!(
            self.schema.columns[column_position].ty,
            DataType::Text | DataType::Varchar(_)
        ) {
            return Err(StorageError::Corrupt(format!(
                "fulltext-GIN index {name:?} requires a TEXT/VARCHAR column; \
                 {column_name:?} is {:?}",
                self.schema.columns[column_position].ty
            )));
        }
        let mut idx = Index::new_gin_fulltext(name, column_position);
        if let IndexKind::GinFulltext(map) = &mut idx.kind {
            for (i, row) in self.rows.iter().enumerate() {
                if let Value::Text(s) = &row.values[column_position] {
                    for lex in fts_simple::simple_lex(s) {
                        if let Some(entries) = map.get_mut(&lex) {
                            entries.push(RowLocator::Hot(i));
                        } else {
                            map.insert_mut(
                                lex,
                                crate::posting::PostingList::single(RowLocator::Hot(i)),
                            );
                        }
                    }
                }
            }
        }
        self.indices.push(idx);
        Ok(())
    }

    /// v7.17.0 Phase 2.2 — restore a fulltext-GIN from its
    /// catalog snapshot payload. Mirrors
    /// [`Self::restore_gin_trgm_index`].
    pub fn restore_gin_fulltext_index(
        &mut self,
        name: String,
        column_name: &str,
        map: PersistentBTreeMap<String, crate::posting::PostingList>,
    ) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        let mut idx = Index::new_gin_fulltext(name, column_position);
        idx.kind = IndexKind::GinFulltext(map);
        self.indices.push(idx);
        Ok(())
    }

    /// v7.37.8(sentori Epic 5 P2)— JSONB-GIN over a `Json` /
    /// `Jsonb` column. Walks every row, extracts canonical
    /// `(path, leaf)` tokens via
    /// [`crate::jsonb_gin::extract_tokens`], and builds the
    /// posting-list map. NULL or non-Json cells contribute no
    /// tokens(`<col> @> <jsonb>` against a NULL row is always
    /// false so absence here is correct).
    pub fn add_gin_jsonb_index(
        &mut self,
        name: String,
        column_name: &str,
    ) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        if !matches!(
            self.schema.columns[column_position].ty,
            DataType::Json | DataType::Jsonb
        ) {
            return Err(StorageError::Corrupt(format!(
                "JSONB-GIN index {name:?} requires a JSON/JSONB column; \
                 {column_name:?} is {:?}",
                self.schema.columns[column_position].ty
            )));
        }
        let mut idx = Index::new_gin_jsonb(name, column_position);
        if let IndexKind::GinJsonb(map) = &mut idx.kind {
            for (i, row) in self.rows.iter().enumerate() {
                if let Value::Json(s) = &row.values[column_position] {
                    for tok in jsonb_gin::extract_tokens(s) {
                        if let Some(entries) = map.get_mut(&tok) {
                            entries.push(RowLocator::Hot(i));
                        } else {
                            map.insert_mut(
                                tok,
                                crate::posting::PostingList::single(RowLocator::Hot(i)),
                            );
                        }
                    }
                }
            }
        }
        self.indices.push(idx);
        Ok(())
    }

    /// v7.37.8 — restore a JSONB-GIN from its catalog snapshot
    /// payload. Mirrors [`Self::restore_gin_fulltext_index`].
    pub fn restore_gin_jsonb_index(
        &mut self,
        name: String,
        column_name: &str,
        map: PersistentBTreeMap<String, crate::posting::PostingList>,
    ) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        let mut idx = Index::new_gin_jsonb(name, column_position);
        idx.kind = IndexKind::GinJsonb(map);
        self.indices.push(idx);
        Ok(())
    }

    /// v5.1: register cold-tier locators on a `BTree` index. Used
    /// after [`Catalog::load_segment_bytes`] to wire every cold-
    /// tier row's PK back to its segment so
    /// [`Catalog::lookup_by_pk`] can resolve it. Each call
    /// appends to the index — keys that already have hot or cold
    /// locators keep them. Returns the number of locators
    /// registered.
    ///
    /// Pre-v5.2 (freezer) this is the only path that adds Cold
    /// variants to a PB; post-freezer the background freezer
    /// thread produces these as a batch under the engine write
    /// lock and this API becomes its in-memory primitive.
    ///
    /// Errors if `index_name` doesn't exist or names an NSW graph
    /// (NSW indices don't carry per-key row locators — they're
    /// vector-search structures).
    pub fn register_cold_locators<I>(
        &mut self,
        index_name: &str,
        locators: I,
    ) -> Result<usize, StorageError>
    where
        I: IntoIterator<Item = (IndexKey, RowLocator)>,
    {
        let idx = self
            .indices
            .iter_mut()
            .find(|i| i.name == index_name)
            .ok_or_else(|| StorageError::Corrupt(format!("index {index_name:?} not found")))?;
        let map = match &mut idx.kind {
            IndexKind::BTree(map) => map,
            IndexKind::Nsw(_)
            | IndexKind::Brin { .. }
            | IndexKind::Gin(_)
            | IndexKind::GinTrgm(_)
            | IndexKind::GinFulltext(_)
            | IndexKind::GinJsonb(_)
            | IndexKind::BTreeMulti(_) => {
                return Err(StorageError::Corrupt(format!(
                    "index {index_name:?} is not BTree; cold locators apply only to BTree indices"
                )));
            }
        };
        let mut count = 0usize;
        for (key, locator) in locators {
            if let Some(entries) = map.get_mut(&key) {
                entries.push(locator);
            } else {
                map.insert_mut(key, crate::posting::PostingList::single(locator));
            }
            count += 1;
        }
        Ok(count)
    }

    /// v7.12.3 — GIN-side parallel to [`Self::register_cold_locators`].
    /// Re-attaches `word → cold RowLocator` posting-list entries after
    /// the from-rows rebuild loop. Errors when the index doesn't
    /// exist or isn't a GIN. Both tsvector-GIN and trigram-GIN
    /// variants share posting-list shape (`String → Vec<RowLocator>`),
    /// so this helper accepts either.
    pub fn register_gin_cold_locators<I>(
        &mut self,
        index_name: &str,
        locators: I,
    ) -> Result<usize, StorageError>
    where
        I: IntoIterator<Item = (String, RowLocator)>,
    {
        let idx = self
            .indices
            .iter_mut()
            .find(|i| i.name == index_name)
            .ok_or_else(|| StorageError::Corrupt(format!("index {index_name:?} not found")))?;
        let map = match &mut idx.kind {
            // v7.17.0 Phase 2.2 — fulltext-GIN posting lists are
            // shape-compatible with tsvector / trigram GINs, so
            // cold-locator re-attach handles all three.
            // v7.37.8 — JSONB-GIN shares the same posting-list shape,
            // so it joins the same re-attach path.
            IndexKind::Gin(map)
            | IndexKind::GinTrgm(map)
            | IndexKind::GinFulltext(map)
            | IndexKind::GinJsonb(map) => map,
            IndexKind::BTree(_)
            | IndexKind::Nsw(_)
            | IndexKind::Brin { .. }
            | IndexKind::BTreeMulti(_) => {
                return Err(StorageError::Corrupt(format!(
                    "register_gin_cold_locators: index {index_name:?} is not GIN"
                )));
            }
        };
        let mut count = 0usize;
        for (word, locator) in locators {
            if let Some(entries) = map.get_mut(&word) {
                entries.push(locator);
            } else {
                map.insert_mut(word, crate::posting::PostingList::single(locator));
            }
            count += 1;
        }
        Ok(count)
    }

    /// v5.2.3: remove every `Cold` locator currently registered on
    /// `index_name` under the given `key`. `Hot` locators for the
    /// same key are left in place — useful when a row has just been
    /// promoted hot-side and the caller wants the old Cold pointer
    /// retired without losing the new hot entry.
    ///
    /// Returns the number of cold locators removed (0 when the key
    /// has only hot entries or the key isn't present at all).
    /// Errors when the index doesn't exist or isn't a `BTree`.
    pub fn remove_cold_locators_for_key(
        &mut self,
        index_name: &str,
        key: &IndexKey,
    ) -> Result<usize, StorageError> {
        let idx = self
            .indices
            .iter_mut()
            .find(|i| i.name == index_name)
            .ok_or_else(|| {
                StorageError::Corrupt(format!(
                    "remove_cold_locators_for_key: index {index_name:?} not found"
                ))
            })?;
        let map = match &mut idx.kind {
            IndexKind::BTree(map) => map,
            IndexKind::Nsw(_)
            | IndexKind::Brin { .. }
            | IndexKind::Gin(_)
            | IndexKind::GinTrgm(_)
            | IndexKind::GinFulltext(_)
            | IndexKind::GinJsonb(_)
            | IndexKind::BTreeMulti(_) => {
                return Err(StorageError::Corrupt(format!(
                    "remove_cold_locators_for_key: index {index_name:?} is not BTree; \
                     cold locators apply only to BTree indices"
                )));
            }
        };
        let Some(entries) = map.get(key) else {
            return Ok(0);
        };
        let mut kept: crate::posting::PostingList =
            entries.iter().copied().filter(RowLocator::is_hot).collect();
        let removed = entries.len() - kept.len();
        if removed == 0 {
            return Ok(0);
        }
        // PersistentBTreeMap has no remove API in v5.2; when every
        // locator for `key` was Cold, the key keeps an empty Vec
        // entry. `Index::lookup_eq` already treats `Some(&[])` and
        // `None` as the same empty slice (via `Vec::as_slice`), so
        // callers can't distinguish the two. The space cost is one
        // empty Vec per shadowed-then-promoted key — bounded and
        // recoverable when the future compaction job lands.
        map.insert_mut(key.clone(), kept);
        Ok(removed)
    }

    /// v7.13.0 — append a new column to the schema and back-fill
    /// every existing row with `fill_value`. Used by the engine's
    /// `ALTER TABLE t ADD COLUMN …` handler (mailrs round-5 G1).
    /// Indices on existing columns keep working — column positions
    /// don't shift since the new column lands at the end — so no
    /// index rebuild is needed.
    pub fn add_column(&mut self, col: ColumnSchema, fill_value: Value<'static>) {
        self.schema.columns.push(col);
        let mut new_rows: PersistentVec<Row<'static>> = PersistentVec::new();
        for row in self.rows.iter() {
            let mut values = row.values.clone();
            values.push(fill_value.clone());
            new_rows.push_mut(Row::new(values));
        }
        self.rows = new_rows;
    }

    /// v7.15.0 — replace the partial-index predicate source on
    /// the index at slot `idx`. Used by `ALTER TABLE … RENAME
    /// COLUMN` after the engine rewrites column-identifier
    /// references in the predicate source text. Pure metadata
    /// edit; index rows are unaffected (they're keyed by
    /// column position, not predicate text).
    pub fn set_partial_predicate(&mut self, idx: usize, pred: Option<String>) {
        debug_assert!(idx < self.indices.len());
        self.indices[idx].partial_predicate = pred;
    }

    /// v7.15.0 — rename the column at `col_pos` to `new_name`.
    /// The on-disk row encoding is positional, so no row rewrite
    /// is needed; only the schema's column name changes. Indices,
    /// UCs, FKs all key off column positions and are unaffected.
    /// Source-text references that hold the column name (CHECK
    /// predicates, partial-index predicates, runtime DEFAULT
    /// expressions, trigger `UPDATE OF` lists) are rewritten by
    /// the engine before this helper is called — the storage
    /// layer doesn't depend on `spg-sql` and so can't re-parse the
    /// predicate sources itself.
    pub fn rename_column(&mut self, col_pos: usize, new_name: &str) {
        debug_assert!(col_pos < self.schema.columns.len());
        self.schema.columns[col_pos].name = new_name.to_string();
    }

    /// v7.13.3 — drop the column at `col_pos`. Removes the entry
    /// from the schema, the value from every row, any index that
    /// references the column (pure drop, not shift), and shifts
    /// every remaining index/UC/FK column position that pointed
    /// past `col_pos` down by one. Used by `ALTER TABLE t DROP
    /// COLUMN <c>` (mailrs round-7 S8). FK dependents on this
    /// column must already have been removed by the caller (CASCADE
    /// path); the helper assumes only same-column index removal is
    /// needed.
    pub fn drop_column(&mut self, col_pos: usize) {
        debug_assert!(col_pos < self.schema.columns.len());
        // v7.39 (round 215) — dropping a column shifts every later column's
        // position, which would leave a range-exclusion index pointing at the
        // wrong column. Drop the indexes rather than risk a silent-wrong
        // probe; enforce falls back to the correct O(n) scan until they are
        // rebuilt (`ensure_excl_range_index` from the constraint's updated
        // column position).
        self.excl_indexes.clear();
        // Strip the column from the schema.
        self.schema.columns.remove(col_pos);
        // Rewrite every row to omit the cell at col_pos.
        let mut new_rows: PersistentVec<Row> = PersistentVec::new();
        for row in self.rows.iter() {
            let mut values = row.values.clone();
            if col_pos < values.len() {
                values.remove(col_pos);
            }
            new_rows.push_mut(Row::new(values));
        }
        self.rows = new_rows;
        // Drop indices on the column outright; shift the rest.
        // v7.38.1 (L12) — an index whose EXTRA columns name the dropped
        // one goes too (PG drops dependent indexes with the column).
        // Before this, `extra_column_positions` was neither dropped nor
        // shifted, so a composite UNIQUE's enforcement silently read
        // the wrong columns after any earlier column was dropped.
        self.indices.retain(|idx| {
            idx.column_position != col_pos && !idx.extra_column_positions.contains(&col_pos)
        });
        for idx in &mut self.indices {
            if idx.column_position > col_pos {
                idx.column_position -= 1;
            }
            // Same shift for any included-columns reference.
            for inc in &mut idx.included_columns {
                if *inc > col_pos {
                    *inc -= 1;
                }
            }
            for extra in &mut idx.extra_column_positions {
                if *extra > col_pos {
                    *extra -= 1;
                }
            }
        }
        // Shift uniqueness-constraint column positions (and drop
        // entries that lose all columns, though that shouldn't
        // happen in practice — caller has already CASCADE-removed
        // FKs and there's no general CASCADE for UCs).
        let mut surviving_ucs: Vec<UniquenessConstraint> = Vec::new();
        for mut uc in core::mem::take(&mut self.schema.uniqueness_constraints) {
            uc.columns.retain(|&c| c != col_pos);
            if uc.columns.is_empty() {
                continue;
            }
            for c in &mut uc.columns {
                if *c > col_pos {
                    *c -= 1;
                }
            }
            surviving_ucs.push(uc);
        }
        self.schema.uniqueness_constraints = surviving_ucs;
        // Shift FK local_columns (parent-pointing column positions
        // are off-table and untouched).
        for fk in &mut self.schema.foreign_keys {
            for c in &mut fk.local_columns {
                if *c > col_pos {
                    *c -= 1;
                }
            }
        }
        // Rebuild remaining indices' payload — the column-position
        // shift means existing IndexKey entries are still keyed by
        // the same column data but the position numbers changed;
        // existing key→locator maps stay valid because they're
        // keyed by Value not position. The rebuild is conservative
        // — same pattern delete_rows uses post-mutation.
        self.rebuild_indices();
    }

    /// v4.4: delete the rows at the given positions in one pass.
    /// `positions` must be unique; ordering doesn't matter. Indices
    /// are rebuilt from scratch (cheaper than tracking incremental
    /// shifts across both B-tree and NSW). Returns the number of
    /// rows removed.
    /// v7.17.0 Phase 1.3 — wipe every row. Used by REFRESH
    /// MATERIALIZED VIEW; same effect as `delete_rows((0..N).into())`
    /// but skips the per-position bookkeeping for the all-removed
    /// fast path. Indices are rebuilt (empty).
    pub fn truncate(&mut self) {
        self.rows = PersistentVec::new();
        // v7.37.15 (Phase A.2) — keep headers lock-step.
        self.headers = PersistentVec::new();
        // v7.37.15 (Phase C.1) — clear rowids lock-step. `next_rowid`
        // is NOT reset: ids stay globally monotonic within the
        // relation so a post-truncate insert never reuses a pre-
        // truncate id that a stale reference might still name.
        self.rowids = PersistentVec::new();
        self.hot_bytes = 0;
        self.rebuild_indices();
    }

    pub fn delete_rows(&mut self, positions: &[usize]) -> usize {
        // v7.37.15 (Epic W slice 1) — capture the RowIds of the targeted
        // rows BEFORE the deletion shifts them out. One id per input
        // position (parallel to `positions`), `RowId::UNASSIGNED` for an
        // out-of-bounds position. Only pay for it when redo capture is
        // on. `writer_version` (xmax) is 0: the deleting TxId is not
        // threaded to this layer yet.
        let redo_rowids: Vec<crate::row_header::RowId> = if self.redo_log.is_some() {
            positions
                .iter()
                .map(|&p| {
                    self.rowids()
                        .get(p)
                        .copied()
                        .unwrap_or(crate::row_header::RowId::UNASSIGNED)
                })
                .collect()
        } else {
            Vec::new()
        };
        let removed = self.delete_rows_no_index(positions);
        if removed > 0 {
            self.rebuild_indices();
            // v7.34 — capture row-level redo. Record the input positions
            // (replay's `delete_rows` dedups + bounds-filters identically);
            // skip a no-op delete so the log stays minimal.
            self.record_redo(move |table| RowChange::Delete {
                table,
                positions: positions.to_vec(),
                rowids: redo_rowids,
                writer_version: 0,
            });
        }
        removed
    }

    /// v7.37.5 (mailrs crash-recovery Ask 3) — row-only delete for the
    /// WAL-replay batch path: removes the rows + decrements `hot_bytes`,
    /// **does NOT** call `rebuild_indices()` and does **NOT** capture
    /// redo. The caller is responsible for invoking `rebuild_indices_pub`
    /// once after a sequence of `*_no_index` mutations on this table.
    /// Skipping the per-call rebuild closes the
    /// O(records × rows × indices × log rows) replay blow-up
    /// (5000 DELETEs × 100k × 13 × ln 100k ≈ minutes → seconds).
    /// Returns the number of rows actually removed (dedup + bounds-
    /// filtered identically to `delete_rows`).
    pub fn delete_rows_no_index(&mut self, positions: &[usize]) -> usize {
        if positions.is_empty() {
            return 0;
        }
        // Mark positions; v4.39: PV has no in-place retain, so we rebuild
        // a fresh PV by pushing the survivors. Still O(n log₃₂ n); the
        // structural-sharing win shows up at `Catalog::clone()`, not here.
        let mut to_remove = alloc::vec![false; self.rows.len()];
        let mut removed = 0;
        for &p in positions {
            if p < to_remove.len() && !to_remove[p] {
                to_remove[p] = true;
                removed += 1;
            }
        }
        if removed == 0 {
            return 0;
        }
        let mut new_rows: PersistentVec<Row> = PersistentVec::new();
        let mut new_headers: PersistentVec<crate::row_header::RowHeader> = PersistentVec::new();
        // v7.37.15 (Phase C.1) — survivors carry their stable RowId
        // across the compaction so a held lock / redo reference keeps
        // naming the same row while its physical slot shifts down.
        let mut new_rowids: PersistentVec<crate::row_header::RowId> = PersistentVec::new();
        let mut removed_bytes: u64 = 0;
        // v7.37.16 (autovacuum) — recount dead survivors: this rebuild
        // is the compaction hub (vacuum and physical delete both land
        // here), so the incremental counter re-bases exactly.
        let mut surviving_dead: u64 = 0;
        for (i, row) in self.rows.iter().enumerate() {
            if to_remove[i] {
                removed_bytes =
                    removed_bytes.saturating_add(row_body_encoded_len(row, &self.schema) as u64);
            } else {
                new_rows.push_mut(row.clone());
                // v7.37.15 (Phase A.2) — keep headers lock-step.
                // Phase C will stamp xmax with the deleting tx's
                // id INSTEAD of physically dropping the row; Phase
                // A.2 keeps physical-delete semantics so
                // serialisation + WAL paths stay identical.
                if let Some(h) = self.headers.get(i) {
                    if h.xmax != crate::row_header::XMAX_ALIVE {
                        surviving_dead += 1;
                    }
                    new_headers.push_mut(*h);
                } else {
                    new_headers.push_mut(crate::row_header::RowHeader::frozen());
                }
                if let Some(rid) = self.rowids.get(i) {
                    new_rowids.push_mut(*rid);
                } else {
                    // Should not happen once C.1 is wired everywhere;
                    // allocate a fresh id as a defensive fallback so
                    // the lock-step invariant survives a legacy path.
                    let rid = crate::row_header::RowId(
                        self.next_rowid
                            .fetch_add(1, core::sync::atomic::Ordering::Relaxed),
                    );
                    new_rowids.push_mut(rid);
                }
            }
        }
        self.rows = new_rows;
        self.headers = new_headers;
        self.rowids = new_rowids;
        self.hot_bytes = self.hot_bytes.saturating_sub(removed_bytes);
        self.dead_rows = surviving_dead;
        debug_assert_eq!(
            self.rows.len(),
            self.headers.len(),
            "headers must stay in lock-step with rows after delete_rows_no_index"
        );
        removed
    }

    /// v7.37.5 — public alias for the private `rebuild_indices` helper.
    /// Used by `Catalog::apply_redo` to coalesce per-record rebuilds
    /// across a batch of `RowChange`s into one rebuild per touched table.
    pub fn rebuild_indices_pub(&mut self) {
        self.rebuild_indices();
    }

    /// v7.37.5 (mailrs crash-recovery Ask 3) — replace the table's
    /// row vector + `hot_bytes` in one shot, then rebuild every
    /// index from the new rows. Used by `Catalog::apply_redo`'s
    /// batched run: a contiguous slice of `RowChange`s targeting
    /// this table is composed into a final `(PersistentVec<Row>,
    /// hot_bytes)` pair via in-memory bookkeeping, then handed to
    /// this method ONCE for index regeneration. Replaces N per-
    /// record `rebuild_indices` calls with 1 per run.
    /// v7.39 (flip crash-replay P0) — like
    /// [`Self::set_rows_and_rebuild_indices`] but KEEPS the caller's
    /// per-slot RowIds. Redo replay applies one WAL record per
    /// statement; reassigning ids between records broke every later
    /// record's tombstone targets (they name the ids the crashed
    /// process allocated), resurrecting deleted rows. The id
    /// allocator advances past every preserved id so post-replay
    /// inserts never collide.
    pub fn set_rows_and_rebuild_indices_with_rowids(
        &mut self,
        new_rows: PersistentVec<Row<'static>>,
        new_hot_bytes: u64,
        rowids: &[crate::row_header::RowId],
        headers: &[crate::row_header::RowHeader],
    ) {
        debug_assert_eq!(new_rows.len(), rowids.len());
        debug_assert_eq!(new_rows.len(), headers.len());
        let mut new_headers: PersistentVec<crate::row_header::RowHeader> = PersistentVec::new();
        let mut new_rowids: PersistentVec<crate::row_header::RowId> = PersistentVec::new();
        let mut dead: u64 = 0;
        for (rid, h) in rowids.iter().zip(headers) {
            // Preserve the caller's header — an earlier replayed WAL
            // record's tombstone stamp must survive this record's
            // rebuild (per-statement replay re-freezing every header
            // resurrected every previously-deleted row).
            if h.xmax != crate::row_header::XMAX_ALIVE {
                dead += 1;
            }
            new_headers.push_mut(*h);
            let rid = if *rid == crate::row_header::RowId::UNASSIGNED {
                crate::row_header::RowId(
                    self.next_rowid
                        .fetch_add(1, core::sync::atomic::Ordering::Relaxed),
                )
            } else {
                self.next_rowid
                    .fetch_max(rid.0 + 1, core::sync::atomic::Ordering::Relaxed);
                *rid
            };
            new_rowids.push_mut(rid);
        }
        self.rows = new_rows;
        self.headers = new_headers;
        self.rowids = new_rowids;
        self.hot_bytes = new_hot_bytes;
        self.dead_rows = dead;
        debug_assert_eq!(self.rows.len(), self.headers.len());
        debug_assert_eq!(self.rows.len(), self.rowids.len());
        self.rebuild_indices();
    }

    pub fn set_rows_and_rebuild_indices(
        &mut self,
        new_rows: PersistentVec<Row<'static>>,
        new_hot_bytes: u64,
    ) {
        // v7.37.15 (Phase A.2) — synthesise frozen headers for
        // the replacement rows. Phase D's catalog snapshot format
        // (bumped to V6) will start carrying headers verbatim,
        // letting recovery preserve real xmin/xmax instead of
        // freezing everything; until then frozen is the safe
        // default for replay (all visible to every snapshot).
        let mut new_headers: PersistentVec<crate::row_header::RowHeader> = PersistentVec::new();
        // v7.37.15 (Phase C.1) — fresh monotonic ids for the
        // replacement rows drawn from the relation allocator, so a
        // post-replay id never collides with a pre-replay one.
        let mut new_rowids: PersistentVec<crate::row_header::RowId> = PersistentVec::new();
        for _ in 0..new_rows.len() {
            new_headers.push_mut(crate::row_header::RowHeader::frozen());
            let rid = crate::row_header::RowId(
                self.next_rowid
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed),
            );
            new_rowids.push_mut(rid);
        }
        self.rows = new_rows;
        self.headers = new_headers;
        self.rowids = new_rowids;
        self.hot_bytes = new_hot_bytes;
        // All-frozen replacement headers → no dead rows by construction.
        self.dead_rows = 0;
        debug_assert_eq!(
            self.rows.len(),
            self.headers.len(),
            "headers must stay in lock-step with rows after set_rows_and_rebuild_indices"
        );
        debug_assert_eq!(
            self.rows.len(),
            self.rowids.len(),
            "rowids must stay in lock-step with rows after set_rows_and_rebuild_indices"
        );
        self.rebuild_indices();
    }

    /// v7.37.5 (mailrs crash-recovery Ask 3) — row-only insert for the
    /// WAL-replay batch path: pushes the row + bumps `hot_bytes`, and
    /// **does NOT** update any index (B-tree, GIN, NSW). The caller is
    /// responsible for invoking `rebuild_indices_pub` once after a
    /// sequence of `*_no_index` mutations on this table.
    /// Schema validation (arity + per-column type compatibility) is
    /// applied so a malformed redo log surfaces honestly.
    pub fn insert_no_index(&mut self, row: Row<'static>) -> Result<(), StorageError> {
        if row.len() != self.schema.columns.len() {
            return Err(StorageError::ArityMismatch {
                expected: self.schema.columns.len(),
                actual: row.len(),
            });
        }
        validate_row_against_schema(&row.values, &self.schema)?;
        self.hot_bytes = self
            .hot_bytes
            .saturating_add(row_body_encoded_len(&row, &self.schema) as u64);
        self.rows.push_mut(row);
        // v7.37.15 (Phase A.2) — keep headers lock-step for the
        // WAL replay path. Replay-time headers are frozen because
        // pre-V6 envelopes carry no header info; Phase D will
        // restore the original xmin/xmax once the V6 catalog
        // format ships.
        self.headers
            .push_mut(crate::row_header::RowHeader::frozen());
        // v7.37.15 (Phase C.1) — RowId lock-step for the WAL-replay
        // append path.
        let rid = self.alloc_rowid();
        self.rowids.push_mut(rid);
        debug_assert_eq!(
            self.rows.len(),
            self.headers.len(),
            "headers must stay in lock-step with rows after insert_no_index"
        );
        debug_assert_eq!(
            self.rows.len(),
            self.rowids.len(),
            "rowids must stay in lock-step with rows after insert_no_index"
        );
        Ok(())
    }

    /// v7.37.5 (mailrs crash-recovery Ask 3) — row-only update for the
    /// WAL-replay batch path: replaces the row at `position` + adjusts
    /// `hot_bytes`, and **does NOT** touch any index. Skipping the
    /// per-update incremental index work is safe because the trailing
    /// `rebuild_indices_pub` regenerates indices from `self.rows` in
    /// their final state.
    pub fn update_row_no_index(
        &mut self,
        position: usize,
        new_values: Vec<Value<'static>>,
    ) -> Result<(), StorageError> {
        if position >= self.rows.len() {
            return Err(StorageError::Corrupt(alloc::format!(
                "update_row_no_index: position {position} out of bounds (rows={})",
                self.rows.len()
            )));
        }
        if new_values.len() != self.schema.columns.len() {
            return Err(StorageError::ArityMismatch {
                expected: self.schema.columns.len(),
                actual: new_values.len(),
            });
        }
        validate_row_against_schema(&new_values, &self.schema)?;
        let old_row = self
            .rows
            .get(position)
            .expect("position bounds-checked above");
        let old_bytes = row_body_encoded_len(old_row, &self.schema) as u64;
        let new_row = Row::new(new_values);
        let new_bytes = row_body_encoded_len(&new_row, &self.schema) as u64;
        self.rows = self
            .rows
            .set(position, new_row)
            .expect("position bounds-checked above");
        self.hot_bytes = self
            .hot_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
        Ok(())
    }

    /// v4.4: replace the row at `position` with `new_values` (must
    /// match the schema arity + types). v7.20: index maintenance is
    /// incremental — only indices whose key value changed are
    /// touched (B-tree entry move in place; NSW / BRIN / GIN fall
    /// back to a full rebuild when their column changed).
    pub fn update_row(
        &mut self,
        position: usize,
        new_values: Vec<Value<'static>>,
    ) -> Result<(), StorageError> {
        if position >= self.rows.len() {
            return Err(StorageError::Corrupt(alloc::format!(
                "update_row: position {position} out of bounds (rows={})",
                self.rows.len()
            )));
        }
        if new_values.len() != self.schema.columns.len() {
            return Err(StorageError::ArityMismatch {
                expected: self.schema.columns.len(),
                actual: new_values.len(),
            });
        }
        // Reuse the per-cell type-compat validation that `insert`
        // applies. The body below mirrors that check intentionally —
        // factoring it would be more code than the duplication.
        for (i, (val, col)) in new_values.iter().zip(&self.schema.columns).enumerate() {
            if val.is_null() {
                if !col.nullable {
                    return Err(StorageError::NullInNotNull {
                        column: col.name.clone(),
                    });
                }
                continue;
            }
            // v7.39 (read01 round 54) — `data_type()` is None for the
            // eval-only variants that carry no DataType (RegClass, Composite).
            // They are NOT NULL, so `.expect("non-null")` PANICKED on them —
            // materialising a CTE like `WITH w AS (SELECT 't'::regclass)` blew
            // up the query with an "internal error". Report a clean type
            // mismatch instead; the engine coerces these before they get here
            // on every path that knows how.
            let Some(actual) = val.data_type() else {
                // An eval-only value (RegClass carries oid + name, Composite a
                // field tuple) has no DataType in the storage lattice. It is
                // NOT NULL, so the old `.expect("non-null")` PANICKED — which
                // is how `WITH w AS (SELECT 't'::regclass)` blew up with an
                // "internal error". Accept it: the value keeps its dual shape
                // and downstream comparisons (RegClass vs BigInt oid) handle it.
                continue;
            };
            let compatible = column_accepts(actual, col.ty);
            if !compatible {
                return Err(StorageError::TypeMismatch {
                    column: col.name.clone(),
                    expected: col.ty,
                    actual,
                    position: i,
                });
            }
        }
        let old_row = self
            .rows
            .get(position)
            .expect("position bounds-checked above");
        let old_bytes = row_body_encoded_len(old_row, &self.schema) as u64;
        let new_row = Row::new(new_values);
        let new_bytes = row_body_encoded_len(&new_row, &self.schema) as u64;
        // v7.20 P4 — incremental index maintenance. `rows.set`
        // replaces the row in place, so every OTHER row's Hot
        // locator stays valid; only indices whose key value
        // actually changed at `position` need touching. The
        // common OLTP shape (`UPDATE … SET non_indexed_col = …
        // WHERE pk = $1`) touches no index at all — pre-v7.20
        // this path paid a full rebuild_indices() (O(rows ×
        // indices)) per UPDATE, which dominated the profiled
        // write cost on a 5k-row table (~1 ms/stmt).
        //
        // BTree gets an in-place entry move (drop Hot(position)
        // from the old key's locator list, append to the new
        // key's). NSW graphs / BRIN summaries / GIN posting
        // lists have no cheap single-key move — a changed column
        // under one of those falls back to the full rebuild.
        enum IdxFix {
            BTreeMove {
                idx_pos: usize,
                old_key: Option<IndexKey>,
                new_key: Option<IndexKey>,
            },
            // v7.38.1 (L12) — composite-key move. `None` = the row is
            // not in the index on that side (some component unkeyable).
            MultiMove {
                idx_pos: usize,
                old_key: Option<alloc::boxed::Box<[IndexKey]>>,
                new_key: Option<alloc::boxed::Box<[IndexKey]>>,
            },
            FullRebuild,
        }
        let mut fixes: Vec<IdxFix> = Vec::new();
        for (idx_pos, idx) in self.indices.iter().enumerate() {
            let col = idx.column_position;
            // v7.38.1 (L12) — a multi index moves when ANY component
            // changes, so it must be judged on the whole tuple BEFORE
            // the leading-column short-circuit below can skip it.
            if matches!(idx.kind, IndexKind::BTreeMulti(_)) {
                // Cheap pre-check on the raw values — composing two
                // boxed key tuples per index per UPDATE is real money
                // on write-heavy loads, and most updates touch no key
                // component at all.
                let component_changed = core::iter::once(col)
                    .chain(idx.extra_column_positions.iter().copied())
                    .any(|p| old_row.values.get(p) != new_row.values.get(p));
                if component_changed {
                    let old_key = idx.multi_key_for_row(&old_row.values);
                    let new_key = idx.multi_key_for_row(&new_row.values);
                    if old_key != new_key {
                        fixes.push(IdxFix::MultiMove {
                            idx_pos,
                            old_key,
                            new_key,
                        });
                    }
                }
                continue;
            }
            let old_v = &old_row.values[col];
            let new_v = &new_row.values[col];
            if old_v == new_v {
                continue;
            }
            match &idx.kind {
                IndexKind::BTree(_) => fixes.push(IdxFix::BTreeMove {
                    idx_pos,
                    old_key: IndexKey::from_value(old_v),
                    new_key: IndexKey::from_value(new_v),
                }),
                IndexKind::Nsw(_)
                | IndexKind::Brin { .. }
                | IndexKind::Gin(_)
                | IndexKind::GinTrgm(_)
                | IndexKind::GinFulltext(_)
                | IndexKind::GinJsonb(_)
                | IndexKind::BTreeMulti(_) => {
                    fixes.clear();
                    fixes.push(IdxFix::FullRebuild);
                    break;
                }
            }
        }
        // v7.39 (round 215) — capture the range-exclusion key move BEFORE the
        // in-place `set` consumes `new_row`. A `FullRebuild` (a GIN/NSW/BRIN
        // column changed) rebuilds the excl indexes too via `rebuild_indices`,
        // so only apply the incremental move on the pure-BTreeMove path.
        let excl_has_full = fixes.iter().any(|f| matches!(f, IdxFix::FullRebuild));
        let excl_moves: Vec<(usize, Option<(i128, u8)>, Option<(i128, u8)>)> =
            if self.excl_indexes.is_empty() || excl_has_full {
                Vec::new()
            } else {
                self.excl_indexes
                    .iter()
                    .filter_map(|e| {
                        let c = e.column_position;
                        let old_k = old_row.values.get(c).and_then(crate::range_excl_index_key);
                        let new_k = new_row.values.get(c).and_then(crate::range_excl_index_key);
                        if old_k == new_k {
                            None // range bound unchanged — no index touch
                        } else {
                            Some((c, old_k, new_k))
                        }
                    })
                    .collect()
            };
        self.rows = self
            .rows
            .set(position, new_row)
            .expect("position bounds-checked above");
        self.hot_bytes = self
            .hot_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
        // v7.34 — capture row-level redo (after the row is in place; the
        // immutable read of the new values is dropped before record_redo's
        // mutable borrow, and gated so capture-off pays nothing).
        if self.redo_log.is_some() {
            let new_row = self
                .rows
                .get(position)
                .map(|r| r.values.clone())
                .unwrap_or_default();
            // v7.37.15 (Epic W slice 1) — carry the stable RowId of the
            // updated row (`position` is bounds-checked above, so the id
            // is present). `writer_version` (xmax of the superseded
            // tuple) is 0: the writing TxId is not threaded here yet.
            let redo_rowid = self
                .rowids()
                .get(position)
                .copied()
                .unwrap_or(crate::row_header::RowId::UNASSIGNED);
            self.record_redo(|table| RowChange::Update {
                table,
                pos: position,
                new_row,
                rowid: redo_rowid,
                writer_version: 0,
            });
        }
        for fix in fixes {
            match fix {
                IdxFix::FullRebuild => {
                    self.rebuild_indices();
                    break;
                }
                IdxFix::BTreeMove {
                    idx_pos,
                    old_key,
                    new_key,
                } => {
                    let IndexKind::BTree(map) = &mut self.indices[idx_pos].kind else {
                        unreachable!("IdxFix::BTreeMove built from a BTree index");
                    };
                    // NULL keys never enter the B-tree (from_value
                    // returns None), so a None on either side means
                    // "no entry on that side".
                    if let Some(k) = old_key
                        && let Some(locs) = map.get(&k)
                    {
                        let mut locs = locs.clone();
                        locs.retain(|l| l != RowLocator::Hot(position));
                        // No remove_mut on the persistent map: an
                        // empty locator list is the tombstone —
                        // lookup_eq returns an empty slice, and the
                        // next rebuild_indices() drops the key.
                        map.insert_mut(k, locs);
                    }
                    if let Some(k) = new_key {
                        if let Some(entries) = map.get_mut(&k) {
                            entries.push(RowLocator::Hot(position));
                        } else {
                            map.insert_mut(
                                k,
                                crate::posting::PostingList::single(RowLocator::Hot(position)),
                            );
                        }
                    }
                }
                // v7.38.1 (L12) — same drop-old/append-new dance over the
                // composite key space.
                IdxFix::MultiMove {
                    idx_pos,
                    old_key,
                    new_key,
                } => {
                    let IndexKind::BTreeMulti(map) = &mut self.indices[idx_pos].kind else {
                        unreachable!("IdxFix::MultiMove built from a BTreeMulti index");
                    };
                    if let Some(k) = old_key
                        && let Some(locs) = map.get(&k)
                    {
                        let mut locs = locs.clone();
                        locs.retain(|l| l != RowLocator::Hot(position));
                        map.insert_mut(k, locs);
                    }
                    if let Some(k) = new_key {
                        if let Some(entries) = map.get_mut(&k) {
                            entries.push(RowLocator::Hot(position));
                        } else {
                            map.insert_mut(
                                k,
                                crate::posting::PostingList::single(RowLocator::Hot(position)),
                            );
                        }
                    }
                }
            }
        }
        // v7.39 (round 215) — apply the range-exclusion key moves captured
        // above (skipped when a FullRebuild already re-emitted every excl
        // index). Same shape as the BTreeMove: drop Hot(position) from the
        // old key, append it to the new key.
        for (col, old_k, new_k) in excl_moves {
            let Some(ex) = self
                .excl_indexes
                .iter_mut()
                .find(|e| e.column_position == col)
            else {
                continue;
            };
            if let Some(k) = old_k
                && let Some(locs) = ex.map.get(&k)
            {
                let mut locs = locs.clone();
                locs.retain(|l| l != RowLocator::Hot(position));
                ex.map.insert_mut(k, locs);
            }
            if let Some(k) = new_k {
                if let Some(entries) = ex.map.get_mut(&k) {
                    entries.push(RowLocator::Hot(position));
                } else {
                    ex.map.insert_mut(
                        k,
                        crate::posting::PostingList::single(RowLocator::Hot(position)),
                    );
                }
            }
        }
        Ok(())
    }

    /// v4.4 helper used by `delete_rows` / `update_row`: discard all
    /// index payloads and rebuild from `self.rows`. Cheap enough
    /// for typical SPG scale (catalogs in the docker-compose
    /// deployment shape are small); the alternative — incremental
    /// shift bookkeeping across B-tree + NSW — would be far more
    /// invasive than the savings justify.
    fn rebuild_indices(&mut self) {
        // v5.2.3: capture every `Cold` locator on every BTree index
        // before the rebuild, so the from-rows re-emission below
        // (which only produces `Hot` locators) doesn't drop cold-
        // tier entries on keys unrelated to the row that changed.
        // Pre-v5.2.3 this was a `freeze_oldest_to_cold` worry only
        // and the freezer did its own capture-then-reregister; v5.2.3
        // promotes that pattern into the base helper because UPDATE
        // / DELETE now run rebuild_indices on tables with cold rows.
        let preserved_cold: Vec<(String, Vec<(IndexKey, RowLocator)>)> = self
            .indices
            .iter()
            .filter_map(|idx| match &idx.kind {
                IndexKind::BTree(map) => {
                    let cold: Vec<(IndexKey, RowLocator)> = map
                        .iter()
                        .flat_map(|(k, locs)| {
                            locs.iter()
                                .filter(|l| l.is_cold())
                                .copied()
                                .map(move |l| (k.clone(), l))
                        })
                        .collect();
                    if cold.is_empty() {
                        None
                    } else {
                        Some((idx.name.clone(), cold))
                    }
                }
                // BRIN / NSW carry no key→locator map. GIN handles
                // its own cold preservation below in `preserved_gin_cold`.
                // BTreeMulti never receives Cold locators (the freezer
                // refuses tables carrying one — see freeze site note).
                IndexKind::Nsw(_)
                | IndexKind::Brin { .. }
                | IndexKind::Gin(_)
                | IndexKind::GinTrgm(_)
                | IndexKind::GinFulltext(_)
                | IndexKind::GinJsonb(_)
                | IndexKind::BTreeMulti(_) => None,
            })
            .collect();

        // v7.12.3 — same cold-preservation pattern for GIN's
        // `word → Vec<RowLocator>` posting lists. Parallel to the
        // BTree pass above (different key type so a separate vec is
        // cleaner than a generic merge). v7.15.0: trigram-GIN
        // (`gin_trgm_ops`) shares the same posting-list shape, so
        // one pass handles both — the `RebuildKind` carries the
        // kind tag to drive resurrection.
        let preserved_gin_cold: Vec<(String, Vec<(String, RowLocator)>)> = self
            .indices
            .iter()
            .filter_map(|idx| match &idx.kind {
                // v7.17.0 Phase 2.2 — fulltext-GIN posting lists
                // share the `String → Vec<RowLocator>` shape, so
                // cold preservation handles all three GIN flavours
                // in one pass.
                IndexKind::Gin(map)
                | IndexKind::GinTrgm(map)
                | IndexKind::GinFulltext(map)
                | IndexKind::GinJsonb(map) => {
                    let cold: Vec<(String, RowLocator)> = map
                        .iter()
                        .flat_map(|(w, locs)| {
                            locs.iter()
                                .filter(|l| l.is_cold())
                                .copied()
                                .map(move |l| (w.clone(), l))
                        })
                        .collect();
                    if cold.is_empty() {
                        None
                    } else {
                        Some((idx.name.clone(), cold))
                    }
                }
                IndexKind::BTree(_)
                | IndexKind::Nsw(_)
                | IndexKind::Brin { .. }
                | IndexKind::BTreeMulti(_) => None,
            })
            .collect();

        // v6.7.1 — descriptor needs to capture index kind so the
        // rebuild loop can resurrect BTree / NSW / BRIN / GIN exactly
        // as they were. (NSW carries m; BRIN carries the column type
        // snapshot; BTree / GIN need no extra payload.)
        #[derive(Clone)]
        enum RebuildKind {
            BTree,
            // v7.38.1 (L12) — rebuilt from rows over the full column
            // tuple, exactly like BTree but with composite keys.
            BTreeMulti,
            Nsw(usize),
            Brin(DataType),
            Gin,
            GinTrgm,
            GinFulltext,
            GinJsonb,
        }
        // v7.39 (round 170) — the descriptor must carry the FULL index
        // metadata: the rebuild used to reconstruct via bare
        // `Index::new_btree(name, pos)`, silently DROPPING is_unique /
        // extra_column_positions / partial_predicate / expression /
        // included_columns / nulls_not_distinct — so the first VACUUM
        // (or any delete-path rebuild) turned every UNIQUE INDEX into a
        // plain one and stopped enforcing it (probe-reproduced:
        // duplicate keys inserted silently after VACUUM).
        struct RebuildDesc {
            name: String,
            column_position: usize,
            kind: RebuildKind,
            is_unique: bool,
            extra_column_positions: Vec<usize>,
            partial_predicate: Option<String>,
            expression: Option<String>,
            included_columns: Vec<usize>,
            nulls_not_distinct: bool,
            // v7.39 (round 537) — carried through a rebuild like the rest.
            descending: bool,
            nulls_first: Option<bool>,
            collation: Option<String>,
        }
        let descriptors: Vec<RebuildDesc> = self
            .indices
            .iter()
            .map(|idx| {
                let kind = match &idx.kind {
                    IndexKind::Nsw(g) => RebuildKind::Nsw(g.m),
                    IndexKind::Brin { column_type, .. } => RebuildKind::Brin(*column_type),
                    IndexKind::BTree(_) => RebuildKind::BTree,
                    IndexKind::BTreeMulti(_) => RebuildKind::BTreeMulti,
                    IndexKind::Gin(_) => RebuildKind::Gin,
                    IndexKind::GinTrgm(_) => RebuildKind::GinTrgm,
                    IndexKind::GinFulltext(_) => RebuildKind::GinFulltext,
                    IndexKind::GinJsonb(_) => RebuildKind::GinJsonb,
                };
                RebuildDesc {
                    name: idx.name.clone(),
                    column_position: idx.column_position,
                    kind,
                    is_unique: idx.is_unique,
                    extra_column_positions: idx.extra_column_positions.clone(),
                    partial_predicate: idx.partial_predicate.clone(),
                    expression: idx.expression.clone(),
                    included_columns: idx.included_columns.clone(),
                    nulls_not_distinct: idx.nulls_not_distinct,
                    descending: idx.descending,
                    nulls_first: idx.nulls_first,
                    collation: idx.collation.clone(),
                }
            })
            .collect();
        self.indices.clear();
        for desc in descriptors {
            let RebuildDesc {
                name,
                column_position,
                kind: rebuild_kind,
                is_unique,
                extra_column_positions,
                partial_predicate,
                expression,
                included_columns,
                nulls_not_distinct,
                descending,
                nulls_first,
                collation,
            } = desc;
            let pre_len = self.indices.len();
            match rebuild_kind {
                RebuildKind::Nsw(m) => {
                    let idx = Index::new_nsw(name, column_position, m);
                    self.indices.push(idx);
                    let idx_pos = self.indices.len() - 1;
                    let row_indices: Vec<usize> = (0..self.rows.len()).collect();
                    for row_idx in row_indices {
                        nsw_insert_at(self, idx_pos, row_idx);
                    }
                }
                RebuildKind::Brin(column_type) => {
                    self.indices
                        .push(Index::new_brin(name, column_position, column_type));
                    // v7.38.11 — recompute the hot-tier summaries. They
                    // are derived from the rows, so a rebuild is a
                    // single pass and can never disagree with what is
                    // stored; that is also why they are not serialised.
                    //
                    // Without this an UPDATE — which lands here, since
                    // BRIN cannot be repaired in place — would leave the
                    // summaries empty. That is SAFE (an absent summary
                    // is never skipped) but it silently turns pruning
                    // off for the rest of the table's life, which is the
                    // kind of regression nothing would report.
                    let idx_pos = self.indices.len() - 1;
                    let n = self.rows.len();
                    let mut sums: Vec<Option<(i64, i64)>> =
                        alloc::vec![None; n.div_ceil(crate::BRIN_RANGE_ROWS)];
                    let mut cur = self.rows.run_cursor();
                    for i in 0..n {
                        let Some(row) = cur.get(i) else { continue };
                        let Some(v) = row.values.get(column_position) else {
                            continue;
                        };
                        if let Some(k) = crate::brin_scalar(v) {
                            let r = i / crate::BRIN_RANGE_ROWS;
                            sums[r] = Some(match sums[r] {
                                Some((lo, hi)) => (lo.min(k), hi.max(k)),
                                None => (k, k),
                            });
                        }
                    }
                    if let crate::IndexKind::Brin { summaries, .. } =
                        &mut self.indices[idx_pos].kind
                    {
                        *summaries = sums;
                    }
                }
                RebuildKind::BTree => {
                    // v7.39 (round 170) — bulk build: collect + sort +
                    // group + from_sorted. The per-row insert_mut paid a
                    // path-copy allocation per row per index (~15ms per
                    // index on a 50k-row VACUUM, the dominant cost).
                    let mut idx = Index::new_btree(name, column_position);
                    let mut pairs: Vec<(IndexKey, usize)> = Vec::with_capacity(self.rows.len());
                    for (i, row) in self.rows.iter().enumerate() {
                        if let Some(key) = IndexKey::from_value(&row.values[column_position]) {
                            pairs.push((key, i));
                        }
                    }
                    pairs.sort_by(|a, b| a.0.cmp(&b.0));
                    let mut grouped: Vec<(IndexKey, crate::posting::PostingList)> = Vec::new();
                    for (key, i) in pairs {
                        match grouped.last_mut() {
                            Some((k, locs)) if *k == key => locs.push(RowLocator::Hot(i)),
                            _ => grouped.push((
                                key,
                                crate::posting::PostingList::single(RowLocator::Hot(i)),
                            )),
                        }
                    }
                    idx.kind = IndexKind::BTree(
                        crate::persistent_btree::PersistentBTreeMap::from_sorted(grouped),
                    );
                    self.indices.push(idx);
                }
                // v7.38.1 (L12) — bulk build over the full column tuple.
                // Same collect + sort + group + from_sorted shape as the
                // BTree arm; a row with any unkeyable component stays out.
                RebuildKind::BTreeMulti => {
                    let mut idx = Index::new_btree_multi(name, column_position);
                    let mut pairs: Vec<(alloc::boxed::Box<[IndexKey]>, usize)> =
                        Vec::with_capacity(self.rows.len());
                    for (i, row) in self.rows.iter().enumerate() {
                        if let Some(key) = crate::compose_multi_key(
                            &row.values,
                            column_position,
                            &extra_column_positions,
                        ) {
                            pairs.push((key, i));
                        }
                    }
                    pairs.sort_by(|a, b| a.0.cmp(&b.0));
                    let mut grouped: Vec<(
                        alloc::boxed::Box<[IndexKey]>,
                        crate::posting::PostingList,
                    )> = Vec::new();
                    for (key, i) in pairs {
                        match grouped.last_mut() {
                            Some((k, locs)) if *k == key => locs.push(RowLocator::Hot(i)),
                            _ => grouped.push((
                                key,
                                crate::posting::PostingList::single(RowLocator::Hot(i)),
                            )),
                        }
                    }
                    idx.kind = IndexKind::BTreeMulti(
                        crate::persistent_btree::PersistentBTreeMap::from_sorted(grouped),
                    );
                    self.indices.push(idx);
                }
                RebuildKind::Gin => {
                    let mut idx = Index::new_gin(name, column_position);
                    if let IndexKind::Gin(map) = &mut idx.kind {
                        for (i, row) in self.rows.iter().enumerate() {
                            if let Value::TsVector(lexemes) = &row.values[column_position] {
                                for lex in lexemes {
                                    if let Some(entries) = map.get_mut(&lex.word) {
                                        entries.push(RowLocator::Hot(i));
                                    } else {
                                        map.insert_mut(
                                            lex.word.clone(),
                                            crate::posting::PostingList::single(RowLocator::Hot(i)),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    self.indices.push(idx);
                }
                RebuildKind::GinTrgm => {
                    let mut idx = Index::new_gin_trgm(name, column_position);
                    if let IndexKind::GinTrgm(map) = &mut idx.kind {
                        for (i, row) in self.rows.iter().enumerate() {
                            if let Value::Text(s) = &row.values[column_position] {
                                for tri in trgm::extract_trigrams(s) {
                                    // r1019 — address the String-keyed map with the borrowed
                                    // trigram; allocate one only for a key the map has never
                                    // seen, which after the first rows is rare.
                                    let key = trgm::trigram_str(&tri);
                                    if let Some(entries) = map.get_mut_by(key) {
                                        entries.push(RowLocator::Hot(i));
                                    } else {
                                        map.insert_mut(
                                            alloc::string::ToString::to_string(key),
                                            crate::posting::PostingList::single(RowLocator::Hot(i)),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    self.indices.push(idx);
                }
                RebuildKind::GinFulltext => {
                    // v7.17.0 Phase 2.2 — re-derive the lexeme
                    // posting list from each TEXT/VARCHAR cell.
                    // Mirrors the GinTrgm rebuild shape but
                    // tokenises via `fts_simple::simple_lex`
                    // (same rule as `to_tsvector('simple')`).
                    let mut idx = Index::new_gin_fulltext(name, column_position);
                    if let IndexKind::GinFulltext(map) = &mut idx.kind {
                        for (i, row) in self.rows.iter().enumerate() {
                            if let Value::Text(s) = &row.values[column_position] {
                                for lex in fts_simple::simple_lex(s) {
                                    if let Some(entries) = map.get_mut(&lex) {
                                        entries.push(RowLocator::Hot(i));
                                    } else {
                                        map.insert_mut(
                                            lex,
                                            crate::posting::PostingList::single(RowLocator::Hot(i)),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    self.indices.push(idx);
                }
                RebuildKind::GinJsonb => {
                    // v7.37.8 — re-derive the JSONB posting list
                    // from each `Value::Json` cell.
                    let mut idx = Index::new_gin_jsonb(name, column_position);
                    if let IndexKind::GinJsonb(map) = &mut idx.kind {
                        for (i, row) in self.rows.iter().enumerate() {
                            if let Value::Json(s) = &row.values[column_position] {
                                for tok in jsonb_gin::extract_tokens(s) {
                                    if let Some(entries) = map.get_mut(&tok) {
                                        entries.push(RowLocator::Hot(i));
                                    } else {
                                        map.insert_mut(
                                            tok,
                                            crate::posting::PostingList::single(RowLocator::Hot(i)),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    self.indices.push(idx);
                }
            }
            // v7.39 (round 170) — restore the captured metadata onto
            // whatever this arm pushed (see RebuildDesc above).
            if let Some(idx) = self.indices.get_mut(pre_len) {
                idx.is_unique = is_unique;
                idx.extra_column_positions = extra_column_positions;
                idx.partial_predicate = partial_predicate;
                idx.expression = expression;
                idx.included_columns = included_columns;
                idx.nulls_not_distinct = nulls_not_distinct;
                idx.descending = descending;
                idx.nulls_first = nulls_first;
                idx.collation = collation;
            }
        }

        // Re-attach preserved cold locators after the from-rows
        // rebuild. `register_cold_locators` handles the per-key
        // entries-vec append; no key collisions arise because the
        // rebuild loop above produced only Hot locators.
        for (idx_name, locators) in preserved_cold {
            // Errors here would only fire if the index disappeared
            // between snapshot and rebuild, which can't happen
            // because the rebuild restores the same descriptor set.
            let _ = self.register_cold_locators(&idx_name, locators);
        }
        // v7.12.3 — same for GIN posting-list cold locators.
        for (idx_name, locators) in preserved_gin_cold {
            let _ = self.register_gin_cold_locators(&idx_name, locators);
        }
        // v7.39 (round 215) — the range-exclusion indexes address rows by the
        // same physical slot, so a compaction that shifted slots invalidates
        // their Hot locators too. Re-emit them from the (post-compaction) rows.
        if !self.excl_indexes.is_empty() {
            self.rebuild_excl_indexes();
        }
    }

    fn add_nsw_index_inner(
        &mut self,
        name: String,
        column_name: &str,
        m: usize,
        restore: Option<NswGraph>,
    ) -> Result<(), StorageError> {
        if self.indices.iter().any(|i| i.name == name) {
            return Err(StorageError::DuplicateIndex { name });
        }
        let column_position = self.schema.column_position(column_name).ok_or_else(|| {
            StorageError::ColumnNotFound {
                column: column_name.into(),
            }
        })?;
        if !matches!(
            self.schema.columns[column_position].ty,
            DataType::Vector { .. }
        ) {
            return Err(StorageError::TypeMismatch {
                column: column_name.into(),
                expected: DataType::Vector {
                    dim: 0,
                    encoding: VecEncoding::F32,
                },
                actual: self.schema.columns[column_position].ty,
                position: column_position,
            });
        }
        if let Some(graph) = restore {
            self.indices.push(Index {
                name,
                column_position,
                kind: IndexKind::Nsw(graph),
                included_columns: Vec::new(),
                partial_predicate: None,
                expression: None,
                is_unique: false,
                nulls_not_distinct: false,
                descending: false,
                nulls_first: None,
                collation: None,
                extra_column_positions: Vec::new(),
            });
            return Ok(());
        }
        let idx = Index::new_nsw(name, column_position, m);
        self.indices.push(idx);
        let idx_pos = self.indices.len() - 1;
        // Bulk-build by walking the existing rows in order — each insert
        // sees the partial graph and links into it.
        let row_indices: Vec<usize> = (0..self.rows.len()).collect();
        for row_idx in row_indices {
            nsw_insert_at(self, idx_pos, row_idx);
        }
        Ok(())
    }
}

/// v7.37.5 (mailrs crash-recovery Ask 3) — per-cell schema-compat
/// check shared by `insert_no_index` and `update_row_no_index`. The
/// logic mirrors the inline body in `insert` / `update_row` (NULL
/// handling, the cross-type compatibility map: TEXT ↔ VARCHAR/CHAR/
/// JSON/JSONB, TIMESTAMP ↔ TIMESTAMPTZ, BIT ↔ VARBIT, INET ↔ CIDR,
/// NUMERIC scale match).
/// v7.39 (round 642/643) — does a value of type `actual` belong in a
/// column declared `declared`?
///
/// This existed in THREE copies — insert, update and the standalone
/// row validator — and they had drifted apart in three independent
/// places: only insert accepted the `name` pairs, only insert and
/// update accepted a bit-to-bit pair with differing typmods, and only
/// update accepted a NEGATIVE declared numeric scale. Each omission was
/// a hole waiting for a value to reach that path; none was a deliberate
/// tightening, so the union below is the rule and all three now ask it.
///
/// Measured before converging: every shape the three disagreed about
/// answers identically to PG18 today, so this fixes nothing observable.
/// What it fixes is the next type — adding `xid` in round 640 meant
/// remembering to patch three places, and forgetting one would have
/// half-wired it.
///
/// The rule itself: a pair is compatible when the value's storage shape
/// is what the column stores. Length and precision contracts are NOT
/// checked here — they belong to coercion, which runs first.
/// `#[inline]` is not decoration. Extracting this matrix out of its
/// three call sites — a change with no semantic content at all — cost
/// `SELECT count(*) FROM d WHERE g BETWEEN 10 AND 20` **23x**, 5.8 ms
/// to 133 ms over 500 000 rows, reproducibly and outside the panel.
/// None of the three callers is on a scan path; taking the matrix out
/// of them was enough to move whatever else in this module the row loop
/// depends on being inlined. Round 641 learned the same thing about
/// `eval::binop::compare`. A refactor that reads as pure structure is
/// still a codegen change.
#[inline]
fn column_accepts(actual: DataType, declared: DataType) -> bool {
    if actual == declared {
        return true;
    }
    if matches!(
        (actual, declared),
        // A NAME column stores a Value::Text: the type identity is the
        // schema's and a value can never be one, so both directions.
        (
            DataType::Text,
            DataType::Varchar(_)
                | DataType::Char(_)
                | DataType::Name
                | DataType::Json
                | DataType::Jsonb
        ) | (DataType::Name, DataType::Text)
            // An XID column stores the Value::BigInt a transaction id
            // has always been; xid8 has no value of its own at all.
            | (DataType::BigInt, DataType::Xid | DataType::Xid8)
            | (DataType::Xid | DataType::Xid8, DataType::BigInt)
            // v7.39 (round 667) — an OID column likewise stores a plain
            // integer. INT is listed as well as BIGINT because a bare
            // literal arrives as one: PG takes `INSERT INTO t(o) VALUES
            // (42)` into an oid column, and measured, it does NOT take the
            // same integer into an xid column ("column is of type xid but
            // expression is of type integer"). SPG has been laxer than PG
            // on that xid direction since before this round — that is the
            // limitation `DataType::Xid8` documents, not something added
            // here.
            | (
                DataType::BigInt | DataType::Int | DataType::SmallInt,
                DataType::Oid,
            )
            | (DataType::Oid, DataType::BigInt | DataType::Int)
            // v7.39 (round 694) — `oid[]` rides in a BigIntArray cell, so
            // it accepts one either way, exactly as the scalar above does.
            | (DataType::BigIntArray | DataType::IntArray, DataType::OidArray)
            | (DataType::OidArray, DataType::BigIntArray)
            | (DataType::Json | DataType::Jsonb, DataType::Text)
            | (DataType::Json, DataType::Jsonb)
            | (DataType::Jsonb, DataType::Json)
            | (DataType::Timestamp, DataType::Timestamptz)
            | (DataType::Timestamptz, DataType::Timestamp)
            // BIT / VARBIT share the BitString storage shape; INET /
            // CIDR likewise. Same-family pairs with different typmods
            // are compatible HERE — the length contract is coercion's.
            | (DataType::Bit(_), DataType::BitVarying(_))
            | (DataType::BitVarying(_), DataType::Bit(_))
            | (DataType::Bit(_), DataType::Bit(_))
            | (DataType::BitVarying(_), DataType::BitVarying(_))
            | (DataType::Inet, DataType::Cidr)
            | (DataType::Cidr, DataType::Inet)
    ) {
        return true;
    }
    // NUMERIC carries its own scale in the value while the column
    // declares the expected one. An unconstrained `numeric` (the
    // precision-0/scale-0 sentinel) takes any scale; a declared
    // `numeric(p,s)` needs the rescaled value; and a NEGATIVE declared
    // scale stores at display scale 0, having been rounded to a
    // multiple of 10^|s|.
    matches!(
        (actual, declared),
        (
            DataType::Numeric { scale: a, .. },
            DataType::Numeric {
                precision: bp,
                scale: b,
            },
        ) if a == b || (bp == 0 && b == 0) || (b < 0 && a == 0)
    )
}

fn validate_row_against_schema(
    values: &[Value<'static>],
    schema: &TableSchema,
) -> Result<(), StorageError> {
    for (i, (val, col)) in values.iter().zip(&schema.columns).enumerate() {
        if val.is_null() {
            if !col.nullable {
                return Err(StorageError::NullInNotNull {
                    column: col.name.clone(),
                });
            }
            continue;
        }
        // v7.39 (read01 round 54) — see above: no panic on an untyped value.
        let Some(actual) = val.data_type() else {
            // See above: an eval-only untyped value is accepted, not a panic.
            continue;
        };
        let compatible = column_accepts(actual, col.ty);
        if !compatible {
            return Err(StorageError::TypeMismatch {
                column: col.name.clone(),
                expected: col.ty,
                actual,
                position: i,
            });
        }
    }
    Ok(())
}

/// v6.0.4 — re-encode a single cell to the target `VecEncoding`.
/// Used by `Table::rebuild_nsw_index` when ALTER INDEX REBUILD
/// includes the optional `WITH (encoding = …)` clause. Round-trip
/// goes through f32: `current → Vec<f32> → target`, leaving NULL
/// cells untouched. Returns `Unsupported` on a non-vector cell —
/// the caller should have rejected the schema before reaching this.
fn recode_vector_cell(
    cell: Value<'static>,
    target: VecEncoding,
) -> Result<Value<'static>, StorageError> {
    if matches!(cell, Value::Null) {
        return Ok(cell);
    }
    // Step 1 — extract the f32 representation of the source cell.
    let as_f32: Vec<f32> = match &cell {
        Value::Vector(v) => v.to_vec(),
        Value::Sq8Vector(q) => quantize::dequantize(q),
        Value::HalfVector(h) => h.to_f32_vec(),
        other => {
            return Err(StorageError::Unsupported(format!(
                "ALTER INDEX REBUILD: cannot recode non-vector cell {:?}",
                other.data_type()
            )));
        }
    };
    // Step 2 — encode into the target shape. `F32` is the identity
    // path (saves one alloc round-trip when the source is already
    // F32 — but `Value::Vector(as_f32)` is the right answer
    // regardless).
    Ok(match target {
        VecEncoding::F32 => Value::Vector(Cow::Owned(as_f32)),
        VecEncoding::Sq8 => Value::Sq8Vector(quantize::quantize(&as_f32)),
        VecEncoding::F16 => Value::HalfVector(halfvec::HalfVector::from_f32_slice(&as_f32)),
    })
}

/// v7.39 (round 562) — a cursor over `Table`'s row headers that holds
/// the trie leaf it last descended to.
///
/// See `Table::header_runs` for why. Ask about ascending positions and
/// the descent happens once per 32; ask about scattered ones and it
/// happens as often as `position_visible` would have done it.
#[derive(Debug)]
pub struct HeaderRuns<'a> {
    table: &'a Table,
    /// `(start, run)` — `run[i - start]` is the header for position `i`.
    run: Option<(usize, &'a [crate::row_header::RowHeader])>,
}

impl HeaderRuns<'_> {
    /// Is the row at this position visible to the snapshot?
    ///
    /// Answers exactly as `Table::position_visible` does — same
    /// `SKIP LOCKED` handling, same snapshot rules — and the pins in
    /// `e2e_index_only_scan_round560` hold both to it.
    pub fn visible(&mut self, idx: usize, snapshot: &crate::snapshot::Snapshot) -> bool {
        if let Some((start, run)) = self.run
            && idx >= start
            && idx - start < run.len()
        {
            return self.table.header_visible(idx, &run[idx - start], snapshot);
        }
        let Some((start, run)) = self.table.headers.run_containing(idx) else {
            return false;
        };
        self.run = Some((start, run));
        self.table.header_visible(idx, &run[idx - start], snapshot)
    }
}
