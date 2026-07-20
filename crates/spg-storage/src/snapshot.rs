//! v7.37.15 (Phase A) — per-statement / per-transaction snapshot.
//!
//! A `Snapshot` captures **which other transactions had committed**
//! at the moment the reader took the snapshot. Combined with the
//! row's [`crate::row_header::RowHeader`] it answers the central
//! MVCC question: "should THIS reader see THIS row?"
//!
//! ## Compared to PG
//!
//! PG `SnapshotData` carries `xmin / xmax / xip[] / xcnt /
//! suboverflowed / takenDuringRecovery / curcid / speculativeToken /
//! whenTaken / lsn`. SPG strips that to the four fields the
//! visibility rule actually consumes:
//!
//! - `version` — the upper bound: any row whose `xmin` exceeds
//!   this didn't exist at snapshot time.
//! - `in_progress` — the bitset of transactions that were ALREADY
//!   ALLOCATED (i.e. `xmin <= version`) but had NOT YET committed
//!   at snapshot time. Their writes are invisible to this reader.
//! - `oldest_active` — the floor used by vacuum to safely reclaim
//!   tombstones: any row whose `xmax < oldest_active` is dead to
//!   every live snapshot.
//! - `tx_id` — the reader's OWN transaction id, so the snapshot
//!   can implement the "see your own writes" rule (READ COMMITTED
//!   sees its own UPDATE result).
//!
//! That is enough for READ COMMITTED + REPEATABLE READ +
//! SERIALIZABLE (SSI conflict tracking lives in a sidecar — see
//! Phase E).

extern crate alloc;
use alloc::vec::Vec;

use crate::row_header::{HEAP_XMIN_FROZEN, RowHeader, XMAX_ALIVE};

/// v7.37.15 (Phase C.2) — terminal state of a transaction / row
/// version, as seen by the visibility oracle.
///
/// The current [`Snapshot::visible`] rule assumes "not in the
/// snapshot's `in_progress` set ⟹ committed". That holds while every
/// write is committed-and-alive or frozen. Once Phase C.3's in-place
/// write path leaves ABORTED xmin/xmax stamps physically present
/// (rolled-back or crash-orphaned versions vacuum hasn't reclaimed
/// yet), a version can be `<= snapshot.version` and `∉ in_progress`
/// yet aborted — and the two-state rule would wrongly show its rows.
/// [`Snapshot::visible_with_status`] adds the third state.
///
/// The state is **monotonic and immutable once terminal**: a version
/// only ever moves InProgress → {Committed, Aborted} and never back,
/// so a live oracle lookup for a version outside the snapshot's frozen
/// `in_progress` set returns the same answer forever — which is why
/// the snapshot stays a value type and only the oracle is shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XactStatus {
    /// Allocated but not yet committed or aborted.
    InProgress,
    /// Committed — its inserts are real, its deletes took effect.
    Committed,
    /// Rolled back / crash-orphaned — its inserts never happened, its
    /// deletes never took effect.
    Aborted,
}

/// v7.37.15 (Phase C.2) — the visibility oracle: maps a version id to
/// its terminal [`XactStatus`].
///
/// `spg-storage` (`no_std`) defines the contract; the real registry
/// (a sharded `DashMap<u64, XactStatus>`) lives in `spg-engine` and
/// implements this. Kept as a trait so the visibility rule stays in
/// storage next to [`Snapshot::visible`] without storage depending on
/// the engine's concurrency primitives.
pub trait XactStatusOracle {
    /// Terminal status of `version`. Implementations return
    /// [`XactStatus::Committed`] for any version they no longer track
    /// (pruned below `oldest_active`, or frozen) — those are, by
    /// definition, committed-and-old.
    fn status(&self, version: u64) -> XactStatus;
}

/// An oracle that reports every version as committed. Equivalent to
/// the pre-Phase-C.2 two-state world; lets a caller that does not yet
/// track aborts reuse [`Snapshot::visible_with_status`] and get
/// behaviour identical to [`Snapshot::visible`].
#[derive(Debug, Clone, Copy, Default)]
pub struct AllCommitted;

impl XactStatusOracle for AllCommitted {
    #[inline]
    fn status(&self, _version: u64) -> XactStatus {
        XactStatus::Committed
    }
}

/// Compact in-progress set. Stored as a sorted `Vec<u64>` so the
/// `contains` check is a binary search — O(log n) and zero
/// allocation per lookup. We expect `n` to be tens at most (the
/// active transaction count); for that range bsearch beats a
/// hashset by a wide margin on both wall-clock and cache.
///
/// When `n` blows past ~1k (which would mean a runaway leak — every
/// real OLTP workload caps at a few dozen concurrent writers) we
/// would consider a roaring-bitmap-style sparse representation;
/// not needed at v7.37.15.0 scale.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InProgressSet {
    sorted: Vec<u64>,
}

impl InProgressSet {
    /// Construct from a pre-sorted slice. The caller must verify
    /// monotonic order; debug builds assert.
    #[must_use]
    pub fn from_sorted(sorted: Vec<u64>) -> Self {
        debug_assert!(
            sorted.windows(2).all(|w| w[0] < w[1]),
            "InProgressSet::from_sorted requires strictly monotonic input"
        );
        Self { sorted }
    }

    /// Empty set — no transactions in flight. The default for
    /// a snapshot taken in a quiescent moment.
    #[must_use]
    pub const fn empty() -> Self {
        Self { sorted: Vec::new() }
    }

    /// True iff `xid` is one of the in-flight transactions.
    /// Binary search; O(log n).
    #[must_use]
    pub fn contains(&self, xid: u64) -> bool {
        self.sorted.binary_search(&xid).is_ok()
    }

    /// Number of in-flight transactions captured.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sorted.len()
    }

    /// True iff no transactions were in flight at capture time.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sorted.is_empty()
    }
}

/// Per-statement / per-transaction snapshot.
///
/// Cheap to clone (Vec inside an InProgressSet is the only
/// non-Copy field; bounded at active-tx count which is small).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The upper bound. A row whose `xmin` exceeds this is
    /// in the snapshot's future — invisible.
    pub version: u64,
    /// In-flight transactions at snapshot time.
    pub in_progress: InProgressSet,
    /// Floor used by vacuum. Any row whose `xmax < oldest_active`
    /// is dead to every live snapshot.
    pub oldest_active: u64,
    /// The reading transaction's OWN id. Used to implement "see
    /// your own writes" — a row your transaction inserted is
    /// visible to you even before commit. `0` for non-
    /// transactional reads (autocommit SELECT).
    pub tx_id: u64,
    /// v7.39 (round 297, E3 Phase 1b) — rows a `SKIP LOCKED` pre-pass
    /// found held by another transaction, as `(relation, row indices)`.
    ///
    /// It rides the SNAPSHOT because that is the only channel every row
    /// source already threads. Adding the filter at individual scan
    /// sites missed the real path three times running — `index_access`
    /// alone performs the visibility test in ten places. A row that
    /// someone else holds is, for this statement, exactly as
    /// unavailable as a row the snapshot cannot see.
    pub locked_out: Option<(crate::row_header::RelId, alloc::collections::BTreeSet<usize>)>,
}

impl Snapshot {
    /// A "see everything visible" snapshot — version at the
    /// current upper-bound u64, in-progress empty. Equivalent to
    /// the pre-v7.37.15 "Arc-snapshot reads the entire catalog"
    /// behaviour; useful for phase-A migration where the engine
    /// doesn't yet track per-tx state.
    #[must_use]
    pub const fn unbounded() -> Self {
        Self {
            locked_out: None,
            version: u64::MAX,
            in_progress: InProgressSet::empty(),
            oldest_active: u64::MAX,
            tx_id: 0,
        }
    }

    /// Construct from explicit fields. The version cursor and
    /// in-progress set come from the engine's per-process
    /// version counter at snapshot time; oldest_active is the
    /// MIN of every live snapshot's version (vacuum reads it).
    #[must_use]
    pub fn new(version: u64, in_progress: InProgressSet, oldest_active: u64, tx_id: u64) -> Self {
        Self {
            locked_out: None,
            version,
            in_progress,
            oldest_active,
            tx_id,
        }
    }

    /// Should the row be visible to a reader holding this
    /// snapshot? The five-step rule mirrors PG's HeapTupleSatisfiesMVCC.
    ///
    /// 1. Self-write: if the row's writer is THIS reader's own tx,
    ///    the row is visible (READ COMMITTED sees its own writes).
    /// 2. xmin in the future: invisible.
    /// 3. xmin still in-progress: invisible.
    /// 4. Alive (xmax == ALIVE): visible.
    /// 5. xmax in the future or in-progress: visible (the delete
    ///    hasn't committed yet from this reader's point of view).
    /// 6. xmax in the past + committed: invisible (deleted before
    ///    this reader's snapshot).
    #[must_use]
    pub fn visible(&self, h: &RowHeader) -> bool {
        // Step 1: your own writes. A row THIS transaction deleted is
        // invisible to it (whatever inserted it) — PG's "you don't see
        // what you deleted"; a row this transaction inserted and has
        // not deleted is visible. (If `xmin == tx_id` and it isn't the
        // deleter, `xmax` can only be ALIVE — no other transaction can
        // delete a row this uncommitted tx inserted.)
        if self.tx_id != 0 {
            if h.xmax == self.tx_id {
                return false;
            }
            if h.xmin == self.tx_id {
                return true;
            }
        }
        // Step 2: future.
        if h.xmin > self.version {
            return false;
        }
        // Step 3: in-flight at snapshot time.
        if self.in_progress.contains(h.xmin) {
            return false;
        }
        // Step 4: still alive.
        if h.xmax == XMAX_ALIVE {
            return true;
        }
        // Step 5: deletion is future or in-flight → still visible.
        if h.xmax > self.version || self.in_progress.contains(h.xmax) {
            return true;
        }
        // Step 6: deletion committed before our snapshot.
        false
    }

    /// v7.37.15 (Phase C.2) — abort-aware visibility. Same as
    /// [`Self::visible`] but consults a [`XactStatusOracle`] to
    /// distinguish a *committed* version from an *aborted* one when
    /// neither is in the snapshot's `in_progress` set. Phase C.3's
    /// in-place write path needs this: a rolled-back or crash-orphaned
    /// version leaves its xmin/xmax stamp physically present until
    /// vacuum reclaims it, and the two-state [`Self::visible`] would
    /// wrongly treat it as committed.
    ///
    /// Two extra branches vs `visible` (marked NEW):
    /// - xmin aborted → the insert never happened → invisible.
    /// - xmax aborted → the delete never happened → still visible.
    ///
    /// The frozen-and-alive fast path returns before any oracle call,
    /// so steady-state scans over old data pay no oracle cost —
    /// preserving the Phase B "≤5% overhead" result. Passing
    /// [`AllCommitted`] makes this behave exactly like `visible`.
    #[must_use]
    pub fn visible_with_status<O: XactStatusOracle + ?Sized>(
        &self,
        h: &RowHeader,
        xact: &O,
    ) -> bool {
        // Step 1: your own writes (see `visible` for the rationale). A
        // row this tx deleted is invisible; a row it inserted and has
        // not deleted is visible.
        if self.tx_id != 0 {
            if h.xmax == self.tx_id {
                return false;
            }
            if h.xmin == self.tx_id {
                return true;
            }
        }
        // Frozen fast path: frozen + alive is visible to everyone and
        // never consults the oracle (a frozen xmin is committed-and-old
        // by definition).
        if h.flags & HEAP_XMIN_FROZEN != 0 && h.xmax == XMAX_ALIVE {
            return true;
        }
        // Step 2: future.
        if h.xmin > self.version {
            return false;
        }
        // Step 3: in-flight at snapshot time.
        if self.in_progress.contains(h.xmin) {
            return false;
        }
        // Step 4 (NEW): xmin aborted → the insert never committed.
        if xact.status(h.xmin) == XactStatus::Aborted {
            return false;
        }
        // Step 5: still alive.
        if h.xmax == XMAX_ALIVE {
            return true;
        }
        // Step 6: deletion is future or in-flight → still visible.
        if h.xmax > self.version || self.in_progress.contains(h.xmax) {
            return true;
        }
        // Step 7 (NEW): xmax aborted → the delete never took effect.
        if xact.status(h.xmax) == XactStatus::Aborted {
            return true;
        }
        // Step 8: deletion committed before our snapshot.
        false
    }
}

impl Default for Snapshot {
    fn default() -> Self {
        Self::unbounded()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row_header::RowHeader;

    fn ips(xs: &[u64]) -> InProgressSet {
        InProgressSet::from_sorted(xs.to_vec())
    }

    #[test]
    fn unbounded_snapshot_sees_everything() {
        let s = Snapshot::unbounded();
        let frozen = RowHeader::frozen();
        let alive = RowHeader::alive(7);
        assert!(s.visible(&frozen));
        assert!(s.visible(&alive));
    }

    #[test]
    fn snapshot_hides_future_writes() {
        let s = Snapshot::new(100, ips(&[]), 100, 0);
        let row = RowHeader::alive(150); // written after snapshot
        assert!(!s.visible(&row));
    }

    #[test]
    fn snapshot_hides_in_progress_writes() {
        let s = Snapshot::new(200, ips(&[50, 60, 70]), 50, 0);
        let row = RowHeader::alive(60); // tx 60 still in flight
        assert!(!s.visible(&row));
        let row2 = RowHeader::alive(55); // tx 55 not in in_progress => committed
        assert!(s.visible(&row2));
    }

    #[test]
    fn snapshot_hides_committed_deletions() {
        let s = Snapshot::new(200, ips(&[]), 100, 0);
        let row = RowHeader {
            xmin: 50,
            xmax: 100, // deleted before our snapshot
            flags: 0,
        };
        assert!(!s.visible(&row));
    }

    #[test]
    fn snapshot_keeps_pending_deletions_visible() {
        let s = Snapshot::new(200, ips(&[150]), 100, 0);
        let row = RowHeader {
            xmin: 50,
            xmax: 150, // delete by an in-flight tx
            flags: 0,
        };
        assert!(s.visible(&row));
    }

    #[test]
    fn reader_sees_its_own_insert_but_not_its_own_delete() {
        let s = Snapshot::new(100, ips(&[]), 100, 42);
        // A row I inserted and have not deleted: visible.
        let own_insert = RowHeader::alive(42);
        assert!(s.visible(&own_insert));
        // A row I inserted AND deleted (BEGIN; INSERT; DELETE; SELECT):
        // invisible — you don't see what you deleted (PG semantics).
        let own_insert_then_delete = RowHeader {
            xmin: 42,
            xmax: 42,
            flags: 0,
        };
        assert!(!s.visible(&own_insert_then_delete));
        // A committed row I deleted (xmin other, xmax me): invisible.
        let other_insert_i_deleted = RowHeader {
            xmin: 7,
            xmax: 42,
            flags: 0,
        };
        assert!(!s.visible(&other_insert_i_deleted));
    }

    #[test]
    fn snapshot_hides_future_deletion_done_by_in_flight_tx() {
        // Edge: tx 150 in-flight AND xmin = 30 (not in in_progress)
        // → row is alive to us even though xmax is set.
        let s = Snapshot::new(200, ips(&[150]), 30, 0);
        let row = RowHeader {
            xmin: 30,
            xmax: 150,
            flags: 0,
        };
        assert!(s.visible(&row));
    }

    /// Test oracle: every version in the set is Aborted, all others
    /// Committed. Mirrors what the engine's real registry reports for
    /// a version whose tx rolled back.
    struct AbortedSet(alloc::vec::Vec<u64>);
    impl XactStatusOracle for AbortedSet {
        fn status(&self, v: u64) -> XactStatus {
            if self.0.contains(&v) {
                XactStatus::Aborted
            } else {
                XactStatus::Committed
            }
        }
    }

    #[test]
    fn all_committed_oracle_matches_plain_visible() {
        // With every version committed, visible_with_status must
        // agree with visible on a spread of header shapes.
        let s = Snapshot::new(200, ips(&[150]), 50, 42);
        let headers = [
            RowHeader::frozen(),
            RowHeader::alive(60),
            RowHeader::alive(250),
            RowHeader {
                xmin: 50,
                xmax: 100,
                flags: 0,
            },
            RowHeader {
                xmin: 50,
                xmax: 150,
                flags: 0,
            },
            RowHeader {
                xmin: 42,
                xmax: XMAX_ALIVE,
                flags: 0,
            },
        ];
        for h in &headers {
            assert_eq!(
                s.visible(h),
                s.visible_with_status(h, &AllCommitted),
                "mismatch on {h:?}"
            );
        }
    }

    #[test]
    fn aborted_xmin_hides_the_row() {
        // Row inserted by version 60, which then aborted. It is NOT in
        // in_progress (it reached a terminal state), so plain visible
        // would wrongly show it; the oracle hides it.
        let s = Snapshot::new(200, ips(&[]), 50, 0);
        let row = RowHeader::alive(60);
        assert!(s.visible(&row), "two-state rule shows the orphan");
        assert!(
            !s.visible_with_status(&row, &AbortedSet(alloc::vec![60])),
            "abort oracle hides the never-committed insert"
        );
    }

    #[test]
    fn aborted_xmax_revives_the_row() {
        // Row inserted by (committed) 50, deleted by 90 which aborted.
        // The delete never took effect → the row is still visible.
        let s = Snapshot::new(200, ips(&[]), 50, 0);
        let row = RowHeader {
            xmin: 50,
            xmax: 90,
            flags: 0,
        };
        assert!(
            !s.visible(&row),
            "two-state rule treats delete as committed"
        );
        assert!(
            s.visible_with_status(&row, &AbortedSet(alloc::vec![90])),
            "abort oracle keeps the row whose delete was rolled back"
        );
    }

    #[test]
    fn frozen_row_skips_the_oracle() {
        // A frozen+alive row is visible without ever consulting the
        // oracle — even a (nonsensical) oracle that would abort xmin=1.
        struct Panicking;
        impl XactStatusOracle for Panicking {
            fn status(&self, _v: u64) -> XactStatus {
                panic!("oracle must not be consulted for a frozen+alive row");
            }
        }
        let s = Snapshot::new(200, ips(&[]), 50, 0);
        assert!(s.visible_with_status(&RowHeader::frozen(), &Panicking));
    }

    #[test]
    fn in_progress_set_binary_search_correctness() {
        let s = ips(&[10, 20, 30, 40, 50]);
        assert!(s.contains(10));
        assert!(s.contains(30));
        assert!(s.contains(50));
        assert!(!s.contains(0));
        assert!(!s.contains(25));
        assert!(!s.contains(60));
    }
}
