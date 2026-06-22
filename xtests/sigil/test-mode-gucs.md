# Test-mode GUCs — v7.38 P0 元机制 D index

> Source of truth for every `SPG_TEST_*` env var the engine honours.
> Adding a new GUC must update this table in the same commit; CI lint
> rejects any `SPG_TEST_*` symbol that does not appear below.
>
> Design: `.claude/notes/v7.38-test-mode-gucs-design.md`.
> Framework: `crates/spg-engine/src/testkit/env_config.rs`.

## Index

| GUC | Surface pinned | Engine acceptor (file:line) | Unit pin | Status |
|---|---|---|---|---|
| `SPG_TEST_EXPLAIN_NO_COSTS=1` | EXPLAIN ANALYZE `elapsed=…us` annotation stripped | `crates/spg-engine/src/lib.rs:4970` (`exec_explain` Total line) | `crates/spg-engine/tests/e2e/e2e_env_cfg_explain_no_costs.rs` | LANDED |
| `SPG_TEST_DISABLE_TOPK=1` | Aggregate `ORDER BY … LIMIT` fast path (`partial_sort_tagged`) falls back to full sort | `crates/spg-engine/src/lib.rs:8249` + `:8686` (both `partial_sort_tagged` call sites) | `crates/spg-engine/tests/e2e/e2e_env_cfg_disable_topk.rs` | LANDED |
| `SPG_TEST_RANDOM_SEED=N` | Single engine RNG seed source (`Engine::rng_seed`) | `crates/spg-engine/src/lib.rs:1223` (`Engine::rng_seed`) | `crates/spg-engine/tests/e2e/e2e_env_cfg_random_seed.rs` | LANDED |
| `SPG_TEST_COMPUTE_QUERY_ID=regress` | Query-identifier annotation stripped from EXPLAIN output | TBD: `crates/spg-engine/src/lib.rs` (annotate_explain_lines) | TBD: `crates/spg-engine/tests/e2e/e2e_env_cfg_compute_query_id.rs` | TBD (P1) |
| `SPG_TEST_STATS_FROZEN=1` | `ANALYZE` becomes a no-op; INSERT/UPDATE auto-stats refresh suppressed | TBD: `crates/spg-engine/src/statistics.rs` (`update_statistic_for_table` entry) | TBD: `crates/spg-engine/tests/e2e/e2e_env_cfg_stats_frozen.rs` | TBD (P1) |
| `SPG_TEST_PLAN_DETERMINISTIC=1` | Cost-based plan / join-order decisions fall back to signature-hash lexical tie-break | TBD: `crates/spg-engine/src/plan_cache.rs` + `crates/spg-engine/src/reorder.rs` | TBD: `crates/spg-engine/tests/e2e/e2e_env_cfg_plan_deterministic.rs` | TBD (P1) |
| `SPG_TEST_DISABLE_JOINFOLD=1` | v7.37 joinfold rewrite suppressed; planner sees the raw join tree | TBD: `crates/spg-engine/src/lib.rs` (joinfold rewrite entry) | TBD: `crates/spg-engine/tests/e2e/e2e_env_cfg_disable_joinfold.rs` | TBD (P1) |

## Conventions

- **Acceptor**: the single read site (`engine.env_cfg().<field>`) that
  flips the production behaviour. If a GUC needs multiple read sites,
  list all of them on the same row, comma-separated. The first listed
  site is the canonical reference for `grep` audits.
- **Status**:
  - `LANDED` — acceptor wired + unit pin green.
  - `TBD` — slot reserved; needs an acceptor + a unit pin to flip to
    `LANDED`. TBD rows do not pass the day-5 acceptance bar but exist
    so the lint can already enforce "no orphan `SPG_TEST_*` symbols".
- **Adding a GUC**: edit `crates/spg-engine/src/testkit/env_config.rs`
  + add a unit pin under `crates/spg-engine/tests/e2e/` + insert a row
  here. All three land in the same commit.
- **Removing a GUC**: must have a v7.x replacement and a migration
  note; do not silently delete a row.

## Lint check

CI runs:

```sh
git grep -E 'SPG_TEST_[A-Z_]+' crates/ \
    | grep -v 'crates/spg-engine/src/testkit/env_config.rs' \
    | grep -v 'crates/spg-engine/tests/e2e/e2e_env_cfg_' \
    | awk -F'SPG_TEST_' '{print $2}' \
    | sed -E 's/[^A-Z_].*//' \
    | sort -u
```

Every printed symbol must appear in the table above. Any orphan fails
the build.

## Maintenance principles

1. Flaky test → first reflex is **"which GUC pins the nondeterminism?"**,
   not `#[ignore]` or retry. If no GUC exists, add one.
2. `EnvConfig` defaults must remain production-identical so a release
   build with no env vars set is bit-for-bit the historical engine.
3. Acceptors are `if env_cfg.<field>` gates on a `bool` / `Option<u64>`
   field load. They must not consult `std::env` on a hot path.
4. Every acceptor carries an inline comment `v7.38 元机制 D acceptor
   — <GUC_NAME>` so `git grep` lands on the implementation from this
   index.
