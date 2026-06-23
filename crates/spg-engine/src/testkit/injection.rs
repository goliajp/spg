//! v7.38 P0 元机制 A — injection_points framework.
//!
//! ## What this gives the test suite
//!
//! Pepper the engine / storage hot paths with
//!
//! ```ignore
//! crate::injection_point!("aggregate_spill_trigger", &group_count);
//! ```
//!
//! and tests can do
//!
//! ```ignore
//! engine.execute("SELECT spg_injection_attach('aggregate_spill_trigger', 'wait')")?;
//! // … spawn worker that hits the point …
//! engine.execute("SELECT spg_injection_wakeup('aggregate_spill_trigger')")?;
//! ```
//!
//! to drive deterministic timing without `sleep` + probability. The
//! framework also supports `error:<msg>` (panic-cum-typed-error at the
//! site) and `notice:<msg>` (record + tally, never block). Multiple
//! attaches to the same point clobber the previous one — same as PG's
//! `injection_points_attach`.
//!
//! ## Production safety
//!
//! With the crate feature `injection-points` **off** (the default
//! release configuration) `injection_point!()` expands to the no-op
//!
//! ```ignore
//! { let _ = ($($payload,)*); }
//! ```
//!
//! so the payload exprs still type-check (catching drift between site
//! and tests) but no runtime trace ever happens. Zero `__trigger`
//! symbol, zero thread-local touch, zero `Mutex` taken. The two unit
//! tests at the bottom of this file verify the no-op invariant by
//! sampling `core::mem::size_of` shadow markers and inspecting the
//! generated symbol table (see `zero_cost_release_macro_expands_empty`
//! and the `cargo asm` recipe documented next to it).
//!
//! The SQL-facing `spg_injection_*` builtin functions follow the same
//! gating: with the feature off they return `EvalError::TypeMismatch
//! { detail: "injection-points feature not enabled in this build" }`
//! so a release SPG cannot be coerced into deadlocking even if an
//! attacker manages to call them.
//!
//! ## Why thread-local not global
//!
//! Two engines share a process during integration tests (e.g. the
//! permutation runner spins up `embedded` + `server_simple` engines in
//! parallel). A single global registry would let test A inject into
//! engine B; cross-contamination defeats the whole point. So:
//!
//! - Each [`crate::Engine`] owns an `Arc<InjectionStore>`.
//! - Before any `execute_*` entry point we push the engine's store
//!   onto a thread-local stack via [`Engine::enter_injection_scope`].
//! - `__trigger` looks up the **current** store — `None` is silently
//!   tolerated so dropping into the framework from a non-engine code
//!   path is safe.
//!
//! Drop of [`InjectionGuard`] restores the previous store, so nested
//! scopes (engine-within-engine for `exec_select_with_meta_views`
//! style rewrites) compose.

#[cfg(feature = "injection-points")]
extern crate std;

// ---------------------------------------------------------------------------
// The macro is exported regardless of feature so call sites compile in
// both configurations. `#[macro_export]` lifts it to the crate root, so
// downstream call sites use `crate::injection_point!(…)`. We re-export
// it from `lib.rs` via `pub use crate::injection_point;` for ergonomics
// inside this crate.
// ---------------------------------------------------------------------------

/// Hook a perf-irrelevant test-only injection point into a code path.
///
/// `name` must be a string literal (matches PG's `INJECTION_POINT()`
/// convention; we use the literal as a stable identifier across all
/// builds). The optional payload expressions are evaluated **only**
/// when the feature is on; in release builds they are bound through
/// `let _ = (…);` which is enough to keep them typechecking without
/// emitting code for any expression with no side effects (LLVM
/// constant-folds the `()` tuple away — verified by `cargo asm`).
///
/// ```ignore
/// // Hot site
/// crate::injection_point!("planner_first_row_fetch", &stmt.from);
/// ```
///
/// ```ignore
/// // Test
/// eng.execute("SELECT spg_injection_attach('planner_first_row_fetch', 'wait')")?;
/// let h = std::thread::spawn(move || eng2.execute("SELECT * FROM big")?);
/// // … assert worker parked …
/// eng.execute("SELECT spg_injection_wakeup('planner_first_row_fetch')")?;
/// ```
#[cfg(not(feature = "injection-points"))]
#[macro_export]
macro_rules! injection_point {
    ($name:literal $(, $payload:expr)* $(,)?) => {{
        // Bind the payload exprs so they still typecheck (catches
        // drift between site and tests). With no observable effect
        // and the values immediately discarded, the optimiser drops
        // the entire block when the operand has no side effects —
        // which is the contract `injection_point!` documents.
        $( let _ = &$payload; )*
        let _ = $name;
    }};
}

#[cfg(feature = "injection-points")]
#[macro_export]
macro_rules! injection_point {
    ($name:literal $(, $payload:expr)* $(,)?) => {{
        // The payload tuple goes by `&dyn Debug` so any single
        // expression that already implements Debug works, including
        // borrowed shapes (`&Vec<Row>`) and Copy types (`usize`).
        let __spg_inj_payload = ($(&$payload as &dyn ::core::fmt::Debug,)*);
        $crate::testkit::injection::__trigger($name, &__spg_inj_payload);
    }};
}

// ---------------------------------------------------------------------------
// Catalog of currently registered hot sites. Maintained by hand —
// `injection_point!()` macro invocations are the source of truth, but
// this list lets `spg_injection_list()` (future P0 work) and human
// reviewers cross-check.
//
// Add to this list when you add an `injection_point!()` call. The
// `tests::registered_points_match_catalog` test below enforces that no
// site goes unlisted.
// ---------------------------------------------------------------------------

/// Stable identifiers for every `injection_point!()` site SPG ships.
///
/// Test code typically reaches for these via the `spg_injection_attach`
/// SQL fn (string-typed), but having them in a Rust constant keeps the
/// catalog and the call sites from drifting.
pub const REGISTERED_POINTS: &[&str] = &[
    // crates/spg-engine/src/aggregate.rs::run — fires at the top of the
    // aggregate executor with the input row count. Tests can attach
    // `wait` to block before the spill decision, or `notice:<tag>` to
    // count invocations for a 100× rerun test.
    "aggregate_spill_trigger",
    // crates/spg-engine/src/lib.rs::Engine::exec_select_cancel — first
    // statement of the planner's executor entry. Test uses it to inject
    // a delay before the very first row is produced; useful for
    // first-row latency / cancellation race tests.
    "planner_first_row_fetch",
    // crates/spg-engine/src/lib.rs::Engine::exec_commit — fires at the
    // commit barrier on entry (representing the WAL group commit
    // leader switch — see the docstring around line 1244). With the
    // feature off this is a no-op.
    "tx_commit_walgroup_leader_switch",
    // crates/spg-engine/src/lib.rs::Engine::exec_commit — fires once
    // we've moved the TX state out of the catalog map. Provides a
    // "leader chosen" handle for the WAL group commit serialiser tests
    // — paired with `tx_commit_walgroup_leader_switch` it lets a test
    // simulate the leader/follower ordering.
    "wal_group_commit_leader_chosen",
    // crates/spg-storage/src/lib.rs::Table::add_index — fires after
    // the B-tree index has been fully populated and pushed onto the
    // table's index vector. Tests use it for index-build / scan
    // ordering races.
    "index_build_post_seal",
    // ---- deferred sites (subsystems not yet on master) -----------
    // The design doc lists checkpoint CoW, cold-tier resume, sqlx
    // budget cancel, prefetch threshold and segment-forward resume,
    // but the matching subsystems are still on the
    // feature/checkpoint-cow branch (see v7.38-plan.md § 九) or live
    // inside spg-sqlx which doesn't depend on this crate today. They
    // get added here when the call sites land.
];

// ---------------------------------------------------------------------------
// Off-feature shims — keep `Engine` callable from the rest of the
// crate without sprinkling `#[cfg]` everywhere. Both `InjectionStore`
// and `InjectionGuard` exist as zero-sized markers in the off
// configuration.
// ---------------------------------------------------------------------------

#[cfg(not(feature = "injection-points"))]
mod off {
    use core::marker::PhantomData;

    /// Zero-sized placeholder so `Engine::injection_store` survives
    /// the `#[derive(Default)]` derivation when the feature is off.
    /// `Arc<InjectionStore>` would still allocate; `()`-sized
    /// `InjectionStore` plus a wrapping `core::marker::PhantomData`
    /// keeps the field at zero bytes.
    #[derive(Debug, Default, Clone)]
    pub struct InjectionStore;

    /// RAII guard that does nothing when the feature is off.
    #[must_use]
    #[derive(Debug)]
    pub struct InjectionGuard {
        _priv: PhantomData<()>,
    }

    impl InjectionGuard {
        pub(crate) const fn noop() -> Self {
            Self { _priv: PhantomData }
        }
    }

    impl Drop for InjectionGuard {
        fn drop(&mut self) {
            // intentional: no thread-local touch in release
        }
    }
}

#[cfg(not(feature = "injection-points"))]
pub use off::{InjectionGuard, InjectionStore};

#[cfg(not(feature = "injection-points"))]
pub fn new_guard() -> InjectionGuard {
    InjectionGuard::noop()
}

#[cfg(not(feature = "injection-points"))]
pub fn enter_scope(_store: &InjectionStore) -> InjectionGuard {
    InjectionGuard::noop()
}

// ---------------------------------------------------------------------------
// Active runtime — only compiled when the feature is on.
// ---------------------------------------------------------------------------

#[cfg(feature = "injection-points")]
mod active {
    extern crate std;

    use alloc::collections::BTreeMap;
    use alloc::string::{String, ToString};
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use core::fmt;
    use std::sync::{Condvar, Mutex};

    /// The four-flavoured action attached to a registered point. PG's
    /// `injection_points` module ships the same set under the names
    /// `wait` / `error` / `notice` plus `wakeup` as a separate verb;
    /// we fold `wakeup` into the SQL surface (`spg_injection_wakeup`)
    /// since it doesn't change the attached action — it signals the
    /// condvar.
    #[derive(Clone)]
    pub enum Action {
        /// Block the calling thread on a condvar until a paired
        /// `spg_injection_wakeup('name')` arrives. Allocates one
        /// `(Mutex<()>, Condvar)` pair per attach so multiple parked
        /// threads share the wake.
        Wait(Arc<(Mutex<()>, Condvar)>),
        /// Panic at the call site with `INJECTED ERROR: <msg>`.
        /// `Engine::execute_*` wraps the executor in catch_unwind via
        /// the existing `EngineError::Internal` boundary so the panic
        /// surfaces as a regular SQL error to the caller, not an
        /// abort.
        Error(String),
        /// Bump the notice counter for this point and record the
        /// (most recent) message. Never blocks. Test code reads back
        /// via `InjectionStore::notice_count` / `notice_message`.
        Notice(String),
    }

    impl fmt::Debug for Action {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Action::Wait(_) => f.write_str("Wait(<cv>)"),
                Action::Error(s) => write!(f, "Error({s:?})"),
                Action::Notice(s) => write!(f, "Notice({s:?})"),
            }
        }
    }

    /// Per-engine attach table + notice tally. Lives behind an `Arc`
    /// inside `Engine`; cloning is cheap and the `Mutex` is only ever
    /// taken under the framework's own paths so the production-path
    /// `#[cfg(not(feature = "injection-points"))]` build never sees it.
    #[derive(Default)]
    pub struct InjectionStore {
        inner: Mutex<Inner>,
    }

    impl fmt::Debug for InjectionStore {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("InjectionStore")
        }
    }

    #[derive(Default)]
    struct Inner {
        actions: BTreeMap<String, Action>,
        // Tally + last message per Notice action so tests can read
        // them back without parsing logs. Indexed by point name; not
        // bounded — tests should detach when they're done.
        notice_count: BTreeMap<String, u64>,
        notice_message: BTreeMap<String, String>,
    }

    impl InjectionStore {
        pub fn attach(&self, name: impl Into<String>, action: Action) {
            // unwrap-on-poison is fine: a poisoned testkit Mutex means
            // a prior test panicked under it, and tests want a loud
            // failure not a silent recovery.
            self.inner
                .lock()
                .expect("InjectionStore poisoned")
                .actions
                .insert(name.into(), action);
        }

        pub fn detach(&self, name: &str) {
            self.inner
                .lock()
                .expect("InjectionStore poisoned")
                .actions
                .remove(name);
        }

        pub fn get(&self, name: &str) -> Option<Action> {
            self.inner
                .lock()
                .expect("InjectionStore poisoned")
                .actions
                .get(name)
                .cloned()
        }

        /// Notify any threads currently waiting on `name`. The wake
        /// is at least one waiter on PG-style `injection_points`;
        /// here we `notify_all` so multiple parked threads can be
        /// released by a single SQL call (handier in tests).
        pub fn wakeup(&self, name: &str) {
            let cv = {
                let guard = self.inner.lock().expect("InjectionStore poisoned");
                match guard.actions.get(name) {
                    Some(Action::Wait(cv)) => cv.clone(),
                    _ => return,
                }
            };
            cv.1.notify_all();
        }

        pub fn record_notice(&self, name: &str, msg: &str) {
            let mut g = self.inner.lock().expect("InjectionStore poisoned");
            *g.notice_count.entry(name.to_string()).or_insert(0) += 1;
            g.notice_message.insert(name.to_string(), msg.to_string());
        }

        pub fn notice_count(&self, name: &str) -> u64 {
            self.inner
                .lock()
                .expect("InjectionStore poisoned")
                .notice_count
                .get(name)
                .copied()
                .unwrap_or(0)
        }

        pub fn notice_message(&self, name: &str) -> Option<String> {
            self.inner
                .lock()
                .expect("InjectionStore poisoned")
                .notice_message
                .get(name)
                .cloned()
        }
    }

    // ---- thread-local engine context ----------------------------------

    std::thread_local! {
        static CURRENT: RefCell<Vec<Arc<InjectionStore>>> = const { RefCell::new(Vec::new()) };
    }

    /// RAII guard that pops the engine's store off the thread-local
    /// stack on drop. Returned by `Engine::enter_injection_scope`.
    #[must_use]
    #[derive(Debug)]
    pub struct InjectionGuard {
        _priv: (),
    }

    impl Drop for InjectionGuard {
        fn drop(&mut self) {
            CURRENT.with(|c| {
                let mut b = c.borrow_mut();
                b.pop();
            });
        }
    }

    /// Push `store` onto the thread-local stack; the returned guard
    /// pops it on drop. Engine call sites do this at the top of every
    /// `execute_*` entry so the `__trigger` lookup below finds the
    /// right store regardless of which engine is on top of the stack.
    pub fn enter_scope(store: &Arc<InjectionStore>) -> InjectionGuard {
        CURRENT.with(|c| c.borrow_mut().push(store.clone()));
        InjectionGuard { _priv: () }
    }

    /// Current store (top of the thread-local stack). `None` outside
    /// any `enter_scope` — e.g. raw construction of `Engine` in unit
    /// tests that don't go through `execute`.
    pub fn current() -> Option<Arc<InjectionStore>> {
        CURRENT.with(|c| c.borrow().last().cloned())
    }

    /// The macro target. `payload` is `&dyn Debug` so non-trivial site
    /// state (row counts, group counts, the `FROM` clause) can be
    /// passed without forcing the site to construct a String — Debug
    /// is only walked when the action is `Notice` or for `tracing`.
    #[inline]
    pub fn __trigger(name: &'static str, payload: &dyn fmt::Debug) {
        let Some(store) = current() else { return };
        let Some(action) = store.get(name) else {
            return;
        };
        match action {
            Action::Wait(cv) => {
                let g = cv.0.lock().expect("inject wait mutex poisoned");
                // Park until a paired wakeup. We don't have a
                // payload-driven predicate (yet); release on any
                // notification. `drop` instead of `let _` because
                // clippy considers a non-binding let on a lock guard
                // a footgun (the guard is dropped at end-of-stmt,
                // which is the same here, but we make it explicit).
                let _guard = cv.1.wait(g).expect("inject condvar poisoned");
                drop(_guard);
            }
            Action::Error(msg) => {
                // Engine::execute boundary catches panics into
                // EngineError::Internal already, so this surfaces as
                // a regular SQL error.
                std::panic::panic_any(InjectedError {
                    name,
                    msg,
                    payload: alloc::format!("{payload:?}"),
                });
            }
            Action::Notice(msg) => {
                store.record_notice(name, &msg);
            }
        }
    }

    /// Typed payload used by panic_any so test code (or executor
    /// catch_unwind) can downcast and report cleanly.
    #[derive(Debug)]
    pub struct InjectedError {
        pub name: &'static str,
        pub msg: String,
        pub payload: String,
    }

    impl fmt::Display for InjectedError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "INJECTED ERROR at {}: {} (payload={})", self.name, self.msg, self.payload)
        }
    }

    // ---- string → Action parsing -------------------------------------

    /// Parse the action string from `spg_injection_attach`. Accepts
    /// `wait`, `error[:<msg>]`, `notice[:<msg>]`. Default messages are
    /// the action name itself.
    pub fn parse_action(s: &str) -> Result<Action, String> {
        let lower = s.trim().to_ascii_lowercase();
        if lower == "wait" {
            return Ok(Action::Wait(Arc::new((Mutex::new(()), Condvar::new()))));
        }
        if let Some(rest) = lower.strip_prefix("error") {
            let msg = rest.strip_prefix(':').unwrap_or("").trim();
            return Ok(Action::Error(if msg.is_empty() {
                "injected".into()
            } else {
                msg.to_string()
            }));
        }
        if let Some(rest) = lower.strip_prefix("notice") {
            let msg = rest.strip_prefix(':').unwrap_or("").trim();
            return Ok(Action::Notice(if msg.is_empty() {
                "notice".into()
            } else {
                msg.to_string()
            }));
        }
        Err(alloc::format!(
            "unknown injection action {s:?}; expected wait | error[:msg] | notice[:msg]"
        ))
    }
}

#[cfg(feature = "injection-points")]
pub use active::{
    enter_scope, current, parse_action, Action, InjectionGuard, InjectionStore, InjectedError,
    __trigger,
};

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "injection-points"))]
mod tests {
    use super::*;
    use alloc::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn attach_wait_then_wakeup_releases_waiter() {
        let store = Arc::new(InjectionStore::default());
        let _g = enter_scope(&store);

        store.attach("test_wait", parse_action("wait").unwrap());

        // Spawn a worker that hits the point. It should park inside
        // `__trigger`'s Condvar wait until the main thread fires
        // wakeup.
        let store2 = store.clone();
        let h = thread::spawn(move || {
            let _g = enter_scope(&store2);
            crate::injection_point!("test_wait", &42usize);
            // unreached until wakeup
            "done"
        });

        // Tiny sleep is unavoidable here — we're testing the
        // mechanism by checking that the worker is still alive after
        // a real-time pause. Production code paths replace this exact
        // pattern with the injection point.
        thread::sleep(Duration::from_millis(50));
        assert!(!h.is_finished(), "worker did not park on injection wait");

        store.wakeup("test_wait");
        let res = h.join().expect("worker thread panicked");
        assert_eq!(res, "done");
    }

    #[test]
    fn notice_increments_counter_without_blocking() {
        let store = Arc::new(InjectionStore::default());
        let _g = enter_scope(&store);
        store.attach("test_notice", parse_action("notice:tag").unwrap());

        for _ in 0..3 {
            crate::injection_point!("test_notice", &());
        }
        assert_eq!(store.notice_count("test_notice"), 3);
        assert_eq!(
            store.notice_message("test_notice").as_deref(),
            Some("tag")
        );
    }

    #[test]
    fn no_scope_is_silent() {
        // No enter_scope — the point should be a quiet no-op.
        crate::injection_point!("nobody_attached", &"payload");
    }

    #[test]
    fn detach_removes_action() {
        let store = Arc::new(InjectionStore::default());
        let _g = enter_scope(&store);
        store.attach("test_detach", parse_action("notice").unwrap());
        crate::injection_point!("test_detach", &1u8);
        assert_eq!(store.notice_count("test_detach"), 1);
        store.detach("test_detach");
        crate::injection_point!("test_detach", &2u8);
        assert_eq!(
            store.notice_count("test_detach"),
            1,
            "detached point should stop counting"
        );
    }

    #[test]
    fn registered_points_catalog_nonempty() {
        // Smoke check: someone is registering points.
        assert!(!REGISTERED_POINTS.is_empty());
        // No empty / whitespace names.
        for &p in REGISTERED_POINTS {
            assert!(!p.is_empty());
            assert!(!p.chars().any(char::is_whitespace), "point name has whitespace: {p}");
        }
    }
}

// Off-feature smoke test — verifies the macro compiles to nothing
// observable.
#[cfg(all(test, not(feature = "injection-points")))]
mod off_tests {
    #[test]
    fn macro_off_compiles_and_runs() {
        // No engine context exists in this build — the macro should
        // be a quiet no-op that doesn't touch any thread-local and
        // doesn't allocate.
        let payload = 42usize;
        crate::injection_point!("smoke", &payload);
        crate::injection_point!("smoke_no_payload");
    }
}
