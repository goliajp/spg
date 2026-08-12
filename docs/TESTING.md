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

**`--full` runs the ignored unit tests in release.** The everyday unit pass
stays unoptimised, which is right for a loop that runs constantly. But some
`#[ignore]`d tests are ignored *because they measure something* and say in
their own comments to run them with `--release`; handing those a debug build
fails them by construction. The first `--full` run this repo ever did died on
a 200 ns budget measured at 1913 ns — 48 ns when built the way the test asks
for. So `run_unit` adds a second pass, `cargo test --release … -- --ignored`,
and the ordinary pass is untouched.

A measurement that must survive a shared machine takes the **best of N**
passes rather than one: interference can only make a pass slower, so the
fastest is closest to what the code costs, and a real regression still slows
every pass. Where a budget is meaningless without optimisation, the test
prints its number under `debug_assertions` and declines to judge — the same
reasoning `#![cfg(not(debug_assertions))]` applies to a whole `perf_gate`
target, applied to one assertion that lives in a unit test.

### Opt-in tests

`--include-ignored` cannot tell "long-running" from "needs conditions this
machine cannot give". Two groups therefore opt in by environment variable
instead of relying on `#[ignore]` alone:

| Variable | Gates | Why it is not merely `#[ignore]` |
|---|---|---|
| `SPG_SOAK_TESTS=1` | the 100M-row restart, the 30M-row RSS ceiling, the SQ8 1M kNN/RSS gates, the 1M-row WAL throughput gate, the 1M-row cold-start gate | They need a machine to themselves. A 6 GiB RSS ceiling measured beside another project's compiler is measuring the machine; the 100M restart sat at 0% CPU for 94 minutes and took a whole `--full` run with it. |
| `SPG_CAPTURE_FIXTURES=1` | `capture_v4_41_fixture`, `capture_v5_2_fixture` | They **write into** `xtests/compat-fixtures/`, the corpus the cross-version gate replays. Regenerating those with the current binary turns "an old version's bytes still restore" into "this version restores its own output", and the originals cannot be recaptured — the binaries that wrote them are gone. |

Both print why they skipped, so a run that did not exercise them says so.

### Tests that need something outside the process

A suite that needs a live server, a DSN, or a container **skips and names
what is missing** — it does not panic. `gate.sh`'s perf category is the
model: it says what is unset, skips, and only fails when `PERF_REQUIRED=1`
marks a release run. `xtests/sqlx-pgwire`'s two suites follow it. A missing
DSN is a statement about the environment, not a failing test, and a run that
reports it as a failure hides the failures that matter.

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

Caveat: `biz` needs Docker — its harnesses run psql out of a container —
and the `diffcorpus` runner needs a live PG18 beside it (the
`spg-bench-postgres` container, PG on 25432). Both exist on the testbed, so
`biz` runs there; what it needs is for those containers to be **up**. After
a reboot they are not, and the failure mode is quiet: every leg errors
identically, the runner diffs stderr too, and twenty categories come back
`IDENTICAL` with the baseline's 31 differing lines reported as zero — a
result that reads like a sweeping improvement and whose documented next step
(`--rebaseline`) would overwrite the record and leave the gate unable to
fail again. `diffcorpus/run.sh` now asks each leg for a row before scoring
anything and refuses if either cannot answer.

## Acceptance-shape conventions (rounds 12-20 lessons)

Any new SQL-shape regression (a customer round, a dialect gap, a
dropin probe) follows three rules. Each exists because its absence
shipped a bug:

1. **Seeded.** Empty-table cases declared victory twice (rounds
   17/18 parse-level gates masked the rows-present eval bugs of
   round 19). Insert 2-3 rows that exercise the shape — including a
   NULL where ordering/aggregation semantics differ.
2. **Multi-path.** The engine has three dispatch families and they
   do not share pre-execution passes uniformly: direct
   `execute()`, prepared / readonly-on-snapshot (the sqlx inline
   path), and the wire server (pgwire). Round 16's correlated-EXISTS
   gap and round 18's CTE placeholders were path-local. Pin at
   least direct + prepared in the embed e2e; promote the shape to
   `scripts/dropin-acceptance.sh` (psql against the docker image =
   the server path) — see the "rounds 13-20" panel section there
   for the pattern, including `run_case_expect` (value-asserted,
   not rc-only) and `run_case_expect_tolerant` (for
   "rejection IS the assertion" cases).
3. **Typed.** Column TypeInfo is part of the behaviour: round 20's
   aggregate-columns-as-TEXT broke every sqlx decode while all
   VALUES were correct. When the shape produces aggregate or
   expression columns, decode into the real Rust tuple in a
   spg-sqlx test (`tests/mailrs_round20.rs` is the template).

## Release battery

Before any release ack, all of the following must be green (see
`.claude/git-flow.md` for the branch mechanics):

1. `scripts/gate.sh all` (lint + unit + e2e + gates + biz)
2. the mailrs zero-change validation (external customer fixture; lives
   outside this repo)
3. `scripts/dropin-acceptance.sh` against the candidate image (CI runs this
   as the `dropin_acceptance` job)

### Where the drop-in panel sits, and what that costs

Inside `scripts/release.sh` the panel runs **after** crates.io and docker.
It is a mirror held up to a published artefact, not a gate in front of one.
That is worth stating plainly because it has been paid for: `text || <REAL>`
regressed between 7.37.9 and 7.37.13, the panel's
`round20.aggregate_group_composite` case caught it exactly as designed, and
by then thirteen crates and three image tags were already public. The fix
shipped as 7.37.15.

So a shape that the panel covers is **not** thereby covered before a
release. When a panel case matters, put the same shape somewhere
`gate.sh all` reaches — the sqllogictest corpus (`15_regressions/`) is the
cheapest home, and it is where that concat shape now lives.

The panel reports two different totals depending on how it was invoked, and
neither is stale. `scripts/dropin-acceptance.sh` on its own runs its **57**
built-in cases and writes `./dropin-acceptance-report.md`; `release.sh` adds
`--fixture` twice (the mailrs pg-extensions and init-schema files) for
**59**, and directs the report to `scripts/dropin-acceptance-report-v<X.Y.Z>
.md`. Only the versioned one records a release. Grepping the script for
`^run_case` gives 60, three of which are the function definitions.

The panel asserts the **last line of stdout**. It used to read stdout and
stderr merged, which cannot be made reliable: a psql error is two lines
(`ERROR:` then `DETAIL:`), psql block-buffers stdout when it is not a tty
while stderr stays unbuffered, and so a case whose last row is the assertion
could read back a `DETAIL:` line instead. `round13.inline_pk_enforces` did
that on the 7.37.15 panel having passed on 7.37.9 and 7.37.14, with nothing
between them that could touch it. Filtering more prefixes would chase the
symptom; the rows come from stdout, so the assertion reads stdout and the
diagnostics are reported from stderr.
