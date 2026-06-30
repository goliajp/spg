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
#   scripts/test-on-mini.sh biz       # now supported (was previously local-only)
#
# Mini.local has OrbStack docker (per `feedback-offload-heavy-to-mini`
# memory + verified during v7.37 ship cycle running docker-fair bench).
# We export the OrbStack PATH so the remote shell finds `docker`, and
# we sync .git so `git rev-parse` etc work in biz harness paths.
set -euo pipefail
cd "$(dirname "$0")/.."

HOST="${SPG_MINI_HOST:-mini.local}"
RDIR="${SPG_MINI_DIR:-workspace/goliajp/spg-ci}"

[[ $# -ge 1 ]] || { echo "usage: $0 <gate.sh args...>" >&2; exit 2; }

ssh "$HOST" "mkdir -p '$RDIR'"
# Sync .git so biz / sqllogictest harnesses that call `git rev-parse`
# don't fail with "not a git repository". target/ stays via gitignore
# (rsync's --filter ignores .gitignore'd paths).
rsync -az --delete --filter=':- .gitignore' \
    ./ "$HOST:$RDIR/"
# OrbStack PATH so `docker` resolves on the non-interactive ssh shell.
# `export` (not just prefix) so child processes — including `cargo run`
# subprocesses spawned by gate.sh — inherit the PATH and can find
# docker. Without `export`, the prefix-set PATH was visible to bash
# but not to its children, making biz dump_compat / data_compat fail
# silently while sqllogictest (which doesn't shell out to docker) passed.
exec ssh "$HOST" "export PATH=/Applications/OrbStack.app/Contents/MacOS/xbin:\$PATH && cd '$RDIR' && exec scripts/gate.sh $*"
