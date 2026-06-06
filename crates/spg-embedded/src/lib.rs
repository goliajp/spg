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

pub use spg_engine::{Engine, EngineError, ParsedStatement, QueryResult};
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
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// v7.11.3 — wall-clock provider injected into every embedded
/// `Engine`. Microseconds since the Unix epoch; clamps to
/// `i64::MAX` if the system clock is far-future. Used by SQL's
/// `NOW()` / `CURRENT_TIMESTAMP` / `CURRENT_DATE` rewrite layer
/// so PG-idiomatic time queries work without the caller wiring
/// their own clock.
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

/// v7.1 — auto-checkpoint threshold. Once the WAL grows past
/// this many bytes, the next successful `execute()` call ends
/// with a `checkpoint()` so the WAL stays bounded. Tunable via
/// `SPG_EMBEDDED_CHECKPOINT_BYTES` env.
fn default_checkpoint_threshold_bytes() -> u64 {
    std::env::var("SPG_EMBEDDED_CHECKPOINT_BYTES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(4 * 1024 * 1024)
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
                0x02 => {
                    // durability_checkpoint marker — skip, no SQL.
                    cur += header_len + rec_len;
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
    /// `execute(sql)` that mutates state appends a v3
    /// `auto_commit_sql` WAL record + fsyncs before the call
    /// returns; `Drop` writes a final catalog snapshot to
    /// `<db_path>` so the next session boots from a clean
    /// snapshot + an empty WAL. `None` = in-memory only (the
    /// v6.10.3 shape).
    persistence: Option<PersistenceCtx>,
}

#[derive(Debug)]
#[allow(dead_code)] // `wal_path` is read at boot; kept for Drop/diag introspection.
struct PersistenceCtx {
    db_path: PathBuf,
    wal_path: PathBuf,
    wal: File,
    /// Cached WAL file length so each `execute()` doesn't have
    /// to stat. Refreshed on append + on `checkpoint()` (which
    /// truncates back to 0).
    wal_len: u64,
    checkpoint_threshold_bytes: u64,
    /// v7.1.4 — `<db_path>.spg/segments/` directory. Cold-tier
    /// segments produced by `freeze_oldest_to_cold` / compaction
    /// are persisted here as `seg_<id>.spg` files; the manifest
    /// at `<db_path>.spg/manifest.v10` records every active
    /// segment + its CRC32 so the next boot can verify + reload.
    cold_segments_dir: PathBuf,
    cold_segment_paths: BTreeMap<u32, PathBuf>,
}

impl Database {
    /// Open a fresh in-memory database. No WAL, no catalog
    /// snapshot on disk — perfect for tests + short-lived
    /// CLI tools.
    #[must_use]
    pub fn open_in_memory() -> Self {
        Self {
            engine: Engine::new().with_clock(wall_clock_micros),
            persistence: None,
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
        if let Some(parent) = db_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
        let mut engine = if db_path.exists() {
            let bytes = std::fs::read(&db_path).map_err(io_err)?;
            let engine = Engine::restore_envelope(&bytes).map_err(|e| {
                EngineError::Storage(spg_storage::StorageError::Corrupt(format!(
                    "restore from {}: {e}",
                    db_path.display()
                )))
            })?;
            engine.with_clock(wall_clock_micros)
        } else {
            Engine::new().with_clock(wall_clock_micros)
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
        if wal_path.exists() {
            let wal_bytes = std::fs::read(&wal_path).map_err(io_err)?;
            if !wal_bytes.is_empty() {
                replay_wal_into_engine(&wal_bytes, &mut engine)
                    .map_err(|m| EngineError::Storage(spg_storage::StorageError::Corrupt(m)))?;
            }
        }
        let wal = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&wal_path)
            .map_err(io_err)?;
        let wal_len = wal.metadata().map_err(io_err)?.len();
        Ok(Self {
            engine,
            persistence: Some(PersistenceCtx {
                db_path,
                wal_path,
                wal,
                wal_len,
                checkpoint_threshold_bytes: default_checkpoint_threshold_bytes(),
                cold_segments_dir,
                cold_segment_paths,
            }),
        })
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

    /// v7.1 — flush a fresh catalog snapshot to `db_path` and
    /// truncate the WAL. Idempotent; cheap when nothing has
    /// happened since the last checkpoint. No-op when the
    /// database is in-memory (no `db_path` configured).
    ///
    /// Called automatically when:
    /// - the WAL grows past
    ///   `SPG_EMBEDDED_CHECKPOINT_BYTES` (default 4 MiB) at the
    ///   end of an `execute()`, and
    /// - `Drop` runs (best-effort; checkpoint failure on drop is
    ///   logged to stderr).
    pub fn checkpoint(&mut self) -> Result<(), EngineError> {
        let snapshot = self.engine.snapshot();
        let Some(p) = &mut self.persistence else {
            return Ok(());
        };
        // Snapshot first (atomic via tmp+rename), then WAL
        // truncate. Same order as `spg-server`'s CHECKPOINT —
        // a crash between the two leaves the WAL holding
        // already-snapshotted ops, which replay cleanly on the
        // next boot (idempotent for SPG's standard DDL/DML
        // mutations).
        let tmp = {
            let mut t = p.db_path.clone();
            let mut name = t
                .file_name()
                .map(std::ffi::OsStr::to_os_string)
                .unwrap_or_default();
            name.push(".tmp");
            t.set_file_name(name);
            t
        };
        std::fs::write(&tmp, &snapshot).map_err(io_err)?;
        std::fs::rename(&tmp, &p.db_path).map_err(io_err)?;
        // v7.1.4 — refresh the manifest so the next boot can
        // reload cold segments alongside the snapshot. Bytes
        // come from the freshly-written snapshot file (= the
        // canonical CRC source).
        if !p.cold_segment_paths.is_empty() {
            let snap_crc = spg_crypto::crc32::crc32(&snapshot);
            let entries: Vec<ColdSegmentEntry> = p
                .cold_segment_paths
                .iter()
                .filter_map(|(&segment_id, path)| {
                    let bytes = std::fs::read(path).ok()?;
                    Some(ColdSegmentEntry {
                        segment_id,
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
            let m_path = spg_manifest_path(&p.db_path);
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
        p.wal.set_len(0).map_err(io_err)?;
        p.wal.seek(SeekFrom::Start(0)).map_err(io_err)?;
        p.wal.sync_data().map_err(io_err)?;
        p.wal_len = 0;
        Ok(())
    }

    /// Restore a database from a previously-captured catalog
    /// snapshot. Pairs with `Database::snapshot()` for
    /// round-tripping in-memory state without going through
    /// the `spg-server` WAL.
    pub fn restore(snapshot: &[u8]) -> Result<Self, EngineError> {
        let engine = Engine::restore_envelope(snapshot).map_err(|e| {
            EngineError::Storage(spg_storage::StorageError::Corrupt(format!("restore: {e}")))
        })?;
        Ok(Self {
            engine,
            persistence: None,
        })
    }

    /// Take a catalog snapshot suitable for `Database::restore`.
    /// The bytes are SPG's canonical catalog envelope (FILE_MAGIC
    /// + version + payload); round-trips through every released
    /// SPG version per the STABILITY contract.
    #[must_use]
    pub fn snapshot(&self) -> Vec<u8> {
        self.engine.snapshot()
    }

    /// Execute a SQL statement and return the engine's
    /// `QueryResult` verbatim. Pass-through for callers that
    /// want to keep PG-flavoured column/row metadata.
    ///
    /// v7.1 — when the database was opened via `open_path`,
    /// successful mutations are appended to the WAL + fsynced
    /// before the call returns. A subsequent process crash will
    /// recover state up to the last successful return from
    /// `execute()`. Read-only statements (SELECT / SHOW /
    /// EXPLAIN / BEGIN-COMMIT-ROLLBACK / CHECKPOINT / COMPACT
    /// etc.) skip the WAL entirely.
    pub fn execute(&mut self, sql: &str) -> Result<QueryResult, EngineError> {
        let result = self.engine.execute(sql)?;
        if self.persistence.is_some()
            && !sql_is_read_only(sql)
            && matches!(
                &result,
                QueryResult::CommandOk {
                    modified_catalog: true,
                    ..
                }
            )
        {
            // Append + sync the v3 record AFTER the in-memory
            // exec succeeds, so a WAL record never describes a
            // mutation that didn't actually apply. The crash
            // window between in-memory commit and WAL fsync is
            // bounded by one record — replay re-applies the
            // statement idempotently on next boot if we crashed
            // between (and SPG's DDL/DML are crash-idempotent at
            // the granularities the wire protocol exposes).
            let record = encode_v3_auto_commit(sql);
            let p = self.persistence.as_mut().expect("checked above");
            p.wal.write_all(&record).map_err(io_err)?;
            p.wal.sync_data().map_err(io_err)?;
            p.wal_len = p.wal_len.saturating_add(record.len() as u64);
            if p.wal_len >= p.checkpoint_threshold_bytes {
                self.checkpoint()?;
            }
        }
        Ok(result)
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
        let stmt = self.engine.prepare_cached(sql).map_err(EngineError::Parse)?;
        Ok(Statement {
            stmt,
            sql: sql.to_string(),
        })
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
        let result = self.engine.execute_prepared(stmt.stmt.clone(), params)?;
        // WAL persistence on the bind-final SQL. Build the
        // canonical Display form by re-printing the
        // placeholder-substituted statement (cheap — the AST
        // is already in hand from execute_prepared's internal
        // clone) so replay's path is identical to the
        // simple-query path.
        if self.persistence.is_some()
            && matches!(
                &result,
                QueryResult::CommandOk {
                    modified_catalog: true,
                    ..
                }
            )
        {
            // Render the AST back to SQL for WAL replay. The
            // placeholder positions are already substituted in
            // the executed clone; we re-substitute on a fresh
            // clone here purely to obtain the canonical text.
            let mut wal_stmt = stmt.stmt.clone();
            // Use the engine's substitute_placeholders entry —
            // exposed via execute_prepared above. Here we
            // re-run the substitution only for Display.
            crate::wal_render_with_params(&mut wal_stmt, params);
            let canonical = format!("{wal_stmt}");
            let record = encode_v3_auto_commit(&canonical);
            let p = self.persistence.as_mut().expect("checked above");
            p.wal.write_all(&record).map_err(io_err)?;
            p.wal.sync_data().map_err(io_err)?;
            p.wal_len = p.wal_len.saturating_add(record.len() as u64);
            if p.wal_len >= p.checkpoint_threshold_bytes {
                self.checkpoint()?;
            }
        }
        Ok(result)
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
            Some(p) => (p.wal_len, true),
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
    let wal_bytes = std::fs::read(wal_path.as_ref()).map_err(io_err)?;
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
        engine.execute(sql)?;
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
    let payload = &tail[header_len..header_len + rec_len];
    let sql_bytes = if is_v3 {
        let type_byte = tail[8];
        // v3 type 0x01 = auto_commit_sql (payload = SQL).
        // v3 type 0x02 = durability marker (payload = u64
        // offset, no SQL to apply). Anything else is unknown.
        if type_byte == WAL_V3_TYPE_AUTO_COMMIT_SQL {
            payload.to_vec()
        } else {
            // Caller treats empty payload as a skip-marker.
            Vec::new()
        }
    } else {
        payload.to_vec()
    };
    Ok((sql_bytes, header_len + rec_len))
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

#[cfg(test)]
mod tests {
    use super::*;

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
