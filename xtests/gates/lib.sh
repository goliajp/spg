# xtests/gates/lib.sh — shared by the gate harnesses (sourced, not run).
#
# Every harness drives a database the way sentori's deployment does: a
# container from an image, sentori's 17 migrations, and a client that is
# NOT on the host — psql / pgbench from the oracle image with host
# networking, so SPG and PostgreSQL are reached by the same route.
#
# Env:
#   SENTORI_MIGRATIONS  sentori's core/migrations (default: the sibling checkout)
#   ORACLE              PostgreSQL image for the client tools and the PG leg
#                       (default postgres:18-alpine, what sentori's compose runs)
#   LIMITS              docker resource flags for every leg (default --cpus=2 --memory=2g)

set -uo pipefail
GATES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
SENTORI_MIGRATIONS=${SENTORI_MIGRATIONS:-$(cd "$GATES_DIR/../../../sentori/core/migrations" 2>/dev/null && pwd)}
ORACLE=${ORACLE:-postgres:18-alpine}
LIMITS=${LIMITS:---cpus=2 --memory=2g}

# psql against a leg on `port`, from the oracle image.
q() { docker run --rm -i --network host -e PGPASSWORD=p "$ORACLE" psql -h 127.0.0.1 -p "$1" -U u -d d -X "${@:2}"; }

# boot <image> <port> <container> [extra docker args…]
# Both engines take POSTGRES_USER / POSTGRES_PASSWORD / POSTGRES_DB.
boot() {
  local img=$1 port=$2 name=$3; shift 3
  docker rm -f "$name" >/dev/null 2>&1
  # shellcheck disable=SC2086
  # `BOOT_CMD` is the command the image runs, appended AFTER the image
  # name — where a `postgres -c name=value` setting has to go. Docker
  # reads anything before the image as its own flag, and `-c` there is
  # `--cpu-shares`.
  # shellcheck disable=SC2086
  docker run -d --name "$name" $LIMITS -p "$port":5432 \
    -e POSTGRES_PASSWORD=p -e POSTGRES_USER=u -e POSTGRES_DB=d "$@" "$img" \
    ${BOOT_CMD:-} >/dev/null || return 2
  wait_up "$port" || { echo "✗ $img on :$port never answered"; docker logs --tail 20 "$name" 2>&1 | sed 's/^/    /'; return 2; }
}

wait_up() { for _ in $(seq 1 120); do q "$1" -tAc 'select 1' >/dev/null 2>&1 && return 0; sleep 0.5; done; return 1; }

# Apply sentori's migrations; the table count proves it (a setup step
# whose failure is discarded looks exactly like a finding).
load_schema() {
  local port=$1 f n
  n=$(ls "$SENTORI_MIGRATIONS"/*.sql 2>/dev/null | wc -l | tr -d ' ')
  [ "$n" -ge 17 ] || { echo "✗ $n migrations under '$SENTORI_MIGRATIONS', expected ≥ 17"; return 2; }
  for f in "$SENTORI_MIGRATIONS"/*.sql; do
    q "$port" -v ON_ERROR_STOP=1 -q -f - < "$f" >/dev/null 2>/tmp/gates-mig.$$ \
      || { echo "✗ $(basename "$f") does not load on :$port: $(head -1 /tmp/gates-mig.$$)"; return 2; }
  done
  local t; t=$(q "$port" -tAc "SELECT count(*) FROM pg_tables WHERE schemaname = 'public'")
  [ "${t:-0}" -ge 20 ] || { echo "✗ :$port holds ${t:-0} tables after the migrations"; return 2; }
}

# One project and `issues` issues, fingerprints fp1..fpN — the ingest
# workload's "issue already exists" path, which is the ordinary one.
seed() {
  local port=$1 issues=${2:-200}
  q "$port" -v ON_ERROR_STOP=1 -q <<SQL || return 2
INSERT INTO projects (id, name) VALUES ('00000000-0000-0000-0000-000000000001', 'bench');
INSERT INTO issues (id, project_id, fingerprint, kind, group_title, first_seen, last_seen)
SELECT ('00000000-0000-0000-0001-' || lpad(g::text, 12, '0'))::uuid,
       '00000000-0000-0000-0000-000000000001', 'fp' || g, 'error', 'TypeError ' || g, now(), now()
FROM generate_series(1, $issues) g;
SQL
  local n; n=$(q "$port" -tAc "SELECT count(*) FROM issues")
  [ "$n" = "$issues" ] || { echo "✗ :$port seeded $n issues, wanted $issues"; return 2; }
}

# Where an image keeps its data. SPG's images declare /data; the
# postgres images keep theirs under /var/lib/postgresql.
datadir() { case "$1" in postgres:*|*/postgres:*) echo /var/lib/postgresql ;; *) echo /data ;; esac; }

# A hash of one table's contents, order-independent: COPY writes the
# rows, the host sorts them. Row counts are the weakest thing a backup
# check can compare, so nothing here compares them.
# `< /dev/null` because psql runs with docker's -i: without it the
# client reads the CALLER's stdin and eats whatever is feeding the loop.
table_hash() { q "$1" -tAc "COPY (SELECT * FROM $2) TO STDOUT" < /dev/null 2>/dev/null | LC_ALL=C sort | hashsum; }

hashsum() { if command -v md5 >/dev/null; then md5 -q; else md5sum | cut -d' ' -f1; fi; }

# The public tables, in a fixed order. Sorted HERE, not by the server:
# `ORDER BY` asks the server's collation, and two versions of the same
# engine may not sort `issue_user_hits` to the same place — which reads
# as a table appearing and disappearing when two lists are compared.
tables() { q "$1" -tAc "SELECT tablename FROM pg_tables WHERE schemaname = 'public'" < /dev/null | LC_ALL=C sort; }

# Every table's hash, one `name hash` line each.
fingerprint_db() {
  local p=$1 t list
  list=$(tables "$p")
  for t in $list; do echo "$t $(table_hash "$p" "$t")"; done
}
