#!/usr/bin/env bash
# v7.38.17 — run the `full` tier and leave a report where a person will
# see it.
#
# `full` holds ten steps that nothing schedules. CI has a push job and a
# daily drop-in check; neither touches the tier. So those steps run only
# when someone types them, which means they are not evidence — a release
# reads a green `prerelease` and stops. `suite-run` now prints a NOT RUN
# line naming them, and this script is the other half.
#
# Budgets sum to about two hours, so this belongs on a schedule rather
# than in a release train.
#
# Install (on the testbed, not from here — a cron entry on someone's
# machine is theirs to add):
#
#   crontab -e
#   17 3 * * *  cd /path/to/spg && scripts/nightly-full.sh >> /tmp/spg-nightly.log 2>&1
#
# Exit status is the tier's. A red nightly is meant to be noticed.
set -euo pipefail

cd "$(dirname "$0")/.."
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUT="target/suite/nightly-full-${STAMP}.log"
mkdir -p target/suite

echo "== nightly full tier ${STAMP} =="
echo "== HEAD $(git rev-parse --short HEAD) on $(git branch --show-current) =="

# OrbStack keeps docker off the default PATH; the oracle and drop-in
# steps need it.
ORB=/Applications/OrbStack.app/Contents/MacOS/xbin
[ -d "$ORB" ] && export PATH="$PATH:$ORB"

set +e
cargo run -q --release -p suitelib --bin suite-run -- full 2>&1 | tee "$OUT"
status=${PIPESTATUS[0]}
set -e

echo "== full tier exit ${status}; report ${OUT} =="
# r664 — the runner that fails politely is the runner nobody hears.
exit "$status"
