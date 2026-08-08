//! v7.39 (round 786, T35 Phase A) — host-provided temporary storage for
//! query-time spilling.
//!
//! The engine is `no_std`: it cannot open a file. Every other host
//! capability it needs — the wall clock, backend signalling, timezone
//! lookups — arrives as an injected `fn` pointer, and spill storage
//! follows the same shape.
//!
//! Why it exists: a large `ORDER BY` is materialised whole today. With
//! `SPG_MAX_QUERY_BYTES` set the query DIES at the ceiling
//! (`QueryBytesExceeded`); with the budget off, resident memory grows
//! with the input — round 785 measured a 60 MB sort taking 240 MB of
//! RSS, while PG18 answered the same query under a 4 MB `work_mem` with
//! `Sort Method: external merge  Disk: 61072kB`. Both of SPG's outcomes
//! break its own rules (never-die; resident memory must not grow
//! linearly), and the capability gap is not speed — it is whether the
//! query can finish at all.
//!
//! This module is the seam only. Run generation and the k-way merge
//! land in Phase B/C; with no factory injected the engine behaves
//! exactly as it does today, byte for byte.

use alloc::boxed::Box;
use alloc::string::String;

/// Why a spill operation could not proceed. The host maps its own I/O
/// errors into `Io`; the engine only ever reports them upward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TempStoreError {
    /// The host's storage refused the operation (disk full, permissions,
    /// the temp directory vanished mid-query).
    Io(String),
}

impl core::fmt::Display for TempStoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(m) => write!(f, "temporary storage: {m}"),
        }
    }
}

/// One spill run: written once in sorted order, then read back once in
/// that same order by a merge cursor.
///
/// The two phases are deliberate — a run is never appended to after it
/// is sealed, and never seeks. That is all an external merge needs, and
/// keeping the contract that narrow means a host implementation is a
/// file handle and nothing else.
///
/// Dropping a run MUST remove its backing storage: a cancelled or
/// panicking query has no other chance to clean up.
pub trait TempRun: Send {
    /// Append to the write phase.
    fn append(&mut self, bytes: &[u8]) -> Result<(), TempStoreError>;

    /// End the write phase and rewind for reading.
    fn seal(&mut self) -> Result<(), TempStoreError>;

    /// Fill `buf` from the current read position. `Ok(0)` is EOF.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, TempStoreError>;

    /// Bytes appended so far. Round 884 wired this to PG's
    /// `pg_stat_database.temp_bytes`, read after `seal` so the figure is
    /// what the run really holds; the comment here used to say the
    /// column was hard-coded 0, and it was.
    fn bytes_written(&self) -> u64;
}

/// Host factory: hand back a fresh, empty run.
///
/// `None` on the engine (embedded with no temp dir, or a host that has
/// not opted in) means spilling is unavailable and the ceiling behaves
/// as it does today.
pub type TempRunFactory = fn() -> Result<Box<dyn TempRun>, TempStoreError>;

/// v7.37 (round 884) — what a spill actually cost, for
/// `pg_stat_database.temp_files` / `temp_bytes` and for EXPLAIN
/// ANALYZE's `Sort Method`.
///
/// PG counts these per backend and rolls them into the per-database
/// view; a monitoring query watches `temp_bytes` to find the queries
/// that outgrow `work_mem`. SPG reported 0 for both while it was
/// spilling 26 runs a query, and EXPLAIN said `quicksort` for a sort
/// that had gone to disk — both because nothing was counting.
#[derive(Debug, Default)]
pub struct SpillStats {
    /// Runs created. PG counts one temp FILE per run, which is what a
    /// run is here.
    pub files: core::sync::atomic::AtomicU64,
    /// Bytes written across those runs.
    pub bytes: core::sync::atomic::AtomicU64,
}
