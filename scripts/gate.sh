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
    cargo test --workspace --locked --lib --bins -- "${TIER_ARGS[@]+"${TIER_ARGS[@]}"}"
    cargo test --workspace --locked --doc
}

run_e2e() {
    banner e2e
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
