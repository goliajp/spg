//! v5.4.1 — background flusher thread (async-commit mode).
//!
//! In sync-commit mode (`SPG_SYNCHRONOUS_COMMIT=on`, the default)
//! the flusher does NOTHING: every WAL write path already
//! `sync_data`s before acknowledging the client (v4.42 group
//! commit + v5.3.2 CHECKPOINT both call `sync_data` themselves).
//! No flusher thread is spawned in that mode and the env knob is
//! the only user-visible knob.
//!
//! In async-commit mode (`SPG_SYNCHRONOUS_COMMIT=off`), the
//! flusher thread runs the periodic emission of v5.4.0
//! `durability_checkpoint` markers. Each marker tells future
//! crash-recovery readers that "every WAL byte before this
//! marker reached `fsync` at the time the marker was written."
//!
//! v5.4.1 ships the flusher infrastructure but **does not yet
//! change the client write path** — every INSERT still
//! `sync_data`s synchronously. The async write path lands in
//! v5.4.2; until then the flusher's markers are visible-but-
//! decorative (they cost ~17 bytes / interval and one extra
//! `fsync` on the WAL mutex but don't gate any client ack). This
//! sequencing lets the wire format, env knob, /metrics surface,
//! and lifecycle integration all settle before the durability-
//! semantics change in v5.4.2 ships.
//!
//! Cadence default: 200 µs per marker. Picked aggressively
//! relative to PG's `wal_writer_delay = 200ms` so the durability
//! window stays bounded at hundreds of microseconds even under
//! 100K+ records/sec load — matching the v5.4 ship-gate target
//! (single-client INSERT ≥ 200K r/s with async-commit on). The
//! per-iteration `fsync` cost is the same whether we run it at
//! 200 µs or 200 ms; the window is what changes.

use std::env;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::{SHUTDOWN_FLAG, ServerState, append_durability_marker};

pub(crate) const DEFAULT_INTERVAL_US: u64 = 200;
/// Lower bound on the per-marker interval — sub-10 µs ticks
/// would spin the WAL mutex faster than `sync_data` can return,
/// burning CPU for no durability-window improvement.
const MIN_INTERVAL_US: u64 = 10;
/// Cap on how long the worker may sleep between shutdown-flag
/// checks. The marker interval can be arbitrarily small, but the
/// shutdown wake-up needs to stay responsive — bound it at 50 ms
/// so server stop doesn't hang waiting for one more marker.
const SHUTDOWN_POLL_CAP: Duration = Duration::from_millis(50);

/// Configuration knobs surfaced via env vars. `async_mode` is
/// derived from `SPG_SYNCHRONOUS_COMMIT` (off/false/0 → async);
/// when false the flusher is never spawned in the first place.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FlusherConfig {
    pub(crate) interval: Duration,
    pub(crate) async_mode: bool,
}

impl FlusherConfig {
    pub(crate) fn from_env() -> Self {
        // v5.4.2 — single point of truth for the
        // SPG_SYNCHRONOUS_COMMIT parse. Keeps the env contract
        // consistent between this module and the WAL write path
        // even if the keyword set is widened later.
        let async_mode = crate::synchronous_commit_disabled();
        let interval_us = env::var("SPG_FLUSHER_INTERVAL_US")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&n| n >= MIN_INTERVAL_US)
            .unwrap_or(DEFAULT_INTERVAL_US);
        Self {
            interval: Duration::from_micros(interval_us),
            async_mode,
        }
    }
}

/// Spawn the background flusher. Before v7.37.15 this only spawned
/// when async-commit mode was opted in via
/// `SPG_SYNCHRONOUS_COMMIT=off`; now it always spawns (like PG's
/// wal writer) but stays **dormant** — no markers, no fsync, one
/// atomic load per 50 ms — until either the env opts into async
/// mode or a session's `SET synchronous_commit = off` commit
/// latches `async_commit_dirty`. Sync-mode servers that never see
/// an async commit keep their exact pre-7.37.15 WAL byte stream
/// (zero marker frames).
pub(crate) fn spawn(state: Arc<ServerState>) -> Option<JoinHandle<()>> {
    let config = FlusherConfig::from_env();
    let handle = thread::Builder::new()
        .name("spg-flusher".into())
        .spawn(move || run(&state, config))
        .expect("spawn flusher thread");
    Some(handle)
}

fn run(state: &ServerState, config: FlusherConfig) {
    let mut last_tick = Instant::now();
    loop {
        if SHUTDOWN_FLAG.load(Ordering::Acquire) {
            break;
        }
        // v7.37.15 (r172) — dormant until async commits exist. The
        // dirty latch is set by the WAL append path the first time a
        // session-GUC async commit skips its fsync; it is never
        // cleared (PG's wal writer also never stops).
        if !config.async_mode && !state.async_commit_dirty.load(Ordering::Acquire) {
            thread::sleep(SHUTDOWN_POLL_CAP);
            continue;
        }
        let elapsed = last_tick.elapsed();
        if elapsed < config.interval {
            // `saturating_sub` keeps the lint happy without
            // changing observable behaviour — the branch guard
            // already proved `elapsed < config.interval`.
            let remaining = config.interval.saturating_sub(elapsed);
            thread::sleep(remaining.min(SHUTDOWN_POLL_CAP));
            continue;
        }
        last_tick = Instant::now();
        match append_durability_marker(state) {
            Ok(pre_marker_offset) => {
                // v5.4.3 — update durability barrier metadata so
                // /metrics can compute lag. `pre_marker_offset` is
                // the WAL byte position the marker started at; the
                // 17-byte marker frame itself lands inside the same
                // `sync_data`, so the post-marker WAL end is the
                // last byte guaranteed durable. `MARKER_FRAME_BYTES`
                // mirrors the v5.4.0 wire pin (4 sentinel+len + 4
                // CRC + 1 type + 8 payload).
                const MARKER_FRAME_BYTES: u64 = 17;
                let post_marker_offset = pre_marker_offset.saturating_add(MARKER_FRAME_BYTES);
                state
                    .metrics
                    .last_durable_wal_offset
                    .store(post_marker_offset, Ordering::Relaxed);
                // `as_micros()` returns u128; clamp into u64 so a
                // pathological clock skew can't wrap. Anything
                // sensible (epoch micros < ~5.8e18) round-trips
                // exactly.
                let now_us = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .and_then(|d| u64::try_from(d.as_micros()).ok())
                    .unwrap_or(u64::MAX);
                state.metrics.last_fsync_us.store(now_us, Ordering::Relaxed);
                state
                    .metrics
                    .flusher_iterations
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                // ENOSPC / quota exceeded / mutex poisoned — log
                // and surface via metrics. The next tick retries;
                // a persistent failure shows up as a flatline of
                // `spg_flusher_iterations_total` plus a rising
                // `spg_flusher_errors_total`, so operators can
                // see it without scraping logs.
                eprintln!("spg-flusher: append_durability_marker failed: {e}");
                state.metrics.flusher_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}
