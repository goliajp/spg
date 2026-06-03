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
    encode, parse_command_complete, parse_data_row, parse_data_row_batch, parse_error_response, parse_row_description,
    parse_stats_response,
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
                die("usage: spg wal-lint <wal_path> --against-schema <db_path>", 2);
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
        engine.execute(sql).map_err(|e| format!(
            "apply rejected {sql:?} at seq {applied}: {e:?}"
        ))?;
        applied += 1;
    }
    let snapshot = engine.snapshot();
    fs::write(out_path, &snapshot)
        .map_err(|e| format!("write {out_path}: {e}"))?;
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
    let snapshot = fs::read(db_path)
        .map_err(|e| (0u64, format!("read schema {db_path}: {e}")))?;
    let mut engine = Engine::restore_envelope(&snapshot)
        .map_err(|e| (0u64, format!("restore schema: {e}")))?;
    let wal_bytes = fs::read(wal_path)
        .map_err(|e| (0u64, format!("read wal {wal_path}: {e}")))?;
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
                        let batch = parse_data_row_batch(&f)
                            .map_err(|e| format!("decode DRB: {e}"))?;
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
}
