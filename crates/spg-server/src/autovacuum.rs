//! v7.39 (round 173) — background autovacuum worker.
//!
//! PG's autovacuum never runs inside a client statement: dedicated
//! workers wake on a naptime cadence, check the per-table dead-row
//! stats, and vacuum the tables over threshold. Before this round the
//! server inherited the engine's embedded-shaped inline trigger —
//! the unlucky DML statement that crossed `dead >= 1000 && dead*4 >=
//! live` carried the whole vacuum (compact + index rebuild, ~9-12 ms
//! on a 50k×2-index table) in its own latency.
//!
//! Now the server flips the engine's `autovacuum_inline` flag off at
//! boot (iff this worker actually spawns — never-die: no silent
//! window where neither party vacuums) and this thread drives
//! `Engine::autovacuum_tick` under the engine write lock instead.
//! Client statements only keep the dead-row meters current; the
//! vacuum cost lands on the worker cadence, off every client's
//! critical path (a concurrent statement can still queue behind the
//! tick's write lock — the coarse-RwLock wall — but no statement
//! *carries* the vacuum anymore).
//!
//! Knobs: `SPG_AUTOVACUUM=0|false|off` disables autovacuum entirely
//! (worker not spawned, inline stays off via `set_autovacuum`);
//! `SPG_AUTOVACUUM_NAPTIME_MS` sets the cadence (default 1000 ms —
//! PG's naptime is 60 s, but SPG's absolute 1000-dead-row floor is
//! far lower than PG's scale-factor thresholds, so a tighter cadence
//! keeps the bloat ceiling proportionally small).

use std::env;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::{SHUTDOWN_FLAG, ServerState};

const DEFAULT_NAPTIME_MS: u64 = 1000;
/// Lower bound — sub-10 ms ticks would spin the engine write lock
/// faster than a vacuum pass can possibly pay for.
const MIN_NAPTIME_MS: u64 = 10;
/// Cap on how long the worker sleeps between shutdown-flag checks,
/// so server stop stays responsive under a long naptime.
const SHUTDOWN_POLL_CAP: Duration = Duration::from_millis(50);

pub(crate) fn naptime_from_env() -> Duration {
    let ms = env::var("SPG_AUTOVACUUM_NAPTIME_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n >= MIN_NAPTIME_MS)
        .unwrap_or(DEFAULT_NAPTIME_MS);
    Duration::from_millis(ms)
}

/// Spawn the background autovacuum worker. The caller (server boot)
/// only invokes this when `SPG_AUTOVACUUM` is on, and flips the
/// engine's inline trigger off in the same breath — the two must
/// move together.
pub(crate) fn spawn(state: Arc<ServerState>) -> JoinHandle<()> {
    let naptime = naptime_from_env();
    thread::Builder::new()
        .name("spg-autovacuum".into())
        .spawn(move || run(&state, naptime))
        .expect("spawn autovacuum thread")
}

fn run(state: &ServerState, naptime: Duration) {
    let mut slept = Duration::ZERO;
    loop {
        if SHUTDOWN_FLAG.load(Ordering::Acquire) {
            break;
        }
        if slept < naptime {
            let remaining = naptime.saturating_sub(slept);
            let chunk = remaining.min(SHUTDOWN_POLL_CAP);
            thread::sleep(chunk);
            slept += chunk;
            continue;
        }
        slept = Duration::ZERO;
        let Ok(mut engine) = state.engine.write() else {
            // Poisoned engine lock — the server is already beyond
            // saving; stop the worker instead of spinning on it.
            break;
        };
        let vacuumed = engine.autovacuum_tick();
        drop(engine);
        if vacuumed > 0 {
            state
                .metrics
                .autovacuum_ticks
                .fetch_add(vacuumed as u64, Ordering::Relaxed);
        }
    }
}
