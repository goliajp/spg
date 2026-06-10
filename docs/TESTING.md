# Testing

The test surface is split into five categories. `scripts/gate.sh` is the
single entry point for all of them; CI runs the same categories as separate
jobs.

| Category | What runs | Build | Entry point |
|---|---|---|---|
| **lint** | `cargo fmt --check`, `cargo clippy --all-targets -D warnings` | debug | `scripts/gate.sh lint` |
| **unit** | in-crate `#[test]` (`--lib --bins`) + doc tests | debug | `scripts/gate.sh unit` |
| **e2e** | every integration-test target (`--tests`): the merged per-crate `e2e` binaries, `group_commit`, etc. Perf/SLO targets compile empty here by design. | debug | `scripts/gate.sh e2e` |
| **gates** | release-mode budget gates: `perf_gate` × every crate that has one, `prod_ready`, `slo_smoke` | release | `scripts/gate.sh gates` |
| **biz** | customer-facing harnesses: `sqllogictest` conformance corpus, `xtests/dump_compat` (pg_dump-shape schemas), `xtests/data_compat` (data round-trip row counts) | release | `scripts/gate.sh biz` |

`scripts/gate.sh all` runs everything in the order above.

## fast vs full tiers

Timing-gated targets carry two tiers:

- **fast** (default) — every non-`#[ignore]` test. Hard budget/ratio
  assertions sized to catch real regressions without false-firing on host
  noise. This is what CI runs.
- **full** (`--full` → `--include-ignored`) — the `#[ignore]`'d long-running
  tests: the 1M-row cold-start gate (`SPG_PERF_1B_ROW_BUDGET` rows), the SQ8
  1M kNN/RSS gates (minutes; HNSW index build is the long pole), the SLO 1M
  throughput gates, and the exploratory benches (concurrency sweep,
  prepared-vs-simple, kNN stage timings).

The same convention applies anywhere a target has both a quick regression
probe and a long-running deep version: default = fast, `#[ignore]` = full.

## perf_gate convention

Every perf target is one `perf_gate` integration target per crate
(`tests/perf_gate.rs`, or `tests/perf_gate/main.rs` once a crate has more
than one module). Rules:

- `#![cfg(not(debug_assertions))]` on the target — timing under debug
  codegen is meaningless, so debug sweeps see an empty binary and only
  release runs exercise the budgets.
- Budgets live next to the measured number (and in the crate's `BUDGETS.md`
  where one exists), with ~10–1000× headroom over the measured floor.
- Each timed test takes the target-local `perf_lock()` (500 ms cool-down
  Mutex) so in-binary parallelism cannot skew numbers; harness runs add
  `--test-threads=1` on top.
- Run one crate at a time: a single failure then points at exactly which
  gate regressed (this is how the CI `perf_gate` job loops).

## Offloading to the testbed

`scripts/test-on-mini.sh <gate.sh args...>` rsyncs the working tree
(gitignore-filtered) to the LAN testbed (`mini.local`, override with
`SPG_MINI_HOST` / `SPG_MINI_DIR`) and runs `gate.sh` there. The remote keeps
its own `target/`, so repeat runs build incrementally. Use it to keep the
dev machine free while a full sweep runs:

```sh
scripts/test-on-mini.sh e2e          # functional sweep, off-box
scripts/test-on-mini.sh gates --full # long-running perf tiers
```

Caveat: `biz` needs Docker (its harnesses run psql from the postgres:15
image) and a `.git` dir for `git rev-parse` — neither exists on the
testbed mirror, so run biz locally.

## Release battery

Before any release ack, all of the following must be green (see
`.claude/git-flow.md` for the branch mechanics):

1. `scripts/gate.sh all` (lint + unit + e2e + gates + biz)
2. the mailrs zero-change validation (external customer fixture; lives
   outside this repo)
3. `scripts/dropin-acceptance.sh` against the candidate image (CI runs this
   as the `dropin_acceptance` job)
