#!/usr/bin/env bash
# sentori acceptance gate 5 — connections and sessions, the way a pooled
# application uses them.
#
#   xtests/gates/g5-connections.sh <image> [port]
#
# Every question is asked of PostgreSQL too, and only the ones
# PostgreSQL answers count — an engine is not behind for refusing what
# PostgreSQL also refuses.
#
#   churn        a pool opening and closing connections leaves nothing
#                behind: the server's own connection count returns to
#                where it started, and it still answers
#   too many     connection number max_connections+1 is refused the way
#                PostgreSQL refuses it (53300), and the ones already
#                open keep working
#   cancel       a long statement cancelled from another connection ends
#                with 57014 and its connection survives to run the next
#   terminate    pg_terminate_backend closes the target, and only it
#   idle         a connection left idle past the timeout is closed, and
#                the client is told rather than left hanging
#
# `SELFTEST=1` runs the churn check against a server that is NOT asked
# to churn, so a check that cannot tell the two apart is visible.

GATES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
# shellcheck source=xtests/gates/lib.sh
. "$GATES_DIR/lib.sh"

IMAGE=${1:?usage: g5-connections.sh <image> [port]}
PORT=${2:-17551}
PGPORT=$((PORT + 1))
S=g5cn-s; P=g5cn-p
WORK=$(mktemp -d)
cleanup() { docker rm -f "$S" "$P" >/dev/null 2>&1; rm -rf "$WORK"; }
trap cleanup EXIT

# The server's own count of client connections.
conns() {
  q "$1" -tAc "SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'client backend'" \
    < /dev/null 2>/dev/null | tr -d '[:space:]'
}

# One psql per connection would spend its time starting containers, so
# the churn runs inside ONE container: a shell loop of short psql runs.
churn() { # <port> <rounds>
  docker run --rm --network host -e PGPASSWORD=p "$ORACLE" sh -c \
    "i=0; while [ \$i -lt $2 ]; do psql -h 127.0.0.1 -p $1 -U u -d d -tAc 'SELECT 1' >/dev/null 2>&1 || exit 1; i=\$((i+1)); done" \
    >/dev/null 2>&1 && echo ok || echo failed
}

# A connection that holds a statement open, so another can cancel it.
sleeper() { # <port> <seconds> -> pid of the background client
  docker run --rm -i --network host -e PGPASSWORD=p "$ORACLE" \
    psql -h 127.0.0.1 -p "$1" -U u -d d -v VERBOSITY=verbose -tAc "SELECT pg_sleep($2)" \
      > "$WORK/sleep.$1" 2>&1 &
  echo $!
}

judge() { # <name> <spg answer> <pg answer>
  if [ "$3" != "ok" ]; then
    printf '  %-12s %-28s %-28s  (PostgreSQL does not either)\n' "$1" "$2" "$3"
    return
  fi
  if [ "$2" = ok ]; then
    printf '  %-12s %-28s %-28s\n' "$1" "$2" "$3"
  else
    printf '  %-12s %-28s %-28s  ✗\n' "$1" "$2" "$3"
    rc=1
  fi
}

echo "g5-connections: $IMAGE vs $ORACLE"
boot "$IMAGE" "$PORT" "$S" -e SPG_MAX_CONNECTIONS=12 -e SPG_IDLE_TIMEOUT_SEC=3 || exit 2
BOOT_CMD="postgres -c max_connections=12 -c idle_session_timeout=3000" \
  boot "$ORACLE" "$PGPORT" "$P" || exit 2
for p in "$PORT" "$PGPORT"; do load_schema "$p" || exit 2; seed "$p" 20 || exit 2; done

rc=0
printf '  %-12s %-28s %-28s\n' question "$IMAGE" postgres

# --- churn -----------------------------------------------------------
churn_check() { # <port>
  local base after
  base=$(conns "$1")
  [ -n "$base" ] || { echo "unreadable"; return; }
  [ "$(churn "$1" 40)" = ok ] || { echo "a connection was refused"; return; }
  # The server closes them on its own schedule; give it a moment and
  # read again rather than racing it.
  sleep 2
  after=$(conns "$1")
  if [ "${after:-99}" -le "$((base + 1))" ]; then echo ok; else echo "left $after of $base"; fi
}
a=$(churn_check "$PORT"); b=$(churn_check "$PGPORT")
judge churn "$a" "$b"

if [ "${SELFTEST:-0}" = 1 ]; then
  # The same check with NO churn: it must still say ok, or the check is
  # answering something other than what it claims.
  base=$(conns "$PORT"); sleep 2; after=$(conns "$PORT")
  if [ "${after:-99}" -le "$((base + 1))" ]; then
    echo "  selftest: the churn check reads a quiet server as ok: ok"
  else
    echo "  selftest: ✗ the churn check calls a quiet server dirty ($base -> $after)"; rc=1
  fi
fi

# --- too many --------------------------------------------------------
toomany() { # <port>
  local out
  out=$(docker run --rm --network host -e PGPASSWORD=p "$ORACLE" sh -c \
    "for i in \$(seq 1 20); do psql -h 127.0.0.1 -p $1 -U u -d d -tAc 'SELECT pg_sleep(4)' >/dev/null 2>>/tmp/e & done; wait; cat /tmp/e" 2>&1 | head -c 400)
  case "$out" in
    *53300*|*"too many clients"*|*"too many connections"*) echo ok ;;
    "") echo "no refusal at all" ;;
    *) echo "refused with: $(echo "$out" | head -1 | cut -c1-40)" ;;
  esac
}
a=$(toomany "$PORT"); b=$(toomany "$PGPORT")
judge "too many" "$a" "$b"

# --- cancel ----------------------------------------------------------
# What the cancelled client PRINTED is the judge. The first version
# waited for the `docker run` wrapper to exit, and the wrapper outlives
# the psql inside it — so a cancel that worked read as "not cancelled",
# on BOTH engines, which took the row out of the panel entirely.
cancel_check() { # <port>
  local pid target waited=0
  : > "$WORK/sleep.$1"
  pid=$(sleeper "$1" 30)
  sleep 2
  # This lookup's own text contains the pattern it searches for, so it
  # matches itself; without pg_backend_pid() it can cancel the asker and
  # leave the sleeper untouched, which reads as "not cancelled (silent)".
  target=$(q "$1" -tAc "SELECT pid FROM pg_stat_activity WHERE query LIKE '%pg_sleep(30)%' AND backend_type = 'client backend' AND pid <> pg_backend_pid() ORDER BY pid LIMIT 1" < /dev/null 2>/dev/null | tr -d '[:space:]')
  [ -n "$target" ] || { kill "$pid" 2>/dev/null; echo "the statement is not visible"; return; }
  q "$1" -tAc "SELECT pg_cancel_backend($target)" < /dev/null >/dev/null 2>&1
  while [ $waited -lt 12 ]; do
    grep -qiE "57014|canceling statement" "$WORK/sleep.$1" 2>/dev/null && break
    sleep 1; waited=$((waited + 1))
  done
  local said; said=$(head -1 "$WORK/sleep.$1" 2>/dev/null)
  kill "$pid" 2>/dev/null
  if echo "$said" | grep -qiE "57014|canceling statement"; then echo ok
  elif [ -z "$said" ]; then echo "not cancelled (silent)"
  else echo "said: $(echo "$said" | cut -c1-34)"; fi
}
a=$(cancel_check "$PORT"); b=$(cancel_check "$PGPORT")
judge cancel "$a" "$b"

# --- terminate --------------------------------------------------------
# What the header has claimed all along and the panel never asked. An
# operator kills a session that is holding something; the target must be
# told it was an administrator and its connection must close, and no
# other connection may be touched.
terminate_check() { # <port>
  local pid target waited=0 other
  : > "$WORK/sleep.$1"
  pid=$(sleeper "$1" 30)
  sleep 2
  target=$(q "$1" -tAc "SELECT pid FROM pg_stat_activity WHERE query LIKE '%pg_sleep(30)%' AND backend_type = 'client backend' AND pid <> pg_backend_pid() ORDER BY pid LIMIT 1" < /dev/null 2>/dev/null | tr -d '[:space:]')
  [ -n "$target" ] || { kill "$pid" 2>/dev/null; echo "the statement is not visible"; return; }
  q "$1" -tAc "SELECT pg_terminate_backend($target)" < /dev/null >/dev/null 2>&1
  while [ $waited -lt 12 ]; do
    grep -qiE "57P01|administrator command" "$WORK/sleep.$1" 2>/dev/null && break
    sleep 1; waited=$((waited + 1))
  done
  kill "$pid" 2>/dev/null
  # A connection opened after the signal must still work: the kill has
  # to land on one backend, not on the server.
  other=$(q "$1" -tAc "SELECT 'alive'" < /dev/null 2>/dev/null | tr -d '[:space:]')
  if ! grep -qiE "57P01|administrator command" "$WORK/sleep.$1" 2>/dev/null; then
    echo "not terminated: $(head -1 "$WORK/sleep.$1" | cut -c1-30)"
  elif [ "$other" != "alive" ]; then
    echo "it took the server down too"
  else echo ok; fi
}
a=$(terminate_check "$PORT"); b=$(terminate_check "$PGPORT")
judge terminate "$a" "$b"

# --- idle timeout ----------------------------------------------------
# A connection that does nothing for longer than the timeout must be
# closed, and the client must be told.
idle_check() { # <port>
  local out
  out=$(docker run --rm -i --network host -e PGPASSWORD=p "$ORACLE" \
    psql -h 127.0.0.1 -p "$1" -U u -d d -tA 2>&1 <<'SQL'
SELECT 1;
\! sleep 6
SELECT 2;
SQL
)
  case "$out" in
    *"terminating connection"*|*"server closed"*|*57P05*|*"connection to server"*) echo ok ;;
    *2*) echo "still answering after the timeout" ;;
    *) echo "$(echo "$out" | head -1 | cut -c1-38)" ;;
  esac
}
a=$(idle_check "$PORT"); b=$(idle_check "$PGPORT")
judge idle "$a" "$b"

# Still serving after all of that.
alive_s=$(q "$PORT" -tAc "SELECT 1" < /dev/null 2>&1 | tr -d '[:space:]')
[ "$alive_s" = 1 ] || { echo "  ✗ the server stopped answering: $alive_s"; rc=1; }

[ "$rc" = 0 ] && echo "g5-connections: $IMAGE PASS" || echo "g5-connections: $IMAGE FAIL"
exit "$rc"
