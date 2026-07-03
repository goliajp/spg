//! v7.37.15 (Phase C.4) — row-level lock table, the four PG tuple-lock
//! modes, and wait-for deadlock detection.
//!
//! ## Why this exists
//!
//! Phase C's in-place write path (C.3) lets concurrent transactions
//! update / delete different rows without blocking. But two writers
//! that touch the SAME row must serialise, and a `SELECT ... FOR
//! UPDATE` must be able to reserve rows ahead of the write. This
//! module is the lock table that arbitrates: it keys locks on the
//! stable `(RelId, RowId)` identity (Phase C.1) so a held lock keeps
//! naming the same row across concurrent compaction.
//!
//! ## Additive at this commit
//!
//! Pure `no_std` logic with no consumer yet: the write path (C.3/C.4)
//! calls `acquire` / `release_all`, and the host (`spg-server`,
//! thread-per-connection) parks a waiting thread behind a `Parker`
//! informed by [`LockOutcome::WouldBlock`]. The engine core only
//! records the wait-for edges and runs the cycle detector — keeping it
//! `no_std` (no thread primitives leak into the core). Under today's
//! single external engine lock the table is mutated serially; the
//! sharded lock-free version is Phase C.5.
//!
//! ## The four modes and their conflicts
//!
//! Mirrors PG's tuple-lock strengths (weakest → strongest):
//! `KeyShare < Share < NoKeyUpdate < Exclusive`. The load-bearing
//! compatibility is `KeyShare ∥ NoKeyUpdate`: an FK existence check
//! (`FOR KEY SHARE`) runs concurrently with a non-key `UPDATE`
//! (`NoKeyUpdate`) on the same parent row — a real concurrency win we
//! match, not a coincidence.

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use spg_storage::row_header::{RelId, RowId};

/// A PG tuple-lock strength. `FOR KEY SHARE` / `FOR SHARE` / `FOR NO
/// KEY UPDATE` / `FOR UPDATE`, plus the implicit modes a write takes:
/// a key-touching UPDATE or any DELETE takes `Exclusive`; a non-key
/// UPDATE takes `NoKeyUpdate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LockMode {
    KeyShare,
    Share,
    NoKeyUpdate,
    Exclusive,
}

impl LockMode {
    /// PG tuple-lock conflict matrix. `held.conflicts_with(requested)`
    /// is true iff a currently-held lock in mode `self` blocks a new
    /// request in mode `requested`.
    ///
    /// ```text
    ///   held \ req  KeyShare  Share  NoKeyUpd  Excl
    ///   KeyShare       ok      ok      ok        X
    ///   Share          ok      ok      X         X
    ///   NoKeyUpdate    ok      X       X         X
    ///   Exclusive      X       X       X         X
    /// ```
    #[must_use]
    pub fn conflicts_with(self, requested: LockMode) -> bool {
        use LockMode::{Exclusive, KeyShare, NoKeyUpdate, Share};
        match self {
            KeyShare => matches!(requested, Exclusive),
            Share => matches!(requested, NoKeyUpdate | Exclusive),
            NoKeyUpdate => !matches!(requested, KeyShare),
            Exclusive => true,
        }
    }
}

/// What a caller wants to happen when the lock it requests is not
/// immediately available. Mirrors PG's `LockWaitPolicy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitPolicy {
    /// Block until the lock is granted (the default DML behaviour and
    /// bare `FOR UPDATE`).
    Wait,
    /// `FOR UPDATE NOWAIT` — fail immediately rather than block.
    NoWait,
    /// `FOR UPDATE SKIP LOCKED` — skip this row rather than block.
    SkipLocked,
}

/// The result of an [`LockTable::acquire`] attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockOutcome {
    /// The lock is held by the requesting version.
    Granted,
    /// The request conflicts and the policy is `Wait`; the caller
    /// should park until one of `on` releases. The wait-for edges are
    /// already recorded, and no cycle was found.
    WouldBlock { on: Vec<u64> },
    /// `SkipLocked` policy and the row is locked — skip it.
    Skip,
    /// `NoWait` policy and the row is locked — fail the statement.
    NotAvailable,
    /// Granting the wait would close a wait-for cycle; the caller must
    /// abort transaction `victim` (the youngest in the cycle) with a
    /// deadlock error rather than park.
    Deadlock { victim: u64 },
}

#[derive(Debug, Default, Clone)]
struct LockEntry {
    /// `(version, mode)` pairs currently holding this row.
    holders: Vec<(u64, LockMode)>,
    /// Versions parked waiting for this row (in FIFO order).
    waiters: Vec<u64>,
}

/// The row-lock table. Keyed on stable `(RelId, RowId)`; carries the
/// wait-for graph used for deadlock detection.
///
/// `Clone` to match the engine's other transient concurrency state
/// (`active_writer_versions`) that rides on `Engine`; the shared
/// (non-cloned) lock manager is Phase C.5, when the single engine lock
/// is split.
#[derive(Debug, Default, Clone)]
pub struct LockTable {
    entries: BTreeMap<(RelId, RowId), LockEntry>,
    /// `waiter → versions it is blocked on`. Rebuilt as waits are
    /// added / released; the deadlock detector walks it.
    wait_for: BTreeMap<u64, BTreeSet<u64>>,
}

impl LockTable {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Try to acquire `mode` on `(rel, row)` for transaction
    /// `version`. Re-locking a row this version already holds upgrades
    /// in place (idempotent for equal-or-weaker modes).
    pub fn acquire(
        &mut self,
        rel: RelId,
        row: RowId,
        mode: LockMode,
        version: u64,
        policy: WaitPolicy,
    ) -> LockOutcome {
        let entry = self.entries.entry((rel, row)).or_default();

        // Collect the distinct conflicting holders (never conflict with
        // your own held lock — self-conflict would deadlock trivially).
        let mut blockers: Vec<u64> = Vec::new();
        for &(hv, hmode) in &entry.holders {
            if hv != version && hmode.conflicts_with(mode) && !blockers.contains(&hv) {
                blockers.push(hv);
            }
        }

        if blockers.is_empty() {
            // Grant: record the holder if not already present.
            if !entry.holders.iter().any(|&(hv, hm)| hv == version && hm == mode) {
                entry.holders.push((version, mode));
            }
            // A previously-parked waiter that now gets in drops its
            // wait edges.
            entry.waiters.retain(|&w| w != version);
            self.wait_for.remove(&version);
            return LockOutcome::Granted;
        }

        match policy {
            WaitPolicy::NoWait => LockOutcome::NotAvailable,
            WaitPolicy::SkipLocked => LockOutcome::Skip,
            WaitPolicy::Wait => {
                if !entry.waiters.contains(&version) {
                    entry.waiters.push(version);
                }
                let edges = self.wait_for.entry(version).or_default();
                for &b in &blockers {
                    edges.insert(b);
                }
                // Deadlock check: does following wait-for edges from
                // `version` return to `version`?
                if let Some(cycle) = self.find_cycle(version) {
                    // Abort the youngest (highest version) in the cycle.
                    let victim = cycle.into_iter().max().unwrap_or(version);
                    return LockOutcome::Deadlock { victim };
                }
                LockOutcome::WouldBlock { on: blockers }
            }
        }
    }

    /// Release every lock + wait held by `version` (transaction end:
    /// commit or abort). Removes it from all entries and the wait-for
    /// graph, and drops now-empty entries.
    pub fn release_all(&mut self, version: u64) {
        self.entries.retain(|_, e| {
            e.holders.retain(|&(hv, _)| hv != version);
            e.waiters.retain(|&w| w != version);
            !(e.holders.is_empty() && e.waiters.is_empty())
        });
        self.wait_for.remove(&version);
        for edges in self.wait_for.values_mut() {
            edges.remove(&version);
        }
    }

    /// Number of rows with at least one holder or waiter. For
    /// `pg_locks` enumeration (Phase C.4) and tests.
    #[must_use]
    pub fn locked_row_count(&self) -> usize {
        self.entries.len()
    }

    /// DFS over `wait_for` from `start`; returns the set of versions on
    /// a cycle through `start`, or `None` if the wait graph is acyclic
    /// from here. Bounded by the number of active waiters.
    fn find_cycle(&self, start: u64) -> Option<BTreeSet<u64>> {
        let mut stack: Vec<u64> = Vec::new();
        let mut on_path: BTreeSet<u64> = BTreeSet::new();
        let mut visited: BTreeSet<u64> = BTreeSet::new();
        stack.push(start);
        // Iterative DFS tracking the current path so we can detect a
        // return to `start`.
        self.dfs_cycle(start, start, &mut on_path, &mut visited, &mut stack)
    }

    fn dfs_cycle(
        &self,
        start: u64,
        node: u64,
        on_path: &mut BTreeSet<u64>,
        visited: &mut BTreeSet<u64>,
        path: &mut Vec<u64>,
    ) -> Option<BTreeSet<u64>> {
        on_path.insert(node);
        visited.insert(node);
        if let Some(edges) = self.wait_for.get(&node) {
            for &next in edges {
                if next == start {
                    // Closed a cycle back to the origin.
                    let mut cyc: BTreeSet<u64> = on_path.iter().copied().collect();
                    cyc.insert(start);
                    return Some(cyc);
                }
                if !on_path.contains(&next) {
                    path.push(next);
                    if let Some(c) = self.dfs_cycle(start, next, on_path, visited, path) {
                        return Some(c);
                    }
                    path.pop();
                }
            }
        }
        on_path.remove(&node);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: RelId = RelId(1);
    fn row(n: u64) -> RowId {
        RowId(n)
    }

    #[test]
    fn conflict_matrix_matches_pg() {
        use LockMode::{Exclusive, KeyShare, NoKeyUpdate, Share};
        // Row = held, Col = requested. true = conflict (blocks).
        assert!(!KeyShare.conflicts_with(KeyShare));
        assert!(!KeyShare.conflicts_with(Share));
        assert!(!KeyShare.conflicts_with(NoKeyUpdate));
        assert!(KeyShare.conflicts_with(Exclusive));

        assert!(!Share.conflicts_with(KeyShare));
        assert!(!Share.conflicts_with(Share));
        assert!(Share.conflicts_with(NoKeyUpdate));
        assert!(Share.conflicts_with(Exclusive));

        assert!(!NoKeyUpdate.conflicts_with(KeyShare)); // load-bearing
        assert!(NoKeyUpdate.conflicts_with(Share));
        assert!(NoKeyUpdate.conflicts_with(NoKeyUpdate));
        assert!(NoKeyUpdate.conflicts_with(Exclusive));

        assert!(Exclusive.conflicts_with(KeyShare));
        assert!(Exclusive.conflicts_with(Share));
        assert!(Exclusive.conflicts_with(NoKeyUpdate));
        assert!(Exclusive.conflicts_with(Exclusive));
    }

    #[test]
    fn compatible_locks_both_granted() {
        let mut t = LockTable::new();
        // FK check (KeyShare) + non-key UPDATE (NoKeyUpdate) on the same
        // row both succeed — the concurrency win we match.
        assert_eq!(
            t.acquire(R, row(1), LockMode::KeyShare, 10, WaitPolicy::Wait),
            LockOutcome::Granted
        );
        assert_eq!(
            t.acquire(R, row(1), LockMode::NoKeyUpdate, 20, WaitPolicy::Wait),
            LockOutcome::Granted
        );
    }

    #[test]
    fn exclusive_blocks_and_nowait_skiplocked_report() {
        let mut t = LockTable::new();
        assert_eq!(
            t.acquire(R, row(1), LockMode::Exclusive, 10, WaitPolicy::Wait),
            LockOutcome::Granted
        );
        // A conflicting Wait parks.
        match t.acquire(R, row(1), LockMode::Exclusive, 20, WaitPolicy::Wait) {
            LockOutcome::WouldBlock { on } => assert_eq!(on, alloc::vec![10]),
            other => panic!("expected WouldBlock, got {other:?}"),
        }
        // NoWait / SkipLocked report immediately instead.
        assert_eq!(
            t.acquire(R, row(1), LockMode::Exclusive, 30, WaitPolicy::NoWait),
            LockOutcome::NotAvailable
        );
        assert_eq!(
            t.acquire(R, row(1), LockMode::Exclusive, 40, WaitPolicy::SkipLocked),
            LockOutcome::Skip
        );
    }

    #[test]
    fn release_lets_a_waiter_in() {
        let mut t = LockTable::new();
        t.acquire(R, row(1), LockMode::Exclusive, 10, WaitPolicy::Wait);
        t.acquire(R, row(1), LockMode::Exclusive, 20, WaitPolicy::Wait);
        t.release_all(10);
        assert_eq!(
            t.acquire(R, row(1), LockMode::Exclusive, 20, WaitPolicy::Wait),
            LockOutcome::Granted
        );
        assert_eq!(t.locked_row_count(), 1);
        t.release_all(20);
        assert_eq!(t.locked_row_count(), 0);
    }

    #[test]
    fn deadlock_cycle_aborts_youngest() {
        let mut t = LockTable::new();
        // tx10 holds row1, tx20 holds row2.
        t.acquire(R, row(1), LockMode::Exclusive, 10, WaitPolicy::Wait);
        t.acquire(R, row(2), LockMode::Exclusive, 20, WaitPolicy::Wait);
        // tx10 waits for row2 (held by 20): edge 10 -> 20.
        assert!(matches!(
            t.acquire(R, row(2), LockMode::Exclusive, 10, WaitPolicy::Wait),
            LockOutcome::WouldBlock { .. }
        ));
        // tx20 waits for row1 (held by 10): edge 20 -> 10 closes the
        // cycle → abort the youngest (20).
        assert_eq!(
            t.acquire(R, row(1), LockMode::Exclusive, 20, WaitPolicy::Wait),
            LockOutcome::Deadlock { victim: 20 }
        );
    }

    #[test]
    fn relock_same_version_is_idempotent() {
        let mut t = LockTable::new();
        assert_eq!(
            t.acquire(R, row(1), LockMode::Exclusive, 10, WaitPolicy::Wait),
            LockOutcome::Granted
        );
        // Same version re-locking the same row never blocks on itself.
        assert_eq!(
            t.acquire(R, row(1), LockMode::Exclusive, 10, WaitPolicy::Wait),
            LockOutcome::Granted
        );
    }
}
