#!/usr/bin/env bash
# test-on-mini.sh — run scripts/gate.sh on the mini.local testbed
# instead of the dev machine, so local builds/sessions and the
# test sweep stop fighting over CPU and the cargo build lock.
#
# Syncs the working tree (gitignore-filtered, so target/ and
# .claude/ never travel) to $SPG_MINI_HOST:$SPG_MINI_DIR and execs
# gate.sh there. The remote keeps its own target/ between runs, so
# repeat runs build incrementally — see the `P /target/` protect rule
# at the rsync below for why the gitignore filter alone did not do it.
#
# Usage: scripts/test-on-mini.sh <gate.sh args...>
#   scripts/test-on-mini.sh e2e
#   scripts/test-on-mini.sh gates --full
#   scripts/test-on-mini.sh biz       # now supported (was previously local-only)
#
# Long runs: --detach / --result
#
#   scripts/test-on-mini.sh --detach all    # sync, start, return
#   scripts/test-on-mini.sh --result        # verdict, or "still running"
#
# A `gate.sh all` takes over twelve minutes, and an ssh that drops in the
# middle of one is not a failed run — it is a run that KEEPS GOING on the
# testbed while the caller is told it finished. r1034 lost an hour to
# that: three "completed" gates whose logs stopped mid-way, and an
# orphaned `cargo test` still holding the build lock, which made the next
# run block and then look like it had been killed too.
#
# Detached, the verdict is a file on the machine that produced it, so a
# dropped connection costs nothing and there is never more than one
# runner. `--result` refuses to guess: it reports "still running" rather
# than an empty log, because an empty log and a clean pass look alike.
#
# Mini.local has OrbStack docker (per `feedback-offload-heavy-to-mini`
# memory + verified during v7.37 ship cycle running docker-fair bench).
# We export the OrbStack PATH so the remote shell finds `docker`, and
# we sync .git so `git rev-parse` etc work in biz harness paths.
set -euo pipefail
cd "$(dirname "$0")/.."

HOST="${SPG_MINI_HOST:-mini.local}"
RDIR="${SPG_MINI_DIR:-workspace/goliajp/spg-ci}"

[[ $# -ge 1 ]] || { echo "usage: $0 [--detach|--result] <gate.sh args...>" >&2; exit 2; }

REMOTE_LOG=/tmp/spg-gate.log
REMOTE_DONE=/tmp/spg-gate.done

# `--prev` reads the log the run before this one left behind.
if [[ "$1" == "--prev" ]]; then
    ssh "$HOST" "tail -40 '$REMOTE_LOG.prev' 2>/dev/null || echo 'no previous log'"
    exit $?
fi

if [[ "$1" == "--result" ]]; then
    ssh "$HOST" "
        if [ -f '$REMOTE_DONE' ]; then
            grep -aE 'GATE EXIT|TOTAL |FAILING|gate\.sh .*(PASS|FAIL)' '$REMOTE_LOG' | tail -20
        elif pgrep -qf 'scripts/gate\.sh'; then
            echo 'still running:'
            tail -2 '$REMOTE_LOG' 2>/dev/null
        else
            echo 'NOT RUNNING and no sentinel — the run died. Last lines:'
            tail -5 '$REMOTE_LOG' 2>/dev/null
            exit 1
        fi"
    exit $?
fi

DETACH=""
if [[ "$1" == "--detach" ]]; then
    DETACH=1
    shift
    [[ $# -ge 1 ]] || { echo "usage: $0 --detach <gate.sh args...>" >&2; exit 2; }
fi

ssh "$HOST" "mkdir -p '$RDIR'"
# Sync .git so biz / sqllogictest harnesses that call `git rev-parse`
# don't fail with "not a git repository". target/ stays via gitignore
# (rsync's --filter ignores .gitignore'd paths).
# r1022 — `P /target/` PROTECTS the remote build directory from --delete.
#
# The line above used to be the two filters without it, and the comment
# below it said the gitignore filter kept the remote `target/` between
# runs. It did not. Tested directly: drop a marker file in the remote
# `target/`, run this rsync, and the marker — and the whole directory — is
# gone. An exclude keeps rsync from SENDING a path; it does not, through a
# per-directory merge file, keep --delete from removing it on the receiver.
#
# So every run here was a cold rebuild. That is where this session's
# 1200-1500 s gate runs and two-minute single-crate builds came from.
# `P` is a protect rule, which is the receiver-side half the exclude was
# assumed to carry.
rsync -az --delete --filter='P /target/' --filter=':- .gitignore' \
    ./ "$HOST:$RDIR/"

# Stage what just arrived, because the selectors here are diffs.
#
# `affected_selection` and `pins_current` both ask
# `git diff --name-only HEAD`, which lists tracked files — staged or not.
# The remote checkout sits at whatever commit it was last synced from and
# receives everything else over rsync, so a file that is NEW locally
# arrives here UNTRACKED and no diff mentions it.
#
# v7.38.23 found what that costs: a new e2e pin was added, the tier ran,
# and `pins-current` reported "no e2e pins touched — skipped". The step
# whose entire purpose is to run this commit's pins — and to go red when
# a pin file is not wired into `main.rs` — saw nothing, because the pin
# was the one untracked file in the tree. Staged, the same run reports
# "1 pin(s) over 1 touched file(s)".
#
# `git diff ... HEAD` counts staged changes, so this only ever ADDS to
# what the selectors see. Nothing here is committed or pushed.
ssh "$HOST" "cd '$RDIR' && git add -A" 2>/dev/null || true
if [[ -n "$DETACH" ]]; then
    # One runner at a time: a second would fight the first for the cargo
    # build lock and both would look hung.
    ssh "$HOST" "pkill -f 'scripts/gate\\.sh' 2>/dev/null; sleep 1; true"
    ssh "$HOST" "cd '$RDIR' && nohup bash scripts/mini-gate-runner.sh $* > /dev/null 2>&1 &"
    echo "started on $HOST — read it with: scripts/test-on-mini.sh --result"
    exit 0
fi

# OrbStack PATH so `docker` resolves on the non-interactive ssh shell.
# `export` (not just prefix) so child processes — including `cargo run`
# subprocesses spawned by gate.sh — inherit the PATH and can find
# docker. Without `export`, the prefix-set PATH was visible to bash
# but not to its children, making biz dump_compat / data_compat fail
# silently while sqllogictest (which doesn't shell out to docker) passed.
exec ssh "$HOST" "export PATH=/Applications/OrbStack.app/Contents/MacOS/xbin:\$PATH && cd '$RDIR' && exec scripts/gate.sh $*"
