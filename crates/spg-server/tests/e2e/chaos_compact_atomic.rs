//! v6.7.3 ship-gate #3 — `manifest_swap_is_atomic_under_crash`.
//!
//! Scenario:
//!   1. Ingest rows + let the freezer cut ≥ 2 small cold segments.
//!   2. `CHECKPOINT` — manifest now lists the sources, catalog
//!      snapshot baked the BTree-Cold locators that point at them.
//!   3. Restart cleanly, verify every PK resolves, run
//!      `COMPACT COLD SEGMENTS` — in-memory BTree is rewritten,
//!      merged segment is persisted, but **no CHECKPOINT happens
//!      afterwards**.
//!   4. `SIGKILL` mid-flight (== crash before any next CHECKPOINT
//!      could land).
//!   5. Restart. Because the disk state (catalog snapshot + manifest +
//!      WAL) wasn't touched after step 2, the boot path resolves
//!      back to the pre-compact state — every original PK still
//!      visible.
//!
//! This locks the atomicity guarantee from V6_7_DESIGN.md §4:
//! compaction is a fully in-memory swap with disk-side effects
//! (merged segment file, paths-map update) that aren't committed
//! until the next CHECKPOINT writes them into the manifest. A
//! crash between compaction and CHECKPOINT must roll back cleanly.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use spg_wire::{Op, build_query, encode, parse_data_row_batch, parse_error_response};

const READ_TIMEOUT: Duration = Duration::from_secs(10);

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-chaos-compact-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn graceful_stop(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as libc::pid_t;
        // SAFETY: libc::kill FFI; pid live from child.id().
        let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let _ = child.wait();
}

fn sigkill_stop(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as libc::pid_t;
        // SAFETY: libc::kill FFI; pid live from child.id().
        let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let _ = child.wait();
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

fn exec_native(s: &mut TcpStream, sql: &str) {
    send_query(s, sql);
    drain_until_cc(s, sql);
}

fn count_rows(s: &mut TcpStream, sql: &str) -> usize {
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
            Op::CommandComplete => return total,
            Op::ErrorResponse | Op::Error => {
                let f = spg_wire::Frame { op, payload: body };
                panic!(
                    "select failed: {sql} → {}",
                    parse_error_response(&f).unwrap_or("<undecodable>")
                );
            }
            _ => continue,
        }
    }
}

/// Count how many `id` values in `range` resolve to exactly one row
/// via a PK = lookup. v5.2.x cold tier is reachable only via index
/// seek (PK = literal), not via full table scan, so this is the
/// canonical "did the frozen rows survive?" probe.
fn count_present_pks(s: &mut TcpStream, range: impl IntoIterator<Item = i64>) -> usize {
    let mut hit = 0usize;
    for id in range {
        let n = count_rows(s, &format!("SELECT id FROM t WHERE id = {id}"));
        if n > 0 {
            hit += 1;
        }
    }
    hit
}

fn wait_for_cold_segments(s: &mut TcpStream, want: usize) {
    // SELECT * required — `spg_stat_segment` is a virtual table
    // and the engine's short-circuit dispatch only matches the
    // bare `SELECT * FROM <view>` shape.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let got = count_rows(s, "SELECT * FROM spg_stat_segment");
        if got >= want {
            return;
        }
        if Instant::now() > deadline {
            panic!("freezer never produced {want} cold segments; got {got}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn manifest_swap_is_atomic_under_crash() {
    let dir = unique_tmpdir("atomic");
    let db = dir.join("spg.db");
    let wal = dir.join("wal.log");

    // Phase 1: ingest + freeze + CHECKPOINT.
    {
        let (mut raw, addrs) = common::ServerBuilder::new()
            .arg_path(&db)
            .env("SPG_WAL", wal.to_string_lossy().into_owned())
            // Tight budget so the freezer fires within a few inserts.
            .env("SPG_HOT_TIER_BYTES", "32")
            .env("SPG_FREEZER_TICK_MS", "50")
            .env("SPG_FREEZER_BATCH_ROWS", "3")
            .spawn();
        {
            let mut s = TcpStream::connect(&addrs.native).unwrap();
            s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
            exec_native(&mut s, "CREATE TABLE t (id INT NOT NULL, name TEXT)");
            exec_native(&mut s, "CREATE INDEX by_id ON t (id)");
            for i in 0..12i64 {
                exec_native(&mut s, &format!("INSERT INTO t VALUES ({i}, 'row-{i}')"));
            }
            wait_for_cold_segments(&mut s, 2);
            // Sanity: every PK is reachable via index seek BEFORE
            // checkpoint (hot + cold tier combined). `SELECT *`
            // alone walks the hot tier only — cold rows need PK seek.
            let pre_ck = count_present_pks(&mut s, 0..12);
            assert_eq!(pre_ck, 12, "pre-checkpoint reachable PKs");
            // CHECKPOINT pins manifest + catalog snapshot to the
            // pre-compact state.
            exec_native(&mut s, "CHECKPOINT");
        }
        graceful_stop(&mut raw);
    }

    // Phase 2: restart, sanity-check, COMPACT, SIGKILL.
    {
        let (mut raw, addrs) = common::ServerBuilder::new()
            .arg_path(&db)
            .env("SPG_WAL", wal.to_string_lossy().into_owned())
            // Freezer off so we don't race on new segments.
            .env("SPG_FREEZER_DISABLE", "1")
            .spawn();
        {
            let mut s = TcpStream::connect(&addrs.native).unwrap();
            s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
            // Pre-compact sanity: all 12 PKs are reachable via
            // PK seek (hot + cold sources combined).
            let pre = count_present_pks(&mut s, 0..12);
            assert_eq!(pre, 12, "pre-compact reachable PKs");
            // Run compaction with a generous threshold so the
            // freezer-made small segments merge.
            exec_native(&mut s, "COMPACT COLD SEGMENTS");
            // Sanity again — in-memory state shows the merged
            // segment now, every PK still reachable.
            let mid = count_present_pks(&mut s, 0..12);
            assert_eq!(mid, 12, "post-compact in-memory reachable PKs");
        }
        // Crash before any CHECKPOINT lands.
        sigkill_stop(&mut raw);
    }

    // Phase 3: restart — manifest still points at sources, catalog
    // snapshot still has Cold locators on the source ids. Every
    // original PK must remain visible.
    {
        let (mut raw, addrs) = common::ServerBuilder::new()
            .arg_path(&db)
            .env("SPG_WAL", wal.to_string_lossy().into_owned())
            .env("SPG_FREEZER_DISABLE", "1")
            .spawn();
        let mut s = TcpStream::connect(&addrs.native).unwrap();
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        let after = count_present_pks(&mut s, 0..12);
        graceful_stop(&mut raw);
        assert_eq!(
            after, 12,
            "post-crash reachable PKs must match pre-compact (manifest swap is atomic)"
        );
    }
}

/// Companion test: when a CHECKPOINT lands *after* the compact, the
/// merged segment is the one the manifest commits to. A subsequent
/// restart resolves every PK through the merged segment; the
/// orphaned source files on disk are harmless (not referenced by
/// the manifest, never loaded).
#[test]
fn checkpoint_after_compact_commits_merged_segment() {
    let dir = unique_tmpdir("commit");
    let db = dir.join("spg.db");
    let wal = dir.join("wal.log");

    {
        let (mut raw, addrs) = common::ServerBuilder::new()
            .arg_path(&db)
            .env("SPG_WAL", wal.to_string_lossy().into_owned())
            .env("SPG_HOT_TIER_BYTES", "32")
            .env("SPG_FREEZER_TICK_MS", "50")
            .env("SPG_FREEZER_BATCH_ROWS", "3")
            .spawn();
        {
            let mut s = TcpStream::connect(&addrs.native).unwrap();
            s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
            exec_native(&mut s, "CREATE TABLE t (id INT NOT NULL, name TEXT)");
            exec_native(&mut s, "CREATE INDEX by_id ON t (id)");
            for i in 0..12i64 {
                exec_native(&mut s, &format!("INSERT INTO t VALUES ({i}, 'row-{i}')"));
            }
            wait_for_cold_segments(&mut s, 2);
            exec_native(&mut s, "CHECKPOINT");
        }
        graceful_stop(&mut raw);
    }
    {
        let (mut raw, addrs) = common::ServerBuilder::new()
            .arg_path(&db)
            .env("SPG_WAL", wal.to_string_lossy().into_owned())
            .env("SPG_FREEZER_DISABLE", "1")
            .spawn();
        {
            let mut s = TcpStream::connect(&addrs.native).unwrap();
            s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
            exec_native(&mut s, "COMPACT COLD SEGMENTS");
            // Commit the post-compact state into the manifest.
            exec_native(&mut s, "CHECKPOINT");
        }
        graceful_stop(&mut raw);
    }
    // Restart after the post-compact CHECKPOINT: manifest must
    // resolve to the merged segment, every PK still visible.
    {
        let (mut raw, addrs) = common::ServerBuilder::new()
            .arg_path(&db)
            .env("SPG_WAL", wal.to_string_lossy().into_owned())
            .env("SPG_FREEZER_DISABLE", "1")
            .spawn();
        let mut s = TcpStream::connect(&addrs.native).unwrap();
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        let after = count_present_pks(&mut s, 0..12);
        graceful_stop(&mut raw);
        assert_eq!(
            after, 12,
            "post-CHECKPOINT restart must resolve every PK through the merged segment"
        );
    }
}
