#!/usr/bin/env bash
# precommit-tier.sh — run the precommit tier where the machine is idle,
# and prove it graded THIS commit.
#
# The pre-commit hook used to be `exec scripts/suite.sh precommit`, run
# wherever the operator happened to be typing. On the development box
# under its usual load that tier has taken over six minutes and been
# measured at 3,136 s for a step the testbed does in 6.8 s; the same
# tier on the testbed finishes in 45 s. What a gate that slow actually
# produces is `--no-verify`: two commits on 2026-09-05 alone, each
# bypassing the gate and then running it by hand on the testbed anyway.
# A gate people route around is not a gate.
#
# So this runs it on the testbed when there is one, and the interesting
# part is not the offload — it is the WITNESS.
#
# Running a gate on a synced copy is the defect class this repository
# has been bitten by twice: `rsync -a` preserved mtimes and cargo
# graded a stale binary (v7.40.0's MySQL corpus), and rsync'd files are
# untracked on the far side so diff-selected steps could not see them
# (7.38.x). A copy that is nearly right grades green and says nothing.
#
# The witness is `git write-tree`. Locally it hashes the INDEX — which
# during a pre-commit hook is exactly the tree about to be committed.
# On the testbed, after `git add -A`, it hashes the synced worktree.
# Equal object ids mean the two trees are byte-identical, whatever
# either side's HEAD or history says. Unequal means the testbed would
# be grading something else, and the tier runs locally instead —
# which is what happens for a partial commit, correctly, because then
# the worktree is not what is being committed.
#
# Env:
#   SPG_TESTBED       host to offload to (default mini.local)
#   SPG_TESTBED_PATH  repo path there (default ~/workspace/goliajp/spg-ci)
#   SPG_NO_OFFLOAD=1  run locally, no questions
set -euo pipefail
cd "$(dirname "$0")/.."

host="${SPG_TESTBED:-mini.local}"
path="${SPG_TESTBED_PATH:-workspace/goliajp/spg-ci}"

run_local() { exec scripts/suite.sh precommit; }

[[ -z "${SPG_NO_OFFLOAD:-}" ]] || run_local
ssh -o BatchMode=yes -o ConnectTimeout=4 "$host" true 2>/dev/null || {
    echo "precommit: ${host} not reachable — running here"
    run_local
}

local_tree=$(git write-tree)

rsync -a --delete --exclude target --exclude .git ./ "${host}:${path}/" || {
    echo "precommit: rsync to ${host} failed — running here"
    run_local
}

# Rebuild the far index FROM THE WORKTREE, rather than adding on top of
# whatever its HEAD carries.
#
# `git add -A` alone was not enough, and the witness said so on its
# first two runs. The testbed's `.git` is deliberately not synced — its
# HEAD sits wherever it was last left — so files that are tracked THERE
# and excluded from the rsync (anything under a `target/`) stay in its
# index and land in its tree. The first run differed by 1,036 such
# paths; that is how the appsql artefacts above were found.
#
# Emptying the index first makes the tree a pure function of the synced
# worktree and the ignore rules, which is the thing being compared. The
# far clone is a scratch checkout; a mass deletion against its stale
# HEAD costs nothing and is never committed.
#
# `git read-tree --empty` rather than `git rm -r --cached .`: the latter
# REFUSES entries whose index, worktree and HEAD disagree three ways,
# which is the normal state of a scratch clone, and it refuses them one
# at a time. The first version of this line suppressed that failure with
# `;` and `2>&1 >/dev/null` and left one file behind —
# `xtests/appsql/differ/Cargo.lock` — which is the same
# swallowed-partial-failure shape as everything else this release fixed.
remote_tree=$(ssh "$host" "cd ${path} && git read-tree --empty && git add -A && git write-tree") || {
    echo "precommit: could not read ${host}'s tree — running here"
    run_local
}

if [[ "$local_tree" != "$remote_tree" ]]; then
    echo "precommit: ${host}'s tree is ${remote_tree}, this commit is ${local_tree}"
    echo "precommit: it would be grading something else — running here"
    run_local
fi

echo "precommit: on ${host}, tree ${local_tree} — the same bytes this commit carries"
ssh "$host" "cd ${path} && export PATH=\$HOME/.orbstack/bin:\$PATH && bash scripts/suite.sh precommit"
