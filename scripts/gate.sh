#!/usr/bin/env bash
# gate.sh — unified test-category runner.
#
# The test surface is split into five categories (see docs/TESTING.md):
#
#   lint   cargo fmt --check + clippy -D warnings
#   unit   in-crate #[test] (--lib --bins) + doc tests, debug
#   e2e    every integration-test target (--tests), debug,
#          followed by the v7.38 element-B permutation matrix
#          runner (fast tier: 3 core permutations x sample corpus;
#          --full: every non-full-only permutation x full corpus);
#          perf/SLO targets compile empty here by design
#   gates  release-mode budget gates: perf_gate × crates +
#          prod_ready + slo_smoke
#   biz    customer-facing harnesses: sqllogictest + dump-compat +
#          data-compat (needs Docker — psql runs from the postgres:18 image)
#   dogfood  v7.37.5+ dogfood-replay testbed — every customer prod
#            incident encoded as a SPG-internal regression gate.
#            Fast tier skips snapshot-backed fixtures (synthetic
#            scenarios only); --full runs everything.
#   perf   SPGS against PG18 across the ORDER BY shape matrix +
#          the dogfood endpoints. RELEASE-BLOCKING as of round 895:
#          a measured loss fails the gate. Needs PG_URI + SPG_URI;
#          refuses (does not skip) without them.
#   all    everything above, in that order
#
# Tiers: the default run is the fast tier. `--full` adds
# `--include-ignored`, pulling in the long-running #[ignore]'d
# tests (1M-row gates, SQ8 kNN, exploratory benches). biz has a
# single tier — its harnesses are corpus-driven, not #[ignore]-split.
#
# Usage: scripts/gate.sh <lint|unit|e2e|gates|biz|dogfood|perf|all> [--full]
# Offload to the mini.local testbed: scripts/test-on-mini.sh <same args>
set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
    echo "usage: $0 <lint|unit|e2e|gates|biz|dogfood|perf|all> [--full]" >&2
    exit 2
}

CATEGORY="${1:-}"
[[ -n "$CATEGORY" ]] || usage
shift

FULL=0
for arg in "$@"; do
    case "$arg" in
        --full) FULL=1 ;;
        *) echo "gate.sh: unknown argument: $arg" >&2; usage ;;
    esac
done

# libtest args appended after `--` on full-tier runs.
#
TIER_ARGS=()
if [[ "$FULL" == 1 ]]; then
    TIER_ARGS+=(--include-ignored)
fi

# Crates with a release-only perf_gate integration target.
PERF_GATE_CRATES=(
    spg-audit spg-crypto spg-engine spg-sql spg-storage spg-wire
    spgctl spg-server
)

banner() { printf '\n══ gate.sh %s ══\n' "$*"; }

run_lint() {
    banner lint
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --locked -- -D warnings
}

run_unit() {
    banner unit
    # The everyday tier runs the unit tests unoptimised, which is the right
    # trade for a loop that runs constantly.
    #
    # `--full` adds `--include-ignored`, and some of those ignored tests are
    # ignored precisely BECAUSE they measure something — they carry budgets
    # in nanoseconds and say in their own comments to run them with
    # `--release`. Handing them a debug build fails them by construction: the
    # first `--full` run this branch ever did died on a 200 ns budget
    # measured at 1913 ns, which is 48 ns when built the way the test asks
    # for. That is the profile, not a regression.
    #
    # So the ignored pass runs in release, where its numbers mean something,
    # and the ordinary pass stays as it was.
    # v7.39.12 — the benchmark crate's binaries are not a unit-test
    # surface, and `--bins` was running all fifty-six of them.
    #
    # Counted, not timed, so the number reads the same on any machine:
    # `--lib --bins` selects 84 harnesses across this workspace and
    # **64 of them carry zero tests**. Sixty-two of the 64 are binary
    # targets, and 56 of the 62 belong to `spg-bench-competitor` — the
    # side-by-side benchmark harness whose `main`s time SPG against
    # PostgreSQL and MySQL, and whose manifest already says
    # `test = false` for them.
    #
    # It says so and the flag overrode it: an explicit target selector
    # wins over the manifest. Measured on `heavy`, which the manifest
    # marks `test = false` — `--tests` selects it 0 times and
    # `--lib --bins` selects it once; over nine benchmark bins, 1
    # against 9. Excluding the crate says the same thing in the place
    # that is honoured: 84 harnesses become 27 and the test count does
    # not move, 1,206 before and 1,206 after.
    #
    # The six binary targets that DO carry tests are in other crates
    # and keep running: spg-server (108), spgctl (23), the oracle (8),
    # perm (3) and dogfood (2) runners, sqllogictest (1).
    #
    # Why it is worth the line: each harness is a process spawn, and a
    # spawn is only free on an idle machine. This tier measured
    # 10,777 s on the testbed while another workload on the same box
    # held `syspolicyd` at 90% CPU; a probe binary that took 99 s to
    # launch under that load runs in under a second when the queue is
    # empty. None of that was the tier's work — but the spawns are the
    # part this file can decide not to make.
    # v7.39.12 — and `--bins` is gone, because `e2e` runs every one of
    # them minutes later.
    #
    # `cargo test --tests` selects the library and binary targets too,
    # not only `tests/`. Compared by harness NAME, the set this step
    # selected was a **subset of e2e's with nothing left over** — zero
    # names on the difference. So this invocation ran 27 harnesses and
    # 1,206 tests that the next step ran again.
    #
    # `--lib` stays: it is the tier's fast-fail signal, sixteen
    # harnesses of the crates' own unit tests, and a broken one should
    # not wait out an hour of e2e to say so. `--bins` was eleven more
    # harnesses — including spg-server's 108-test binary — for no
    # coverage `--tests` does not already give.
    # v7.39.12 — doc tests, and nothing else.
    #
    # Every distinct `cargo test` target selection costs a full
    # rebuild, because cargo resolves features over the selected
    # members and a different member set is a different feature set.
    # Measured on this workspace, the same command, the same tree:
    #
    #   after a DIFFERENT selection ran   358 s
    #   after the SAME selection ran       11 s
    #   the fifteen harnesses, by hand     11 s
    #
    # The 347 s is cargo re-resolving and rebuilding. This tier made a
    # dozen differently shaped calls and paid it at every transition —
    # `e2e` alone read 4,156 s of a 7,314 s tier, and the tests inside
    # it report 127 s.
    #
    # So each profile gets ONE selection. `--lib` is gone from here:
    # compared by harness name its set was a subset of e2e's `--tests`
    # with nothing left over, so it ran 1,206 tests that e2e ran again
    # AND made every later step rebuild. Doc tests are the one thing
    # `--tests` cannot reach, and they need their own rustdoc pass
    # whatever else happens.
    cargo test --workspace --exclude spg-bench-competitor --locked --doc
}


# v7.39.12 — one release build for every step that needs release
# artefacts, because each distinct cargo selection is a rebuild.
#
# The release side made eleven differently shaped calls — `-p
# spg-server --test prod_ready`, `-p spg-server --test slo_smoke`,
# `-p <crate> --test perf_gate` eight times, `-p sqllogictest`, `-p
# spg-dogfood-replay`, `-p spg-perm-runner` — and cargo re-resolved
# features and rebuilt at every transition. Measured on the debug
# side, same command and same tree: 358 s after a different selection,
# 11 s after the same one.
#
# Two selections now cover all of it: the workspace's binaries, and the
# workspace's release test harnesses. Everything downstream runs what
# they produced. `--features perf-counters` stays its own build; a
# different feature set is a different artefact and cannot be shared.
_release_built=0
ensure_release_build() {
    [[ "$_release_built" == 1 ]] && return 0
    banner "release build (once, shared by gates / biz / dogfood / perm)"
    cargo build --release --locked --workspace --exclude spg-bench-competitor
    cargo test --release --locked --workspace --exclude spg-bench-competitor \
        --tests --no-run
    _release_built=1
}

run_e2e() {
    banner e2e
    # 7.38.1 S1.3 (D10) — server-spawning e2e binaries under load:
    # bound the spawn storm and widen the child-startup deadline.
    # Overridable from the environment; a dead server still reds.
    export RUST_TEST_THREADS="${RUST_TEST_THREADS:-6}"
    export SPG_TEST_SPAWN_DEADLINE_SECS="${SPG_TEST_SPAWN_DEADLINE_SECS:-30}"
    # --tests = every test target (the merged e2e binaries, plus
    # in-crate unittests, plus the perf/SLO targets — which compile
    # empty under debug, so this stays a functional sweep).
    # v7.39.12 — the same exclude the unit step carries, for the same
    # reason and with the same kind of evidence. `--tests` selected 102
    # harnesses of which 60 carried zero tests; excluding the benchmark
    # crate leaves 62 harnesses, 20 of them empty, and 9,086 tests —
    # the same 9,086. Forty spawns that bought nothing, on top of the
    # fifty-seven the unit step was making.
    # v7.39.12 — build the selection once, then run what it produced.
    #
    # `run-test-binaries.sh` executes each harness directly, with the
    # package root as its working directory, which is what cargo does.
    # It exists so this step and the collation leg below share ONE
    # build instead of being two selections with a rebuild between
    # them. See the note in `run_unit` for the measurement.
    scripts/run-test-binaries.sh e2e \
        --workspace --exclude spg-bench-competitor --tests

    # v7.39.5 — and the wire panel a second time, under the collation the
    # published image ships.
    #
    # Until this version the panel declared no collation at all: the
    # harness clears three variables and inherits the rest, so what these
    # servers ordered text by was the operator's shell. Both machines
    # here export `LANG=en_US.UTF-8`, so the panel had been running under
    # a locale while every fixture in it was authored under `C` — and a
    # runner with `LANG` unset was running a different panel.
    #
    # `C` is the declared default now, and this leg is the other one. It
    # is the whole spg-server test surface, not a subset: the point is
    # that a locale changes ANSWERS, and there is no telling in advance
    # which fixture notices. Measured when it was added: 734 tests, green
    # both ways, which is why `e2e_panel_collation_v7395` exists — a
    # second panel that no fixture can distinguish is theatre.
    #
    # v7.39.12 — from the SAME build as the leg above, not a second
    # cargo selection. `cargo test -p spg-server --tests` names one
    # member, which is a different feature resolution and therefore a
    # full rebuild of the dependency graph — the trap this repository
    # documented for `unit-affected` and then repeated here. The
    # harnesses are already built; this runs the spg-server ones again
    # with the collation set.
    banner "e2e: shipped collation"
    RUN_FILTER=spg-server RUN_ENV="SPG_E2E_DB_COLLATION=en_US.utf8" \
        scripts/run-test-binaries.sh "e2e: shipped collation" \
        --workspace --exclude spg-bench-competitor --tests

    # v7.38 element B — permutation matrix runner. Fast tier walks
    # 3 core permutations (embedded / server_simple / topk_off) over
    # the fast_tier_sample corpus subset (~15 fixtures). Full tier
    # walks every non-full-only permutation over the include_globs.
    # See xtests/perm-runner/README.md for the matrix definition.
    banner "e2e: perm-runner"
    ensure_release_build
    if [[ "$FULL" == 1 ]]; then
        target/release/spg-perm-runner all --full
    else
        target/release/spg-perm-runner all --fast
    fi
}

run_gates() {
    banner gates
    # v7.39.12 — eleven cargo selections became one shared build plus
    # three filtered runs over what it produced. See
    # `ensure_release_build`; the per-crate `perf_gate` loop is now a
    # filter over already-built harnesses rather than eight rebuilds.
    ensure_release_build
    RUN_FILTER=prod_ready scripts/run-test-binaries.sh "gates: prod_ready" \
        --release --workspace --exclude spg-bench-competitor --tests
    RUN_FILTER=slo_smoke scripts/run-test-binaries.sh "gates: slo_smoke" \
        --release --workspace --exclude spg-bench-competitor --tests
    RUN_FILTER=perf_gate scripts/run-test-binaries.sh "gates: perf_gate" \
        --release --workspace --exclude spg-bench-competitor --tests
    # r1018 — the two counter pins, each its own cargo invocation.
    #
    # They read process-global `UNIQ_PROBE_*` counters, which only exist
    # under `perf-counters`, and that feature must not unify into the
    # workspace build (round 718: it leaked through a shared build graph and
    # made target/release/spg-server 7-13x slower). Separate invocation,
    # separate build graph. Separate TARGETS too, because the counters are
    # process-global and two tests in one binary dilute each other's reading
    # — measured, not assumed (round 751, and again in r1018).
    #
    # They ran nowhere before this. `scripts/gates.sh` had the round-751
    # step and is a hand-copied testbed script `gate.sh` never calls, whose
    # `cd ~/spg` points at a clone that had been stale for six days. A pin
    # outside the gate that runs is a pin that does not exist.
    for target in uniq_prune_counters uniq_composite_probe pred_int_lane_counters; do
        banner "gates: ${target} (perf-counters, own process)"
        cargo test --release --locked -p spg-engine --features perf-counters \
            --test "$target"
    done
    # r1049 — the sqlx suite, against a server this gate starts itself.
    #
    # It had sat behind $SPG_PG_URL since v7.9 and nothing in the gate
    # ever set the variable, so the ONE harness that binds parameters
    # the way real drivers do — binary format — had never run here.
    # sentori's second report (jsonb/array binary Bind refused, suite
    # dead on step four) was sitting in an #[ignore]d test the whole
    # time. A pin outside the gate that runs is a pin that does not
    # exist — same words as the counter pins above, same lesson.
    banner "gates: sqlx-pgwire (binary Bind/results, own server)"
    cargo build --release --locked -p spg-server
    rm -rf /tmp/spg-gate-sqlx-db /tmp/spg-gate-sqlx-wal /tmp/spg-gate-sqlx-audit
    SPG_PG_ADDR=127.0.0.1:25460 ./target/release/spg-server 127.0.0.1:25461 \
        /tmp/spg-gate-sqlx-db /tmp/spg-gate-sqlx-audit /tmp/spg-gate-sqlx-wal \
        > /tmp/spg-gate-sqlx.log 2>&1 &
    local sqlx_srv=$!
    sleep 1
    local sqlx_rc=0
    # v7.39.12 — from the shared release build, filtered, rather than a
    # single-package selection that rebuilds the graph. `--ignored` is
    # passed through to the harness.
    RUN_FILTER=spg-sqlx-pgwire \
    RUN_ENV="SPG_PG_URL=postgres://bench:bench@127.0.0.1:25460/bench" \
    RUN_ARGS="--ignored" \
        scripts/run-test-binaries.sh "biz: sqlx-pgwire" \
        --release --workspace --exclude spg-bench-competitor --tests \
        || sqlx_rc=$?
    kill "$sqlx_srv" 2>/dev/null || true
    return "$sqlx_rc"
}

run_biz() {
    banner biz
    ensure_release_build
    target/release/sqllogictest
    # v7.39.4 — the same corpus under the collation the PRODUCT SHIPS.
    #
    # The image carries `LANG=en_US.utf8` and says so on the way up
    # (`spg-server: database collation "en_US.utf8"`); every record above
    # runs on a catalog that orders by BYTES. A defect that needs the
    # database to name a collation is invisible in the first run no
    # matter how many records it grows, which is how v7.39.3 shipped a
    # session collation that reached equality and not ordering.
    #
    # The first run of this panel found two more, neither about
    # ordering: a UNIQUE text column ADMITTED A DUPLICATE, and MySQL
    # `MATCH … AGAINST` over a FULLTEXT index answered no rows. Both are
    # fixed in this version; this leg is what keeps them fixed.
    #
    # Records that assert byte order say `skipif spg-collated` and are
    # skipped here — nowhere else.
    SPG_SLT_DB_COLLATION=en_US.utf8 target/release/sqllogictest
    xtests/dump_compat/run.sh local-build
    xtests/data_compat/run.sh local-build
    # v7.39 (round 666) — the differential corpus joins the protocol.
    # It was the sixth gate in practice and the only one nobody could run
    # from here: it lived under `.claude/` (gitignored) and assumed a human
    # had already started a server. Both are fixed, so it belongs in the
    # list. It needs the live PG18 oracle container, same as the two
    # harnesses above.
    xtests/diffcorpus/run.sh
}

run_dogfood() {
    banner dogfood
    ensure_release_build
    # v7.37.5 — dogfood-replay testbed. Fast tier skips fixtures
    # whose snapshot is >100 MB (i.e. not present in CI) and still
    # exercises every synthetic recovery scenario. `--full` runs
    # every fixture including snapshot-backed ones.
    # v7.37.8 — `--bin spg-dogfood-replay` disambiguates against the
    # peer `spg-stress-cascade` binary (added in v7.37.7 for the K02
    # cascade reproducer). Without it cargo errors with
    # "could not determine which binary to run".
    if [[ "$FULL" == 1 ]]; then
        target/release/spg-dogfood-replay all
    else
        target/release/spg-dogfood-replay all --fast
    fi
}

# v7.37 (round 895) — performance against PG18, and it BLOCKS.
#
# The owner reversed the old policy on 2026-08-09: a measured loss stops
# the release whether or not a customer has reported it. The panel this
# runs judges by non-overlapping ranges and carries a same-binary control
# that reports its own resolution, so a cell it calls a loss is one it can
# actually tell apart.
#
# It needs a live PG18 and a live SPGS, which a plain checkout does not
# have. When they are not configured this REFUSES rather than skipping:
# a silent skip is how the ORDER BY surface stayed outside the panel while
# 29 of its 32 cells lost. `SKIP_PERF=1` is the deliberate escape hatch and
# it says so on the way past.
run_perf() {
    banner "perf vs PG18 (release-blocking)"
    if [[ -n "${SKIP_PERF:-}" ]]; then
        echo "perf: SKIPPED by SKIP_PERF=1 — a release built on this run has"
        echo "      NOT been checked against PG18."
        return 0
    fi
    if [[ -z "${PG_URI:-}" || -z "${SPG_URI:-}" ]]; then
        # v7.39.2 — before deciding this is unconfigured, ask the step
        # that configures ITSELF.
        #
        # `suite-run step perf-sweep` builds both legs rather than taking
        # them from whoever is typing, and it carries three corrections
        # that hand-configuration got wrong: the PostgreSQL leg has to be
        # the C-collation `bench_c` database (an `en_US` leg made the
        # sixty-four cells report wins on the collation difference), the
        # two legs have to be reached over the same route (a
        # container-to-host hop on one side alone turned 2 losing cells
        # into 20), and the SPGS leg has to DECLARE `SPG_LC_COLLATE=C`
        # rather than inherit the machine's.
        #
        # Asking the operator for `PG_URI` and `SPG_URI` blocked the
        # v7.39.1 train twice, and the second time the hand-written
        # configuration had to reproduce all three of those corrections
        # from the maintained step's source. A gate should not require
        # that of the person running it.
        if [[ -x "$HOME/spgbench/bin/psql" ]]; then
            echo "perf: PG_URI / SPG_URI unset; using the maintained sweep, which"
            echo "      configures both legs itself (testbed detected)."
            cargo run --quiet -p suitelib -- step perf-sweep
            return $?
        fi
        # A routine gate on a checkout has no PG18 to compare against, and
        # breaking the everyday loop is not what "perf blocks the release"
        # asks for. It blocks the RELEASE: `release.sh` sets
        # PERF_REQUIRED=1, and there the same missing configuration is a
        # hard failure.
        if [[ -n "${PERF_REQUIRED:-}" ]]; then
            echo "perf: PG_URI and SPG_URI are unset, so nothing was compared." >&2
            echo "      Performance blocks the release, so on a release run this" >&2
            echo "      is a failure and not a skip. Point both at a live PG18 and" >&2
            echo "      a live SPGS, or set SKIP_PERF=1 to put on the record that" >&2
            echo "      this build shipped unchecked." >&2
            return 1
        fi
        echo "perf: SKIPPED — PG_URI / SPG_URI unset, so SPGS was not compared"
        echo "      against PG18. This is fine for a working gate and is NOT"
        echo "      fine for a release: release.sh sets PERF_REQUIRED=1 and the"
        echo "      same state fails there."
        return 0
    fi
    scripts/perf-endpoint-sweep.sh
}

START=$SECONDS
case "$CATEGORY" in
    lint)    run_lint ;;
    unit)    run_unit ;;
    e2e)     run_e2e ;;
    gates)   run_gates ;;
    biz)     run_biz ;;
    dogfood) run_dogfood ;;
    perf)    run_perf ;;
    all)     run_lint; run_unit; run_e2e; run_gates; run_biz; run_dogfood; run_perf ;;
    *) usage ;;
esac
printf '\n══ gate.sh %s%s: PASS (%ss) ══\n' \
    "$CATEGORY" "$([[ "$FULL" == 1 ]] && echo ' --full')" "$((SECONDS - START))"
