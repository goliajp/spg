#!/usr/bin/env bash
# dropin-perf — compare a drop-in candidate against a reference
# PostgreSQL image, on the same box, on YOUR statements.
#
#   xbench/dropin-perf/run.sh goliakk/spg:7.38.7
#   PROFILE=profiles/sentori xbench/dropin-perf/run.sh goliakk/spg:7.38.7 postgres:18-alpine
#
# Why this exists, and what it refuses to do.
#
# A benchmark that compares a build against the vendor's own previous
# build measures whether the vendor is moving. It says nothing about
# whether you should switch. The comparison that decides anything is
# against the image your compose ships today, on the box you have, on
# the statements you actually run — so that is the only comparison this
# harness knows how to make.
#
#   * Both databases run as containers on this box, started the same
#     way, seeded by the same SQL, in the same run.
#   * The legs INTERLEAVE at the cell level and the starting leg rotates
#     every round. Running all of A and then all of B puts one leg
#     always second and the machine drifts underneath it; we have twice
#     had that bias invent a result, once in each direction.
#   * Every run carries a CONTROL leg — the reference image served a
#     second time from a second container. Cells where the control
#     differs from the reference are this run's own noise, and no
#     verdict in the table is worth more than that count.
#   * Ranges that overlap are reported `unresolved`, never as a small
#     win or a small loss. Disjoint-but-adjacent prints its gap, because
#     a 0.4% separation and a 40% one should not read the same.
#
# Exit codes — 2 is reserved for the harness being broken, and is not
# reachable from the same path as a clean run:
#   0  ran, and the control found no false difference
#   1  ran, but the control found one — the box was too noisy to read
#   2  the harness could not measure: an image would not boot, a seed
#      landed the wrong row count, a profile is missing
#
# A profile is a directory:
#   schema.sql   DDL + seed, must be valid on both engines
#   shapes.tsv   one shape per line: name<TAB>SQL
#   rows         optional, the seed row count to assert (default: none)
set -u
cd "$(dirname "$0")"

CAND="${1:?usage: run.sh <candidate-image> [reference-image]}"
REF="${2:-postgres:18-alpine}"
PROFILE="${PROFILE:-profiles/sentori}"
N="${N:-6}"
PSQL="${PSQL:-psql}"
USER_NAME=bench
DB_NAME=bench
CAND_PORT="${CAND_PORT:-25601}"
REF_PORT="${REF_PORT:-25602}"
CTL_PORT="${CTL_PORT:-25603}"

[ -f "$PROFILE/schema.sql" ] || { echo "fatal: $PROFILE/schema.sql missing" >&2; exit 2; }
[ -f "$PROFILE/shapes.tsv" ] || { echo "fatal: $PROFILE/shapes.tsv missing" >&2; exit 2; }
command -v "$PSQL" >/dev/null || { echo "fatal: psql not found (set PSQL=)" >&2; exit 2; }
command -v docker >/dev/null || { echo "fatal: docker not found" >&2; exit 2; }

NAMES=(dropin_perf_cand dropin_perf_ref dropin_perf_ctl)
cleanup() { for c in "${NAMES[@]}"; do docker rm -f "$c" >/dev/null 2>&1; done; }
trap cleanup EXIT
cleanup

boot() { # $1=container $2=image $3=port
  local args=(-d --name "$1" -p "127.0.0.1:$3:5432")
  case "$2" in
    postgres:*) args+=(-e POSTGRES_USER="$USER_NAME" -e POSTGRES_PASSWORD="$USER_NAME"
                       -e POSTGRES_DB="$DB_NAME" "$2") ;;
    *)          args+=("$2") ;;   # spg listens on 5432 with no bootstrap env
  esac
  docker run "${args[@]}" >/dev/null 2>&1 || { echo "fatal: $2 would not start" >&2; exit 2; }
}

uri() { echo "postgres://$USER_NAME:$USER_NAME@127.0.0.1:$1/$DB_NAME"; }

wait_up() { # $1=uri $2=label
  local i
  for ((i = 0; i < 120; i++)); do
    "$PSQL" --no-psqlrc -X -q -t -A "$1" -c 'SELECT 1' >/dev/null 2>&1 && return 0
    sleep 1
  done
  echo "fatal: $2 never answered SELECT 1" >&2; exit 2
}

seed() { # $1=uri $2=label
  "$PSQL" --no-psqlrc -X -q -v ON_ERROR_STOP=1 "$1" -f "$PROFILE/schema.sql" >/dev/null 2>&1 \
    || { echo "fatal: seed failed on $2 — run it by hand against $1" >&2; exit 2; }
  if [ -f "$PROFILE/rows" ]; then
    local want got
    while read -r tbl want; do
      [ -n "$tbl" ] || continue
      got="$("$PSQL" --no-psqlrc -X -q -t -A "$1" -c "SELECT count(*) FROM $tbl" 2>/dev/null)"
      [ "$got" = "$want" ] || {
        echo "fatal: $2 seeded $tbl with $got rows, wanted $want" >&2; exit 2; }
    done < "$PROFILE/rows"
  fi
}

# One sample = best of three executions in one session. Single
# executions on a laptop spread 9-25%; min-of-three spreads 1-2%, which
# is the difference between resolving a 10% change and not.
time_one() { # $1=uri $2=sql
  "$PSQL" --no-psqlrc -X -q -t -A "$1" -c '\timing on' -c "$2" -c "$2" -c "$2" 2>&1 |
    grep -E '^Time:' | sed 's/Time: //; s/ ms//' | sort -g | head -1
}
lo() { printf '%s\n' "$@" | sort -g | head -1; }
hi() { printf '%s\n' "$@" | sort -g | tail -1; }
verdict() { # amin amax bmin bmax -- is the CANDIDATE slower than the reference
  awk -v amin="$1" -v amax="$2" -v bmin="$3" -v bmax="$4" \
    'BEGIN { if (amin > bmax) print "SLOWER"; else if (amax < bmin) print "FASTER"; else print "unresolved" }'
}
gap_pct() {
  awk -v amin="$1" -v amax="$2" -v bmin="$3" -v bmax="$4" \
    'BEGIN { if (amin > bmax) g = (amin - bmax) / bmax;
             else if (amax < bmin) g = (bmin - amax) / amax;
             else { print "-"; exit }
             printf "%.1f%%", g * 100 }'
}

echo "profile:   $PROFILE"
echo "candidate: $CAND"
echo "reference: $REF  (control = a second container on the same image)"
echo "rounds:    N=$N, each sample min-of-3, legs interleaved with a rotating start"
echo "load before: $(uptime)"

boot "${NAMES[0]}" "$CAND" "$CAND_PORT"
boot "${NAMES[1]}" "$REF"  "$REF_PORT"
boot "${NAMES[2]}" "$REF"  "$CTL_PORT"
A_URI="$(uri "$CAND_PORT")"; B_URI="$(uri "$REF_PORT")"; C_URI="$(uri "$CTL_PORT")"
wait_up "$A_URI" "$CAND"; wait_up "$B_URI" "$REF"; wait_up "$C_URI" "$REF (control)"
seed "$A_URI" "$CAND"; seed "$B_URI" "$REF"; seed "$C_URI" "$REF (control)"

printf '\n%-34s %-17s %-17s %-17s %-11s %-7s %s\n' \
  SHAPE 'candidate' 'reference' 'control' 'verdict' gap control
printf '%-34s %-17s %-17s %-17s %-11s %-7s %s\n' \
  ---------------------------------- ----------------- ----------------- ----------------- ----------- ------- -------

CELLS=0; SLOWER=0; FASTER=0; CONTROL_DIFFS=0
while IFS=$'\t' read -r name sql; do
  case "$name" in ''|'#'*) continue ;; esac
  a=(); b=(); c=()
  for ((i = 0; i < N; i++)); do
    for step in 0 1 2; do
      case $(( (i + step) % 3 )) in
        0) a+=("$(time_one "$A_URI" "$sql")") ;;
        1) b+=("$(time_one "$B_URI" "$sql")") ;;
        2) c+=("$(time_one "$C_URI" "$sql")") ;;
      esac
    done
  done
  amin="$(lo "${a[@]}")"; amax="$(hi "${a[@]}")"
  bmin="$(lo "${b[@]}")"; bmax="$(hi "${b[@]}")"
  cmin="$(lo "${c[@]}")"; cmax="$(hi "${c[@]}")"
  v="$(verdict "$amin" "$amax" "$bmin" "$bmax")"
  g="$(gap_pct "$amin" "$amax" "$bmin" "$bmax")"
  ctl="$(verdict "$cmin" "$cmax" "$bmin" "$bmax")"
  [ "$ctl" = unresolved ] && ctl=clean || CONTROL_DIFFS=$((CONTROL_DIFFS + 1))
  [ "$v" = SLOWER ] && SLOWER=$((SLOWER + 1))
  [ "$v" = FASTER ] && FASTER=$((FASTER + 1))
  CELLS=$((CELLS + 1))
  printf '%-34s %-17s %-17s %-17s %-11s %-7s %s\n' \
    "$name" "$amin-$amax" "$bmin-$bmax" "$cmin-$cmax" "$v" "$g" "$ctl"
done < "$PROFILE/shapes.tsv"

echo
echo "load after: $(uptime)"
echo "cells=$CELLS candidate_slower=$SLOWER candidate_faster=$FASTER control_false_differences=$CONTROL_DIFFS"
if [ "$CONTROL_DIFFS" -gt 0 ]; then
  echo "The control leg — the reference image compared against itself — reported"
  echo "$CONTROL_DIFFS difference(s). The box moved under the run; no verdict above is"
  echo "worth more than that. Re-run on a quiet machine before believing any cell."
  exit 1
fi
exit 0
