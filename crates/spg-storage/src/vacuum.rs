//! v7.37.15 (Phase D) — vacuum primitives.
//!
//! ## What this does
//!
//! Once Phase C wires DELETE / UPDATE through `mark_row_deleted`,
//! tombstoned rows accumulate in the table physically: the `xmax`
//! field records "this row was deleted by tx V", but the row's
//! storage stays so concurrent readers holding older snapshots
//! still see it.
//!
//! Vacuum reclaims those rows once **every** live snapshot can no
//! longer see them: a row is safely reclaimable when
//!     row.xmax != XMAX_ALIVE  &&  row.xmax < oldest_active_snapshot
//! — the version that deleted it is older than the oldest reader's
//! snapshot, so no reader can still observe the row.
//!
//! ## Layered primitives
//!
//! This module ships:
//!
//! - [`VacuumReport`] — per-pass result (dry-run friendly: counts
//!   reclaimable rows without mutating).
//! - [`Table::reclaimable_rows`] / [`Table::vacuum`] — the
//!   single-table reclaim pass.
//! - [`Catalog::vacuum_all`] — fleet-wide pass over every user
//!   table.
//!
//! Vacuum is intentionally NOT a background thread at this layer.
//! Hosts (spg-embedded / spg-server) wire their own thread that
//! periodically calls `Catalog::vacuum_all` under the engine write
//! lock; the storage primitives just describe what to do per pass.
//! That keeps the storage crate `#![no_std]` (vacuum-as-thread
//! needs `std::thread`) and lets hosts pick their cadence /
//! cost-yield policy.

use crate::row_header::XMAX_ALIVE;

/// Per-vacuum-pass report. Used by hosts to emit metrics + drive
/// adaptive cadence (more dead rows ⇒ run more often).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VacuumReport {
    /// Number of rows physically reclaimed in this pass. `0` is the
    /// expected steady state once vacuum keeps up with the delete
    /// rate.
    pub rows_reclaimed: u64,
    /// Number of rows examined (== `headers.len()` at pass entry).
    /// Hosts compare this against `rows_reclaimed` to know the
    /// dead-fraction ratio per table.
    pub rows_examined: u64,
    /// Per-table breakdown for fleet passes. Empty for single-table
    /// passes.
    pub per_table: alloc::vec::Vec<(alloc::string::String, u64)>,
}

impl VacuumReport {
    /// Combine two reports. Used by the fleet pass to fold per-table
    /// reports into the rollup.
    pub fn merge(&mut self, other: VacuumReport) {
        self.rows_reclaimed += other.rows_reclaimed;
        self.rows_examined += other.rows_examined;
        self.per_table.extend(other.per_table);
    }
}

/// True iff the row at `(xmax, oldest_active)` is reclaimable. A
/// row is safely reclaimable when its delete is older than the
/// oldest snapshot still in flight (== no reader can still see
/// it).
///
/// `XMAX_ALIVE` (= 0) means "row not deleted" → always false.
#[inline]
#[must_use]
pub fn is_reclaimable(xmax: u64, oldest_active: u64) -> bool {
    xmax != XMAX_ALIVE && xmax < oldest_active
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row_header::XMAX_ALIVE;

    #[test]
    fn alive_row_is_not_reclaimable() {
        assert!(!is_reclaimable(XMAX_ALIVE, 100));
    }

    #[test]
    fn deleted_row_is_reclaimable_when_xmax_older_than_oldest_active() {
        assert!(is_reclaimable(10, 100));
    }

    #[test]
    fn deleted_row_is_not_reclaimable_while_snapshot_could_still_see_it() {
        // oldest_active = 5 means there's still a reader whose
        // snapshot was taken at version 5 (or earlier). That reader
        // could still observe the row even though it was deleted
        // at version 100. Don't reclaim.
        assert!(!is_reclaimable(100, 5));
    }

    #[test]
    fn report_merge_aggregates_counters() {
        let mut a = VacuumReport {
            rows_reclaimed: 5,
            rows_examined: 100,
            per_table: alloc::vec![("t".into(), 5)],
        };
        let b = VacuumReport {
            rows_reclaimed: 3,
            rows_examined: 40,
            per_table: alloc::vec![("u".into(), 3)],
        };
        a.merge(b);
        assert_eq!(a.rows_reclaimed, 8);
        assert_eq!(a.rows_examined, 140);
        assert_eq!(a.per_table.len(), 2);
    }
}
