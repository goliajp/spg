# `xtests/oracle/` — v7.38 元机制 C 三主差分 oracle

> **Status:** v7.38 C **scaffolding shipped**. Framework / CLI /
> normalisation pipeline / R32 EXPECTED FAILURE anchor are in
> place; the two execution hooks (`run_on_spg`, `run_on_oracle`)
> are stubs that land during v7.38 P1 corpus fill.
>
> See `.claude/notes/v7.38-differential-oracle-design.md` for the
> full design and the P1 day-by-day fill plan.

## What this is

A test harness that takes one SQL fixture, runs it on **SPG**
*and* on a reference master (PG18, MySQL 8, MariaDB 11), and
asserts the two result sets are **byte-equal** after a fixed
normalisation pipeline. Any unexplained divergence = test fail.

This is the mechanism that lets us say "SPG is a drop-in" with a
straight face: every claim of compatibility lives or dies by a
side-by-side compare against the real engine the customer would
otherwise have used.

## Three buckets — `port.` / `orig.` / `depd.`

Naming convention is borrowed from YugabyteDB's `yb.port.*`
triplet (cleanly separates "ported from upstream" from "ours" from
"setup"). Every file in `sql/` **must** start with one of these
three prefixes; the runner refuses to dispatch otherwise.

| prefix      | meaning                                              | expected files                                  |
|-------------|------------------------------------------------------|-------------------------------------------------|
| `port.*`    | ported from PG `src/test/regress/sql/`               | `.pg.out` always; `.spg.out` if EXPECTED FAILURE |
| `orig.*`    | SPG-original test, no upstream PG analogue           | `.pg.out` (oracle baseline of record)           |
| `depd.*`    | setup-only fixture, depended-on by other fixtures    | none (never run standalone)                     |

## Running

### fast tier (no docker — 60s budget)

```bash
cargo run -p spg-oracle-runner --release -- self-diff
```

Runs the corpus on three SPG permutations (embedded /
server-simple / server-extended) and asserts byte-equal across
the three internal paths. Stands in for the docker path when CI's
fast-tier sub-budget can't absorb container startup.

### full tier (docker — three masters)

```bash
cargo run -p spg-oracle-runner --release -- docker up
cargo run -p spg-oracle-runner --release -- all
cargo run -p spg-oracle-runner --release -- docker down
```

Brings up PG18 / MySQL 8 / MariaDB 11 in docker-compose, runs the
full corpus against each, tears down. Typical wall time 5–30 min.

### single-master differential

```bash
cargo run -p spg-oracle-runner --release -- run --oracle pg18
```

Useful for triaging a PG-specific divergence without paying the
MySQL / MariaDB container costs.

## Adding a new oracle test

1. **Decide the bucket** — port / orig / depd.
2. **Write the SQL** at `sql/<bucket>.<name>.sql`. Single fixture
   per file; reference `depd.*` setup via `# oracle: depends
   depd.<name>` header directive.
3. **Capture the oracle baseline:**
   ```bash
   # P1+ — once --bless lands
   cargo run -p spg-oracle-runner -- docker up
   cargo run -p spg-oracle-runner -- run --oracle pg18 --bless
   ```
   Writes `expected/<bucket>.<name>.pg.out` from PG18's actual
   output. Repeat for `mysql` / `mariadb` if the test is portable.
4. **Run the differential:**
   ```bash
   cargo run -p spg-oracle-runner -- run --oracle pg18
   ```
   Green = SPG matches; red = either real bug (fix SPG) or known
   divergence (write `.spg.out` lock, link backlog).

## Promoting a v7.37 baseline test to an oracle test

The v7.37 SQL baseline corpus at
`xtests/sqllogictest/corpus/spg_baseline/` (94 .test files, 452
records) is a candidate starter set. To promote a fixture:

1. Read the `.test` file's `statement ok` / `query I rowsort`
   blocks. The sqllogictest format already encodes "result is
   sort-insensitive" via `rowsort`, which matches the oracle
   default normalisation.
2. Decide bucket — most `spg_baseline` fixtures are `orig.*`
   (SPG-original surface), a handful that mirror PG regress are
   `port.*`.
3. Convert to flat SQL at `xtests/oracle/sql/<bucket>.<name>.sql`.
4. `--bless` against PG18 to lock the baseline.

The two corpora live in parallel — sqllogictest stays as the
"can SPG execute this?" smoke; the oracle corpus is the "does
SPG match real PG on this?" gate.

## EXPECTED FAILURE workflow (R32 example)

Some divergences we know about, can reproduce, and have on the
backlog. Rather than block CI, we **lock** the current SPG
behaviour in `expected/<fixture>.spg.out` so any **drift** from
that known-bad output still fires.

Example: `port.subquery_correlated_agg.sql` (backlog R32 —
outer-agg correlated subquery dialect).

```
sql/port.subquery_correlated_agg.sql                 # PG-shape SQL
expected/port.subquery_correlated_agg.pg.out         # PG18 baseline
expected/port.subquery_correlated_agg.spg.out        # CURRENT SPG output (EXPECTED FAILURE)
```

Runner semantics:

- `.spg.out` exists → SPG must match `.spg.out` byte-equal.
- `.spg.out` exists **and SPG now matches `.pg.out`** → runner
  errors "EXPECTED FAILURE lock is no longer failing — delete".
- `.spg.out` does not exist → SPG must match `.pg.out` byte-equal.

Net effect: fix R32, SPG now matches PG18, CI fires demanding the
lock file be removed, you delete it, the test starts enforcing
the PG baseline directly. **Bug closure is observable in-tree.**

## Debugging a differential mismatch

The runner uses `similar::TextDiff` to display unified diffs on
fail (P1 fill). Quick triage workflow:

1. **Look at the bottom of the diff first.** The
   `AdjustOrderingViaSort` step sorts lines lexically as the last
   pipeline step; the smallest-string divergence (`(null)` vs
   `NULL`, leading-space vs not) bubbles to the top of the diff
   and is usually a missing `adjust_*()` rule, not a real
   semantic bug.
2. **Bisect the pipeline.** Pass `--explain-pipeline` (P1) to
   show which step absorbed (or introduced) the divergence. Each
   step is independently unit-tested in
   `src/normalise.rs::tests`.
3. **Re-bless if the change is intentional.**
   `run --oracle pg18 --bless` rewrites `expected/<f>.pg.out`
   from the oracle's current output. Don't bless SPG output as
   the oracle — that's the failure mode this whole harness
   exists to catch.

## Layout

```
xtests/oracle/
├── Cargo.toml                 # spg-oracle-runner bin crate
├── docker-compose.yml         # three masters on isolated network
├── pg18/                      # PG18 image config + initdb hooks
├── mysql/                     # MySQL 8 image config + initdb hooks
├── mariadb/                   # MariaDB 11 image config + initdb hooks
├── src/                       # runner source
│   ├── main.rs                # CLI orchestrator
│   ├── dialect.rs             # Oracle enum + DialectAdapter
│   ├── normalise.rs           # adjust_*() pipeline
│   ├── naming.rs              # port./orig./depd. dispatch
│   ├── docker.rs              # docker-compose up/down (stub)
│   ├── runner.rs              # per-fixture orchestrator (stub)
│   └── self_diff.rs           # fast-tier replacement (stub)
├── sql/                       # corpus
│   ├── depd.setup_users.sql
│   ├── port.select_implicit.sql
│   ├── port.subquery_correlated_agg.sql    # R32 anchor
│   └── orig.spg_jsonb_at_op.sql
├── expected/                  # captured baselines
│   ├── port.select_implicit.pg.out
│   ├── port.subquery_correlated_agg.pg.out
│   ├── port.subquery_correlated_agg.spg.out  # R32 EXPECTED FAILURE lock
│   └── orig.spg_jsonb_at_op.pg.out
└── corpus/                    # reserved for `.slt` sqllogictest-style
                               # fixtures during P1 promotion from
                               # xtests/sqllogictest/corpus/spg_baseline
```

## Constraints

- **Fast tier skips docker** by design (60s budget; three
  containers each take ~10s to come up). SPG-self differential
  via `self-diff` is the substitute; docker oracle only runs in
  full-tier (`gate.sh full --oracle`).
- **No hash comparison, no sampling.** Sort-then-byte-equal end
  to end. "50 rows × 50 rows, same length, looks OK" is not
  passing — see plan §三 工艺 #5.
- **Three masters or bust.** Skipping MySQL / MariaDB because
  "dropin is mostly about PG" is rejected — v7.38 ships all
  three together or none.
