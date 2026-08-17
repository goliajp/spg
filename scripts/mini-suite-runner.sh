#!/usr/bin/env bash
# Runs suite.sh ON the testbed, detached, sentinel on completion —
# the suite twin of mini-gate-runner.sh (S1.4; same reasoning: a file
# has one level of quoting, an inline ssh command has three).
set -u
cd "$(dirname "$0")/.."
export PATH=/Applications/OrbStack.app/Contents/MacOS/xbin:$PATH
LOG=/tmp/spg-suite.log
DONE=/tmp/spg-suite.done
[ -f "$LOG" ] && mv -f "$LOG" "$LOG.prev"
rm -f "$DONE"
scripts/suite.sh "$@" > "$LOG" 2>&1
echo "SUITE EXIT=$?" >> "$LOG"
touch "$DONE"
