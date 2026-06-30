# SPG concurrency invariants

Audit doc for the load-bearing race-protection contracts in the
engine. Updated whenever a new contract is added or an existing
one is verified by a test / fixed in a commit.

Each invariant lists:

- **What** — the contract in one sentence.
- **Why** — what breaks when it doesn't hold.
- **Where** — files / line ranges that enforce it.
- **How tested** — TDD test name or fault-injection scenario.

PG-equivalent rationale included where SPG's model differs from PG's
(e.g. when SPG dispenses with a PG mechanism because of a different
underlying invariant).

## 1. Checkpoint vs. write atomicity (Arc-snapshot model)

**What.** Every checkpoint captures a coherent prior-committed view
of the catalog **without** stopping concurrent writers.

**Why.** Without this, a checkpoint that interleaves with an
in-flight write would persist a torn snapshot (half-applied changes
visible to recovery). PG defends with `DELAY_CHKPT_START` /
`DELAY_CHKPT_COMPLETE` flags that critical sections set during
2PC / buffer flush so the checkpointer waits before recording the
"all committed up to" marker. SPG **does not need** that mechanism
because of a stronger underlying invariant — see Why-SPG below.

**Where.**
- `crates/spg-embedded/src/lib.rs::snapshot_checkpoint_job` (CoW-2,
  v7.34) — captures the snapshot via `engine.snapshot_data()`, an
  Arc-clone of the persistent catalog tries.
- `crates/spg-embedded/src/lib.rs::execute_checkpoint_job` — runs
  serialize + tmp+rename off the engine borrow on the checkpoint
  worker thread. Live writers continue to enqueue WAL records that
  ride the *next* checkpoint's marker.
- `crates/spg-engine/src/lib.rs::Engine::snapshot_data` — relies on
  the Persistent BTree's structural sharing: `Clone` is an O(1) Arc
  bump, never observes a partially-applied change.

**Why-SPG doesn't need DELAY_CHKPT_START.** SPG's catalog is
*immutable on the read path*: every committed mutation produces a
new Arc-shared catalog version, and the active engine state always
points at a fully-applied prior version. `snapshot_checkpoint_job`
bumps the Arc count of *that* version; in-flight writers are
constructing the *next* version (which the checkpoint will not
see). There's no "half-committed visible" window because the only
moment a writer's changes become visible is the atomic swap at
commit, which is mutually exclusive with the snapshot bump via the
engine's single-writer invariant (see §2).

PG needs `DELAY_CHKPT_START` because its commit path has multiple
phases (XLogInsert + ProcArray flip + buffer dirty), and a
checkpoint that observed an in-progress 2PC PREPARE / COMMIT could
record an inconsistent xact log boundary. SPG's per-version commit
swap collapses those phases into one atomic Arc replace; there's
no multi-phase window for the checkpointer to peek inside.

**How tested.**
- `crates/spg-embedded/src/lib.rs::tests::v7_37_13_checkpoint_stats_record_timing_and_percentiles`
  — checkpoints under concurrent writes (5 rounds of 10 INSERTs +
  explicit checkpoint each) without producing torn snapshots.
- `injection_point!("checkpoint_cow_swap_pre")` /
  `injection_point!("checkpoint_cow_swap_post")` — let tests inject
  a delay around the snapshot rename to simulate the worst-case
  race window. With the `injection-points` feature on, the test
  suite can attach `wait` to either point and assert recovery still
  loads a coherent snapshot.

## 2. Single-writer invariant

**What.** At any moment, exactly one thread holds the engine's
write lock (`tokio::sync::RwLock<Database>` in async wrappers,
`std::sync::Mutex<Engine>` equivalents in the sync engine).

**Why.** Catalog versioning relies on the writer producing a new
version atomically; two concurrent writers would race on the
"next version" pointer and could lose a committed change.

**Where.**
- `crates/spg-embedded-tokio/src/lib.rs::AsyncDatabase` — inner is
  `Arc<RwLock<Database>>`; `execute` takes `blocking_write`.
- `crates/spg-embedded/src/lib.rs::Database::execute_buffered` —
  `&mut self` enforces the invariant at the type level.

**How tested.**
- `crates/spg-embedded-tokio/tests/e2e/async_db.rs::concurrent_inserts_serialise`
  — N concurrent INSERT tasks against one AsyncDatabase; final row
  count equals N (no lost update).
- `crates/spg-embedded-tokio/tests/e2e/read_handle.rs::read_handle_does_not_block_writer`
  — concurrent snapshot reads don't observe a partially-applied
  commit.

## 3. WAL group-commit leader contract (v7.20 P2)

**What.** One leader thread fsyncs for every record currently in
the batch; followers park until their seq is covered. The leader
never holds the WAL `state` mutex during fsync (released between
`take_batch()` and `write_all + sync_data`).

**Why.** If the leader held `state` across fsync, all followers
would block waiting for the lock instead of waiting for the
durability boundary — collapsing the per-fsync benefit of group
commit. Worse, a panic mid-fsync without releasing `leader_active`
would deadlock every later writer.

**Where.**
- `crates/spg-embedded/src/lib.rs::WalGroup::wait_flushed` — lock
  acquire / drop / fsync / re-acquire sequence.
- `LeaderGuard` (v7.34) — panic-safe disarm; on unwind the guard
  Drop releases `leader_active` and notifies a follower to
  re-elect.

**How tested.**
- `crates/spg-embedded-tokio/tests/e2e/async_db.rs::concurrent_inserts_serialise`
  — multiple writers share one fsync; durability still per-record.
- `injection_point!("tx_commit_walgroup_leader_switch")` — lets a
  test pin the leader and assert followers don't busy-wait.

## 4. WAL fsync-failure policy (v7.37.13 A1.4 / A1.5)

**What.** A WAL `sync_data` failure either aborts the process
(default; PG-equivalent PANIC) or returns `EngineError` via the
WAL poison path (opt-in `SPG_DATA_SYNC_RETRY=on`).

**Why.** Continuing on a poisoned WAL hides corruption from the
application — every later commit claims a durability it does not
have. Aborting and letting the supervisor restart + replay
re-establishes a consistent state from the last good WAL boundary,
which is the only honest answer.

**Where.**
- `crates/spg-embedded/src/lib.rs::handle_wal_fsync_fail` — central
  policy gate.
- `crates/spg-embedded/src/lib.rs::data_sync_retry_on` — cached
  env lookup; the policy is a process-lifetime invariant, not a
  runtime tunable.

**How tested.**
- `tests::v7_37_13_fsync_policy_retry_and_default_abort` — both
  phases (retry path returns Err; default path panics in test cfg,
  aborts in release).

## 5. Self-wake checkpoint timer (v7.37.13 A1.1)

**What.** Even when the application is fully idle, the AsyncDatabase
background self-wake task invokes `maybe_trigger_checkpoint` every
`min(checkpoint_time_threshold / 2, 500 ms)` so the snapshot
advances without any caller-driven SQL.

**Why.** Mailrs cascade 8 (2026-06-24 prod report) observed 17 h
between base.spg mtime advances — the caller-side `wal_after_ok`
time trigger was bypassed by a 14 h idle window followed by a
30 KB/hr trickle that never crossed the byte threshold. Without
self-wake, any new commit path that forgets to call `wal_after_ok`
silently disables auto-checkpoint.

**Where.**
- `crates/spg-embedded-tokio/src/lib.rs::spawn_self_wake_checkpoint_task`
  — spawned at open_path; holds Weak<RwLock<Database>>; exits on
  last clone drop.
- `crates/spg-embedded/src/lib.rs::Database::maybe_trigger_checkpoint`
  — public façade so external schedulers can drive the trigger
  without owning `&mut`.

**How tested.**
- `crates/spg-embedded-tokio/tests/e2e/async_db.rs::v7_37_13_checkpoint_self_wakes_when_idle`
  — pure-idle window, no SQL, asserts `self_wake_fire_count()`
  advanced ≥ 2 ticks.

## Maintenance

Add new invariants below this section when introducing them. The
review checklist:

1. **Where** must point to actual file:line ranges, not hand-wavy
   "somewhere in spg-engine".
2. **How tested** must name a `#[test]` or an injection-point
   scenario. If a contract isn't tested, it's a hope, not an
   invariant — write the test first.
3. **Why-SPG doesn't need X** sections are encouraged when SPG
   intentionally dispenses with a PG mechanism. Force the author
   to articulate the *replacement* invariant rather than just
   asserting "we don't need that".
