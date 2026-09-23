#!/usr/bin/env bash
# sentori acceptance gate 5 — a backup that restores to the same data.
#
#   xtests/gates/g5-backup.sh <image> [port]
#
# Three ways to take one, each judged by comparing every public table's
# CONTENTS — the rows themselves, hashed — against the original. Row
# counts are not compared: they are the weakest evidence a restore can
# offer, and two of the defects this project shipped would have passed
# a count.
#
#   logical  pg_dump | psql into a second, empty server
#   cold     the server stopped, its data directory copied, a new
#            server started on the copy
#   self     the restored server's own dump equals the first dump
#            (the fixed point a dump has to reach to be a backup)
#
# `SELFTEST=1` adds the negative control: one row is changed in the
# restored copy and the comparison has to report that table.

GATES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
# shellcheck source=xtests/gates/lib.sh
. "$GATES_DIR/lib.sh"

IMAGE=${1:?usage: g5-backup.sh <image> [port]}
PORT=${2:-17511}
PORT2=$((PORT + 1))
SRC=g5bk-src; DST=g5bk-dst
VOL=g5bk-vol-$$; VOL2=g5bk-vol2-$$
DATA=$(datadir "$IMAGE")
WORK=$(mktemp -d)
cleanup() { docker rm -f "$SRC" "$DST" >/dev/null 2>&1; docker volume rm "$VOL" "$VOL2" >/dev/null 2>&1; rm -rf "$WORK"; }
trap cleanup EXIT

# `\restrict <token>` / `\unrestrict <token>`: pg_dump writes a fresh
# random token into every dump, so two dumps of the same database differ
# on those two lines and nothing else. Dropped here, or the fixed point
# below can never be reached.
pgdump() {
  docker run --rm --network host -e PGPASSWORD=p "$ORACLE" \
    pg_dump -h 127.0.0.1 -p "$1" -U u -d d "${@:2}" \
  | sed -E 's/^\\(un)?restrict .*/\\\1restrict <token>/'
}

# The tables whose hash differs between two fingerprints, or the word
# `SCHEMA` when the two do not even hold the same tables.
differing() {
  local a=$1 b=$2
  [ "$(cut -d' ' -f1 "$a")" = "$(cut -d' ' -f1 "$b")" ] || { echo SCHEMA; return; }
  join "$a" "$b" | awk '$2 != $3 { printf "%s ", $1 }'
}

echo "g5-backup: $IMAGE"
docker volume create "$VOL" >/dev/null
boot "$IMAGE" "$PORT" "$SRC" -v "$VOL:$DATA" || exit 2
load_schema "$PORT" || exit 2
seed "$PORT" 200 || exit 2
# Rows in every table the ingest path touches, so the comparison has
# something to be wrong about.
q "$PORT" -v ON_ERROR_STOP=1 -q <<'SQL' || exit 2
INSERT INTO issue_user_hits (issue_id, user_key, hit_count)
SELECT id, 'u' || (ord % 7), (ord % 5) + 1 FROM (SELECT id, row_number() OVER (ORDER BY id) AS ord FROM issues) s;
INSERT INTO events (id, project_id, issue_id, kind, platform, occurred_at, payload)
SELECT ('00000000-0000-0000-0003-' || lpad(ord::text, 12, '0'))::uuid,
       '00000000-0000-0000-0000-000000000001', id, 'error', 'ios', now(),
       ('{"n":' || ord || ',"t":"x' || ord || '"}')::jsonb
FROM (SELECT id, row_number() OVER (ORDER BY id) AS ord FROM issues) s;
UPDATE issues SET event_count = 1, users_count = 1;
SQL
fingerprint_db "$PORT" > "$WORK/orig"
for t in $(tables "$PORT"); do echo "$t $(q "$PORT" -tAc "SELECT count(*) FROM $t" < /dev/null)"; done > "$WORK/counts"
rows=$(awk 'END{print NR}' "$WORK/orig")
[ "${rows:-0}" -ge 20 ] || { echo "✗ fingerprinted only $rows tables"; exit 2; }
pgdump "$PORT" > "$WORK/dump1" 2> "$WORK/dump1.err"
[ -s "$WORK/dump1" ] || { echo "✗ pg_dump produced nothing: $(head -3 "$WORK/dump1.err")"; exit 1; }

rc=0
report() { # <name> <diff text>
  case "$1:$2" in
    *:) echo "  $1: ok" ;;
    *) echo "  $1: ✗ $2"; rc=1 ;;
  esac
}

# --- logical: dump, then restore into an empty server -----------------
docker volume create "$VOL2" >/dev/null
boot "$IMAGE" "$PORT2" "$DST" -v "$VOL2:$DATA" || exit 2
if ! docker run --rm -i --network host -e PGPASSWORD=p "$ORACLE" \
     psql -h 127.0.0.1 -p "$PORT2" -U u -d d -X -q -v ON_ERROR_STOP=1 -f - \
     < "$WORK/dump1" > "$WORK/restore.log" 2>&1; then
  echo "  logical: ✗ the dump does not restore: $(grep -m1 -i error "$WORK/restore.log")"
  rc=1
else
  fingerprint_db "$PORT2" > "$WORK/restored"
  report logical "$(differing "$WORK/orig" "$WORK/restored")"
  # And the restored server's own dump has to be the first dump again.
  pgdump "$PORT2" > "$WORK/dump2" 2>/dev/null
  if diff -q "$WORK/dump1" "$WORK/dump2" >/dev/null; then echo "  self: ok"
  else
    cp "$WORK/dump1" "$WORK/dump2" /tmp/ 2>/dev/null
    echo "  self: ✗ the restored server dumps something else:"
    diff "$WORK/dump1" "$WORK/dump2" | head -12 | sed 's/^/      /'
    rc=1
  fi

  if [ "${SELFTEST:-0}" = 1 ]; then
    q "$PORT2" -tAc "UPDATE issues SET group_title = 'changed' WHERE fingerprint = 'fp1'" >/dev/null
    fingerprint_db "$PORT2" > "$WORK/tampered"
    case "$(differing "$WORK/orig" "$WORK/tampered")" in
      *issues*) echo "  selftest: the comparison goes red on one changed row: ok" ;;
      *) echo "  selftest: ✗ the comparison stayed green on one changed row"; rc=1 ;;
    esac
  fi
fi
docker rm -f "$DST" >/dev/null 2>&1

# --- cold: stop, copy the data directory, start on the copy ----------
# A cold backup is taken with the server SHUT DOWN, so this asks for a
# clean stop and waits for it. (`docker rm -f` would be a kill, which
# g5-crash already covers.)
docker stop -t 60 "$SRC" >/dev/null 2>&1
docker rm -f "$SRC" >/dev/null 2>&1
docker volume rm "$VOL2" >/dev/null 2>&1
docker volume create "$VOL2" >/dev/null
docker run --rm -v "$VOL:/s" -v "$VOL2:/d" "$ORACLE" sh -c 'cp -a /s/. /d/' >/dev/null 2>&1 \
  || { echo "  cold: ✗ the data directory would not copy"; rc=1; }
if boot "$IMAGE" "$PORT2" "$DST" -v "$VOL2:$DATA"; then
  fingerprint_db "$PORT2" > "$WORK/cold"
  cold_diff=$(differing "$WORK/orig" "$WORK/cold")
  report cold "$cold_diff"
  for t in $cold_diff; do
    [ "$t" = SCHEMA ] && continue
    printf '      %s: %s rows on the original, %s on the copy\n' "$t" \
      "$(grep "^$t " "$WORK/counts" | cut -d' ' -f2)" \
      "$(q "$PORT2" -tAc "SELECT count(*) FROM $t" < /dev/null 2>/dev/null)"
  done
  w=$(q "$PORT2" -tAc "INSERT INTO issue_activity (id, issue_id, kind, body)
        SELECT gen_random_uuid(), id, 'note', '{}'::jsonb FROM issues LIMIT 1 RETURNING 1" 2>&1 | head -1)
  [ "$w" = "1" ] && echo "  cold writable: ok" || { echo "  cold writable: ✗ $w"; rc=1; }
else
  echo "  cold: ✗ a server on the copy never answered"; rc=1
fi

[ "$rc" = 0 ] && echo "g5-backup: $IMAGE PASS" || echo "g5-backup: $IMAGE FAIL"
exit "$rc"
