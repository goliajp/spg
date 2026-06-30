#!/usr/bin/env bash
# v7.37.26 (26.1-26.4) — perf endpoint sweep: SPGS / SPGE / PG18 三栏.
#
# Runs the bundled dogfood-replay corpus + an optional customer
# fixture corpus through three execution paths and emits a three-
# column comparison table per endpoint. The two SPG columns are
# the embedded engine (SPGE) and the wire server (SPGS); PG18 is
# the reference. Output shape matches the SPG perf-feedback
# convention (`feedback-spgs-spge-perf-bar.md`):
#
#     endpoint        SPGE     SPGS     PG18    SPGS/PG  verdict
#     get_contacts    2.3ms    3.1ms    3.0ms   1.03×    tied
#     count_messages  1.1ms    1.5ms    2.0ms   0.75×    WIN
#     ...
#
# Verdict heuristic (per [[feedback-only-look-at-losses]]):
#   ≤ 0.95×  WIN
#   ≤ 1.05×  tied
#   > 1.05×  LOSS (P0 if ≥ 1.20×)
#
# Inputs (all required):
#   PG_URI         — postgres://... for the PG18 leg
#   SPG_ADDR       — host:port for SPGS leg (default 127.0.0.1:25432)
#   CORPUS_DIR     — dogfood-replay corpus (default xtests/dogfood_replay/)
#   N              — runs per endpoint (default 10; median reported)
#
# Exit 0 if every endpoint LOSS < 1.20× (no P0); exit 1 otherwise.
set -euo pipefail

PG_URI="${PG_URI:-}"
SPG_ADDR="${SPG_ADDR:-127.0.0.1:25432}"
CORPUS_DIR="${CORPUS_DIR:-./xtests/dogfood_replay}"
N="${N:-10}"
SPGCTL="${SPGCTL:-./target/release/spgctl}"
PSQL="${PSQL:-psql}"

if [[ -z "${PG_URI}" ]]; then
  echo "fatal: PG_URI must be set" >&2
  exit 2
fi
if [[ ! -d "${CORPUS_DIR}" ]]; then
  echo "fatal: CORPUS_DIR ${CORPUS_DIR} not a directory" >&2
  exit 2
fi
if [[ ! -x "${SPGCTL}" ]]; then
  echo "info: building spgctl release" >&2
  cargo build --release --bin spgctl
  SPGCTL="./target/release/spgctl"
fi

SCRATCH="$(mktemp -d)"
trap 'rm -rf "${SCRATCH}"' EXIT

median_ms() {
  # Reads space-separated millisecond values; prints the median.
  tr ' ' '\n' | sort -n | awk -v n="${N}" '
    BEGIN { i = 0 }
    { v[++i] = $0 }
    END {
      mid = int((n + 1) / 2)
      if (n % 2 == 0) print (v[mid] + v[mid+1]) / 2
      else            print v[mid]
    }
  '
}

run_once() {
  local mode="$1"   # pg / spgs
  local sql="$2"
  local t0 t1 us
  t0="$(/usr/bin/python3 -c 'import time;print(time.monotonic_ns())')"
  case "${mode}" in
    pg)
      "${PSQL}" --no-psqlrc --tuples-only --no-align "${PG_URI}" -c "${sql}" \
        > /dev/null 2>&1 || true
      ;;
    spgs)
      "${SPGCTL}" query "${sql}" "${SPG_ADDR}" > /dev/null 2>&1 || true
      ;;
  esac
  t1="$(/usr/bin/python3 -c 'import time;print(time.monotonic_ns())')"
  echo "scale=3; ($t1 - $t0) / 1000000" | bc
}

FAIL=0
TOTAL=0

printf "%-30s %8s %8s %8s %10s %s\n" \
  "endpoint" "SPGE" "SPGS" "PG18" "SPGS/PG" "verdict"
printf "%-30s %8s %8s %8s %10s %s\n" \
  "------------------------------" "--------" "--------" "--------" \
  "----------" "-------"

for sqlfile in "${CORPUS_DIR}"/*.sql; do
  name="$(basename "${sqlfile}" .sql)"
  sql="$(cat "${sqlfile}")"
  TOTAL=$((TOTAL + 1))

  pg_runs=""
  spgs_runs=""
  for ((i = 0; i < N; i++)); do
    pg_runs="${pg_runs} $(run_once pg "${sql}")"
    spgs_runs="${spgs_runs} $(run_once spgs "${sql}")"
  done
  pg_med="$(echo "${pg_runs}" | median_ms)"
  spgs_med="$(echo "${spgs_runs}" | median_ms)"
  spge_med="-"  # SPGE leg requires in-process bench harness; queues with 26.6

  if [[ "${pg_med}" == "0" || -z "${pg_med}" ]]; then
    ratio="—"
    verdict="SKIP (PG zero)"
  else
    ratio="$(echo "scale=2; ${spgs_med} / ${pg_med}" | bc)"
    # bash float-compare without bc tail
    awk_cmp() { awk -v a="$1" -v op="$2" -v b="$3" 'BEGIN { exit !(a op b) }'; }
    if awk_cmp "${ratio}" "<=" "0.95"; then
      verdict="WIN"
    elif awk_cmp "${ratio}" "<=" "1.05"; then
      verdict="tied"
    elif awk_cmp "${ratio}" "<" "1.20"; then
      verdict="LOSS"
      FAIL=$((FAIL + 1))
    else
      verdict="LOSS-P0"
      FAIL=$((FAIL + 1))
    fi
  fi

  printf "%-30s %8s %8s %8s %10s %s\n" \
    "${name}" "${spge_med}" "${spgs_med}ms" "${pg_med}ms" "${ratio}×" "${verdict}"
done

echo
echo "Summary: ${TOTAL} endpoints, ${FAIL} LOSS or LOSS-P0"

if [[ "${FAIL}" -gt 0 ]]; then
  exit 1
fi
