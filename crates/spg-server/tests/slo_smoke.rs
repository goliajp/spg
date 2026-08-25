// Timing-sensitive perf/SLO target — meaningless under debug
// codegen, so it only compiles in release (matches the
// perf_gate convention).
#![cfg(not(debug_assertions))]
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::items_after_statements,
    clippy::manual_assert,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    unused_mut,
    unused_variables
)]

//! v4.32 SLO smoke test — a fast bench-shaped sanity check that
//! the published SLO numbers in PERFORMANCE.md §SLO still hold.
//!
//! **Purpose**: catch regressions in the latency / throughput hot
//! path before they ship. Not a perf bench (those live in
//! `xbench/competitor/`); this just gates against gross drift.
//!
//! Budget: should take < 5 s on M-series 8-core. Numbers are
//! intentionally loose — set 2× the v4.27 baseline as the floor
//! so noise / shared CI runners don't false-alarm.
//!
//! If this test fails, look at the long-form numbers in
//! `xbench/competitor/src/bin/latency.rs` /
//! `throughput.rs` for the real story.

#[path = "common/mod.rs"]
mod common;

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Child;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Serialize execution of every SLO test in this binary, with a
/// brief cool-down between tests so host noise from the previous
/// run doesn't bleed into the next test's measurement window.
///
/// Background: cargo runs `#[test]` functions across N threads (one
/// per CPU by default). For functional correctness tests that's fine.
/// For SLO tests it's catastrophic — every test here spawns an
/// spg-server child and measures p99 latency or throughput. Running
/// two such tests in parallel means two server processes share the
/// same CPU + disk fsync queue; the measured p99 reflects host
/// contention, not the path being gated. v4.42 4-client throughput
/// fell from 458 r/s isolated to 183 r/s when the multi-client SLO
/// (`slo_wal_insert_multi_client_p99_under_budget`) ran alongside,
/// blowing both gates for a reason that wasn't a real regression.
///
/// Even after serializing, the *back-to-back* effect matters: when a
/// heavy WAL test (`slo_wal_insert_1m_rows_throughput`, ~10 s of
/// continuous fsync) releases the lock, the macOS APFS journal still
/// has pending flushes and the previous child is in `waitpid` cleanup.
/// The next test's first 500 measurements pick up those tail latencies
/// and a 500 µs p99 ceiling false-alarms. A 500 ms cool-down after
/// lock acquisition gives the OS time to settle; 6 tests × 500 ms =
/// 3 s overhead on a ~20 s binary, ~15 %, which beats intermittent
/// failures by a wide margin.
fn perf_lock() -> MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = L
        .get_or_init(Mutex::default)
        // Poisoned only if a previous holder panicked; we still want
        // the next test to run (and report its own failure), so
        // unwrap the inner ().
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    thread::sleep(Duration::from_millis(500));
    guard
}

use spg_wire::{Frame, Op, build_query, encode};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// SLOs from PERFORMANCE.md §SLO (v4.32). Numbers in µs.
///
/// Loose p99 ceilings (~25× the measured v4.27 baseline of 77/70 µs)
/// so a shared/loaded CI runner — or this binary's back-to-back
/// `perf_lock` ordering, where a heavy WAL test (10 s of continuous
/// fsync) hands off to this in-memory smoke right as the APFS journal
/// is still flushing — doesn't false-alarm. The bench harness in
/// `xbench/competitor/` is the source of truth for actual numbers;
/// this gates only against gross regression (≥ 25× the typical
/// number on a quiet box). Raising these ceilings further requires a
/// PR comment explaining why the floor moved.
///
/// History: 500 µs was the original v4.32 ceiling, but at that level
/// the test failed ~20 % of full-binary runs after v6.0.0 added
/// `perf_lock` serialization (5/25 runs, one sample of 500 spiking
/// to 1100 µs from prior-test residual host noise — the underlying
/// path is still <50 µs in isolation, see PERFORMANCE.md). Raising
/// to 2000 µs keeps the safety margin honest without softening the
/// regression detector — a real wrap-clone regression would push p99
/// past 5 ms (50× baseline), well above the new ceiling.
const SLO_SEL_P99_US: u128 = 2000;
const SLO_INS_P99_US: u128 = 2000;

/// v4.34 — WAL-on INSERT p99 ceiling for the implicit-BEGIN..COMMIT
/// wrap path. fsync per write dominates this number; the ceiling
/// has to absorb pathological host I/O contention (concurrent
/// builds, other test binaries fsync'ing the same volume) since
/// the SLO test is a CI gate, not a microbenchmark. The 1 s
/// ceiling still catches the regressions the wrap is most at risk
/// of — repeated catalog clones, multiple fsyncs per write, lost
/// batching — which would push p99 by orders of magnitude. Local
/// quiet-disk baseline sits ~20 ms p99 on APFS; the
/// `xbench/competitor/src/bin/latency.rs` harness is the source
/// of truth for actual numbers.
const SLO_WAL_INS_P99_US: u128 = 1_000_000;

#[allow(dead_code)]
fn unique_tmpdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir()
        .join("spg-tests")
        .join(format!("spg-slo-{nanos}"));
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

/// Spawn an in-memory server (no db, no WAL) on an OS-chosen port.
/// Returns `(child, addr)` with the actual bound address parsed from
/// stderr. Race-free vs the old `bind:0` → drop → pass-port pattern;
/// see e2e_wal_binary's `spawn_server_on_ephemeral_port` for the
/// rationale.
fn spawn_in_memory() -> (Child, String) {
    // `SPG_DB` / `SPG_WAL` on top of the shared spawner's defaults: this
    // one wants no store at all, and an inherited path would give it one.
    let (child, addrs) = common::ServerBuilder::new()
        .env_remove("SPG_DB")
        .env_remove("SPG_WAL")
        .spawn();
    (child, addrs.native)
}

fn connect_to(addr: &str) -> TcpStream {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match TcpStream::connect(addr) {
            Ok(s) => {
                s.set_nodelay(true).ok();
                return s;
            }
            Err(e) => {
                assert!(Instant::now() < deadline, "connect {addr}: {e}");
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn round_trip(s: &mut TcpStream, sql: &str) {
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).unwrap();
    s.write_all(&out).unwrap();
    // Drain frames until CommandComplete.
    loop {
        let mut hdr = [0u8; spg_wire::FRAME_HEADER_LEN];
        s.read_exact(&mut hdr).unwrap();
        let plen = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
        let op = Op::from_byte(hdr[4]).unwrap();
        let mut payload = vec![0u8; plen];
        if plen > 0 {
            s.read_exact(&mut payload).unwrap();
        }
        match op {
            Op::CommandComplete => return,
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&Frame { op, payload })
                    .unwrap_or("<undecodable>")
                    .to_string();
                panic!("server rejected {sql:?}: {msg}");
            }
            _ => {}
        }
    }
}

fn p99(samples_us: &mut [u128]) -> u128 {
    samples_us.sort_unstable();
    let idx = ((samples_us.len() as f64) * 0.99) as usize;
    samples_us[idx.min(samples_us.len() - 1)]
}

#[test]
fn slo_smoke_select_and_insert_p99_under_budget() {
    let _perf = perf_lock();
    let (raw_child, addr) = spawn_in_memory();
    let _child = ChildGuard(raw_child);
    let mut s = connect_to(&addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    // Setup + warm-up.
    round_trip(&mut s, "CREATE TABLE slo (id INT NOT NULL, v INT NOT NULL)");
    for i in 0..200 {
        round_trip(&mut s, &format!("INSERT INTO slo VALUES ({i}, {})", i * 7));
    }
    // Warm-up.
    for _ in 0..200 {
        round_trip(&mut s, "SELECT count(*) FROM slo");
    }

    // Measure.
    const N: usize = 500;
    let mut sel = Vec::with_capacity(N);
    for _ in 0..N {
        let t = Instant::now();
        round_trip(&mut s, "SELECT count(*) FROM slo");
        sel.push(t.elapsed().as_micros());
    }

    let mut ins = Vec::with_capacity(N);
    for i in 1000..1000 + N {
        let t = Instant::now();
        round_trip(&mut s, &format!("INSERT INTO slo VALUES ({i}, {})", i * 7));
        ins.push(t.elapsed().as_micros());
    }

    let sel_p99 = p99(&mut sel);
    let ins_p99 = p99(&mut ins);
    eprintln!(
        "SLO smoke: SEL p99 = {sel_p99} µs (SLO ≤ {SLO_SEL_P99_US}) | INS p99 = {ins_p99} µs (SLO ≤ {SLO_INS_P99_US})"
    );

    assert!(
        sel_p99 <= SLO_SEL_P99_US,
        "SEL p99 {sel_p99} µs blew the SLO ceiling of {SLO_SEL_P99_US} µs — see PERFORMANCE.md §SLO and xbench/competitor/src/bin/latency.rs"
    );
    assert!(
        ins_p99 <= SLO_INS_P99_US,
        "INS p99 {ins_p99} µs blew the SLO ceiling of {SLO_INS_P99_US} µs — see PERFORMANCE.md §SLO and xbench/competitor/src/bin/latency.rs"
    );
}

fn spawn_wal(db: &std::path::Path, wal: &std::path::Path) -> (Child, String) {
    spawn_wal_with_env(db, wal, &[])
}

/// v4.42 — like `spawn_wal` but with extra env vars (e.g.
/// `SPG_COMMIT_DELAY_US` for the multi-client SLO smoke). Returns
/// `(child, addr)` from the ephemeral-port allocation.
/// v7.38.22 — one named counter out of the native `Stats` op.
///
/// The server keeps these always on and answers them over the wire
/// precisely so a measuring window can bracket a workload with them; the
/// alternative channel, `SPG_WAL_TRACE`, is env-gated and writes a line
/// from inside the fsync path, which both perturbs what it measures and
/// (its own comment says so) goes quiet exactly when fsync is off.
///
/// Panics rather than defaulting when the counter is absent: a name that
/// no longer exists must not read as zero.
fn stat_counter(s: &mut TcpStream, key: &str) -> u64 {
    let mut out = Vec::new();
    encode(&spg_wire::build_stats_request(), &mut out).unwrap();
    s.write_all(&out).unwrap();
    let mut hdr = [0u8; spg_wire::FRAME_HEADER_LEN];
    s.read_exact(&mut hdr).unwrap();
    let plen = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
    let op = Op::from_byte(hdr[4]).unwrap();
    assert_eq!(op, Op::StatsResponse, "Stats answered with {op:?}");
    let mut payload = vec![0u8; plen];
    if plen > 0 {
        s.read_exact(&mut payload).unwrap();
    }
    let body = spg_wire::parse_stats_response(&Frame {
        op,
        payload: payload.clone(),
    })
    .expect("stats body is utf-8")
    .to_string();
    body.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("Stats has no counter named {key}; it reports:\n{body}"))
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("counter {key} is not a number: {e}"))
}

/// v7.38.22 — through the SHARED spawner, which this file used to
/// duplicate.
///
/// The copy here had its own deadline and its own failure message, so a
/// missed startup said different things depending on which test binary
/// hit it — and only one of the two learned to name the host as the
/// cause. One spawner, one rule.
fn spawn_wal_with_env(
    db: &std::path::Path,
    wal: &std::path::Path,
    extra_env: &[(&str, &str)],
) -> (Child, String) {
    let mut b = common::ServerBuilder::new()
        .arg_path(db)
        .arg("-")
        .arg_path(wal);
    for (k, v) in extra_env {
        b = b.env(*k, *v);
    }
    let (child, addrs) = b.spawn();
    (child, addrs.native)
}

/// v4.34: perf gate for the implicit BEGIN..COMMIT wrap. Runs the
/// same shape of INSERTs as the in-memory smoke above, but against
/// a server with WAL enabled (so every write goes through the
/// wrap → atomic WAL block → COMMIT path).
///
/// The ceiling is set well above pure-disk fsync latency to keep
/// CI noise / shared-runner I/O contention from false-alarming. A
/// real regression in the wrap (e.g. an extra catalog clone, or a
/// missed batched fsync) would still blow it.
#[test]
fn slo_wal_insert_p99_under_budget() {
    let _perf = perf_lock();
    let dir = unique_tmpdir();
    let db = dir.join("slo.db");
    let wal = dir.join("slo.wal");
    let (raw_child, addr) = spawn_wal(&db, &wal);
    let _child = ChildGuard(raw_child);
    let mut s = connect_to(&addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

    round_trip(
        &mut s,
        "CREATE TABLE slo_wal (id INT NOT NULL, v INT NOT NULL)",
    );
    // Warm-up: amortize first-write cost (catalog grow, pagecache
    // priming, fsync metadata journaling on cold FS).
    for i in 0..200 {
        round_trip(
            &mut s,
            &format!("INSERT INTO slo_wal VALUES ({i}, {})", i * 7),
        );
    }

    const N: usize = 200;
    let mut ins = Vec::with_capacity(N);
    for i in 1000..1000 + N {
        let t = Instant::now();
        round_trip(
            &mut s,
            &format!("INSERT INTO slo_wal VALUES ({i}, {})", i * 7),
        );
        ins.push(t.elapsed().as_micros());
    }
    let ins_p99 = p99(&mut ins);
    eprintln!("SLO smoke (WAL-on): INS p99 = {ins_p99} µs (ceiling ≤ {SLO_WAL_INS_P99_US})");
    assert!(
        ins_p99 <= SLO_WAL_INS_P99_US,
        "WAL-mode INS p99 {ins_p99} µs blew the v4.34 ceiling of {SLO_WAL_INS_P99_US} µs — \
         the implicit BEGIN..COMMIT wrap may have regressed; see PROD_READY row 1.11"
    );
}

/// v4.39 — 1M-row throughput gate for the implicit BEGIN..COMMIT wrap on a
/// PV-backed catalog. Mirrors the `xbench/competitor/src/bin/sweep.rs` shape
/// (multi-VALUES INSERTs of 500 rows / batch) so the gate fires when the
/// `Catalog::clone()` inside the wrap regresses from O(1) back toward O(N
/// rows). Baseline-v4.37 sat at 9.4K r/s for this workload; v4.39 should
/// comfortably clear 50K r/s.
const SLO_V4_39_INSERT_1M_FLOOR_RPS: f64 = 50_000.0;
const SLO_V4_39_BATCH_ROWS: usize = 500;
const SLO_V4_39_TOTAL_ROWS: usize = 1_000_000;

/// v4.42 — multi-client INSERT p99 ceiling. Each client opens its
/// own connection and issues single-row INSERTs; the commit-
/// barrier leader coalesces them into groups (up to
/// `SPG_COMMIT_GROUP_MAX = 16`), sharing one fsync per group.
/// Worst-case latency includes queue-wait time for a non-leader
/// arrival, so the ceiling is looser than the single-client p99
/// (which doesn't wait).
///
/// History: 50 ms was the original v4.42 ceiling. v6.0.x's
/// `perf_lock` serialisation forces this test to run back-to-back
/// with `slo_wal_insert_4client_group_commit_coalesces` (10 s of
/// continuous WAL fsync) instead of in isolation. Even after the
/// 500 ms cool-down, 1 out of every 5 runs saw a single sample of
/// the 200 INSERTs (= the p99 sample) spike past 50 ms from
/// APFS-journal residue. Raised to 200 ms (~4× headroom) — same
/// shape as the in-memory smoke's 500 µs → 2000 µs adjustment. A
/// real group-commit regression (per-task fsync, leader handoff
/// broken, queue starvation) would push the p99 past 1 s, still
/// well above the new ceiling.
const SLO_V4_42_MULTI_CLIENT_P99_US: u128 = 200_000;

/// v4.42 — 4-client INSERT throughput floor.
///
/// **Why the floor is conservative (300 r/s, not the 148K ship
/// gate from NEXT.md):** the SLO smoke runs on the developer /
/// CI machine, and on macOS APFS a single `fsync` clocks in at
/// 5-7 ms — physical floor of all single-row write throughput on
/// that platform regardless of group size. Even ideal group
/// commit (one fsync amortised across all 4 writers) caps at
/// `4 / 6ms ≈ 660 r/s` on a quiet macOS dev box. The 148K
/// production gate from NEXT.md row 5 was sized against Linux
/// ext4/btrfs production hosts where `fsync` is sub-millisecond;
/// PERFORMANCE.md "v4.42 scale sweep" is the source-of-truth
/// number against the docker-compose competitor stack. This SLO
/// floor catches a regression where group commit fails to
/// activate at all (would drop multi-client throughput below
/// single-client, since the queue overhead would dominate).
/// v7.38.22 — commits per commit group, which is what group commit
/// produces. From measurement, not from a target.
///
/// Measured on 2,000 commits from 4 clients, three runs each:
///
///     adaptive (the default, and what this gate runs)   2.00
///     window pinned to 0                                2.77
///     window pinned to 200 µs                           2.21
///
/// The failure this guards is the leader ceasing to drain the queue,
/// which puts every commit in its own group and drives the number to
/// 1.00 on any box. 1.5 sits between that and the least favourable
/// regime measured, with room for the adaptive window to be IMPROVED
/// without the floor having to move — a floor set at the current
/// reading is a floor that fights its own fix.
const SLO_V4_42_4C_COMMITS_PER_FSYNC_FLOOR: f64 = 1.5;
const SLO_V4_42_4C_THREADS: usize = 4;
const SLO_V4_42_4C_PER_THREAD: usize = 500;

#[test]
// SLO throughput target tuned for Apple-M release; CI shared
// runners don't reliably hit it. Run locally with
// `cargo test --release -p spg-server --test slo_smoke -- --ignored`.
#[ignore]
fn slo_wal_insert_1m_rows_throughput() {
    // v7.37 (round 1002) — the heavy ones are opt-in.
    //
    // `--full` reaches them, and on a shared machine they either fail for
    // reasons that are not the code's (a 6 GiB RSS ceiling measured beside
    // another project's compiler) or run long enough to look hung: the
    // 100M-row restart sat at 0% CPU with no output for 94 minutes and took
    // the whole gate with it.
    //
    // They keep their names — `prod_ready` asserts the documentation still
    // names some of them — so this is an opt-in, not a deletion:
    //
    //     SPG_SOAK_TESTS=1 cargo test --release ... -- --ignored
    if std::env::var_os("SPG_SOAK_TESTS").is_none() {
        eprintln!(
            "skipping slo_wal_insert_1m_rows_throughput: heavy soak — set SPG_SOAK_TESTS=1 to run it"
        );
        return;
    }
    let _perf = perf_lock();
    let dir = unique_tmpdir();
    let db = dir.join("slo1m.db");
    let wal = dir.join("slo1m.wal");
    let (raw_child, addr) = spawn_wal(&db, &wal);
    let _child = ChildGuard(raw_child);
    let mut s = connect_to(&addr);
    // The 1M-row loop on a tight quiet box clears comfortably under 30 s
    // post-v4.39; the read timeout caps a pathological regression instead
    // of running the whole CI budget out.
    s.set_read_timeout(Some(Duration::from_mins(2))).unwrap();

    round_trip(
        &mut s,
        "CREATE TABLE slo1m (id INT NOT NULL, v INT NOT NULL)",
    );

    let start = Instant::now();
    let mut next_id: usize = 0;
    while next_id < SLO_V4_39_TOTAL_ROWS {
        let end = (next_id + SLO_V4_39_BATCH_ROWS).min(SLO_V4_39_TOTAL_ROWS);
        let mut sql = String::with_capacity(SLO_V4_39_BATCH_ROWS * 24);
        sql.push_str("INSERT INTO slo1m VALUES ");
        let mut first = true;
        for i in next_id..end {
            if !first {
                sql.push(',');
            }
            first = false;
            write!(sql, "({i}, {})", i * 7).expect("String write never fails");
        }
        round_trip(&mut s, &sql);
        next_id = end;
    }
    let elapsed = start.elapsed();
    let rps = SLO_V4_39_TOTAL_ROWS as f64 / elapsed.as_secs_f64();
    eprintln!(
        "SLO smoke (WAL-on, 1M rows): {rps:.0} r/s over {:.1} s (floor ≥ {} r/s)",
        elapsed.as_secs_f64(),
        SLO_V4_39_INSERT_1M_FLOOR_RPS as u64,
    );
    assert!(
        rps >= SLO_V4_39_INSERT_1M_FLOOR_RPS,
        "v4.39 1M-row INSERT throughput {rps:.0} r/s blew the floor of {:.0} r/s — \
         the PersistentVec-backed Catalog::clone may have regressed or the auto-commit \
         wrap is taking the wrong path; see NEXT.md §v4.39 + PROD_READY row 1.11",
        SLO_V4_39_INSERT_1M_FLOOR_RPS
    );
}

/// v4.42 — multi-client INSERT p99 gate. Four client threads each
/// run single-row INSERTs against the same WAL-backed server.
/// Each push goes through the commit-barrier queue; a leader
/// coalesces concurrent arrivals into one group, fsyncs once, and
/// acks every survivor. Latency tracks **per-statement
/// round-trip** wall time (including queue wait + leader's
/// install loop), so the ceiling accommodates the worst case
/// where a writer arrives at the tail of a group right after the
/// leader released the queue lock.
#[test]
fn slo_wal_insert_multi_client_p99_under_budget() {
    let _perf = perf_lock();
    let dir = unique_tmpdir();
    let db = dir.join("slo_multi.db");
    let wal = dir.join("slo_multi.wal");
    // Engage the v4.42 group-commit spin window so concurrent
    // writers coalesce into batched fsync groups (the SLO this
    // gates is the *multi-client* p99 under group commit).
    let (raw_child, addr) = spawn_wal_with_env(&db, &wal, &[("SPG_COMMIT_DELAY_US", "200")]);
    let _child = ChildGuard(raw_child);
    let mut setup = connect_to(&addr);
    setup.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    round_trip(
        &mut setup,
        "CREATE TABLE slo_multi (tid INT NOT NULL, i INT NOT NULL)",
    );
    drop(setup);

    const WARMUP: usize = 50;
    const PER_THREAD: usize = 200;
    const THREADS: usize = 4;

    let samples: Arc<std::sync::Mutex<Vec<u128>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut handles = Vec::with_capacity(THREADS);
    for t in 0..THREADS {
        let addr = addr.clone();
        let samples = Arc::clone(&samples);
        handles.push(thread::spawn(move || {
            let mut s = TcpStream::connect(&addr).expect("connect");
            s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
            // Warm-up: amortise first-write cost across leader
            // election + catalog grow + page cache.
            for i in 0..WARMUP {
                round_trip(&mut s, &format!("INSERT INTO slo_multi VALUES ({t}, {i})"));
            }
            let mut local = Vec::with_capacity(PER_THREAD);
            for i in WARMUP..WARMUP + PER_THREAD {
                let start = Instant::now();
                round_trip(&mut s, &format!("INSERT INTO slo_multi VALUES ({t}, {i})"));
                local.push(start.elapsed().as_micros());
            }
            samples
                .lock()
                .expect("samples mutex poisoned")
                .extend(local);
        }));
    }
    for h in handles {
        h.join().expect("worker thread panicked");
    }
    let mut all = samples.lock().expect("samples mutex poisoned").clone();
    let n = all.len();
    assert_eq!(n, THREADS * PER_THREAD, "sample count mismatch");
    let p99 = p99(&mut all);
    eprintln!(
        "SLO smoke (WAL-on, {THREADS} clients, {n} INSERTs total): \
         p99 = {p99} µs (ceiling ≤ {SLO_V4_42_MULTI_CLIENT_P99_US})"
    );
    assert!(
        p99 <= SLO_V4_42_MULTI_CLIENT_P99_US,
        "v4.42 multi-client INS p99 {p99} µs blew the ceiling of {SLO_V4_42_MULTI_CLIENT_P99_US} µs — \
         group commit leader handoff / queue wait may have regressed",
    );
}

/// v4.42 — 4-client group-commit gate. Four client threads stream
/// `SLO_V4_42_4C_PER_THREAD` single-row INSERTs in parallel. The leader
/// pulls multiple INSERTs into each group (rolling drain up to
/// `SPG_COMMIT_GROUP_MAX = 16`), so wall time `≈ groups × fsync_us`, not
/// `total × fsync_us`.
///
/// v7.38.22 — this **counts fsyncs** instead of timing throughput, and
/// the reason is that the timed form spent this release cycle reporting
/// the machine.
///
/// It asserted `≥ 300 r/s`. On a host at load 66-126 with 21.76 GB of
/// 22.5 GB swap in use it read 238 and failed; the same commit on a quiet
/// box was never in doubt. Nothing about the code was different, and the
/// message it printed — *group commit fsync coalescing may have
/// regressed* — was about a mechanism it never measured. (Its own doc
/// claimed a floor of `80K r/s` while the constant said `300`, which is
/// how long nobody had read it.)
///
/// Commits-per-fsync IS that mechanism, it is a count rather than a
/// duration, and its machine-sensitivity runs the SAFE way: a slower box
/// lets MORE commits queue behind the leader's fsync, so load can only
/// push this number up. A regression that stops the coalescing drives it
/// to 1.0 on any box.
///
/// Calibrated against the mechanism's own off-switch: `SPG_COMMIT_DELAY_US=0`
/// closes the window and the same workload measures ~1 commit per fsync.
/// The throughput line is still printed — as an observation, which is all
/// it ever was.
#[test]
fn slo_wal_insert_4client_group_commit_coalesces() {
    let _perf = perf_lock();
    let dir = unique_tmpdir();
    let db = dir.join("slo_4c.db");
    let wal = dir.join("slo_4c.wal");
    // Engage the v4.42 group-commit spin window so concurrent
    // writers coalesce — this gate is the throughput unlock the
    // delay is designed to demonstrate.
    // v7.38.22 — the server runs its DEFAULT commit window here, which
    // it did not before.
    //
    // This test used to pass `SPG_COMMIT_DELAY_US=200`, with a comment
    // saying that engaged the group-commit window. It does the opposite:
    // an explicit value PINS the window and turns off the adaptive one
    // (`effective_commit_delay_us`), so ever since the AIMD window landed
    // this gate has been measuring a configuration no deployment runs —
    // and measuring it in the mechanism's OFF position while asserting
    // about the mechanism.
    //
    // Measured, same box, same workload, 2,000 commits:
    //
    //     pinned 200 µs   2.21 commits/group   1.17 s
    //     pinned 0 µs     2.77 commits/group   0.47 s
    //     adaptive        see the floor below
    //
    // `SPG_TEST_COMMIT_DELAY_US` still pins it, for calibrating against
    // those two regimes; unset — the way the gate runs — the server
    // adapts, which is what customers get.
    let pin = std::env::var("SPG_TEST_COMMIT_DELAY_US").ok();
    let cap = std::env::var("SPG_TEST_COMMIT_GROUP_MAX").ok();
    let mut env: Vec<(&str, &str)> = Vec::new();
    if let Some(v) = pin.as_deref() {
        env.push(("SPG_COMMIT_DELAY_US", v));
    }
    // The negative control this floor is calibrated against:
    // `SPG_TEST_COMMIT_GROUP_MAX=1` lets the leader take one task per
    // group, which is the regression the assertion names.
    if let Some(v) = cap.as_deref() {
        env.push(("SPG_COMMIT_GROUP_MAX", v));
    }
    let (raw_child, addr) = spawn_wal_with_env(&db, &wal, &env);
    let _child = ChildGuard(raw_child);
    let mut setup = connect_to(&addr);
    setup.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    round_trip(
        &mut setup,
        "CREATE TABLE slo_4c (tid INT NOT NULL, i INT NOT NULL)",
    );
    drop(setup);

    // Warm-up via a setup connection so the steady-state phase
    // skips first-write cost.
    let mut warm = TcpStream::connect(&addr).expect("connect");
    warm.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    for i in 0..50 {
        round_trip(&mut warm, &format!("INSERT INTO slo_4c VALUES (0, {i})"));
    }
    // v7.38.22 — the counters as the warm-up leaves them, read on the
    // warm-up's own connection before it is dropped.
    let before_solo = stat_counter(&mut warm, "commit_groups_solo");
    let before_coalesced = stat_counter(&mut warm, "commit_groups_coalesced");
    let before_fsyncs = stat_counter(&mut warm, "wal_fsyncs");
    drop(warm);

    let inserted = Arc::new(AtomicUsize::new(0));
    let elapsed_ns = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::with_capacity(SLO_V4_42_4C_THREADS);
    let start_barrier = Arc::new(std::sync::Barrier::new(SLO_V4_42_4C_THREADS + 1));
    for t in 0..SLO_V4_42_4C_THREADS {
        let addr = addr.clone();
        let inserted = Arc::clone(&inserted);
        let start_barrier = Arc::clone(&start_barrier);
        let elapsed_ns = Arc::clone(&elapsed_ns);
        handles.push(thread::spawn(move || {
            let mut s = TcpStream::connect(&addr).expect("connect");
            s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            start_barrier.wait();
            let started = Instant::now();
            for i in 0..SLO_V4_42_4C_PER_THREAD {
                round_trip(&mut s, &format!("INSERT INTO slo_4c VALUES ({t}, {i})"));
            }
            let took = started.elapsed();
            inserted.fetch_add(SLO_V4_42_4C_PER_THREAD, Ordering::Relaxed);
            // Each thread reports its own wall time; the overall
            // throughput uses the max across threads so a slow
            // straggler isn't masked by faster siblings.
            let took_ns = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
            elapsed_ns.fetch_max(took_ns, Ordering::Relaxed);
        }));
    }
    start_barrier.wait();
    for h in handles {
        h.join().expect("worker thread panicked");
    }
    let total = inserted.load(Ordering::Relaxed) as f64;
    let max_ns = elapsed_ns.load(Ordering::Relaxed) as f64;
    assert!(max_ns > 0.0, "no elapsed time recorded");
    let rps = total * 1_000_000_000.0 / max_ns;

    // The instrument answers before the mechanism does. A counter that
    // did not move and a mechanism that did not run look identical from
    // here, so the reading is checked for life before it is believed.
    let mut probe = connect_to(&addr);
    probe.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let solo = stat_counter(&mut probe, "commit_groups_solo") - before_solo;
    let coalesced = stat_counter(&mut probe, "commit_groups_coalesced") - before_coalesced;
    let groups = solo + coalesced;
    let fsyncs = stat_counter(&mut probe, "wal_fsyncs") - before_fsyncs;
    drop(probe);
    assert!(
        groups > 0 && fsyncs > 0,
        "the server reports {groups} commit groups and {fsyncs} fsyncs for {total:.0} \
         committed INSERTs — a counter that stayed still is an instrument failure, not a \
         passing run"
    );
    // Two independently sited counters for one story: every group ends in
    // an fsync, so the group count cannot exceed the fsync count. If it
    // does, one of them is counting something other than what its name
    // says and neither reading can be used.
    assert!(
        groups <= fsyncs,
        "{groups} commit groups against {fsyncs} fsyncs — every group ends in an fsync, so \
         these two counters disagree about what they are counting"
    );

    let per_group = total / groups as f64;
    eprintln!(
        "SLO smoke (WAL-on, 4 clients, single-row INSERTs): {total:.0} writes in {:.3} s \
         → {rps:.0} r/s (observation); {groups} groups ({solo} solo, {coalesced} coalesced), \
         {fsyncs} fsyncs → {per_group:.2} commits per group \
         (floor ≥ {SLO_V4_42_4C_COMMITS_PER_FSYNC_FLOOR:.2})",
        max_ns / 1_000_000_000.0,
    );
    assert!(
        per_group >= SLO_V4_42_4C_COMMITS_PER_FSYNC_FLOOR,
        "v4.42 group commit coalesced {per_group:.2} commits per group, under the floor of \
         {SLO_V4_42_4C_COMMITS_PER_FSYNC_FLOOR:.2} — the leader is no longer draining the \
         queue into one fsync. ({total:.0} commits, {groups} groups, {fsyncs} fsyncs, \
         {rps:.0} r/s.) `SPG_TEST_COMMIT_GROUP_MAX=1` reproduces the uncoalesced regime: \
         it reads 1.00 here, while its throughput — 1,384 and 5,208 r/s on two runs — \
         stays far above the 300 r/s floor this assertion replaced."
    );
}

/// v5.4 — async-commit single-client INSERT throughput.
///
/// V5_DESIGN row 5.4's ship-gate: with
/// `SPG_SYNCHRONOUS_COMMIT=off`, a single client must sustain ≥
/// 200K single-row INSERTs / second. The whole point of v5.4's
/// async-commit mode is exactly this: in sync mode every CC is
/// gated on `fsync`, which on macOS APFS bottoms out at ~5-7 ms
/// per call → ~150 r/s single-client max. Async-commit removes
/// `fsync` from the hot path, leaving CPU + WAL `write_all` as
/// the only cost.
///
/// Two gates ship side-by-side, following the v5.3 doctrine
/// (CI-fast smoke + release-process exact ship gate):
///
///   * `slo_wal_insert_async_commit_smoke_speedup_vs_sync` (CI) —
///     measures the *ratio* `t_sync / t_async` on the same
///     workload at the same host. Asserts ≥ 5× speedup. This
///     is **host-noise tolerant**: both runs absorb the same OS
///     scheduler / IO contention, so the ratio stays meaningful
///     even on a CI host with concurrent rustc builds eating
///     CPU. Catches regressions where async-commit drops back
///     toward fsync-bound throughput while remaining fast
///     enough to fit the < 30 s CI budget on any reasonable
///     box.
///
///   * `slo_wal_insert_async_commit_above_200k` (release-process,
///     `#[ignore]`-marked) — 1M INSERTs, ≥ 200K r/s absolute
///     floor. This is the exact V5_DESIGN ship gate; the
///     measured number from running it lands in PERFORMANCE.md
///     §async commit. Marked `#[ignore]` because: (a) on a host
///     shared with concurrent rustc invocations the spg-server
///     child gets starved and the test runs for minutes without
///     completing; (b) the gate target is "is the async path
///     working at full speed" which doesn't belong in CI's
///     < 30 s test budget anyway.
///
/// **Why 100K, not the V5_DESIGN-spec 200K:** matches the
/// existing v4.42 4-client SLO doctrine
/// (`SLO_V4_42_4C_THROUGHPUT_FLOOR_RPS = 300`) — SLO floors are
/// sized so they catch regressions on the developer / CI machine
/// even when the V5_DESIGN target was sized against production
/// Linux numbers. On macOS APFS the per-fsync floor is ~5 ms,
/// which caps even the async-commit path (the flusher's `fsync`
/// every `SPG_FLUSHER_INTERVAL_US` µs serialises through that
/// 5 ms latency). v5.4.4 measured 122K r/s on this dev box;
/// Linux ext4/xfs hosts hit 300K+ trivially. The 100K floor is
/// 1000× the sync-mode physical maximum on the same host
/// (~150 r/s single-client, fsync-bound), so any regression
/// that re-introduces a per-batch fsync flips the test red.
/// The full 200K production target lands in PERFORMANCE.md
/// §"v5.4 async commit" alongside the measured APFS number.
const SLO_V5_4_ASYNC_FLOOR_RPS: f64 = 100_000.0;
const SLO_V5_4_ASYNC_BATCH_ROWS: usize = 500;
const SLO_V5_4_ASYNC_TOTAL_ROWS: usize = 1_000_000;
const SLO_V5_4_ASYNC_SMOKE_SPEEDUP_FLOOR: f64 = 1.5;
const SLO_V5_4_ASYNC_SMOKE_ROWS: usize = 200;

fn run_insert_workload(commit_env: &str, rows: usize, warmup: usize) -> Duration {
    let dir = unique_tmpdir();
    let db = dir.join("slo_async.db");
    let wal = dir.join("slo_async.wal");
    let env: Vec<(&str, &str)> = if commit_env.is_empty() {
        vec![]
    } else {
        vec![("SPG_SYNCHRONOUS_COMMIT", commit_env)]
    };
    let (raw_child, addr) = spawn_wal_with_env(&db, &wal, &env);
    let _child = ChildGuard(raw_child);
    let mut s = connect_to(&addr);
    s.set_read_timeout(Some(Duration::from_mins(1))).unwrap();

    round_trip(
        &mut s,
        "CREATE TABLE slo_async (id INT NOT NULL, v INT NOT NULL)",
    );
    for i in 0..warmup {
        round_trip(
            &mut s,
            &format!("INSERT INTO slo_async VALUES ({i}, {})", i * 7),
        );
    }

    let start = Instant::now();
    for i in warmup..(warmup + rows) {
        round_trip(
            &mut s,
            &format!("INSERT INTO slo_async VALUES ({i}, {})", i * 7),
        );
    }
    start.elapsed()
}

/// v5.4 — batched-VALUES INSERT workload. Mirrors
/// `xbench/competitor/src/bin/throughput.rs` exactly (and
/// `slo_wal_insert_1m_rows_throughput`'s shape) so the v5.4
/// ship-gate number is comparable to the source-of-truth
/// competitor numbers. `commit_env` is the
/// `SPG_SYNCHRONOUS_COMMIT` value; total `rows` get split into
/// statements of `batch_rows` each.
fn run_batched_insert_workload(commit_env: &str, total_rows: usize, batch_rows: usize) -> Duration {
    let dir = unique_tmpdir();
    let db = dir.join("slo_async.db");
    let wal = dir.join("slo_async.wal");
    let env: Vec<(&str, &str)> = vec![("SPG_SYNCHRONOUS_COMMIT", commit_env)];
    let (raw_child, addr) = spawn_wal_with_env(&db, &wal, &env);
    let _child = ChildGuard(raw_child);
    let mut s = connect_to(&addr);
    s.set_read_timeout(Some(Duration::from_mins(2))).unwrap();

    round_trip(
        &mut s,
        "CREATE TABLE slo_async (id INT NOT NULL, v INT NOT NULL)",
    );

    let start = Instant::now();
    let mut next_id: usize = 0;
    while next_id < total_rows {
        let end = (next_id + batch_rows).min(total_rows);
        let mut sql = String::with_capacity(batch_rows * 24);
        sql.push_str("INSERT INTO slo_async VALUES ");
        let mut first = true;
        for i in next_id..end {
            if !first {
                sql.push(',');
            }
            first = false;
            write!(sql, "({i}, {})", i * 7).expect("String write never fails");
        }
        round_trip(&mut s, &sql);
        next_id = end;
    }
    start.elapsed()
}

#[test]
fn slo_wal_insert_async_commit_smoke_speedup_vs_sync() {
    let _perf = perf_lock();
    // CI gate — host-noise-tolerant ratio test. Run the same
    // 200-INSERT workload twice, once under sync (default) and
    // once under async (`SPG_SYNCHRONOUS_COMMIT=off`). The two
    // runs share the same host (same OS scheduler, same IO
    // contention, same temp filesystem), so the ratio
    // `t_sync / t_async` reflects the v5.4 async path's
    // genuine speedup and isn't dragged around by background
    // rustc / docker / spotlight activity.
    //
    // 1.5× floor is conservative: theoretical max is ~20-50×
    // (sync `fsync` ~5 ms vs async sub-ms per CC on macOS
    // APFS). Anything below 1.5× would mean either async stopped
    // skipping fsync, the flusher monopolises the wal mutex
    // (v5.4.4 fixed by moving fsync outside the mutex via
    // `wal_sync_clone`), or the test client itself is the
    // bottleneck. v5.4.4 measured 5.4× on a quiet APFS box at
    // the start of a test run; subsequent runs after the OS
    // page cache fills (e.g. running this test after the 200K
    // ship-gate workload) drop to ~2× on the same hardware
    // because APFS write coalescing slows down. The 1.5× floor
    // sits below the worst observed (2.0×) so a real
    // regression (re-introducing per-write fsync flips the
    // ratio to ~1.0×, and removing async-commit entirely
    // produces a ratio < 1.0×) still fails the gate.
    let warmup = 20;
    let sync_dur = run_insert_workload("on", SLO_V5_4_ASYNC_SMOKE_ROWS, warmup);
    let async_dur = run_insert_workload("off", SLO_V5_4_ASYNC_SMOKE_ROWS, warmup);
    let speedup = sync_dur.as_secs_f64() / async_dur.as_secs_f64();
    eprintln!(
        "SLO smoke (sync vs async-commit, {SLO_V5_4_ASYNC_SMOKE_ROWS} INSERTs each): \
         sync = {:.3} s, async = {:.3} s, speedup = {speedup:.2}× (floor ≥ {:.1}×)",
        sync_dur.as_secs_f64(),
        async_dur.as_secs_f64(),
        SLO_V5_4_ASYNC_SMOKE_SPEEDUP_FLOOR,
    );
    assert!(
        speedup >= SLO_V5_4_ASYNC_SMOKE_SPEEDUP_FLOOR,
        "v5.4 async-commit speedup {speedup:.2}× over sync mode blew the smoke floor of {:.1}× — \
         the v5.4.2 conditional `sync_data` skip or v5.4.1 flusher may have regressed; see \
         PERFORMANCE.md §async commit for the source-of-truth number",
        SLO_V5_4_ASYNC_SMOKE_SPEEDUP_FLOOR
    );
}

#[test]
#[ignore = "release-process trigger: V5_DESIGN row 5.4 ship gate (200K r/s, 1M rows via 500-row VALUES batches — same shape as xbench/competitor/throughput.rs); ~5-10 s on a quiet box, several minutes on a CI host shared with rustc — record the measured number into PERFORMANCE.md §'v5.4 async commit'"]
fn slo_wal_insert_async_commit_above_200k() {
    let _perf = perf_lock();
    let elapsed =
        run_batched_insert_workload("off", SLO_V5_4_ASYNC_TOTAL_ROWS, SLO_V5_4_ASYNC_BATCH_ROWS);
    let rps = SLO_V5_4_ASYNC_TOTAL_ROWS as f64 / elapsed.as_secs_f64();
    eprintln!(
        "v5.4 async-commit ship gate ({SLO_V5_4_ASYNC_TOTAL_ROWS} rows via {SLO_V5_4_ASYNC_BATCH_ROWS}-row VALUES batches): {rps:.0} r/s over {:.2} s",
        elapsed.as_secs_f64(),
    );
    assert!(
        rps >= SLO_V5_4_ASYNC_FLOOR_RPS,
        "v5.4 async-commit single-client INSERT throughput {rps:.0} r/s blew the V5_DESIGN ship gate of {:.0} r/s — \
         the v5.4.2 async write path or v5.4.1 flusher may have regressed; see PERFORMANCE.md \
         §async commit for the source-of-truth number",
        SLO_V5_4_ASYNC_FLOOR_RPS
    );
}
