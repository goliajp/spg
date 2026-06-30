#!/usr/bin/env bash
# v7.37.24 (24.11) — diff SPG result vs PG 18 result for the same SQL.
#
# Runs the same query against PG (via psql) and SPG (via spgctl) and
# diffs the stdout. Designed for ad-hoc gap finding: pick a SQL that
# matters in your dogfood workload, run this against both endpoints,
# inspect the first diff.
#
# This is a SPG-side `EXPLAIN ANALYZE`-class tool, NOT a benchmark:
# we don't measure timing, we don't normalize whitespace, we don't
# strip transient columns (queryid, last_call_time, etc.). Diff is
# raw — a column-order or formatting difference IS the signal.
#
# Usage:
#   PG_URI=postgres://...  SPG_ADDR=127.0.0.1:25432 \
#     scripts/diff-with-pg.sh --sql "SELECT count(*) FROM t"
#
# Or batch from a file (one SQL per line, blank lines + # comments ignored):
#   scripts/diff-with-pg.sh --file queries.txt
#
# Exit 0 = zero diffs across every SQL; non-zero = any SQL diffed.
set -euo pipefail

SPG_ADDR="${SPG_ADDR:-127.0.0.1:25432}"
SPGCTL="${SPGCTL:-./target/release/spgctl}"
PSQL="${PSQL:-psql}"

SQL_INLINE=""
SQL_FILE=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --sql)   SQL_INLINE="$2"; shift 2 ;;
    --file)  SQL_FILE="$2"; shift 2 ;;
    -h|--help)
      sed -n '2,/^set/p' "$0" | head -25
      exit 0
      ;;
    *)
      echo "fatal: unknown arg $1" >&2
      exit 2
      ;;
  esac
done

if [[ -z "${PG_URI:-}" ]]; then
  echo "fatal: PG_URI must be set" >&2
  exit 2
fi
if [[ -z "${SQL_INLINE}" && -z "${SQL_FILE}" ]]; then
  echo "fatal: --sql or --file required" >&2
  exit 2
fi
if [[ ! -x "${SPGCTL}" ]]; then
  echo "info: building spgctl release" >&2
  cargo build --release --bin spgctl
  SPGCTL="./target/release/spgctl"
fi

SCRATCH="$(mktemp -d)"
trap 'rm -rf "${SCRATCH}"' EXIT

TOTAL=0
DIFFED=0

run_one() {
  local sql="$1"
  local pg_out="${SCRATCH}/pg.$$.txt"
  local spg_out="${SCRATCH}/spg.$$.txt"

  "${PSQL}" --no-psqlrc --tuples-only --no-align \
      "${PG_URI}" -c "${sql}" > "${pg_out}" 2>&1 || true

  "${SPGCTL}" query "${sql}" "${SPG_ADDR}" > "${spg_out}" 2>&1 || true

  # spgctl wraps output in a table; PG with -t -A is bare tuple per
  # line. Normalize spgctl output to bare tuples for comparison.
  local spg_bare="${SCRATCH}/spg.bare.$$.txt"
  awk 'NR > 2 && !/^\([0-9]+ row/ { print }' "${spg_out}" > "${spg_bare}"

  if diff -u "${pg_out}" "${spg_bare}" > "${SCRATCH}/diff.$$" 2>&1; then
    echo "PASS  ${sql}"
  else
    DIFFED=$((DIFFED + 1))
    echo "DIFF  ${sql}"
    sed 's/^/  /' "${SCRATCH}/diff.$$"
  fi
  TOTAL=$((TOTAL + 1))
}

if [[ -n "${SQL_INLINE}" ]]; then
  run_one "${SQL_INLINE}"
fi
if [[ -n "${SQL_FILE}" ]]; then
  while IFS= read -r line; do
    [[ -z "${line}" || "${line}" =~ ^# ]] && continue
    run_one "${line}"
  done < "${SQL_FILE}"
fi

echo
echo "Summary: ${TOTAL} queries, ${DIFFED} diffed"

if [[ "${DIFFED}" -gt 0 ]]; then
  exit 1
fi
