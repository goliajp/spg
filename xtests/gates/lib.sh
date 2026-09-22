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
  docker run -d --name "$name" $LIMITS -p "$port":5432 \
    -e POSTGRES_PASSWORD=p -e POSTGRES_USER=u -e POSTGRES_DB=d "$@" "$img" >/dev/null || return 2
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
