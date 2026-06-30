#!/usr/bin/env bash
# v7.37.24 (24.4) — PG 18 builtin function coverage audit.
#
# Compares the function name set SPG exposes via pg_catalog.pg_proc
# against the canonical PG 18 builtin list (pg_proc.dat from the
# postgres source tree). Reports three buckets:
#
#   COVERED    — function name in both PG 18 and SPG
#   MISSING    — function name in PG 18, not in SPG (24.5 work queue)
#   SPG_EXTRA  — function name in SPG, not in PG 18 (spg_* / SPG+)
#
# Exit code 0 always; this is a coverage report, not a gate. Pipe to
# `wc -l` per section to track delta release-to-release.
#
# Inputs (all required):
#   SPG_ADDR      — server addr (default 127.0.0.1:25432)
#   PG_PROC_DAT   — local path to PG 18 src/include/catalog/pg_proc.dat
#                   (clone postgres-pg18 tag once; this script does not
#                   network-fetch)
#
# Usage:
#   PG_PROC_DAT=/path/to/postgres/src/include/catalog/pg_proc.dat \
#     scripts/audit-pg-builtins.sh
#
# Or with a non-default server:
#   SPG_ADDR=127.0.0.1:25500 PG_PROC_DAT=... scripts/audit-pg-builtins.sh
set -euo pipefail

SPG_ADDR="${SPG_ADDR:-127.0.0.1:25432}"
if [[ -z "${PG_PROC_DAT:-}" || ! -r "${PG_PROC_DAT}" ]]; then
  echo "fatal: PG_PROC_DAT must point to a readable pg_proc.dat" >&2
  echo "       (clone postgres at tag REL_18_0 and pass" >&2
  echo "        src/include/catalog/pg_proc.dat)" >&2
  exit 2
fi

SCRATCH="$(mktemp -d)"
trap 'rm -rf "${SCRATCH}"' EXIT

# --- 1. Extract proname set from PG 18 pg_proc.dat -------------------
# pg_proc.dat is a perl-like data file; each entry has `proname => 'X'`.
# Strip duplicates (PG overloads share a name) so we count by NAME.
grep -E "^[[:space:]]+\{ oid" "${PG_PROC_DAT}" \
  | grep -oE "proname => '[^']+'" \
  | sed -E "s/proname => '([^']+)'/\1/" \
  | sort -u > "${SCRATCH}/pg.names"

PG_COUNT="$(wc -l < "${SCRATCH}/pg.names" | tr -d ' ')"

# --- 2. Extract proname set from SPG via pg_catalog.pg_proc ----------
SPGCTL="${SPGCTL:-./target/release/spgctl}"
if [[ ! -x "${SPGCTL}" ]]; then
  SPGCTL="cargo run --release --quiet --bin spgctl --"
fi

# `spg query` prints the result table. The proname column is index 1
# (after the header + separator + summary line). Stream-parse with awk.
${SPGCTL} query \
  "SELECT proname FROM pg_catalog.pg_proc ORDER BY proname" \
  "${SPG_ADDR}" \
  | awk 'NR > 2 && !/^\([0-9]+ row/ { gsub(/^ *| *$/, "", $0); if ($0 != "" && $0 != "proname") print }' \
  | sort -u > "${SCRATCH}/spg.names"

SPG_COUNT="$(wc -l < "${SCRATCH}/spg.names" | tr -d ' ')"

# --- 3. Bucket diff ---------------------------------------------------
comm -12 "${SCRATCH}/pg.names" "${SCRATCH}/spg.names" > "${SCRATCH}/covered"
comm -23 "${SCRATCH}/pg.names" "${SCRATCH}/spg.names" > "${SCRATCH}/missing"
comm -13 "${SCRATCH}/pg.names" "${SCRATCH}/spg.names" > "${SCRATCH}/extra"

COVERED="$(wc -l < "${SCRATCH}/covered" | tr -d ' ')"
MISSING="$(wc -l < "${SCRATCH}/missing" | tr -d ' ')"
EXTRA="$(wc -l < "${SCRATCH}/extra" | tr -d ' ')"
COVERAGE_PCT="$(( COVERED * 100 / PG_COUNT ))"

# --- 4. Report --------------------------------------------------------
cat <<EOF
# PG 18 builtin function coverage audit (v7.37.24.4)

PG 18 distinct proname:    ${PG_COUNT}
SPG distinct proname:      ${SPG_COUNT}
Covered (intersection):    ${COVERED}  (${COVERAGE_PCT}%)
Missing (PG 18 only):      ${MISSING}
SPG-extra (SPG only):      ${EXTRA}

## Missing (24.5 work queue)
EOF
sed 's/^/  - /' "${SCRATCH}/missing"

cat <<'EOF'

## SPG-extra
EOF
sed 's/^/  - /' "${SCRATCH}/extra"
