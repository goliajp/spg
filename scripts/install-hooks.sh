#!/usr/bin/env bash
# install-hooks.sh — point this clone's hooks at the tracked ones.
#
# `core.hooksPath` rather than copying: a copy goes stale the moment
# the tracked hook changes, and nothing would say so.
set -euo pipefail
cd "$(dirname "$0")/.."
git config core.hooksPath scripts/hooks
echo "hooks: core.hooksPath = scripts/hooks"
ls -1 scripts/hooks
