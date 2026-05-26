#!/usr/bin/env bash
# Bring up the three competitor DB containers, then wait for each
# to pass its health check before returning. Bench runs after this
# script can assume all three sockets are ready.
set -euo pipefail

cd "$(dirname "$0")/.."

echo "== bringing up postgres + mysql + mariadb (loopback only) =="
docker compose up -d

for svc in postgres mysql mariadb; do
    printf "  %-10s waiting … " "$svc"
    for _ in $(seq 1 60); do
        status=$(docker inspect -f '{{.State.Health.Status}}' "spg-bench-$svc" 2>/dev/null || echo "starting")
        if [ "$status" = "healthy" ]; then
            echo "healthy"
            break
        fi
        sleep 2
    done
    if [ "$status" != "healthy" ]; then
        echo "FAILED (status=$status)"
        docker compose logs "$svc" | tail -20 >&2
        exit 1
    fi
done

echo "== ports =="
echo "  postgres → 127.0.0.1:25432  (user=bench db=bench pass=bench)"
echo "  mysql    → 127.0.0.1:23306  (user=bench db=bench pass=bench)"
echo "  mariadb  → 127.0.0.1:23307  (user=bench db=bench pass=bench)"
