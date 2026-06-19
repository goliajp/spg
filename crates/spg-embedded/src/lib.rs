// v7.7.2 — every public item in this crate must carry a
// doc-comment; new code that adds a `pub` without one fails CI.
#![deny(missing_docs)]

//! # spg-embedded
//!
//! Ergonomic embedded-mode entry point for SPG. Wraps the
//! `spg-engine` execution layer for in-process applications
//! that don't want to spin up a TCP listener / fork to the
//! `spg-server` binary.
//!
//! ## Quick start
//!
//! ```no_run
//! use spg_embedded::Database;
//!
//! // On-disk, durable. WAL fsynced per commit; auto-checkpoint
//! // at 4 MiB WAL by default.
//! let mut db = Database::open_path("/data/app.db").unwrap();
//! db.execute("CREATE TABLE users (id INT NOT NULL, name TEXT)").unwrap();
//! db.execute("INSERT INTO users VALUES (1, 'alice')").unwrap();
//! let rows = db.query("SELECT name FROM users WHERE id = 1").unwrap();
//! for row in &rows {
//!     println!("{:?}", row);
//! }
//! ```
//!
//! ## Production checklist (v7.5)
//!
//! - **Persistence**: `Database::open_path(p)` writes a
//!   crash-consistent WAL + periodic checkpoint snapshot. The
//!   on-disk format is byte-identical to what `spg-server`
//!   produces, so a database can move between modes without
//!   conversion.
//! - **Durability**: every `execute()` that mutates calls
//!   `fsync` before returning `Ok`. There is no group commit
//!   in embedded mode — every commit pays one fsync. If you
//!   need batch throughput, wrap multiple statements in
//!   [`Database::with_transaction`] which fsyncs only at
//!   commit.
//! - **Concurrency**: [`Database`] is `Send` but **not** `Sync`.
//!   Share across threads via `Arc<Mutex<Database>>`. The
//!   single-writer model is intentional — see
//!   [STABILITY § A1](https://github.com/lihao/spg/blob/master/STABILITY.md).
//! - **Background work**: [`Database::spawn_background_freezer`]
//!   moves cold rows to disk-resident segments while you keep
//!   serving requests. It runs in a dedicated thread; drop the
//!   returned [`FreezerHandle`] (or call `stop()`) for clean
//!   shutdown.
//! - **Errors**: all public enums ([`EngineError`],
//!   [`QueryResult`], [`Value`]) are `#[non_exhaustive]`. Match
//!   them with a wildcard arm so future v7.x releases can add
//!   variants without breaking your code.
//!
//! ## Panic contract
//!
//! - **No `execute()` / `query()` call panics on user input.**
//!   Malformed SQL, type mismatches, missing tables — all
//!   return `Err(EngineError::…)`. If you observe a panic on
//!   a user-controlled string, that is a bug; file an issue.
//! - The library panics **only** on internal invariant
//!   violations (e.g., catalog snapshot magic mismatch, WAL
//!   record CRC sentinel corruption that survived the boot-
//!   time validation). These represent silent disk corruption
//!   and an unwind would leak inconsistent state, so the
//!   release profile uses `panic = abort` — your host process
//!   dies fast rather than continuing on poisoned data.
//! - If you cannot tolerate `panic = abort`, build with
//!   `--profile release-dbg` (keeps unwind tables) and use
//!   `std::panic::catch_unwind` at your application boundary.
//!
//! ## Why a separate crate?
//!
//! `spg-engine` is `no_std`-compatible (vendored alloc-only).
//! The embedded-mode entry point uses `std` (filesystem,
//! threading), so it lives in its own crate to keep the
//! `no_std` boundary clean.

pub use spg_engine::{CatalogSnapshot, Engine, EngineError, ParsedStatement, QueryResult};
pub use spg_storage::{ColumnSchema, DataType, Value};

/// v7.16.0 — handle for a parsed-and-planned SQL statement.
/// Hand off to [`Database::execute_prepared`] / [`Database::query_prepared`]
/// with a `&[Value]` slice carrying the bind parameters (PG-style
/// `$1`, `$2`, … positional). Cheap to `Clone`; the underlying AST
/// is shared by handle copies and cloned per bind call by the
/// engine's executor.
///
/// The handle holds a snapshot of the AST at prepare time. If
/// the engine's plan cache evicts the entry between prepare and
/// execute (e.g. ANALYZE bumps the statistics version) the
/// stored AST keeps working — `execute_prepared` operates on
/// the handle's clone, not the cache entry.
#[derive(Debug, Clone)]
pub struct Statement {
    /// The parsed + planned AST. `spg-engine::prepare_cached`
    /// returns it as a clone of the cached plan, so any rewrite
    /// passes (`expand_group_by_all`, `reorder_joins`, …) have
    /// already run.
    pub(crate) stmt: ParsedStatement,
    /// Original SQL source, kept for `Display` / debug only.
    /// WAL persistence renders from the AST so a bind-time
    /// rewrite of `$1..$N` survives replay.
    pub(crate) sql: String,
}

impl Statement {
    /// Borrow the original SQL source — useful for tracing and
    /// debug logs. WAL replay does NOT use this; it serialises
    /// the bind-final AST instead.
    #[must_use]
    pub fn sql(&self) -> &str {
        &self.sql
    }
}

/// v7.16.0 — internal WAL helper. Mirrors what
/// `Engine::execute_prepared` does to the cloned AST so the WAL
/// record carries the bind-final SQL text (so replay's
/// simple-query path reconstructs the same row state without
/// needing the original `Statement` handle to still be alive).
/// Errors from the underlying engine helper would only fire if
/// the bind-final stmt referenced a placeholder past the params
/// slice — and that case has already errored in the executor
/// above before this helper runs, so we discard the Result here.
fn wal_render_with_params(stmt: &mut ParsedStatement, params: &[Value]) {
    let _ = spg_engine::substitute_placeholders(stmt, params);
}

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// v7.11.3 — wall-clock provider injected into every embedded
/// `Engine`. Microseconds since the Unix epoch; clamps to
/// `i64::MAX` if the system clock is far-future. Used by SQL's
/// `NOW()` / `CURRENT_TIMESTAMP` / `CURRENT_DATE` rewrite layer
/// so PG-idiomatic time queries work without the caller wiring
/// their own clock.
/// v7.36 (mailrs ask #4) — flatten an `EXPLAIN` QueryResult into
/// the QUERY PLAN string lines. `EXPLAIN` always returns a single-
/// column TEXT table; anything else is treated as no plan output.
fn extract_query_plan_lines(result: QueryResult) -> Vec<String> {
    match result {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .filter_map(|r| {
                r.values.into_iter().next().and_then(|v| match v {
                    Value::Text(s) => Some(s),
                    _ => None,
                })
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// v7.37.2 — auto-warm the OS page cache for cold-tier segments at
/// `open_path` / `restore` time. Per the zero-customer-change rule
/// the client never calls `warm_up_cold_tier()` from app code; the
/// catalog is server-ready when its constructor returns.
///
/// Budget controls:
/// * `SPG_WARM_UP_COLD_BUDGET_MS=N` — stop warming after N ms
///   wall-clock (best-effort; granularity is per-table). Unset =
///   no cap.
/// * `SPG_WARM_UP_COLD_BUDGET_MS=0` — skip warm-up entirely (escape
///   hatch for ops that need fast restart even at the cost of the
///   first-query cold spike).
fn autowarm_cold_tier_on_open(db: &Database) {
    let budget_ms = std::env::var("SPG_WARM_UP_COLD_BUDGET_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok());
    if let Some(0) = budget_ms {
        return;
    }
    let start = std::time::Instant::now();
    let touched = db.warm_up_cold_tier();
    let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    let over_budget = budget_ms.is_some_and(|b| elapsed_ms > b);
    let _ = (touched, over_budget); // future: tracing::info
}

fn wall_clock_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
}

use spg_manifest::{CatalogManifest, ColdSegmentEntry, manifest_path as spg_manifest_path};

// -- v7.1 WAL format constants (mirror `spg-server`'s) ---------
// Kept private so callers can't mis-frame records; the v3 layout
// is the same the server uses, so a `spg-server` boot can read a
// database an embedded process wrote and vice versa.
const WAL_V2_SENTINEL: u32 = 0x8000_0000;
const WAL_V3_FLAG: u32 = 0x4000_0000;
const WAL_V3_TYPE_AUTO_COMMIT_SQL: u8 = 0x01;
/// v7.18 — durability checkpoint marker stays at 0x02 (skipped on replay).
const WAL_V3_TYPE_DURABILITY_CHECKPOINT: u8 = 0x02;
/// v7.18 PITR — auto-commit-sql record with appended (commit_lsn,
/// commit_unix_us) fields so replay can target a specific point in
/// time. Backward-compat: v3 records (type 0x01) keep working, the
/// envelope flag bits are unchanged. The new type byte is the
/// schema-version discriminator.
const WAL_V4_TYPE_AUTO_COMMIT_SQL: u8 = 0x10;
/// v7.18 — sentinel for "no wall clock" inside a v4 record's
/// commit_unix_us slot. Restore-to-timestamp skips records with
/// this sentinel (no time anchor); LSN-based restore is
/// unaffected.
const WAL_V4_NO_CLOCK: i64 = i64::MIN;
/// v7.18 — extra header bytes after the type byte in a v4 record:
/// 8 bytes commit_lsn (u64 LE) + 8 bytes commit_unix_us (i64 LE).
const WAL_V4_EXTRA_HEADER: usize = 16;
/// v7.18 PITR — checkpoint anchor record written to the WAL *before*
/// the snapshot file replaces the on-disk catalog. Carries the
/// (lsn, ts, snapshot_path) triple so restore tooling can find the
/// matching base snapshot without scanning the filesystem. Replay
/// dispatch skips it (same as the v3 durability marker).
const WAL_V4_TYPE_CHECKPOINT_MARKER: u8 = 0x11;

/// v7.21 (mailrs embed round-12 polish) — one COMMITted explicit
/// transaction, flushed atomically at COMMIT time. Payload = the
/// transaction's bind-final mutation statements joined with `";\n"`;
/// replay re-splits via [`split_statements`] and applies in order.
/// Same 16-byte (commit_lsn, commit_unix_us) prefix as the v4
/// auto-commit record. The record is CRC-framed like every other
/// record, so replay applies the whole transaction or — torn tail —
/// none of it; a transaction can never half-resurrect.
///
/// Why it exists: in-transaction mutations only touch the engine's
/// shadow catalog (`modified_catalog: false`), so the per-statement
/// auto-commit append never fired and a COMMIT followed by a crash
/// (no graceful Drop checkpoint) lost the transaction.
const WAL_V4_TYPE_TX_COMMIT_SQL: u8 = 0x12;

/// v7.34 (crash-recovery P0 #2) — row-level physical redo record. Same v4
/// envelope (lsn + ts + payload + CRC) but the payload is `encode_redo_log`
/// bytes, not SQL. Replay applies the physical [`RowChange`]s via
/// `Engine::apply_redo` instead of re-executing — O(changed rows), not the
/// O(records × catalog_rows) statement-replay that hung the mailrs P0.
const WAL_V5_TYPE_ROW_REDO: u8 = 0x13;

/// v7.1 — auto-checkpoint threshold. Once the WAL grows past
/// this many bytes, the next successful `execute()` call ends
/// with a `checkpoint()` so the WAL stays bounded. Tunable via
/// `SPG_EMBEDDED_CHECKPOINT_BYTES` env.
/// v7.34 (crash-recovery P0 #2) — opt-in row-level redo WAL records.
/// Default OFF during bringup; `SPG_WAL_ROW_REDO=1` makes mutating
/// statements log physical changes (0x13) instead of SQL, so crash
/// recovery applies them in O(changed rows) rather than re-executing in
/// O(records × catalog_rows) (the superlinear replay hang root-caused on
/// the mailrs P0). DDL still logs as SQL (hybrid log). When this returns
/// true, `open_path` arms the engine's redo capture.
fn row_redo_enabled() -> bool {
    std::env::var("SPG_WAL_ROW_REDO")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn default_checkpoint_threshold_bytes() -> u64 {
    std::env::var("SPG_EMBEDDED_CHECKPOINT_BYTES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(4 * 1024 * 1024)
}

/// v7.30.3 (mailrs round-26) — per-query byte budget on join/filter
/// materialisation, default ON at 256 MiB for embed parity with the
/// server's allocator-level `SPG_MAX_QUERY_BYTES` default. A fat
/// backfill batch (1000 × full mail bodies) then errors with
/// `QueryBytesExceeded` instead of walking the host into reclaim
/// livelock. `SPG_MAX_QUERY_BYTES=0` disables; any other value
/// overrides. NOT applied to the WAL-replay engine — replay must
/// never fail on a tuning knob.
fn engine_with_query_byte_budget(engine: Engine) -> Engine {
    const DEFAULT_MAX_QUERY_BYTES: usize = 256 * 1024 * 1024;
    match std::env::var("SPG_MAX_QUERY_BYTES")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
    {
        Some(0) => engine,
        Some(n) => engine.with_max_query_bytes(n),
        None => engine.with_max_query_bytes(DEFAULT_MAX_QUERY_BYTES),
    }
}

/// v7.1 — encode one v3 `auto_commit_sql` record. Layout:
///
/// ```text
/// [u32 LE (len | WAL_V2_SENTINEL | WAL_V3_FLAG)]
/// [u32 LE crc32 over (type_byte || sql_bytes)]
/// [u8 type = 0x01]
/// [sql bytes]
/// ```
fn encode_v3_auto_commit(sql: &str) -> Vec<u8> {
    let payload = sql.as_bytes();
    let mut crc_buf = Vec::with_capacity(1 + payload.len());
    crc_buf.push(WAL_V3_TYPE_AUTO_COMMIT_SQL);
    crc_buf.extend_from_slice(payload);
    let crc = spg_crypto::crc32::crc32(&crc_buf);
    let header = ((payload.len() as u32) | WAL_V2_SENTINEL | WAL_V3_FLAG).to_le_bytes();
    let mut out = Vec::with_capacity(4 + 4 + 1 + payload.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&crc.to_le_bytes());
    out.push(WAL_V3_TYPE_AUTO_COMMIT_SQL);
    out.extend_from_slice(payload);
    out
}

/// v7.20 P2 — WAL group-commit. N concurrent commits share one
/// fsync (the 4.2 ms p50 that profile_breakdown measured as
/// 99.2% of the durable write path).
///
/// Leader-follower protocol, same family as PG's group commit:
///
/// 1. `enqueue(record)` — called while the caller still holds
///    the engine's write lock. Appends the encoded record to the
///    shared buffer, returns a sequence ticket. O(memcpy).
/// 2. Caller RELEASES the engine write lock (the next writer's
///    mutation proceeds in parallel with this batch's fsync).
/// 3. `wait_flushed(seq)` — if nobody is flushing, the caller
///    elects itself leader: swaps the buffer out, writes +
///    fsyncs ONCE for every record in the batch, marks the
///    batch durable, wakes all followers. Otherwise it parks on
///    the condvar until a leader covers its seq.
///
/// Durability contract is unchanged from v7.19: `execute()`
/// does not return Ok until the record that describes its
/// mutation is fsynced. The only change is N callers sharing
/// one fsync instead of paying one each.
///
/// Lock order (deadlock-free): `state` then `file`; never the
/// reverse. The leader holds `file` WITHOUT `state` during IO so
/// enqueues continue while fsync runs.
#[derive(Debug)]
struct WalGroup {
    state: Mutex<WalGroupState>,
    cond: std::sync::Condvar,
    /// Active chunk file handle. Separate lock from `state` so
    /// the leader's write+fsync doesn't block concurrent
    /// enqueues. Swapped by `checkpoint()` at rotation.
    file: Mutex<File>,
}

#[derive(Debug)]
struct WalGroupState {
    /// Encoded records awaiting flush.
    buf: Vec<u8>,
    /// Monotonic enqueue counter (1-based).
    enqueued_seq: u64,
    /// Highest seq whose record is fsynced.
    flushed_seq: u64,
    /// True while some caller is inside the leader IO section.
    leader_active: bool,
    /// Sticky fatal error — a failed fsync poisons the WAL
    /// (loud, never silent). All current + future waiters error.
    failed: Option<String>,
    /// Bytes written to the active chunk since rotation —
    /// drives the auto-checkpoint trigger.
    written_len: u64,
}

/// Ticket returned by the buffered write path; `wait()` blocks
/// until the record it covers is durable (or the WAL is
/// poisoned). Cheap to move across threads.
#[derive(Debug)]
pub struct WalTicket {
    group: Arc<WalGroup>,
    seq: u64,
}

/// v7.34 (crash-recovery P0 #2) — RAII reset for the WalGroup leader
/// flag. Electing a leader sets `leader_active = true` and releases the
/// state lock for the sleep+IO window; if a panic unwinds through that
/// window the flag would stay true and every follower would park forever
/// on the condvar — no one left to flush or wake them, the same
/// total-write hang an unclean stop causes, but self-inflicted. This
/// guard clears the flag and wakes the followers (so one re-elects) on
/// ANY drop, including a panic unwind; the normal path disarms it after
/// resetting the flag itself.
struct LeaderGuard<'a> {
    group: &'a WalGroup,
    armed: bool,
}

impl Drop for LeaderGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut g = self.group.state.lock().unwrap_or_else(|e| e.into_inner());
            g.leader_active = false;
            drop(g);
            self.group.cond.notify_all();
        }
    }
}

impl WalGroup {
    fn new(file: File, initial_len: u64) -> Self {
        Self {
            state: Mutex::new(WalGroupState {
                buf: Vec::new(),
                enqueued_seq: 0,
                flushed_seq: 0,
                leader_active: false,
                failed: None,
                written_len: initial_len,
            }),
            cond: std::sync::Condvar::new(),
            file: Mutex::new(file),
        }
    }

    /// Append `record` to the pending batch. Returns the seq the
    /// caller must wait on. Called under the engine write lock —
    /// keep it O(memcpy).
    fn enqueue(&self, record: &[u8]) -> u64 {
        let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        g.buf.extend_from_slice(record);
        g.enqueued_seq += 1;
        g.enqueued_seq
    }

    /// Block until `seq` is durable. Leader-follower: the first
    /// arriving waiter flushes for everyone.
    fn wait_flushed(&self, seq: u64) -> Result<(), EngineError> {
        let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(e) = &g.failed {
                return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                    format!("WAL poisoned by earlier flush failure: {e}"),
                )));
            }
            if g.flushed_seq >= seq {
                return Ok(());
            }
            if !g.leader_active {
                // Elect self leader.
                g.leader_active = true;
                drop(g);
                // v7.34 — panic-safety: if anything below unwinds before
                // `leader_active` is reset, this guard releases it +
                // wakes a follower to re-elect (else all writers park
                // forever). Disarmed on the normal path after the reset.
                let mut leader_guard = LeaderGuard {
                    group: self,
                    armed: true,
                };
                // v7.20 — commit_delay (PG's same-named knob):
                // before taking the batch, give in-flight
                // writers a short window to enqueue so the
                // shared fsync covers more commits. 150 µs costs
                // ~3.5% on a solo 4.2 ms fsync but multiplies
                // batch size under load. Tunable via
                // SPG_COMMIT_DELAY_US (0 disables).
                let delay = commit_delay_us();
                if delay > 0 {
                    std::thread::sleep(std::time::Duration::from_micros(delay));
                }
                let (batch, flush_to) = {
                    let mut g2 = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    (core::mem::take(&mut g2.buf), g2.enqueued_seq)
                };
                let io_result: std::io::Result<()> = (|| {
                    let mut f = self.file.lock().unwrap_or_else(|e| e.into_inner());
                    f.write_all(&batch)?;
                    f.sync_data()
                })();
                g = self.state.lock().unwrap_or_else(|e| e.into_inner());
                g.leader_active = false;
                leader_guard.armed = false; // normal completion — disarm
                match io_result {
                    Ok(()) => {
                        g.flushed_seq = flush_to;
                        g.written_len = g.written_len.saturating_add(batch.len() as u64);
                    }
                    Err(e) => {
                        g.failed = Some(e.to_string());
                    }
                }
                self.cond.notify_all();
                //

                // Loop continues: either our seq is now covered
                // (leader path normally returns next iteration)
                // or the error branch surfaces.
                continue;
            }
            g = self.cond.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Drain the pending batch + flush synchronously. Caller must
    /// guarantee no concurrent enqueues (checkpoint holds the
    /// engine exclusively). Used before rotation so the marker
    /// lands in the right chunk.
    fn flush_now(&self) -> Result<(), EngineError> {
        let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = &g.failed {
            return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                format!("WAL poisoned: {e}"),
            )));
        }
        let batch = core::mem::take(&mut g.buf);
        let flush_to = g.enqueued_seq;
        if batch.is_empty() {
            return Ok(());
        }
        drop(g);
        let io: std::io::Result<()> = (|| {
            let mut f = self.file.lock().unwrap_or_else(|e| e.into_inner());
            f.write_all(&batch)?;
            f.sync_data()
        })();
        let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match io {
            Ok(()) => {
                g.flushed_seq = flush_to;
                g.written_len = g.written_len.saturating_add(batch.len() as u64);
                self.cond.notify_all();
                Ok(())
            }
            Err(e) => {
                g.failed = Some(e.to_string());
                self.cond.notify_all();
                Err(io_err(e))
            }
        }
    }

    /// Swap the active chunk handle (rotation). Caller flushes
    /// first; both locks taken in canonical order.
    fn rotate_file(&self, new_file: File) {
        let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = self.file.lock().unwrap_or_else(|e| e.into_inner());
        *f = new_file;
        g.written_len = 0;
    }

    fn written_len(&self) -> u64 {
        let g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        g.written_len + g.buf.len() as u64
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// CoW-2 (v7.34) — background-checkpoint worker.
//
// Splits checkpoint into two halves so the front-end pays only the cheap one:
//   • Capture (`Database::snapshot_checkpoint_job`) — under &mut self,
//     Arc-bump the catalog + cheap trailer/cold-segment clones + atomic
//     commit_lsn load. Front returns to caller in microseconds.
//   • Execute (`execute_checkpoint_job`, on the worker thread) — serialize
//     the snapshot, tmp+rename the db / manifest files (each fsynced via
//     the rename + dir-fsync), enqueue the v4 marker through the WalGroup
//     (which is already thread-safe so live commits interleave fine),
//     then rotate the chunk file.
//
// Replay floor is the marker LSN captured at front-end time. A crash any
// time during the worker's sequence is safe: nothing past the previous
// checkpoint's marker can have been forgotten until the new marker hits
// the WAL, and live writes between the two go into the same chunk under
// the old marker — replay re-applies them after restoring the (older)
// snapshot. snapshot+manifest atomicity (D10) is unchanged from the sync
// path — CoW-4 tightens it later.
//
// Single-instance: a state machine of {pending, inflight} so a new
// trigger fires only when the worker is fully idle. Any sticky error
// surfaces on the next `wait()`.

#[derive(Debug)]
struct CheckpointJob {
    snapshot: spg_engine::EngineSnapshot,
    marker_lsn: u64,
    db_path: PathBuf,
    wal_dir: PathBuf,
    wal: Arc<WalGroup>,
    /// Snapshot-time view of the cold-tier segment set. Carried into the
    /// worker so any concurrent `freeze_oldest_to_cold` after the trigger
    /// rides the *next* checkpoint's manifest — same staleness window
    /// the sync path already had.
    cold_segments: Vec<(u32, PathBuf)>,
    /// Shared with `PersistenceCtx` so the worker's chunk rotation is
    /// visible to subsequent diag / Drop introspection.
    current_chunk_path: Arc<Mutex<PathBuf>>,
}

#[derive(Debug, Default)]
struct CheckpointState {
    /// Set by the front when it has a job ready; cleared when the worker
    /// picks it up.
    pending: Option<CheckpointJob>,
    /// True while the worker is mid-execute. `pending.is_some() || inflight`
    /// defines "busy" for the trigger / wait predicate.
    inflight: bool,
    /// Sticky error from the worker's last failure. Cleared when surfaced
    /// to a `wait()` caller.
    last_error: Option<EngineError>,
    /// Drop signal — worker exits after the current job (or immediately if
    /// idle and no pending).
    shutdown: bool,
}

#[derive(Debug)]
struct CheckpointWorker {
    state: Arc<(Mutex<CheckpointState>, Condvar)>,
    handle: Option<JoinHandle<()>>,
}

impl CheckpointWorker {
    fn spawn() -> Self {
        let state: Arc<(Mutex<CheckpointState>, Condvar)> =
            Arc::new((Mutex::new(CheckpointState::default()), Condvar::new()));
        let state_for_thread = Arc::clone(&state);
        let handle = thread::Builder::new()
            .name("spg-checkpoint".into())
            .spawn(move || checkpoint_worker_loop(&state_for_thread))
            .expect("spawn checkpoint worker");
        Self {
            state,
            handle: Some(handle),
        }
    }

    /// Try to enqueue a job. Returns `Ok(true)` if the worker accepted it,
    /// `Ok(false)` if a job was already pending or in flight (skip — the
    /// next trigger will pick up newer state). Surfaces any sticky error
    /// from a previous run before considering the new job, so async paths
    /// can't lose a failure indefinitely.
    fn try_enqueue(&self, job: CheckpointJob) -> Result<bool, EngineError> {
        let (lock, cond) = &*self.state;
        let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = g.last_error.take() {
            return Err(e);
        }
        if g.pending.is_some() || g.inflight {
            return Ok(false);
        }
        g.pending = Some(job);
        cond.notify_one();
        Ok(true)
    }

    /// Block until the worker is idle (no pending, not in flight). Returns
    /// any sticky error from the last run; clears it on the way out.
    fn wait(&self) -> Result<(), EngineError> {
        let (lock, cond) = &*self.state;
        let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
        while g.pending.is_some() || g.inflight {
            g = cond.wait(g).unwrap_or_else(|e| e.into_inner());
        }
        match g.last_error.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Drop for CheckpointWorker {
    fn drop(&mut self) {
        {
            let (lock, cond) = &*self.state;
            let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
            g.shutdown = true;
            cond.notify_one();
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn checkpoint_worker_loop(state: &Arc<(Mutex<CheckpointState>, Condvar)>) {
    let (lock, cond) = &**state;
    loop {
        let job = {
            let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
            while g.pending.is_none() && !g.shutdown {
                g = cond.wait(g).unwrap_or_else(|e| e.into_inner());
            }
            if g.pending.is_none() {
                // shutdown with no pending → exit cleanly.
                return;
            }
            // Even on shutdown, drain the pending job first so the Drop-time
            // final checkpoint is durable before exit.
            let job = g.pending.take().expect("loop invariant");
            g.inflight = true;
            job
        };
        let result = execute_checkpoint_job(job);
        {
            let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
            g.inflight = false;
            if let Err(e) = result {
                g.last_error = Some(e);
            }
            cond.notify_all();
        }
    }
}

fn execute_checkpoint_job(job: CheckpointJob) -> Result<(), EngineError> {
    // 1. Serialize the captured snapshot. Heavy; this is the whole point
    //    of CoW — it runs off the engine borrow.
    let snapshot = job.snapshot.serialize();
    // 2. Snapshot tmp+rename. Atomic on POSIX; rename implicitly fsyncs
    //    the data the next directory walk sees.
    let tmp = {
        let mut t = job.db_path.clone();
        let mut name = t
            .file_name()
            .map(std::ffi::OsStr::to_os_string)
            .unwrap_or_default();
        name.push(".tmp");
        t.set_file_name(name);
        t
    };
    std::fs::write(&tmp, &snapshot).map_err(io_err)?;
    std::fs::rename(&tmp, &job.db_path).map_err(io_err)?;
    // 3. Manifest tmp+rename (cold tier present).
    if !job.cold_segments.is_empty() {
        let snap_crc = spg_crypto::crc32::crc32(&snapshot);
        let entries: Vec<ColdSegmentEntry> = job
            .cold_segments
            .iter()
            .filter_map(|(segment_id, path)| {
                let bytes = std::fs::read(path).ok()?;
                Some(ColdSegmentEntry {
                    segment_id: *segment_id,
                    path: path.clone(),
                    crc32: spg_crypto::crc32::crc32(&bytes),
                })
            })
            .collect();
        let manifest = CatalogManifest {
            catalog_crc32: snap_crc,
            cold_segments: entries,
            wal_baseline_offset: 0,
        };
        let m_bytes = manifest.serialize();
        let m_path = spg_manifest_path(&job.db_path);
        if let Some(dir) = m_path.parent() {
            std::fs::create_dir_all(dir).map_err(io_err)?;
        }
        let m_tmp = {
            let mut t = m_path.clone();
            let mut name = t
                .file_name()
                .map(std::ffi::OsStr::to_os_string)
                .unwrap_or_default();
            name.push(".tmp");
            t.set_file_name(name);
            t
        };
        std::fs::write(&m_tmp, &m_bytes).map_err(io_err)?;
        std::fs::rename(&m_tmp, &m_path).map_err(io_err)?;
    }
    // 4. Enqueue the v4 checkpoint marker carrying the captured LSN. The
    //    WalGroup is thread-safe so a live commit can interleave — the
    //    marker's LSN, not its position in the chunk, anchors replay.
    let marker_ts = wall_clock_micros();
    let marker = encode_v4_checkpoint_marker(job.marker_lsn, marker_ts, &job.db_path);
    job.wal.enqueue(&marker);
    job.wal.flush_now()?;
    // 5. Rotate the active chunk. New commits land in the fresh chunk;
    //    pre-marker history stays addressable in the old chunk for PITR /
    //    retention. The shared `current_chunk_path` is updated under its
    //    own lock before the WalGroup swap so diag readers never see a
    //    handle that no longer matches the recorded path.
    let new_chunk_path = job
        .wal_dir
        .join(chunk_filename(marker_ts, job.marker_lsn + 1));
    let new_handle = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(&new_chunk_path)
        .map_err(io_err)?;
    fsync_dir(&job.wal_dir);
    {
        let mut p = job
            .current_chunk_path
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *p = new_chunk_path;
    }
    job.wal.rotate_file(new_handle);
    Ok(())
}

impl WalTicket {
    /// Block until the record this ticket covers is durable.
    ///
    /// Under `SPG_SYNCHRONOUS_COMMIT=off` this returns
    /// immediately — the background flusher (or the next
    /// checkpoint / clean shutdown) makes the record durable
    /// within `SPG_WAL_WRITER_DELAY_MS`. Same contract as PG's
    /// `synchronous_commit = off`.
    ///
    /// # Errors
    /// Surfaces the leader's IO error if the batch flush failed
    /// (the WAL is then poisoned for all subsequent writes).
    pub fn wait(&self) -> Result<(), EngineError> {
        if !synchronous_commit_on() {
            return Ok(());
        }
        self.group.wait_flushed(self.seq)
    }
}

/// v7.19 P3 — retention sweep loop. Runs in a dedicated thread
/// spawned by `Database::open_path` when `SPG_PITR_RETENTION_HOURS`
/// is set to a non-zero value. Wakes every
/// `SPG_PITR_RETENTION_CHECK_SEC` (default 60 s), enumerates chunks
/// under `wal_dir`, archives via `SPG_PITR_ARCHIVE_CMD` if set, and
/// deletes anything older than `retention_hours`.
///
/// Loud-failure posture matches PG's `archive_command`: if the
/// archive command returns non-zero, the chunk stays on disk and
/// a warning prints to stderr. The retention sweep doesn't delete
/// a chunk it failed to archive.
fn retention_sweep_loop(
    wal_dir: PathBuf,
    retention_hours: u64,
    check_interval: std::time::Duration,
    archive_cmd: Option<String>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::SeqCst) {
        if let Err(e) = retention_sweep_once(&wal_dir, retention_hours, archive_cmd.as_deref()) {
            eprintln!("spg-embedded: retention sweep error: {e}");
        }
        // Sleep in short ticks so shutdown isn't blocked on a
        // 60 s naptime when Drop signals.
        let mut elapsed = std::time::Duration::ZERO;
        let tick = std::time::Duration::from_millis(250);
        while elapsed < check_interval {
            if shutdown.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(tick);
            elapsed += tick;
        }
    }
}

/// v7.19 P3 — one retention sweep pass over `wal_dir`. Extracted
/// from the loop so tests can drive it directly. Public so the
/// e2e_pitr_retention integration test (and any future operator
/// tooling that wants synchronous retention) can call it.
pub fn retention_sweep_once(
    wal_dir: &Path,
    retention_hours: u64,
    archive_cmd: Option<&str>,
) -> std::io::Result<()> {
    if !wal_dir.exists() {
        return Ok(());
    }
    let now_us = wall_clock_micros();
    let cutoff_us = (now_us as i128 - (retention_hours as i128 * 3_600 * 1_000_000)) as i64;
    let chunks = sorted_wal_chunks(wal_dir)?;
    for chunk in chunks {
        // Don't sweep the most-recent chunk; it's the live one
        // execute() is appending to. Compare against the largest
        // filename-prefix unix_us.
        let stem = match chunk.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };
        let chunk_us: i64 = stem
            .split_once('_')
            .and_then(|(prefix, _)| i64::from_str_radix(prefix, 16).ok())
            .unwrap_or(0);
        if chunk_us >= cutoff_us {
            continue;
        }
        // Archive first if requested.
        if let Some(cmd) = archive_cmd {
            if !cmd.is_empty() {
                let output = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(cmd)
                    .arg("--")
                    .arg(&chunk)
                    .output()?;
                if !output.status.success() {
                    eprintln!(
                        "spg-embedded: SPG_PITR_ARCHIVE_CMD failed for {} (exit {}); chunk stays on disk",
                        chunk.display(),
                        output.status.code().unwrap_or(-1)
                    );
                    continue;
                }
            }
        }
        // Delete the chunk + its sibling .checksum if present.
        if let Err(e) = std::fs::remove_file(&chunk) {
            eprintln!(
                "spg-embedded: retention remove {} failed: {e}",
                chunk.display()
            );
            continue;
        }
        let mut cs = chunk.clone();
        let mut name = cs.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(".checksum");
        cs.set_file_name(name);
        let _ = std::fs::remove_file(&cs);
    }
    Ok(())
}

/// v7.20 — group-commit delay window in µs (PG `commit_delay`
/// analogue). The flush leader sleeps this long before taking
/// the batch so concurrent writers pile in. Default 150 µs;
/// `SPG_COMMIT_DELAY_US=0` disables.
fn commit_delay_us() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("SPG_COMMIT_DELAY_US")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(150)
    })
}

/// v7.20 — PG `synchronous_commit` analogue. `on` (default):
/// `execute()` blocks until its WAL record is fsynced —
/// zero-loss durability. `off`: `execute()` returns after the
/// in-memory mutation + WAL enqueue; a background flusher
/// thread writes + fsyncs every `SPG_WAL_WRITER_DELAY_MS`
/// (default 200 ms — PG's `wal_writer_delay` default). Crash
/// window = up to one flush interval of confirmed-but-unsynced
/// commits — exactly the trade PG documents for the same
/// setting. Clean shutdown (Drop / checkpoint) always flushes.
fn synchronous_commit_on() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        !std::env::var("SPG_SYNCHRONOUS_COMMIT")
            .map(|v| v.eq_ignore_ascii_case("off") || v == "0" || v.eq_ignore_ascii_case("false"))
            .unwrap_or(false)
    })
}

/// v7.20 — background WAL flusher cadence for
/// `SPG_SYNCHRONOUS_COMMIT=off` (PG `wal_writer_delay`).
fn wal_writer_delay_ms() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("SPG_WAL_WRITER_DELAY_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(200)
    })
}

fn pitr_retention_hours() -> u64 {
    std::env::var("SPG_PITR_RETENTION_HOURS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0)
}

fn pitr_retention_check_sec() -> u64 {
    std::env::var("SPG_PITR_RETENTION_CHECK_SEC")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(60)
}

fn pitr_archive_cmd() -> Option<String> {
    std::env::var("SPG_PITR_ARCHIVE_CMD")
        .ok()
        .filter(|s| !s.is_empty())
}

/// v7.19 — replay every record from `wal_bytes` whose
/// `commit_lsn` is strictly greater than `floor_lsn`. v3 records
/// (no LSN) and v4 records with `commit_lsn <= floor_lsn` are
/// skipped — the snapshot loaded ahead of this call already
/// reflects them, and re-applying would DuplicateTable /
/// double-insert. v3 records inside the legacy migration chunk
/// always apply because the migration sets `floor_lsn = 0` and
/// v3 records carry no LSN to compare; the pre-migration
/// behaviour (every record replays) is what the migration
/// preserves.
///
/// Returns the count of records successfully applied. Same
/// torn-tail semantics as `replay_wal_into_engine`.
fn replay_wal_filtered(
    wal_bytes: &[u8],
    engine: &mut Engine,
    floor_lsn: u64,
    quarantine: &mut Vec<QuarantinedStmt>,
) -> Result<usize, String> {
    let records = parse_wal_records(wal_bytes)?;
    let mut applied = 0usize;
    for r in &records {
        // Skip markers + non-SQL records.
        if r.type_byte == WAL_V3_TYPE_DURABILITY_CHECKPOINT
            || r.type_byte == WAL_V4_TYPE_CHECKPOINT_MARKER
        {
            continue;
        }
        // v4 SQL records carry an LSN. Apply iff strictly above
        // the snapshot floor.
        if r.type_byte == WAL_V4_TYPE_AUTO_COMMIT_SQL
            || r.type_byte == WAL_V4_TYPE_TX_COMMIT_SQL
            || r.type_byte == WAL_V5_TYPE_ROW_REDO
        {
            if let Some(lsn) = r.commit_lsn {
                if lsn <= floor_lsn {
                    continue;
                }
            }
        }
        // v7.34 (crash-recovery P0 #2) — row-level redo record: apply the
        // physical changes directly (O(changed rows)) instead of
        // re-executing SQL (the O(records × rows) statement-replay that
        // hung the mailrs P0). The payload is `encode_redo_log` bytes, not
        // SQL, so it never enters the from_utf8 / split_statements path.
        if r.type_byte == WAL_V5_TYPE_ROW_REDO {
            let changes = spg_storage::decode_redo_log(r.sql)
                .map_err(|e| format!("redo decode at offset {}: {e:?}", r.offset))?;
            engine
                .apply_redo(&changes)
                .map_err(|e| format!("redo apply at offset {}: {e:?}", r.offset))?;
            applied += 1;
            continue;
        }
        // v3 records (type 0x01, no LSN) always apply — the
        // legacy migration path is the only place they appear,
        // and floor_lsn=0 there.
        let sql = match std::str::from_utf8(r.sql) {
            Ok(s) => s,
            Err(e) => return Err(format!("non-UTF-8 SQL at offset {}: {e}", r.offset)),
        };
        // v7.21 — a tx-commit record carries the whole transaction
        // as a `";\n"`-joined script; auto-commit records are a
        // single statement, for which split_statements is a no-op.
        //
        // v7.30.1 (mailrs round-24 ask 2) — a statement the engine
        // REJECTS is quarantined, not fatal: "one statement failed
        // to replay" ≠ "the catalog is corrupt". Framing damage
        // (parse_wal_records / non-UTF-8 above) still errors — that
        // IS corruption. Subsequent statements of a tx script keep
        // applying: the bricking class is a no-op-at-runtime
        // statement that re-applies non-idempotently, and skipping
        // just it reconstructs the runtime state.
        for stmt in split_statements(sql) {
            if let Err(e) = engine.execute(stmt) {
                quarantine.push(QuarantinedStmt {
                    offset: r.offset,
                    sql: stmt.to_string(),
                    error: format!("{e:?}"),
                });
            }
        }
        applied += 1;
    }
    Ok(applied)
}

/// v7.30.1 (mailrs round-24 ask 2) — one statement that failed to
/// re-apply during boot replay. Kept for forensics in a
/// `quarantine-*.log` beside the WAL chunks; the boot continues.
struct QuarantinedStmt {
    offset: usize,
    sql: String,
    error: String,
}

fn format_quarantine_line(q: &QuarantinedStmt) -> String {
    format!("offset {}: {}\n  rejected: {}\n", q.offset, q.sql, q.error)
}

/// v7.19 — WAL chunk filename format. Zero-padded 16-digit
/// hex on both parts so default lexicographic sort matches
/// numeric order, with the unix_us prefix coming first so
/// the on-disk listing is chronological too.
/// v7.34 (crash-recovery P0 #2) — fsync a directory so a newly created
/// file's entry is durable. `sync_data` on a chunk file persists its
/// bytes but NOT the parent directory entry that names it; a power loss
/// after creating a fresh WAL chunk could lose that entry and make the
/// chunk (and the committed records in it) unreachable on restart.
/// Best-effort — a platform that rejects directory fsync is no worse off.
fn fsync_dir(dir: &Path) {
    if let Ok(f) = File::open(dir) {
        let _ = f.sync_all();
    }
}

fn chunk_filename(unix_us: i64, leading_lsn: u64) -> String {
    // Negative timestamps shouldn't happen in practice (we sit
    // post-1970), but clamp to 0 so the zero-padded
    // representation stays sortable.
    let us = unix_us.max(0) as u64;
    format!("{us:016x}_{leading_lsn:016x}.wal")
}

/// v7.19 — filename used for the legacy single-file WAL when
/// `open_path` migrates a v7.18-layout database into the new
/// chunk directory. Lexicographically smallest possible value
/// so subsequent chunks sort after it.
fn legacy_chunk_filename() -> String {
    chunk_filename(0, 0)
}

/// CoW-4 (v7.34) — D10 fallback: read one cold-segment file and
/// hand its bytes to the catalog. The segment binary is self-validating
/// (magic + internal CRC32 via `OwnedSegment::from_bytes`), so we don't
/// need the manifest's `segment_crc32` to trust it. Returns `true` on a
/// successful attach (caller bumps `cold_segment_paths`), `false` on a
/// per-segment failure that is logged but doesn't abort boot.
fn attach_segment_from_disk(engine: &mut Engine, segment_id: u32, path: &Path) -> bool {
    if engine.catalog().cold_segment(segment_id).is_some() {
        return true;
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!(
                "spg-embedded: cold-segment scan skip {}: read failed: {e}",
                path.display()
            );
            return false;
        }
    };
    let mut new_cat = engine.catalog().clone();
    if let Err(e) = new_cat.load_segment_bytes_at(segment_id, bytes) {
        eprintln!(
            "spg-embedded: cold-segment scan skip {}: parse/load failed: {e}",
            path.display()
        );
        return false;
    }
    engine.replace_catalog(new_cat);
    true
}

/// CoW-4 (v7.34) — D10 + missing-manifest fallback: scan
/// `<db>.spg/segments/` for `seg_<id>.spg` files and attach any that
/// aren't already in `cold_segment_paths`. Closes the window where a
/// crash between snapshot rename and manifest rename leaves
/// post-checkpoint cold segments orphaned on disk (the snapshot's CRC
/// no longer matches the stale manifest, so the manifest path
/// silently dropped them). The segment parser self-verifies, so a
/// torn write surfaces as a per-segment skip, never silent corruption.
fn scan_cold_segments_dir(
    segments_dir: &Path,
    engine: &mut Engine,
    cold_segment_paths: &mut BTreeMap<u32, PathBuf>,
) {
    // v7.34.1 (mailrs prod report bug A): single-file catalogs (e.g.
    // `/data/spg/mailrs.spg` is a regular file, not the `<db>/<db>.spg`
    // layout this scan assumes) make the computed `<db>.spg/segments`
    // path traverse a file inode, which surfaces as ENOTDIR (`Not a
    // directory`, errno 20). Treat any non-directory state — absent,
    // file-in-the-way, stat-blocked — as "no segments to scan" and
    // silently return. The eprintln below only fires for the genuine
    // mid-walk read errors (permission flip, IO failure) that operators
    // need to see.
    if !segments_dir.is_dir() {
        return;
    }
    let read_dir = match std::fs::read_dir(segments_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            eprintln!(
                "spg-embedded: cold-segment scan: cannot read {}: {e}",
                segments_dir.display()
            );
            return;
        }
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        // Only the canonical `seg_<id>.spg` form. `.tmp` half-renames
        // and unknown extensions are skipped — the segment writer's
        // tmp+rename pattern guarantees `.spg` files are either fully
        // written or absent.
        if path.extension().and_then(|s| s.to_str()) != Some("spg") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(id_str) = stem.strip_prefix("seg_") else {
            continue;
        };
        let Ok(segment_id) = id_str.parse::<u32>() else {
            continue;
        };
        if cold_segment_paths.contains_key(&segment_id) {
            continue;
        }
        if attach_segment_from_disk(engine, segment_id, &path) {
            cold_segment_paths.insert(segment_id, path);
        }
    }
}

/// v7.19 — list every `.wal` file in `wal_dir` in
/// lexicographic order (which doubles as chunk-creation
/// order thanks to the zero-padded filename format).
fn sorted_wal_chunks(wal_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let read_dir = match std::fs::read_dir(wal_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(paths),
        Err(e) => return Err(e),
    };
    for entry in read_dir {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("wal") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

/// v7.18 PITR — encode one v4 `checkpoint_marker` record. Layout:
///
/// ```text
/// [u32 LE (payload_len | WAL_V2_SENTINEL | WAL_V3_FLAG)]
/// [u32 LE crc32 over (type_byte || payload)]
/// [u8  type = 0x11]
/// payload:
///   [u64 LE checkpoint_lsn]
///   [i64 LE checkpoint_unix_us  (WAL_V4_NO_CLOCK if no clock)]
///   [u16 LE snapshot_path_len]
///   [snapshot_path_bytes]
/// ```
///
/// `payload_len` covers only the payload — keeping the framing
/// uniform across v3 / v4 record types so torn-write detection in
/// `replay_wal_into_engine` stays trivial.
fn encode_v4_checkpoint_marker(
    checkpoint_lsn: u64,
    checkpoint_unix_us: i64,
    snapshot_path: &Path,
) -> Vec<u8> {
    let snapshot_bytes = snapshot_path.to_string_lossy().into_owned();
    let snap_payload = snapshot_bytes.as_bytes();
    let snap_len_u16: u16 = snap_payload.len().min(u16::MAX as usize) as u16;
    let mut payload = Vec::with_capacity(8 + 8 + 2 + snap_payload.len());
    payload.extend_from_slice(&checkpoint_lsn.to_le_bytes());
    payload.extend_from_slice(&checkpoint_unix_us.to_le_bytes());
    payload.extend_from_slice(&snap_len_u16.to_le_bytes());
    payload.extend_from_slice(&snap_payload[..snap_len_u16 as usize]);
    let mut crc_buf = Vec::with_capacity(1 + payload.len());
    crc_buf.push(WAL_V4_TYPE_CHECKPOINT_MARKER);
    crc_buf.extend_from_slice(&payload);
    let crc = spg_crypto::crc32::crc32(&crc_buf);
    let header = ((payload.len() as u32) | WAL_V2_SENTINEL | WAL_V3_FLAG).to_le_bytes();
    let mut out = Vec::with_capacity(4 + 4 + 1 + payload.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&crc.to_le_bytes());
    out.push(WAL_V4_TYPE_CHECKPOINT_MARKER);
    out.extend_from_slice(&payload);
    out
}

/// v7.18 PITR — encode one v4 `auto_commit_sql` record. Layout:
///
/// ```text
/// [u32 LE (sql_len | WAL_V2_SENTINEL | WAL_V3_FLAG)]
/// [u32 LE crc32 over (type_byte || lsn || ts || sql_bytes)]
/// [u8  type = 0x10]
/// [u64 LE commit_lsn]
/// [i64 LE commit_unix_us  (= WAL_V4_NO_CLOCK when no ClockFn)]
/// [sql bytes]
/// ```
///
/// `sql_len` field stays the SQL byte count — same shape as v3 — so
/// replay-buffer torn-write detection compares against
/// `WAL_V4_EXTRA_HEADER + sql_len`. v3 records (type 0x01) stay
/// readable by the same loop with their original 9-byte header
/// arithmetic.
fn encode_v4_auto_commit(sql: &str, commit_lsn: u64, commit_unix_us: i64) -> Vec<u8> {
    encode_v4_framed(
        WAL_V4_TYPE_AUTO_COMMIT_SQL,
        sql.as_bytes(),
        commit_lsn,
        commit_unix_us,
    )
}

/// v7.21 — same envelope, `WAL_V4_TYPE_TX_COMMIT_SQL` type byte.
/// `script` = the transaction's statements joined with `";\n"`.
fn encode_v4_tx_commit(script: &str, commit_lsn: u64, commit_unix_us: i64) -> Vec<u8> {
    encode_v4_framed(
        WAL_V4_TYPE_TX_COMMIT_SQL,
        script.as_bytes(),
        commit_lsn,
        commit_unix_us,
    )
}

/// v7.34 (crash-recovery P0 #2) — encode one row-level redo record. Same
/// v4 envelope + CRC, type byte 0x13; the payload is the
/// `encode_redo_log` bytes (physical changes) instead of SQL text, so
/// replay applies them in place of re-executing the statement.
fn encode_v5_row_redo(redo_bytes: &[u8], commit_lsn: u64, commit_unix_us: i64) -> Vec<u8> {
    encode_v4_framed(WAL_V5_TYPE_ROW_REDO, redo_bytes, commit_lsn, commit_unix_us)
}

fn encode_v4_framed(
    type_byte: u8,
    payload: &[u8],
    commit_lsn: u64,
    commit_unix_us: i64,
) -> Vec<u8> {
    let mut crc_buf = Vec::with_capacity(1 + WAL_V4_EXTRA_HEADER + payload.len());
    crc_buf.push(type_byte);
    crc_buf.extend_from_slice(&commit_lsn.to_le_bytes());
    crc_buf.extend_from_slice(&commit_unix_us.to_le_bytes());
    crc_buf.extend_from_slice(payload);
    let crc = spg_crypto::crc32::crc32(&crc_buf);
    let header = ((payload.len() as u32) | WAL_V2_SENTINEL | WAL_V3_FLAG).to_le_bytes();
    let mut out = Vec::with_capacity(4 + 4 + 1 + WAL_V4_EXTRA_HEADER + payload.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&crc.to_le_bytes());
    out.push(type_byte);
    out.extend_from_slice(&commit_lsn.to_le_bytes());
    out.extend_from_slice(&commit_unix_us.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// v7.1 — decode + apply every record in `wal_bytes` to `engine`.
/// Returns the count of records successfully applied. A truncated
/// trailing record (mid-write torn) is dropped silently — the
/// same recovery story `spg-server`'s boot path uses.
fn replay_wal_into_engine(wal_bytes: &[u8], engine: &mut Engine) -> Result<usize, String> {
    let mut applied = 0usize;
    let mut cur = 0usize;
    while cur < wal_bytes.len() {
        if wal_bytes.len() - cur < 4 {
            // Trailing partial header — torn write, drop and stop.
            break;
        }
        let raw_len = u32::from_le_bytes(wal_bytes[cur..cur + 4].try_into().unwrap());
        let is_v2 = raw_len & WAL_V2_SENTINEL != 0;
        let is_v3 = is_v2 && (raw_len & WAL_V3_FLAG != 0);
        let len_mask = if is_v3 {
            !(WAL_V2_SENTINEL | WAL_V3_FLAG)
        } else {
            !WAL_V2_SENTINEL
        };
        let rec_len = (raw_len & len_mask) as usize;
        let header_len = if is_v3 {
            9
        } else if is_v2 {
            8
        } else {
            4
        };
        if wal_bytes.len() - cur < header_len + rec_len {
            // Torn record at the tail — drop, stop.
            break;
        }
        if is_v3 {
            let type_byte = wal_bytes[cur + 8];
            match type_byte {
                WAL_V3_TYPE_AUTO_COMMIT_SQL => {}
                WAL_V3_TYPE_DURABILITY_CHECKPOINT => {
                    // durability_checkpoint marker — skip, no SQL.
                    cur += header_len + rec_len;
                    continue;
                }
                WAL_V4_TYPE_CHECKPOINT_MARKER => {
                    // v7.18 PITR — checkpoint anchor, skip on replay
                    // (engine state past this point reflects the
                    // matching snapshot already loaded by the caller).
                    cur += header_len + rec_len;
                    continue;
                }
                WAL_V4_TYPE_AUTO_COMMIT_SQL | WAL_V4_TYPE_TX_COMMIT_SQL => {
                    // v7.18 PITR — v4 record carries 16 bytes of
                    // (commit_lsn, commit_unix_us) between the type
                    // byte and the SQL payload. Replay reads them but
                    // does not enforce them — the engine doesn't
                    // surface LSN/clock here. Restore tooling
                    // (spgctl) parses them via parse_wal_record below.
                    //
                    // v7.21 — tx-commit records (0x12) carry a whole
                    // transaction as a `";\n"`-joined script;
                    // split_statements is a no-op on the single-
                    // statement auto-commit form.
                    let v4_total = header_len + WAL_V4_EXTRA_HEADER + rec_len;
                    if wal_bytes.len() - cur < v4_total {
                        // Torn v4 record at the tail — drop, stop.
                        break;
                    }
                    let sql_start = cur + header_len + WAL_V4_EXTRA_HEADER;
                    let sql_bytes = &wal_bytes[sql_start..sql_start + rec_len];
                    let sql = std::str::from_utf8(sql_bytes)
                        .map_err(|e| format!("WAL replay: non-UTF-8 SQL at offset {cur}: {e}"))?;
                    for stmt in split_statements(sql) {
                        engine.execute(stmt).map_err(|e| {
                            format!("WAL replay: apply {stmt:?} at offset {cur} rejected: {e:?}")
                        })?;
                    }
                    applied += 1;
                    cur += v4_total;
                    continue;
                }
                other => {
                    return Err(format!(
                        "WAL replay: unknown v3 type byte {other:#04x} at offset {cur}"
                    ));
                }
            }
        }
        let sql_bytes = &wal_bytes[cur + header_len..cur + header_len + rec_len];
        let sql = std::str::from_utf8(sql_bytes)
            .map_err(|e| format!("WAL replay: non-UTF-8 SQL at offset {cur}: {e}"))?;
        engine
            .execute(sql)
            .map_err(|e| format!("WAL replay: apply {sql:?} at offset {cur} rejected: {e:?}"))?;
        applied += 1;
        cur += header_len + rec_len;
    }
    Ok(applied)
}

/// v7.18 PITR — parsed WAL record, surfaced for restore / verify
/// tooling. The replay loop above doesn't expose LSN/timestamp;
/// `spgctl restore --to <timestamp>` and `spgctl verify` need them.
/// Returned offsets are byte-positions inside the WAL buffer.
#[derive(Debug, Clone)]
pub struct WalRecord<'a> {
    /// Byte offset in the WAL buffer where this record starts.
    pub offset: usize,
    /// Type byte (0x01 = v3 auto-commit, 0x10 = v4 auto-commit,
    /// 0x02 = durability checkpoint marker).
    pub type_byte: u8,
    /// `Some(lsn)` for v4 records, `None` for v3.
    pub commit_lsn: Option<u64>,
    /// `Some(unix_us)` for v4 records carrying a clock-set timestamp,
    /// `None` for v3 or for v4 records explicitly written with
    /// `WAL_V4_NO_CLOCK` (sentinel for "no ClockFn at commit time").
    pub commit_unix_us: Option<i64>,
    /// SQL payload as borrowed bytes. Empty for durability markers.
    pub sql: &'a [u8],
}

/// v7.18 PITR — iterate over `wal_bytes` yielding one `WalRecord`
/// per intact record. Torn-tail records terminate iteration
/// silently (same recovery story as `replay_wal_into_engine`).
/// Unknown type bytes inside a v3 envelope return `Err` so the
/// caller knows the WAL was written by a newer SPG.
pub fn parse_wal_records(wal_bytes: &[u8]) -> Result<Vec<WalRecord<'_>>, String> {
    let mut out = Vec::new();
    let mut cur = 0usize;
    while cur < wal_bytes.len() {
        if wal_bytes.len() - cur < 4 {
            break;
        }
        let raw_len = u32::from_le_bytes(wal_bytes[cur..cur + 4].try_into().unwrap());
        let is_v2 = raw_len & WAL_V2_SENTINEL != 0;
        let is_v3 = is_v2 && (raw_len & WAL_V3_FLAG != 0);
        let len_mask = if is_v3 {
            !(WAL_V2_SENTINEL | WAL_V3_FLAG)
        } else {
            !WAL_V2_SENTINEL
        };
        let rec_len = (raw_len & len_mask) as usize;
        let header_len = if is_v3 {
            9
        } else if is_v2 {
            8
        } else {
            4
        };
        if wal_bytes.len() - cur < header_len + rec_len {
            break;
        }
        if !is_v3 {
            // v1 / v2 records carry no type byte; treat as legacy
            // auto-commit SQL with no LSN/time.
            let sql = &wal_bytes[cur + header_len..cur + header_len + rec_len];
            out.push(WalRecord {
                offset: cur,
                type_byte: WAL_V3_TYPE_AUTO_COMMIT_SQL,
                commit_lsn: None,
                commit_unix_us: None,
                sql,
            });
            cur += header_len + rec_len;
            continue;
        }
        let type_byte = wal_bytes[cur + 8];
        match type_byte {
            WAL_V3_TYPE_AUTO_COMMIT_SQL => {
                let sql = &wal_bytes[cur + header_len..cur + header_len + rec_len];
                out.push(WalRecord {
                    offset: cur,
                    type_byte,
                    commit_lsn: None,
                    commit_unix_us: None,
                    sql,
                });
                cur += header_len + rec_len;
            }
            WAL_V3_TYPE_DURABILITY_CHECKPOINT => {
                out.push(WalRecord {
                    offset: cur,
                    type_byte,
                    commit_lsn: None,
                    commit_unix_us: None,
                    sql: &[],
                });
                cur += header_len + rec_len;
            }
            WAL_V4_TYPE_CHECKPOINT_MARKER => {
                // v7.18 PITR — payload = (lsn u64)(ts i64)(path_len u16)(path bytes).
                // We surface lsn + ts on the WalRecord; the path lives
                // in `sql` since the type byte already disambiguates
                // record meaning and adding a dedicated field would
                // bloat the iterator return type for every variant.
                if rec_len < 18 {
                    return Err(format!(
                        "WAL parse: checkpoint marker at offset {cur} too short ({rec_len} bytes)"
                    ));
                }
                let lsn = u64::from_le_bytes(
                    wal_bytes[cur + header_len..cur + header_len + 8]
                        .try_into()
                        .unwrap(),
                );
                let ts_raw = i64::from_le_bytes(
                    wal_bytes[cur + header_len + 8..cur + header_len + 16]
                        .try_into()
                        .unwrap(),
                );
                let path_len = u16::from_le_bytes(
                    wal_bytes[cur + header_len + 16..cur + header_len + 18]
                        .try_into()
                        .unwrap(),
                ) as usize;
                if rec_len < 18 + path_len {
                    return Err(format!(
                        "WAL parse: checkpoint marker at offset {cur} truncated path"
                    ));
                }
                let path_start = cur + header_len + 18;
                let path_bytes = &wal_bytes[path_start..path_start + path_len];
                let commit_unix_us = if ts_raw == WAL_V4_NO_CLOCK {
                    None
                } else {
                    Some(ts_raw)
                };
                out.push(WalRecord {
                    offset: cur,
                    type_byte,
                    commit_lsn: Some(lsn),
                    commit_unix_us,
                    sql: path_bytes,
                });
                cur += header_len + rec_len;
            }
            WAL_V4_TYPE_AUTO_COMMIT_SQL | WAL_V4_TYPE_TX_COMMIT_SQL | WAL_V5_TYPE_ROW_REDO => {
                let v4_total = header_len + WAL_V4_EXTRA_HEADER + rec_len;
                if wal_bytes.len() - cur < v4_total {
                    break;
                }
                let lsn = u64::from_le_bytes(
                    wal_bytes[cur + header_len..cur + header_len + 8]
                        .try_into()
                        .unwrap(),
                );
                let ts_raw = i64::from_le_bytes(
                    wal_bytes[cur + header_len + 8..cur + header_len + 16]
                        .try_into()
                        .unwrap(),
                );
                let commit_unix_us = if ts_raw == WAL_V4_NO_CLOCK {
                    None
                } else {
                    Some(ts_raw)
                };
                let sql_start = cur + header_len + WAL_V4_EXTRA_HEADER;
                let sql = &wal_bytes[sql_start..sql_start + rec_len];
                out.push(WalRecord {
                    offset: cur,
                    type_byte,
                    commit_lsn: Some(lsn),
                    commit_unix_us,
                    sql,
                });
                cur += v4_total;
            }
            other => {
                return Err(format!(
                    "WAL parse: unknown type byte {other:#04x} at offset {cur}"
                ));
            }
        }
    }
    Ok(out)
}

/// v7.1 — predicate for "should the next `execute()` mutate the
/// WAL?" Returns `false` for SELECT / SHOW / EXPLAIN / BEGIN /
/// COMMIT / ROLLBACK and the SPG-specific verbs that don't go
/// through the auto-commit record path on the server (CHECKPOINT,
/// COMPACT). Conservative: anything we don't explicitly know is
/// read-only falls through to "write a WAL record".
fn sql_is_read_only(sql: &str) -> bool {
    let t = sql.trim_start();
    let head = t
        .split(|c: char| c.is_whitespace() || c == ';' || c == '(')
        .next()
        .unwrap_or("");
    matches!(
        head.to_ascii_lowercase().as_str(),
        "select"
            | "show"
            | "explain"
            | "begin"
            | "commit"
            | "rollback"
            | "checkpoint"
            | "compact"
            | "wait"
            | "with"
    )
}

/// Embedded SPG database handle. Owns an `Engine` + provides
/// ergonomic wrappers around `execute` and `query`. Drops the
/// engine on `Drop` — no WAL flush / fsync, because v6.10.3
/// is in-memory only.
#[derive(Debug)]
pub struct Database {
    engine: Engine,
    /// v7.1 — persistence sidecar. When `Some(p)`, every
    /// `execute(sql)` that mutates state appends a v4
    /// `auto_commit_sql` WAL record + fsyncs before the call
    /// returns; `Drop` writes a final catalog snapshot to
    /// `<db_path>` so the next session boots from a clean
    /// snapshot + an empty WAL. `None` = in-memory only (the
    /// v6.10.3 shape).
    persistence: Option<PersistenceCtx>,
    /// v7.18 PITR — monotonic per-database commit LSN. Increments
    /// before each successful WAL append; bootstrapped at
    /// open_path from `max(parse_wal_records → commit_lsn)` so
    /// reopen never reuses an LSN. In-memory databases start at
    /// 0 and never advance (no WAL = no LSN-meaningful records).
    commit_lsn: AtomicU64,
    /// v7.21 (round-12 polish) — explicit-transaction WAL buffer.
    /// `Some` between an engine-accepted BEGIN and its
    /// COMMIT / ROLLBACK on a persistent database. In-transaction
    /// mutations only touch the engine's shadow catalog and report
    /// `modified_catalog: false`, so the per-statement auto-commit
    /// append never fires for them; their bind-final SQL collects
    /// here instead and COMMIT flushes the lot as ONE atomic
    /// `WAL_V4_TYPE_TX_COMMIT_SQL` record (ROLLBACK just drops it).
    /// Always `None` for in-memory databases.
    tx_wal: Option<TxWalBuffer>,
}

/// See [`Database::tx_wal`].
#[derive(Debug, Default)]
struct TxWalBuffer {
    /// Bind-final SQL of every non-read-only statement the engine
    /// accepted inside the open transaction, in execution order.
    statements: Vec<String>,
    /// `(savepoint_name, statements.len() at SAVEPOINT time)` —
    /// `ROLLBACK TO SAVEPOINT` truncates `statements` back to the
    /// recorded mark so the WAL record matches what the engine
    /// keeps. PG name-reuse semantics (latest wins).
    savepoints: Vec<(String, usize)>,
}

/// Statement-level transaction-control classification for the WAL
/// buffer. Runs AFTER the engine accepted the statement, so the
/// engine stays the single validator — this only mirrors state.
enum TxControl {
    Begin,
    Commit,
    Rollback,
    RollbackToSavepoint(String),
    Savepoint(String),
    ReleaseSavepoint,
}

fn tx_control_kind(sql: &str) -> Option<TxControl> {
    let mut words = sql
        .split(|c: char| c.is_whitespace() || c == ';')
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase);
    let head = words.next()?;
    match head.as_str() {
        "begin" | "start" => Some(TxControl::Begin),
        "commit" | "end" => Some(TxControl::Commit),
        "savepoint" => words.next().map(TxControl::Savepoint),
        "release" => Some(TxControl::ReleaseSavepoint),
        "rollback" => match words.next().as_deref() {
            // ROLLBACK TO [SAVEPOINT] <name>
            Some("to") => {
                let next = words.next()?;
                let name = if next == "savepoint" {
                    words.next()?
                } else {
                    next
                };
                Some(TxControl::RollbackToSavepoint(name))
            }
            _ => Some(TxControl::Rollback),
        },
        _ => None,
    }
}

#[derive(Debug)]
#[allow(dead_code)] // `wal_dir`/`current_chunk_path` are read at boot; kept for Drop/diag introspection.
struct PersistenceCtx {
    db_path: PathBuf,
    /// v7.19 — WAL chunk directory at `<db_path>.wal/`.
    /// Replaces the v7.18 single-file `<db_path>.wal` layout.
    /// Each chunk file inside is named
    /// `<unix_us>_<leading_lsn>.wal` (zero-padded to 16 digits
    /// so default-lex sort = LSN order).
    wal_dir: PathBuf,
    /// Path of the currently-open chunk file inside `wal_dir`.
    /// Rotated at checkpoint and whenever the chunk crosses
    /// `checkpoint_threshold_bytes`. CoW-2 (v7.34) wraps it in
    /// `Arc<Mutex<…>>` because the background-checkpoint worker
    /// performs the rotation; this struct keeps a clone so Drop /
    /// diag introspection still see the live path.
    current_chunk_path: Arc<Mutex<PathBuf>>,
    /// v7.19 P3 — retention sweeper handle. `Some` when
    /// `SPG_PITR_RETENTION_HOURS > 0` at open_path time; `None`
    /// when retention is disabled (the default; v7.18 behaviour
    /// preserved). The thread polls `wal_dir` every
    /// `SPG_PITR_RETENTION_CHECK_SEC` seconds, archives via
    /// `SPG_PITR_ARCHIVE_CMD` if set, then deletes chunks older
    /// than the retention window. Signalled to exit via
    /// `retention_shutdown` on Drop.
    retention_shutdown: Option<Arc<AtomicBool>>,
    retention_thread: Option<std::thread::JoinHandle<()>>,
    /// v7.20 — background WAL flusher for
    /// `SPG_SYNCHRONOUS_COMMIT=off`. `None` in the default
    /// synchronous mode. Flushes the pending batch every
    /// `SPG_WAL_WRITER_DELAY_MS`; signalled + joined on Drop
    /// before the final checkpoint so clean shutdown never
    /// loses confirmed commits.
    flusher_shutdown: Option<Arc<AtomicBool>>,
    flusher_thread: Option<std::thread::JoinHandle<()>>,
    /// v7.20 P2 — group-commit WAL. Shared with WalTickets
    /// returned by the buffered write path so `wait()` can run
    /// after the engine write lock is released.
    wal: Arc<WalGroup>,
    checkpoint_threshold_bytes: u64,
    /// v7.1.4 — `<db_path>.spg/segments/` directory. Cold-tier
    /// segments produced by `freeze_oldest_to_cold` / compaction
    /// are persisted here as `seg_<id>.spg` files; the manifest
    /// at `<db_path>.spg/manifest.v10` records every active
    /// segment + its CRC32 so the next boot can verify + reload.
    cold_segments_dir: PathBuf,
    cold_segment_paths: BTreeMap<u32, PathBuf>,
    /// v7.17.0 Phase 6.2 — cross-process exclusion lock. Acquired
    /// via `fs::create_dir` on `<db_path>.lock` at open_path
    /// entry; released on Drop by `fs::remove_dir`. atomic on
    /// every supported platform. A second process opening the
    /// same path while the first is still alive hits the
    /// create_dir failure and returns
    /// `EngineError::Unsupported("database is locked by another
    /// process: …")`. Stale locks (process crashed mid-session)
    /// must be cleared via `Database::force_unlock(path)` —
    /// SPG can't safely fingerprint who owned a stale directory
    /// without a libc dep, which would violate spg-embedded's
    /// zero-deps charter.
    lock_path: PathBuf,
    /// CoW-2 (v7.34) — background-checkpoint worker. `None` only
    /// transiently inside `Drop` after the worker has been signalled
    /// and joined. The worker carries Arc clones of `wal` and
    /// `current_chunk_path`, so it can rotate the active chunk and
    /// reflect the new path back here even after the front-end has
    /// returned to the caller.
    checkpoint_worker: Option<CheckpointWorker>,
}

impl Database {
    /// Open a fresh in-memory database. No WAL, no catalog
    /// snapshot on disk — perfect for tests + short-lived
    /// CLI tools.
    #[must_use]
    pub fn open_in_memory() -> Self {
        Self {
            engine: engine_with_query_byte_budget(Engine::new().with_clock(wall_clock_micros)),
            persistence: None,
            commit_lsn: AtomicU64::new(0),
            tx_wal: None,
        }
    }

    /// v7.1 — Open or create a persistent database backed by
    /// the file at `db_path`. The WAL lives at `db_path` +
    /// ".wal" (e.g. `./data/spg.db` → `./data/spg.db.wal`). Boot
    /// path:
    ///
    /// 1. If `db_path` exists, restore the catalog snapshot.
    /// 2. If the WAL exists, replay every record into the
    ///    restored engine — the same recovery story
    ///    `spg-server` uses.
    /// 3. Open the WAL in append+sync mode so subsequent
    ///    `execute()` writes durably commit (one fsync per
    ///    mutation).
    ///
    /// `Drop` writes a final catalog snapshot + truncates the
    /// WAL — operators that need a sync barrier at a specific
    /// point use `checkpoint()` explicitly.
    pub fn open_path(db_path: impl AsRef<Path>) -> Result<Self, EngineError> {
        let db_path = db_path.as_ref().to_path_buf();
        // v7.19 — WAL is a directory of chunk files. Legacy
        // single-file path stays variable-named `wal_path` for
        // the backward-compat migration block below.
        let wal_path = {
            let mut p = db_path.clone();
            let name = p
                .file_name()
                .map(|n| {
                    let mut s = n.to_os_string();
                    s.push(".wal");
                    s
                })
                .unwrap_or_else(|| std::ffi::OsString::from(".wal"));
            p.set_file_name(name);
            p
        };
        let wal_dir = wal_path.clone();
        if let Some(parent) = db_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
        // v7.17.0 Phase 6.2 — acquire cross-process exclusion
        // lock before touching any catalog / WAL bytes. atomic
        // mkdir on every supported platform; a second process
        // opening the same path while the first is still alive
        // hits the create_dir failure and gets a clear error.
        let lock_path = {
            let mut p = db_path.clone();
            let name = p
                .file_name()
                .map(|n| {
                    let mut s = n.to_os_string();
                    s.push(".lock");
                    s
                })
                .unwrap_or_else(|| std::ffi::OsString::from(".lock"));
            p.set_file_name(name);
            p
        };
        acquire_path_lock(&lock_path)?;
        let mut engine = if db_path.exists() {
            let bytes = std::fs::read(&db_path).map_err(io_err)?;
            let engine = Engine::restore_envelope(&bytes).map_err(|e| {
                EngineError::Storage(spg_storage::StorageError::Corrupt(format!(
                    "restore from {}: {e}",
                    db_path.display()
                )))
            })?;
            engine_with_query_byte_budget(engine.with_clock(wall_clock_micros))
        } else {
            engine_with_query_byte_budget(Engine::new().with_clock(wall_clock_micros))
        };
        // v7.1.4 — manifest-driven cold-segment reload. The
        // manifest sidecar pairs the catalog snapshot CRC with a
        // list of `(segment_id, path, crc32)` triples; verify
        // before loading so a torn or stale manifest doesn't
        // surface phantom data.
        let cold_segments_dir = {
            let parent = db_path.parent().unwrap_or_else(|| Path::new("."));
            let stem = db_path
                .file_stem()
                .unwrap_or_else(|| std::ffi::OsStr::new("db"))
                .to_string_lossy()
                .into_owned();
            parent.join(format!("{stem}.spg")).join("segments")
        };
        let mut cold_segment_paths: BTreeMap<u32, PathBuf> = BTreeMap::new();
        let manifest_pth = spg_manifest_path(&db_path);
        if manifest_pth.exists() && db_path.exists() {
            let m_bytes = std::fs::read(&manifest_pth).map_err(io_err)?;
            if let Ok(m) = CatalogManifest::deserialize(&m_bytes) {
                let snap_bytes = std::fs::read(&db_path).map_err(io_err)?;
                let snap_crc = spg_crypto::crc32::crc32(&snap_bytes);
                if snap_crc == m.catalog_crc32 {
                    for entry in &m.cold_segments {
                        if let Ok(seg_bytes) = std::fs::read(&entry.path) {
                            let computed = spg_crypto::crc32::crc32(&seg_bytes);
                            if computed != entry.crc32 {
                                eprintln!(
                                    "spg-embedded: manifest skip segment {}: CRC mismatch",
                                    entry.segment_id
                                );
                                continue;
                            }
                            if engine.catalog().cold_segment(entry.segment_id).is_some() {
                                // Already loaded via Catalog::clone path (shouldn't happen
                                // since Engine::new + restore_envelope don't populate cold).
                                continue;
                            }
                            let mut new_cat = engine.catalog().clone();
                            if let Err(e) =
                                new_cat.load_segment_bytes_at(entry.segment_id, seg_bytes)
                            {
                                eprintln!(
                                    "spg-embedded: manifest load segment {} failed: {e}",
                                    entry.segment_id
                                );
                                continue;
                            }
                            engine.replace_catalog(new_cat);
                            cold_segment_paths.insert(entry.segment_id, entry.path.clone());
                        } else {
                            eprintln!(
                                "spg-embedded: manifest skip segment {}: file unreadable",
                                entry.segment_id
                            );
                        }
                    }
                }
            }
        }
        // CoW-4 (v7.34) — D10 + missing-manifest fallback. Walk
        // `<db>.spg/segments/` and attach any `seg_<id>.spg` file that
        // the manifest didn't already cover (manifest absent / CRC
        // mismatched / a fresher freeze landed after the last
        // checkpoint wrote its manifest). The segment binary's own
        // magic + CRC32 guards integrity — no need to trust a stale
        // manifest entry to trust the file.
        scan_cold_segments_dir(&cold_segments_dir, &mut engine, &mut cold_segment_paths);
        // v7.19 — chunked WAL on-disk layout.
        //
        // Three cases handled here:
        //
        // 1. wal_dir exists as a DIRECTORY → scan its
        //    `<unix_us>_<leading_lsn>.wal` chunks (sorted
        //    lexicographically = chunk-creation order), replay
        //    them in sequence, advance the LSN watermark to the
        //    max commit_lsn seen.
        //
        // 2. wal_path exists as a FILE → legacy v7.18 layout.
        //    Migrate it: create `wal_dir/`, move the single file
        //    inside as `0000000000000000_0000000000000000.wal`,
        //    then fall through to case 1's replay loop.
        //
        // 3. Neither exists → fresh database; create wal_dir.
        let mut initial_lsn: u64 = 0;
        if wal_path.is_file() {
            // Case 2: legacy single-file WAL migration.
            let legacy_bytes = std::fs::read(&wal_path).map_err(io_err)?;
            std::fs::remove_file(&wal_path).map_err(io_err)?;
            std::fs::create_dir_all(&wal_dir).map_err(io_err)?;
            if !legacy_bytes.is_empty() {
                let migrated = wal_dir.join(legacy_chunk_filename());
                std::fs::write(&migrated, &legacy_bytes).map_err(io_err)?;
            }
        } else if !wal_dir.exists() {
            // Case 3: fresh database.
            std::fs::create_dir_all(&wal_dir).map_err(io_err)?;
        }
        // Cases 1 + 2 share replay logic now that wal_dir is
        // guaranteed to exist (and may be empty for case 3).
        //
        // Two-pass replay so we don't double-apply records the
        // snapshot already reflects:
        //
        // 1. Find the highest commit_lsn carried by a
        //    checkpoint_marker across all chunks. That LSN is the
        //    snapshot's high-water mark — anything ≤ it is
        //    already in `<db_path>` and replaying it would
        //    DuplicateTable / double-insert.
        // 2. Replay only records strictly above that LSN.
        //
        // Case 2 migration (legacy single-file WAL) lands here
        // too: the migrated chunk has no marker so the LSN floor
        // is 0 and every record applies — exactly the v7.18
        // behaviour the migration is supposed to preserve.
        let chunk_paths = sorted_wal_chunks(&wal_dir).map_err(io_err)?;
        let mut snapshot_lsn: u64 = 0;
        for chunk in &chunk_paths {
            let bytes = std::fs::read(chunk).map_err(io_err)?;
            if let Ok(records) = parse_wal_records(&bytes) {
                for r in &records {
                    if r.type_byte == WAL_V4_TYPE_CHECKPOINT_MARKER {
                        if let Some(l) = r.commit_lsn {
                            if l > snapshot_lsn {
                                snapshot_lsn = l;
                            }
                        }
                    }
                }
            }
        }
        let mut quarantined: Vec<QuarantinedStmt> = Vec::new();
        for chunk in &chunk_paths {
            let bytes = std::fs::read(chunk).map_err(io_err)?;
            if bytes.is_empty() {
                continue;
            }
            replay_wal_filtered(&bytes, &mut engine, snapshot_lsn, &mut quarantined)
                .map_err(|m| EngineError::Storage(spg_storage::StorageError::Corrupt(m)))?;
            if let Ok(records) = parse_wal_records(&bytes) {
                if let Some(max) = records.iter().filter_map(|r| r.commit_lsn).max() {
                    if max > initial_lsn {
                        initial_lsn = max;
                    }
                }
            }
        }
        // v7.30.1 (mailrs round-24 ask 2) — replay rejects no longer
        // brick the open. Persist the rejected statements beside the
        // WAL chunks for forensics and say so loudly; the boot
        // continues with every other record applied.
        if !quarantined.is_empty() {
            let mut body = String::new();
            for q in &quarantined {
                body.push_str(&format_quarantine_line(q));
            }
            let qpath = wal_dir.join(format!(
                "quarantine-{:016x}.log",
                wall_clock_micros().max(0) as u64
            ));
            match std::fs::write(&qpath, &body) {
                Ok(()) => eprintln!(
                    "spg-embedded: WAL replay quarantined {} statement(s) — boot continues; \
                     forensics at {}",
                    quarantined.len(),
                    qpath.display()
                ),
                Err(e) => eprintln!(
                    "spg-embedded: WAL replay quarantined {} statement(s) — boot continues; \
                     quarantine file write FAILED ({e}), entries follow:\n{body}",
                    quarantined.len()
                ),
            }
        }
        // Open the "current" chunk — either the last existing
        // chunk file (so subsequent appends extend it until the
        // size threshold rotates) or a fresh first chunk.
        let now_us = wall_clock_micros();
        let current_chunk_path = if let Some(last) = chunk_paths.last() {
            last.clone()
        } else {
            wal_dir.join(chunk_filename(now_us, initial_lsn + 1))
        };
        let wal_file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&current_chunk_path)
            .map_err(io_err)?;
        // Persist the (possibly freshly created) chunk's directory entry.
        fsync_dir(&wal_dir);
        let wal_len = wal_file.metadata().map_err(io_err)?.len();
        let wal = Arc::new(WalGroup::new(wal_file, wal_len));
        // v7.19 P3 — spawn retention sweep thread when the
        // operator opted in via SPG_PITR_RETENTION_HOURS > 0.
        // Otherwise stay on the v7.18 behaviour (chunks accumulate
        // until something else — backup-pitr archival, manual
        // cleanup — moves them).
        let retention_hours = pitr_retention_hours();
        let (retention_shutdown, retention_thread) = if retention_hours > 0 {
            let shutdown = Arc::new(AtomicBool::new(false));
            let shutdown_clone = Arc::clone(&shutdown);
            let wal_dir_clone = wal_dir.clone();
            let check_interval = std::time::Duration::from_secs(pitr_retention_check_sec());
            let archive_cmd = pitr_archive_cmd();
            let handle = std::thread::Builder::new()
                .name("spg-pitr-retention".into())
                .spawn(move || {
                    retention_sweep_loop(
                        wal_dir_clone,
                        retention_hours,
                        check_interval,
                        archive_cmd,
                        shutdown_clone,
                    );
                })
                .map_err(io_err)?;
            (Some(shutdown), Some(handle))
        } else {
            (None, None)
        };
        // v7.20 — background flusher for SPG_SYNCHRONOUS_COMMIT=off.
        let (flusher_shutdown, flusher_thread) = if synchronous_commit_on() {
            (None, None)
        } else {
            let shutdown = Arc::new(AtomicBool::new(false));
            let shutdown_clone = Arc::clone(&shutdown);
            let group = Arc::clone(&wal);
            let interval = std::time::Duration::from_millis(wal_writer_delay_ms());
            let handle = std::thread::Builder::new()
                .name("spg-wal-flusher".into())
                .spawn(move || {
                    while !shutdown_clone.load(Ordering::SeqCst) {
                        std::thread::sleep(interval);
                        if let Err(e) = group.flush_now() {
                            eprintln!("spg-embedded: background WAL flush failed: {e:?}");
                        }
                    }
                    // Final drain on shutdown signal.
                    let _ = group.flush_now();
                })
                .map_err(io_err)?;
            (Some(shutdown), Some(handle))
        };
        // v7.34 (crash-recovery P0 #2) — arm row-level redo capture for
        // subsequent writes (AFTER replay, so re-executed SQL records
        // don't capture; 0x13 records replay via apply_redo and never do).
        if row_redo_enabled() {
            engine.set_redo_capture(true);
        }
        let db = Self {
            engine,
            commit_lsn: AtomicU64::new(initial_lsn),
            tx_wal: None,
            persistence: Some(PersistenceCtx {
                db_path,
                wal_dir,
                current_chunk_path: Arc::new(Mutex::new(current_chunk_path)),
                wal,
                checkpoint_threshold_bytes: default_checkpoint_threshold_bytes(),
                cold_segments_dir,
                cold_segment_paths,
                lock_path,
                retention_shutdown,
                retention_thread,
                flusher_shutdown,
                flusher_thread,
                checkpoint_worker: Some(CheckpointWorker::spawn()),
            }),
        };
        // v7.37.2 (mailrs prod 7.35 pool-exhaustion incident — surface
        // fix per `feedback-zero-customer-change-warmup-incident`) —
        // automatic cold-tier OS page-cache warm-up so the catalog is
        // fully server-ready on return. The client never sees a SPG-
        // specific call site; `open_path` behaves like PG's "ready to
        // accept queries" semantics. Bounded by
        // `SPG_WARM_UP_COLD_BUDGET_MS` (default unset = no cap;
        // env-only spec channel, never a client-visible API). `0` =
        // skip warm-up entirely (escape hatch for fast restart).
        autowarm_cold_tier_on_open(&db);
        Ok(db)
    }

    /// v7.1.4 — freeze the oldest `max_rows` of `table_name`'s
    /// hot tier into a brand-new cold-tier segment + persist
    /// it to disk. Same semantics as `spg-server`'s freezer
    /// thread; embedded just runs the freeze synchronously on
    /// the caller's thread. Persistence + manifest update
    /// happen as part of the next `checkpoint()` (or on Drop).
    pub fn freeze_oldest_to_cold(
        &mut self,
        table_name: &str,
        index_name: &str,
        max_rows: usize,
    ) -> Result<spg_storage::FreezeReport, EngineError> {
        let report = self
            .engine
            .freeze_oldest_to_cold(table_name, index_name, max_rows)?;
        if let Some(p) = &mut self.persistence {
            std::fs::create_dir_all(&p.cold_segments_dir).map_err(io_err)?;
            let final_path = p
                .cold_segments_dir
                .join(format!("seg_{}.spg", report.segment_id));
            let tmp_path = p
                .cold_segments_dir
                .join(format!("seg_{}.spg.tmp", report.segment_id));
            std::fs::write(&tmp_path, &report.segment_bytes).map_err(io_err)?;
            std::fs::rename(&tmp_path, &final_path).map_err(io_err)?;
            p.cold_segment_paths.insert(report.segment_id, final_path);
        }
        Ok(report)
    }

    /// v7.1 — override the auto-checkpoint WAL-size ceiling for
    /// this `Database` instance. Default is
    /// `SPG_EMBEDDED_CHECKPOINT_BYTES` env (4 MiB if unset); the
    /// setter wins. No-op when the database is in-memory.
    pub fn set_checkpoint_threshold_bytes(&mut self, bytes: u64) {
        if let Some(p) = &mut self.persistence {
            p.checkpoint_threshold_bytes = bytes.max(1);
        }
    }

    /// v7.31 (memory campaign, round-26 ask 1/ask 4) — per-bucket
    /// memory snapshot for the embedding host. Poll it from prod to
    /// see where resident bytes live (rows / representation /
    /// indexes per table) and to drive host-side shedding before
    /// the kernel does it. Same numbers as the server path's
    /// `SELECT * FROM spg_memory_stats`.
    #[must_use]
    pub fn memory_stats(&self) -> spg_engine::MemoryStats {
        let mut stats = self.engine.memory_stats();
        // v7.31 C2 — fill in bucket D: the engine leaves `wal_bytes`
        // None (it has no WAL); we report the live (uncheckpointed)
        // WAL footprint via the same `written_len()` meter `metrics()`
        // reads. In-memory databases have no persistence → stays None.
        if let Some(p) = &self.persistence {
            stats.wal_bytes = Some(p.wal.written_len());
        }
        stats
    }

    /// v7.1 — flush a fresh catalog snapshot to `db_path` and
    /// rotate the WAL. Idempotent; cheap when nothing has happened
    /// since the last checkpoint. No-op when the database is in-memory.
    ///
    /// CoW-2 (v7.34): the heavy half (serialize + tmp+rename + fsync +
    /// marker enqueue + chunk rotation) runs on a dedicated worker thread
    /// so the caller's engine borrow is released after the cheap capture
    /// step. This entry point keeps the **synchronous** contract — it
    /// waits for the worker to finish before returning — so existing
    /// callers, tests, and operator scripts see no behaviour change;
    /// they just pay one extra hop. The non-blocking variant lives at
    /// `trigger_checkpoint`, used by the auto-checkpoint hot path so
    /// the write that crossed `SPG_EMBEDDED_CHECKPOINT_BYTES` doesn't
    /// stall on disk IO.
    ///
    /// Called automatically when:
    /// - the WAL grows past `SPG_EMBEDDED_CHECKPOINT_BYTES` (default
    ///   4 MiB) at the end of an `execute()` (via `trigger_checkpoint`,
    ///   non-blocking), and
    /// - `Drop` runs (synchronous; best-effort, failures logged).
    pub fn checkpoint(&mut self) -> Result<(), EngineError> {
        if self.persistence.is_none() {
            return Ok(());
        }
        // Drain any prior async checkpoint first so our snapshot reflects
        // post-it state (and so a sticky error from it surfaces here, not
        // smeared across the next two `wait`s).
        self.wait_checkpoint()?;
        let Some(job) = self.snapshot_checkpoint_job() else {
            return Ok(());
        };
        let Some(worker) = self
            .persistence
            .as_ref()
            .and_then(|p| p.checkpoint_worker.as_ref())
        else {
            return Ok(());
        };
        // `wait_checkpoint` above guaranteed idle; `try_enqueue` only
        // returns Ok(false) when busy, so we expect Ok(true) here. The
        // bool is dropped — we wait unconditionally to honour the sync
        // contract.
        let _ = worker.try_enqueue(job)?;
        self.wait_checkpoint()
    }

    /// CoW-2 (v7.34) — non-blocking checkpoint trigger used by the
    /// auto-checkpoint hot path (`wal_after_ok` over the threshold).
    /// Captures the engine state under `&mut self` then signals the
    /// background worker and returns; the serialize / fsync / rotate
    /// sequence runs on the worker thread. If a checkpoint is already
    /// pending or in flight, the new trigger is silently dropped —
    /// the next threshold crossing picks up the newer state.
    ///
    /// Sticky errors from a prior async run surface here (via
    /// `try_enqueue`), so a failed background checkpoint still reaches
    /// the caller eventually rather than vanishing.
    fn trigger_checkpoint(&mut self) -> Result<(), EngineError> {
        if self.persistence.is_none() {
            return Ok(());
        }
        let Some(job) = self.snapshot_checkpoint_job() else {
            return Ok(());
        };
        let Some(worker) = self
            .persistence
            .as_ref()
            .and_then(|p| p.checkpoint_worker.as_ref())
        else {
            return Ok(());
        };
        let _accepted = worker.try_enqueue(job)?;
        Ok(())
    }

    /// CoW-2 (v7.34) — block until the background checkpoint worker is
    /// idle. Used by sync `checkpoint()` and by Drop to ensure the final
    /// snapshot is durable before the process exits.
    fn wait_checkpoint(&self) -> Result<(), EngineError> {
        match self
            .persistence
            .as_ref()
            .and_then(|p| p.checkpoint_worker.as_ref())
        {
            Some(w) => w.wait(),
            None => Ok(()),
        }
    }

    /// CoW-2 (v7.34) — capture a checkpoint job under `&mut self` (or
    /// `&self`, since reading from atomics + cheap clones don't mutate).
    /// Returns `None` if the database is in-memory.
    fn snapshot_checkpoint_job(&self) -> Option<CheckpointJob> {
        let p = self.persistence.as_ref()?;
        Some(CheckpointJob {
            snapshot: self.engine.snapshot_data(),
            marker_lsn: self.commit_lsn.load(Ordering::SeqCst),
            db_path: p.db_path.clone(),
            wal_dir: p.wal_dir.clone(),
            wal: Arc::clone(&p.wal),
            cold_segments: p
                .cold_segment_paths
                .iter()
                .map(|(&id, path)| (id, path.clone()))
                .collect(),
            current_chunk_path: Arc::clone(&p.current_chunk_path),
        })
    }

    /// Restore a database from a previously-captured catalog
    /// snapshot. Pairs with `Database::snapshot()` for
    /// round-tripping in-memory state without going through
    /// the `spg-server` WAL.
    pub fn restore(snapshot: &[u8]) -> Result<Self, EngineError> {
        let engine = Engine::restore_envelope(snapshot).map_err(|e| {
            EngineError::Storage(spg_storage::StorageError::Corrupt(format!("restore: {e}")))
        })?;
        let db = Self {
            engine,
            persistence: None,
            commit_lsn: AtomicU64::new(0),
            tx_wal: None,
        };
        // v7.37.2 — auto-warm on snapshot restore for the same reason
        // `open_path` does (catalog is server-ready when constructor
        // returns; client never sees a SPG-specific warmup call).
        autowarm_cold_tier_on_open(&db);
        Ok(db)
    }

    /// Take a catalog snapshot suitable for `Database::restore`.
    /// The bytes are SPG's canonical catalog envelope (FILE_MAGIC
    /// + version + payload); round-trips through every released
    /// SPG version per the STABILITY contract.
    #[must_use]
    pub fn snapshot(&self) -> Vec<u8> {
        self.engine.snapshot()
    }

    /// v7.36 (mailrs ask #4) — programmatic `EXPLAIN` over `sql`,
    /// returning each line of the QUERY PLAN as an owned `String`.
    /// Skips the WAL (`EXPLAIN` is read-only) and runs against the
    /// engine's live catalog. Dogfood callers can attach the plan
    /// to a report or assert on its shape from a test without
    /// having to parse a tabular result themselves.
    ///
    /// `sql` is the inner SELECT (no `EXPLAIN` prefix); the helper
    /// adds it. For SQL with `$N` placeholders, substitute them
    /// into the SQL string before calling — programmatic
    /// placeholder-aware EXPLAIN is on the v7.37 plan.
    ///
    /// # Errors
    /// Propagates parse errors on `sql`, plus any engine error the
    /// `EXPLAIN` itself raises (table not found, column not found).
    pub fn explain(&self, sql: &str) -> Result<Vec<String>, EngineError> {
        let full = format!("EXPLAIN {sql}");
        let result = self.engine.execute_readonly(&full)?;
        Ok(extract_query_plan_lines(result))
    }

    /// Write-side single-statement execute. Runs the SQL through
    /// the buffered group-commit pipeline and blocks until the
    /// resulting batch's WAL fsync returns. Read-only statements
    /// (SELECT / SHOW / EXPLAIN / BEGIN-COMMIT-ROLLBACK /
    /// CHECKPOINT / COMPACT etc.) skip the WAL entirely.
    pub fn execute(&mut self, sql: &str) -> Result<QueryResult, EngineError> {
        // v7.20 P2 — single-caller convenience over the buffered
        // path: enqueue + immediately wait. Batch size is 1 here,
        // so the durability behaviour (one fsync before Ok) is
        // identical to v7.19. Concurrent callers go through
        // `execute_buffered` (AsyncDatabase does) and share the
        // leader's fsync.
        let (result, ticket) = self.execute_buffered(sql)?;
        if let Some(t) = ticket {
            t.wait()?;
        }
        Ok(result)
    }

    /// v7.20 P2 — group-commit write entry. Runs the engine
    /// mutation + encodes/enqueues the WAL record, then RETURNS
    /// WITHOUT waiting for the fsync. The caller must call
    /// [`WalTicket::wait`] before treating the write as durable
    /// — crucially, the caller can (and should) drop whatever
    /// lock guards this `Database` first, so the next writer's
    /// mutation overlaps this batch's fsync.
    ///
    /// `None` ticket = nothing hit the WAL (read-only statement,
    /// no-op DDL, or in-memory database) — the result is final
    /// as returned.
    ///
    /// # Errors
    /// Engine errors propagate unchanged. Auto-checkpoint (when
    /// the active chunk crosses the threshold) runs inline and
    /// may surface IO errors.
    pub fn execute_buffered(
        &mut self,
        sql: &str,
    ) -> Result<(QueryResult, Option<WalTicket>), EngineError> {
        let result = self.engine.execute(sql)?;
        let modified = matches!(
            &result,
            QueryResult::CommandOk {
                modified_catalog: true,
                ..
            }
        );
        let ticket = self.wal_after_ok(sql, modified)?;
        Ok((result, ticket))
    }

    /// v7.21 (round-12 polish) — post-engine WAL bookkeeping shared
    /// by the simple ([`Self::execute_buffered`]) and prepared
    /// ([`Self::execute_prepared_buffered`]) write paths. `canonical`
    /// is the replay text (bind-final for prepared statements);
    /// `modified_catalog` comes from the engine result. Three routes:
    ///
    /// - transaction control → maintain [`Self::tx_wal`]: BEGIN opens
    ///   the buffer, COMMIT flushes it as ONE atomic
    ///   `WAL_V4_TYPE_TX_COMMIT_SQL` record, ROLLBACK drops it,
    ///   SAVEPOINT / ROLLBACK TO mark / truncate it. The engine has
    ///   already accepted the statement, so this only mirrors state.
    /// - inside an open transaction → buffer the statement (shadow-
    ///   catalog mutations report `modified_catalog: false`, so the
    ///   auto-commit arm below can't see them).
    /// - auto-commit mutation → classic per-statement v4 record.
    ///
    /// v7.18 PITR — v4 records carry commit LSN + wall-clock micros.
    /// The crash window remains one BATCH: replay re-applies
    /// idempotently exactly as before, and a torn batch tail drops
    /// cleanly (same torn-write handling).
    fn wal_after_ok(
        &mut self,
        canonical: &str,
        modified_catalog: bool,
    ) -> Result<Option<WalTicket>, EngineError> {
        if self.persistence.is_none() {
            return Ok(None);
        }
        let mut record = None;
        match tx_control_kind(canonical) {
            Some(TxControl::Begin) => {
                self.tx_wal = Some(TxWalBuffer::default());
            }
            Some(TxControl::Commit) => {
                if let Some(buf) = self.tx_wal.take()
                    && !buf.statements.is_empty()
                {
                    let script = buf.statements.join(";\n");
                    let lsn = self.commit_lsn.fetch_add(1, Ordering::SeqCst) + 1;
                    record = Some(encode_v4_tx_commit(&script, lsn, wall_clock_micros()));
                }
            }
            Some(TxControl::Rollback) => {
                self.tx_wal = None;
            }
            Some(TxControl::Savepoint(name)) => {
                if let Some(buf) = &mut self.tx_wal {
                    // PG name-reuse semantics: latest mark wins.
                    buf.savepoints.retain(|(n, _)| n != &name);
                    let mark = buf.statements.len();
                    buf.savepoints.push((name, mark));
                }
            }
            Some(TxControl::RollbackToSavepoint(name)) => {
                if let Some(buf) = &mut self.tx_wal
                    && let Some(pos) = buf.savepoints.iter().position(|(n, _)| n == &name)
                {
                    let mark = buf.savepoints[pos].1;
                    buf.statements.truncate(mark);
                    // Later savepoints die with the rollback; the
                    // target itself survives (PG keeps it
                    // re-rollbackable).
                    buf.savepoints.truncate(pos + 1);
                }
            }
            Some(TxControl::ReleaseSavepoint) => {
                // RELEASE folds the savepoint into the enclosing tx —
                // buffered statements stay. The mark also stays:
                // marks are only consulted by ROLLBACK TO, which the
                // engine validates first, so a dangling mark is
                // unreachable.
            }
            None => {
                if let Some(buf) = &mut self.tx_wal {
                    if !sql_is_read_only(canonical) {
                        buf.statements.push(canonical.to_string());
                    }
                } else if modified_catalog && !sql_is_read_only(canonical) {
                    let lsn = self.commit_lsn.fetch_add(1, Ordering::SeqCst) + 1;
                    // v7.34 (crash-recovery P0 #2) — hybrid log: when
                    // row-level redo is on and this statement produced row
                    // changes (DML), write a physical 0x13 redo record so
                    // replay applies it directly. A statement with no row
                    // changes (DDL: CREATE/ALTER, never goes through
                    // Table::insert/update/delete) drains an empty redo and
                    // keeps the SQL record so the schema still replays.
                    let redo = if row_redo_enabled() {
                        self.engine.take_redo()
                    } else {
                        Vec::new()
                    };
                    record = Some(if redo.is_empty() {
                        encode_v4_auto_commit(canonical, lsn, wall_clock_micros())
                    } else {
                        encode_v5_row_redo(
                            &spg_storage::encode_redo_log(&redo),
                            lsn,
                            wall_clock_micros(),
                        )
                    });
                }
            }
        }
        let mut ticket = None;
        if let Some(record) = record {
            let p = self.persistence.as_mut().expect("checked above");
            let seq = p.wal.enqueue(&record);
            ticket = Some(WalTicket {
                group: Arc::clone(&p.wal),
                seq,
            });
            if p.wal.written_len() >= p.checkpoint_threshold_bytes {
                // CoW-2 (v7.34): hot path — fire-and-forget. The worker
                // serializes off this thread so the commit that just
                // crossed the threshold doesn't stall on a multi-hundred-ms
                // snapshot write. Any sticky error from a prior async
                // checkpoint surfaces here.
                self.trigger_checkpoint()?;
            }
        }
        Ok(ticket)
    }

    /// v7.3.0 — typed-row variant of [`Database::query`]. Each
    /// row decodes into a `T: FromSpgRow` so callers don't
    /// pattern-match on `Value` themselves. Use [`spg_row!`] to
    /// generate the impl, or write it by hand.
    pub fn query_typed<T: FromSpgRow>(&mut self, sql: &str) -> Result<Vec<T>, EngineError> {
        let rows = self.query(sql)?;
        rows.into_iter().map(|r| T::from_spg_row(&r)).collect()
    }

    /// Run a SELECT and return rows as a `Vec<Vec<Value>>` —
    /// strips the column-schema metadata for read-side
    /// ergonomics. Errors on non-Rows results (DML / DDL
    /// statements should go through `execute` instead).
    pub fn query(&mut self, sql: &str) -> Result<Vec<Vec<Value>>, EngineError> {
        match self.engine.execute(sql)? {
            QueryResult::Rows { rows, .. } => Ok(rows.into_iter().map(|r| r.values).collect()),
            QueryResult::CommandOk { .. } => Err(EngineError::Unsupported(
                "query() expects a SELECT — use execute() for DML/DDL".into(),
            )),
            // v7.5.0 — QueryResult is #[non_exhaustive]; any future
            // variant is not a SELECT row stream, treat as Unsupported.
            _ => Err(EngineError::Unsupported(
                "query() expects a SELECT — use execute() for DML/DDL".into(),
            )),
        }
    }

    /// v7.16.0 — column-aware variant of [`Self::query`].
    /// Returns the column schema vec alongside the rows so
    /// adapters (the spg-sqlx Row impl most notably) can drive
    /// name + type-based column lookups. Errors on non-Rows
    /// results identically to `query`.
    pub fn query_with_columns(
        &mut self,
        sql: &str,
    ) -> Result<(Vec<spg_storage::ColumnSchema>, Vec<Vec<Value>>), EngineError> {
        match self.engine.execute(sql)? {
            QueryResult::Rows { columns, rows } => {
                Ok((columns, rows.into_iter().map(|r| r.values).collect()))
            }
            QueryResult::CommandOk { .. } => Err(EngineError::Unsupported(
                "query_with_columns() expects a SELECT — use execute() for DML/DDL".into(),
            )),
            _ => Err(EngineError::Unsupported(
                "query_with_columns() expects a SELECT — use execute() for DML/DDL".into(),
            )),
        }
    }

    /// v7.16.0 — column-aware variant of
    /// [`Self::query_prepared`]. Same shape as
    /// `query_with_columns` but driven from a prepared
    /// statement + bound params.
    pub fn query_prepared_with_columns(
        &mut self,
        stmt: &Statement,
        params: &[Value],
    ) -> Result<(Vec<spg_storage::ColumnSchema>, Vec<Vec<Value>>), EngineError> {
        match self.engine.execute_prepared(stmt.stmt.clone(), params)? {
            QueryResult::Rows { columns, rows } => {
                Ok((columns, rows.into_iter().map(|r| r.values).collect()))
            }
            QueryResult::CommandOk { .. } => Err(EngineError::Unsupported(
                "query_prepared_with_columns() expects a SELECT — use execute_prepared() for DML/DDL".into(),
            )),
            _ => Err(EngineError::Unsupported(
                "query_prepared_with_columns() expects a SELECT — use execute_prepared() for DML/DDL".into(),
            )),
        }
    }

    /// Borrow the underlying engine. Escape hatch for callers
    /// that need access to `spg-engine` APIs not yet surfaced
    /// here (transactions, EXPLAIN ANALYZE, etc.).
    #[must_use]
    pub const fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Mutable borrow of the underlying engine. Same intent as
    /// `engine()` but for write-side APIs (e.g. inserting
    /// directly through `Catalog::insert` for high-throughput
    /// bulk loads that bypass SQL parsing).
    pub const fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }

    /// v7.38 (mailrs prod 7.35 pool-exhaustion incident) — boot-time
    /// plan-IR cache warm-up. Pre-prepares the listed SQL shapes so
    /// the first user-facing request doesn't pay the 2-3 s
    /// first-fire parse + JOIN-reorder cost on the readonly-blocking
    /// pool. Recommended call site: `Database::new` immediately after
    /// catalog restore, before serving any traffic. Returns the
    /// number of statements successfully cached.
    pub fn warm_up_plan_cache(&mut self, sqls: &[&str]) -> usize {
        self.engine.warm_up_plan_cache(sqls)
    }

    /// v7.38 (mailrs prod 7.35 pool-exhaustion incident) — boot-time
    /// cold-tier OS page-cache warm-up. Touches every cold segment
    /// file in the active catalog so the kernel page cache loads
    /// them before user traffic arrives. On a hot-only catalog the
    /// call is a near-no-op. Returns the total cold rows touched.
    pub fn warm_up_cold_tier(&self) -> usize {
        self.engine.warm_up_cold_tier()
    }

    /// v7.16.0 — parse + plan a SQL string ONCE so subsequent
    /// `execute_prepared` / `query_prepared` calls can re-bind
    /// parameters without re-parsing. The returned [`Statement`]
    /// is a thin handle around the AST + cached source SQL; it's
    /// `Clone` so the same plan can drive many bind calls
    /// concurrently (each call clones the AST and runs
    /// placeholder substitution on the clone — the cached
    /// plan stays intact).
    ///
    /// Plan caching follows the engine's existing version-aware
    /// rule: a prepared `Statement` whose statistics version
    /// has rolled (ANALYZE ran between prepare and execute)
    /// will silently re-prepare under the hood. Callers don't
    /// need to detect this.
    ///
    /// Placeholders in the SQL use PG's `$1`, `$2`, … convention.
    /// `bind`-time `Value`s are passed as a slice; arity
    /// mismatches surface as `EvalError::PlaceholderOutOfRange`
    /// at `execute_prepared` time, not here.
    ///
    /// # Errors
    /// Surfaces `EngineError` (parse error / plan rewrite
    /// failure) from the underlying `Engine::prepare`.
    pub fn prepare(&mut self, sql: &str) -> Result<Statement, EngineError> {
        // Use the cached path so repeated prepares of the same
        // SQL are O(1). The engine's plan cache stays shared
        // across all callers of this Database — a single
        // `PgPool`-shaped consumer (or, later, the spg-sqlx
        // adapter) prepares once and reaps the win on every bind.
        let stmt = self
            .engine
            .prepare_cached(sql)
            .map_err(EngineError::Parse)?;
        Ok(Statement {
            stmt,
            sql: sql.to_string(),
        })
    }

    /// v7.17.0 Phase 3.P0-66 — describe a SQL string without
    /// executing. Returns `(parameter_oid_count, output_columns)`
    /// where `output_columns` is empty for non-SELECT statements
    /// or for SELECT shapes the describe planner can't resolve
    /// (JOIN / subquery / unknown table). Wraps
    /// `Engine::describe_prepared` so the spg-sqlx bridge can
    /// surface PG-shape Describe replies for
    /// `sqlx::query!()` compile-time validation.
    ///
    /// # Errors
    /// Propagates parse errors from the underlying prepare path.
    pub fn describe(&mut self, sql: &str) -> Result<(Vec<u32>, Vec<ColumnSchema>), EngineError> {
        let stmt = self
            .engine
            .prepare_cached(sql)
            .map_err(EngineError::Parse)?;
        Ok(self.engine.describe_prepared(&stmt))
    }

    /// v7.16.0 — execute a prepared statement with bound
    /// parameters. Mirrors `Engine::execute_prepared`: clones
    /// the AST, substitutes `$1..$N` → `params[0..N-1]`, runs.
    ///
    /// Persistence (WAL fsync + auto-checkpoint) follows the
    /// same rules as `execute(sql)`: mutating statements get a
    /// WAL record AFTER the in-memory exec succeeds. The WAL
    /// record carries the substituted, bind-final SQL, so
    /// replay reconstructs the same row state without needing
    /// the original prepared `Statement` to still be alive.
    ///
    /// # Errors
    /// Propagates engine errors. Param arity mismatch surfaces
    /// as `EvalError::PlaceholderOutOfRange`.
    pub fn execute_prepared(
        &mut self,
        stmt: &Statement,
        params: &[Value],
    ) -> Result<QueryResult, EngineError> {
        let (result, ticket) = self.execute_prepared_buffered(stmt, params)?;
        if let Some(t) = ticket {
            t.wait()?;
        }
        Ok(result)
    }

    /// v7.20 P2 — group-commit variant of
    /// [`Database::execute_prepared`]. Same contract as
    /// [`Database::execute_buffered`]: mutation + enqueue happen
    /// here; the caller waits on the ticket AFTER releasing
    /// whatever lock guards this `Database`.
    ///
    /// # Errors
    /// Engine errors propagate unchanged; inline auto-checkpoint
    /// may surface IO errors.
    pub fn execute_prepared_buffered(
        &mut self,
        stmt: &Statement,
        params: &[Value],
    ) -> Result<(QueryResult, Option<WalTicket>), EngineError> {
        let result = self.engine.execute_prepared(stmt.stmt.clone(), params)?;
        let modified = matches!(
            &result,
            QueryResult::CommandOk {
                modified_catalog: true,
                ..
            }
        );
        // WAL persistence on the bind-final SQL. Build the
        // canonical Display form by re-printing the
        // placeholder-substituted statement (cheap — the AST
        // is already in hand from execute_prepared's internal
        // clone) so replay's path is identical to the
        // simple-query path. v7.21: also when a transaction is
        // open — in-tx mutations report `modified_catalog: false`
        // but must reach the tx WAL buffer (see `wal_after_ok`).
        let mut ticket = None;
        if self.persistence.is_some()
            && (modified
                || (self.tx_wal.is_some() && !sql_is_read_only(&stmt.sql))
                || tx_control_kind(&stmt.sql).is_some())
        {
            let mut wal_stmt = stmt.stmt.clone();
            crate::wal_render_with_params(&mut wal_stmt, params);
            let canonical = format!("{wal_stmt}");
            ticket = self.wal_after_ok(&canonical, modified)?;
        }
        Ok((result, ticket))
    }

    /// v7.16.0 — run a prepared SELECT with bound params and
    /// return rows as `Vec<Vec<Value>>`, matching `query()`
    /// shape. SELECTs are read-only so this never writes the
    /// WAL.
    ///
    /// # Errors
    /// Returns `Unsupported` if the prepared statement isn't a
    /// SELECT (use `execute_prepared` for DML/DDL).
    pub fn query_prepared(
        &mut self,
        stmt: &Statement,
        params: &[Value],
    ) -> Result<Vec<Vec<Value>>, EngineError> {
        match self.engine.execute_prepared(stmt.stmt.clone(), params)? {
            QueryResult::Rows { rows, .. } => Ok(rows.into_iter().map(|r| r.values).collect()),
            QueryResult::CommandOk { .. } => Err(EngineError::Unsupported(
                "query_prepared() expects a SELECT — use execute_prepared() for DML/DDL".into(),
            )),
            _ => Err(EngineError::Unsupported(
                "query_prepared() expects a SELECT — use execute_prepared() for DML/DDL".into(),
            )),
        }
    }

    /// v7.18 — parse + plan a SQL string against a
    /// `CatalogSnapshot`. Mirror of [`Database::prepare`] for the
    /// readonly fan-out path: no writer lock taken, no WAL write,
    /// no plan-cache mutation. Static-on-`Self` so callers can
    /// dispatch against a snapshot without an `&mut Database`
    /// borrow — `AsyncReadHandle::prepare` in spg-embedded-tokio
    /// is the load-bearing consumer.
    ///
    /// # Errors
    /// Propagates `EngineError::Parse` from the parser.
    pub fn prepare_on_snapshot(
        snapshot: &CatalogSnapshot,
        sql: &str,
    ) -> Result<Statement, EngineError> {
        let stmt =
            spg_engine::Engine::prepare_on_snapshot(snapshot, sql).map_err(EngineError::Parse)?;
        Ok(Statement {
            stmt,
            sql: sql.to_string(),
        })
    }

    /// v7.18 — execute a prepared `Statement` against a
    /// `CatalogSnapshot` with bound params. Mirror of
    /// [`Database::execute_prepared`] on the readonly path:
    /// writes / DDL hit `EngineError::WriteRequired`. No WAL
    /// write, no writer lock, multiple snapshots can run
    /// concurrently — the snapshot is immutable from prepare time.
    ///
    /// # Errors
    /// Surfaces `EngineError::WriteRequired` for non-readonly
    /// statements; propagates other engine errors.
    pub fn execute_prepared_on_snapshot(
        snapshot: &CatalogSnapshot,
        stmt: &Statement,
        params: &[Value],
    ) -> Result<QueryResult, EngineError> {
        spg_engine::Engine::execute_readonly_prepared_on_snapshot(
            snapshot,
            stmt.stmt.clone(),
            params,
        )
    }

    /// v7.28 (round-22) — deadline-bounded variant of
    /// [`Database::execute_prepared_on_snapshot`]. Returns
    /// `EngineError::Cancelled` once the budget elapses; the
    /// sqlx driver uses this to keep readonly-INLINE execution
    /// from monopolising the caller's async runtime (four slow
    /// inbox queries saturated mailrs's whole tokio pool) and
    /// re-runs over the blocking pool on timeout.
    ///
    /// # Errors
    /// `EngineError::Cancelled` on budget expiry; engine errors
    /// otherwise.
    pub fn execute_prepared_on_snapshot_with_budget(
        snapshot: &CatalogSnapshot,
        stmt: &Statement,
        params: &[Value],
        budget_us: u64,
    ) -> Result<QueryResult, EngineError> {
        fn mono_now_us() -> u64 {
            use std::time::{SystemTime, UNIX_EPOCH};
            // Monotonic enough for a per-call relative budget: the
            // engine only compares (now - start) against the budget
            // within one call.
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
                .unwrap_or(0)
        }
        let deadline = mono_now_us().saturating_add(budget_us);
        let token = spg_engine::CancelToken::none().with_deadline(mono_now_us, deadline);
        spg_engine::Engine::execute_readonly_prepared_on_snapshot_with_cancel(
            snapshot,
            stmt.stmt.clone(),
            params,
            token,
        )
    }

    /// v7.18 — describe a SQL string against a
    /// `CatalogSnapshot`. Mirror of [`Database::describe`] on
    /// the readonly path. Pure function on the snapshot's
    /// catalog; safe to call from any thread.
    ///
    /// # Errors
    /// Propagates `EngineError::Parse` from the parser.
    pub fn describe_on_snapshot(
        snapshot: &CatalogSnapshot,
        sql: &str,
    ) -> Result<(Vec<u32>, Vec<ColumnSchema>), EngineError> {
        let stmt =
            spg_engine::Engine::prepare_on_snapshot(snapshot, sql).map_err(EngineError::Parse)?;
        Ok(spg_engine::Engine::describe_prepared_on_snapshot(
            snapshot, &stmt,
        ))
    }

    /// v7.21 (round-12 polish) — run a multi-statement SQL script
    /// with PG simple-query semantics: the statements execute in
    /// order inside ONE implicit transaction, so a mid-script error
    /// rolls back the whole script (PG wraps every simple-query
    /// message in an implicit transaction). Three exceptions, all
    /// PG-faithful:
    ///
    /// - a script that carries its OWN transaction control
    ///   (BEGIN / COMMIT / …) runs statement-by-statement — the
    ///   script owns its boundaries;
    /// - a script run while the caller already has a transaction
    ///   open joins that transaction (no nested BEGIN), and the
    ///   caller's COMMIT / ROLLBACK decides its fate;
    /// - a single-statement script is plain auto-commit.
    ///
    /// Returns one `QueryResult` per executed statement. This is the
    /// engine behind `sqlx::raw_sql` (mailrs feeds whole
    /// `init-schema.sql` files through it) and `spgctl import`.
    ///
    /// # Errors
    /// The first failing statement's error propagates after the
    /// implicit ROLLBACK; nothing from the script remains applied.
    pub fn execute_script(&mut self, sql: &str) -> Result<Vec<QueryResult>, EngineError> {
        let stmts = split_statements(sql);
        let script_owns_tx = stmts.iter().any(|s| tx_control_kind(s).is_some());
        let wrap = stmts.len() > 1 && !script_owns_tx && !self.engine.in_transaction();
        if !wrap {
            let mut out = Vec::with_capacity(stmts.len());
            for stmt in &stmts {
                out.push(self.execute_dump_statement(stmt)?);
            }
            return Ok(out);
        }
        self.execute("BEGIN")?;
        let mut out = Vec::with_capacity(stmts.len());
        for stmt in &stmts {
            match self.execute_dump_statement(stmt) {
                Ok(r) => out.push(r),
                Err(e) => {
                    // Best-effort rollback; surface the script error.
                    let _ = self.execute("ROLLBACK");
                    return Err(e);
                }
            }
        }
        self.execute("COMMIT")?;
        Ok(out)
    }

    /// v7.22 (round-13 T2) — execute one `split_statements` chunk,
    /// lowering a `COPY … FROM stdin;` block (statement + its data
    /// lines, as one chunk) to per-row INSERTs through the shared
    /// `spg_engine::copy` helpers. Default-format pg_dump emits
    /// COPY blocks, so the zero-change import promise needs this on
    /// the embed path; non-COPY statements pass straight through to
    /// [`Self::execute`]. Public so `spgctl import` can keep its
    /// per-statement error indexing while sharing the lowering.
    ///
    /// # Errors
    /// Engine errors propagate; for COPY the failing row's INSERT
    /// error carries the synthesized statement context.
    pub fn execute_dump_statement(&mut self, stmt: &str) -> Result<QueryResult, EngineError> {
        // Strip pg_dump's `-- Data for Name: …;` banner (it carries
        // semicolons of its own) before splitting head from data.
        let stmt_clean = strip_leading_sql_noise(stmt);
        let head_is_copy = stmt_clean
            .get(..4)
            .is_some_and(|p| p.eq_ignore_ascii_case("copy"));
        if head_is_copy
            && let Some((head, data)) = stmt_clean.split_once(';')
            && let Some(spec) = spg_engine::copy::parse_copy_from_stdin_head(head)
        {
            let mut affected: usize = 0;
            for line in data.lines() {
                // Empty fragments only occur at the chunk boundary
                // (the remainder of the COPY line right after `;`);
                // data rows are whole non-empty lines.
                let line = line.strip_suffix('\r').unwrap_or(line);
                if line.is_empty() {
                    continue;
                }
                let values = spg_engine::copy::decode_copy_text_row(line);
                let insert = spg_engine::copy::build_copy_insert(
                    &spec.table,
                    spec.columns.as_deref(),
                    &values,
                );
                match self.execute(&insert)? {
                    QueryResult::CommandOk { affected: n, .. } => affected += n,
                    _ => affected += 1,
                }
            }
            return Ok(QueryResult::CommandOk {
                affected,
                modified_catalog: false,
            });
        }
        self.execute(stmt)
    }

    /// v7.2.0 — run `body` inside an implicit `BEGIN` /
    /// `COMMIT` pair. The body receives `&mut Database` so it
    /// can `execute()` / `query()` like any other code path;
    /// the only difference is that every write in the body
    /// lands inside one transaction, and a returned `Err` from
    /// the body triggers `ROLLBACK` before the error propagates.
    ///
    /// Nested calls are not supported — SPG's transaction
    /// model is single-writer with explicit `BEGIN` /
    /// `COMMIT` / `ROLLBACK`, and a nested `with_transaction`
    /// would hit `EngineError::Unsupported("nested
    /// transaction")` at the inner `BEGIN`.
    pub fn with_transaction<R, F>(&mut self, body: F) -> Result<R, EngineError>
    where
        F: FnOnce(&mut Self) -> Result<R, EngineError>,
    {
        self.execute("BEGIN")?;
        match body(self) {
            Ok(value) => {
                self.execute("COMMIT")?;
                Ok(value)
            }
            Err(e) => {
                // Best-effort rollback. If ROLLBACK itself
                // fails (rare — the engine reports it via
                // `Unsupported` only when there's no active
                // TX, which can't happen here) we surface the
                // original body error, not the rollback error.
                let _ = self.execute("ROLLBACK");
                Err(e)
            }
        }
    }
}

impl Default for Database {
    fn default() -> Self {
        Self::open_in_memory()
    }
}

/// v7.7.5 — observability snapshot returned by
/// [`Database::metrics`]. Plain data, no allocations beyond
/// what the struct itself takes; cheap to construct and
/// cheap to serialise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct EmbeddedMetrics {
    /// Total live row count across every user table (hot
    /// tier only — cold-tier rows live in segment files).
    pub hot_rows: u64,
    /// Sum of `Table::hot_bytes` across every user table.
    /// Tracks against the freezer's `hot_tier_bytes` budget.
    pub hot_bytes: u64,
    /// Number of cold-tier segments registered in the catalog.
    /// Includes tombstoned slots (segments retired by
    /// compaction whose disk file may still be on disk).
    pub cold_segments: u64,
    /// User-table count (excludes any future engine-managed
    /// internal tables).
    pub tables: u64,
    /// WAL size at last `execute()` / `checkpoint()`. Zero
    /// when the database is in-memory.
    pub wal_bytes: u64,
    /// `true` when the database was opened with `open_path` —
    /// i.e. WAL + checkpoint persistence is active.
    pub persistent: bool,
}

/// v7.2.1 — handle returned by `spawn_background_freezer`.
/// Drop signals the worker thread to wind down + joins it,
/// so a `Database` (or its shared `Arc<Mutex<Database>>`)
/// can safely drop after the handle does.
#[must_use = "the background freezer keeps running until this handle is dropped"]
#[derive(Debug)]
pub struct FreezerHandle {
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl FreezerHandle {
    /// v7.2.1 — request the worker stop + join. Idempotent;
    /// safe to call from `Drop` (which also calls it).
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }
}

impl Drop for FreezerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// v7.2.1 — knobs for `Database::spawn_background_freezer`.
#[derive(Debug, Clone)]
pub struct FreezerOptions {
    /// Tick interval. Worker wakes every `tick`, checks the
    /// catalog's `hot_tier_bytes`, and freezes if over budget.
    pub tick: Duration,
    /// Hot-tier byte budget. Exceeded → next tick freezes the
    /// largest table's oldest `batch_rows` rows into a new
    /// cold segment.
    pub hot_tier_bytes: u64,
    /// Max rows the freezer demotes per fire.
    pub batch_rows: usize,
    /// v7.7.4 — auto-compact threshold. When the catalog has
    /// at least this many cold segments across all tables, the
    /// freezer fires a compaction pass after its next freeze.
    /// Set to `usize::MAX` to disable auto-compact entirely;
    /// the default is `64`, matching the `spg-server` operating
    /// point for SPG_COLD_COMPACT_SEGMENT_THRESHOLD.
    pub compact_when_segments_exceed: usize,
    /// v7.7.4 — target segment size for compaction merges,
    /// in bytes. Default 64 MiB, mirroring `spg-server`. Small
    /// segments below this size are merge candidates;
    /// segments at or above stay untouched.
    pub compact_target_bytes: u64,
}

impl Default for FreezerOptions {
    fn default() -> Self {
        // Match the `spg-server` freezer's default operating
        // point (SPG_HOT_TIER_BYTES = 4 GiB, batch 1000 rows,
        // tick every 1 s) so embedded behaviour is predictable
        // for operators familiar with the server.
        Self {
            tick: Duration::from_secs(1),
            hot_tier_bytes: 4 * 1024 * 1024 * 1024,
            batch_rows: 1000,
            compact_when_segments_exceed: 64,
            compact_target_bytes: 64 * 1024 * 1024,
        }
    }
}

impl Database {
    /// v7.7.4 — observe the catalog's cold-segment count.
    /// Useful for tests + dashboards that want to verify
    /// auto-compaction is firing.
    #[must_use]
    pub fn cold_segment_count(&self) -> usize {
        self.engine.catalog().cold_segment_count()
    }

    /// v7.7.5 — observability snapshot. Returns a point-in-time
    /// view of the engine + persistence counters. Cheap (no
    /// locks beyond the existing `&self` borrow), so safe to
    /// call from a hot metrics-scrape path.
    ///
    /// Fields mirror the operational dashboard
    /// [`spg-server`](https://crates.io/crates/spg-server) exposes,
    /// minus the network counters that don't apply to embedded.
    #[must_use]
    pub fn metrics(&self) -> EmbeddedMetrics {
        let cat = self.engine.catalog();
        let mut hot_rows: u64 = 0;
        let mut hot_bytes: u64 = 0;
        for name in cat.table_names() {
            if let Some(t) = cat.get(&name) {
                hot_rows = hot_rows.saturating_add(t.row_count() as u64);
                hot_bytes = hot_bytes.saturating_add(t.hot_bytes());
            }
        }
        let (wal_bytes, persistent) = match &self.persistence {
            Some(p) => (p.wal.written_len(), true),
            None => (0, false),
        };
        EmbeddedMetrics {
            hot_rows,
            hot_bytes,
            cold_segments: cat.cold_segment_count() as u64,
            tables: cat.table_count() as u64,
            wal_bytes,
            persistent,
        }
    }

    /// v7.2.1 — spawn a background thread that periodically
    /// runs `freeze_oldest_to_cold` when the catalog-wide hot
    /// tier exceeds `opts.hot_tier_bytes`. The `Arc<Mutex<_>>`
    /// pattern matches the v7.2 sharing story: callers wrap
    /// their `Database` in `Arc::new(Mutex::new(db))` once,
    /// then clone the Arc for the worker + for foreground
    /// access. Return value is a handle whose `Drop` joins the
    /// worker.
    ///
    /// Picks the freeze target the same way `spg-server`'s
    /// freezer does: largest-`hot_bytes` user table with at
    /// least one BTree integer-PK index. Tables without a
    /// freezable index are skipped silently.
    pub fn spawn_background_freezer(
        db: Arc<Mutex<Database>>,
        opts: FreezerOptions,
    ) -> FreezerHandle {
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_for_thread = Arc::clone(&shutdown);
        let join = thread::Builder::new()
            .name("spg-embedded-freezer".into())
            .spawn(move || {
                background_freezer_loop(db, opts, shutdown_for_thread);
            })
            .expect("spawn background freezer thread");
        FreezerHandle {
            shutdown,
            join: Some(join),
        }
    }
}

/// v7.2.1 — the freezer's main loop, factored out so the
/// `Database::spawn_background_freezer` path stays readable.
fn background_freezer_loop(
    db: Arc<Mutex<Database>>,
    opts: FreezerOptions,
    shutdown: Arc<AtomicBool>,
) {
    // Sleep in short slices so a shutdown request resolves
    // quickly (vs sleeping the full tick).
    let slice = Duration::from_millis(50.min(opts.tick.as_millis() as u64));
    let mut last_tick = std::time::Instant::now();
    loop {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        thread::sleep(slice);
        if last_tick.elapsed() < opts.tick {
            continue;
        }
        last_tick = std::time::Instant::now();
        let Ok(mut guard) = db.lock() else {
            return;
        };
        if guard.engine.catalog().hot_tier_bytes() <= opts.hot_tier_bytes {
            continue;
        }
        let Some((table, index)) = pick_freeze_target(&guard) else {
            continue;
        };
        let row_count = guard
            .engine
            .catalog()
            .get(&table)
            .map_or(0, spg_storage::Table::row_count);
        let to_freeze = opts.batch_rows.min(row_count);
        if to_freeze == 0 {
            continue;
        }
        if let Err(e) = guard.freeze_oldest_to_cold(&table, &index, to_freeze) {
            eprintln!("spg-embedded: background freeze on {table}.{index} failed: {e:?}");
            continue;
        }
        // v7.7.4 — auto-compact. If the catalog now carries
        // more cold segments than the configured threshold,
        // run a single compaction pass. Failures are reported
        // but don't kill the loop; the next tick will retry.
        let count = guard.engine.catalog().cold_segment_count();
        if count > opts.compact_when_segments_exceed {
            if let Err(e) = guard
                .engine
                .compact_cold_segments_with_target(opts.compact_target_bytes)
            {
                eprintln!(
                    "spg-embedded: background compact failed (segments={count}, \
                     threshold={}): {e:?}",
                    opts.compact_when_segments_exceed,
                );
            }
        }
    }
}

/// v7.2.1 — pick the highest-`hot_bytes` user table with a
/// BTree integer-PK index. Returns `(table, index_name)` so the
/// caller can dispatch through `freeze_oldest_to_cold`.
fn pick_freeze_target(db: &Database) -> Option<(String, String)> {
    let cat = db.engine.catalog();
    let mut best: Option<(String, String, u64)> = None;
    for name in cat.table_names() {
        let Some(t) = cat.get(&name) else { continue };
        if t.row_count() == 0 {
            continue;
        }
        let cols = &t.schema().columns;
        let Some(idx) = t.indices().iter().find(|i| {
            matches!(i.kind, spg_storage::IndexKind::BTree(_))
                && i.column_position < cols.len()
                && matches!(
                    cols[i.column_position].ty,
                    spg_storage::DataType::SmallInt
                        | spg_storage::DataType::Int
                        | spg_storage::DataType::BigInt
                )
        }) else {
            continue;
        };
        let hot = t.hot_bytes();
        match best {
            None => best = Some((name, idx.name.clone(), hot)),
            Some((_, _, best_hot)) if hot > best_hot => {
                best = Some((name, idx.name.clone(), hot));
            }
            _ => {}
        }
    }
    best.map(|(t, i, _)| (t, i))
}

/// v7.7.6 — replay the first `to_seq` records of the WAL at
/// `wal_path` into a fresh engine and write the resulting
/// catalog snapshot to `out_db_path`. Same semantics as
/// `spg revert --wal … --to-seq N --out …` from the CLI:
///
///   - `to_seq == 0` → snapshot is the empty catalog
///   - WAL records beyond `to_seq` are not applied
///   - durability-checkpoint markers (v3 type 0x02) are
///     consumed without counting against the budget
///
/// Returns the number of statements actually applied
/// (`≤ to_seq`). The output snapshot is byte-identical to
/// what `Database::open_path(out_db_path)` would consume on
/// a subsequent open.
///
/// This is the "rewind" operator for an embedded database
/// that has been corrupted by a poison statement or a
/// half-applied migration. Pair with `cold_segment_paths`
/// preservation if your cold-tier files are still on disk.
///
/// # Errors
///
/// - `wal_path` unreadable or truncated mid-record
/// - WAL record decodes to invalid UTF-8 SQL
/// - WAL record's SQL is rejected by the engine
/// - `out_db_path` unwritable
pub fn revert_wal_to_seq(
    wal_path: impl AsRef<Path>,
    to_seq: u64,
    out_db_path: impl AsRef<Path>,
) -> Result<u64, EngineError> {
    // v7.19 — accept either a single-file legacy WAL (v7.18 and
    // earlier layout) or a chunked WAL directory (v7.19+). For a
    // directory, concatenate every `.wal` chunk in sorted order
    // — the same order open_path replays them in — so revert
    // sees the full record stream.
    let path = wal_path.as_ref();
    let wal_bytes = if path.is_dir() {
        let mut combined = Vec::new();
        let chunks = sorted_wal_chunks(path).map_err(io_err)?;
        for chunk in chunks {
            let bytes = std::fs::read(&chunk).map_err(io_err)?;
            combined.extend_from_slice(&bytes);
        }
        combined
    } else {
        std::fs::read(path).map_err(io_err)?
    };
    let mut engine = Engine::new();
    let mut applied = 0u64;
    let mut cur = 0usize;
    while cur < wal_bytes.len() && applied < to_seq {
        let (sql_bytes, total) = decode_wal_record(&wal_bytes[cur..])?;
        cur += total;
        if sql_bytes.is_empty() {
            continue;
        }
        let sql = core::str::from_utf8(&sql_bytes).map_err(|e| {
            EngineError::Storage(spg_storage::StorageError::Corrupt(format!(
                "WAL record at offset {cur}: non-UTF-8 SQL: {e}"
            )))
        })?;
        // v7.21 — tx-commit records carry a multi-statement script;
        // split_statements is a no-op for single-statement records.
        for stmt in split_statements(sql) {
            engine.execute(stmt)?;
        }
        applied += 1;
    }
    let snapshot = engine.snapshot();
    std::fs::write(out_db_path.as_ref(), &snapshot).map_err(io_err)?;
    Ok(applied)
}

/// v7.7.6 — decode one WAL record from a byte tail. Returns
/// `(sql_bytes, header_plus_payload_len)`. Handles the three
/// on-disk formats (v1 / v2 / v3) the same way the CLI
/// `decode_one_record` and the engine's `replay_wal_bytes`
/// do. CRCs are not re-validated; the caller's intent is
/// "apply", not "validate".
fn decode_wal_record(tail: &[u8]) -> Result<(Vec<u8>, usize), EngineError> {
    if tail.len() < 4 {
        return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
            format!("WAL truncated record: {} < 4 header bytes", tail.len()),
        )));
    }
    let raw_len = u32::from_le_bytes(tail[..4].try_into().unwrap());
    let is_v2 = raw_len & WAL_V2_SENTINEL != 0;
    let is_v3 = is_v2 && (raw_len & WAL_V3_FLAG != 0);
    let len_mask = if is_v3 {
        !(WAL_V2_SENTINEL | WAL_V3_FLAG)
    } else {
        !WAL_V2_SENTINEL
    };
    let rec_len = (raw_len & len_mask) as usize;
    let header_len = if is_v3 {
        9
    } else if is_v2 {
        8
    } else {
        4
    };
    if tail.len() < header_len + rec_len {
        return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
            format!(
                "WAL truncated record: header+payload {} > available {}",
                header_len + rec_len,
                tail.len()
            ),
        )));
    }
    if is_v3 {
        let type_byte = tail[8];
        // v3 type 0x01 = auto_commit_sql (payload = SQL).
        // v3 type 0x02 = durability marker (no SQL to apply).
        // v4 type 0x10 = auto_commit_sql with 16-byte (lsn, ts)
        //                prefix between type and SQL — strip
        //                the prefix so the caller still sees raw
        //                SQL bytes.
        // Anything else is unknown.
        if type_byte == WAL_V3_TYPE_AUTO_COMMIT_SQL {
            let payload = &tail[header_len..header_len + rec_len];
            return Ok((payload.to_vec(), header_len + rec_len));
        }
        if type_byte == WAL_V4_TYPE_AUTO_COMMIT_SQL || type_byte == WAL_V4_TYPE_TX_COMMIT_SQL {
            let v4_total = header_len + WAL_V4_EXTRA_HEADER + rec_len;
            if tail.len() < v4_total {
                return Err(EngineError::Storage(spg_storage::StorageError::Corrupt(
                    format!(
                        "WAL truncated v4 record: header+payload {v4_total} > available {}",
                        tail.len()
                    ),
                )));
            }
            let sql_start = header_len + WAL_V4_EXTRA_HEADER;
            let sql_bytes = tail[sql_start..sql_start + rec_len].to_vec();
            return Ok((sql_bytes, v4_total));
        }
        // Caller treats empty payload as a skip-marker.
        return Ok((Vec::new(), header_len + rec_len));
    }
    let payload = &tail[header_len..header_len + rec_len];
    Ok((payload.to_vec(), header_len + rec_len))
}

impl Drop for Database {
    fn drop(&mut self) {
        // v7.1 — best-effort final checkpoint when a persistent
        // Database leaves scope. Failures here go to stderr so
        // operators see them, but Drop can't propagate errors —
        // the WAL itself is already durable, so a checkpoint
        // miss only means the next boot replays a few more
        // records than strictly necessary.
        if self.persistence.is_some() {
            if let Err(e) = self.checkpoint() {
                eprintln!(
                    "spg-embedded: final checkpoint on Drop failed: {e:?} \
                     (WAL is intact; next open_path will replay)"
                );
            }
        }
        // v7.19 P3 / v7.20 — signal the retention + flusher
        // threads to exit, then wait for them. Done BEFORE the
        // lock release so background threads don't outlive the
        // database handle. The flusher drains the pending batch
        // on its way out (final flush_now in the thread body),
        // so `SPG_SYNCHRONOUS_COMMIT=off` never loses confirmed
        // commits across a clean shutdown.
        if let Some(ctx) = self.persistence.as_mut() {
            if let Some(shutdown) = ctx.retention_shutdown.take() {
                shutdown.store(true, Ordering::SeqCst);
            }
            if let Some(handle) = ctx.retention_thread.take() {
                let _ = handle.join();
            }
            if let Some(shutdown) = ctx.flusher_shutdown.take() {
                shutdown.store(true, Ordering::SeqCst);
            }
            if let Some(handle) = ctx.flusher_thread.take() {
                let _ = handle.join();
            }
            // CoW-2 (v7.34) — final checkpoint above left the worker
            // idle; explicitly drop it here so its shutdown signal +
            // thread join happens with a deterministic ordering (before
            // the lock release / persistence drop), not whenever Rust
            // happens to drop the PersistenceCtx fields.
            ctx.checkpoint_worker = None;
        }
        // v7.17.0 Phase 6.2 — release the cross-process lock on
        // clean shutdown. Failure is logged but never panics;
        // the operator can clear a stale lock via
        // `Database::force_unlock` if a crash kept the
        // directory around.
        if let Some(ctx) = &self.persistence
            && ctx.lock_path.exists()
        {
            // remove_dir_all: the lock dir carries the owner-pid
            // record since round-12.
            if let Err(e) = std::fs::remove_dir_all(&ctx.lock_path) {
                eprintln!(
                    "spg-embedded: lock release on Drop failed for {}: {e:?}",
                    ctx.lock_path.display()
                );
            }
        }
    }
}

impl Database {
    /// v7.17.0 Phase 6.2 — clear a stale cross-process lock.
    /// Use when a previous process crashed mid-session and
    /// left `<db_path>.lock` behind. Operators should confirm
    /// no other process is currently using the database before
    /// calling this — SPG cannot fingerprint stale-vs-live
    /// without a libc dep, which would violate spg-embedded's
    /// zero-deps charter.
    pub fn force_unlock(db_path: impl AsRef<Path>) -> Result<(), EngineError> {
        let lock_path = {
            let mut p = db_path.as_ref().to_path_buf();
            let name = p
                .file_name()
                .map(|n| {
                    let mut s = n.to_os_string();
                    s.push(".lock");
                    s
                })
                .unwrap_or_else(|| std::ffi::OsString::from(".lock"));
            p.set_file_name(name);
            p
        };
        if !lock_path.exists() {
            return Ok(());
        }
        std::fs::remove_dir_all(&lock_path).map_err(io_err)
    }
}

/// v7.1 — turn a `std::io::Error` into the workspace's
/// `EngineError` shape. `EngineError::Storage(Corrupt(_))` is
/// the closest existing variant — io failures during boot or
/// during a WAL append surface as a storage-layer fault to
/// callers, which keeps the public error enum unchanged.
fn io_err(e: std::io::Error) -> EngineError {
    EngineError::Storage(spg_storage::StorageError::Corrupt(format!("io: {e}")))
}

/// v7.2.2 — `Database` is `Send`, so the recommended sharing
/// pattern for multi-threaded callers is `Arc<Mutex<Database>>`:
///
/// ```no_run
/// use std::sync::{Arc, Mutex};
/// use spg_embedded::Database;
///
/// let db = Database::open_in_memory();
/// let shared = Arc::new(Mutex::new(db));
/// let shared_for_worker = Arc::clone(&shared);
/// std::thread::spawn(move || {
///     let mut guard = shared_for_worker.lock().unwrap();
///     guard.execute("INSERT INTO t VALUES (1)").unwrap();
/// });
/// ```
///
/// Internal `RwLock`-wrapped state — letting many threads
/// hold concurrent `&Database` for `SELECT` without contending
/// — is parked as STABILITY § "Out of v7.2"; multi-reader
/// embedded throughput needs a planner-side change to release
/// the engine read lock between scans, which is the v7.x
/// "Choice A" line of work already documented in v6.9.1's
/// carve-out.
#[allow(dead_code)]
fn _database_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Database>();
}

/// v6.10.3 — trait that maps a row's columns onto a user
/// struct's fields. v7.3.0 ships the [`spg_row!`] declarative
/// macro that generates `impl FromSpgRow for YourStruct` from
/// a struct definition (no proc-macro, no syn/quote/
/// proc-macro2 deps — the workspace's "0 external deps"
/// policy holds).
///
/// Implementors map a row's columns onto a user struct's
/// fields. Errors surface as `EngineError::Unsupported` so the
/// caller's error type stays uniform.
pub trait FromSpgRow: Sized {
    /// Decode one query result row into `Self`. Called once per
    /// row by [`Database::query_typed`]. The slice length equals
    /// the number of columns in the SELECT projection.
    fn from_spg_row(row: &[Value]) -> Result<Self, EngineError>;
}

/// v7.3.0 — declarative macro that generates `FromSpgRow` impl
/// for a user struct. Avoids proc-macro deps
/// (syn/quote/proc-macro2) so the workspace's 0-deps policy
/// holds; the trade-off vs `#[derive(SpgRow)]` is that the
/// macro takes the entire struct definition (fields + types)
/// as input rather than annotating an existing struct.
///
/// ```no_run
/// use spg_embedded::{Database, spg_row, FromSpgRow};
///
/// spg_row! {
///     pub struct User {
///         pub id: i32,
///         pub name: String,
///     }
/// }
///
/// let mut db = Database::open_in_memory();
/// db.execute("CREATE TABLE users (id INT NOT NULL, name TEXT)").unwrap();
/// db.execute("INSERT INTO users VALUES (1, 'alice')").unwrap();
/// let users: Vec<User> = db.query_typed("SELECT id, name FROM users").unwrap();
/// ```
///
/// Supported field types: `i16`, `i32`, `i64`, `f32`, `f64`,
/// `bool`, `String`, `Vec<f32>` (for `VECTOR(N)` columns),
/// `Option<T>` of any of the above.
#[macro_export]
macro_rules! spg_row {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident {
            $(
                $(#[$fmeta:meta])*
                $fvis:vis $field:ident : $ty:ty,
            )*
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone)]
        $vis struct $name {
            $(
                $(#[$fmeta])*
                $fvis $field : $ty,
            )*
        }

        impl $crate::FromSpgRow for $name {
            fn from_spg_row(row: &[$crate::Value]) -> ::core::result::Result<Self, $crate::EngineError> {
                let mut __spg_row_iter = row.iter();
                $(
                    let $field: $ty = {
                        let v = __spg_row_iter
                            .next()
                            .ok_or_else(|| $crate::EngineError::Unsupported(
                                ::std::format!(
                                    "spg_row! {}: missing column for field `{}`",
                                    ::core::stringify!($name),
                                    ::core::stringify!($field)
                                )
                            ))?;
                        <$ty as $crate::FromSpgValue>::from_spg_value(v)
                            .map_err(|e| $crate::EngineError::Unsupported(
                                ::std::format!(
                                    "spg_row! {}: column `{}`: {}",
                                    ::core::stringify!($name),
                                    ::core::stringify!($field),
                                    e
                                )
                            ))?
                    };
                )*
                Ok(Self { $($field,)* })
            }
        }
    };
}

/// v7.3.0 — per-column decoder used by `spg_row!`. Surface
/// covers every numeric / text / bytes / bool variant in
/// `Value`, plus `Option<T>` for nullable columns.
pub trait FromSpgValue: Sized {
    /// Decode one cell into `Self`. The returned `&'static str`
    /// is a short diagnostic for type mismatches (e.g. `"expected
    /// integer, got TEXT"`); callers wrap it into their own
    /// error type.
    fn from_spg_value(v: &Value) -> Result<Self, &'static str>;
}

macro_rules! impl_from_value_int {
    ($($t:ty),* $(,)?) => {
        $(
            impl FromSpgValue for $t {
                fn from_spg_value(v: &Value) -> Result<Self, &'static str> {
                    match v {
                        Value::SmallInt(n) => <$t>::try_from(*n).map_err(|_| "SmallInt does not fit target int type"),
                        Value::Int(n)      => <$t>::try_from(*n).map_err(|_| "Int does not fit target int type"),
                        Value::BigInt(n)   => <$t>::try_from(*n).map_err(|_| "BigInt does not fit target int type"),
                        Value::Null        => Err("NULL in non-Option int column"),
                        _ => Err("non-integer value in int column"),
                    }
                }
            }
        )*
    };
}
impl_from_value_int!(i16, i32, i64);

impl FromSpgValue for f32 {
    fn from_spg_value(v: &Value) -> Result<Self, &'static str> {
        match v {
            Value::Float(f) => Ok(*f as f32),
            Value::Null => Err("NULL in non-Option float column"),
            _ => Err("non-float value in float column"),
        }
    }
}

impl FromSpgValue for f64 {
    fn from_spg_value(v: &Value) -> Result<Self, &'static str> {
        match v {
            Value::Float(f) => Ok(*f),
            Value::Null => Err("NULL in non-Option float column"),
            _ => Err("non-float value in float column"),
        }
    }
}

impl FromSpgValue for bool {
    fn from_spg_value(v: &Value) -> Result<Self, &'static str> {
        match v {
            Value::Bool(b) => Ok(*b),
            Value::Null => Err("NULL in non-Option bool column"),
            _ => Err("non-bool value in bool column"),
        }
    }
}

impl FromSpgValue for String {
    fn from_spg_value(v: &Value) -> Result<Self, &'static str> {
        match v {
            Value::Text(s) => Ok(s.clone()),
            Value::Null => Err("NULL in non-Option text column"),
            _ => Err("non-text value in String column"),
        }
    }
}

impl FromSpgValue for Vec<f32> {
    fn from_spg_value(v: &Value) -> Result<Self, &'static str> {
        match v {
            Value::Vector(xs) => Ok(xs.clone()),
            Value::Null => Err("NULL in non-Option vector column"),
            _ => Err("non-vector value in Vec<f32> column"),
        }
    }
}

impl<T: FromSpgValue> FromSpgValue for Option<T> {
    fn from_spg_value(v: &Value) -> Result<Self, &'static str> {
        match v {
            Value::Null => Ok(None),
            other => T::from_spg_value(other).map(Some),
        }
    }
}

/// Acquire the cross-process exclusion lock at `lock_path` (atomic
/// `mkdir`), recording the owner pid inside. If the lock already
/// exists, read the recorded pid and probe liveness — a lock left
/// behind by a killed process (docker SIGKILL, crash) is reclaimed
/// automatically instead of forcing the operator to delete it by
/// hand (mailrs embed round-12: a restarted server came up in
/// degraded mode because the previous instance's lock survived).
/// v7.27 (mailrs round-21 B) — the prober's environment identity:
/// `(hostname, boot-or-container id)`. A pid is only meaningful
/// inside the PID namespace that recorded it; mailrs's recovery
/// window saw "locked by pid 1" from a STOPPED container because
/// the prober's pid 1 (its own init) was alive. When the lock's
/// identity differs from ours, liveness is UNDECIDABLE and we
/// refuse honestly instead of guessing in either direction.
fn host_identity() -> (String, String) {
    let hostname = std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    // Linux boot id; containers share the host kernel's boot id, so
    // hostname (= container id by default) is the namespace
    // discriminator and boot id catches host reboots / pid reuse.
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .or_else(|_| {
            std::process::Command::new("sysctl")
                .args(["-n", "kern.bootsessionuuid"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .unwrap_or_default();
    (hostname, boot_id)
}

/// v7.34 (crash-recovery P0 #2) — process start-time, to tell a reused
/// pid apart from a genuinely-held lock. In a container the holder is
/// always pid 1; `docker start` reuses the container so the NEW process
/// is pid 1 too, on the same host+boot id — a bare `pid_alive(1)` probe
/// (`ps -p 1` always succeeds) reads a dead owner's lock as live and the
/// engine self-deadlocks on its own catalog. The `(pid, start-time)`
/// pair is unique per live process within a boot: a reused pid carries a
/// LATER start-time, so a mismatch means the recorded owner is gone.
/// Linux reads `/proc/<pid>/stat` field 22 (clock ticks since boot);
/// `comm` (field 2) is parenthesised and may contain spaces, so fields
/// are taken after the LAST ')'. Other platforms return None and the
/// liveness check falls back to pid-alive + the self-pid reclaim. Pure
/// std — no libc.
#[cfg(target_os = "linux")]
fn process_start_time(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = stat.rsplit_once(')').map(|(_, rest)| rest)?;
    // After comm: state(1) ppid(2) … starttime is the 20th token.
    after.split_whitespace().nth(19).map(str::to_string)
}

#[cfg(not(target_os = "linux"))]
fn process_start_time(_pid: u32) -> Option<String> {
    None
}

fn acquire_path_lock(lock_path: &Path) -> Result<(), EngineError> {
    for attempt in 0..2 {
        match std::fs::create_dir(lock_path) {
            Ok(()) => {
                // Best-effort owner record; liveness probing treats a
                // missing pid file as stale (crash between mkdir and
                // write is indistinguishable from an ancient lock).
                // v7.27 — lines 2+3 record the owner's environment
                // identity (hostname, boot id) so a prober in a
                // different namespace refuses instead of misreading
                // the pid. v7.34 — line 4 records the owner's process
                // start-time so a reused pid (container pid-1 restart)
                // is distinguishable from a live holder.
                let (host, boot) = host_identity();
                let start = process_start_time(std::process::id()).unwrap_or_default();
                let _ = std::fs::write(
                    lock_path.join("pid"),
                    format!("{}\n{host}\n{boot}\n{start}\n", std::process::id()),
                );
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt == 0 => {
                let record = std::fs::read_to_string(lock_path.join("pid")).unwrap_or_default();
                let mut lines = record.lines();
                let owner = lines.next().and_then(|s| s.trim().parse::<u32>().ok());
                let lock_host = lines.next().unwrap_or("").trim().to_string();
                let lock_boot = lines.next().unwrap_or("").trim().to_string();
                let lock_start = lines.next().unwrap_or("").trim().to_string();
                // Note(v7.37.10 design choice): we do NOT auto-reclaim a
                // lock whose (pid, start-time) matches OUR own process.
                // Tempting fix for the mailrs 2026-06-19 recurrence —
                // "the prior open_path future got cancelled, its lock
                // leaked" — but `AsyncDatabase::open_path` runs the
                // blocking `Database::open_path` inside
                // `tokio::task::spawn_blocking`, which CANNOT be
                // cancelled mid-flight. When the awaiting future is
                // dropped (pool acquire-timeout), the spawn_blocking
                // task keeps running and STILL HOLDS the lock; auto-
                // reclaiming would let a concurrent retry steal a live
                // task's lock and corrupt WAL replay. The mailrs flow
                // is correctly resolved by waiting for the in-flight
                // replay to finish — sentori's spg-sqlx pool config
                // needs a higher `acquire_timeout` than spg's worst-
                // case replay time. Tracking that separately as a
                // spg-sqlx pool-default change for v7.38.
                // v7.27 — identity check BEFORE the pid probe. A pid
                // recorded in another namespace is undecidable both
                // ways (a stale lock can look held, a held lock can
                // look stale — the unsafe direction). Old-format
                // locks (pid only) keep the legacy same-host
                // assumption.
                // v7.37.10 — skip host_identity when the recorded owner
                // is PID 1. PID 1 means containerised; `docker compose
                // up -d` recreates the container with a new hostname so
                // a strict host-identity match would refuse every
                // restart even when the start-time check below would
                // correctly declare the old generation stale. The
                // start-time check is more accurate for the container
                // case anyway — let it decide.
                let lock_is_pid1 = owner == Some(1);
                if !lock_host.is_empty() && !lock_is_pid1 {
                    let (my_host, my_boot) = host_identity();
                    let same_env = lock_host == my_host
                        && (lock_boot.is_empty() || my_boot.is_empty() || lock_boot == my_boot);
                    if !same_env {
                        return Err(EngineError::Unsupported(format!(
                            "database lock {} was taken in a different host/container \
                             (owner: pid {} on {:?}; we are {:?}) — liveness is \
                             undecidable from here. If you are sure the owner is gone, \
                             call Database::force_unlock() or `spg import --force-unlock`.",
                            lock_path.display(),
                            owner.unwrap_or(0),
                            lock_host,
                            my_host
                        )));
                    }
                }
                // v7.34 (crash-recovery P0 #2) — pid-reuse-safe liveness.
                // A bare `pid_alive` self-deadlocks in a container: the
                // dead owner was pid 1, `docker start` reuses the container
                // so the prober is pid 1 too, and `ps -p 1` always succeeds.
                // The recorded (pid, start-time) pair settles it — the
                // owner is alive ONLY if its pid is alive AND its CURRENT
                // start-time still matches the recorded one:
                //  - container restart: pid 1 alive, but the new pid-1's
                //    start-time differs from the dead owner's → stale.
                //  - genuine double-open (same live process): start-time
                //    matches (it wrote it) → held — correctly refused, so a
                //    second writer can't steal a live lock.
                // v7.37.10 — for PID-1 owners with no recorded start-time
                // (a pre-v7.34 lock from a previous container generation),
                // treat as stale: a new container's PID 1 cannot share
                // identity with the previous container's PID 1. Gated on
                // `process_start_time` having returned `Some(_)` so the
                // arm only fires on Linux (where /proc is queryable); on
                // macOS, where PID 1 is `launchd` (a real long-running
                // system process), the empty-start-time fallback keeps
                // the safer pid-alive answer.
                let owner_alive = owner.is_some_and(|p| {
                    if !pid_alive(p) {
                        return false;
                    }
                    let now = process_start_time(p);
                    match (now, lock_start.is_empty()) {
                        (Some(t), false) => t == lock_start,
                        (Some(_), true) if p == 1 => false,
                        _ => true,
                    }
                });
                if owner_alive {
                    return Err(EngineError::Unsupported(format!(
                        "database is locked by another process (pid {}): {}; \
                         stop that process first, or call Database::force_unlock()",
                        owner.unwrap_or(0),
                        lock_path.display()
                    )));
                }
                // Stale — owner pid dead, reused, or unrecorded. Reclaim.
                eprintln!(
                    "spg-embedded: reclaiming stale lock {} (owner pid {:?} not a live holder)",
                    lock_path.display(),
                    owner
                );
                std::fs::remove_dir_all(lock_path).map_err(io_err)?;
                // Loop retries the create_dir; a concurrent reclaimer
                // winning the race surfaces as AlreadyExists on
                // attempt 1 below.
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(EngineError::Unsupported(format!(
                    "database is locked by another process: {}; \
                     stop that process first, or call Database::force_unlock()",
                    lock_path.display()
                )));
            }
            Err(e) => return Err(io_err(e)),
        }
    }
    unreachable!("acquire_path_lock loop covers both attempts")
}

/// Probe whether `pid` is a live process. Unix: `ps -p` via the
/// system binary (std-only — no libc dependency). `ps -p` exits 0
/// for ANY live pid regardless of owner; `kill -0` was rejected
/// here because it fails with EPERM on another user's live process,
/// which would read as "dead" and reclaim a held lock. Probe
/// failure (no `ps` binary, exec error) conservatively reports
/// alive so locks are never auto-reclaimed on doubt; non-unix
/// targets do the same.
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    match std::process::Command::new("ps")
        .arg("-p")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(status) => status.success(),
        Err(_) => true,
    }
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    true
}

/// Strip leading whitespace, `--` line comments and NON-conditional
/// block comments from a chunk so statement-head checks (COPY
/// detection most notably) see the first real token. pg_dump
/// prefixes every data block with a `-- Data for Name: …;` banner —
/// which itself contains semicolons, so head checks must run on the
/// stripped text. MySQL executable conditional comments (`/*!`) are
/// content and stay.
/// v7.22 — see `split_statements`' `mysql_escapes` tracking. Only
/// short chunks are inspected (the signal statements are one-liners;
/// COPY data blocks are skipped by the length guard).
fn note_dialect_signals(chunk: &str, mysql_escapes: &mut bool) {
    if chunk.len() > 4096 {
        return;
    }
    let lower = chunk.to_ascii_lowercase();
    if lower.contains("sql_mode") {
        *mysql_escapes = true;
    } else if lower.contains("standard_conforming_strings") {
        *mysql_escapes = lower.contains("off");
    }
}

fn strip_leading_sql_noise(mut s: &str) -> &str {
    loop {
        let t = s.trim_start();
        if let Some(rest) = t.strip_prefix("--") {
            s = rest.split_once('\n').map_or("", |(_, r)| r);
            continue;
        }
        if t.starts_with("/*") && !t.starts_with("/*!") {
            match t.find("*/") {
                Some(e) => {
                    s = &t[e + 2..];
                    continue;
                }
                None => return "",
            }
        }
        return t;
    }
}

/// Split a multi-statement SQL script into individual statements on
/// top-level `;`, honouring single-quoted strings (with `''`
/// escapes), double-quoted identifiers, dollar-quoted bodies
/// (`$tag$ … $tag$`), line comments (`--`) and MySQL executable
/// conditional comments (`/*!… */` stay statement content; plain
/// nested block comments don't). Chunks that contain no statement
/// content (whitespace / comments only) are dropped. PG's
/// simple-query protocol does this server-side; the embed path owns
/// it here.
///
/// v7.22 (mailrs round-13 gap 1) — psql meta-command lines are
/// dropped for client parity: a line whose first non-whitespace
/// byte is `\` BETWEEN statements (PG 18's pg_dump wraps scripts in
/// `\restrict` / `\unrestrict`) never reaches the parser, the same
/// way psql consumes `\`-lines client-side and never sends them. A
/// mid-statement backslash stays an ordinary byte — pg_dump only
/// emits meta-commands between statements.
pub fn split_statements(sql: &str) -> Vec<&str> {
    let bytes = sql.as_bytes();
    let mut stmts = Vec::new();
    let mut start = 0usize;
    let mut has_content = false;
    // v7.22 (round-13 T3) — stream-tracked string dialect, mirroring
    // the engine's session flag: a statement mentioning `sql_mode`
    // (mysqldump preamble, often inside `/*!…*/`) switches plain
    // strings to backslash-escape scanning;
    // `standard_conforming_strings` (pg_dump preamble) switches
    // back. Without this the scanner ends a MySQL `'…\'…'` literal
    // early and splits inside data.
    let mut mysql_escapes = false;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if !has_content => {
                // Start-of-statement `\` = psql meta-command line.
                // Consume through end-of-line; restart the chunk
                // after it so the line never lands in the output.
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                start = if i < bytes.len() { i + 1 } else { i };
            }
            b'\'' => {
                has_content = true;
                // PG escape-string form `E'...'` honours backslash
                // escapes (`E'a\';b'` is ONE literal) — detect via
                // the immediately-preceding standalone E/e. MySQL
                // dialect sessions treat EVERY plain string that way.
                let escape_string = mysql_escapes
                    || (i >= 1
                        && matches!(bytes[i - 1], b'e' | b'E')
                        && !(i >= 2
                            && (bytes[i - 2].is_ascii_alphanumeric() || bytes[i - 2] == b'_')));
                i += 1;
                while i < bytes.len() {
                    if escape_string && bytes[i] == b'\\' {
                        // Skip the escaped byte (covers \' and \\).
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'\'' {
                        // `''` is an escaped quote inside the literal.
                        if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'"' => {
                has_content = true;
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += 1;
                }
            }
            b'$' => {
                // Possible dollar-quote opener `$tag$` (tag may be
                // empty). If the shape doesn't match, it's a plain
                // `$` (positional param) — fall through.
                let tag_end = bytes[i + 1..]
                    .iter()
                    .position(|&b| !(b.is_ascii_alphanumeric() || b == b'_'))
                    .map(|off| i + 1 + off);
                if let Some(te) = tag_end
                    && te < bytes.len()
                    && bytes[te] == b'$'
                {
                    has_content = true;
                    let tag = &sql[i..=te];
                    // Find the closing `$tag$`.
                    if let Some(close) = sql[te + 1..].find(tag) {
                        i = te + 1 + close + tag.len();
                        continue;
                    }
                    // Unterminated — consume the rest; the parser
                    // will report it.
                    i = bytes.len();
                    continue;
                }
                has_content = true;
            }
            b'-' if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                // v7.22 (round-13 T3) — MySQL conditional comments
                // `/*!40101 … */` are EXECUTABLE (mysqldump wraps
                // its whole preamble + DISABLE KEYS hints in them);
                // they must stay statement content for the engine,
                // not be skipped as commentary.
                if i + 2 < bytes.len() && bytes[i + 2] == b'!' {
                    has_content = true;
                }
                let mut depth = 1usize;
                i += 2;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                continue;
            }
            b';' => {
                if has_content {
                    let head = &sql[start..i];
                    // v7.22 (round-13 T2) — a `COPY … FROM stdin;`
                    // statement owns its following data block
                    // through the `\.` terminator line (data lines
                    // may contain `;`, so generic splitting would
                    // shred them). Swallow head + data into ONE
                    // chunk; `execute_script` lowers it to INSERTs.
                    // pg_dump prefixes the COPY with a comment
                    // banner — strip it before the head check.
                    let head_clean = strip_leading_sql_noise(head);
                    let is_copy_head = head_clean
                        .get(..4)
                        .is_some_and(|p| p.eq_ignore_ascii_case("copy"))
                        && spg_engine::copy::parse_copy_from_stdin_head(head_clean).is_some();
                    if is_copy_head {
                        // Scan whole lines after the ';' until the
                        // `\.` terminator (or EOF — torn dumps lose
                        // their tail, same as psql would error).
                        let mut j = i + 1;
                        let data_end;
                        loop {
                            if j >= bytes.len() {
                                data_end = bytes.len();
                                break;
                            }
                            let line_end = sql[j..].find('\n').map_or(bytes.len(), |off| j + off);
                            if sql[j..line_end].trim_end_matches('\r').trim() == "\\." {
                                data_end = j;
                                i = line_end; // bottom i += 1 skips \n
                                break;
                            }
                            j = line_end + 1;
                        }
                        stmts.push(&sql[start..data_end]);
                        if data_end == bytes.len() {
                            i = bytes.len();
                        }
                        start = i + 1;
                        has_content = false;
                        i += 1;
                        continue;
                    }
                    note_dialect_signals(head, &mut mysql_escapes);
                    stmts.push(head);
                }
                start = i + 1;
                has_content = false;
            }
            b => {
                if !b.is_ascii_whitespace() {
                    has_content = true;
                }
            }
        }
        i += 1;
    }
    if has_content {
        stmts.push(&sql[start..]);
    }
    stmts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_statements_basic_and_trailing() {
        assert_eq!(
            split_statements("CREATE TABLE a (x INT); INSERT INTO a VALUES (1)"),
            vec!["CREATE TABLE a (x INT)", " INSERT INTO a VALUES (1)"]
        );
        // whitespace/comment-only chunks drop
        assert!(split_statements("  ;; -- nothing\n;").is_empty());
    }

    #[test]
    fn split_statements_quoting_forms() {
        // ';' inside a plain literal, a doubled quote, an E-string
        // backslash escape, a quoted identifier, and a dollar-quoted
        // body must not split.
        let cases = [
            "INSERT INTO t VALUES ('a;b')",
            "INSERT INTO t VALUES ('it''s; fine')",
            r"INSERT INTO t VALUES (E'it\'s; fine')",
            "CREATE TABLE \"odd;name\" (x INT)",
            "DO $body$ BEGIN PERFORM 1; END $body$",
            "DO $$ SELECT 1; $$",
        ];
        for sql in cases {
            assert_eq!(split_statements(sql), vec![sql], "must stay whole: {sql}");
        }
        // ...and each still splits cleanly from a neighbour.
        for sql in cases {
            let script = format!("{sql};\nSELECT 2");
            assert_eq!(
                split_statements(&script),
                vec![sql, "\nSELECT 2"],
                "must split after: {sql}"
            );
        }
    }

    #[test]
    fn split_statements_drops_psql_meta_lines() {
        // v7.22 round-13 gap 1 — PG 18 pg_dump wraps scripts in
        // `\restrict` / `\unrestrict`; psql parity = the lines never
        // reach the parser.
        let script = "\\restrict TOKEN123\nSELECT 1;\n\\unrestrict TOKEN123\nSELECT 2;\n\\.\n";
        assert_eq!(split_statements(script), vec!["SELECT 1", "SELECT 2"]);
        // Mid-statement backslash is NOT a meta-command.
        let s2 = r"SELECT E'a\\b'";
        assert_eq!(split_statements(s2), vec![s2]);
    }

    #[test]
    fn split_statements_comments_hide_semicolons() {
        let script = "-- c1 ; still comment\nSELECT 1; /* a ; b /* nested ; */ */ SELECT 2";
        let got = split_statements(script);
        assert_eq!(got.len(), 2);
        assert!(got[0].contains("SELECT 1"));
        assert!(got[1].contains("SELECT 2"));
    }

    #[test]
    fn in_memory_create_insert_select() {
        let mut db = Database::open_in_memory();
        db.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 'alice')").unwrap();
        db.execute("INSERT INTO t VALUES (2, 'bob')").unwrap();
        let rows = db.query("SELECT id FROM t WHERE id = 1").unwrap();
        assert_eq!(rows.len(), 1);
        match &rows[0][0] {
            Value::Int(1) => {}
            other => panic!("expected Int(1), got {other:?}"),
        }
    }

    #[test]
    fn query_on_non_select_errors() {
        let mut db = Database::open_in_memory();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        let r = db.query("INSERT INTO t VALUES (1)");
        assert!(r.is_err(), "query() on INSERT must error");
    }

    #[test]
    fn snapshot_roundtrip() {
        let mut db = Database::open_in_memory();
        db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        db.execute("INSERT INTO t VALUES (42)").unwrap();
        let bytes = db.snapshot();
        let mut restored = Database::restore(&bytes).unwrap();
        let rows = restored.query("SELECT id FROM t WHERE id = 42").unwrap();
        assert_eq!(rows.len(), 1);
        match &rows[0][0] {
            Value::Int(42) => {}
            other => panic!("expected Int(42), got {other:?}"),
        }
    }

    #[test]
    fn from_spg_row_trait_shape() {
        struct User {
            _id: i32,
        }
        impl FromSpgRow for User {
            fn from_spg_row(row: &[Value]) -> Result<Self, EngineError> {
                match row.first() {
                    Some(Value::Int(n)) => Ok(Self { _id: *n }),
                    _ => Err(EngineError::Unsupported("bad id".into())),
                }
            }
        }
        let row = vec![Value::Int(7)];
        let _u = User::from_spg_row(&row).unwrap();
    }
}
