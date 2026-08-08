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
# Every `docker` call below needs OrbStack's client, which a
# non-interactive shell does not have on PATH. `diffcorpus/run.sh`
# has carried this line since round 666; the two harnesses beside
# it did not, and the failure that caused was invisible — see the
# wait loop's comment in dump_compat/run.sh.
export PATH=/Applications/OrbStack.app/Contents/MacOS/xbin:$PATH
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
        # v7.22 — SIGTERM drains gracefully; the next start_server
        # must not race the bind. Wait for the port to actually
        # free, escalate to -9 if the drain hangs. Without this the
        # new server fails to bind, dies silently, and psql talks
        # to the PREVIOUS fixture's server ("table already exists"
        # ghosts across fixtures).
        for _ in $(seq 1 50); do
            lsof -ti :"$PORT" >/dev/null 2>&1 || break
            sleep 0.2
        done
        if lsof -ti :"$PORT" >/dev/null 2>&1; then
            lsof -ti :"$PORT" | xargs kill -9 2>/dev/null || true
            sleep 0.3
        fi
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
        # Kill any stale HOST-side listener on $PORT (orphaned
        # local-build server) — otherwise `docker run -p` fails
        # and psql silently talks to the stale process. Same
        # guard as data_compat/run.sh (2026-06-10 drift).
        if lsof -ti :$PORT >/dev/null 2>&1; then
            lsof -ti :$PORT | xargs kill -9 2>/dev/null || true
            sleep 0.3
        fi
        docker run -d --name "$CONTAINER" \
            -e POSTGRES_DB=app -e POSTGRES_USER=u -e POSTGRES_PASSWORD=p \
            -p "$PORT:5432" "goliakk/spg:$VERSION" >/dev/null
    fi
    # Wait for ready
    for i in $(seq 1 30); do
        if docker run --rm postgres:18 pg_isready \
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
            postgres:18 psql "$URL" -X -q -f /schema.sql 2>&1 || true)
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
        # v7.22 — mysql/mariadb WITH-DATA fixtures skip the psql
        # wire pass: psql splits client-side with PG string
        # semantics, and mysqldump data uses backslash escapes —
        # psql itself shreds the INSERTs before SPG ever sees them
        # (a transport mismatch, not an SPG gap; no real MySQL
        # workload arrives via psql). They run in the import pass
        # below, which is dialect-aware.
        if [[ "$dialect" != "pg" && "$app" == *-with-data ]]; then
            printf '| %s | %s | SKIP(wire) | - | mysql data via psql is a transport mismatch; covered by import pass |\n' \
                "$dialect" "$app" >> "$REPORT"
            continue
        fi
        run_one "$dialect" "$app" "$file" >> "$REPORT"
    done
done

# v7.22 (round-13) — second pass: the EMBED path. Every fixture must
# ALSO load via `spg import` with zero preprocessing. psql hides
# `\`-meta-lines client-side and the server intercepts COPY / SET
# before the engine parser, so the wire pass alone structurally
# misses embed-only gaps — that is exactly how the round-13 list
# stayed invisible while this gate showed 10/10. local-build mode
# only (the import binary builds from the workspace; docker-tag mode
# stays wire-only).
if [[ "$VERSION" == "local-build" ]]; then
    (cd "$ROOT" && cargo build --release -p spgctl -q)
    SPG_BIN="$ROOT/target/release/spg"
    {
        echo
        echo "## Embed import pass (\`spg import\`)"
        echo
        echo "| Dialect | App | Import |"
        echo "|---|---|---|"
    } >> "$REPORT"
    IMPORT_SCRATCH=$(mktemp -d)
    for dialect in pg mysql mariadb; do
        for app_dir in "$HERE/$dialect"/*/; do
            app=$(basename "$app_dir")
            file="$app_dir/schema.sql"
            [[ -f "$file" ]] || continue
            tmpdb="$IMPORT_SCRATCH/${dialect}-${app}.spgdb"
            if out=$("$SPG_BIN" import --db "$tmpdb" --file "$file" 2>&1); then
                printf '| %s | %s | PASS |\n' "$dialect" "$app" >> "$REPORT"
            else
                err=$(echo "$out" | head -2 | tr '\n' ' ' | head -c 180 | sed 's/|/\\|/g')
                printf '| %s | %s | FAIL: %s |\n' "$dialect" "$app" "$err" >> "$REPORT"
            fi
        done
    done
    rm -rf "$IMPORT_SCRATCH"
fi

echo
echo "report written to $REPORT"
cat "$REPORT"

# Gate exit code: any FAIL row (wire or import) fails the run —
# previously this script always exited 0 and the four-gate protocol
# leaned on a human reading the table.
FAILS=$(grep -c '| FAIL' "$REPORT" || true)
if [[ "$FAILS" -gt 0 ]]; then
    echo "dump-compat: $FAILS failing fixture(s)" >&2
    exit 1
fi
