#!/usr/bin/env bash
# v7.37.23 (23.10) — one-shot pg_dump → spgctl import migration.
#
# Pipes pg_dump from a live PG instance straight into a fresh SPG
# catalog file. Runs the existing G4 dump-compat path end-to-end on
# real schema instead of a fixture; no PG↔SPG side-by-side server.
#
# This is the operator-side companion to the bundled
# `scripts/dropin-acceptance.sh` G4 dump-compat harness; that harness
# uses pinned fixtures, this script uses whatever PG database the
# operator points it at.
#
# Usage:
#   PG_URI=postgres://user@host:5432/dbname \
#     scripts/migrate-from-pg.sh --db /tmp/migrated.spg
#
# Optional flags:
#   --schema-only             pass --schema-only to pg_dump (no data)
#   --data-only               pass --data-only to pg_dump
#   --exclude-table <pat>     pg_dump --exclude-table=PATTERN
#   --keep-dump <path>        save the pg_dump output for re-import
#                             without re-hitting the source PG
#
# Exit 0 = catalog written + spgctl confirms table count; non-zero
# = pg_dump or import failure with reason on stderr.
set -euo pipefail

DB_OUT=""
KEEP_DUMP=""
PGDUMP_EXTRA=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --db)             DB_OUT="$2"; shift 2 ;;
    --schema-only)    PGDUMP_EXTRA+=(--schema-only); shift ;;
    --data-only)      PGDUMP_EXTRA+=(--data-only); shift ;;
    --exclude-table)  PGDUMP_EXTRA+=(--exclude-table="$2"); shift 2 ;;
    --keep-dump)      KEEP_DUMP="$2"; shift 2 ;;
    -h|--help)
      sed -n '2,/^set -euo/p' "$0" | sed -n '1,/^set/p' | head -25
      exit 0
      ;;
    *)
      echo "fatal: unknown arg $1" >&2
      exit 2
      ;;
  esac
done

if [[ -z "${PG_URI:-}" ]]; then
  echo "fatal: PG_URI must be set (postgres://...)" >&2
  exit 2
fi
if [[ -z "${DB_OUT}" ]]; then
  echo "fatal: --db <path> required" >&2
  exit 2
fi

SPGCTL="${SPGCTL:-./target/release/spgctl}"
PG_DUMP="${PG_DUMP:-pg_dump}"

if [[ ! -x "${SPGCTL}" ]]; then
  echo "info: building spgctl release" >&2
  cargo build --release --bin spgctl
  SPGCTL="./target/release/spgctl"
fi

if [[ -e "${DB_OUT}" ]]; then
  echo "fatal: ${DB_OUT} already exists; pick a fresh path or remove it first" >&2
  exit 2
fi

SCRATCH="$(mktemp -d)"
trap 'rm -rf "${SCRATCH}"' EXIT

DUMP_PATH="${SCRATCH}/source.sql"
if [[ -n "${KEEP_DUMP}" ]]; then
  DUMP_PATH="${KEEP_DUMP}"
fi

echo "[1/2] pg_dump → ${DUMP_PATH}" >&2
"${PG_DUMP}" "${PGDUMP_EXTRA[@]}" --no-owner --no-acl \
    --format=plain "${PG_URI}" > "${DUMP_PATH}"

DUMP_BYTES="$(wc -c < "${DUMP_PATH}" | tr -d ' ')"
DUMP_STMTS="$(grep -cE "^(CREATE|ALTER|INSERT|COPY|SELECT pg_catalog\.setval)" "${DUMP_PATH}" || true)"
echo "      ${DUMP_BYTES} bytes, ~${DUMP_STMTS} top-level statements" >&2

echo "[2/2] spgctl import → ${DB_OUT}" >&2
"${SPGCTL}" import --db "${DB_OUT}" --file "${DUMP_PATH}"

echo
echo "migration complete:"
echo "  source:  ${PG_URI}"
echo "  catalog: ${DB_OUT}"
echo "  dump:    ${DUMP_PATH}$([[ -z "${KEEP_DUMP}" ]] && echo " (removed)")"
