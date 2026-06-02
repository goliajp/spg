#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::manual_assert,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    unused_mut,
    unused_variables
)]

//! v5.2.4 — chaos suite for the freezer thread (v5.2.2).
//!
//! Two tests, both written against the v5.2 → v5.3 trigger spelled
//! out in V5_DESIGN.md:
//!
//! 1. `chaos_kill_during_freeze_recovers_clean_state` (CI) — kill -9
//!    the server while the freezer is actively demoting rows. WAL
//!    replay on restart must restore every committed INSERT, and
//!    the catalog must be internally consistent (no orphan Cold
//!    locators pointing to segments that aren't reloaded). The
//!    v5.2.x design intentionally makes freezes non-durable
//!    (no WAL freeze_commit record — that's v5.3 manifest), so the
//!    expected post-restart state is "all rows back in hot tier,
//!    cold_segments_total = 0". That's clean, just not optimized.
//!
//! 2. `freeze_30m_rss_stays_under_6gib_during_sweep_loop`
//!    (`#[ignore]`) — drives 30M INSERTs at sweep schema, samples
//!    RSS every 1M rows via `ps`, asserts the process never crosses
//!    6 GiB. Marked ignored because the run is ~5-15 minutes and CI
//!    can't pay for it on every commit; release-process invocation
//!    is the contract:
//!
//!    ```sh
//!    cargo test --release -p spg-server --test e2e_chaos_freeze \
//!        -- --ignored freeze_30m_rss_stays_under_6gib_during_sweep_loop
//!    ```

use common::rss_kib_of;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use spg_wire::{Frame, Op, WireValue, build_query, encode, parse_data_row, parse_data_row_batch};

use std::thread;

mod common;

fn local_spawn(
    db: &std::path::Path,
    wal: &std::path::Path,
    env: &[(&str, String)],
) -> (std::process::Child, common::ServerAddrs) {
    let mut b = common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal);
    for (k, v) in env {
        b = b.env(*k, v);
    }
    b.spawn()
}

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-chaos-freeze-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

// `ChildGuard` from `tests/common::ChildGuard` is reused; this file
// previously had a local `pid()` helper that just forwarded to
// `child.0.id()`. Inline the call at the use sites instead.

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

fn run_query_ok(s: &mut TcpStream, sql: &str) -> bool {
    send(s, &build_query(sql));
    loop {
        let f = read_frame(s);
        match f.op {
            Op::CommandComplete => return true,
            Op::ErrorResponse | Op::Error => return false,
            _ => {}
        }
    }
}

fn exec_ok(s: &mut TcpStream, sql: &str) {
    assert!(run_query_ok(s, sql), "expected ok for {sql:?}");
}

fn select_int(s: &mut TcpStream, sql: &str) -> i64 {
    send(s, &build_query(sql));
    let rd = read_frame(s);
    if rd.op == Op::ErrorResponse {
        let msg = spg_wire::parse_error_response(&rd).unwrap_or("<undecodable>");
        panic!("server rejected SQL {sql:?}: {msg}");
    }
    assert_eq!(rd.op, Op::RowDescription);
    let mut count: i64 = -1;
    loop {
        let f = read_frame(s);
        match f.op {
            Op::DataRow => count = wire_to_i64(&parse_data_row(&f).unwrap()[0]),
            Op::DataRowBatch => {
                let rows = parse_data_row_batch(&f).unwrap();
                count = wire_to_i64(&rows[0][0]);
            }
            Op::CommandComplete => return count,
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn wire_to_i64(v: &WireValue) -> i64 {
    match v {
        WireValue::Int(n) => i64::from(*n),
        WireValue::BigInt(n) => *n,
        WireValue::Text(t) => t.parse().unwrap(),
        other => panic!("expected integer, got {other:?}"),
    }
}

/// Kill -9 the server while the freezer is actively demoting rows.
/// Restart with the same db + wal. Every INSERT that the client
/// got CC for must replay, and the catalog must be internally
/// consistent — no orphan Cold locators pointing at segments the
/// new process can't see (freezes aren't durable until v5.3
/// manifest; the v5.2.x recovery contract is "rolled back to the
/// pre-freeze state, no corruption").
#[test]
fn chaos_kill_during_freeze_recovers_clean_state() {
    let dir = unique_tmpdir("kill-mid-freeze");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");

    // Tight budget + fast tick → freezer fires within a few hundred
    // ms of the budget being exceeded. PK gets a BTree index so the
    // freezer's target-picking accepts the table.
    let env: Vec<(&str, String)> = vec![
        ("SPG_HOT_TIER_BYTES", "512".to_string()),
        ("SPG_FREEZER_TICK_MS", "20".to_string()),
        ("SPG_FREEZER_BATCH_ROWS", "4".to_string()),
    ];

    let committed: i64;
    {
        let (raw, addrs1) = local_spawn(&db, &wal, &env);
        let mut c = common::ChildGuard(raw);
        let mut s = common::connect_to(&addrs1.native);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

        exec_ok(
            &mut s,
            "CREATE TABLE big (id BIGINT NOT NULL, name TEXT NOT NULL)",
        );
        exec_ok(&mut s, "CREATE INDEX by_id ON big (id)");
        // Push past the 512 B budget by a wide margin — ~50 rows
        // gives ~1.5 KiB of hot data so the freezer has ≥3 batches
        // worth of work to do in flight.
        let mut written: i64 = 0;
        for i in 0..50i64 {
            if run_query_ok(&mut s, &format!("INSERT INTO big VALUES ({i}, 'u-{i}')")) {
                written += 1;
            } else {
                break;
            }
        }
        committed = written;
        assert!(
            committed >= 40,
            "expected most inserts to succeed; got {committed}"
        );

        // Let the freezer have a few ticks to start demoting before
        // the kill. We can't easily prove the kill lands mid-freeze
        // deterministically — but with a 20 ms tick and ≥10 batches
        // of work to do, the probability the kill hits during a
        // freezer-held write lock is high enough across a single
        // CI run that the test exercises the recovery path in
        // expectation. The recovery invariant must hold regardless
        // of where the kill lands; that's the whole point.
        thread::sleep(Duration::from_millis(80));

        // SIGKILL — Child::kill on Unix sends SIGKILL by default.
        let _ = c.0.kill();
        let _ = c.0.wait();
    }
    thread::sleep(Duration::from_millis(200));

    // Restart on a fresh port, same files. The expected recovery
    // shape:
    //   - WAL replay restores every committed INSERT into the hot
    //     tier (committed BEFORE the kill).
    //   - Any freezes the freezer did in-memory pre-kill are lost
    //     (no WAL record; cold_segments_total = 0).
    //   - count(*) returns exactly `committed`.
    //   - Random PK lookups for ids in [0, committed) all resolve.
    let (raw, addrs2) = local_spawn(&db, &wal, &[("SPG_FREEZER_DISABLE", "1".to_string())]);
    let _c2 = common::ChildGuard(raw);
    let mut s2 = common::connect_to(&addrs2.native);
    s2.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    let after = select_int(&mut s2, "SELECT count(*) FROM big");
    assert_eq!(
        after, committed,
        "WAL replay must restore every CC'd INSERT (got {after}, want {committed})"
    );
    // Random PK lookups for a handful of pre-kill ids — every one
    // must resolve through the hot tier (freezes were lost), and
    // none must surface a stale Cold-locator-pointing-to-missing-
    // segment error. Pick the boundary cases (0, mid, last).
    let pick = [0i64, committed / 2, committed - 1];
    for id in pick {
        if id < 0 {
            continue;
        }
        let n = select_int(
            &mut s2,
            &format!("SELECT count(*) FROM big WHERE id = {id}"),
        );
        assert_eq!(n, 1, "PK {id} must resolve post-restart (got {n})");
    }
}

// ---- 30M INSERT loop RSS gate ------------------------------------

/// v5.2 → v5.3 trigger half 2: 30M INSERT loop completes without
/// the process RSS climbing past 6 GiB at any sample point. Marked
/// `#[ignore]` because the run is slow; the release-process invocation
/// is documented in this file's module docstring. The ship-gate
/// number lands in PROD_READY.md alongside the chaos sign-off.
///
/// **Note on row width**: this test uses a sweep-style schema (id
/// BIGINT + 16-byte name) tuned so 30M rows ≈ 750 MiB encoded.
/// With `SPG_HOT_TIER_BYTES = 512 MiB`, the freezer demotes ~⅔ of
/// the corpus to cold segments; the remaining hot footprint plus
/// page cache plus index bytes must stay under 6 GiB.
#[test]
#[ignore = "release-process trigger: ~10 min runtime; see file docstring"]
fn freeze_30m_rss_stays_under_6gib_during_sweep_loop() {
    const TOTAL_ROWS: i64 = 30_000_000;
    const SAMPLE_EVERY: i64 = 1_000_000;
    const RSS_CEILING_KIB: u64 = 6 * 1024 * 1024; // 6 GiB

    let dir = unique_tmpdir("rss-30m");
    let db = dir.join("a.db");
    let wal = dir.join("a.wal");

    let env: Vec<(&str, String)> = vec![
        // 512 MiB budget — the freezer should run constantly past
        // the first few million rows and keep hot footprint near
        // the budget.
        ("SPG_HOT_TIER_BYTES", (512u64 * 1024 * 1024).to_string()),
        ("SPG_FREEZER_TICK_MS", "200".to_string()),
        // 50k rows per batch — at sweep row width that's ~1.2 MiB
        // of encode work per freezer tick, well under 250 ms p99.
        ("SPG_FREEZER_BATCH_ROWS", "50000".to_string()),
    ];

    let (raw, addrs) = local_spawn(&db, &wal, &env);
    let c = common::ChildGuard(raw);
    let pid = c.0.id();
    let mut s = common::connect_to(&addrs.native);
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();

    exec_ok(
        &mut s,
        "CREATE TABLE sweep (id BIGINT NOT NULL, payload TEXT NOT NULL)",
    );
    exec_ok(&mut s, "CREATE INDEX sweep_by_id ON sweep (id)");

    let mut peak_kib: u64 = 0;
    for i in 0..TOTAL_ROWS {
        // 16-byte payload keeps a known row width for the gate
        // calculation.
        let sql = format!("INSERT INTO sweep VALUES ({i}, 'p_{:013}')", i);
        if !run_query_ok(&mut s, &sql) {
            panic!("INSERT at row {i} failed");
        }
        if (i + 1) % SAMPLE_EVERY == 0 {
            let rss = rss_kib_of(pid);
            peak_kib = peak_kib.max(rss);
            assert!(
                rss <= RSS_CEILING_KIB,
                "RSS {} KiB exceeded 6 GiB ceiling at row {} (peak {} KiB)",
                rss,
                i + 1,
                peak_kib
            );
        }
    }
    eprintln!("freeze-30m: peak RSS = {} KiB", peak_kib);

    // Sanity: post-loop, both ends of the corpus must still resolve
    // through the indexed PK path.
    for id in [0i64, TOTAL_ROWS / 2, TOTAL_ROWS - 1] {
        let n = select_int(
            &mut s,
            &format!("SELECT count(*) FROM sweep WHERE id = {id}"),
        );
        assert_eq!(n, 1, "PK {id} must resolve at end of 30M sweep");
    }
}

// v6.0.1 step 8: `rss_kib_of` was promoted to
// `tests/common/mod.rs::rss_kib_of`. Callers in this file
// take it from `common::*` via the existing `use common::*`
// at the top of the spawn block.
