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

# S1.4 — `--result` reads the detached mini run's verdict; refuses to
# guess while it is still running (test-on-mini.sh discipline).
if [[ "${1:-}" == "--result" ]]; then
    exec ssh "${SPG_MINI_HOST:-mini.local}" '
        if [ -f /tmp/spg-suite.done ]; then
            tail -20 /tmp/spg-suite.log
        elif pgrep -qf "scripts/suite\.sh"; then
            echo "still running:"; tail -3 /tmp/spg-suite.log
        else
            echo "NOT RUNNING and no sentinel — the run died. Last lines:"
            tail -5 /tmp/spg-suite.log; exit 1
        fi'
fi

TIER="${1:-}"
case "$TIER" in
    precommit|prerelease|full) ;;
    *) echo "usage: $0 <precommit|prerelease|full> [--on-mini] | --result" >&2; exit 2 ;;
esac
shift

# S1.4 — run this tier on the testbed: rsync (P-protecting target/,
# r1022), launch detached via the runner file, come back for the
# verdict with `suite.sh --result`.
if [[ "${1:-}" == "--on-mini" ]]; then
    HOST="${SPG_MINI_HOST:-mini.local}"
    RDIR="${SPG_MINI_DIR:-workspace/goliajp/spg-ci}"
    rsync -az --delete --filter='P /target/' --filter=':- .gitignore' \
        ./ "$HOST:$RDIR/"
    ssh "$HOST" "pkill -f 'scripts/suite\\.sh' 2>/dev/null; sleep 1; true"
    ssh "$HOST" "cd '$RDIR' && nohup bash scripts/mini-suite-runner.sh $TIER > /dev/null 2>&1 &"
    echo "suite $TIER started on $HOST — read it with: scripts/suite.sh --result"
    exit 0
fi

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
