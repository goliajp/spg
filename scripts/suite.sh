#!/usr/bin/env bash
# suite.sh — v7.38 tier entry (design D1: this shell stays THIN).
#
#   scripts/suite.sh precommit
#   scripts/suite.sh prerelease [--on-mini]     (S1.4 wires --on-mini)
#   scripts/suite.sh full
#
# All orchestration lives in `suite-run` (xtests/suitelib); this file
# only checks the environment the tier is entitled to and forwards.
# Canonical design: .claude/testsuite/ — CHECKLIST.md is the build plan.
set -euo pipefail
cd "$(dirname "$0")/.."

TIER="${1:-}"
case "$TIER" in
    precommit|prerelease|full) ;;
    *) echo "usage: $0 <precommit|prerelease|full> [args...]" >&2; exit 2 ;;
esac
shift

# Tier entitlements (audit A12): precommit must run anywhere — no
# docker, no network beyond localhost. Say so before a step fails
# confusingly halfway in.
if [[ "$TIER" != "precommit" ]]; then
    command -v docker >/dev/null 2>&1 || {
        echo "suite.sh: $TIER needs docker (live-PG legs) and it is not on PATH" >&2
        exit 2
    }
fi

exec cargo run -q -p suitelib --bin suite-run -- "$TIER" "$@"
