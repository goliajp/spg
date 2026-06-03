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
//! let mut db = Database::open_in_memory();
//! db.execute("CREATE TABLE users (id INT NOT NULL, name TEXT)").unwrap();
//! db.execute("INSERT INTO users VALUES (1, 'alice')").unwrap();
//! let rows = db.query("SELECT name FROM users WHERE id = 1").unwrap();
//! for row in &rows {
//!     println!("{:?}", row);
//! }
//! ```
//!
//! ## v6.10.3 scope
//!
//! v6.10.3 ships the crate scaffold + a thin `Database` wrapper
//! that:
//!
//! - Constructs an `Engine` (in-memory or restored from a
//!   catalog snapshot byte slice).
//! - Forwards `execute(sql)` directly to the engine.
//! - Returns query results as a `Vec<Vec<Value>>` for
//!   read-side ergonomics.
//!
//! The following are explicit **STABILITY § "Out of v6.10"**
//! carve-outs:
//!
//! - **Typed query API**: `db.query::<User>("SELECT …")` that
//!   row-decodes into a user struct. Lands once the macro
//!   landed; until then, callers pattern-match on `Value`.
//! - **`#[derive(SpgRow)]`**: proc-macro that generates the
//!   `FromRow` impl mapping schema columns → struct fields.
//!   Needs a new proc-macro crate (`spg-embedded-macros`); the
//!   shape is reserved by the trait sketch below.
//! - **On-disk persistence**: `Database::open_path(p)` that
//!   restores from a catalog snapshot + drives a WAL.
//!   v6.10.3 ships in-memory + byte-slice round-trip;
//!   persistence is `spg-server`'s job today.
//!
//! ## Why a separate crate?
//!
//! `spg-engine` is `no_std`-compatible (vendored alloc-only).
//! The embedded-mode entry point uses `std` (filesystem,
//! threading), so it lives in its own crate to keep the
//! `no_std` boundary clean.

pub use spg_engine::{Engine, EngineError, QueryResult};
pub use spg_storage::Value;

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

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
        let sql = std::str::from_utf8(sql_bytes).map_err(|e| format!("WAL replay: non-UTF-8 SQL at offset {cur}: {e}"))?;
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
            engine: Engine::new(),
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
            engine
        } else {
            Engine::new()
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
                            if engine
                                .catalog()
                                .cold_segment(entry.segment_id)
                                .is_some()
                            {
                                // Already loaded via Catalog::clone path (shouldn't happen
                                // since Engine::new + restore_envelope don't populate cold).
                                continue;
                            }
                            let mut new_cat = engine.catalog().clone();
                            if let Err(e) = new_cat
                                .load_segment_bytes_at(entry.segment_id, seg_bytes)
                            {
                                eprintln!(
                                    "spg-embedded: manifest load segment {} failed: {e}",
                                    entry.segment_id
                                );
                                continue;
                            }
                            engine.replace_catalog(new_cat);
                            cold_segment_paths
                                .insert(entry.segment_id, entry.path.clone());
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
        let engine = Engine::restore_envelope(snapshot)
            .map_err(|e| EngineError::Storage(spg_storage::StorageError::Corrupt(format!("restore: {e}"))))?;
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
            && matches!(&result, QueryResult::CommandOk { modified_catalog: true, .. })
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
}

impl Default for Database {
    fn default() -> Self {
        Self::open_in_memory()
    }
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

/// v6.10.3 — sketch trait for the future `#[derive(SpgRow)]`
/// proc-macro to implement. The trait shape is reserved now so
/// downstream callers can write impl blocks by hand against a
/// stable signature; the proc-macro crate lands as a
/// STABILITY carve-out follow-up.
///
/// Implementors map a row's columns onto a user struct's
/// fields. Errors surface as `EngineError::Unsupported` so the
/// caller's error type stays uniform.
pub trait FromSpgRow: Sized {
    fn from_spg_row(row: &[Value]) -> Result<Self, EngineError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_create_insert_select() {
        let mut db = Database::open_in_memory();
        db.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)").unwrap();
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
