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
#
# v7.38.13 — the liveness probe used to look for "scripts/suite.sh",
# which `--on-mini` never launches: it launches
# `bash scripts/mini-suite-runner.sh`. So EVERY in-progress run was
# reported as "the run died", exit 1. That is the worst direction for
# this particular lie to point -- it invites killing a healthy run and
# starting it over. Caught during the 7.38.13 release, on a run whose
# clippy was visibly still going.
if [[ "${1:-}" == "--result" ]]; then
    exec ssh "${SPG_MINI_HOST:-mini.local}" '
        if [ -f /tmp/spg-suite.done ]; then
            tail -20 /tmp/spg-suite.log
        elif pgrep -qf "mini-suite-runner\.sh|suite-run (precommit|prerelease|full)"; then
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
    # v7.38.14 — kill the RUNNER, not only the suite it wrapped.
    #
    # This killed the inner `scripts/suite.sh` and left
    # `mini-suite-runner.sh` alive. The orphaned runner then carried on to
    # its last two lines -- append "SUITE EXIT" and `touch` the done
    # sentinel -- while the NEW run was already writing the same log. So a
    # freshly started run inherited a completion signal from the corpse of
    # the one before it, and anything waiting on that sentinel read the new
    # run as finished within seconds. Measured: a wait fired on a sentinel
    # stamped 00:33:41 while the log it claimed to describe was still
    # growing at 00:35:11.
    #
    # An instrument that says "done" about a run still in progress is the
    # same failure as one that says "dead" about a run still alive, which
    # this file also had until this release.
    ssh "$HOST" "pkill -f 'mini-suite-runner\\.sh' 2>/dev/null; \
                 pkill -f 'scripts/suite\\.sh' 2>/dev/null; sleep 1; \
                 rm -f /tmp/spg-suite.done; true"
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
