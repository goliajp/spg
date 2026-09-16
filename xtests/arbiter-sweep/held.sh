#!/usr/bin/env bash
# A unique key written by a second session while the first holds it.
#
#   held.sh <pg-uri> <spg-uri>     exit 0 = every case ends as on PostgreSQL,
#                                  1 = one differs, 2 = the panel could not run
#
# 8.0.3 — a release gate (`write-arbitration`), at sentori's request (their
# §2 and §8.3, with their `held-transaction-probe.sh` as the model). 8.0.0
# to 8.0.2 let session B write a key session A had inserted and not yet
# committed; A — already told INSERT 0 1 — then failed at COMMIT with
# 40001. Nothing in the gates could see it: every other writer commits the
# statement it races, so no row is ever held uncommitted long enough.
#
# Session A inserts key 1 and holds its transaction for two seconds. Half
# a second in, B runs the case's statement. Each case records how A ended,
# how B ended (by SQLSTATE), whether B WAITED for A, and the rows left.
# Waiting is part of the answer: a B that does not wait can end with the
# same rows and still be wrong.
set -uo pipefail
PG=${1:?usage: $0 <pg-uri> <spg-uri>}
SPG=${2:?usage: $0 <pg-uri> <spg-uri>}
PSQL=${PSQL:-psql}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# case <uri> <setup> <A's end> <B's statement> -> one line of outcome
run_case() {
  local uri=$1 setup=$2 aend=$3 bstmt=$4
  local Q=("$PSQL" "$uri" --no-psqlrc -X -q -v ON_ERROR_STOP=1 -v VERBOSITY=verbose)
  "${Q[@]}" -c "DROP TABLE IF EXISTS held_probe" \
    -c "CREATE TABLE held_probe (k int UNIQUE, n int)" -c "$setup" >"$WORK/setup" 2>&1 || return 2
  ( "${Q[@]}" -c "BEGIN" -c "INSERT INTO held_probe VALUES (1, 1)" \
      -c "SELECT pg_sleep(2)" -c "$aend" >"$WORK/a" 2>&1; echo $? >"$WORK/a.rc" ) &
  sleep 0.5
  local t0 t1
  t0=$(python3 -c 'import time; print(time.time())')
  "${Q[@]}" -c "$bstmt" >"$WORK/b" 2>&1
  local brc=$?
  t1=$(python3 -c 'import time; print(time.time())')
  wait
  local waited
  waited=$(python3 -c "print('waited' if $t1 - $t0 >= 1.0 else 'no-wait')")
  local rows
  rows=$("$PSQL" "$uri" --no-psqlrc -X -tA -c "SELECT string_agg(k || ':' || n, ',' ORDER BY k, n) FROM held_probe")
  local astate="ok" bstate="ok"
  [ "$(cat "$WORK/a.rc")" = 0 ] || astate="error $(grep -o 'ERROR:  [0-9A-Z]\{5\}' "$WORK/a" | head -1 | cut -c9-)"
  [ "$brc" = 0 ] || bstate="error $(grep -o 'ERROR:  [0-9A-Z]\{5\}' "$WORK/b" | head -1 | cut -c9-)"
  echo "A=$astate B=$bstate $waited rows=${rows:-none}"
}

CASES=(
  "plain INSERT, A commits|SELECT 1|COMMIT|INSERT INTO held_probe VALUES (1, 2)"
  "DO NOTHING, A commits|SELECT 1|COMMIT|INSERT INTO held_probe VALUES (1, 2) ON CONFLICT (k) DO NOTHING"
  "DO NOTHING, A rolls back|SELECT 1|ROLLBACK|INSERT INTO held_probe VALUES (1, 2) ON CONFLICT (k) DO NOTHING"
  "DO UPDATE, A commits|SELECT 1|COMMIT|INSERT INTO held_probe VALUES (1, 2) ON CONFLICT (k) DO UPDATE SET n = EXCLUDED.n"
  "UPDATE onto the held key, A commits|INSERT INTO held_probe VALUES (2, 2)|COMMIT|UPDATE held_probe SET k = 1 WHERE k = 2"
)
MIN_CASES=5

fails=0
checked=0
for c in "${CASES[@]}"; do
  IFS='|' read -r name setup aend bstmt <<<"$c"
  want=$(run_case "$PG" "$setup" "$aend" "$bstmt") || { echo "✗ PostgreSQL could not run '$name'"; exit 2; }
  got=$(run_case "$SPG" "$setup" "$aend" "$bstmt") || { echo "✗ SPG could not set up '$name'"; exit 2; }
  checked=$((checked + 1))
  if [ "$want" = "$got" ]; then
    printf '  ✓ %s: %s\n' "$name" "$want"
  else
    fails=$((fails + 1))
    printf '  ✗ %s\n      PostgreSQL: %s\n      SPG       : %s\n' "$name" "$want" "$got"
  fi
done
[ "$checked" -ge "$MIN_CASES" ] || { echo "✗ ran $checked cases, expected $MIN_CASES"; exit 2; }
[ "$fails" = 0 ] || { echo "$fails of $checked held-transaction cases differ"; exit 1; }
echo "held=$checked diffs=0"
