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

use crate::row_header::{RowHeader, XMAX_ALIVE};

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
        // Step 1: see your own writes.
        if self.tx_id != 0 && h.xmin == self.tx_id {
            return h.xmax == XMAX_ALIVE || h.xmax == self.tx_id;
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
    fn reader_sees_its_own_writes() {
        let s = Snapshot::new(100, ips(&[]), 100, 42);
        let own_write = RowHeader::alive(42);
        assert!(s.visible(&own_write));
        let own_delete = RowHeader {
            xmin: 42,
            xmax: 42,
            flags: 0,
        };
        assert!(s.visible(&own_delete));
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
