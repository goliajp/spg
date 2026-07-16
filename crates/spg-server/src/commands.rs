//! Server admin command handlers — STATS render, BACKUP, CHECKPOINT,
//! and COMPACT COLD SEGMENTS. Lifted out of `main.rs` (server file
//! split); re-exported at the crate root via `pub(crate) use commands::*`
//! so the dispatch call sites are unchanged.

use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};

use spg_wire::{build_command_complete, build_error_response};

use crate::wire::write_frame;
use crate::{ServerState, backup, parse_env_u64, write_atomic, write_manifest_alongside};

/// Render a `key=value`-per-line summary of server state for the Stats opcode.
/// Acquires the engine and audit locks; intentionally cheap (no per-table
/// row walk beyond `row_count()`).
pub(crate) fn render_stats(state: &ServerState) -> std::io::Result<String> {
    use std::fmt::Write as _;
    let engine = state
        .engine
        .read()
        .map_err(|_| std::io::Error::other("engine rwlock poisoned"))?;
    let audit = state
        .audit_log
        .lock()
        .map_err(|_| std::io::Error::other("audit mutex poisoned"))?;
    let catalog = engine.catalog();

    let mut out = String::new();
    writeln!(out, "spg_version={}", env!("CARGO_PKG_VERSION")).unwrap();
    writeln!(out, "tables={}", catalog.table_count()).unwrap();
    for i in 0..catalog.table_count() {
        // The catalog doesn't currently expose iteration directly; walk via
        // table name lookup via successive name(). It does have `.get()` but
        // not an iterator API. For v1.0 we ship the simplest stats.
        // Catalog has private `tables` field — use `get_*` on names…
        // Actually we have no name iterator. Skip per-table breakdown; emit
        // total row count instead.
        let _ = i; // suppress unused; placeholder until catalog grows iter.
    }
    // Total rows: walk via the public API. There is no rows-iterator on
    // Catalog yet; for v1.0 stats we report total table count and audit /
    // wal facts. Per-table row counts will require a Catalog::table_names()
    // addition — left for v1.1.
    writeln!(out, "in_transaction={}", engine.in_transaction()).unwrap();
    writeln!(out, "audit_entries={}", audit.len()).unwrap();
    writeln!(
        out,
        "db_path={}",
        state
            .db_path
            .as_deref()
            .map_or("<in-memory>".to_string(), |p| p.display().to_string())
    )
    .unwrap();
    writeln!(
        out,
        "audit_path={}",
        state
            .audit_path
            .as_deref()
            .map_or("<disabled>".to_string(), |p| p.display().to_string())
    )
    .unwrap();
    writeln!(
        out,
        "wal_path={}",
        state
            .wal_path
            .as_deref()
            .map_or("<disabled>".to_string(), |p| p.display().to_string())
    )
    .unwrap();
    Ok(out)
}

/// v4.25: parse `BACKUP TO '<path>'` and
/// `BACKUP TO '<path>' INCREMENTAL SINCE <n>` from the raw SQL
/// text. Returns None if the statement isn't a backup. Preserves
/// the path's original case (the lowercased form is only used to
/// recognise keywords).
pub(crate) fn parse_backup_intent(sql: &str) -> Option<BackupIntent> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let lower = trimmed.to_ascii_lowercase();
    let after_prefix = lower
        .strip_prefix("backup ")?
        .trim_start()
        .strip_prefix("to ")?
        .trim_start();
    let prefix_consumed = lower.len() - after_prefix.len();
    if !trimmed[prefix_consumed..].starts_with('\'') {
        return None;
    }
    let after_open = &trimmed[prefix_consumed + 1..];
    let close = after_open.find('\'')?;
    let path = after_open[..close].to_string();
    let tail = after_open[close + 1..].trim().to_ascii_lowercase();
    if tail.is_empty() {
        return Some(BackupIntent::Full { path });
    }
    let since_str = tail
        .strip_prefix("incremental ")?
        .trim_start()
        .strip_prefix("since ")?
        .trim_start();
    let since: u64 = since_str.parse().ok()?;
    Some(BackupIntent::Incremental { path, since })
}

#[derive(Debug)]
pub(crate) enum BackupIntent {
    Full { path: String },
    Incremental { path: String, since: u64 },
}

/// v5.3.2: parse the `CHECKPOINT` keyword. No arguments, no
/// variations — the SQL form is intentionally minimal because the
/// operation always means the same thing: snapshot the engine,
/// write the manifest, truncate the WAL.
pub(crate) fn parse_checkpoint_intent(sql: &str) -> bool {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    trimmed.eq_ignore_ascii_case("checkpoint")
}

/// v5.3.2 — `CHECKPOINT` handler. Writes a fresh snapshot to
/// `db_path`, an updated manifest to the sibling
/// `<db>.spg/manifest.v10`, and truncates the WAL file to 0 bytes.
/// The next boot loads the manifest, preloads every cold segment,
/// and starts WAL replay from byte 0 (which is now empty until the
/// next post-checkpoint write).
///
/// Single-fsync semantics: snapshot → manifest → WAL truncate is a
/// strict order. A crash between any two of those leaves the
/// system in a state the boot path can detect (snapshot CRC vs
/// manifest's `catalog_crc32`) and falls back to legacy
/// snapshot+WAL-from-0 replay. v5.3.x intentionally doesn't add a
/// CHECKPOINT WAL record; v5.4 manifest-with-WAL-coordination is
/// a separate trigger.
pub(crate) fn run_checkpoint_command(
    stream: &mut TcpStream,
    state: &ServerState,
) -> std::io::Result<()> {
    let Some(db_path) = state.db_path.as_deref() else {
        return write_frame(
            stream,
            &build_error_response("CHECKPOINT requires a db_path (server started without one)"),
        );
    };
    // Acquire write lock so no concurrent mutation can land between
    // snapshot capture and WAL truncate.
    let snapshot_bytes = {
        let engine = state
            .engine
            .write()
            .map_err(|_| std::io::Error::other("engine rwlock poisoned"))?;
        if engine.in_transaction() {
            return write_frame(
                stream,
                &build_error_response("CHECKPOINT refused: an open transaction is in flight"),
            );
        }
        let bytes = engine.snapshot();
        drop(engine);
        bytes
    };
    if let Err(e) = write_atomic(db_path, &snapshot_bytes) {
        return write_frame(
            stream,
            &build_error_response(&format!("CHECKPOINT snapshot write failed: {e}")),
        );
    }
    let cold_paths = state
        .cold_segment_paths
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    // Post-truncate the WAL will start at byte 0, so the manifest's
    // `wal_baseline_offset` for the *next* boot is also 0 — every
    // byte after this point is post-checkpoint and must be replayed.
    write_manifest_alongside(db_path, &snapshot_bytes, &cold_paths, 0);
    // Truncate WAL last — until the manifest lands, a crash here
    // would leave the WAL holding old bytes that the manifest CRC
    // check will detect on the next boot.
    if let Some(wal_mutex) = state.wal.as_ref() {
        let wal_lock = wal_mutex
            .lock()
            .map_err(|_| std::io::Error::other("WAL mutex poisoned"))?;
        if let Err(e) = wal_lock.set_len(0) {
            // Best-effort: log + report. The snapshot + manifest
            // already landed; on next boot the manifest's CRC will
            // match and the residual WAL bytes will replay as a
            // (defensive) no-op idempotency replay. Not a hard
            // failure.
            return write_frame(
                stream,
                &build_error_response(&format!("CHECKPOINT WAL truncate failed: {e}")),
            );
        }
        if let Err(e) = wal_lock.sync_data() {
            return write_frame(
                stream,
                &build_error_response(&format!("CHECKPOINT WAL sync failed: {e}")),
            );
        }
        drop(wal_lock);
    }
    // Return 0 in the affected-rows slot — there's no natural row
    // count for a checkpoint. Operators can poll `wal_path` size
    // afterwards to confirm the truncate.
    write_frame(stream, &build_command_complete(0))
}

/// v6.7.3 — parse `COMPACT COLD SEGMENTS` (case-insensitive,
/// whitespace-tolerant, trailing semicolon optional). The v6.7.3
/// SQL form takes no arguments; a future v6.7.x can extend with
/// `WHERE` predicates (currently STABILITY carve-out).
pub(crate) fn parse_compact_cold_segments_intent(sql: &str) -> bool {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let mut parts = trimmed.split_whitespace();
    matches!(parts.next(), Some(w) if w.eq_ignore_ascii_case("compact"))
        && matches!(parts.next(), Some(w) if w.eq_ignore_ascii_case("cold"))
        && matches!(parts.next(), Some(w) if w.eq_ignore_ascii_case("segments"))
        && parts.next().is_none()
}

/// v6.7.3 — read `SPG_COMPACTION_TARGET_SEGMENT_BYTES` (default
/// `COMPACTION_TARGET_DEFAULT_BYTES` = 4 MiB). Cached after first
/// call. Invalid values fall through to the default — operators
/// reading the spg-server stderr will see the parse failure.
pub(crate) fn compaction_target_bytes() -> u64 {
    static CHECKED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CHECKED.get_or_init(|| {
        parse_env_u64("SPG_COMPACTION_TARGET_SEGMENT_BYTES")
            .unwrap_or(spg_engine::COMPACTION_TARGET_DEFAULT_BYTES)
    })
}

/// v6.7.3 — `COMPACT COLD SEGMENTS` handler. Takes the engine
/// write lock, runs `Engine::compact_cold_segments_with_target`,
/// persists each merged segment to
/// `<db>.spg/segments/seg_<merged_id>.spg`, and updates
/// `cold_segment_paths` (remove sources, add merged) so the next
/// CHECKPOINT writes a manifest that no longer lists the retired
/// sources. Returns one `CommandComplete` carrying the count of
/// merges that ran.
pub(crate) fn run_compact_cold_segments_command(
    stream: &mut TcpStream,
    state: &ServerState,
) -> std::io::Result<()> {
    let target = compaction_target_bytes();
    let reports = {
        let mut engine = state
            .engine
            .write()
            .map_err(|_| std::io::Error::other("engine rwlock poisoned"))?;
        if engine.in_transaction() {
            return write_frame(
                stream,
                &build_error_response(
                    "COMPACT COLD SEGMENTS refused: an open transaction is in flight",
                ),
            );
        }
        match engine.compact_cold_segments_with_target(target) {
            Ok(r) => r,
            Err(e) => {
                return write_frame(
                    stream,
                    &build_error_response(&format!("COMPACT COLD SEGMENTS failed: {e:?}")),
                );
            }
        }
    };

    let merged_count = reports.len();
    // Persist every merged segment to disk + update the in-memory
    // path map. A persist failure is logged + reported but doesn't
    // roll back the in-memory swap — the in-memory state is the
    // source of truth until the next CHECKPOINT writes a manifest,
    // and the legacy SPG_PRELOAD_COLD_SEGMENT path can pick up
    // anything the manifest path missed.
    if let Some(db_path) = state.db_path.as_deref() {
        for (_tname, _iname, report) in &reports {
            let Some(merged_id) = report.merged_segment_id else {
                continue;
            };
            match persist_compact_merged_segment(db_path, merged_id, &report.merged_segment_bytes) {
                Ok(merged_path) => {
                    if let Ok(mut paths) = state.cold_segment_paths.lock() {
                        for src in &report.sources {
                            paths.remove(src);
                        }
                        paths.insert(merged_id, merged_path);
                    }
                }
                Err(e) => {
                    eprintln!(
                        "spg-server: COMPACT persist of merged segment {merged_id} failed: {e}"
                    );
                }
            }
        }
        state.metrics.cold_segments.store(
            state
                .engine
                .read()
                .ok()
                .map(|e| e.catalog().cold_segment_count() as u64)
                .unwrap_or(0),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
    write_frame(stream, &build_command_complete(merged_count as u64))
}

/// v6.7.3 — write a compaction-merged segment to
/// `<parent>/<db_stem>.spg/segments/seg_<merged_id>.spg` via the
/// same tmp+rename atomicity that `freezer::persist_segment` uses.
/// Honours the v6.6.2 segment v2-envelope compression knob
/// (`SPG_SEGMENT_COMPRESSION`).
pub(crate) fn persist_compact_merged_segment(
    db_path: &Path,
    merged_id: u32,
    merged_segment_bytes: &[u8],
) -> std::io::Result<PathBuf> {
    let parent = db_path.parent().unwrap_or_else(|| Path::new("."));
    let stem = db_path
        .file_stem()
        .unwrap_or_else(|| std::ffi::OsStr::new("db"))
        .to_string_lossy();
    let seg_dir = parent.join(format!("{stem}.spg")).join("segments");
    fs::create_dir_all(&seg_dir)?;
    let final_path = seg_dir.join(format!("seg_{merged_id}.spg"));
    let tmp_path = seg_dir.join(format!("seg_{merged_id}.spg.tmp"));
    let bytes_to_write = if std::env::var("SPG_SEGMENT_COMPRESSION")
        .map_or(true, |v| !v.eq_ignore_ascii_case("none"))
    {
        spg_storage::wrap_v2_envelope(merged_segment_bytes.to_vec(), true)
    } else {
        merged_segment_bytes.to_vec()
    };
    // v7.39 (round 147, durable-rename audit) — fsync bytes + dir entry so a
    // crash cannot leave the catalog referencing an empty / missing segment.
    {
        use std::io::Write;
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(&bytes_to_write)?;
        f.sync_all()?;
    }
    fs::rename(&tmp_path, &final_path)?;
    crate::fsync_dir(&seg_dir);
    Ok(final_path)
}

pub(crate) fn run_backup_command(
    stream: &mut TcpStream,
    state: &ServerState,
    intent: &BackupIntent,
) -> std::io::Result<()> {
    let result = match intent {
        BackupIntent::Full { path } => backup::take_full_backup(state, Path::new(path)),
        BackupIntent::Incremental { path, since } => {
            backup::take_incremental_backup(state, Path::new(path), *since)
        }
    };
    match result {
        // Re-use the existing `affected rows` slot to ship the
        // captured WAL position back to the caller — it's the
        // number an incremental backup will pass as SINCE.
        Ok(wal_pos) => write_frame(stream, &build_command_complete(wal_pos)),
        Err(e) => write_frame(
            stream,
            &build_error_response(&format!("backup failed: {e}")),
        ),
    }
}
