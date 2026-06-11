//! v6.7.5 ship gates — segment forwarding replication.
//!
//! Spawns a master with the freezer fast-tuned so cold segments
//! land quickly, populates rows, lets the freezer produce ≥ 2
//! cold segments, then attaches a follower and asserts the
//! three V6_7_DESIGN.md L2 gates:
//!
//!   - `follower_bootstrap_via_forwarding` — every frozen PK is
//!     reachable on the freshly-bootstrapped follower without
//!     waiting for WAL replay (PK seek hits cold tier).
//!   - `byte_equal_segment_after_transfer` — each forwarded
//!     segment file on the follower's disk is byte-identical to
//!     the master's.
//!   - `resumable_after_disconnect` — a follower that connects
//!     a second time skips segment files it already has (segment-
//!     level resume by file-existence; true chunk-level resume
//!     is carved out to STABILITY § v6.7).

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use spg_wire::{Op, build_query, encode, parse_data_row_batch, parse_error_response};

const READ_TIMEOUT: Duration = Duration::from_secs(15);
// v7.28 — 60 s: this is a liveness guard, not a timing assertion.
// Under a full parallel suite (320 tests + several child servers)
// the follower bootstrap was starved past 20 s twice (single-run
// completes in ~1.4 s); the deadline only exists to fail hung
// replication, so headroom is free.
const REPLICATION_TIMEOUT: Duration = Duration::from_secs(60);
const N_ROWS: i64 = 30;

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-seg-fwd-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn segments_dir_of(db_path: &std::path::Path) -> PathBuf {
    let parent = db_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let stem = db_path
        .file_stem()
        .unwrap_or_else(|| std::ffi::OsStr::new("db"))
        .to_string_lossy()
        .into_owned();
    parent.join(format!("{stem}.spg")).join("segments")
}

fn list_segment_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("seg_") && n.ends_with(".spg"))
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

fn spawn_master(
    db: &std::path::Path,
    wal: &std::path::Path,
) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal)
        .with_repl()
        // Tight budget so the freezer cuts segments quickly.
        .env("SPG_HOT_TIER_BYTES", "32")
        .env("SPG_FREEZER_TICK_MS", "50")
        .env("SPG_FREEZER_BATCH_ROWS", "6")
        .spawn()
}

fn spawn_follower(
    db: &std::path::Path,
    wal: &std::path::Path,
    follow_of: &str,
) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal)
        .env("SPG_FOLLOW_OF", follow_of)
        // Disable the freezer on the follower; the cold tier
        // arrives via forwarding, not via local freeze.
        .env("SPG_FREEZER_DISABLE", "1")
        .spawn()
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let q = build_query(sql);
    let mut out = Vec::new();
    encode(&q, &mut out).unwrap();
    s.write_all(&out).unwrap();
}

fn drain_until_cc(s: &mut TcpStream, sql: &str) {
    loop {
        let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
        s.read_exact(&mut header).unwrap();
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let op = Op::from_byte(header[4]).unwrap();
        let mut body = vec![0u8; len];
        if len > 0 {
            s.read_exact(&mut body).unwrap();
        }
        match op {
            Op::CommandComplete => return,
            Op::ErrorResponse | Op::Error => {
                let f = spg_wire::Frame { op, payload: body };
                panic!(
                    "SQL failed: {sql:?} → {}",
                    parse_error_response(&f).unwrap_or("<undecodable>")
                );
            }
            _ => continue,
        }
    }
}

fn exec_ok(s: &mut TcpStream, sql: &str) {
    send_query(s, sql);
    drain_until_cc(s, sql);
}

/// Returns `None` when the server replied with an
/// `ErrorResponse` (e.g. `table not found` before the follower
/// has rehydrated). Callers treat `None` as "not ready yet" and
/// keep polling.
fn count_rows(s: &mut TcpStream, sql: &str) -> Option<usize> {
    send_query(s, sql);
    let mut total = 0usize;
    loop {
        let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
        s.read_exact(&mut header).unwrap();
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let op = Op::from_byte(header[4]).unwrap();
        let mut body = vec![0u8; len];
        if len > 0 {
            s.read_exact(&mut body).unwrap();
        }
        match op {
            Op::DataRow => total += 1,
            Op::DataRowBatch => {
                let f = spg_wire::Frame { op, payload: body };
                if let Ok(rows) = parse_data_row_batch(&f) {
                    total += rows.len();
                }
            }
            Op::CommandComplete => return Some(total),
            Op::ErrorResponse | Op::Error => return None,
            _ => continue,
        }
    }
}

/// Same shape as `count_rows` but panics on `ErrorResponse` —
/// for queries we expect to always succeed (master side).
fn count_rows_strict(s: &mut TcpStream, sql: &str) -> usize {
    count_rows(s, sql).unwrap_or_else(|| panic!("select failed: {sql}"))
}

fn wait_for_cold_segments(s: &mut TcpStream, want: usize) {
    let deadline = Instant::now() + REPLICATION_TIMEOUT;
    loop {
        let got = count_rows_strict(s, "SELECT * FROM spg_stat_segment");
        if got >= want {
            return;
        }
        if Instant::now() > deadline {
            panic!("master never produced {want} cold segments; got {got}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Block until the master's segment count stops changing across
/// two consecutive samples 250 ms apart (the freezer has quiesced).
/// Avoids the bootstrap-vs-fresh-freeze race where the follower
/// hits N_ROWS reachable PKs while the master is still cutting
/// new cold segments off the residual hot tier.
fn wait_for_master_freezer_quiescence(s: &mut TcpStream) {
    let deadline = Instant::now() + REPLICATION_TIMEOUT;
    let mut last = count_rows_strict(s, "SELECT * FROM spg_stat_segment");
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let now = count_rows_strict(s, "SELECT * FROM spg_stat_segment");
        if now == last {
            return;
        }
        last = now;
        if Instant::now() > deadline {
            panic!("master freezer never quiesced (last segment count = {last})");
        }
    }
}

/// Count how many PKs in `range` resolve. Returns `None` if the
/// table doesn't exist yet (follower mid-snapshot), so callers
/// can retry; otherwise the hit count.
fn count_reachable_pks(s: &mut TcpStream, range: impl IntoIterator<Item = i64>) -> Option<usize> {
    let mut hit = 0;
    for id in range {
        match count_rows(s, &format!("SELECT id FROM t WHERE id = {id}")) {
            None => return None,
            Some(n) if n > 0 => hit += 1,
            Some(_) => {}
        }
    }
    Some(hit)
}

/// Build master with freezer-induced cold segments, then return
/// the follower db/wal paths + the master ChildGuard + addrs so
/// the caller can chain phase 2.
fn build_master_with_cold_segments(
    dir: &std::path::Path,
) -> (
    common::ChildGuard,
    common::ServerAddrs,
    PathBuf,
    PathBuf,
    TcpStream,
) {
    let master_db = dir.join("master.db");
    let master_wal = dir.join("master.wal");
    let (raw, master_addrs) = spawn_master(&master_db, &master_wal);
    let master_guard = common::ChildGuard(raw);
    let mut ms = common::connect_to(&master_addrs.native);
    ms.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    exec_ok(
        &mut ms,
        "CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)",
    );
    exec_ok(&mut ms, "CREATE INDEX by_id ON t (id)");
    for i in 0..N_ROWS {
        exec_ok(&mut ms, &format!("INSERT INTO t VALUES ({i}, 'row-{i}')"));
    }
    wait_for_cold_segments(&mut ms, 2);
    wait_for_master_freezer_quiescence(&mut ms);
    (master_guard, master_addrs, master_db, master_wal, ms)
}

#[test]
fn follower_bootstrap_via_forwarding() {
    let dir = unique_tmpdir("bootstrap");
    let (_master_guard, master_addrs, master_db, _master_wal, _ms) =
        build_master_with_cold_segments(&dir);

    // Attach follower.
    let follower_db = dir.join("follower.db");
    let follower_wal = dir.join("follower.wal");
    let (raw, follower_addrs) = spawn_follower(
        &follower_db,
        &follower_wal,
        master_addrs.repl.as_ref().unwrap(),
    );
    let _follower_guard = common::ChildGuard(raw);
    let mut fs = common::connect_to(&follower_addrs.native);
    fs.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // Wait for snapshot + segment forwarding to finish: every PK
    // resolves via index seek on the follower.
    let deadline = Instant::now() + REPLICATION_TIMEOUT;
    loop {
        if let Some(hit) = count_reachable_pks(&mut fs, 0..N_ROWS) {
            if hit == N_ROWS as usize {
                break;
            }
            if Instant::now() > deadline {
                panic!("follower never bootstrapped via forwarding: {hit}/{N_ROWS} PKs reachable");
            }
        } else if Instant::now() > deadline {
            panic!("follower never restored snapshot (table still missing)");
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Follower's segment directory carries the same `seg_*.spg`
    // files as the master's.
    let master_segs = list_segment_files(&segments_dir_of(&master_db));
    let follower_segs = list_segment_files(&segments_dir_of(&follower_db));
    assert!(
        !master_segs.is_empty(),
        "master segment dir is empty — freezer never persisted?"
    );
    assert_eq!(
        master_segs.len(),
        follower_segs.len(),
        "follower segment count {} ≠ master {}: {:?} vs {:?}",
        follower_segs.len(),
        master_segs.len(),
        follower_segs
            .iter()
            .map(|p| p.file_name())
            .collect::<Vec<_>>(),
        master_segs
            .iter()
            .map(|p| p.file_name())
            .collect::<Vec<_>>(),
    );
}

#[test]
fn byte_equal_segment_after_transfer() {
    let dir = unique_tmpdir("byteq");
    let (_master_guard, master_addrs, master_db, _master_wal, _ms) =
        build_master_with_cold_segments(&dir);

    let follower_db = dir.join("follower.db");
    let follower_wal = dir.join("follower.wal");
    let (raw, follower_addrs) = spawn_follower(
        &follower_db,
        &follower_wal,
        master_addrs.repl.as_ref().unwrap(),
    );
    let _follower_guard = common::ChildGuard(raw);
    let mut fs = common::connect_to(&follower_addrs.native);
    fs.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // Wait until every master segment file has a sibling on the
    // follower side.
    let master_dir = segments_dir_of(&master_db);
    let follower_dir = segments_dir_of(&follower_db);
    let master_segs = {
        let deadline = Instant::now() + REPLICATION_TIMEOUT;
        loop {
            let segs = list_segment_files(&master_dir);
            if segs.len() >= 2 {
                break segs;
            }
            if Instant::now() > deadline {
                panic!("master segments never settled to ≥ 2");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let deadline = Instant::now() + REPLICATION_TIMEOUT;
    loop {
        let follower_segs = list_segment_files(&follower_dir);
        if follower_segs.len() >= master_segs.len() {
            break;
        }
        if Instant::now() > deadline {
            panic!(
                "follower segment dir didn't catch up to master ({} vs {} files)",
                follower_segs.len(),
                master_segs.len()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Compare contents byte-by-byte. Master and follower both
    // write the same v6.6.2 v2 envelope (default `lzss`), so the
    // bytes must match exactly.
    for master_path in &master_segs {
        let name = master_path.file_name().unwrap();
        let follower_path = follower_dir.join(name);
        let master_bytes = std::fs::read(master_path).expect("read master seg");
        let follower_bytes = std::fs::read(&follower_path).expect("read follower seg");
        assert_eq!(
            master_bytes,
            follower_bytes,
            "byte mismatch for {}: master {} bytes vs follower {} bytes",
            name.to_string_lossy(),
            master_bytes.len(),
            follower_bytes.len(),
        );
    }
}

#[test]
fn resumable_after_disconnect() {
    let dir = unique_tmpdir("resume");
    let (_master_guard, master_addrs, master_db, _master_wal, _ms) =
        build_master_with_cold_segments(&dir);

    let follower_db = dir.join("follower.db");
    let follower_wal = dir.join("follower.wal");

    // Phase 1: bootstrap follower fully so segment files land.
    {
        let (raw, follower_addrs) = spawn_follower(
            &follower_db,
            &follower_wal,
            master_addrs.repl.as_ref().unwrap(),
        );
        let mut follower_guard = common::ChildGuard(raw);
        let mut fs = common::connect_to(&follower_addrs.native);
        fs.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        let deadline = Instant::now() + REPLICATION_TIMEOUT;
        loop {
            if count_reachable_pks(&mut fs, 0..N_ROWS) == Some(N_ROWS as usize) {
                break;
            }
            if Instant::now() > deadline {
                panic!("phase-1 follower bootstrap stalled");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Snapshot the segment files post-bootstrap so the
        // post-resume comparison knows what the steady state is.
        drop(fs);
        // Drop the guard kills the follower (graceful via Drop).
        std::mem::drop(follower_guard);
    }

    let follower_dir = segments_dir_of(&follower_db);
    let pre_resume = list_segment_files(&follower_dir);
    assert!(
        !pre_resume.is_empty(),
        "follower disk lost segments after kill?"
    );
    // Record mtimes so we can confirm phase-2 forwarding didn't
    // re-write the existing files.
    let pre_mtimes: Vec<_> = pre_resume
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok().and_then(|m| m.modified().ok()))
        .collect();

    // Phase 2: restart follower against the same disk. Master
    // re-sends every segment (no double-sided handshake on
    // v6.7.5); the follower's segment-level resume skips chunks
    // whose `seg_<id>.spg` files already exist.
    {
        let (raw, follower_addrs) = spawn_follower(
            &follower_db,
            &follower_wal,
            master_addrs.repl.as_ref().unwrap(),
        );
        let _follower_guard = common::ChildGuard(raw);
        let mut fs = common::connect_to(&follower_addrs.native);
        fs.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        // The follower should already serve every PK from disk
        // (cold tier was rehydrated via manifest at boot).
        let deadline = Instant::now() + REPLICATION_TIMEOUT;
        loop {
            if count_reachable_pks(&mut fs, 0..N_ROWS) == Some(N_ROWS as usize) {
                break;
            }
            if Instant::now() > deadline {
                panic!("phase-2 follower never re-bootstrapped");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Give the master a moment to push any extra frames.
        std::thread::sleep(Duration::from_millis(500));
    }

    // Segment files post-resume == phase-1 set (no new files
    // added, no existing files overwritten).
    let post_resume = list_segment_files(&follower_dir);
    assert_eq!(
        pre_resume, post_resume,
        "segment file set changed across reconnect — resume isn't segment-level"
    );
    let post_mtimes: Vec<_> = post_resume
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok().and_then(|m| m.modified().ok()))
        .collect();
    assert_eq!(
        pre_mtimes, post_mtimes,
        "segment file mtimes changed across reconnect — file was rewritten"
    );
    let master_segs = list_segment_files(&segments_dir_of(&master_db));
    for master_path in &master_segs {
        let name = master_path.file_name().unwrap();
        let follower_path = follower_dir.join(name);
        let master_bytes = std::fs::read(master_path).expect("read master seg");
        let follower_bytes = std::fs::read(&follower_path).expect("read follower seg");
        assert_eq!(master_bytes, follower_bytes);
    }
}
