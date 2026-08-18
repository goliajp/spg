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
    cargo test --workspace --locked --lib --bins
    if [[ "$FULL" == 1 ]]; then
        cargo test --release --workspace --locked --lib --bins -- --ignored
    fi
    cargo test --workspace --locked --doc
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
    cargo test --workspace --locked --tests -- "${TIER_ARGS[@]+"${TIER_ARGS[@]}"}"

    # v7.38 element B — permutation matrix runner. Fast tier walks
    # 3 core permutations (embedded / server_simple / topk_off) over
    # the fast_tier_sample corpus subset (~15 fixtures). Full tier
    # walks every non-full-only permutation over the include_globs.
    # See xtests/perm-runner/README.md for the matrix definition.
    banner "e2e: perm-runner"
    if [[ "$FULL" == 1 ]]; then
        cargo run --release --locked -p spg-perm-runner -- all --full
    else
        cargo run --release --locked -p spg-perm-runner -- all --fast
    fi
}

run_gates() {
    banner gates
    cargo test --release --locked -p spg-server --test prod_ready
    cargo test --release --locked -p spg-server --test slo_smoke -- \
        --nocapture "${TIER_ARGS[@]+"${TIER_ARGS[@]}"}"
    for crate in "${PERF_GATE_CRATES[@]}"; do
        banner "gates: perf_gate ${crate}"
        cargo test --release --locked -p "$crate" --test perf_gate -- \
            --test-threads=1 --nocapture "${TIER_ARGS[@]+"${TIER_ARGS[@]}"}"
    done
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
    SPG_PG_URL='postgres://bench:bench@127.0.0.1:25460/bench' \
        cargo test --release --locked -p spg-sqlx-pgwire -- --ignored \
        || sqlx_rc=$?
    kill "$sqlx_srv" 2>/dev/null || true
    return "$sqlx_rc"
}

run_biz() {
    banner biz
    cargo run --release --locked -p sqllogictest
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
    # v7.37.5 — dogfood-replay testbed. Fast tier skips fixtures
    # whose snapshot is >100 MB (i.e. not present in CI) and still
    # exercises every synthetic recovery scenario. `--full` runs
    # every fixture including snapshot-backed ones.
    # v7.37.8 — `--bin spg-dogfood-replay` disambiguates against the
    # peer `spg-stress-cascade` binary (added in v7.37.7 for the K02
    # cascade reproducer). Without it cargo errors with
    # "could not determine which binary to run".
    if [[ "$FULL" == 1 ]]; then
        cargo run --release --locked -p spg-dogfood-replay \
            --bin spg-dogfood-replay -- all
    else
        cargo run --release --locked -p spg-dogfood-replay \
            --bin spg-dogfood-replay -- all --fast
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
