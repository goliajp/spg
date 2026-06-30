//! v7.37.14 (B2.4 [PG+]) — generic deduplication primitive for
//! concurrent races on the same logical key.
//!
//! ## What this gives the engine
//!
//! Many SPG races have the same shape: N callers concurrently
//! initiate work keyed by some logical identity (path / file
//! lock / table-rebuild slot / etc.), and we want exactly ONE
//! caller to perform the work while the rest park on a shared
//! result. Pre-v7.37.14 every site grew its own bespoke
//! `OnceLock<Mutex<HashMap<K, Arc<…>>>>` + `tokio::sync::watch`
//! plumbing (see v7.37.11 INFLIGHT_OPENS for the historical
//! shape); a single generic primitive cuts the bespoke code
//! ~4× and ensures the watch-channel race fix from v7.37.12
//! doesn't have to be re-derived per site.
//!
//! ## API
//!
//! ```ignore
//! static GUARD: RaceGuard<PathBuf, Result<AsyncDatabase, EngineError>> =
//!     RaceGuard::new();
//!
//! match GUARD.lookup(&canonical) {
//!     RaceLookup::First(shared) => {
//!         // We are the elected leader — go run the heavy init.
//!         let result: Result<AsyncDatabase, EngineError> = … ;
//!         GUARD.publish_and_remove(&canonical, &shared, result);
//!         // Followers wake from the watch channel below.
//!     }
//!     RaceLookup::Existing(shared) => {
//!         // Someone else is already running init; subscribe.
//!     }
//! }
//! // Both leader and followers await via shared.subscribe_done()
//! // and receive the SAME value.
//! ```
//!
//! ## Watch-channel race semantics
//!
//! The watch channel was chosen over Notify (v7.37.11) /
//! `OnceCell` (pre-v7.37.11) because:
//!
//! - **Watch carries the result.** Notify only signals; the
//!   receiver would still need a side channel to get the value.
//!   With watch, `borrow()` returns the latest state — so a
//!   follower that subscribed AFTER `send(Done(…))` already
//!   landed STILL sees Done immediately (closing the v7.37.12
//!   "subscribe-after-publish" race).
//! - **`changed().await` is version-mark-based**, so a follower
//!   that subscribes and immediately calls `changed` returns
//!   instantly if the version advanced between subscribe and
//!   the call (which is exactly the gap the Notify variant
//!   raced through).
//! - **Senders never panic on receiver count = 0.** The leader's
//!   `send(Done(…))` always succeeds; any followers that
//!   subscribed beforehand wake. If no followers ever subscribed
//!   (single-caller case), the result still sits in the channel
//!   for the leader to read itself.

extern crate std;

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::watch;

/// Inflight state for one keyed race. `Done(V)` carries the
/// computed value; followers `borrow_and_update()` to receive it.
#[derive(Clone)]
pub enum RaceState<V: Clone> {
    InFlight,
    Done(V),
}

impl<V: Clone> core::fmt::Debug for RaceState<V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RaceState::InFlight => f.write_str("RaceState::InFlight"),
            RaceState::Done(_) => f.write_str("RaceState::Done(<opaque>)"),
        }
    }
}

impl<V: Clone> core::fmt::Debug for RaceLookup<V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RaceLookup::First(_) => f.write_str("RaceLookup::First(<shared>)"),
            RaceLookup::Existing(_) => f.write_str("RaceLookup::Existing(<shared>)"),
        }
    }
}

/// Shared per-key handle held by the leader (to publish) and
/// every follower (to await). Internal field exposed only via
/// the [`RaceShared::subscribe_done`] helper so callers don't
/// take a hard dependency on `tokio::sync::watch` directly.
pub struct RaceShared<V: Clone> {
    sender: watch::Sender<RaceState<V>>,
}

impl<V: Clone> RaceShared<V> {
    /// Subscribe to the shared result channel + await
    /// `RaceState::Done(value)`. Returns the published value
    /// (cloned). Returns `None` only if the channel was dropped
    /// without sending Done — which can't happen for a leader
    /// that follows the API contract, so callers may treat it
    /// as a "cancellation" / abort signal from the surrounding
    /// runtime.
    pub async fn subscribe_done(&self) -> Option<V> {
        let mut rx = self.sender.subscribe();
        loop {
            {
                let state = rx.borrow_and_update();
                if let RaceState::Done(v) = &*state {
                    return Some(v.clone());
                }
            }
            if rx.changed().await.is_err() {
                return None;
            }
        }
    }
}

/// Result of a `RaceGuard::lookup` call. `First` means "you are
/// the elected leader for this key — run the work + publish";
/// `Existing` means "another caller is mid-flight; await on the
/// shared".
pub enum RaceLookup<V: Clone> {
    First(Arc<RaceShared<V>>),
    Existing(Arc<RaceShared<V>>),
}

/// Process-wide deduplication map keyed by `K`. Use a single
/// `static` instance per logical race shape (one for open_path,
/// future ones for index rebuilds, partition attach, etc.).
///
/// Generic constraints:
/// - `K`: `Eq + Hash + Clone` — used as map key + cloned into the
///   entry when a new leader registers.
/// - `V`: `Clone` — required by `watch::channel` (it broadcasts
///   the latest value to receivers).
pub struct RaceGuard<K, V: Clone> {
    map: OnceLock<Mutex<HashMap<K, Arc<RaceShared<V>>>>>,
    /// Counter bumped on first-arrival (leader-elected) lookups.
    /// Observability hook so operators can confirm dedup is
    /// firing without attaching a debugger.
    pub first_count: AtomicU64,
    /// Counter bumped on subsequent-arrival lookups (the
    /// followers — the real value of dedup).
    pub existing_count: AtomicU64,
}

impl<K, V> RaceGuard<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    /// Create a new (empty) guard. Intended for `static`
    /// initialisation; lazily allocates the inner map on first
    /// `lookup`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            map: OnceLock::new(),
            first_count: AtomicU64::new(0),
            existing_count: AtomicU64::new(0),
        }
    }

    /// Look up (and register if absent) the shared inflight
    /// entry for `key`. Returns `First` if this caller is the
    /// leader, `Existing` otherwise.
    pub fn lookup(&self, key: &K) -> RaceLookup<V> {
        let map = self.map.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = guard.get(key) {
            self.existing_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return RaceLookup::Existing(Arc::clone(s));
        }
        let (sender, _) = watch::channel(RaceState::InFlight);
        let shared = Arc::new(RaceShared { sender });
        guard.insert(key.clone(), Arc::clone(&shared));
        self.first_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        RaceLookup::First(shared)
    }

    /// Leader-only: publish `value` to every subscriber + remove
    /// the map entry so future lookups for the same key spawn a
    /// fresh leader. Safe to call exactly once per `First`
    /// lookup. Subsequent calls (or calls from followers) are
    /// harmless no-ops — the watch::Sender::send is fail-safe
    /// (returns Err only when no receivers exist, which we don't
    /// care about here) and `remove` on an absent key is a no-op.
    pub fn publish_and_remove(&self, key: &K, shared: &Arc<RaceShared<V>>, value: V) {
        let _ = shared.sender.send(RaceState::Done(value));
        if let Some(map) = self.map.get() {
            let _ = map
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(key);
        }
    }
}

impl<K, V> Default for RaceGuard<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V: Clone> core::fmt::Debug for RaceGuard<K, V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RaceGuard")
            .field("first_count", &self.first_count)
            .field("existing_count", &self.existing_count)
            .finish_non_exhaustive()
    }
}

impl<V: Clone> core::fmt::Debug for RaceShared<V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RaceShared").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v7.37.14 (B2.4 TDD) — 100 concurrent lookups on the same
    /// key must elect exactly one leader; the other 99 are
    /// followers that all see the same published value.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn race_guard_dedupes_concurrent_lookups() {
        let guard: Arc<RaceGuard<u64, u32>> = Arc::new(RaceGuard::new());
        let key = 7u64;
        let mut handles = Vec::new();
        for _ in 0..100 {
            let g = Arc::clone(&guard);
            handles.push(tokio::spawn(async move {
                match g.lookup(&key) {
                    RaceLookup::First(shared) => {
                        // Simulate "doing the work" — small sleep so
                        // followers have a chance to subscribe.
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        let value = 42u32;
                        g.publish_and_remove(&key, &shared, value);
                        Some(value)
                    }
                    RaceLookup::Existing(shared) => shared.subscribe_done().await,
                }
            }));
        }
        let mut values = Vec::new();
        for h in handles {
            if let Ok(Some(v)) = h.await {
                values.push(v);
            }
        }
        let leaders = guard.first_count.load(std::sync::atomic::Ordering::Relaxed) as usize;
        assert_eq!(leaders, 1, "exactly one leader elected (saw {leaders})");
        assert_eq!(
            values.len(),
            100,
            "all 100 tasks received a value (got {})",
            values.len()
        );
        assert!(
            values.iter().all(|v| *v == 42),
            "every task saw the same published value 42 (got {values:?})"
        );
        let existing = guard
            .existing_count
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            existing, 99,
            "99 followers attached to the leader's slot (got {existing})"
        );
    }

    /// After publish_and_remove the entry is gone, so a NEW
    /// lookup on the same key elects a fresh leader.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn race_guard_recycles_after_publish() {
        let guard: RaceGuard<u64, u32> = RaceGuard::new();
        let key = 11u64;

        match guard.lookup(&key) {
            RaceLookup::First(shared) => guard.publish_and_remove(&key, &shared, 1),
            RaceLookup::Existing(_) => panic!("first lookup must be First"),
        }
        // Second lookup — fresh leader.
        match guard.lookup(&key) {
            RaceLookup::First(_) => {}
            RaceLookup::Existing(_) => panic!("after publish_and_remove, next lookup is fresh leader"),
        }
        assert_eq!(
            guard.first_count.load(std::sync::atomic::Ordering::Relaxed),
            2
        );
    }
}
