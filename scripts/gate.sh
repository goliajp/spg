#!/usr/bin/env bash
# gate.sh — unified test-category runner.
#
# The test surface is split into five categories (see docs/TESTING.md):
#
#   lint   cargo fmt --check + clippy -D warnings
#   unit   in-crate #[test] (--lib --bins) + doc tests, debug
#   e2e    every integration-test target (--tests), debug;
#          perf/SLO targets compile empty here by design
#   gates  release-mode budget gates: perf_gate × crates +
#          prod_ready + slo_smoke
#   biz    customer-facing harnesses: sqllogictest + dump-compat +
#          data-compat (needs Docker — psql runs from the postgres:18 image)
#   all    everything above, in that order
#
# Tiers: the default run is the fast tier. `--full` adds
# `--include-ignored`, pulling in the long-running #[ignore]'d
# tests (1M-row gates, SQ8 kNN, exploratory benches). biz has a
# single tier — its harnesses are corpus-driven, not #[ignore]-split.
#
# Usage: scripts/gate.sh <category> [--full]
# Offload to the mini.local testbed: scripts/test-on-mini.sh <same args>
set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
    echo "usage: $0 <lint|unit|e2e|gates|biz|all> [--full]" >&2
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
}

START=$SECONDS
case "$CATEGORY" in
    lint)  run_lint ;;
    unit)  run_unit ;;
    e2e)   run_e2e ;;
    gates) run_gates ;;
    biz)   run_biz ;;
    all)   run_lint; run_unit; run_e2e; run_gates; run_biz ;;
    *) usage ;;
esac
printf '\n══ gate.sh %s%s: PASS (%ss) ══\n' \
    "$CATEGORY" "$([[ "$FULL" == 1 ]] && echo ' --full')" "$((SECONDS - START))"
