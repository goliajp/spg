#!/usr/bin/env bash
# perf endpoint sweep — SPGS against PG18, one client, one window.
#
# Round 885 rewrote this. What it used to do, and why each part had to
# change, is worth keeping because the old shape is the shape a perf
# harness naturally grows into:
#
#   * It drove SPG with `spgctl query` and PG with `psql`, then compared
#     wall clock. That is `docs/BENCH_PROTOCOL.md` rule 1, and the rule
#     exists because the violation once reported SPG at 2.1x slower when
#     the real figure through one client was 1.15-1.35x — about 0.35 s of
#     the "loss" was the probe. The panel was measuring two CLIENTS.
#     SPG speaks the PG wire protocol, so both legs run psql now.
#
#   * It judged on the ratio of MEDIANS at a 1.05x threshold. The same
#     testbed has produced 8-23 differing cells from two runs of the SAME
#     binary (v7.37 audit, r648-r650), so a 1.05x median threshold
#     manufactures losses that no change can fix. Verdicts are now
#     non-overlapping ranges — `BENCH_PROTOCOL.md` rule 4 — and every run
#     carries a same-binary CONTROL whose differing-cell count IS the
#     run's resolution. Cells inside that resolution report `unresolved`,
#     not `tied`: the panel does not get to call something equal when it
#     cannot tell.
#
#   * It covered the dogfood corpus only. The whole ORDER BY surface was
#     outside it, which is how 29 losing cells out of 32 went unreported
#     until a one-off sweep found them (see
#     `.claude/state/orderby-perf-design-2026-08-09.md`). The shapes are
#     built in below and run at four sizes.
#
# Inputs:
#   PG_URI      — postgres://... for the PG18 leg (required)
#   SPG_URI     — postgres://... for the SPGS leg (required)
#   N           — timings per side per cell (default 5; rule 4 wants >= 3,
#                 and 3 has proved too few to separate 10% at this size)
#   CORPUS_DIR  — extra .sql endpoints to include (optional)
#   SIZES       — row counts for the built-in shapes (default "1000 10000 50000 400000")
#
# Exit 0 when no cell LOSES beyond the run's own resolution; 1 otherwise.
set -euo pipefail
cd "$(dirname "$0")/.."

PG_URI="${PG_URI:-}"
SPG_URI="${SPG_URI:-}"
N="${N:-5}"
SIZES="${SIZES:-1000 10000 50000 400000}"
CORPUS_DIR="${CORPUS_DIR:-}"
PSQL="${PSQL:-psql}"

[[ -n "${PG_URI}" ]]  || { echo "fatal: PG_URI must be set" >&2; exit 2; }
[[ -n "${SPG_URI}" ]] || { echo "fatal: SPG_URI must be set (both legs run psql — rule 1)" >&2; exit 2; }

echo "load before: $(uptime)"

# One client, both legs. `\timing` reports the round trip psql measures,
# which excludes process startup — that is the number to compare.
# Each sample is the best of three executions in one session, not one
# execution. Round 935 measured the difference this makes on this
# testbed, in the same window, on the same 400k shapes: a run whose
# samples were single executions carried a baseline spread of 9-25%,
# and one whose samples were min-of-three carried 1-2%. A panel that
# blocks the release cannot resolve a 10% regression at the former, and
# resolves it comfortably at the latter — that round's own 10% change
# had to be measured OUTSIDE this script for exactly that reason.
#
# The minimum is the right statistic here because the thing being
# compared is how long the work takes, and everything else the machine
# does can only add. Both legs are treated identically, so no warmth
# accrues to one side.
time_one() { # $1=uri $2=sql $3=work_mem setting
  "${PSQL}" --no-psqlrc -X -q -t -A "$1" -c "$3" -c '\timing on' -c "$2" -c "$2" -c "$2" 2>&1 |
    grep -E '^Time:' | sed 's/Time: //; s/ ms//' | sort -g | head -1
}
lo() { printf '%s\n' "$@" | sort -g | head -1; }
hi() { printf '%s\n' "$@" | sort -g | tail -1; }

# a-range strictly above b-range => LOSS; strictly below => win; else the
# panel cannot tell them apart at this resolution.
verdict() { # $1=amin $2=amax $3=bmin $4=bmax
  awk -v amin="$1" -v amax="$2" -v bmin="$3" -v bmax="$4" \
    'BEGIN { if (amin > bmax) print "LOSS"; else if (amax < bmin) print "win"; else print "unresolved" }'
}

setup_table() { # $1=uri $2=table $3=rows $4=work_mem-setting
  "${PSQL}" --no-psqlrc -X -q "$1" \
    -c "DROP TABLE IF EXISTS $2" \
    -c "CREATE TABLE $2 (id INT PRIMARY KEY, k INT, pad TEXT)" >/dev/null 2>&1
  "${PSQL}" --no-psqlrc -X -q "$1" \
    -c "INSERT INTO $2 SELECT g, ((g::bigint*7919)%$3)::int, repeat(chr(97+(g%26)),200) FROM generate_series(1,$3) g" >/dev/null 2>&1
  local got
  got="$("${PSQL}" --no-psqlrc -X -q -t -A "$1" -c "SELECT count(*) FROM $2")"
  # Rule 2: a timing read off an unverified table is not evidence.
  [[ "${got}" == "$3" ]] || { echo "SETUP FAILED: $2 on $1 has ${got} rows, wanted $3 — refusing to time" >&2; exit 2; }
}

SPG_WM='SET work_mem = 4096'
PG_WM="SET work_mem='4MB'"

SHAPES=(
  'wide, non-indexed key|SELECT pad FROM @T@ ORDER BY k'
  'narrow, non-indexed key|SELECT id FROM @T@ ORDER BY k'
  'indexed key|SELECT pad FROM @T@ ORDER BY id'
  'top-N LIMIT 10|SELECT pad FROM @T@ ORDER BY k LIMIT 10'
  'two keys|SELECT pad FROM @T@ ORDER BY k, id'
  'descending|SELECT pad FROM @T@ ORDER BY k DESC'
  'distinct then order|SELECT DISTINCT k FROM @T@ ORDER BY k'
  'filtered then order|SELECT pad FROM @T@ WHERE id % 3 = 0 ORDER BY k'
)

LOSSES=0; CELLS=0; CONTROL_DIFFS=0

printf '\n%-8s %-26s %-16s %-16s %s\n' SIZE SHAPE 'SPGS(min-max)' 'PG18(min-max)' VERDICT
printf '%-8s %-26s %-16s %-16s %s\n' -------- -------------------------- ---------------- ---------------- -------

for rows in ${SIZES}; do
  T="sweep_${rows}"
  setup_table "${SPG_URI}" "${T}" "${rows}"
  setup_table "${PG_URI}"  "${T}" "${rows}"

  for entry in "${SHAPES[@]}"; do
    name="${entry%%|*}"; sql="${entry#*|}"; sql="${sql//@T@/${T}}"
    s=(); g=()
    for ((i = 0; i < N; i++)); do
      # Rule 4: alternate, and flip which side starts each round.
      if (( i % 2 == 0 )); then
        s+=("$(time_one "${SPG_URI}" "${sql}" "${SPG_WM}")")
        g+=("$(time_one "${PG_URI}"  "${sql}" "${PG_WM}")")
      else
        g+=("$(time_one "${PG_URI}"  "${sql}" "${PG_WM}")")
        s+=("$(time_one "${SPG_URI}" "${sql}" "${SPG_WM}")")
      fi
    done
    smin="$(lo "${s[@]}")"; smax="$(hi "${s[@]}")"
    gmin="$(lo "${g[@]}")"; gmax="$(hi "${g[@]}")"
    v="$(verdict "${smin}" "${smax}" "${gmin}" "${gmax}")"
    [[ "${v}" == LOSS ]] && LOSSES=$((LOSSES + 1))
    CELLS=$((CELLS + 1))
    printf '%-8s %-26s %-16s %-16s %s\n' "${rows}" "${name}" "${smin}-${smax}" "${gmin}-${gmax}" "${v}"
  done
done

# The control: SPG against ITSELF, same binary, same window. Any cell it
# calls a difference is the panel's own noise, and that count is the
# resolution every verdict above has to be read against.
echo
echo "control — SPGS against itself, same binary (differing cells here are this run's noise floor):"
# The control runs on a table this sweep actually built — hardcoding a
# size meant the control silently queried a missing table whenever SIZES
# did not contain it, and a control that errors on every cell reports a
# clean noise floor it never measured.
CT="sweep_$(set -- ${SIZES}; echo "$1")"
for entry in "${SHAPES[@]}"; do
  name="${entry%%|*}"; sql="${entry#*|}"; sql="${sql//@T@/${CT}}"
  a=(); b=()
  for ((i = 0; i < N; i++)); do
    if (( i % 2 == 0 )); then
      a+=("$(time_one "${SPG_URI}" "${sql}" "${SPG_WM}")")
      b+=("$(time_one "${SPG_URI}" "${sql}" "${SPG_WM}")")
    else
      b+=("$(time_one "${SPG_URI}" "${sql}" "${SPG_WM}")")
      a+=("$(time_one "${SPG_URI}" "${sql}" "${SPG_WM}")")
    fi
  done
  cv="$(verdict "$(lo "${a[@]}")" "$(hi "${a[@]}")" "$(lo "${b[@]}")" "$(hi "${b[@]}")")"
  if [[ "${cv}" != unresolved ]]; then
    CONTROL_DIFFS=$((CONTROL_DIFFS + 1))
    printf '  %-26s %s  <- same binary, called %s\n' "${name}" "${cv}" "${cv}"
  fi
done

echo
echo "load after: $(uptime)"
echo "cells=${CELLS} losses=${LOSSES} control_false_differences=${CONTROL_DIFFS}"
if (( CONTROL_DIFFS > 0 )); then
  echo "WARNING: the control found ${CONTROL_DIFFS} difference(s) between a binary and itself."
  echo "         Every verdict above is only as good as that. Re-run on a quieter machine"
  echo "         or raise N before acting on any single cell."
fi
(( LOSSES == 0 )) || exit 1
