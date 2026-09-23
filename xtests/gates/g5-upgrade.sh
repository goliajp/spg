#!/usr/bin/env bash
# sentori acceptance gate 5 — an old data directory opens in the new
# image, unchanged.
#
#   xtests/gates/g5-upgrade.sh <new image> [old image…]
#
# For each old image: build sentori's schema and rows with it, stop it,
# start the NEW image on the same volume, and ask three things —
#
#   same      every public table hashes to what the old image held
#   writable  the new image takes a row
#   dump      pg_dump still runs and restores (gate 5's other half)
#
# A migration that does not load on an old image is a finding about
# that image, reported as `schema`, not a harness failure.
#
# `SELFTEST=1` adds the negative control: the comparison is run once
# against a deliberately changed row and has to report it.

GATES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
# shellcheck source=xtests/gates/lib.sh
. "$GATES_DIR/lib.sh"

NEW=${1:?usage: g5-upgrade.sh <new image> [old image…]}
shift
OLDS=("$@")
[ ${#OLDS[@]} -gt 0 ] || OLDS=(goliakk/spg:8.0.4 goliakk/spg:9.0.0 goliakk/spg:9.0.1 goliakk/spg:9.0.2 goliakk/spg:9.0.3)
PORT=${PORT:-17521}
NAME=g5up
WORK=$(mktemp -d)
cleanup() { docker rm -f "$NAME" >/dev/null 2>&1; rm -rf "$WORK"; }
trap cleanup EXIT

fill() { # rows in every table the ingest path writes
  q "$1" -v ON_ERROR_STOP=1 -q <<'SQL'
INSERT INTO issue_user_hits (issue_id, user_key, hit_count)
SELECT id, 'u' || (ord % 7), (ord % 5) + 1 FROM (SELECT id, row_number() OVER (ORDER BY id) AS ord FROM issues) s;
INSERT INTO events (id, project_id, issue_id, kind, platform, occurred_at, payload)
SELECT ('00000000-0000-0000-0004-' || lpad(ord::text, 12, '0'))::uuid,
       '00000000-0000-0000-0000-000000000001', id, 'error', 'ios', now(),
       ('{"n":' || ord || '}')::jsonb
FROM (SELECT id, row_number() OVER (ORDER BY id) AS ord FROM issues) s;
UPDATE issues SET event_count = 1, users_count = 1, last_release = '1.4.2';
SQL
}

echo "g5-upgrade: into $NEW"
rc=0
for OLD in "${OLDS[@]}"; do
  VOL=g5up-vol-$$-$(echo "$OLD" | tr -c 'a-zA-Z0-9' '-')
  docker volume rm "$VOL" >/dev/null 2>&1; docker volume create "$VOL" >/dev/null
  DATA=$(datadir "$OLD")

  if ! boot "$OLD" "$PORT" "$NAME" -v "$VOL:$DATA" >/dev/null 2>&1; then
    echo "  $OLD: ✗ would not start"; rc=1; docker volume rm "$VOL" >/dev/null 2>&1; continue
  fi
  if ! load_schema "$PORT" >"$WORK/mig" 2>&1 || ! seed "$PORT" 200 >>"$WORK/mig" 2>&1 || ! fill "$PORT" >/dev/null 2>&1; then
    echo "  $OLD: schema — sentori's migrations do not load on it: $(head -1 "$WORK/mig")"
    docker rm -f "$NAME" >/dev/null 2>&1; docker volume rm "$VOL" >/dev/null 2>&1; continue
  fi
  # Write the rows out before the swap.
  #
  # Without this the new image rebuilds them by REPLAYING the old
  # image's WAL, and a WAL written before 9.0.4 does not carry what its
  # statements drew from the clock — so every `now()` column comes back
  # holding the moment of recovery and the comparison reports a change
  # this test cannot fix. What it is here to ask is whether the new
  # image reads the old image's DATA; g5-crash asks the replay question.
  q "$PORT" -tAc "CHECKPOINT" < /dev/null > /dev/null 2>&1
  fingerprint_db "$PORT" > "$WORK/before"
  n=$(awk 'END{print NR}' "$WORK/before")
  [ "${n:-0}" -ge 20 ] || { echo "  $OLD: ✗ fingerprinted only $n tables"; rc=1; }
  # Empty tables hash the same on both sides, so a comparison over them
  # is green whatever the new image does with the data directory.
  rows=$(q "$PORT" -tAc "SELECT count(*) FROM issues" < /dev/null | tr -d '[:space:]')
  [ "${rows:-0}" -ge 200 ] || { echo "  $OLD: ✗ only ${rows:-0} rows to compare"; rc=1; docker rm -f "$NAME" >/dev/null 2>&1; docker volume rm "$VOL" >/dev/null 2>&1; continue; }
  docker rm -f "$NAME" >/dev/null 2>&1

  if ! boot "$NEW" "$PORT" "$NAME" -v "$VOL:$(datadir "$NEW")" >/dev/null 2>&1; then
    echo "  $OLD → $NEW: ✗ the new image will not open that data directory"
    docker logs --tail 15 "$NAME" 2>&1 | sed 's/^/      /'
    rc=1; docker rm -f "$NAME" >/dev/null 2>&1; docker volume rm "$VOL" >/dev/null 2>&1; continue
  fi
  fingerprint_db "$PORT" > "$WORK/after"
  same=$(join "$WORK/before" "$WORK/after" | awk '$2 != $3 { printf "%s ", $1 }')
  [ "$(cut -d' ' -f1 "$WORK/before")" = "$(cut -d' ' -f1 "$WORK/after")" ] || same="SCHEMA $same"
  w=$(q "$PORT" -tAc "INSERT INTO issue_activity (id, issue_id, kind, body)
       SELECT gen_random_uuid(), id, 'note', '{}'::jsonb FROM issues LIMIT 1 RETURNING 1" 2>&1 | head -1)
  d=ok
  docker run --rm --network host -e PGPASSWORD=p "$ORACLE" pg_dump -h 127.0.0.1 -p "$PORT" -U u -d d \
    > "$WORK/dump" 2>"$WORK/dump.err" || d="pg_dump: $(head -1 "$WORK/dump.err")"
  [ -s "$WORK/dump" ] || d="pg_dump wrote nothing"

  v=ok
  [ -z "$same" ] || { v="✗ changed: $same"; rc=1; }
  [ "$w" = "1" ] || { v="✗ not writable: $w"; rc=1; }
  [ "$d" = ok ] || { v="✗ $d"; rc=1; }
  echo "  $OLD → $NEW: tables=$n writable=$w dump=${d:0:40}  $v"

  if [ "${SELFTEST:-0}" = 1 ] && [ -z "${SELFTESTED:-}" ]; then
    q "$PORT" -tAc "UPDATE issues SET group_title = 'changed' WHERE fingerprint = 'fp1'" >/dev/null
    fingerprint_db "$PORT" > "$WORK/tampered"
    case "$(join "$WORK/before" "$WORK/tampered" | awk '$2 != $3 { printf "%s ", $1 }')" in
      *issues*) echo "  selftest: the comparison goes red on one changed row: ok" ;;
      *) echo "  selftest: ✗ the comparison stayed green on one changed row"; rc=1 ;;
    esac
    SELFTESTED=1
  fi
  docker rm -f "$NAME" >/dev/null 2>&1; docker volume rm "$VOL" >/dev/null 2>&1
done

[ "$rc" = 0 ] && echo "g5-upgrade: PASS" || echo "g5-upgrade: FAIL"
exit "$rc"
