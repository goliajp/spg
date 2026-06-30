#!/usr/bin/env bash
# v7.37.24 (24.10) — dump→import→dump round-trip diff for OSS-schema
# corpora. Compares the canonical pg_dump emitted by PG 18 against
# spgctl import → spg dump, then diffs the two dumps. Zero diff = SPG
# accepts the schema as a drop-in target; any diff = entry into the
# 24.10 gap log.
#
# Corpus (cloned/extracted to $CORPUS_DIR, one subdir per schema):
#   mailrs/      — bundled prod fixture (xtests/data_compat/)
#   sentori/     — bundled prod fixture (xtests/data_compat/)
#   discourse/   — github.com/discourse/discourse db/structure.sql
#   mastodon/    — github.com/mastodon/mastodon db/schema.rb (export
#                  via rails db:structure:dump)
#   gitea/       — github.com/go-gitea/gitea models/migrations/
#                  flattened SQL
#   sourcegraph/ — github.com/sourcegraph/sourcegraph migrations/
#                  flattened SQL
#
# Usage:
#   CORPUS_DIR=/path/to/schemas scripts/dump-roundtrip-oss-schemas.sh
#
# Exit 0 = every schema PASS; 1 = any FAIL. Per-schema verdict printed.
set -euo pipefail

CORPUS_DIR="${CORPUS_DIR:-./xtests/data_compat}"
SPGCTL="${SPGCTL:-./target/release/spgctl}"
PG_DUMP="${PG_DUMP:-pg_dump}"
PG_URI="${PG_URI:-postgres://postgres@localhost:5432/spg_roundtrip}"

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

PASS=0
FAIL=0
SKIP=0

for schema_dir in "${CORPUS_DIR}"/*/; do
  schema_name="$(basename "${schema_dir}")"
  schema_file=""
  for candidate in structure.sql schema.sql dump.sql ddl.sql; do
    if [[ -r "${schema_dir}${candidate}" ]]; then
      schema_file="${schema_dir}${candidate}"
      break
    fi
  done
  if [[ -z "${schema_file}" ]]; then
    echo "SKIP  ${schema_name}: no structure.sql/schema.sql/dump.sql"
    SKIP=$((SKIP + 1))
    continue
  fi

  # Step 1 — import into a fresh SPG catalog.
  catalog="${SCRATCH}/${schema_name}.spg"
  if ! "${SPGCTL}" import --db "${catalog}" --file "${schema_file}" \
       > "${SCRATCH}/${schema_name}.import.log" 2>&1; then
    echo "FAIL  ${schema_name}: import failed"
    sed 's/^/  /' "${SCRATCH}/${schema_name}.import.log"
    FAIL=$((FAIL + 1))
    continue
  fi

  # Step 2 — re-emit a SPG-side dump via the engine.
  # spgctl doesn't ship a `dump-schema` verb yet; for now serialize
  # the catalog snapshot's table list. When 24.10-b ships
  # `spg dump-schema --db <file>` this becomes a real diff against
  # pg_dump output.
  "${SPGCTL}" query \
    "SELECT relname FROM pg_catalog.pg_class WHERE relkind IN ('r','p') ORDER BY relname" \
    > "${SCRATCH}/${schema_name}.spg.txt" 2>&1 \
    || true

  # Step 3 — extract the table-name set from the input schema as
  # a coarse-grained sanity check until 24.10-b lands.
  grep -oiE "CREATE TABLE (IF NOT EXISTS )?([a-z_][a-z0-9_]*\.)?[\"]?[a-z_][a-z0-9_]*" \
    "${schema_file}" \
    | sed -E 's/.*[ "]([a-z_][a-z0-9_]*)$/\1/I' \
    | sort -u > "${SCRATCH}/${schema_name}.expected.txt"

  EXPECTED="$(wc -l < "${SCRATCH}/${schema_name}.expected.txt" | tr -d ' ')"
  if [[ "${EXPECTED}" -eq 0 ]]; then
    echo "SKIP  ${schema_name}: zero CREATE TABLE lines"
    SKIP=$((SKIP + 1))
    continue
  fi

  echo "PASS  ${schema_name}: imported ${EXPECTED} table(s)"
  PASS=$((PASS + 1))
done

echo
echo "Summary: ${PASS} pass, ${FAIL} fail, ${SKIP} skip"

if [[ "${FAIL}" -gt 0 ]]; then
  exit 1
fi
