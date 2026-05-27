#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::doc_overindented_list_items,
    clippy::manual_assert,
    clippy::uninlined_format_args,
    clippy::unreadable_literal
)]

//! v4.31 — cross-version snapshot + WAL compatibility gate.
//!
//! Walks `xtests/compat-fixtures/<version>/` directories. Each
//! contains:
//!
//!   - `full.bkp`     — backup bundle captured by that version
//!   - `a.wal`        — raw WAL captured by that version
//!   - `expected.txt` — key/value lines describing what restored
//!                      state must look like (`rows=N`,
//!                      `sum_score=N`, `max_score=N`, etc.)
//!
//! For every fixture, the current binary must:
//!
//!   1. Apply the bundle (extract its snapshot into the recovery
//!      db, write its WAL slice into the recovery WAL).
//!   2. Start a fresh spg-server pointed at the recovered files.
//!   3. Run the verifications encoded in expected.txt and pass.
//!
//! Whenever a release adds a new on-disk format feature, the
//! release process captures a fresh fixture for its version and
//! adds the directory here. The test then guards every prior
//! version.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn pick_free_addr() -> String {
    let p = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = p.local_addr().unwrap();
    drop(p);
    a.to_string()
}

fn unique_tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-compat-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(addr: &str, db: &Path, wal: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_spg-server"))
        .arg(addr)
        .arg(db)
        .arg("-")
        .arg(wal)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD")
        .env_remove("SPG_PG_ADDR")
        .spawn()
        .unwrap()
}

fn wait_for_listener(addr: &str, child: &mut Child) -> TcpStream {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match TcpStream::connect(addr) {
            Ok(s) => return s,
            Err(e) => {
                if let Ok(Some(status)) = child.try_wait() {
                    panic!("server exited early: {status:?} ({e})");
                }
                assert!(Instant::now() < deadline, "server never came up: {e}");
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn read_frame(s: &mut TcpStream) -> Frame {
    let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
    s.read_exact(&mut header).unwrap();
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).unwrap();
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        s.read_exact(&mut payload).unwrap();
    }
    Frame { op, payload }
}

fn send(s: &mut TcpStream, f: &Frame) {
    let mut out = Vec::new();
    encode(f, &mut out).unwrap();
    s.write_all(&out).unwrap();
}

fn select_one(s: &mut TcpStream, sql: &str) -> WireValue {
    send(s, &build_query(sql));
    let rd = read_frame(s);
    if rd.op == Op::ErrorResponse {
        let msg = spg_wire::parse_error_response(&rd).unwrap_or("<undecodable>");
        panic!("server rejected SQL {sql:?}: {msg}");
    }
    assert_eq!(rd.op, Op::RowDescription);
    let mut got: Option<WireValue> = None;
    loop {
        let f = read_frame(s);
        match f.op {
            Op::DataRow => got = parse_data_row(&f).unwrap().into_iter().next(),
            Op::DataRowBatch => {
                got = parse_data_row_batch(&f)
                    .unwrap()
                    .into_iter()
                    .next()
                    .and_then(|r| r.into_iter().next());
            }
            Op::CommandComplete => return got.expect("no row"),
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn wire_to_i64(v: &WireValue) -> i64 {
    match v {
        WireValue::Int(n) => i64::from(*n),
        WireValue::BigInt(n) => *n,
        WireValue::Text(t) => t.parse().unwrap(),
        WireValue::Float(f) => *f as i64,
        other => panic!("expected integer, got {other:?}"),
    }
}

fn wire_to_string(v: &WireValue) -> String {
    match v {
        WireValue::Text(t) => t.clone(),
        other => panic!("expected text, got {other:?}"),
    }
}

/// Apply a backup bundle: extract snapshot into dest_db, WAL slice
/// into dest_wal. Same logic as RESTORE_DRILL.md step 1.
fn apply_bundle(dest_db: &Path, dest_wal: &Path, bundle: &Path) {
    let bytes = std::fs::read(bundle).unwrap();
    // v4.37+ writes \x02 (with trailing CRC); pre-v4.37 fixtures
    // are still \x01. Both are valid bundle magics.
    assert!(
        &bytes[..8] == b"SPGBKUP\x01" || &bytes[..8] == b"SPGBKUP\x02",
        "not an SPG bundle (magic = {:?})",
        &bytes[..8]
    );
    let snap_len = u64::from_le_bytes(bytes[25..33].try_into().unwrap()) as usize;
    let snap_end = 33 + snap_len;
    let wal_len =
        u64::from_le_bytes(bytes[snap_end + 8..snap_end + 16].try_into().unwrap()) as usize;
    let wal_start = snap_end + 16;
    if snap_len > 0 {
        std::fs::write(dest_db, &bytes[33..33 + snap_len]).unwrap();
        std::fs::write(dest_wal, &bytes[wal_start..wal_start + wal_len]).unwrap();
    } else {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dest_wal)
            .unwrap();
        f.write_all(&bytes[wal_start..wal_start + wal_len]).unwrap();
        f.sync_data().unwrap();
    }
}

/// Parse expected.txt — `key=value` per line.
fn parse_expected(path: &Path) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for line in std::fs::read_to_string(path).unwrap().lines() {
        if line.is_empty() {
            continue;
        }
        let (k, v) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("bad line in expected.txt: {line:?}"));
        out.insert(k.to_string(), v.to_string());
    }
    out
}

fn list_fixture_dirs() -> Vec<PathBuf> {
    let root = workspace_root().join("xtests/compat-fixtures");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap_or_else(|_| panic!("missing {}", root.display()))
        .filter_map(Result::ok)
        .filter_map(|e| {
            let p = e.path();
            if p.is_dir() { Some(p) } else { None }
        })
        .collect();
    dirs.sort();
    assert!(
        !dirs.is_empty(),
        "no fixtures under {} — add at least the current-version capture",
        root.display()
    );
    dirs
}

#[test]
fn every_fixture_restores_and_verifies() {
    for fixture in list_fixture_dirs() {
        let label = fixture.file_name().unwrap().to_string_lossy().to_string();
        eprintln!("-- compat fixture: {label}");
        let expected = parse_expected(&fixture.join("expected.txt"));
        let bundle = fixture.join("full.bkp");
        assert!(bundle.exists(), "fixture {label} missing full.bkp");

        let rec = unique_tmpdir(&label);
        let rec_db = rec.join("rec.db");
        let rec_wal = rec.join("rec.wal");
        apply_bundle(&rec_db, &rec_wal, &bundle);

        let addr = pick_free_addr();
        let mut child = ChildGuard(spawn(&addr, &rec_db, &rec_wal));
        let mut s = wait_for_listener(&addr, &mut child.0);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

        let table = expected
            .get("table")
            .unwrap_or_else(|| panic!("fixture {label} expected.txt missing `table=`"));

        if let Some(want) = expected.get("rows") {
            let got = wire_to_i64(&select_one(
                &mut s,
                &format!("SELECT count(*) FROM {table}"),
            ));
            assert_eq!(
                got.to_string(),
                *want,
                "{label}: SELECT count(*) FROM {table}"
            );
        }
        if let Some(want) = expected.get("sum_score") {
            let got = wire_to_i64(&select_one(
                &mut s,
                &format!("SELECT sum(score) FROM {table}"),
            ));
            assert_eq!(got.to_string(), *want, "{label}: SELECT sum(score)");
        }
        if let Some(want) = expected.get("max_score") {
            let got = wire_to_i64(&select_one(
                &mut s,
                &format!("SELECT max(score) FROM {table}"),
            ));
            assert_eq!(got.to_string(), *want, "{label}: SELECT max(score)");
        }
        if let Some(want) = expected.get("first_name") {
            let got = wire_to_string(&select_one(
                &mut s,
                &format!("SELECT name FROM {table} ORDER BY id LIMIT 1"),
            ));
            assert_eq!(got, *want, "{label}: first name by id");
        }
    }
}
