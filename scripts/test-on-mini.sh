#!/usr/bin/env bash
# test-on-mini.sh — run scripts/gate.sh on the mini.local testbed
# instead of the dev machine, so local builds/sessions and the
# test sweep stop fighting over CPU and the cargo build lock.
#
# Syncs the working tree (gitignore-filtered, so target/ and
# .claude/ never travel) to $SPG_MINI_HOST:$SPG_MINI_DIR and execs
# gate.sh there. The remote keeps its own target/ between runs —
# rsync's gitignore filter protects it from --delete — so repeat
# runs build incrementally.
#
# Usage: scripts/test-on-mini.sh <gate.sh args...>
#   scripts/test-on-mini.sh e2e
#   scripts/test-on-mini.sh gates --full
#
# Caveat: the biz category needs Docker (its harnesses run psql from
# the postgres:15 image) and a .git dir for rev-parse — neither exists
# on the testbed mirror, so run biz locally.
set -euo pipefail
cd "$(dirname "$0")/.."

HOST="${SPG_MINI_HOST:-mini.local}"
RDIR="${SPG_MINI_DIR:-workspace/goliajp/spg-ci}"

[[ $# -ge 1 ]] || { echo "usage: $0 <gate.sh args...>" >&2; exit 2; }

ssh "$HOST" "mkdir -p '$RDIR'"
rsync -az --delete --filter=':- .gitignore' --exclude /.git \
    ./ "$HOST:$RDIR/"
exec ssh "$HOST" "cd '$RDIR' && exec scripts/gate.sh $*"
