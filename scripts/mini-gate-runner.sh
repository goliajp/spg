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
# Keep the previous run's log. Starting a new one used to delete it, and a
# failure nobody had read yet went with it — which is how r1035 lost the
# detail of a red gate by reading its exit code and relaunching in the same
# breath.
[ -f "$LOG" ] && mv -f "$LOG" "$LOG.prev"
rm -f "$DONE"
scripts/gate.sh "$@" > "$LOG" 2>&1
echo "GATE EXIT=$?" >> "$LOG"
touch "$DONE"
