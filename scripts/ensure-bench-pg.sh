#!/usr/bin/env bash
# v7.40.11 — provision the PostgreSQL bench container, once, for
# everyone who needs it.
#
# `spg-bench-postgres` on port 25432 underpins at least seven callers:
# `perf-sweep`'s PostgreSQL leg, the `generative` differential,
# `pgbench`, `pgdump-roundtrip`, another dump path, and
# `xtests/diffcorpus/run.sh`. Nothing in this repository created it. It
# existed because somebody once typed `docker run` on one machine, and
# every one of those steps was one `docker rm` away from failing with a
# message about the step rather than about the container.
#
# Two of the callers were given their own inline provisioning first.
# That fixed two places and left five, which is how a class survives
# being treated one instance at a time.
#
# The pin lives here, so "which PostgreSQL are we measuring against"
# has ONE answer. It was `postgres:18` as resolved by whoever ran the
# command — 18.4 on this machine, while the rest of the project compares
# against 18.6, and nothing said so.
#
# Idempotent: creates when absent, starts when stopped, and otherwise
# does nothing. Exits 2 and says why when it cannot leave a usable
# container behind.
set -uo pipefail
export PATH="$HOME/.orbstack/bin:/Applications/OrbStack.app/Contents/MacOS/xbin:$PATH"

NAME="${BENCH_PG_CONTAINER:-spg-bench-postgres}"
PORT="${BENCH_PG_PORT:-25432}"
IMAGE="${BENCH_PG_IMAGE:-postgres:18.6}"
PIN="${IMAGE##*:}"; PIN="${PIN%%-*}"

if ! command -v docker >/dev/null 2>&1; then
  echo "ensure-bench-pg: no docker on PATH — cannot provision $NAME" >&2
  exit 2
fi

# v8.0 — the auth method is part of what this container has to BE, not
# an incidental default.
#
# `postgres:18` initdb's default for host connections is scram-sha-256,
# and `xtests/suitelib/src/wireclient.rs` — the client the generative
# differential connects its live-PG leg with — speaks exactly two
# methods: 0 (trust) and 3 (cleartext). So the `generative` step died
# with
#
#   connect live PG leg: "wire: auth method 10 not spoken by this client"
#
# the first time anything actually reached this container. It was not a
# regression: the container had always been scram-only from the host
# (loopback inside the container is `trust`, but a connection through
# docker's NAT is not loopback). Nothing had connected with that client
# before, so nothing had said so.
#
# Baked at initdb, so an existing container with the wrong method has to
# be REPLACED rather than reconfigured. It holds only fixtures the
# harness rebuilds.
AUTH_WANTED="trust"
container_auth() {
  docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' "$1" 2>/dev/null \
    | sed -n 's/^POSTGRES_HOST_AUTH_METHOD=//p' | head -1
}
if docker inspect "$NAME" >/dev/null 2>&1 \
   && [ "$(container_auth "$NAME")" != "$AUTH_WANTED" ]; then
  echo "ensure-bench-pg: $NAME has host auth '$(container_auth "$NAME")', wants \
'${AUTH_WANTED}' — replacing it (initdb bakes this in)" >&2
  docker rm -f "$NAME" >/dev/null 2>&1 || true
fi

if ! docker inspect "$NAME" >/dev/null 2>&1; then
  echo "ensure-bench-pg: creating $NAME from $IMAGE on port $PORT" >&2
  docker run -d --name "$NAME" \
      -e POSTGRES_USER=bench -e POSTGRES_PASSWORD=bench -e POSTGRES_DB=bench \
      -e POSTGRES_HOST_AUTH_METHOD="$AUTH_WANTED" \
      -p "$PORT":5432 "$IMAGE" >/dev/null || {
    echo "ensure-bench-pg: could not create $NAME from $IMAGE" >&2; exit 2; }
elif [ "$(docker inspect -f '{{.State.Running}}' "$NAME" 2>/dev/null)" != "true" ]; then
  echo "ensure-bench-pg: starting $NAME" >&2
  docker start "$NAME" >/dev/null || {
    echo "ensure-bench-pg: could not start $NAME" >&2; exit 2; }
fi

# The readiness probe is the QUERY, not `pg_isready`: measured on a
# first boot, `pg_isready` answered while the server was still
# initialising its data directory and the version query returned nothing
# for another half-minute — which the check below would then have
# reported as a version mismatch, naming the wrong cause.
version=""
for _ in $(seq 1 240); do
  version="$(docker exec "$NAME" psql -U bench -d bench -tA -c 'SHOW server_version;' 2>/dev/null \
      | tr -d '[:space:]')"
  [ -n "$version" ] && break
  sleep 1
done

case "$version" in
  "$PIN"*) ;;
  *)
    echo "ensure-bench-pg: $NAME is running '${version:-<nothing>}' and this repository pins $PIN." >&2
    echo "                 Every measurement taken against it is a measurement against that build," >&2
    echo "                 not the one the project claims to compare with." >&2
    echo "                 Recreate it: docker rm -f $NAME && $0" >&2
    exit 2
    ;;
esac

echo "ensure-bench-pg: $NAME ready — PostgreSQL $version on port $PORT" >&2
