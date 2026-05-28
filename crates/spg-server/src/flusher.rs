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
        let async_mode = env::var("SPG_SYNCHRONOUS_COMMIT")
            .ok()
            .is_some_and(|s| matches!(s.trim().to_lowercase().as_str(), "off" | "false" | "0"));
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

/// Spawn the background flusher iff async-commit mode is opted
/// in via `SPG_SYNCHRONOUS_COMMIT=off`. Returns `None` in sync
/// mode (the default) — no flusher needed because the write path
/// already syncs every record.
pub(crate) fn spawn(state: Arc<ServerState>) -> Option<JoinHandle<()>> {
    let config = FlusherConfig::from_env();
    if !config.async_mode {
        return None;
    }
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
            Ok(_offset) => {
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
