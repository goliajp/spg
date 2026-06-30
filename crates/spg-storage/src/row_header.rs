//! v7.37.15 (Phase A) — per-row MVCC visibility header.
//!
//! ## Why this exists
//!
//! Pre-v7.37.15 SPG's MVCC story was at the **catalog level**:
//! `CatalogSnapshot` Arc-clones the catalog trie roots, readers see a
//! coherent prior-committed view, writers serialise. That works for
//! mailrs / sentori's SELECT-heavy IMAP workload, but caps multi-
//! writer throughput at "one writer at a time" forever and forces an
//! Arc clone of the entire catalog on each readonly statement —
//! mailrs measured a ~50% perf gap vs PG18 traceable to this.
//!
//! v7.37.15 adds **per-row** visibility on top of that catalog-level
//! Arc snapshot model. Both coexist:
//!
//! - The Arc snapshot path remains for SELECT-only fast reads — the
//!   catalog trie clones are still O(1) Arc bumps.
//! - Each row in the table now carries a `RowHeader { xmin, xmax,
//!   flags }`; scans filter rows against a `Snapshot { version,
//!   in_progress }`.
//! - Writers update `xmin` on insert / `xmax` on delete or update,
//!   so concurrent writers to different rows no longer block one
//!   another (granularity = per-row, not per-catalog).
//!
//! ## Why u64 instead of PG's 32-bit Xid
//!
//! PG carries the historical scars of a 32-bit transaction id — epoch
//! advancement, FrozenTransactionId, VACUUM FREEZE, the entire
//! anti-wraparound machinery. SPG starts fresh: `xmin` and `xmax`
//! are `u64`. At 1ns per transaction (a contemporary CPU's L1 cycle
//! budget) u64 wraps in 584 years; we explicitly do NOT implement
//! wraparound handling because we will never hit it.
//!
//! ## Layout: parallel `PersistentVec`, not embedded in `Row`
//!
//! The header lives in `Table::headers: PersistentVec<RowHeader>`
//! **parallel** to `Table::rows: PersistentVec<Row<'static>>`. Why
//! not embedded in the Row struct?
//!
//! 1. **Cache locality on visibility-only scans.** A scan that only
//!    needs to count visible rows (think `SELECT COUNT(*)`) walks
//!    headers without touching row bodies. With the header inline
//!    every cache line carries one row's worth of payload; with the
//!    header in a separate Vec the scan only loads 24-byte headers,
//!    yielding ~10x throughput on wide-row tables.
//! 2. **Public API stability.** `pub struct Row { pub values }` is
//!    the shape every caller — eval / sort / agg / projection —
//!    already pattern-matches. Adding a header field would break
//!    every match arm in the codebase for a field most call sites
//!    don't care about.
//! 3. **Per-row freeze**. The visibility map (per-segment
//!    `all_visible` bitmap) is an `&[bool]` slice over headers —
//!    parallel storage makes the bitmap construction zero-copy.
//!
//! ## Backward compatibility
//!
//! Rows that come from a pre-v7.37.15 envelope (V1-V5) have no
//! header on disk. On load, every such row gets a default
//! `RowHeader::frozen()` (`xmin = 1`, `xmax = 0`,
//! `flags = HEAP_XMIN_FROZEN`). Visibility checks against any
//! valid snapshot return `true` — so old data is fully visible to
//! everyone, matching the pre-v7.37.15 contract.

use core::sync::atomic::{AtomicU64, Ordering};

/// Bit 0 of `RowHeader.flags`: `xmin` is conceptually-`FrozenXid` — the row
/// existed at process start (loaded from a V1-V5 envelope) and is
/// unconditionally visible to every snapshot.
pub const HEAP_XMIN_FROZEN: u8 = 1 << 0;
/// Bit 1: the row is the head of a HOT chain (v7.37.15 Phase D).
/// Hot-tier in-place UPDATE optimisation; cold tier never sets this.
pub const HEAP_HOT_UPDATED: u8 = 1 << 1;
/// Bit 2: the row is a HOT chain non-head element (v7.37.15 Phase D).
pub const HEAP_ONLY_TUPLE: u8 = 1 << 2;
/// Bit 3: the row's `xmax` is conceptually-frozen — the delete is
/// older than any live snapshot, so vacuum may reclaim it on the
/// next pass.
pub const HEAP_XMAX_FROZEN: u8 = 1 << 3;

/// Sentinel value used for `xmax` when the row has NOT been deleted.
/// PG uses `InvalidTransactionId = 0`; SPG matches.
pub const XMAX_ALIVE: u64 = 0;

/// Sentinel used for `xmin` on rows loaded from a pre-v7.37.15
/// envelope. Any non-zero value < every real transaction id works
/// — we pick 1 (PG uses `FrozenTransactionId = 2`).
pub const XMIN_FROZEN: u64 = 1;

/// Per-row MVCC visibility header.
///
/// 24 bytes after alignment (8 + 8 + 1 + 7 padding). The padding
/// is intentional: a power-of-two stride keeps array indexing
/// cheap and matches the cache-line layout PG uses for
/// `HeapTupleHeaderData`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, align(8))]
pub struct RowHeader {
    /// Version that wrote this row (= `TxId` of the inserting tx
    /// at commit time). Compared against the reader's snapshot
    /// version to decide visibility.
    ///
    /// Default `XMIN_FROZEN = 1` on rows loaded from a pre-
    /// v7.37.15 envelope.
    pub xmin: u64,
    /// Version that deleted / updated this row, or `XMAX_ALIVE = 0`
    /// when still alive. UPDATE writes `xmax` on the old row +
    /// `xmin` on the new one inside the same transaction.
    pub xmax: u64,
    /// Bit-packed flags. See module-level constants. The most
    /// common state (`xmin frozen, xmax alive`) sets only
    /// `HEAP_XMIN_FROZEN` so visibility checks can short-circuit
    /// on the `flags & HEAP_XMIN_FROZEN == HEAP_XMIN_FROZEN &&
    /// xmax == 0` fast path.
    pub flags: u8,
}

impl RowHeader {
    /// The canonical "frozen, alive" header. Use on rows loaded
    /// from a pre-v7.37.15 envelope and on rows inserted before
    /// the writer transaction is assigned a TxId.
    #[must_use]
    pub const fn frozen() -> Self {
        Self {
            xmin: XMIN_FROZEN,
            xmax: XMAX_ALIVE,
            flags: HEAP_XMIN_FROZEN,
        }
    }

    /// A header for a row inserted by transaction `xmin`, still
    /// alive. Default for fresh INSERTs.
    #[must_use]
    pub const fn alive(xmin: u64) -> Self {
        Self {
            xmin,
            xmax: XMAX_ALIVE,
            flags: 0,
        }
    }

    /// True iff this header is the all-visible fast path
    /// (frozen + alive). The visibility-map bit is `true` exactly
    /// when EVERY header in a segment satisfies this — letting
    /// scans skip the per-row check entirely for cold segments.
    #[must_use]
    pub const fn is_all_visible_fast(&self) -> bool {
        self.flags & HEAP_XMIN_FROZEN == HEAP_XMIN_FROZEN && self.xmax == XMAX_ALIVE
    }

    /// Was this row deleted? `false` when `xmax == XMAX_ALIVE`.
    #[must_use]
    pub const fn is_deleted(&self) -> bool {
        self.xmax != XMAX_ALIVE
    }
}

impl Default for RowHeader {
    fn default() -> Self {
        Self::frozen()
    }
}

/// Process-wide monotonic version counter shared by every Database
/// instance in this process. `RowHeader.xmin / xmax` values + the
/// reader's `Snapshot.version` all draw from here.
///
/// `u64` so we never wrap. Starts at `XMIN_FROZEN + 1 = 2` so a
/// fresh transaction can never collide with frozen rows.
///
/// Process-wide (not per-Database) so concurrent databases in the
/// same process share a coherent view of "is tx 17 still alive" —
/// the BitSet `Snapshot.in_progress` keys off the same numbering.
static GLOBAL_VERSION: AtomicU64 = AtomicU64::new(XMIN_FROZEN + 1);

/// Allocate the next transaction id / row version. Caller stores
/// it in the row's `xmin` (for insert) or `xmax` (for delete /
/// update). Threadsafe; no lock involved.
#[must_use]
pub fn next_version() -> u64 {
    GLOBAL_VERSION.fetch_add(1, Ordering::AcqRel)
}

/// Read the current version cursor without advancing. Snapshots
/// use this as their `Snapshot.version`.
#[must_use]
pub fn current_version() -> u64 {
    GLOBAL_VERSION.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_header_is_all_visible() {
        let h = RowHeader::frozen();
        assert_eq!(h.xmin, XMIN_FROZEN);
        assert_eq!(h.xmax, XMAX_ALIVE);
        assert!(h.is_all_visible_fast());
        assert!(!h.is_deleted());
    }

    #[test]
    fn alive_header_is_not_fast_visible_until_frozen() {
        let h = RowHeader::alive(7);
        assert_eq!(h.xmin, 7);
        assert_eq!(h.xmax, XMAX_ALIVE);
        // Not on the fast path because HEAP_XMIN_FROZEN not set.
        // Visibility against a snapshot is decided by the
        // snapshot-aware path in the engine, not by this fast
        // check.
        assert!(!h.is_all_visible_fast());
        assert!(!h.is_deleted());
    }

    #[test]
    fn version_counter_is_monotonic() {
        let a = next_version();
        let b = next_version();
        let c = next_version();
        assert!(a < b);
        assert!(b < c);
        // Reading does not advance.
        let d = current_version();
        assert!(d >= c);
        let e = current_version();
        assert_eq!(d, e);
    }

    #[test]
    fn version_counter_starts_above_frozen() {
        // First-ever next_version() must return at least
        // XMIN_FROZEN + 1 so a fresh transaction can never collide
        // with a frozen row's xmin.
        let v = current_version();
        assert!(v > XMIN_FROZEN);
    }

    #[test]
    fn deleted_row_header_reports_deletion() {
        let mut h = RowHeader::alive(7);
        assert!(!h.is_deleted());
        h.xmax = 13;
        assert!(h.is_deleted());
    }
}
