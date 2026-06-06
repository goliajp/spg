#!/usr/bin/env bash
# run.sh — pipe every dump_compat/*/<app>/schema.sql through SPG
# and report per-dialect per-app pass/fail.
#
# Modes:
#   ./run.sh local-build           # use cargo target/release/spg-server
#   ./run.sh <docker-tag>          # use goliakk/spg:<tag>
#
# Output: report.md with one row per dump file:
#   dialect | app | psql_exit | first_error_line | first_error_message
#   (psql_exit == 0 → "PASS"; non-zero → "FAIL")
#
# Honours $PORT (default 6022) and $CONTAINER (default
# spg-dump-compat).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
VERSION="${1:?usage: $0 <spg-tag | local-build>}"
PORT="${PORT:-6022}"
CONTAINER="${CONTAINER:-spg-dump-compat}"
REPORT="$HERE/report.md"
ROOT="$(git -C "$HERE" rev-parse --show-toplevel)"

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
        "$ROOT/target/release/spg-server" 127.0.0.1:0 >/tmp/spg-dump.log 2>&1 &
        LOCAL_PID=$!
        sleep 0.4
    else
        docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
        docker run -d --name "$CONTAINER" \
            -e POSTGRES_DB=app -e POSTGRES_USER=u -e POSTGRES_PASSWORD=p \
            -p "$PORT:5432" "goliakk/spg:$VERSION" >/dev/null
    fi
    # Wait for ready
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

# Run one dump file through PG-wire psql. Prints per-statement result.
# Records: stmt_count_total, stmt_count_passed, first_error_line,
# first_error_msg.
run_one() {
    local dialect="$1"
    local app="$2"
    local file="$3"
    restart_server >/dev/null 2>&1
    local URL="postgres://u:p@host.docker.internal:$PORT/app?sslmode=disable"
    # Use psql with single-transaction off; collect every error.
    local out
    out=$(docker run --rm \
            -v "$file:/schema.sql:ro" \
            postgres:15 psql "$URL" -X -q -f /schema.sql 2>&1 || true)
    local total=$(grep -cE '^(SET|CREATE|ALTER|COMMENT|DROP|INSERT|DO|REVOKE|GRANT|SELECT|BEGIN|COMMIT|/\*)' "$file" 2>/dev/null)
    [[ -z "$total" ]] && total=0
    local errors=$(echo "$out" | grep -cE '^psql:.*ERROR:' 2>/dev/null)
    [[ -z "$errors" ]] && errors=0
    local first_err=$(echo "$out" | grep -m1 -E '^psql:.*ERROR:' || echo "")
    local passed=$((total - errors))
    local status="PASS"
    if [[ $errors -gt 0 ]]; then status="FAIL"; fi
    printf '| %s | %s | %s | %s/%s | %s |\n' \
        "$dialect" "$app" "$status" "$passed" "$total" \
        "$(echo "$first_err" | head -c 200 | sed 's/|/\\|/g')"
}

{
    echo "# SPG dump-compat report"
    echo
    echo "Generated $(date -u +%Y-%m-%dT%H:%M:%SZ) against SPG \`$VERSION\`."
    echo
    echo "| Dialect | App | Status | Stmts pass/total | First error |"
    echo "|---|---|---|---:|---|"
} > "$REPORT"

for dialect in pg mysql mariadb; do
    for app_dir in "$HERE/$dialect"/*/; do
        app=$(basename "$app_dir")
        file="$app_dir/schema.sql"
        [[ -f "$file" ]] || continue
        run_one "$dialect" "$app" "$file" >> "$REPORT"
    done
done

echo
echo "report written to $REPORT"
cat "$REPORT"
