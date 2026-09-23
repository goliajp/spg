#!/usr/bin/env bash
# sentori acceptance gate 5 — the database survives being killed.
#
#   xtests/gates/g5-crash.sh <image> [rounds] [port]
#
# A client writes sentori's ingest transaction in a loop and prints
# `ACK n` to stderr AFTER each COMMIT returns. The server is then
# SIGKILLed and started again, and three things are asked of it:
#
#   durable    every ACKed transaction is there
#   invariant  issues.event_count == the events actually stored, per
#              issue — a torn transaction shows up here and nowhere else
#   writable   it takes a new row after the restart
#
# Each judgement has a negative control (`SELFTEST=1`), because a
# harness that cannot go red is not a measurement: the durability check
# is asked for a transaction that was never acknowledged, the invariant
# check runs after one issue's counter is bumped by hand, and both must
# be reported.
#
# NOT measured here: a power cut. `docker kill -s KILL` ends the
# process; the host's page cache survives it, so this tests the
# engine's own ordering, not the disk's.

GATES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
# shellcheck source=xtests/gates/lib.sh
. "$GATES_DIR/lib.sh"

IMAGE=${1:?usage: g5-crash.sh <image> [rounds] [port]}
ROUNDS=${2:-5}
PORT=${3:-17501}
NAME=g5crash
ISSUES=50
PER_ROUND=${PER_ROUND:-4000}
PROJ='00000000-0000-0000-0000-000000000001'
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# The writer's script: PER_ROUND ingest transactions, each ending in an
# acknowledgement the harness can hold the server to.
writer_script() {
  local from=$1 to=$2 i fp
  echo '\set ON_ERROR_STOP on'
  for ((i = from; i <= to; i++)); do
    fp=$(((i % ISSUES) + 1))
    printf 'BEGIN;\n'
    printf "SELECT id AS iid FROM issues WHERE project_id = '%s' AND fingerprint = 'fp%d' FOR UPDATE \\\\gset\n" "$PROJ" "$fp"
    printf 'UPDATE issues SET event_count = event_count + 1, last_seen = now() WHERE id = :%s;\n' "'iid'"
    printf "INSERT INTO events (id, project_id, issue_id, kind, platform, occurred_at, payload) VALUES ('00000000-0000-0000-0002-%012d', '%s', :%s, 'error', 'ios', now(), '{\"n\":%d}');\n" "$i" "$PROJ" "'iid'" "$i"
    printf 'COMMIT;\n'
    printf '\\warn ACK %d\n' "$i"
  done
}

# The ids the server said it had stored, read back out of its stderr.
acked() { sed -n 's/^ACK \([0-9]*\)$/\1/p' "$1"; }

missing_acked() { # <ack file> -> the acknowledged ids that are not there
  local ids
  ids=$(acked "$1" | paste -sd, -)
  [ -n "$ids" ] || { echo "NOACK"; return; }
  q "$PORT" -tAc "SELECT count(*) FROM (VALUES ($(echo "$ids" | sed 's/,/),(/g'))) v(n)
     WHERE NOT EXISTS (SELECT 1 FROM events e
                       WHERE e.id = ('00000000-0000-0000-0002-' || lpad(v.n::text, 12, '0'))::uuid)"
}

broken_invariant() { # -> how many issues disagree with their events
  q "$PORT" -tAc "SELECT count(*) FROM issues i
     WHERE i.event_count <> (SELECT count(*) FROM events e WHERE e.issue_id = i.id)"
}

writable() { # -> ok / the reason it is not
  local id; id=$(q "$PORT" -tAc \
    "INSERT INTO issue_activity (id, issue_id, kind, body)
     SELECT gen_random_uuid(), id, 'note', '{}'::jsonb FROM issues LIMIT 1 RETURNING id" 2>&1 | head -1)
  case $id in
    *-*-*-*-*) q "$PORT" -tAc "SELECT count(*) FROM issue_activity WHERE id = '$id'" ;;
    *) echo "$id" ;;
  esac
}

echo "g5-crash: $IMAGE, $ROUNDS rounds of $PER_ROUND transactions"
boot "$IMAGE" "$PORT" "$NAME" || exit 2
load_schema "$PORT" || exit 2
seed "$PORT" "$ISSUES" || exit 2

rc=0
for ((r = 1; r <= ROUNDS; r++)); do
  from=$((r * 1000000)); to=$((from + PER_ROUND - 1))
  writer_script "$from" "$to" > "$WORK/w.sql"
  : > "$WORK/ack.$r"
  docker run --rm -i --network host -e PGPASSWORD=p "$ORACLE" \
    psql -h 127.0.0.1 -p "$PORT" -U u -d d -X -q -f - \
    < "$WORK/w.sql" > /dev/null 2> "$WORK/ack.$r" &
  writer=$!
  sleep "${KILL_AFTER:-3}"
  docker kill -s KILL "$NAME" > /dev/null 2>&1
  wait "$writer" 2>/dev/null
  n=$(acked "$WORK/ack.$r" | wc -l | tr -d ' ')

  docker start "$NAME" > /dev/null 2>&1
  if ! wait_up "$PORT"; then
    echo "  round $r: ✗ did not come back after the kill"
    docker logs --tail 20 "$NAME" 2>&1 | sed 's/^/      /'
    rc=1; break
  fi

  miss=$(missing_acked "$WORK/ack.$r"); inv=$(broken_invariant); wr=$(writable)
  verdict=ok
  # A round that acknowledged nothing proves nothing: the kill landed
  # before the first commit, and every check below would pass on an
  # empty set.
  [ "${n:-0}" -ge 20 ] || { verdict="✗ only $n acknowledged — nothing was under way"; }
  [ "$miss" = "0" ] || verdict="✗ $miss acknowledged transactions are gone"
  [ "$inv" = "0" ] || verdict="✗ $inv issues disagree with their events"
  [ "$wr" = "1" ] || verdict="✗ not writable after the restart: $wr"
  [ "$verdict" = ok ] || rc=1
  echo "  round $r: acked=$n missing=$miss torn=$inv writable=$wr  $verdict"
done

if [ "${SELFTEST:-0}" = 1 ]; then
  echo "  selftest: the checks against a database they should refuse"
  printf 'ACK 999999999\n' > "$WORK/fake"
  m=$(missing_acked "$WORK/fake")
  [ "$m" = "1" ] && echo "    durability check goes red: ok" || { echo "    durability check stayed green on a row that was never written ($m)"; rc=1; }
  q "$PORT" -tAc "UPDATE issues SET event_count = event_count + 1 WHERE fingerprint = 'fp1'" > /dev/null
  i=$(broken_invariant)
  [ "$i" = "1" ] && echo "    invariant check goes red: ok" || { echo "    invariant check stayed green on a bumped counter ($i)"; rc=1; }
  q "$PORT" -tAc "UPDATE issues SET event_count = event_count - 1 WHERE fingerprint = 'fp1'" > /dev/null
fi

docker rm -f "$NAME" > /dev/null 2>&1
[ "$rc" = 0 ] && echo "g5-crash: $IMAGE PASS" || echo "g5-crash: $IMAGE FAIL"
exit "$rc"
