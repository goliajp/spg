#!/usr/bin/env bash
#
# Gate 5 — a graceful stop, and a restart under load.
#
#   xtests/gates/g5-shutdown.sh <image> [port]
#
# A crash is the easy case to argue about: nobody promised anything. A
# deploy is the common one, and it happens while the application is
# writing. Both legs are stopped the way an orchestrator stops them —
# SIGTERM, then a KILL if that is ignored — with a writer mid-flight,
# and four things are asked:
#
#   stops     SIGTERM is enough: the container exits without the KILL,
#             and inside the grace period
#   tells     the client that was writing is TOLD, rather than left to
#             notice the socket go away. PostgreSQL's fast shutdown
#             ends it with 57P01 and names the administrator.
#   durable   a clean stop loses nothing that was acknowledged, and
#             leaves no issue disagreeing with its events
#   restarts  it comes back on the same data directory and takes a row
#
# The panel scores against PostgreSQL, so an answer PG does not give
# either is not a loss.
#
# `SELFTEST=1` adds the negative control: the durability check is asked
# for a transaction that was never acknowledged, and has to report it.
GATES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
# shellcheck source=xtests/gates/lib.sh
. "$GATES_DIR/lib.sh"

IMAGE=${1:?usage: g5-shutdown.sh <image> [port]}
PORT=${2:-17561}
PGPORT=$((PORT + 1))
ISSUES=50
GRACE=${GRACE:-25}
PROJ='00000000-0000-0000-0000-000000000001'
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"; docker rm -f g5shut g5shutpg >/dev/null 2>&1' EXIT

rc=0
judge() { # <name> <image answer> <pg answer>
  if [ "$3" != "ok" ]; then
    printf '  %-10s %-34s %-34s  (PostgreSQL does not either)\n' "$1" "$2" "$3"
    return
  fi
  if [ "$2" = ok ]; then printf '  %-10s %-34s %-34s\n' "$1" "$2" "$3"
  else printf '  %-10s %-34s %-34s  ✗\n' "$1" "$2" "$3"; rc=1; fi
}

# The writer: ingest transactions, each acknowledged AFTER its COMMIT
# returns, so the harness holds the server to exactly what it claimed.
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

acked() { sed -n 's/^ACK \([0-9]*\)$/\1/p' "$1"; }

missing_acked() { # <port> <ack file>
  local ids
  ids=$(acked "$2" | paste -sd, -)
  [ -n "$ids" ] || { echo NOACK; return; }
  q "$1" -tAc "SELECT count(*) FROM (VALUES ($(echo "$ids" | sed 's/,/),(/g'))) v(n)
     WHERE NOT EXISTS (SELECT 1 FROM events e
                       WHERE e.id = ('00000000-0000-0000-0002-' || lpad(v.n::text, 12, '0'))::uuid)" < /dev/null
}

broken_invariant() {
  q "$1" -tAc "SELECT count(*) FROM issues i
     WHERE i.event_count <> (SELECT count(*) FROM events e WHERE e.issue_id = i.id)" < /dev/null
}

writable() {
  local id; id=$(q "$1" -tAc \
    "INSERT INTO issue_activity (id, issue_id, kind, body)
     SELECT gen_random_uuid(), id, 'note', '{}'::jsonb FROM issues LIMIT 1 RETURNING id" < /dev/null 2>&1 | head -1)
  case $id in
    *-*-*-*-*) echo ok ;;
    *) echo "${id:0:32}" ;;
  esac
}

# One leg: load, write, stop, restart, and report the four answers as
# four lines the caller reads back.
leg() { # <image> <port> <container> -> stops|tells|durable|restarts
  local img=$1 port=$2 name=$3 t0 t1 code

  boot "$img" "$port" "$name" >/dev/null 2>&1 || { printf '%s\n' "did not start" "-" "-" "-"; return; }
  load_schema "$port" >/dev/null 2>&1 || { printf '%s\n' "schema would not load" "-" "-" "-"; return; }
  seed "$port" "$ISSUES" >/dev/null 2>&1 || { printf '%s\n' "would not seed" "-" "-" "-"; return; }

  writer_script 1000000 1008000 > "$WORK/w.$port.sql"
  : > "$WORK/said.$port"
  docker run --rm -i --network host -e PGPASSWORD=p "$ORACLE" \
    psql -h 127.0.0.1 -p "$port" -U u -d d -X -q -v VERBOSITY=verbose -f - \
    < "$WORK/w.$port.sql" > /dev/null 2> "$WORK/said.$port" &
  local writer=$!
  sleep 4

  # `docker stop -t` is SIGTERM, then SIGKILL after the grace period —
  # what an orchestrator does. The wall clock and the exit code
  # together say which of the two ended it.
  t0=$(date +%s)
  docker stop -t "$GRACE" "$name" >/dev/null 2>&1
  t1=$(date +%s)
  wait "$writer" 2>/dev/null
  code=$(docker inspect -f '{{.State.ExitCode}}' "$name" 2>/dev/null)

  local stops="ok"
  [ "$code" = "137" ] && stops="ignored SIGTERM, needed the KILL"
  [ $((t1 - t0)) -lt "$GRACE" ] || stops="took ${GRACE}s+, the grace period ran out"

  # What the in-flight client was TOLD, which is not the same as what
  # it noticed. psql prints `connection to server was lost` and
  # `SSL error: unexpected eof` by itself, from the socket going away —
  # matching those was this check's own first bug, and it scored a
  # silent drop as ok. Only the server's own sentence counts.
  local tells
  if grep -aqiE '57P01|administrator command|57P03|shutting down' "$WORK/said.$port"; then
    tells=ok
  else
    tells="no word from the server: $(grep -av '^ACK ' "$WORK/said.$port" | grep -av '^	' | head -1 | cut -c1-44)"
  fi

  local n; n=$(acked "$WORK/said.$port" | wc -l | tr -d ' ')
  docker start "$name" >/dev/null 2>&1
  if ! wait_up "$port"; then printf '%s\n' "$stops" "$tells" "it did not come back" "-"; return; fi

  local miss inv durable
  miss=$(missing_acked "$port" "$WORK/said.$port"); inv=$(broken_invariant "$port")
  durable=ok
  # A leg that acknowledged nothing proves nothing — every check below
  # would pass on an empty set.
  [ "${n:-0}" -ge 20 ] || durable="only $n acknowledged, nothing was under way"
  [ "$miss" = "0" ] || durable="$miss acknowledged transactions are gone"
  [ "$inv" = "0" ] || durable="$inv issues disagree with their events"
  printf '%s\n' "$stops" "$tells" "$durable" "$(writable "$port")"
}

echo "g5-shutdown: $IMAGE vs $ORACLE"
# Four lines each, read back by line number — bash 3.2 (what macOS
# ships) has no `mapfile`, and the harnesses run where they are needed.
leg "$IMAGE"  "$PORT"   g5shut   > "$WORK/a"
leg "$ORACLE" "$PGPORT" g5shutpg > "$WORK/b"
ans() { sed -n "$2p" "$WORK/$1"; }
printf '  %-10s %-34s %-34s\n' question "$IMAGE" postgres
judge stops    "$(ans a 1)" "$(ans b 1)"
judge tells    "$(ans a 2)" "$(ans b 2)"
judge durable  "$(ans a 3)" "$(ans b 3)"
judge restarts "$(ans a 4)" "$(ans b 4)"

if [ "${SELFTEST:-0}" = 1 ]; then
  printf 'ACK 999999999\n' > "$WORK/fake"
  m=$(missing_acked "$PORT" "$WORK/fake")
  [ "$m" = "1" ] && echo "  selftest: the durability check goes red on a row nobody wrote: ok" \
                 || { echo "  selftest: ✗ it stayed green ($m)"; rc=1; }
fi

[ "$rc" = 0 ] && echo "g5-shutdown: $IMAGE PASS" || echo "g5-shutdown: $IMAGE FAIL"
exit "$rc"
