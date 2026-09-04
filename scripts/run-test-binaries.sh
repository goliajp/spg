#!/usr/bin/env bash
# v7.39.12 — build one cargo selection, then run its harnesses directly.
#
# Every distinct `cargo test` target selection costs a full rebuild,
# because cargo resolves features over the selected members and a
# different member set is a different feature set. Measured on this
# workspace, same command, same tree:
#
#   after a DIFFERENT selection ran   358 s
#   after the SAME selection ran       11 s
#   the fifteen harnesses, by hand     11 s
#
# So the 347 s is cargo re-resolving and rebuilding, not the tests —
# they take ten seconds — and a tier that makes a dozen differently
# shaped cargo calls pays that over and over. This runs ONE selection
# and then executes what it produced.
#
# Usage: run-test-binaries.sh <label> [cargo test selection args...]
#   env: RUN_FILTER=<substring>   only harnesses whose name contains it
#        RUN_ENV="K=V K2=V2"      extra environment for each harness
#        RUN_ARGS="--ignored"     extra arguments for each harness
set -uo pipefail
cd "$(dirname "$0")/.."

label="${1:?usage: run-test-binaries.sh <label> [cargo args...]}"
shift

list=$(mktemp -t spg-testbins)
times=$(mktemp -t spg-testtimes)

# `--no-run` builds; the JSON stream names what it built and where the
# package lives, which is the working directory cargo would have used.
# v7.39.12 — the build's stderr is kept, because "the build failed" with
# the reason discarded is a message nobody can act on. It cost one
# release-gate run to read as an unexplained failure.
build_err=$(mktemp -t spg-testbuild)
trap 'rm -f "$list" "$times" "$build_err"' EXIT
if ! cargo test -q --locked "$@" --no-run --message-format=json 2>"$build_err" \
    | python3 -c '
import sys, json, os
seen = set()
for line in sys.stdin:
    if not line.startswith("{"):
        continue
    d = json.loads(line)
    exe = d.get("executable")
    if not exe or exe in seen:
        continue
    # Only harnesses. `--tests` also BUILDS the plain binaries an
    # integration test depends on — spg-server, spgctl, pg_isready —
    # and those carry an `executable` too. Running one of those with
    # `--quiet` runs its real `main`: "spg: unknown command: --quiet".
    if not d.get("profile", {}).get("test"):
        continue
    seen.add(exe)
    pkg_dir = os.path.dirname(d.get("manifest_path", ""))
    name = d["target"]["name"]
    pkg = d["package_id"].split("#")[0].rstrip("/").split("/")[-1]
    print(exe + "\t" + pkg_dir + "\t" + pkg + "::" + name)
' > "$list"; then
    echo "$label: the build failed" >&2
    cat "$build_err" >&2
    exit 1
fi

n=$(grep -c . "$list" || true)
[ "$n" -gt 0 ] || { echo "$label: the selection produced no test binaries" >&2; exit 1; }

pass=0; fail=0; tests=0; failed_names=""
while IFS=$'\t' read -r exe pkg name || [ -n "${exe:-}" ]; do
    [ -x "$exe" ] || continue
    # RUN_FILTER is a comma-separated list of substrings; a harness runs
    # when its <package>::<target> label contains any of them.
    if [ -n "${RUN_FILTER:-}" ]; then
        keep=0
        IFS=, read -r -a _wanted <<< "$RUN_FILTER"
        for w in "${_wanted[@]}"; do
            case "$name" in *"$w"*) keep=1; break ;; esac
        done
        [ "$keep" = 1 ] || continue
    fi
    # cargo runs a test binary with the package root as its working
    # directory, and fixtures are opened relative to it.
    started=$(date +%s)
    out=$(cd "$pkg" && env ${RUN_ENV:-} "$exe" --quiet ${RUN_ARGS:-} 2>&1)
    rc=$?
    # v7.39.12 — every harness records its own wall clock.
    #
    # The step-level number and the same work measured by hand have
    # disagreed by 4x, and six hypotheses about the difference were
    # each refuted by measurement. A per-harness row says whether the
    # gap is spread across all of them (the machine) or concentrated in
    # a few (those harnesses), which is the question the step-level
    # number cannot answer.
    #
    # v7.39.13 — and prints it as it goes. This file wrote the rows to a
    # temp file and showed them only in the summary, so a step that ran
    # 3,911 s printed one banner and then nothing for over an hour:
    # there was no way to see where it was without waiting for it to
    # end, and no record afterwards of when each harness started.
    printf '%6ss  %s\n' "$(( $(date +%s) - started ))" "$name" \
        | tee -a "$times"
    t=$(printf '%s' "$out" | grep -oE '[0-9]+ passed' | head -1 | cut -d' ' -f1)
    tests=$(( tests + ${t:-0} ))
    if [ "$rc" = 0 ]; then
        pass=$(( pass + 1 ))
    else
        fail=$(( fail + 1 ))
        failed_names="$failed_names $name"
        printf '%s\n' "$out" | tail -40 >&2
    fi
done < "$list"

echo "$label: $pass/$((pass+fail)) harnesses green, $tests tests"
# The five slowest, so a step's cost is attributable from its own log.
slow=$(sort -rn "$times" | head -5 | tr '\n' ';' | sed 's/;$//')
echo "$label: slowest —$slow"
[ "$fail" = 0 ] || { echo "$label: FAILED —$failed_names" >&2; exit 1; }
