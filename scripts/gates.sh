#!/bin/bash
# r1019 — this script is retired. It now runs `gate.sh all`, which is what
# anyone reaching for it wanted.
#
# What it used to be: a hand-copied testbed runner holding seven gates. Every
# one of them duplicated a `gate.sh` category — TESTS, CLIPPY, CONFORMANCE,
# DUMP-COMPAT, DATA-COMPAT, CORPUS — except the round-751 uniqueness-counter
# step, which existed only here.
#
# Three things were wrong with that, and they compound:
#
#  1. `gate.sh` never called this file. So the round-751 pin, the one gate
#     that lived nowhere else, ran nowhere. It was moved into
#     `gate.sh run_gates` in r1018, alongside its r1019 sibling.
#  2. It began `cd ~/spg`, a path that on the testbed is a clone nobody
#     syncs — six days stale when this was found. A gate that greens against
#     the wrong tree is worse than no gate.
#  3. It could not fail. Steps printed `SLT GATE RED` / `CORPUS GATE RED`
#     rather than exiting, `grep -c PASS` counted instead of asserting, and
#     the script ended on `echo GATES_DONE`, so its exit status was always
#     zero. A caller checking `$?` learned nothing.
#
# `gate.sh` has none of those: it runs in the tree it is invoked from, its
# clippy step is `-D warnings` rather than a warning count, and it propagates
# the first failure.
exec "$(dirname "$0")/gate.sh" "${@:-all}"
