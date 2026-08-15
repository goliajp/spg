#!/usr/bin/env bash
# Runs gate.sh ON the testbed, detached from the ssh session that started
# it, and drops a sentinel when it is done.
#
# A separate file rather than an inline `ssh "..."` command on purpose:
# the inline form needs three levels of quoting to get `$?` and `$PATH`
# through, and the first version of it silently failed to launch. A file
# has one level.
set -u
cd "$(dirname "$0")/.."
export PATH=/Applications/OrbStack.app/Contents/MacOS/xbin:$PATH
LOG=/tmp/spg-gate.log
DONE=/tmp/spg-gate.done
rm -f "$LOG" "$DONE"
scripts/gate.sh "$@" > "$LOG" 2>&1
echo "GATE EXIT=$?" >> "$LOG"
touch "$DONE"
