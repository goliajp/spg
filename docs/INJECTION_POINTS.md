# SPG Injection Points

> v7.37.27 (27.10) — public framework documentation for SPG's
> deterministic-timing test harness. Modelled on PostgreSQL's
> `injection_points` extension (`src/test/modules/injection_points`).

## What this is

Tests that exercise concurrent or timing-sensitive code paths
(WAL group commit, planner first-row fetch, aggregate spill,
checkpoint write) have historically relied on `sleep`,
probabilistic loops, or polling — each fragile and slow. Injection
points replace that with **deterministic hooks**: name a point in
the code, attach a behavior from a test, drive the worker into it,
and assert on the outcome.

The framework is **zero-cost in release builds**. With the
`injection-points` crate feature off (the default), every
`injection_point!()` macro call expands to a no-op that the
optimiser fully elides — verified by `cargo asm` against the
target symbol table. With the feature on (test builds), the macro
threads a typed payload through a thread-local store and triggers
the attached action.

## Behaviors

Each attach binds a behavior string to a named point.

| Behavior            | What happens when the point fires             |
|---------------------|-----------------------------------------------|
| `wait`              | Park the calling thread on a per-point latch. Test wakes it via `spg_injection_wakeup`. Composes with concurrent test workers — multiple threads can park; one wakeup releases each in attach order. |
| `error[:msg]`       | Panic-cum-typed-`EngineError` at the site. Tests assert on the message contents to verify the right path was reached. |
| `notice[:msg]`      | Record-and-tally (never block). Tests count hits via `spg_injection_count`. Use for low-cost coverage assertions. |

Multiple attaches to the same point clobber the previous one
(same as PG's `injection_points_attach`).

## SQL surface

The SPG-side API is a small set of scalar functions:

```sql no-run
-- Attach a behavior to a point. NULL behavior detaches.
SELECT spg_injection_attach('aggregate_spill_trigger', 'wait');
SELECT spg_injection_attach('planner_first_row_fetch', 'notice:select_seen');
SELECT spg_injection_attach('wal_group_commit_leader_chosen', 'error:fault_injection');

-- Wake every parked thread at a point.
SELECT spg_injection_wakeup('aggregate_spill_trigger');

-- Detach (equivalent to attach with NULL).
SELECT spg_injection_detach('aggregate_spill_trigger');
```

With the `injection-points` feature off, every function returns
`EvalError::TypeMismatch { detail: "injection-points feature not
enabled in this build" }` so a production SPG cannot be coerced
into deadlocking even if an attacker invokes them.

## Authoring a new injection point

Two steps.

### 1. Mark the call site

```rust
crate::injection_point!("my_named_point", &payload_expr_1, &payload_expr_2);
```

The first argument **must be a string literal**. The remaining
arguments are debug-formatted into the trace record. In release
builds the payload exprs are bound through `let _ = …` so they
still type-check (catching drift between site and tests) but emit
no runtime code.

### 2. Hit the point in a test

```rust
// Park the worker.
eng.execute(
    "SELECT spg_injection_attach('my_named_point', 'wait')"
).unwrap();

let eng2 = eng.clone();
let handle = std::thread::spawn(move || {
    eng2.execute("SELECT trigger_hot_path()").unwrap();
});

// Assert the worker parked.
testkit::injection::assert_parked("my_named_point");

// Release.
eng.execute(
    "SELECT spg_injection_wakeup('my_named_point')"
).unwrap();
handle.join().unwrap();
```

## Why thread-local, not global

Integration tests routinely spin up multiple engines in the same
process (`embedded` + `server_simple` side-by-side under the
permutation runner). A single global registry would let
behaviors leak between engines and silently corrupt test
expectations.

So each `Engine` owns an `Arc<InjectionStore>` and the macro
looks up the **current** store via a thread-local stack pushed
on entry to every `execute_*` path. Nested scopes (engine-within-
engine for meta-view rewrites) compose because dropping the
`InjectionGuard` restores the previous store.

## Currently registered hot sites

Maintained by hand from the macro invocations across the engine:

| Point name                              | Source file                              |
|-----------------------------------------|------------------------------------------|
| `tx_commit_walgroup_leader_switch`      | `crates/spg-engine/src/transaction.rs`   |
| `wal_group_commit_leader_chosen`        | `crates/spg-engine/src/transaction.rs`   |
| `planner_first_row_fetch`               | `crates/spg-engine/src/select.rs`        |
| `aggregate_spill_trigger`               | `crates/spg-engine/src/aggregate.rs`     |

Add new entries when introducing a new `injection_point!()` call.

## Wire-protocol-level injection

For tests that need to drive pgwire-level behaviour (extended
protocol race conditions, prepared-statement cache invalidation
timing), `spg_injection_attach` can target points inside the
server crate too. The same SQL surface; the only difference is
which engine's `InjectionStore` resolves the lookup.

## Performance contract

The macro's zero-cost contract is verified two ways:

1. `crate::testkit::injection::zero_cost_release_macro_expands_empty`
   uses `core::mem::size_of` shadow markers to assert no
   thread-local touch / Mutex acquisition / store lookup happens
   in the off-feature expansion.

2. The CI release-mode disassembly job
   (`scripts/check-injection-points-zero-cost.sh` — invoked from
   gate.sh) re-runs `cargo asm` on the symbol set and fails the
   build if any `__trigger` reference leaks into the release
   binary.

## Reference

- Source: `crates/spg-engine/src/testkit/injection.rs`
- Tests: `crates/spg-engine/src/tests.rs::injection_attach_*` family
- PG analog: `src/test/modules/injection_points` in PostgreSQL
- See also: [TESTING.md](./TESTING.md) for the five-category test
  taxonomy that injection-points fits into.
