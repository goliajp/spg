# spg-perm-runner — v7.38 element B

> Permutation matrix runner: **N config × 1 corpus**.
>
> One shared corpus (sqllogictest fixtures under `xtests/sqllogictest/corpus/`)
> is exercised against every permutation. A permutation = `(mode, env-vars)` —
> e.g. `embedded`, `server_simple` (pgwire simple-query), `topk_off`
> (`SPG_TEST_DISABLE_TOPK=1`). Each permutation runs in its **own forked
> child process** so engine-level statics (plan cache, catalog, tracing
> subscribers) are reset between permutations, and a panic in one
> permutation cannot poison the next.

## Quick reference

```
spg-perm-runner list                       # what permutations exist
spg-perm-runner verify                     # parse TOML, sanity-check, no execution
spg-perm-runner run --permutation NAME     # one permutation, full corpus
spg-perm-runner run --permutation NAME --sample   # one permutation, fast_tier_sample
spg-perm-runner all --fast                 # fast_tier perms x fast_tier_sample
spg-perm-runner all                        # all non-full_only perms x full corpus
spg-perm-runner all --full                 # everything (incl. full_only perms)
```

## TOML schema

`tests/permutations.toml` is the single source of truth. The runner uses a
hand-rolled parser for a strict TOML subset to avoid pulling `toml`/`serde`
onto the gate.sh fast path; what we accept here parses as standard TOML if
the parser is ever swapped back.

```toml
[default]
corpus_root        = "xtests/sqllogictest/corpus"
include_globs      = ["spg_baseline"]          # corpus subdir names
fast_tier          = ["embedded", "server_simple", "topk_off"]
fast_tier_sample   = "spg_baseline/01_basic_dml"
timeout_secs       = 30

[[permutation]]
name = "embedded"
mode = "embedded"                              # | "server"
env  = { }
# Optional:
# full_only = true                             # only run with --full
```

## Adding a permutation

1. Pick a unique `name`.
2. Decide `mode = "embedded"` (in-process Engine) vs `mode = "server"`
   (pgwire — pending v7.38 day-6 ServerRunner work).
3. Set `env` to the GUC shim env-vars that engine reads at startup.
   For example, the v7.38 element-D `SPG_TEST_DISABLE_TOPK=1` will,
   once that GUC lands, take the agg LIMIT shortcut off.
4. Run `cargo run -p spg-perm-runner -- verify` — it'll fail if the
   permutation's name collides or fast_tier references it but it doesn't
   exist.
5. Run `cargo run -p spg-perm-runner -- run --permutation YOUR_NAME --sample`
   to exercise it against the fast_tier_sample subset.

## Dev cycle — quick iteration

```sh
# Edit src/permutation.rs / src/runner.rs / etc.
cargo check -p spg-perm-runner             # ~3s
cargo test -p spg-perm-runner --release    # parser unit tests
./target/release/spg-perm-runner all --fast   # full skeleton smoke
```

`cargo run -p spg-perm-runner -- verify` is `<1s` after warm build and is
the right safety check in commit-loop scripts.

## Debugging one permutation

```sh
# Standalone child invocation; reproduces exactly what `all` would spawn.
SPG_TEST_DISABLE_TOPK=1 cargo run --release -p spg-perm-runner -- \
    run --permutation topk_off
```

The child writes its per-permutation report to
`target/perm-runner/<name>.json`. The parent's `all` aggregates totals
from those files into a three-column summary on stdout.

## Process isolation

```
parent (spg-perm-runner all)
  for each permutation P:
    Command::new(self_exe)
      .arg("run").arg("--permutation").arg(P.name)
      .env(P.env_vars...)
      .status()
    -- waits, captures exit code, peeks per-perm JSON for totals
```

Why per-permutation `Command::status()`?

- Engine statics (plan_cache / catalog / tracing subscriber / tokio
  runtime) are awkward to reset mid-process across permutations.
- Panic in permutation N cannot poison permutation N+1.
- Real env-var switching semantics — `SPG_TEST_DISABLE_TOPK` is read at
  Engine construction time; mid-process env mutation would race with
  already-cached decisions.
- Failure isolation — if one permutation hangs we can SIGTERM the
  child and parent continues. (Hang handling is a v7.38 follow-up; the
  skeleton just blocks on `status()`.)

## Acceptance status (v7.38 P0 day 5-7 checklist)

- [x] `xtests/perm-runner/` cargo bin compiles
- [x] `tests/permutations.toml` ships 5 core + 2 full-only permutations
- [x] `spg-perm-runner verify` returns OK in <1s
- [x] `spg-perm-runner list` prints a permutation table
- [x] `spg-perm-runner run --permutation embedded` walks the corpus
- [x] `spg-perm-runner all --fast` <90s on dev box
      (current: ~4s end-to-end, well under fast-tier budget)
- [x] `gate.sh e2e` invokes perm-runner after the regular `--tests` sweep
- [x] `gate.sh e2e --full` invokes `all --full`
- [x] `embedded` and `topk_off` byte-equal on the fast_tier_sample corpus
      (SPG-self differential — verified after masking `duration_ms` and
       `permutation` name fields)
- [ ] **Pending day 6**: `ServerRunner` for `server_simple` /
      `server_extended` — currently returns `SkippedPending` with
      a non-failing exit so the matrix shape is correct now and a
      follow-up commit can fill it in without touching the CLI.
- [ ] **Pending element A**: `SPG_TEST_DISABLE_TOPK` /
      `SPG_TEST_DISABLE_JOINFOLD` GUC shims must land on the engine
      side before those permutations actually *exercise* the disabled
      path; until then they are byte-equal smoke (which is itself a
      useful invariant — "env-var present is harmless if engine doesn't
      read it").

## Follow-up work (v7.38 P1+)

- `ServerRunner` — spawn in-proc `spg-server`, dial via psql / sqlx
- `--bail` flag — stop at first failed fixture
- Per-fixture timeout enforcement via `Command::kill_on_drop` + thread
- Reading `# permutation_skip:` / `# permutation_only:` directives off
  individual `.test` files (matches DuckDB's `# LogicTest:` convention)
- Differential oracle integration (element C) — declare per-permutation
  oracle (pg18 / mysql / mariadb) in the TOML
- Sanitizer permutation (ASan / TSan) wired via `build_flags` field
