#!/usr/bin/env bash
# Capture a data-directory fixture for the ironrules gate (S3.2).
#
# The gate (`ironrules_full` in xtests/suitelib/src/steps.rs) opens the
# PREVIOUS release's directory with the CURRENT binary and verifies it
# row for row. So a fixture must be written by its own tag's binary,
# over the wire, in the shape the gate direct-opens: statement WAL with
# no db file, because replay IS the open.
#
#   scripts/capture-datadir-fixture.sh 7.38.7
#
# Writes xtests/compat-datadirs/v<VER>/{audit,wal,wal.cluster_id,expected.txt}.
# Older directories stay on disk beside it.
set -euo pipefail
VER="${1:?usage: capture-datadir-fixture.sh X.Y.Z}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/spg-server"
OUT="$ROOT/xtests/compat-datadirs/v$VER"
PORT="${PORT:-27438}"
PSQL="${PSQL:-psql}"

# NB: no `grep -q` here — under `pipefail` its early exit SIGPIPEs
# `strings` and the guard then reports a version mismatch that is not one.
if [ "$(strings "$BIN" | grep -c "pg_version=$VER")" -eq 0 ]; then
  echo "capture: $BIN is not $VER — build the tag first" >&2; exit 1
fi

WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT
SPG_PG_ADDR="127.0.0.1:$PORT" "$BIN" 127.0.0.1:$((PORT+1)) \
  "$WORK/db" "$WORK/audit" "$WORK/wal" >"$WORK/server.log" 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null || true; rm -rf "$WORK"' EXIT
URI="postgres://suite:suite@127.0.0.1:$PORT/suite"
for _ in $(seq 60); do "$PSQL" "$URI" -tAc 'SELECT 1' >/dev/null 2>&1 && break; sleep 0.5; done
"$PSQL" "$URI" -tAc 'SELECT 1' >/dev/null

# 500 rows across nine scalar types, two secondary indexes, then
# updates and deletes down to 490 live rows, plus an empty table.
"$PSQL" -v ON_ERROR_STOP=1 "$URI" >/dev/null <<'SQL'
CREATE TABLE fx_scalars (
  id        int PRIMARY KEY,
  t         text,
  n         bigint,
  d         double precision,
  b         boolean,
  ts        timestamp,
  dt        date,
  num       numeric,
  u         uuid
);
CREATE INDEX fx_scalars_n  ON fx_scalars (n);
CREATE INDEX fx_scalars_dt ON fx_scalars (dt);
CREATE TABLE fx_empty (id int PRIMARY KEY, t text);
INSERT INTO fx_scalars
SELECT g,
       'row-' || g,
       g * 1000,
       g / 7.0,
       (g % 3 = 0),
       timestamp '2026-01-01 00:00:00' + (g || ' minutes')::interval,
       date '2026-01-01' + g,
       (g::numeric / 3),
       ('00000000-0000-4000-8000-' || lpad(g::text, 12, '0'))::uuid
FROM generate_series(1, 500) g;
UPDATE fx_scalars SET t = 't-' || id WHERE id % 5 = 0;
DELETE FROM fx_scalars WHERE id > 490;
SQL

mkdir -p "$OUT"
{
  echo "fx_scalars $("$PSQL" "$URI" -tAc 'SELECT count(*) FROM fx_scalars')"
  echo "fx_empty $("$PSQL" "$URI" -tAc 'SELECT count(*) FROM fx_empty')"
  echo "checksum $("$PSQL" "$URI" -tAc "SELECT md5(string_agg(t, ',' ORDER BY id)) FROM fx_scalars")"
} > "$OUT/expected.txt"

kill $SRV; wait $SRV 2>/dev/null || true
cp "$WORK/audit" "$WORK/wal" "$WORK/wal.cluster_id" "$OUT/"
echo "captured $OUT"
cat "$OUT/expected.txt"
