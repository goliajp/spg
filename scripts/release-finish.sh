#!/usr/bin/env bash
# release-finish.sh — finish a release branch and leave exactly one tag,
# named the way the rest of the toolchain expects.
#
# Why this exists: `git flow release finish` names the tag after the
# BRANCH, so `release/7.40.0` produces a tag `7.40.0`. The repository's
# convention — and `release.sh`'s own preflight, which refuses to
# publish without it — is `v7.40.0`. `gitflow.prefix.versiontag` is set
# to `v` in this repository's config, but that is a git-flow-avh key and
# the installed tool is git-flow-next 2.0.0, whose configuration schema
# has no version-tag prefix at all. So the setting is read by nobody.
#
# The result is twenty-one bare tags in this repository, from 7.22.0
# onward, several of them pushed. Every release since has depended on
# someone remembering to rename the tag by hand between `finish` and
# `release.sh` — and the failure is silent, because a bare tag is a
# perfectly valid tag.
#
# This passes `--tagname`, and then CHECKS, because a flag that is
# accepted is not the same as a flag that was honoured: if a bare tag
# turned up anyway it is renamed, and either way the run ends by
# asserting that `v<version>` exists, points at master's HEAD, and that
# no bare `<version>` tag is left behind.
#
# Usage: scripts/release-finish.sh <X.Y.Z>
set -euo pipefail
cd "$(dirname "$0")/.."

V="${1:?usage: $0 <X.Y.Z>}"
[[ "$V" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "not a version: $V" >&2; exit 2; }

# A dirty tree stops git-flow at the checkout of master, several steps
# in and after the release branch already exists — "failed to checkout
# target branch 'master'", which names neither the file nor the reason.
# Refuse here, where the message can.
if [[ -n "$(git status --porcelain)" ]]; then
    echo "release-finish: working tree not clean:" >&2
    git status --short >&2
    exit 1
fi

manifest_version=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
[[ "$manifest_version" == "$V" ]] \
    || { echo "Cargo.toml workspace version is ${manifest_version}, expected ${V}" >&2; exit 1; }

# git-flow's merges run the repository's hooks, and the release commit
# has already been through them on the branch.
export GIT_MERGE_AUTOEDIT=no

# Start the branch when it is not there. The first version of this
# script only finished, and `git flow release finish` answers "start
# point branch '7.40.1' does not exist" — which is true and unhelpful,
# because finishing a release IS the two steps and splitting them across
# two commands is how one of them gets forgotten.
if ! git rev-parse -q --verify "refs/heads/release/${V}" >/dev/null; then
    git flow release start "$V"
fi

git flow release finish --tagname "v${V}" -m "v${V}" "$V"

# The check, not the hope.
if git rev-parse -q --verify "refs/tags/${V}" >/dev/null; then
    echo "release-finish: git-flow created a bare tag '${V}' despite --tagname"
    if ! git rev-parse -q --verify "refs/tags/v${V}" >/dev/null; then
        git tag -a "v${V}" -m "v${V}" "${V}^{}"
    fi
    git tag -d "$V"
fi

git rev-parse -q --verify "refs/tags/v${V}" >/dev/null \
    || { echo "release-finish: no tag v${V} after finishing" >&2; exit 1; }
[[ "$(git rev-parse "v${V}^{commit}")" == "$(git rev-parse master)" ]] \
    || { echo "release-finish: v${V} does not point at master" >&2; exit 1; }
! git rev-parse -q --verify "refs/tags/${V}" >/dev/null \
    || { echo "release-finish: a bare tag ${V} is still present" >&2; exit 1; }

echo "release-finish: v${V} at $(git rev-parse --short master), no bare tag"
echo "next: git push origin master develop --follow-tags && scripts/release.sh ${V}"
