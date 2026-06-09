//! SPG CLI.
//!
//! Subcommands:
//! - `spg ping [addr]`                  — sanity check the daemon is reachable.
//! - `spg query <sql> [addr]`           — send SQL, print the result or error.
//! - `spg stats [addr]`                 — fetch server stats.
//! - `spg backup <src> <dst>`           — copy a `.spgdb` file with validation.
//! - `spg restore <src> <dst>`          — alias of backup (file-level symmetry).
//! - `spg version`                      — print CLI version.

use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process;
use std::time::Duration;

use spg_storage::Catalog;
use spg_wire::{
    ColumnDesc, Frame, FrameError, Op, WireValue, build_auth, build_query, build_stats_request,
    encode, parse_command_complete, parse_data_row, parse_data_row_batch, parse_error_response,
    parse_row_description, parse_stats_response,
};

const DEFAULT_ADDR: &str = "127.0.0.1:5544";
const READ_TIMEOUT: Duration = Duration::from_secs(10);

fn main() {
    let mut args = env::args().skip(1);
    let cmd = args.next();
    match cmd.as_deref() {
        Some("ping") => {
            let addr = args.next().unwrap_or_else(|| DEFAULT_ADDR.to_string());
            match ping(&addr) {
                Ok(()) => println!("PONG"),
                Err(e) => die(&format!("ping failed: {e}"), 1),
            }
        }
        Some("query") => {
            let Some(sql) = args.next() else {
                die("usage: spg query <sql> [addr]", 2);
                return;
            };
            let addr = args.next().unwrap_or_else(|| DEFAULT_ADDR.to_string());
            match query(&addr, &sql) {
                Ok(()) => {}
                Err(e) => die(&format!("query failed: {e}"), 1),
            }
        }
        Some("stats") => {
            let addr = args.next().unwrap_or_else(|| DEFAULT_ADDR.to_string());
            match stats(&addr) {
                Ok(text) => print!("{text}"),
                Err(e) => die(&format!("stats failed: {e}"), 1),
            }
        }
        Some("version") => {
            println!("spg {}", env!("CARGO_PKG_VERSION"));
        }
        Some(verb @ ("backup" | "restore")) => {
            let Some(src) = args.next() else {
                die(&format!("usage: spg {verb} <src> <dst>"), 2);
                return;
            };
            let Some(dst) = args.next() else {
                die(&format!("usage: spg {verb} <src> <dst>"), 2);
                return;
            };
            match backup(&src, &dst) {
                Ok(tables) => println!("spg {verb}: validated {tables} table(s); wrote {dst}"),
                Err(e) => die(&format!("{verb} failed: {e}"), 1),
            }
        }
        // v6.10.7 — audit-driven PITR. `spg revert --wal
        // <path> --to-seq <N> --out <db_path>` replays the
        // first N records of the WAL into a fresh engine + writes
        // the resulting snapshot to `--out`. The `--to-audit-entry`
        // variant (resolve N from an audit-chain entry hash) is
        // STABILITY § "Out of v6.10" — the v6.10.7 ship freezes
        // the CLI shape so the future revisit drops in the audit
        // lookup without changing the operator surface.
        // v7.18 PITR P4 — `spg backup-pitr --src <db_path>
        // --dst <backup_dir>` copies the catalog snapshot + the
        // current WAL into a layout pitr-restore can consume:
        //   <dst>/snapshot.spg
        //   <dst>/wal/<unix_us>_<max_lsn>.wal
        // The dst directory is created if absent. Live-daemon
        // safety is the caller's responsibility for v7.18 — P6
        // wires chunk rotation + atomic snapshot capture into
        // the engine so backups stay self-consistent under
        // concurrent writes; P4 just ships the file-copy layer
        // the rest of the PITR sub-epic builds on.
        Some("backup-pitr") => {
            let mut src: Option<String> = None;
            let mut dst: Option<String> = None;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--src" => src = args.next(),
                    "--dst" => dst = args.next(),
                    other => {
                        die(&format!("unknown backup-pitr arg: {other}"), 2);
                        return;
                    }
                }
            }
            let (Some(src), Some(dst)) = (src, dst) else {
                die(
                    "usage: spg backup-pitr --src <db_path> --dst <backup_dir>",
                    2,
                );
                return;
            };
            match backup_pitr(&src, &dst) {
                Ok(report) => println!("{report}"),
                Err(e) => die(&format!("backup-pitr failed: {e}"), 1),
            }
        }
        // v7.18 PITR P3 — `spg pitr-restore --snapshot <file>
        // --wal <file> --to <timestamp> --target <out_path>`.
        // Replays WAL records up to <timestamp> (or LSN, when
        // --to is bare digits) on top of <snapshot>, writes the
        // resulting catalog to <target>.
        //
        // --to formats:
        //   - bare integer: treated as commit_lsn upper bound
        //   - <int>s / <int>ms / <int>us: treated as unix epoch
        //     seconds / millis / micros
        //   - ISO 8601 'YYYY-MM-DD HH:MM:SS' or 'YYYY-MM-DDTHH:MM:SS'
        //     (UTC assumed; no timezone offset parsing yet)
        Some("pitr-restore") => {
            let mut snapshot_path: Option<String> = None;
            let mut wal_path: Option<String> = None;
            let mut to_arg: Option<String> = None;
            let mut target_path: Option<String> = None;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--snapshot" => snapshot_path = args.next(),
                    "--wal" => wal_path = args.next(),
                    "--to" => to_arg = args.next(),
                    "--target" => target_path = args.next(),
                    other => {
                        die(&format!("unknown pitr-restore arg: {other}"), 2);
                        return;
                    }
                }
            }
            let (Some(snapshot_path), Some(wal_path), Some(to_arg), Some(target_path)) =
                (snapshot_path, wal_path, to_arg, target_path)
            else {
                die(
                    "usage: spg pitr-restore --snapshot <file> --wal <file> \
                     --to <timestamp|lsn> --target <out_path>",
                    2,
                );
                return;
            };
            match pitr_restore(&snapshot_path, &wal_path, &to_arg, &target_path) {
                Ok((applied, target_descr)) => {
                    println!(
                        "OK applied={applied} target={target_descr} → {target_path}"
                    );
                }
                Err(msg) => die(&format!("pitr-restore failed: {msg}"), 1),
            }
        }
        Some("revert") => {
            let mut wal_path: Option<String> = None;
            let mut to_seq: Option<u64> = None;
            let mut out_path: Option<String> = None;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--wal" => wal_path = args.next(),
                    "--to-seq" => {
                        to_seq = args.next().and_then(|s| s.parse::<u64>().ok());
                    }
                    "--to-audit-entry" => {
                        die(
                            "--to-audit-entry is STABILITY § Out-of-v6.10; v6.10.7 \
                             supports --to-seq <N> only",
                            2,
                        );
                        return;
                    }
                    "--out" => out_path = args.next(),
                    other => {
                        die(&format!("unknown revert arg: {other}"), 2);
                        return;
                    }
                }
            }
            let (Some(wal_path), Some(to_seq), Some(out_path)) = (wal_path, to_seq, out_path)
            else {
                die(
                    "usage: spg revert --wal <path> --to-seq <N> --out <db_path>",
                    2,
                );
                return;
            };
            match wal_revert(&wal_path, to_seq, &out_path) {
                Ok(applied) => {
                    println!("OK applied={applied} → {out_path}");
                }
                Err(msg) => die(&format!("revert failed: {msg}"), 1),
            }
        }
        // v6.10.5 — WAL schema lint. `spg wal-lint <wal_path>
        // --against-schema <db_path>` parses every record in
        // the WAL file + checks each SQL statement against the
        // catalog snapshot at `db_path` (dry-run apply on a
        // clone). Prints `OK <n>` on success, `FAIL <offset>:
        // <msg>` on the first rejected record.
        Some("wal-lint") => {
            let Some(wal_path) = args.next() else {
                die(
                    "usage: spg wal-lint <wal_path> --against-schema <db_path>",
                    2,
                );
                return;
            };
            let mut db_path: Option<String> = None;
            while let Some(a) = args.next() {
                if a == "--against-schema" {
                    db_path = args.next();
                } else {
                    die(&format!("unknown wal-lint arg: {a}"), 2);
                    return;
                }
            }
            let Some(db_path) = db_path else {
                die("wal-lint: --against-schema <db_path> required", 2);
                return;
            };
            match wal_lint(&wal_path, &db_path) {
                Ok(applied) => println!("OK {applied}"),
                Err((offset, msg)) => {
                    eprintln!("FAIL {offset}: {msg}");
                    process::exit(1);
                }
            }
        }
        Some(other) => die(&format!("unknown command: {other}"), 2),
        None => die(
            "usage: spg <ping|query|stats|backup|restore|wal-lint|revert|version> ...",
            2,
        ),
    }
}

/// v6.10.7 — replay the first `to_seq` records of the WAL at
/// `wal_path` into a fresh engine + write the resulting catalog
/// snapshot to `out_path`. `to_seq == 0` is a special case
/// meaning "replay no records" — the snapshot is the empty
/// catalog. Returns the count of records applied.
/// v7.18 PITR P4 — copy a SPG database's catalog snapshot + WAL
/// into a backup directory layout `pitr-restore` can consume.
///
/// Output layout:
///
///   <dst>/snapshot.spg                 — bit-for-bit copy of <src>
///   <dst>/wal/<unix_us>_<max_lsn>.wal  — bit-for-bit copy of <src>.wal
///
/// The directory and the `wal/` subdir are created if absent.
/// Returns a human-readable summary line for stdout.
///
/// Live-daemon coordination is not enforced here — callers either
/// pause the daemon, accept the WAL chunk being a snapshot of a
/// concurrently-growing file (the chunk will deserialize cleanly
/// to whichever record boundary the read happens to catch),
/// or wait for the P6 atomic-snapshot capture path.
fn backup_pitr(src: &str, dst: &str) -> Result<String, String> {
    use spg_embedded::{WalRecord, parse_wal_records};
    let src_path = std::path::PathBuf::from(src);
    let dst_dir = std::path::PathBuf::from(dst);
    fs::create_dir_all(&dst_dir).map_err(|e| format!("create dst dir {dst}: {e}"))?;
    let wal_dir = dst_dir.join("wal");
    fs::create_dir_all(&wal_dir).map_err(|e| format!("create wal dir: {e}"))?;

    // 1) Snapshot — required.
    let snap_bytes = fs::read(&src_path).map_err(|e| format!("read snapshot {src}: {e}"))?;
    let snap_target = dst_dir.join("snapshot.spg");
    fs::write(&snap_target, &snap_bytes)
        .map_err(|e| format!("write snapshot {}: {e}", snap_target.display()))?;

    // 2) WAL — may be empty / absent (fresh database). Allow
    //    missing files but error on read failures.
    let src_wal = {
        let mut p = src_path.clone();
        let mut name = p
            .file_name()
            .map(std::ffi::OsStr::to_os_string)
            .unwrap_or_default();
        name.push(".wal");
        p.set_file_name(name);
        p
    };
    let (wal_bytes, wal_present) = match fs::read(&src_wal) {
        Ok(b) => (b, true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), false),
        Err(e) => return Err(format!("read wal {}: {e}", src_wal.display())),
    };

    let mut max_lsn: u64 = 0;
    if !wal_bytes.is_empty() {
        let recs: Vec<WalRecord<'_>> = parse_wal_records(&wal_bytes)
            .map_err(|e| format!("parse wal for naming: {e}"))?;
        for r in &recs {
            if let Some(l) = r.commit_lsn {
                if l > max_lsn {
                    max_lsn = l;
                }
            }
        }
    }
    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros());
    let chunk_name = format!("{now_us}_{max_lsn}.wal");
    let chunk_path = wal_dir.join(&chunk_name);
    if !wal_bytes.is_empty() {
        fs::write(&chunk_path, &wal_bytes)
            .map_err(|e| format!("write chunk {}: {e}", chunk_path.display()))?;
    }

    Ok(format!(
        "OK snapshot={} wal_present={} chunk={} max_lsn={}",
        snap_target.display(),
        wal_present,
        if wal_bytes.is_empty() {
            "(empty)".to_string()
        } else {
            chunk_path.display().to_string()
        },
        max_lsn,
    ))
}

/// v7.18 PITR P3 — point-in-time restore.
///
/// Loads the catalog snapshot at `snapshot_path` into a fresh
/// in-process database, parses the WAL at `wal_path`, replays
/// every `auto_commit_sql` record whose (commit_lsn,
/// commit_unix_us) falls at or before the target, then writes
/// the resulting catalog to `target_path`. Returns
/// `(applied_count, human_target_descr)`.
///
/// `to_arg` accepts:
///   - bare unsigned integer ⇒ commit_lsn upper bound
///   - `<n>s` / `<n>ms` / `<n>us` ⇒ unix epoch in that unit
///   - `YYYY-MM-DD HH:MM:SS` / `YYYY-MM-DDTHH:MM:SS` ⇒ UTC
fn pitr_restore(
    snapshot_path: &str,
    wal_path: &str,
    to_arg: &str,
    target_path: &str,
) -> Result<(u64, String), String> {
    use spg_embedded::{Database, WalRecord, parse_wal_records};

    let target = parse_restore_target(to_arg)?;
    let snapshot_bytes = fs::read(snapshot_path)
        .map_err(|e| format!("read snapshot {snapshot_path}: {e}"))?;
    let mut db = Database::restore(&snapshot_bytes)
        .map_err(|e| format!("restore snapshot: {e:?}"))?;

    let wal_bytes = fs::read(wal_path).map_err(|e| format!("read wal {wal_path}: {e}"))?;
    let records: Vec<WalRecord<'_>> =
        parse_wal_records(&wal_bytes).map_err(|e| format!("parse wal: {e}"))?;

    let mut applied: u64 = 0;
    for r in records {
        // Skip everything that doesn't carry SQL — durability
        // markers (0x02) and checkpoint markers (0x11) don't
        // replay; v3 SQL records (0x01) carry no LSN/ts and
        // always apply (they pre-date PITR — replay them
        // unconditionally so the pre-v7.18 history isn't lost).
        match r.type_byte {
            0x02 | 0x11 => continue,
            0x01 => {
                let sql = std::str::from_utf8(r.sql)
                    .map_err(|e| format!("non-UTF-8 SQL at offset {}: {e}", r.offset))?;
                db.execute(sql)
                    .map_err(|e| format!("apply at offset {}: {e:?}", r.offset))?;
                applied += 1;
            }
            0x10 => {
                if !target.includes(r.commit_lsn, r.commit_unix_us) {
                    continue;
                }
                let sql = std::str::from_utf8(r.sql)
                    .map_err(|e| format!("non-UTF-8 SQL at offset {}: {e}", r.offset))?;
                db.execute(sql)
                    .map_err(|e| format!("apply at offset {}: {e:?}", r.offset))?;
                applied += 1;
            }
            other => {
                return Err(format!(
                    "unknown WAL record type {other:#04x} at offset {}",
                    r.offset
                ));
            }
        }
    }

    let final_snapshot = db.snapshot();
    fs::write(target_path, &final_snapshot)
        .map_err(|e| format!("write {target_path}: {e}"))?;
    Ok((applied, target.describe()))
}

/// v7.18 PITR — the `--to` target parsed off the CLI.
#[derive(Debug)]
enum RestoreTarget {
    Lsn(u64),
    UnixMicros(i64),
}

impl RestoreTarget {
    fn includes(&self, lsn: Option<u64>, ts_us: Option<i64>) -> bool {
        match self {
            RestoreTarget::Lsn(cap) => lsn.is_some_and(|l| l <= *cap),
            RestoreTarget::UnixMicros(cap_us) => ts_us.is_some_and(|t| t <= *cap_us),
        }
    }
    fn describe(&self) -> String {
        match self {
            RestoreTarget::Lsn(n) => format!("lsn<={n}"),
            RestoreTarget::UnixMicros(us) => format!("ts<={us}us"),
        }
    }
}

fn parse_restore_target(s: &str) -> Result<RestoreTarget, String> {
    let trimmed = s.trim();
    if let Ok(n) = trimmed.parse::<u64>() {
        return Ok(RestoreTarget::Lsn(n));
    }
    if let Some(rest) = trimmed.strip_suffix("us") {
        if let Ok(n) = rest.parse::<i64>() {
            return Ok(RestoreTarget::UnixMicros(n));
        }
    }
    if let Some(rest) = trimmed.strip_suffix("ms") {
        if let Ok(n) = rest.parse::<i64>() {
            return Ok(RestoreTarget::UnixMicros(n.saturating_mul(1_000)));
        }
    }
    if let Some(rest) = trimmed.strip_suffix('s') {
        if let Ok(n) = rest.parse::<i64>() {
            return Ok(RestoreTarget::UnixMicros(n.saturating_mul(1_000_000)));
        }
    }
    // Try YYYY-MM-DD HH:MM:SS / YYYY-MM-DDTHH:MM:SS, UTC.
    let cleaned = trimmed.replace('T', " ");
    let parts: Vec<&str> = cleaned.split_whitespace().collect();
    if parts.len() == 2 {
        let date: Vec<&str> = parts[0].split('-').collect();
        let time: Vec<&str> = parts[1].split(':').collect();
        if date.len() == 3 && time.len() == 3 {
            let y: i64 = date[0].parse().map_err(|_| format!("bad year: {}", date[0]))?;
            let mo: i64 = date[1].parse().map_err(|_| format!("bad month: {}", date[1]))?;
            let d: i64 = date[2].parse().map_err(|_| format!("bad day: {}", date[2]))?;
            let h: i64 = time[0].parse().map_err(|_| format!("bad hour: {}", time[0]))?;
            let mi: i64 = time[1].parse().map_err(|_| format!("bad minute: {}", time[1]))?;
            let se: i64 = time[2].parse().map_err(|_| format!("bad second: {}", time[2]))?;
            // Days-from-civil from Howard Hinnant's date algorithms
            // (public domain). y, mo, d are calendar values; output
            // is days since 1970-01-01 UTC. Works for any positive
            // proleptic Gregorian date.
            let ymd_to_days = |y: i64, mo: i64, d: i64| -> i64 {
                let y = if mo <= 2 { y - 1 } else { y };
                let era = if y >= 0 { y } else { y - 399 } / 400;
                let yoe = y - era * 400;
                let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
                let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
                era * 146_097 + doe - 719_468
            };
            let days = ymd_to_days(y, mo, d);
            let secs = days * 86_400 + h * 3_600 + mi * 60 + se;
            return Ok(RestoreTarget::UnixMicros(secs.saturating_mul(1_000_000)));
        }
    }
    Err(format!(
        "could not parse --to {s:?}; expected unsigned LSN, '<n>s|ms|us' unix epoch, or 'YYYY-MM-DD HH:MM:SS' UTC"
    ))
}

fn wal_revert(wal_path: &str, to_seq: u64, out_path: &str) -> Result<u64, String> {
    use spg_engine::Engine;
    let mut engine = Engine::new();
    let wal_bytes = fs::read(wal_path).map_err(|e| format!("read wal: {e}"))?;
    let mut applied = 0u64;
    let mut cur = 0usize;
    while cur < wal_bytes.len() && applied < to_seq {
        let (sql_bytes, total) = decode_one_record(&wal_bytes[cur..])
            .map_err(|e| format!("decode at offset {cur}: {e}"))?;
        cur += total;
        if sql_bytes.is_empty() {
            // v3 durability-checkpoint marker — skips, doesn't
            // count against the budget (matches `replay_wal_bytes`
            // semantics).
            continue;
        }
        let sql = std::str::from_utf8(&sql_bytes)
            .map_err(|e| format!("non-UTF-8 SQL at offset {cur}: {e}"))?;
        engine
            .execute(sql)
            .map_err(|e| format!("apply rejected {sql:?} at seq {applied}: {e:?}"))?;
        applied += 1;
    }
    let snapshot = engine.snapshot();
    fs::write(out_path, &snapshot).map_err(|e| format!("write {out_path}: {e}"))?;
    Ok(applied)
}

/// v6.10.5 — dry-run apply every WAL record at `wal_path` to
/// a fresh `Engine` restored from the catalog snapshot at
/// `db_path`. Returns the count of records successfully
/// applied on full success; `(byte_offset, error_msg)` on the
/// first rejection. No persistence — the engine is dropped at
/// fn exit.
fn wal_lint(wal_path: &str, db_path: &str) -> Result<usize, (u64, String)> {
    use spg_engine::Engine;
    let snapshot = fs::read(db_path).map_err(|e| (0u64, format!("read schema {db_path}: {e}")))?;
    let mut engine =
        Engine::restore_envelope(&snapshot).map_err(|e| (0u64, format!("restore schema: {e}")))?;
    let wal_bytes = fs::read(wal_path).map_err(|e| (0u64, format!("read wal {wal_path}: {e}")))?;
    // Iterate records via the same v1/v2/v3 dispatch the server
    // boot path uses. We track offsets so a rejection points at
    // the exact byte where the offending record starts.
    let mut applied = 0usize;
    let mut cur = 0usize;
    while cur < wal_bytes.len() {
        let (sql_bytes, header_plus_payload) = decode_one_record(&wal_bytes[cur..])
            .map_err(|e| (cur as u64, format!("decode: {e}")))?;
        let sql = std::str::from_utf8(&sql_bytes)
            .map_err(|e| (cur as u64, format!("non-UTF-8 SQL: {e}")))?;
        if let Err(e) = engine.execute(sql) {
            return Err((cur as u64, format!("apply rejected {sql:?}: {e:?}")));
        }
        applied += 1;
        cur += header_plus_payload;
    }
    Ok(applied)
}

/// v6.10.5 — decode one WAL record from a byte tail. Returns
/// `(sql_bytes, total_header_plus_payload_len)`. Handles the
/// three on-disk formats (v1: 4-byte len; v2: 4-byte
/// `len|0x8000_0000` + 4-byte CRC; v3: 4-byte
/// `len|0xC000_0000` + 4-byte CRC + 1-byte type) just like
/// `replay_wal_bytes`. CRCs are not re-validated here — the
/// caller's intent is "does the SQL string parse + apply
/// against the schema?", not "is the WAL byte stream itself
/// valid?".
fn decode_one_record(tail: &[u8]) -> Result<(Vec<u8>, usize), String> {
    if tail.len() < 4 {
        return Err(format!("truncated record: {} < 4 header bytes", tail.len()));
    }
    let raw_len = u32::from_le_bytes(tail[..4].try_into().unwrap());
    const WAL_V2_SENTINEL: u32 = 0x8000_0000;
    const WAL_V3_FLAG: u32 = 0x4000_0000;
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
        return Err(format!(
            "truncated payload: need {} bytes, got {}",
            header_len + rec_len,
            tail.len()
        ));
    }
    if is_v3 {
        let type_byte = tail[8];
        // 0x01 = auto_commit_sql; 0x02 = durability checkpoint
        // (skip — no SQL to apply); 0x03 = compressed SQL.
        match type_byte {
            0x01 => {}
            0x02 => {
                return Ok((Vec::new(), header_len + rec_len));
            }
            0x03 => {
                // v6.6.1 LZSS-compressed SQL. Decompress on the
                // fly so the lint applies the canonical text.
                let compressed = &tail[header_len..header_len + rec_len];
                if compressed.is_empty() {
                    return Err("v3 compressed record: empty body".into());
                }
                let algo = compressed[0];
                if algo != 0x01 {
                    return Err(format!(
                        "v3 compressed record: unknown algo byte {algo:#04x}"
                    ));
                }
                let decompressed = spg_crypto::lzss::decompress(&compressed[1..])
                    .map_err(|e| format!("lzss decompress: {e:?}"))?;
                return Ok((decompressed, header_len + rec_len));
            }
            other => {
                return Err(format!("v3 unknown type byte {other:#04x}"));
            }
        }
    }
    let payload = tail[header_len..header_len + rec_len].to_vec();
    Ok((payload, header_len + rec_len))
}

/// Read a `.spgdb` catalog file, validate by round-tripping through the
/// Catalog deserialize → serialize path, write the validated bytes to
/// `dst`. Returns the number of tables in the catalog on success. Used
/// for both `spg backup` and `spg restore` — the file-level operation
/// is symmetric, the verb is just operator-facing context.
///
/// Both paths reject the operation on read / parse / write failure, so
/// a successful return is a hard guarantee that `dst` holds a parseable
/// catalog of the current file-format version.
///
/// Same path for both verbs because the operation is the same: read,
/// validate, re-serialize, write. The verb only changes how the human
/// describes intent ("save a copy" vs "load a copy back"). Splitting
/// them into two functions would just be ceremony.
fn backup(src: &str, dst: &str) -> Result<usize, String> {
    let src_path = Path::new(src);
    let dst_path = Path::new(dst);
    if src_path == dst_path {
        return Err("src and dst must not be the same path".into());
    }
    let bytes = fs::read(src_path).map_err(|e| format!("read {src}: {e}"))?;
    let catalog =
        Catalog::deserialize(&bytes).map_err(|e| format!("parse {src} as catalog: {e}"))?;
    let table_count = catalog.table_count();
    let out = catalog.serialize();
    fs::write(dst_path, out).map_err(|e| format!("write {dst}: {e}"))?;
    Ok(table_count)
}

/// Pull the password from `SPG_PASSWORD` (empty string treated as
/// "no password"). Returns `Ok(None)` when nothing is configured.
fn env_password() -> Option<String> {
    env::var("SPG_PASSWORD").ok().filter(|s| !s.is_empty())
}

/// Send `AUTH <password>` and consume the reply. No-op when no
/// password is configured — keeps the open-instance code path branchless
/// at every call site.
fn maybe_authenticate(stream: &mut TcpStream) -> Result<(), String> {
    let Some(pw) = env_password() else {
        return Ok(());
    };
    let mut out = Vec::new();
    encode(&build_auth(&pw), &mut out).map_err(|e| format!("encode AUTH: {e}"))?;
    stream
        .write_all(&out)
        .map_err(|e| format!("write AUTH: {e}"))?;
    let frame = read_one_frame(stream)?;
    match frame.op {
        Op::Pong => Ok(()),
        Op::ErrorResponse | Op::Error => {
            let msg =
                parse_error_response(&frame).map_or_else(|_| "<undecodable>".into(), str::to_owned);
            Err(format!("AUTH rejected: {msg}"))
        }
        other => Err(format!("unexpected AUTH reply op {other:?}")),
    }
}

fn stats(addr: &str) -> Result<String, String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|e| format!("set_read_timeout: {e}"))?;
    maybe_authenticate(&mut stream)?;
    let mut out = Vec::new();
    encode(&build_stats_request(), &mut out).map_err(|e| format!("encode: {e}"))?;
    stream.write_all(&out).map_err(|e| format!("write: {e}"))?;
    let frame = read_one_frame(&mut stream)?;
    match frame.op {
        Op::StatsResponse => parse_stats_response(&frame)
            .map(str::to_owned)
            .map_err(|e| format!("decode: {e}")),
        Op::ErrorResponse | Op::Error => {
            let msg =
                parse_error_response(&frame).map_or_else(|_| "<undecodable>".into(), str::to_owned);
            Err(format!("server: {msg}"))
        }
        other => Err(format!("unexpected reply op {other:?}")),
    }
}

fn die(msg: &str, code: i32) {
    eprintln!("spg: {msg}");
    process::exit(code);
}

fn ping(addr: &str) -> Result<(), String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|e| format!("set_read_timeout: {e}"))?;
    // Ping itself is always allowed unauthenticated; skip the AUTH
    // round-trip to keep `spg ping` a true low-overhead health check.
    let mut out = Vec::new();
    encode(&Frame::ping(), &mut out).map_err(|e| format!("encode: {e}"))?;
    stream.write_all(&out).map_err(|e| format!("write: {e}"))?;

    let frame = read_one_frame(&mut stream)?;
    match frame.op {
        Op::Pong => Ok(()),
        Op::Error | Op::ErrorResponse => {
            let msg = parse_error_response(&frame)
                .map(str::to_owned)
                .or_else(|_| {
                    Ok::<String, FrameError>(String::from_utf8_lossy(&frame.payload).into_owned())
                })
                .unwrap_or_else(|_| "<undecodable error>".into());
            Err(format!("server error: {msg}"))
        }
        other => Err(format!("unexpected reply op {other:?}")),
    }
}

fn query(addr: &str, sql: &str) -> Result<(), String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|e| format!("set_read_timeout: {e}"))?;
    maybe_authenticate(&mut stream)?;
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).map_err(|e| format!("encode: {e}"))?;
    stream.write_all(&out).map_err(|e| format!("write: {e}"))?;

    // First reply: either RowDescription (start of a row set), CommandComplete
    // (DDL/DML happy path), or ErrorResponse.
    let first = read_one_frame(&mut stream)?;
    match first.op {
        Op::CommandComplete => {
            let affected = parse_command_complete(&first).map_err(|e| format!("decode CC: {e}"))?;
            println!("OK ({affected} row(s) affected)");
            Ok(())
        }
        Op::ErrorResponse => {
            let msg = parse_error_response(&first).map_err(|e| format!("decode error: {e}"))?;
            Err(msg.into())
        }
        Op::RowDescription => {
            let cols = parse_row_description(&first).map_err(|e| format!("decode RD: {e}"))?;
            let mut rows: Vec<Vec<WireValue>> = Vec::new();
            loop {
                let f = read_one_frame(&mut stream)?;
                match f.op {
                    Op::DataRow => {
                        let row = parse_data_row(&f).map_err(|e| format!("decode DR: {e}"))?;
                        rows.push(row);
                    }
                    // v3.3.1 server batches result rows when len > 1.
                    // Decode every row in the batch and append.
                    Op::DataRowBatch => {
                        let batch =
                            parse_data_row_batch(&f).map_err(|e| format!("decode DRB: {e}"))?;
                        rows.extend(batch);
                    }
                    Op::CommandComplete => break,
                    Op::ErrorResponse => {
                        let msg =
                            parse_error_response(&f).map_err(|e| format!("decode error: {e}"))?;
                        return Err(msg.into());
                    }
                    other => return Err(format!("unexpected op in row stream: {other:?}")),
                }
            }
            print_table(&cols, &rows);
            Ok(())
        }
        other => Err(format!("unexpected reply op {other:?}")),
    }
}

fn read_one_frame(stream: &mut TcpStream) -> Result<Frame, String> {
    // Use exact-length reads so we never leave already-arrived bytes
    // stranded in a stack-local buffer between back-to-back frames
    // (which the server emits for SELECT: RowDescription + DataRow* + CC).
    let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
    stream
        .read_exact(&mut header)
        .map_err(|e| format!("read header: {e}"))?;
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).map_err(|e| format!("op: {e}"))?;
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream
            .read_exact(&mut payload)
            .map_err(|e| format!("read payload: {e}"))?;
    }
    Ok(Frame { op, payload })
}

fn print_table(cols: &[ColumnDesc], rows: &[Vec<WireValue>]) {
    // Compute column widths from headers and stringified cell values.
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(format_value).collect())
        .collect();
    let mut widths: Vec<usize> = cols.iter().map(|c| c.name.len()).collect();
    for row in &cells {
        for (i, s) in row.iter().enumerate() {
            if s.len() > widths[i] {
                widths[i] = s.len();
            }
        }
    }

    // Header
    let mut line = String::new();
    for (i, c) in cols.iter().enumerate() {
        if i > 0 {
            line.push_str(" | ");
        }
        line.push_str(&pad(&c.name, widths[i]));
    }
    println!("{line}");

    // Separator
    line.clear();
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            line.push_str("-+-");
        }
        line.push_str(&"-".repeat(*w));
    }
    println!("{line}");

    // Rows
    for row in &cells {
        line.clear();
        for (i, s) in row.iter().enumerate() {
            if i > 0 {
                line.push_str(" | ");
            }
            line.push_str(&pad(s, widths[i]));
        }
        println!("{line}");
    }
    println!("({} row(s))", rows.len());
}

fn pad(s: &str, w: usize) -> String {
    if s.len() >= w {
        s.into()
    } else {
        let mut out = String::with_capacity(w);
        out.push_str(s);
        for _ in s.len()..w {
            out.push(' ');
        }
        out
    }
}

fn format_value(v: &WireValue) -> String {
    match v {
        WireValue::Null => "NULL".into(),
        WireValue::Int(n) => n.to_string(),
        WireValue::BigInt(n) => n.to_string(),
        WireValue::Float(x) => format!("{x}"),
        WireValue::Text(s) => s.clone(),
        WireValue::Bool(b) => (if *b { "TRUE" } else { "FALSE" }).into(),
        WireValue::Vector(v) => {
            use core::fmt::Write as _;
            let mut s = String::from("[");
            for (i, x) in v.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                write!(s, "{x}").expect("format to String");
            }
            s.push(']');
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spg_storage::{ColumnSchema, DataType, Row, TableSchema, Value};
    use std::env::temp_dir;

    fn tmp_path(name: &str) -> std::path::PathBuf {
        // Mix in the process id + nanosecond clock so parallel test
        // runs don't collide on the same path. No external test crate.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let mut p = temp_dir();
        p.push(format!(
            "spg-cli-{}-{}-{nanos}.spgdb",
            std::process::id(),
            name
        ));
        p
    }

    #[test]
    fn backup_roundtrip_preserves_data() {
        let src = tmp_path("backup-src");
        let dst = tmp_path("backup-dst");
        // Build a small catalog and write it out.
        let mut cat = Catalog::new();
        cat.create_table(TableSchema::new(
            "users",
            vec![
                ColumnSchema::new("id", DataType::Int, false),
                ColumnSchema::new("name", DataType::Text, false),
            ],
        ))
        .unwrap();
        let t = cat.get_mut("users").unwrap();
        t.insert(Row::new(vec![Value::Int(1), Value::Text("alice".into())]))
            .unwrap();
        t.insert(Row::new(vec![Value::Int(2), Value::Text("bob".into())]))
            .unwrap();
        fs::write(&src, cat.serialize()).unwrap();
        // Run the backup path.
        let count = backup(src.to_str().unwrap(), dst.to_str().unwrap()).unwrap();
        assert_eq!(count, 1);
        // Validate dst matches src exactly.
        let bytes_src = fs::read(&src).unwrap();
        let bytes_dst = fs::read(&dst).unwrap();
        assert_eq!(bytes_src, bytes_dst);
        // And dst parses cleanly.
        let round = Catalog::deserialize(&bytes_dst).unwrap();
        assert_eq!(round.table_count(), 1);
        // Cleanup.
        let _ = fs::remove_file(&src);
        let _ = fs::remove_file(&dst);
    }

    #[test]
    fn backup_rejects_garbage_file() {
        let src = tmp_path("garbage-src");
        let dst = tmp_path("garbage-dst");
        fs::write(&src, b"not a real spgdb file at all").unwrap();
        let err = backup(src.to_str().unwrap(), dst.to_str().unwrap()).unwrap_err();
        assert!(err.contains("parse"), "expected parse error, got: {err}");
        // dst must not exist on failure.
        assert!(!dst.exists(), "dst should not be written when src is bad");
        let _ = fs::remove_file(&src);
    }

    #[test]
    fn backup_refuses_same_path() {
        let p = tmp_path("same");
        fs::write(&p, b"placeholder").unwrap();
        let err = backup(p.to_str().unwrap(), p.to_str().unwrap()).unwrap_err();
        assert!(err.contains("same path"));
        let _ = fs::remove_file(&p);
    }

    // ---- v7.18 PITR P3 ----

    #[test]
    fn parse_restore_target_accepts_lsn() {
        match parse_restore_target("42").unwrap() {
            RestoreTarget::Lsn(n) => assert_eq!(n, 42),
            other => panic!("expected Lsn, got {other:?}"),
        }
    }

    #[test]
    fn parse_restore_target_accepts_unix_seconds() {
        match parse_restore_target("1750000000s").unwrap() {
            RestoreTarget::UnixMicros(us) => assert_eq!(us, 1_750_000_000_000_000),
            other => panic!("expected UnixMicros, got {other:?}"),
        }
    }

    #[test]
    fn parse_restore_target_accepts_unix_millis() {
        match parse_restore_target("1750000000123ms").unwrap() {
            RestoreTarget::UnixMicros(us) => assert_eq!(us, 1_750_000_000_123_000),
            other => panic!("expected UnixMicros, got {other:?}"),
        }
    }

    #[test]
    fn parse_restore_target_accepts_unix_micros() {
        match parse_restore_target("1750000000123456us").unwrap() {
            RestoreTarget::UnixMicros(us) => assert_eq!(us, 1_750_000_000_123_456),
            other => panic!("expected UnixMicros, got {other:?}"),
        }
    }

    #[test]
    fn parse_restore_target_accepts_iso8601() {
        // 2026-01-01 00:00:00 UTC = 1767225600 unix seconds.
        let t = parse_restore_target("2026-01-01 00:00:00").unwrap();
        match t {
            RestoreTarget::UnixMicros(us) => {
                assert_eq!(us, 1_767_225_600 * 1_000_000);
            }
            other => panic!("expected UnixMicros, got {other:?}"),
        }
        // T separator works too.
        let t = parse_restore_target("2026-01-01T00:00:00").unwrap();
        match t {
            RestoreTarget::UnixMicros(us) => assert_eq!(us, 1_767_225_600 * 1_000_000),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_restore_target_rejects_garbage() {
        assert!(parse_restore_target("yesterday").is_err());
        assert!(parse_restore_target("-1").is_err());
        assert!(parse_restore_target("2026-13-01 00:00:00").is_ok()); // we don't bounds-check fields
    }

    #[test]
    fn backup_pitr_round_trips_with_pitr_restore() {
        use spg_embedded::Database;
        let db_path = tmp_path("bk-src-db");
        let wal_path = {
            let mut p = db_path.clone();
            let mut name = p
                .file_name()
                .map(std::ffi::OsStr::to_os_string)
                .unwrap_or_default();
            name.push(".wal");
            p.set_file_name(name);
            p
        };
        // Apply writes, then checkpoint() so the snapshot file
        // is materialised on disk. checkpoint() truncates the
        // WAL — so the data lives entirely in the snapshot for
        // this round-trip test.
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        db.execute("INSERT INTO t VALUES (1)").unwrap();
        db.execute("INSERT INTO t VALUES (2)").unwrap();
        db.checkpoint().unwrap();
        drop(db);

        // Backup → backup_dir.
        let backup_dir = tmp_path("bk-dst-dir");
        let summary = backup_pitr(
            db_path.to_str().unwrap(),
            backup_dir.to_str().unwrap(),
        )
        .unwrap();
        assert!(summary.starts_with("OK "), "bad summary: {summary}");
        let snap = backup_dir.join("snapshot.spg");
        let wal_dir = backup_dir.join("wal");
        assert!(snap.exists(), "snapshot.spg missing");
        assert!(wal_dir.exists(), "wal/ subdir missing");
        let chunks: Vec<_> = fs::read_dir(&wal_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        // checkpoint() truncated the WAL to 0 bytes before the
        // backup snapshot was read, so the chunk directory ends
        // up empty — all data lives in snapshot.spg. Empty WAL
        // chunks would clutter the backup dir without information
        // and would still need verifier special-casing later, so
        // backup_pitr skips them.
        assert!(chunks.is_empty(), "expected 0 chunks after checkpoint, got {chunks:?}");

        // Restore: feed pitr_restore an empty WAL file to confirm
        // the snapshot-only path also works. Use the source WAL
        // path (currently empty) for that.
        let empty_wal = tmp_path("bk-empty-wal");
        fs::write(&empty_wal, b"").unwrap();
        let target_path = tmp_path("bk-restore-target");
        let (applied, _) = pitr_restore(
            snap.to_str().unwrap(),
            empty_wal.to_str().unwrap(),
            "999",
            target_path.to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(applied, 0, "empty WAL → nothing to replay");

        // Verify rows survived in the snapshot itself.
        let mut restored = Database::restore(&fs::read(&target_path).unwrap()).unwrap();
        let rows = restored.query("SELECT COUNT(*) FROM t").unwrap();
        let count = match &rows[0][0] {
            spg_embedded::Value::Int(n) => i64::from(*n),
            spg_embedded::Value::BigInt(n) => *n,
            other => panic!("{other:?}"),
        };
        assert_eq!(count, 2);

        let _ = fs::remove_dir_all(&backup_dir);
        let _ = fs::remove_file(&target_path);
        let _ = fs::remove_file(&db_path);
        let _ = fs::remove_file(&wal_path);
        let _ = fs::remove_file(&empty_wal);
    }

    #[test]
    fn backup_pitr_handles_missing_wal() {
        use spg_embedded::Database;
        let db_path = tmp_path("bk-no-wal-db");
        // Touch a snapshot file but never run any writes that
        // would create the WAL.
        let mut db = Database::open_path(&db_path).unwrap();
        // No execute() — Drop will checkpoint an empty engine.
        drop(db);
        // Wipe the .wal file (Drop's empty-checkpoint may have
        // created it; nuke explicitly to exercise the missing-
        // WAL branch).
        let wal_path = {
            let mut p = db_path.clone();
            let mut name = p
                .file_name()
                .map(std::ffi::OsStr::to_os_string)
                .unwrap_or_default();
            name.push(".wal");
            p.set_file_name(name);
            p
        };
        let _ = fs::remove_file(&wal_path);

        let backup_dir = tmp_path("bk-no-wal-dst");
        let summary = backup_pitr(
            db_path.to_str().unwrap(),
            backup_dir.to_str().unwrap(),
        )
        .unwrap();
        assert!(summary.contains("wal_present=false"), "summary: {summary}");
        let wal_dir = backup_dir.join("wal");
        // wal/ subdir created but empty.
        assert!(wal_dir.exists());
        let chunks: Vec<_> = fs::read_dir(&wal_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(chunks.is_empty(), "should have produced no chunks");

        let _ = fs::remove_dir_all(&backup_dir);
        let _ = fs::remove_file(&db_path);
    }

    #[test]
    fn pitr_restore_replays_up_to_lsn_only() {
        use spg_embedded::Database;
        // 1) Build a snapshot + WAL by running 3 inserts on a
        //    fresh file-backed Database, then keeping the WAL
        //    alive by mem::forget so checkpoint doesn't truncate.
        let db_path = tmp_path("pitr-src-db");
        let wal_path = {
            let mut p = db_path.clone();
            let mut name = p
                .file_name()
                .map(std::ffi::OsStr::to_os_string)
                .unwrap_or_default();
            name.push(".wal");
            p.set_file_name(name);
            p
        };
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        db.execute("INSERT INTO t VALUES (1)").unwrap();
        db.execute("INSERT INTO t VALUES (2)").unwrap();
        db.execute("INSERT INTO t VALUES (3)").unwrap();
        // Capture the catalog snapshot before any checkpoint.
        let snapshot_bytes = db.snapshot();
        std::mem::forget(db);
        Database::force_unlock(&db_path).unwrap();

        let snap_path = tmp_path("pitr-snap");
        fs::write(&snap_path, &snapshot_bytes).unwrap();

        // The CREATE TABLE is in the snapshot (the engine state
        // was captured after every execute) — but the snapshot
        // here is overkill: the WAL replay path would rebuild
        // everything. We restore from snapshot then add WAL
        // records up to LSN 3 (the CREATE + first two INSERTs).
        // Actually because snapshot_bytes already reflects all 4
        // statements, replay would just no-op or fail. So
        // instead, build a fresh empty engine snapshot — easier
        // because Database::open_in_memory() has no public
        // snapshot path here. Use Engine::new().snapshot().
        use spg_engine::Engine;
        let fresh_snap = Engine::new().snapshot();
        fs::write(&snap_path, &fresh_snap).unwrap();

        let target_path = tmp_path("pitr-target");
        let (applied, descr) = pitr_restore(
            snap_path.to_str().unwrap(),
            wal_path.to_str().unwrap(),
            "3",
            target_path.to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(applied, 3, "expected 3 records (CREATE + 2 INSERTs)");
        assert!(descr.contains("lsn"), "descr should mention lsn: {descr}");

        // Verify the resulting snapshot contains exactly 2 rows.
        let mut restored = Database::restore(&fs::read(&target_path).unwrap()).unwrap();
        let rows = restored.query("SELECT COUNT(*) FROM t").unwrap();
        let count = match &rows[0][0] {
            spg_embedded::Value::Int(n) => i64::from(*n),
            spg_embedded::Value::BigInt(n) => *n,
            other => panic!("unexpected: {other:?}"),
        };
        assert_eq!(count, 2, "LSN<=3 means CREATE + 2 INSERTs");

        let _ = fs::remove_file(&snap_path);
        let _ = fs::remove_file(&target_path);
        let _ = fs::remove_file(&db_path);
        let _ = fs::remove_file(&wal_path);
    }
}
