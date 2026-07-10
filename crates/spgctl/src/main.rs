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
        // v7.37.23 (23.2) — psql meta-command equivalents:
        //   \d  / describe          — list relations  (tables + views + indexes + sequences)
        //   \dt / describe-tables   — list tables only
        //   \di / describe-indexes  — list indexes
        //   \dv / describe-views    — list views
        //   \df / describe-functions— list functions
        //   \du / describe-roles    — list roles / users
        //   \l  / describe-databases— list databases
        //   \dn / describe-schemas  — list schemas
        // Each verb dispatches a canned SQL query against the server over
        // the existing TCP path so the operator gets the same one-line
        // shape as psql without spinning up a REPL (23.1) first. Useful
        // in pipelines and CI gates.
        Some(verb)
            if matches!(
                verb,
                "\\d"
                    | "\\dt"
                    | "\\di"
                    | "\\dv"
                    | "\\df"
                    | "\\du"
                    | "\\l"
                    | "\\dn"
                    | "describe"
                    | "describe-tables"
                    | "describe-indexes"
                    | "describe-views"
                    | "describe-functions"
                    | "describe-roles"
                    | "describe-databases"
                    | "describe-schemas"
            ) =>
        {
            let addr = args.next().unwrap_or_else(|| DEFAULT_ADDR.to_string());
            let sql = match verb {
                "\\d" | "describe" => {
                    "SELECT relname AS name, relkind AS kind, relnamespace AS schema_oid \
                     FROM pg_catalog.pg_class \
                     WHERE relkind IN ('r','v','i','S','m','p') \
                     ORDER BY relkind, relname"
                }
                "\\dt" | "describe-tables" => {
                    "SELECT relname AS name, relnatts AS columns, reltuples AS rows \
                     FROM pg_catalog.pg_class \
                     WHERE relkind IN ('r','p') \
                     ORDER BY relname"
                }
                "\\di" | "describe-indexes" => {
                    "SELECT relname AS name, relnatts AS columns \
                     FROM pg_catalog.pg_class \
                     WHERE relkind = 'i' \
                     ORDER BY relname"
                }
                "\\dv" | "describe-views" => {
                    "SELECT relname AS name, relnatts AS columns \
                     FROM pg_catalog.pg_class \
                     WHERE relkind IN ('v','m') \
                     ORDER BY relname"
                }
                "\\df" | "describe-functions" => {
                    "SELECT proname AS name, pronargs AS args, provolatile AS volatility \
                     FROM pg_catalog.pg_proc \
                     ORDER BY proname"
                }
                "\\du" | "describe-roles" => {
                    "SELECT rolname AS name, rolsuper AS superuser, rolcanlogin AS can_login \
                     FROM pg_catalog.pg_roles \
                     ORDER BY rolname"
                }
                "\\l" | "describe-databases" => {
                    "SELECT datname AS name, datcollate AS collate, datctype AS ctype \
                     FROM pg_catalog.pg_database \
                     ORDER BY datname"
                }
                "\\dn" | "describe-schemas" => {
                    "SELECT nspname AS name, nspowner AS owner_oid \
                     FROM pg_catalog.pg_namespace \
                     ORDER BY nspname"
                }
                _ => unreachable!(),
            };
            match query(&addr, sql) {
                Ok(()) => {}
                Err(e) => die(&format!("{verb}: {e}"), 1),
            }
        }
        // v7.37.22 (22.10) — `spg top` polls `pg_stat_statements` over the
        // existing pgwire/SPG-wire-compatible TCP path every `--interval`
        // seconds (default 2) and prints the top-`--limit` queries
        // (default 10) ranked by `total_exec_time`. Same shape as
        // `top(1)` / `pg_top`, scoped to the workload SPG actually
        // ran. Runs until Ctrl-C; `--once` prints one snapshot and
        // exits (for cron / dashboards).
        Some("top") => {
            let mut addr = DEFAULT_ADDR.to_string();
            let mut interval_secs: u64 = 2;
            let mut limit: u32 = 10;
            let mut once = false;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--addr" => {
                        if let Some(v) = args.next() {
                            addr = v;
                        }
                    }
                    "--interval" => {
                        if let Some(v) = args.next() {
                            if let Ok(n) = v.parse::<u64>() {
                                interval_secs = n.max(1);
                            }
                        }
                    }
                    "--limit" => {
                        if let Some(v) = args.next() {
                            if let Ok(n) = v.parse::<u32>() {
                                limit = n.clamp(1, 1000);
                            }
                        }
                    }
                    "--once" => once = true,
                    other => {
                        die(&format!("top: unknown arg {other:?}"), 2);
                        return;
                    }
                }
            }
            let sql = format!(
                "SELECT queryid, calls, \
                 total_exec_time, mean_exec_time, max_exec_time, \
                 rows, query \
                 FROM pg_catalog.pg_stat_statements \
                 ORDER BY total_exec_time DESC NULLS LAST \
                 LIMIT {limit}"
            );
            loop {
                if !once {
                    print!("\x1b[2J\x1b[H");
                }
                match query(&addr, &sql) {
                    Ok(()) => {}
                    Err(e) => {
                        die(&format!("top: {e}"), 1);
                        return;
                    }
                }
                if once {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_secs(interval_secs));
            }
        }
        Some("import") => {
            // Offline bulk-load: open (or create) a catalog file and
            // execute every statement of a SQL script against it.
            // The server can be down or absent — this is the
            // pg_dump → embedded-catalog migration path (mailrs
            // embed round-12).
            let mut db_path: Option<String> = None;
            let mut file: Option<String> = None;
            let mut force_unlock = false;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--db" => db_path = args.next(),
                    "--file" => file = args.next(),
                    // v7.27 (round-21 B) — recovery-window ergonomics:
                    // clear a lock whose owner is gone (e.g. a stopped
                    // container whose pid is meaningless here) without
                    // raw `rm -rf` on the data dir.
                    "--force-unlock" => force_unlock = true,
                    other => {
                        die(&format!("import: unknown arg {other:?}"), 2);
                    }
                }
            }
            let (Some(db_path), Some(file)) = (db_path, file) else {
                die(
                    "usage: spg import --db <catalog.spg> --file <script.sql> [--force-unlock]",
                    2,
                );
                return;
            };
            if force_unlock {
                if let Err(e) = spg_embedded::Database::force_unlock(&db_path) {
                    die(&format!("--force-unlock failed: {e}"), 1);
                }
                eprintln!("spg import: cleared lock for {db_path} (--force-unlock)");
            }
            match import_script(&db_path, &file) {
                Ok((stmts, affected)) => {
                    println!(
                        "imported {stmts} statements ({affected} rows affected) into {db_path}"
                    );
                }
                Err(e) => die(&format!("import failed: {e}"), 1),
            }
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
        // v7.18 PITR P6 — `spg prune-pitr --dir <backup_dir>
        // --retention-hours <N>` walks <dir>/wal/, removes any
        // chunk whose filename prefix (unix_us) is older than
        // `now - N hours`, plus the matching <chunk>.checksum.
        // Reports how many chunks were kept / removed.
        //
        // v7.37.21 (21.17) — three retention dimensions. A chunk is
        // dropped only when it fails EVERY enabled dimension; any
        // single dimension keeping it wins:
        //   --retention-hours <N>   keep chunks newer than N hours
        //   --retention-bytes <N>   keep newest chunks totalling ≤ N bytes
        //   --retention-count <N>   keep newest N chunks
        // At least one dimension is required; pass two or three to OR
        // them. Bytes accepts a plain integer (raw bytes); operator
        // shorthand `K` / `M` / `G` parses too.
        Some("prune-pitr") => {
            let mut dir: Option<String> = None;
            let mut retention_hours: Option<u64> = None;
            let mut retention_bytes: Option<u64> = None;
            let mut retention_count: Option<u64> = None;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--dir" => dir = args.next(),
                    "--retention-hours" => {
                        retention_hours = args.next().and_then(|s| s.parse::<u64>().ok());
                    }
                    "--retention-bytes" => {
                        retention_bytes = args.next().and_then(|s| parse_size_arg(&s));
                    }
                    "--retention-count" => {
                        retention_count = args.next().and_then(|s| s.parse::<u64>().ok());
                    }
                    other => {
                        die(&format!("unknown prune-pitr arg: {other}"), 2);
                        return;
                    }
                }
            }
            let Some(dir) = dir else {
                die(
                    "usage: spg prune-pitr --dir <backup_dir> --retention-hours <N> \
                     [--retention-bytes <N|NK|NM|NG>] [--retention-count <N>]",
                    2,
                );
                return;
            };
            if retention_hours.is_none() && retention_bytes.is_none() && retention_count.is_none() {
                die(
                    "prune-pitr: at least one of --retention-hours / --retention-bytes / \
                     --retention-count is required",
                    2,
                );
                return;
            }
            match prune_pitr(&dir, retention_hours, retention_bytes, retention_count) {
                Ok(report) => println!("{report}"),
                Err(e) => die(&format!("prune-pitr failed: {e}"), 1),
            }
        }
        // v7.18 PITR P5 — `spg verify-pitr --dir <backup_dir>
        // [--write-missing-checksums]` walks the backup layout
        // backup-pitr produces:
        //   * snapshot.spg must deserialize.
        //   * each wal/*.wal must parse to a monotonic LSN sequence
        //     with no hole inside the chunk.
        //   * each wal/<chunk>.checksum is BLAKE3 of the chunk
        //     bytes; missing checksums are computed and (with
        //     --write-missing-checksums) persisted on the spot,
        //     mismatches are reported and fail the verify.
        //   * replay dry-run: feed snapshot + chunks (sorted by
        //     filename = (unix_us, max_lsn)) into a fresh in-memory
        //     database and confirm every SQL record applies.
        // exit 0 = clean, 1 = corrupt, 2 = harness error.
        Some("verify-pitr") => {
            let mut dir: Option<String> = None;
            let mut write_missing = false;
            // v7.37.21 (21.18) — continuous-mode:
            //   --watch <secs>   re-run every N seconds until SIGINT
            //   --max-runs <N>   cap the run count (default = ∞)
            // A single FAIL inside the watch loop logs + continues
            // rather than exiting; the trailing summary tells the
            // operator how many runs failed. exit 1 if any run
            // failed across the whole watch.
            let mut watch_secs: Option<u64> = None;
            let mut max_runs: Option<u64> = None;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--dir" => dir = args.next(),
                    "--write-missing-checksums" => write_missing = true,
                    "--watch" => {
                        watch_secs = args.next().and_then(|s| s.parse::<u64>().ok());
                    }
                    "--max-runs" => {
                        max_runs = args.next().and_then(|s| s.parse::<u64>().ok());
                    }
                    other => {
                        die(&format!("unknown verify-pitr arg: {other}"), 2);
                        return;
                    }
                }
            }
            let Some(dir) = dir else {
                die("usage: spg verify-pitr --dir <backup_dir>", 2);
                return;
            };
            if let Some(secs) = watch_secs {
                let secs = secs.max(1);
                let mut run_idx: u64 = 0;
                let mut fail_count: u64 = 0;
                let cap = max_runs.unwrap_or(u64::MAX);
                while run_idx < cap {
                    run_idx += 1;
                    eprintln!("[watch run {run_idx}] verify-pitr --dir {dir}");
                    match verify_pitr(&dir, write_missing) {
                        Ok(report) => {
                            println!("{}", report.render());
                            if !report.is_clean() {
                                fail_count += 1;
                            }
                        }
                        Err(e) => {
                            eprintln!("verify-pitr: {e}");
                            fail_count += 1;
                        }
                    }
                    if run_idx < cap {
                        std::thread::sleep(std::time::Duration::from_secs(secs));
                    }
                }
                eprintln!("[watch summary] runs={run_idx} failed={fail_count}");
                if fail_count > 0 {
                    process::exit(1);
                }
            } else {
                match verify_pitr(&dir, write_missing) {
                    Ok(report) => {
                        println!("{}", report.render());
                        if !report.is_clean() {
                            process::exit(1);
                        }
                    }
                    Err(e) => die(&format!("verify-pitr: {e}"), 2),
                }
            }
        }
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
                    println!("OK applied={applied} target={target_descr} → {target_path}");
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
/// v7.18 PITR P5 — backup verification.
///
/// Walks the backup directory `backup-pitr` produced and asserts
/// the snapshot + every WAL chunk are intact + can replay. See
/// the CLI doc-comment in `main` for the layout.
#[derive(Debug)]
struct VerifyReport {
    snapshot_ok: bool,
    snapshot_msg: String,
    chunks: Vec<ChunkReport>,
    replay_ok: bool,
    replay_msg: String,
}

#[derive(Debug)]
struct ChunkReport {
    path: std::path::PathBuf,
    parse_ok: bool,
    parse_msg: String,
    checksum_state: ChecksumState,
}

#[derive(Debug)]
enum ChecksumState {
    Match { hex: String },
    WrittenFresh { hex: String },
    Mismatch { expected: String, actual: String },
    Missing { actual: String },
}

impl VerifyReport {
    fn is_clean(&self) -> bool {
        if !self.snapshot_ok || !self.replay_ok {
            return false;
        }
        for c in &self.chunks {
            if !c.parse_ok {
                return false;
            }
            if matches!(
                c.checksum_state,
                ChecksumState::Mismatch { .. } | ChecksumState::Missing { .. }
            ) {
                return false;
            }
        }
        true
    }
    fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "# verify-pitr report — {}\n\n",
            if self.is_clean() { "PASS" } else { "FAIL" }
        ));
        out.push_str(&format!(
            "snapshot.spg: {} — {}\n",
            if self.snapshot_ok { "OK" } else { "FAIL" },
            self.snapshot_msg
        ));
        out.push_str(&format!(
            "replay dry-run: {} — {}\n",
            if self.replay_ok { "OK" } else { "FAIL" },
            self.replay_msg
        ));
        out.push_str(&format!("\nchunks: {}\n", self.chunks.len()));
        for c in &self.chunks {
            let parse_status = if c.parse_ok { "OK" } else { "FAIL" };
            let csum_status = match &c.checksum_state {
                ChecksumState::Match { hex } => format!("checksum-match ({hex})"),
                ChecksumState::WrittenFresh { hex } => format!("checksum-fresh ({hex})"),
                ChecksumState::Mismatch { expected, actual } => {
                    format!("checksum-MISMATCH expected={expected} actual={actual}")
                }
                ChecksumState::Missing { actual } => {
                    format!(
                        "checksum-MISSING actual={actual} (rerun with --write-missing-checksums)"
                    )
                }
            };
            out.push_str(&format!(
                "  {} — parse: {}; {}\n  parse-msg: {}\n",
                c.path.display(),
                parse_status,
                csum_status,
                c.parse_msg
            ));
        }
        out
    }
}

fn verify_pitr(dir: &str, write_missing_checksums: bool) -> Result<VerifyReport, String> {
    use spg_embedded::{Database, parse_wal_records};

    let dir_path = std::path::PathBuf::from(dir);
    let snap_path = dir_path.join("snapshot.spg");
    let wal_dir = dir_path.join("wal");

    // ---- snapshot ----
    let (snapshot_ok, snapshot_msg, snapshot_bytes) = match fs::read(&snap_path) {
        Ok(bytes) => match Database::restore(&bytes) {
            Ok(_) => (
                true,
                format!("{} bytes deserialize cleanly", bytes.len()),
                bytes,
            ),
            Err(e) => (false, format!("deserialize failed: {e:?}"), Vec::new()),
        },
        Err(e) => (false, format!("read failed: {e}"), Vec::new()),
    };

    // ---- chunks ----
    let mut chunks_meta: Vec<std::path::PathBuf> = Vec::new();
    if wal_dir.exists() {
        for entry in fs::read_dir(&wal_dir).map_err(|e| format!("read wal dir: {e}"))? {
            let entry = entry.map_err(|e| format!("read dir entry: {e}"))?;
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) == Some("wal") {
                chunks_meta.push(p);
            }
        }
    }
    chunks_meta.sort();

    let mut chunks: Vec<ChunkReport> = Vec::new();
    let mut replay_chunks: Vec<Vec<u8>> = Vec::new();
    for path in &chunks_meta {
        let bytes = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let actual_hash = spg_crypto::hex(&spg_crypto::hash(&bytes));
        let cs_path = {
            let mut p = path.clone();
            let mut name = p
                .file_name()
                .map(std::ffi::OsStr::to_os_string)
                .unwrap_or_default();
            name.push(".checksum");
            p.set_file_name(name);
            p
        };
        let csum_state = match fs::read_to_string(&cs_path) {
            Ok(expected) => {
                let expected = expected.trim().to_string();
                if expected.eq_ignore_ascii_case(&actual_hash) {
                    ChecksumState::Match {
                        hex: actual_hash.clone(),
                    }
                } else {
                    ChecksumState::Mismatch {
                        expected,
                        actual: actual_hash.clone(),
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if write_missing_checksums {
                    fs::write(&cs_path, format!("{actual_hash}\n"))
                        .map_err(|e| format!("write checksum {}: {e}", cs_path.display()))?;
                    ChecksumState::WrittenFresh {
                        hex: actual_hash.clone(),
                    }
                } else {
                    ChecksumState::Missing {
                        actual: actual_hash.clone(),
                    }
                }
            }
            Err(e) => return Err(format!("read checksum {}: {e}", cs_path.display())),
        };
        let (parse_ok, parse_msg) = match parse_wal_records(&bytes) {
            Ok(recs) => {
                // Assert SQL-record LSN strictly monotonic inside
                // the chunk. v7.19 checkpoint markers (0x11) carry
                // the SAME LSN as the last SQL record they anchor
                // — skip them in the monotonicity check; only
                // 0x10 SQL records participate.
                let mut last: Option<u64> = None;
                let mut hole_msg: Option<String> = None;
                for r in &recs {
                    if r.type_byte != 0x10 {
                        continue;
                    }
                    if let Some(l) = r.commit_lsn {
                        if let Some(prev) = last {
                            if l <= prev {
                                hole_msg = Some(format!(
                                    "LSN {l} at offset {} not strictly greater than previous {prev}",
                                    r.offset
                                ));
                                break;
                            }
                        }
                        last = Some(l);
                    }
                }
                if let Some(m) = hole_msg {
                    (false, m)
                } else {
                    (true, format!("{} records parsed", recs.len()))
                }
            }
            Err(e) => (false, e),
        };
        if parse_ok {
            replay_chunks.push(bytes);
        }
        chunks.push(ChunkReport {
            path: path.clone(),
            parse_ok,
            parse_msg,
            checksum_state: csum_state,
        });
    }

    // ---- replay dry-run ----
    // v7.19 — same snapshot-floor logic pitr_restore and
    // open_path use: records at or below the highest checkpoint
    // marker LSN are already inside snapshot.spg and must not
    // re-apply during the dry-run.
    let snapshot_floor: u64 = replay_chunks
        .iter()
        .filter_map(|chunk| parse_wal_records(chunk).ok())
        .flatten()
        .filter(|r| r.type_byte == 0x11)
        .filter_map(|r| r.commit_lsn)
        .max()
        .unwrap_or(0);
    let (replay_ok, replay_msg) = if snapshot_ok {
        match Database::restore(&snapshot_bytes) {
            Ok(mut db) => {
                let mut applied = 0u64;
                let mut last_err: Option<String> = None;
                'outer: for chunk in &replay_chunks {
                    match parse_wal_records(chunk) {
                        Ok(recs) => {
                            for r in recs {
                                if r.type_byte == 0x10 {
                                    if let Some(lsn) = r.commit_lsn {
                                        if lsn <= snapshot_floor {
                                            continue;
                                        }
                                    }
                                }
                                if r.type_byte == 0x01 || r.type_byte == 0x10 {
                                    let sql = match std::str::from_utf8(r.sql) {
                                        Ok(s) => s,
                                        Err(e) => {
                                            last_err = Some(format!(
                                                "non-UTF-8 SQL at offset {}: {e}",
                                                r.offset
                                            ));
                                            break 'outer;
                                        }
                                    };
                                    if let Err(e) = db.execute(sql) {
                                        last_err = Some(format!(
                                            "apply rejected at offset {}: {e:?}",
                                            r.offset
                                        ));
                                        break 'outer;
                                    }
                                    applied += 1;
                                }
                            }
                        }
                        Err(e) => {
                            last_err = Some(format!("parse during replay: {e}"));
                            break;
                        }
                    }
                }
                match last_err {
                    Some(msg) => (false, msg),
                    None => (true, format!("{applied} records replayed cleanly")),
                }
            }
            Err(e) => (false, format!("snapshot restore for replay failed: {e:?}")),
        }
    } else {
        (false, "skipped — snapshot did not deserialize".into())
    };

    Ok(VerifyReport {
        snapshot_ok,
        snapshot_msg,
        chunks,
        replay_ok,
        replay_msg,
    })
}

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

    // 2) WAL — three source layouts handled:
    //
    //    a) v7.19 chunked: `<src>.wal/` is a DIRECTORY of
    //       `<unix_us>_<lsn>.wal` chunks → copy each chunk
    //       bit-for-bit, preserving filenames (incremental
    //       backups skip chunks the dst already holds).
    //    b) v7.18 legacy: `<src>.wal` is a FILE → wrap its bytes
    //       into one timestamp-named chunk (the v7.18 behaviour).
    //    c) absent → fresh database, snapshot-only backup.
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
    let mut copied: u64 = 0;
    let mut skipped: u64 = 0;
    let mut max_lsn: u64 = 0;
    let mut archive_status = ArchiveStatus::NotInvoked;
    let wal_present;
    if src_wal.is_dir() {
        // (a) chunked layout — copy chunks preserving filenames.
        wal_present = true;
        let mut entries: Vec<std::path::PathBuf> = fs::read_dir(&src_wal)
            .map_err(|e| format!("read wal dir {}: {e}", src_wal.display()))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("wal"))
            .collect();
        entries.sort();
        for chunk in entries {
            let fname = chunk
                .file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_default();
            let target = wal_dir.join(&fname);
            let bytes =
                fs::read(&chunk).map_err(|e| format!("read chunk {}: {e}", chunk.display()))?;
            if bytes.is_empty() {
                // The live active chunk is empty between a rotation
                // and the next write; copying it adds noise without
                // information.
                skipped += 1;
                continue;
            }
            if let Ok(recs) = parse_wal_records(&bytes) {
                let recs: Vec<WalRecord<'_>> = recs;
                for r in &recs {
                    if let Some(l) = r.commit_lsn {
                        if l > max_lsn {
                            max_lsn = l;
                        }
                    }
                }
            }
            if target.exists() {
                // Incremental: identical filename = identical chunk
                // (chunks are immutable once rotated; only the live
                // chunk grows, and re-copying it is correct).
                let existing = fs::metadata(&target).map_err(|e| format!("stat: {e}"))?;
                if existing.len() == bytes.len() as u64 {
                    skipped += 1;
                    continue;
                }
            }
            fs::write(&target, &bytes)
                .map_err(|e| format!("write chunk {}: {e}", target.display()))?;
            copied += 1;
            // Failure is sticky on the summary line: once any
            // chunk's archival fails, keep that FAILED status
            // even if later chunks succeed.
            let st = archive_chunk(&target)?;
            if !matches!(archive_status, ArchiveStatus::Failed { .. }) {
                archive_status = st;
            }
        }
    } else {
        // (b)/(c) legacy single-file or absent.
        let (wal_bytes, present) = match fs::read(&src_wal) {
            Ok(b) => (b, true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), false),
            Err(e) => return Err(format!("read wal {}: {e}", src_wal.display())),
        };
        wal_present = present;
        if !wal_bytes.is_empty() {
            let recs: Vec<WalRecord<'_>> =
                parse_wal_records(&wal_bytes).map_err(|e| format!("parse wal for naming: {e}"))?;
            for r in &recs {
                if let Some(l) = r.commit_lsn {
                    if l > max_lsn {
                        max_lsn = l;
                    }
                }
            }
            let now_us = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_micros());
            let chunk_path = wal_dir.join(format!("{now_us}_{max_lsn}.wal"));
            fs::write(&chunk_path, &wal_bytes)
                .map_err(|e| format!("write chunk {}: {e}", chunk_path.display()))?;
            copied = 1;
            archive_status = archive_chunk(&chunk_path)?;
        }
    }

    Ok(format!(
        "OK snapshot={} wal_present={wal_present} chunks_copied={copied} chunks_skipped={skipped} max_lsn={max_lsn} archive={}",
        snap_target.display(),
        archive_status.describe(),
    ))
}

/// v7.18 PITR P6 — fork the SPG_PITR_ARCHIVE_CMD external
/// archival command with `chunk_path` as `$1`. Returns:
///
///   ArchiveStatus::NotInvoked   — env unset (operator opted out).
///   ArchiveStatus::Ok           — command exited 0.
///   ArchiveStatus::Failed { exit_code, stderr_snippet }
///                                — command produced a nonzero exit.
///                                  backup-pitr surfaces this as a
///                                  loud line on stdout but does
///                                  NOT delete the chunk — the
///                                  WAL data stays local even when
///                                  archival is down, mirroring PG.
fn archive_chunk(chunk_path: &std::path::Path) -> Result<ArchiveStatus, String> {
    let Ok(cmd) = std::env::var("SPG_PITR_ARCHIVE_CMD") else {
        return Ok(ArchiveStatus::NotInvoked);
    };
    if cmd.is_empty() {
        return Ok(ArchiveStatus::NotInvoked);
    }
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .arg("--") // shell positional arg shim — $0 swallowed
        .arg(chunk_path)
        .output()
        .map_err(|e| format!("spawn archive cmd {cmd:?}: {e}"))?;
    if output.status.success() {
        Ok(ArchiveStatus::Ok)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let snippet: String = stderr
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(200)
            .collect();
        Ok(ArchiveStatus::Failed {
            exit_code: output.status.code().unwrap_or(-1),
            stderr_snippet: snippet,
        })
    }
}

#[derive(Debug)]
enum ArchiveStatus {
    NotInvoked,
    Ok,
    Failed {
        exit_code: i32,
        stderr_snippet: String,
    },
}

impl ArchiveStatus {
    fn describe(&self) -> String {
        match self {
            ArchiveStatus::NotInvoked => "skipped (SPG_PITR_ARCHIVE_CMD unset)".into(),
            ArchiveStatus::Ok => "ok".into(),
            ArchiveStatus::Failed {
                exit_code,
                stderr_snippet,
            } => format!("FAILED exit={exit_code} stderr={stderr_snippet:?}"),
        }
    }
}

/// v7.37.21 (21.17) — parse a `--retention-bytes` argument.
/// Accepts plain integers (raw bytes) and `N{K|M|G}` shorthand
/// (binary multipliers — 1K = 1024). Returns None on parse error.
fn parse_size_arg(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Ok(n) = s.parse::<u64>() {
        return Some(n);
    }
    let (num_part, mult): (&str, u64) = if let Some(rest) = s.strip_suffix(['G', 'g']) {
        (rest, 1024 * 1024 * 1024)
    } else if let Some(rest) = s.strip_suffix(['M', 'm']) {
        (rest, 1024 * 1024)
    } else if let Some(rest) = s.strip_suffix(['K', 'k']) {
        (rest, 1024)
    } else {
        return None;
    };
    num_part
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
}

/// v7.18 PITR P6 — delete WAL chunks older than the retention
/// window. Walks `<dir>/wal/`, parses the unix_us prefix off each
/// `<unix_us>_<max_lsn>.wal`, removes the chunk + its
/// `<chunk>.checksum` sibling when EVERY enabled retention dimension
/// says the chunk is expired. Returns a summary line.
///
/// v7.37.21 (21.17) — three dimensions ORed: a chunk is kept when
/// ANY enabled dimension would retain it.
///   - time:  `prefix_s ≥ now_s - retention_hours * 3600`
///   - bytes: chunk fits within the newest-first running total
///            ≤ retention_bytes
///   - count: chunk is among the newest `retention_count` chunks
/// Dimensions with `None` argument never reject (effectively
/// retention=∞ for that dimension).
fn prune_pitr(
    dir: &str,
    retention_hours: Option<u64>,
    retention_bytes: Option<u64>,
    retention_count: Option<u64>,
) -> Result<String, String> {
    let wal_dir = std::path::PathBuf::from(dir).join("wal");
    if !wal_dir.exists() {
        return Ok(format!(
            "no wal/ subdir at {} — nothing to prune",
            wal_dir.display()
        ));
    }
    let now_s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let time_cutoff_s = retention_hours.map(|h| now_s.saturating_sub(h * 3_600));

    // Collect every chunk with its (prefix_s, size). Sort newest
    // first so the bytes + count dimensions walk in cut-off order.
    struct ChunkEntry {
        path: std::path::PathBuf,
        prefix_s: u64,
        size: u64,
    }
    let mut chunks: Vec<ChunkEntry> = Vec::new();
    for entry in fs::read_dir(&wal_dir).map_err(|e| format!("read wal dir: {e}"))? {
        let entry = entry.map_err(|e| format!("read entry: {e}"))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("wal") {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };
        let prefix_us: u128 = stem
            .split_once('_')
            .and_then(|(prefix, _)| prefix.parse().ok())
            .unwrap_or(0);
        let prefix_s = (prefix_us / 1_000_000) as u64;
        let size = fs::metadata(&path).map_or(0, |m| m.len());
        chunks.push(ChunkEntry {
            path,
            prefix_s,
            size,
        });
    }
    chunks.sort_by(|a, b| b.prefix_s.cmp(&a.prefix_s)); // newest first

    let mut running_bytes: u64 = 0;
    let mut kept = 0u64;
    let mut removed = 0u64;
    for (idx, c) in chunks.iter().enumerate() {
        let keep_by_time = match time_cutoff_s {
            Some(cutoff) => c.prefix_s >= cutoff,
            None => false, // dimension disabled — doesn't vote
        };
        let keep_by_bytes = match retention_bytes {
            Some(budget) => {
                let after = running_bytes.saturating_add(c.size);
                if after <= budget { true } else { false }
            }
            None => false,
        };
        let keep_by_count = match retention_count {
            Some(n) => (idx as u64) < n,
            None => false,
        };
        if keep_by_time || keep_by_bytes || keep_by_count {
            running_bytes = running_bytes.saturating_add(c.size);
            kept += 1;
            continue;
        }
        fs::remove_file(&c.path).map_err(|e| format!("remove {}: {e}", c.path.display()))?;
        let cs = {
            let mut p = c.path.clone();
            let mut name = p
                .file_name()
                .map(std::ffi::OsStr::to_os_string)
                .unwrap_or_default();
            name.push(".checksum");
            p.set_file_name(name);
            p
        };
        if cs.exists() {
            fs::remove_file(&cs).map_err(|e| format!("remove {}: {e}", cs.display()))?;
        }
        removed += 1;
    }
    Ok(format!(
        "OK retention_hours={retention_hours:?} retention_bytes={retention_bytes:?} \
         retention_count={retention_count:?} kept={kept} removed={removed}"
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
    let snapshot_bytes =
        fs::read(snapshot_path).map_err(|e| format!("read snapshot {snapshot_path}: {e}"))?;
    let mut db =
        Database::restore(&snapshot_bytes).map_err(|e| format!("restore snapshot: {e:?}"))?;

    // v7.19 P6 — `--wal` accepts a chunk DIRECTORY (the
    // backup-pitr `wal/` subdir or a live `<db>.wal/` dir) as
    // well as a single chunk file. Directory mode concatenates
    // every chunk in sorted (= LSN) order so the restore walks
    // the full record stream.
    let wal_p = std::path::Path::new(wal_path);
    let wal_bytes = if wal_p.is_dir() {
        let mut entries: Vec<std::path::PathBuf> = fs::read_dir(wal_p)
            .map_err(|e| format!("read wal dir {wal_path}: {e}"))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("wal"))
            .collect();
        entries.sort();
        let mut combined = Vec::new();
        for chunk in entries {
            let bytes =
                fs::read(&chunk).map_err(|e| format!("read chunk {}: {e}", chunk.display()))?;
            combined.extend_from_slice(&bytes);
        }
        combined
    } else {
        fs::read(wal_p).map_err(|e| format!("read wal {wal_path}: {e}"))?
    };
    let records: Vec<WalRecord<'_>> =
        parse_wal_records(&wal_bytes).map_err(|e| format!("parse wal: {e}"))?;

    // v7.19 — snapshot floor. checkpoint() rotates chunks instead
    // of truncating, so the WAL stream may contain records the
    // snapshot already reflects. The checkpoint markers (0x11)
    // carry the LSN high-water mark of each snapshot write; records
    // at or below the highest marker LSN are already inside
    // snapshot.spg and must not re-apply (DuplicateTable /
    // double-insert otherwise). Same logic open_path uses.
    let snapshot_floor: u64 = records
        .iter()
        .filter(|r| r.type_byte == 0x11)
        .filter_map(|r| r.commit_lsn)
        .max()
        .unwrap_or(0);

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
            0x10 | 0x12 => {
                // v7.19 — skip records the snapshot already holds.
                // 0x10 = V4 auto-commit SQL; 0x12 = V4 tx-commit SQL
                // (whole-transaction payload, split + applied in order).
                if let Some(lsn) = r.commit_lsn {
                    if lsn <= snapshot_floor {
                        continue;
                    }
                }
                if !target.includes(r.commit_lsn, r.commit_unix_us) {
                    continue;
                }
                let sql = std::str::from_utf8(r.sql)
                    .map_err(|e| format!("non-UTF-8 SQL at offset {}: {e}", r.offset))?;
                db.execute(sql)
                    .map_err(|e| format!("apply at offset {}: {e:?}", r.offset))?;
                applied += 1;
            }
            0x13 => {
                // v7.34 / v7.37.8 — V5 row-redo. Payload is encoded
                // RowChange bytes (not SQL); decode + apply via the
                // engine's physical redo path. Same flow as
                // spg-embedded's open_path replay loop. Pre-v7.37.9
                // spgctl errored out here (`unknown WAL record type
                // 0x13`), breaking PITR restore on any catalog that
                // had logged after the V5 default-ON flip.
                if let Some(lsn) = r.commit_lsn {
                    if lsn <= snapshot_floor {
                        continue;
                    }
                }
                if !target.includes(r.commit_lsn, r.commit_unix_us) {
                    continue;
                }
                let changes = spg_storage::decode_redo_log(r.sql)
                    .map_err(|e| format!("redo decode at offset {}: {e:?}", r.offset))?;
                db.apply_redo(&changes)
                    .map_err(|e| format!("redo apply at offset {}: {e:?}", r.offset))?;
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
    fs::write(target_path, &final_snapshot).map_err(|e| format!("write {target_path}: {e}"))?;
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
            let y: i64 = date[0]
                .parse()
                .map_err(|_| format!("bad year: {}", date[0]))?;
            let mo: i64 = date[1]
                .parse()
                .map_err(|_| format!("bad month: {}", date[1]))?;
            let d: i64 = date[2]
                .parse()
                .map_err(|_| format!("bad day: {}", date[2]))?;
            let h: i64 = time[0]
                .parse()
                .map_err(|_| format!("bad hour: {}", time[0]))?;
            let mi: i64 = time[1]
                .parse()
                .map_err(|_| format!("bad minute: {}", time[1]))?;
            let se: i64 = time[2]
                .parse()
                .map_err(|_| format!("bad second: {}", time[2]))?;
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

/// `spg import` — execute a multi-statement SQL script against an
/// on-disk catalog, creating it when absent. Statements run in file
/// order inside ONE transaction: the first error aborts with the
/// failing statement's index + a snippet and rolls the whole import
/// back, so a failed import leaves the catalog exactly as it was —
/// fix the script and re-run (v7.21 polish; previously a failed
/// import left a half-applied prefix behind). A script that carries
/// its own BEGIN/COMMIT (`pg_dump --single-transaction` output) owns
/// its boundaries and runs unwrapped.
fn import_script(db_path: &str, file: &str) -> Result<(usize, usize), String> {
    let script = std::fs::read_to_string(file).map_err(|e| format!("read {file:?}: {e}"))?;
    let mut db =
        spg_embedded::Database::open_path(db_path).map_err(|e| format!("open {db_path:?}: {e}"))?;
    let statements = spg_embedded::split_statements(&script);
    let script_owns_tx = statements.iter().any(|s| {
        let head = s
            .split(|c: char| c.is_whitespace() || c == ';')
            .find(|w| !w.is_empty())
            .map(str::to_ascii_lowercase);
        matches!(
            head.as_deref(),
            Some("begin" | "start" | "commit" | "end" | "rollback" | "savepoint" | "release")
        )
    });
    let wrap = statements.len() > 1 && !script_owns_tx;
    if wrap {
        db.execute("BEGIN").map_err(|e| format!("BEGIN: {e:?}"))?;
    }
    let mut stmts = 0usize;
    let mut affected = 0usize;
    for (i, stmt) in statements.iter().enumerate() {
        match db.execute_dump_statement(stmt) {
            Ok(spg_embedded::QueryResult::CommandOk { affected: n, .. }) => {
                stmts += 1;
                affected += n;
            }
            Ok(_) => {
                stmts += 1;
            }
            Err(e) => {
                if wrap {
                    let _ = db.execute("ROLLBACK");
                }
                let snippet: String = stmt.trim().chars().take(120).collect();
                return Err(format!(
                    "statement #{}: {e:?}\n  {snippet}…{}",
                    i + 1,
                    if wrap {
                        "\n  (import rolled back — the catalog is unchanged)"
                    } else {
                        ""
                    }
                ));
            }
        }
    }
    if wrap {
        db.execute("COMMIT").map_err(|e| format!("COMMIT: {e:?}"))?;
    }
    Ok((stmts, affected))
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
    fn import_script_is_atomic_on_failure() {
        let db = tmp_path("import-atomic");
        let script = tmp_path("import-atomic-sql");
        std::fs::write(
            &script,
            "CREATE TABLE good (id INT NOT NULL);\n\
             INSERT INTO good VALUES (1);\n\
             INSERT INTO no_such_table VALUES (1);",
        )
        .unwrap();
        let err = import_script(db.to_str().unwrap(), script.to_str().unwrap()).unwrap_err();
        assert!(err.contains("statement #3"), "got: {err}");
        assert!(err.contains("rolled back"), "got: {err}");
        // The failed import must leave nothing behind — `good` was
        // rolled back with the rest of the script.
        let mut reopened = spg_embedded::Database::open_path(&db).unwrap();
        assert!(
            reopened.query("SELECT id FROM good").is_err(),
            "half-applied import survived"
        );
        drop(reopened);
        // A clean script then applies fully.
        std::fs::write(
            &script,
            "CREATE TABLE good (id INT NOT NULL);\nINSERT INTO good VALUES (1);",
        )
        .unwrap();
        let (stmts, affected) =
            import_script(db.to_str().unwrap(), script.to_str().unwrap()).unwrap();
        assert_eq!((stmts, affected), (2, 1));
        let mut reopened = spg_embedded::Database::open_path(&db).unwrap();
        assert_eq!(reopened.query("SELECT id FROM good").unwrap().len(), 1);
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
            t @ RestoreTarget::UnixMicros(_) => panic!("expected Lsn, got {t:?}"),
        }
    }

    #[test]
    fn parse_restore_target_accepts_unix_seconds() {
        match parse_restore_target("1750000000s").unwrap() {
            RestoreTarget::UnixMicros(us) => assert_eq!(us, 1_750_000_000_000_000),
            t @ RestoreTarget::Lsn(_) => panic!("expected UnixMicros, got {t:?}"),
        }
    }

    #[test]
    fn parse_restore_target_accepts_unix_millis() {
        match parse_restore_target("1750000000123ms").unwrap() {
            RestoreTarget::UnixMicros(us) => assert_eq!(us, 1_750_000_000_123_000),
            t @ RestoreTarget::Lsn(_) => panic!("expected UnixMicros, got {t:?}"),
        }
    }

    #[test]
    fn parse_restore_target_accepts_unix_micros() {
        match parse_restore_target("1750000000123456us").unwrap() {
            RestoreTarget::UnixMicros(us) => assert_eq!(us, 1_750_000_000_123_456),
            t @ RestoreTarget::Lsn(_) => panic!("expected UnixMicros, got {t:?}"),
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
            t @ RestoreTarget::Lsn(_) => panic!("expected UnixMicros, got {t:?}"),
        }
        // T separator works too.
        let t = parse_restore_target("2026-01-01T00:00:00").unwrap();
        match t {
            RestoreTarget::UnixMicros(us) => assert_eq!(us, 1_767_225_600 * 1_000_000),
            t @ RestoreTarget::Lsn(_) => panic!("expected UnixMicros, got {t:?}"),
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
        // v7.19 — checkpoint() ROTATES the chunk (keeps history)
        // instead of truncating. The backup therefore carries the
        // pre-checkpoint chunk (CREATE + 2 INSERTs + marker), and
        // pitr_restore's snapshot-floor logic skips the records
        // the snapshot already reflects.
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        db.execute("INSERT INTO t VALUES (1)").unwrap();
        db.execute("INSERT INTO t VALUES (2)").unwrap();
        db.checkpoint().unwrap();
        drop(db);

        // Backup → backup_dir.
        let backup_dir = tmp_path("bk-dst-dir");
        let summary = backup_pitr(db_path.to_str().unwrap(), backup_dir.to_str().unwrap()).unwrap();
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
        assert!(
            !chunks.is_empty(),
            "v7.19 rotation keeps history — backup must carry ≥1 chunk"
        );

        // Restore from the backup's chunk directory. The
        // snapshot-floor logic skips every record at or below the
        // checkpoint marker, so nothing re-applies and the row
        // count comes straight from the snapshot.
        let target_path = tmp_path("bk-restore-target");
        let (applied, _) = pitr_restore(
            snap.to_str().unwrap(),
            wal_dir.to_str().unwrap(),
            "999",
            target_path.to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(
            applied, 0,
            "all records pre-date the checkpoint marker → snapshot-floor skips them"
        );

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
        let _ = fs::remove_dir_all(&wal_path);
    }

    #[test]
    fn verify_pitr_passes_on_fresh_backup_with_writes() {
        use spg_embedded::Database;
        let db_path = tmp_path("vf-src-db");
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
        // Realistic PITR backup: checkpoint() materialises the
        // base snapshot file + truncates the WAL, then subsequent
        // writes go into the WAL as incremental records. backup_pitr
        // captures (snapshot, wal-incremental) and verify-pitr
        // replays just the WAL on top of the snapshot — no
        // double-apply of the writes that pre-dated the
        // checkpoint.
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
        db.checkpoint().unwrap(); // snapshot = empty-table t
        db.execute("INSERT INTO t VALUES (1)").unwrap();
        db.execute("INSERT INTO t VALUES (2)").unwrap();
        std::mem::forget(db);
        Database::force_unlock(&db_path).unwrap();

        let backup_dir = tmp_path("vf-bk-dir");
        let summary = backup_pitr(db_path.to_str().unwrap(), backup_dir.to_str().unwrap()).unwrap();
        assert!(summary.starts_with("OK "));

        // First verify without checksums — must report Missing
        // for every chunk and NOT-clean. v7.19 rotation keeps
        // history so the backup carries ≥2 chunks here (pre-
        // checkpoint chunk with CREATE+marker, post-checkpoint
        // chunk with the 2 INSERTs).
        let report = verify_pitr(backup_dir.to_str().unwrap(), false).unwrap();
        assert!(report.snapshot_ok);
        assert!(report.replay_ok, "replay msg: {}", report.replay_msg);
        assert!(
            report.chunks.len() >= 2,
            "v7.19 rotation: expected ≥2 chunks, got {}",
            report.chunks.len()
        );
        for c in &report.chunks {
            assert!(
                matches!(c.checksum_state, ChecksumState::Missing { .. }),
                "got: {:?}",
                c.checksum_state
            );
        }
        assert!(
            !report.is_clean(),
            "report should not be clean without checksum"
        );

        // Now write the checksum file via the flag and verify
        // again — must be clean.
        let report = verify_pitr(backup_dir.to_str().unwrap(), true).unwrap();
        for c in &report.chunks {
            assert!(matches!(
                c.checksum_state,
                ChecksumState::WrittenFresh { .. }
            ));
        }

        let report = verify_pitr(backup_dir.to_str().unwrap(), false).unwrap();
        assert!(report.is_clean(), "report: {}", report.render());

        let _ = fs::remove_dir_all(&backup_dir);
        let _ = fs::remove_file(&db_path);
        let _ = fs::remove_dir_all(&wal_path);
    }

    #[test]
    fn verify_pitr_detects_checksum_mismatch() {
        use spg_embedded::Database;
        let db_path = tmp_path("vf-bad-db");
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
        db.checkpoint().unwrap();
        db.execute("INSERT INTO t VALUES (1)").unwrap();
        std::mem::forget(db);
        Database::force_unlock(&db_path).unwrap();

        let backup_dir = tmp_path("vf-bad-bk-dir");
        backup_pitr(db_path.to_str().unwrap(), backup_dir.to_str().unwrap()).unwrap();

        // Stamp every chunk with a real checksum first, then
        // corrupt the FIRST chunk's sidecar — verify must flag
        // exactly that chunk as Mismatch and the report as
        // not-clean. (v7.19 rotation means ≥2 chunks here.)
        let _ = verify_pitr(backup_dir.to_str().unwrap(), true).unwrap();
        let mut chunks: Vec<_> = fs::read_dir(backup_dir.join("wal"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("wal"))
            .collect();
        chunks.sort();
        assert!(!chunks.is_empty());
        let cs_path = {
            let mut p = chunks[0].clone();
            let mut name = p.file_name().unwrap().to_os_string();
            name.push(".checksum");
            p.set_file_name(name);
            p
        };
        fs::write(
            &cs_path,
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n",
        )
        .unwrap();

        let report = verify_pitr(backup_dir.to_str().unwrap(), false).unwrap();
        let mismatches = report
            .chunks
            .iter()
            .filter(|c| matches!(c.checksum_state, ChecksumState::Mismatch { .. }))
            .count();
        assert_eq!(mismatches, 1, "exactly the corrupted sidecar flags");
        assert!(!report.is_clean());

        let _ = fs::remove_dir_all(&backup_dir);
        let _ = fs::remove_file(&db_path);
        let _ = fs::remove_dir_all(&wal_path);
    }

    #[test]
    fn prune_pitr_removes_chunks_past_retention() {
        // Lay down two chunks: one timestamped "10 hours ago",
        // one "1 minute ago". Retention=1h must keep the recent
        // chunk and drop the old one (plus its checksum sibling).
        let backup_dir = tmp_path("prune-dir");
        let wal_dir = backup_dir.join("wal");
        fs::create_dir_all(&wal_dir).unwrap();
        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_micros());
        let old_us = now_us.saturating_sub(10 * 3_600 * 1_000_000);
        let recent_us = now_us.saturating_sub(60 * 1_000_000);
        let old_chunk = wal_dir.join(format!("{old_us}_42.wal"));
        let old_cs = wal_dir.join(format!("{old_us}_42.wal.checksum"));
        let recent_chunk = wal_dir.join(format!("{recent_us}_43.wal"));
        fs::write(&old_chunk, b"old").unwrap();
        fs::write(&old_cs, b"abc\n").unwrap();
        fs::write(&recent_chunk, b"recent").unwrap();

        let summary = prune_pitr(backup_dir.to_str().unwrap(), Some(1), None, None).unwrap();
        assert!(summary.contains("removed=1"), "summary: {summary}");
        assert!(summary.contains("kept=1"), "summary: {summary}");
        assert!(!old_chunk.exists(), "old chunk should have been removed");
        assert!(!old_cs.exists(), "old checksum should have been removed");
        assert!(recent_chunk.exists(), "recent chunk should still exist");

        let _ = fs::remove_dir_all(&backup_dir);
    }

    #[test]
    fn prune_pitr_no_wal_dir_is_noop() {
        let backup_dir = tmp_path("prune-empty");
        // backup_dir doesn't exist at all — prune should treat as
        // a noop, not error.
        let summary = prune_pitr(backup_dir.to_str().unwrap(), Some(24), None, None).unwrap();
        assert!(summary.contains("nothing to prune"), "summary: {summary}");
    }

    /// v7.37.21 (21.17) — count-only retention keeps the newest N.
    #[test]
    fn prune_pitr_retention_count_keeps_newest_n() {
        let backup_dir = tmp_path("prune-count");
        let wal_dir = backup_dir.join("wal");
        fs::create_dir_all(&wal_dir).unwrap();
        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_micros());
        for i in 0..5u128 {
            // i = 0 oldest, i = 4 newest
            let prefix = now_us.saturating_sub((5 - i) * 60 * 1_000_000);
            let chunk = wal_dir.join(format!("{prefix}_{i}.wal"));
            fs::write(&chunk, b"data").unwrap();
        }

        let summary = prune_pitr(backup_dir.to_str().unwrap(), None, None, Some(2)).unwrap();
        assert!(summary.contains("removed=3"), "summary: {summary}");
        assert!(summary.contains("kept=2"), "summary: {summary}");

        let _ = fs::remove_dir_all(&backup_dir);
    }

    /// v7.37.21 (21.17) — bytes-only retention keeps the newest set
    /// whose cumulative size fits the budget.
    #[test]
    fn prune_pitr_retention_bytes_keeps_under_budget() {
        let backup_dir = tmp_path("prune-bytes");
        let wal_dir = backup_dir.join("wal");
        fs::create_dir_all(&wal_dir).unwrap();
        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_micros());
        // 3 chunks @ 1024 bytes each, oldest → newest.
        for i in 0..3u128 {
            let prefix = now_us.saturating_sub((3 - i) * 60 * 1_000_000);
            let chunk = wal_dir.join(format!("{prefix}_{i}.wal"));
            fs::write(&chunk, vec![0u8; 1024]).unwrap();
        }

        // Budget = 2048 → newest 2 fit; oldest gets dropped.
        let summary = prune_pitr(backup_dir.to_str().unwrap(), None, Some(2048), None).unwrap();
        assert!(summary.contains("removed=1"), "summary: {summary}");
        assert!(summary.contains("kept=2"), "summary: {summary}");

        let _ = fs::remove_dir_all(&backup_dir);
    }

    /// v7.37.21 (21.17) — dimensions OR: a chunk a time dimension
    /// would drop is kept when the count dimension still wants it.
    #[test]
    fn prune_pitr_dimensions_or_together() {
        let backup_dir = tmp_path("prune-or");
        let wal_dir = backup_dir.join("wal");
        fs::create_dir_all(&wal_dir).unwrap();
        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_micros());
        // Two ancient chunks. retention_hours=1 would drop both;
        // retention_count=1 keeps the newest. Union → kept=1.
        for i in 0..2u128 {
            let prefix = now_us.saturating_sub((10 - i) * 3_600 * 1_000_000);
            let chunk = wal_dir.join(format!("{prefix}_{i}.wal"));
            fs::write(&chunk, b"x").unwrap();
        }

        let summary = prune_pitr(backup_dir.to_str().unwrap(), Some(1), None, Some(1)).unwrap();
        assert!(summary.contains("kept=1"), "summary: {summary}");
        assert!(summary.contains("removed=1"), "summary: {summary}");

        let _ = fs::remove_dir_all(&backup_dir);
    }

    #[test]
    fn parse_size_arg_handles_kmg_suffix() {
        assert_eq!(parse_size_arg("1024"), Some(1024));
        assert_eq!(parse_size_arg("1K"), Some(1024));
        assert_eq!(parse_size_arg("2M"), Some(2 * 1024 * 1024));
        assert_eq!(parse_size_arg("3G"), Some(3 * 1024 * 1024 * 1024));
        assert_eq!(parse_size_arg("1k"), Some(1024));
        assert_eq!(parse_size_arg("nope"), None);
    }

    // NOTE: archive_chunk end-to-end (SPG_PITR_ARCHIVE_CMD set /
    // unset / failing) is exercised by the P7 CI suite where the
    // test process can mutate its own env without racing the other
    // unit tests in this binary. Inline env mutation here would
    // need `unsafe` on the 2024 edition, which the workspace lint
    // forbids — so we lean on integration coverage.

    #[test]
    fn backup_pitr_handles_missing_wal() {
        use spg_embedded::Database;
        let db_path = tmp_path("bk-no-wal-db");
        // Touch a snapshot file but then wipe the WAL entirely
        // (v7.19: `<db>.wal/` is a DIRECTORY) to exercise the
        // snapshot-only branch.
        let db = Database::open_path(&db_path).unwrap();
        drop(db);
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
        // v7.19 — wal path is a chunk directory; remove it whole.
        let _ = fs::remove_dir_all(&wal_path);
        let _ = fs::remove_file(&wal_path); // no-op if it was a dir

        let backup_dir = tmp_path("bk-no-wal-dst");
        let summary = backup_pitr(db_path.to_str().unwrap(), backup_dir.to_str().unwrap()).unwrap();
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
        let _ = fs::remove_dir_all(&wal_path);
    }
}
