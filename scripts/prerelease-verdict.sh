#!/usr/bin/env bash
# v7.39.11 — did `scripts/suite.sh prerelease` pass on THIS commit?
#
# Exits 0 and prints the report path when a green prerelease report
# exists for the working tree's HEAD; exits 1 otherwise. `release.sh`
# uses it to skip the battery it would otherwise run a second time.
#
# The release used to run the whole thing twice. `suite.sh prerelease`
# is lint / unit / e2e / gates / biz / dogfood / perf-sweep / ironrules
# / oracle-three, about 74 minutes of budget; `release.sh` then ran
# `gate.sh dogfood` and `gate.sh all`, which is lint / unit / e2e /
# gates / biz / dogfood / perf again — the same categories, on the same
# tree, a second and a third time. Nothing was learned by the repeat:
# the tree does not change between them, and the suite writes down what
# it found.
#
# A report only counts when its runid names HEAD's short SHA, so a
# report from the commit before this one does not let a release
# through. That is the whole safety argument: the evidence is tied to
# the exact tree it was gathered on.
set -euo pipefail
cd "$(dirname "$0")/.."

# `--short=7` exactly, because that is what the suite writes into
# the runid; `--short` alone gives eight here and matched nothing.
sha=$(git rev-parse --short=7 HEAD)
newest=""
for f in target/suite/report-prerelease-*-"$sha".json; do
    [ -e "$f" ] || continue
    if [ -z "$newest" ] || [ "$f" -nt "$newest" ]; then
        newest="$f"
    fi
done
if [ -z "$newest" ]; then
    echo "no prerelease report for HEAD ($sha)" >&2
    exit 1
fi
# Every step must have passed. `skipped` is not `pass`: a tier that
# skipped a step did not run it, which is the thing being claimed.
if grep -q '"status": "\(fail\|skipped\)"' "$newest"; then
    echo "prerelease report for $sha is not all-green: $newest" >&2
    grep -o '"name": "[^"]*", "status": "\(fail\|skipped\)"' "$newest" >&2 || true
    exit 1
fi
echo "$newest"
