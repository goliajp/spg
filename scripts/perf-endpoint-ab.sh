#!/usr/bin/env bash
# perf-endpoint-ab.sh — the endpoint panel, comparing TWO SPG binaries.
#
# `perf-endpoint-sweep.sh` answers "does this build lose to PG18". It
# cannot answer "did this change help", and round 936 found out the hard
# way what happens when it is made to: running the sweep once per binary
# puts one binary always second, and the machine drifts under it. That
# run's load climbed 1.35 -> 6.55 across the two legs, the later leg's
# own control reported a false difference, and two attempts disagreed on
# the direction. Round 659 had already recorded this bias from a
# different harness; a second harness grew it back.
#
# So the legs interleave HERE, at the cell level, with the starting leg
# rotating every round — the same discipline the ad-hoc A/B used to
# price round 935's change, made repeatable.
#
# Every run carries a CONTROL leg: binary A served a second time from a
# second process. Cells where the control differs from A are the run's
# own noise, and no verdict is worth more than that count.
#
# Inputs:
#   SPG_A_URI  — the baseline build (required)
#   SPG_B_URI  — the candidate build (required)
#   SPG_C_URI  — control: a SECOND server on binary A (required; the
#                point is to catch the harness inventing a difference)
#   PG_URI     — PG18, to keep both in context (optional)
#   N          — rounds per leg per cell (default 6; each is min-of-3)
#   SIZES      — row counts (default "10000 400000"; 1000 is below what
#                this testbed resolves and only adds runtime)
#   SHAPE_FILTER — run only shapes whose name contains this, so a single
#                cell can be re-run at a higher N without paying for the
#                other fifteen. A cell that the panel resolves by a hair
#                is exactly the one worth asking again.
#
# Exit 0 if the control found no false difference, 1 otherwise — the run
# is only readable when its own noise floor is clean.
set -u
cd "$(dirname "$0")/.."

A_URI="${SPG_A_URI:-}"
B_URI="${SPG_B_URI:-}"
C_URI="${SPG_C_URI:-}"
PG_URI="${PG_URI:-}"
N="${N:-6}"
SIZES="${SIZES:-10000 400000}"
PSQL="${PSQL:-psql}"

for v in A_URI B_URI C_URI; do
  [[ -n "${!v}" ]] || { echo "fatal: SPG_${v%_URI}_URI must be set" >&2; exit 2; }
done

echo "load before: $(uptime)"

# One sample = best of three executions in one session. Round 935: on
# this testbed single executions spread 9-25% at 400k and min-of-three
# spreads 1-2%, which is the difference between resolving a 10% change
# and not.
time_one() { # $1=uri $2=sql $3=setting
  "${PSQL}" --no-psqlrc -X -q -t -A "$1" -c "$3" -c '\timing on' -c "$2" -c "$2" -c "$2" 2>&1 |
    grep -E '^Time:' | sed 's/Time: //; s/ ms//' | sort -g | head -1
}
lo() { printf '%s\n' "$@" | sort -g | head -1; }
hi() { printf '%s\n' "$@" | sort -g | tail -1; }
spread() { awk -v a="$1" -v b="$2" 'BEGIN{ if (a+0 == 0) print 999; else printf "%.0f", (b-a)/a*100 }'; }

# Ranges that do not overlap are a difference; anything else is not one
# this run can see.
verdict() { # $1=amin $2=amax $3=bmin $4=bmax -- "is A slower than B"
  awk -v amin="$1" -v amax="$2" -v bmin="$3" -v bmax="$4" \
    'BEGIN { if (amin > bmax) print "SLOWER"; else if (amax < bmin) print "FASTER"; else print "unresolved" }'
}

# How far apart the two ranges actually are, as a percentage. Disjoint is
# not the same as far apart, and the difference matters: at N=6 this
# panel called the 400k top-N shape SLOWER on a gap of 0.4%, and at N=12
# the same pair of binaries came back unresolved. Six min-max samples can
# separate by a hair through luck alone, and the control leg does not
# catch that — it catches systematic bias, not a single cell's coin
# flip. Printing the gap makes a hair-thin verdict look different from a
# 10% one instead of identical.
gap_pct() { # $1=amin $2=amax $3=bmin $4=bmax
  awk -v amin="$1" -v amax="$2" -v bmin="$3" -v bmax="$4" \
    'BEGIN { if (amin > bmax) g = (amin - bmax) / bmax;
             else if (amax < bmin) g = (bmin - amax) / amax;
             else { print "-"; exit }
             printf "%.1f%%", g * 100 }'
}

setup_table() { # $1=uri $2=table $3=rows
  "${PSQL}" --no-psqlrc -X -q "$1" \
    -c "DROP TABLE IF EXISTS $2" \
    -c "CREATE TABLE $2 (id INT PRIMARY KEY, k INT, pad TEXT)" >/dev/null 2>&1
  "${PSQL}" --no-psqlrc -X -q "$1" \
    -c "INSERT INTO $2 SELECT g, ((g::bigint*7919)%$3)::int, repeat(chr(97+(g%26)),200) FROM generate_series(1,$3) g" >/dev/null 2>&1
  local got
  got="$("${PSQL}" --no-psqlrc -X -q -t -A "$1" -c "SELECT count(*) FROM $2")"
  [[ "${got}" == "$3" ]] || { echo "SETUP FAILED: $2 on $1 has ${got} rows, wanted $3" >&2; exit 2; }
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

FASTER=0; SLOWER=0; CONTROL_DIFFS=0; CELLS=0

printf '\n%-8s %-26s %-15s %-15s %-15s %-15s %-11s %-7s %s\n' \
  SIZE SHAPE 'A base' 'B cand' 'C control' 'PG18' 'B vs A' gap 'control'
printf '%-8s %-26s %-15s %-15s %-15s %-15s %-11s %-7s %s\n' \
  -------- -------------------------- --------------- --------------- --------------- --------------- ----------- ------- -------

for rows in ${SIZES}; do
  T="ab_${rows}"
  setup_table "${A_URI}" "${T}" "${rows}"
  setup_table "${B_URI}" "${T}" "${rows}"
  setup_table "${C_URI}" "${T}" "${rows}"
  [[ -n "${PG_URI}" ]] && setup_table "${PG_URI}" "${T}" "${rows}"

  for entry in "${SHAPES[@]}"; do
    name="${entry%%|*}"; sql="${entry#*|}"; sql="${sql//@T@/${T}}"
    [[ -n "${SHAPE_FILTER:-}" && "${name}" != *"${SHAPE_FILTER}"* ]] && continue
    a=(); b=(); c=(); p=()
    for ((i = 0; i < N; i++)); do
      # Rotate which leg goes first. With the legs in a fixed order the
      # last one absorbs whatever the machine drifted to during the
      # round, which is the bias this script exists to remove.
      for step in 0 1 2 3; do
        case $(( (i + step) % 4 )) in
          0) a+=("$(time_one "${A_URI}" "${sql}" "${SPG_WM}")") ;;
          1) b+=("$(time_one "${B_URI}" "${sql}" "${SPG_WM}")") ;;
          2) c+=("$(time_one "${C_URI}" "${sql}" "${SPG_WM}")") ;;
          3) [[ -n "${PG_URI}" ]] && p+=("$(time_one "${PG_URI}" "${sql}" "${PG_WM}")") ;;
        esac
      done
    done
    amin="$(lo "${a[@]}")"; amax="$(hi "${a[@]}")"
    bmin="$(lo "${b[@]}")"; bmax="$(hi "${b[@]}")"
    cmin="$(lo "${c[@]}")"; cmax="$(hi "${c[@]}")"
    if [[ ${#p[@]} -gt 0 ]]; then pmin="$(lo "${p[@]}")"; pmax="$(hi "${p[@]}")"; else pmin="-"; pmax="-"; fi

    v="$(verdict "${bmin}" "${bmax}" "${amin}" "${amax}")"
    cv="$(verdict "${cmin}" "${cmax}" "${amin}" "${amax}")"
    gp="$(gap_pct "${bmin}" "${bmax}" "${amin}" "${amax}")"
    sp="$(spread "${amin}" "${amax}")"
    # A baseline that cannot hold still resolves nothing, and a control
    # passes trivially at that spread (round 925).
    if (( sp > 20 )); then v="VOID(${sp}%)"; fi
    case "${v}" in
      FASTER) FASTER=$((FASTER + 1)) ;;
      SLOWER) SLOWER=$((SLOWER + 1)) ;;
    esac
    [[ "${cv}" != unresolved ]] && CONTROL_DIFFS=$((CONTROL_DIFFS + 1))
    CELLS=$((CELLS + 1))
    printf '%-8s %-26s %-15s %-15s %-15s %-15s %-11s %-7s %s\n' \
      "${rows}" "${name}" "${amin}-${amax}" "${bmin}-${bmax}" "${cmin}-${cmax}" "${pmin}-${pmax}" "${v}" "${gp}" "${cv}"
  done
done

echo
echo "load after: $(uptime)"
echo "cells=${CELLS} B_faster=${FASTER} B_slower=${SLOWER} control_false_differences=${CONTROL_DIFFS}"
if (( CONTROL_DIFFS > 0 )); then
  echo "WARNING: the control found ${CONTROL_DIFFS} difference(s) between a binary and itself."
  echo "         No verdict above is worth more than that. Re-run quieter or raise N."
  exit 1
fi
