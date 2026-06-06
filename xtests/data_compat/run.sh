#!/usr/bin/env bash
# run.sh — gate #4: realistic pg_dump-shape data round-trip.
#
# Pipes each fixtures/<app>/schema.sql + data.sql through SPG
# via pgwire, then verifies row counts post-load match expected
# values. This is the gate mailrs round-8 recommended adding —
# the dump-compat gate's per-statement error count alone misses
# silent data drops (a COPY block whose individual rows fail at
# INSERT-time still increments only one psql "ERROR" in the
# transcript even when 100% of rows dropped).
#
# Modes:
#   ./run.sh local-build           # use cargo target/release/spg-server
#   ./run.sh <docker-tag>          # use goliakk/spg:<tag>
#
# Per-fixture acceptance:
#   * schema.sql applies cleanly (0 psql ERROR)
#   * data.sql applies cleanly (0 psql ERROR)
#   * SELECT count(*) for every table named in the expected map
#     matches expected exactly. Counted via psql -t.
#
# Exit 0 iff every fixture passes both checks.

set -eo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
VERSION="${1:?usage: $0 <spg-tag | local-build>}"
PORT="${PORT:-6023}"
CONTAINER="${CONTAINER:-spg-data-compat}"
REPORT="$HERE/report.md"
ROOT="$(git -C "$HERE" rev-parse --show-toplevel)"

# Per-fixture expected row counts live in
# `fixtures/<app>/expected.txt`, one `<table> <count>` line each.

cleanup() {
    if [[ "$VERSION" == "local-build" ]]; then
        [[ -n "${LOCAL_PID:-}" ]] && kill "$LOCAL_PID" 2>/dev/null || true
        wait "${LOCAL_PID:-0}" 2>/dev/null || true
    else
        docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

start_server() {
    if [[ "$VERSION" == "local-build" ]]; then
        (cd "$ROOT" && cargo build --release --bin spg-server -q)
        SPG_PG_ADDR=0.0.0.0:$PORT \
            POSTGRES_DB=app POSTGRES_USER=u POSTGRES_PASSWORD=p \
        "$ROOT/target/release/spg-server" 127.0.0.1:0 >/tmp/spg-data.log 2>&1 &
        LOCAL_PID=$!
        sleep 0.4
    else
        docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
        docker run -d --name "$CONTAINER" \
            -e POSTGRES_DB=app -e POSTGRES_USER=u -e POSTGRES_PASSWORD=p \
            -p "$PORT:5432" "goliakk/spg:$VERSION" >/dev/null
    fi
    for i in $(seq 1 30); do
        if docker run --rm postgres:15 pg_isready \
                -h host.docker.internal -p "$PORT" -U u -d app \
                >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    echo "server didn't come up in 30s" >&2
    return 1
}

restart_server() {
    cleanup
    sleep 0.5
    start_server
}

URL="postgres://u:p@host.docker.internal:$PORT/app?sslmode=disable"

run_fixture() {
    local app="$1"
    local schema="$HERE/fixtures/$app/schema.sql"
    local data="$HERE/fixtures/$app/data.sql"
    restart_server >/dev/null 2>&1

    local schema_out
    schema_out=$(docker run --rm -v "$schema:/x.sql:ro" \
        postgres:15 psql "$URL" -X -q -f /x.sql 2>&1 || true)
    local schema_err
    schema_err=$(echo "$schema_out" | grep -cE '^psql:.*ERROR:' 2>/dev/null)
    [[ -z "$schema_err" ]] && schema_err=0

    local data_out
    data_out=$(docker run --rm -v "$data:/x.sql:ro" \
        postgres:15 psql "$URL" -X -q -f /x.sql 2>&1 || true)
    local data_err
    data_err=$(echo "$data_out" | grep -cE '^psql:.*ERROR:' 2>/dev/null)
    [[ -z "$data_err" ]] && data_err=0

    local count_status="PASS"
    local count_detail=""
    local expected_file="$HERE/fixtures/$app/expected.txt"
    if [[ -f "$expected_file" ]]; then
        while read -r table want; do
            [[ -z "$table" ]] && continue
            local got
            got=$(docker run --rm postgres:15 psql "$URL" -X -t -A \
                -c "SELECT count(*) FROM $table" 2>/dev/null \
                | tr -d '[:space:]' || echo "?")
            if [[ "$got" != "$want" ]]; then
                count_status="FAIL"
                count_detail+="${table}=${got}/${want} "
            else
                count_detail+="${table}=${got} "
            fi
        done < "$expected_file"
    fi

    local overall="PASS"
    if [[ $schema_err -gt 0 || $data_err -gt 0 || "$count_status" != "PASS" ]]; then
        overall="FAIL"
    fi
    printf '| %s | %s | schema_err=%s data_err=%s | %s |\n' \
        "$app" "$overall" "$schema_err" "$data_err" "$count_detail"
}

{
    echo "# SPG data-compat report (gate #4 — pg_dump data round-trip)"
    echo
    echo "Generated $(date -u +%Y-%m-%dT%H:%M:%SZ) against SPG \`$VERSION\`."
    echo
    echo "| Fixture | Status | Errors | Row counts (got/expected) |"
    echo "|---|---|---|---|"
} > "$REPORT"

OVERALL_FAIL=0
for app_dir in "$HERE/fixtures"/*/; do
    app=$(basename "$app_dir")
    row=$(run_fixture "$app")
    echo "$row" >> "$REPORT"
    if echo "$row" | grep -q 'FAIL'; then
        OVERALL_FAIL=1
    fi
done

echo
echo "report written to $REPORT"
cat "$REPORT"

exit $OVERALL_FAIL
