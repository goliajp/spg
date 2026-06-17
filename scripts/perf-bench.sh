#!/usr/bin/env bash
# perf-bench.sh — v7.34+ baseline harness (SPGS vs PG18 wire path).
#
# Runs the six baseline shapes through SPGS (target/release/spg-server
# pgwire) AND a sibling spg-bench-postgres docker container (postgres:18
# :25432). Each shape: 1 warm + 10 measured iterations, median + min.
#
# Pair this with `cargo test --release -p spg-server --test perf_gate
# baseline_ -- --nocapture` for the SPGE-side numbers; the
# `.claude/notes/perf-baseline-v*.md` ledger merges the three columns
# and applies the red lines (SPGS vs PG18 / SPGE vs SPGS) per
# [[feedback-spgs-spge-perf-bar]].
#
# Naming discipline:
#   * SPGS = server wire (this script, pgwire over psql).
#   * SPGE = embedded (the perf_gate baseline_* tests).
#
# Requires: target/release/spg-server built; spg-bench-postgres up on
# 127.0.0.1:25432 (postgres:18, user/db = bench); docker (OrbStack /
# Docker Desktop / etc.).
#
# Usage: scripts/perf-bench.sh [N_ITERS]   (default 11 = 1 warm + 10)

set -euo pipefail
cd "$(dirname "$0")/.."

ITERS="${1:-11}"
SPG_BIN="target/release/spg-server"
SPG_PORT=25490
PG_PORT=25432
PG_USER=bench
PG_DB=bench
PSQL_IMG=postgres:18

[ -x "$SPG_BIN" ] || { echo "build first: cargo build --release -p spg-server" >&2; exit 1; }

# ── seed SQL: identical to baseline.rs / inbox_25k.rs / wire-latency-probe.sh.
gen_seed() {
  cat <<'SQL'
DROP TABLE IF EXISTS messages;
DROP TABLE IF EXISTS mailboxes;
DROP TABLE IF EXISTS email_analysis;
CREATE TABLE mailboxes (id BIGSERIAL PRIMARY KEY, name TEXT, user_address TEXT);
CREATE TABLE messages (id BIGSERIAL PRIMARY KEY, mailbox_id BIGINT, thread_id TEXT, subject TEXT, sender TEXT, internal_date BIGINT, flags BIGINT, pinned BOOLEAN, archived BOOLEAN, importance_level TEXT, importance_score REAL, message_id TEXT, text_body TEXT);
CREATE TABLE email_analysis (message_id BIGINT PRIMARY KEY, category TEXT, summary TEXT, requires_action BOOLEAN);
CREATE INDEX idx_thread ON messages(thread_id);
SQL
  for i in $(seq 0 29); do
    echo "INSERT INTO mailboxes (name, user_address) VALUES ('mb$i', 'u@x');"
  done
  body=$(printf 'lorem ipsum dolor sit amet %.0s' $(seq 1 40))
  for batch in $(seq 0 49); do
    vals=""
    for j in $(seq 0 499); do
      i=$(( batch * 500 + j ))
      mb=$(( i % 30 + 1 ))
      snd=$(( i % 100 ))
      idate=$(( 1700000000 + i ))
      fl=$(( i % 8 ))
      sep=""; [ -n "$vals" ] && sep=","
      vals="${vals}${sep}($mb, 'th-$i', 'subject $i', 's$snd@x', $idate, $fl, false, false, 'normal', 0.5, 'mid-$i', '$body $i')"
    done
    echo "INSERT INTO messages (mailbox_id, thread_id, subject, sender, internal_date, flags, pinned, archived, importance_level, importance_score, message_id, text_body) VALUES $vals;"
  done
  vals=""; n=0
  for i in $(seq 0 5999); do
    mid=$(( i * 4 + 1 )); cat=$(( i % 5 )); ra=$([ $(( i % 2 )) -eq 0 ] && echo true || echo false)
    sep=""; [ -n "$vals" ] && sep=","
    vals="${vals}${sep}($mid, 'cat$cat', 'summary $i', $ra)"
    n=$(( n + 1 ))
    if [ "$n" -eq 500 ]; then
      echo "INSERT INTO email_analysis (message_id, category, summary, requires_action) VALUES $vals;"
      vals=""; n=0
    fi
  done
}

# ── shape definitions — must mirror crates/spg-server/tests/perf_gate/baseline.rs.

SQL_SELECT_1="SELECT id FROM messages WHERE id = 1"
SQL_SELECT_COUNT_STAR="SELECT COUNT(*) FROM messages"
SQL_PROJ_25K="SELECT m.id, m.subject, m.sender, m.internal_date, mb.user_address FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id WHERE mb.user_address = 'u@x'"
SQL_INBOX_25K="SELECT m.thread_id, MAX(m.subject), COUNT(DISTINCT m.id), MAX(m.internal_date), COALESCE((SELECT e2.category FROM email_analysis e2 JOIN messages m2 ON e2.message_id = m2.id WHERE m2.thread_id = m.thread_id ORDER BY m2.internal_date DESC LIMIT 1), 'general'), COALESCE((SELECT e3.summary FROM email_analysis e3 JOIN messages m3 ON e3.message_id = m3.id WHERE m3.thread_id = m.thread_id ORDER BY m3.internal_date DESC LIMIT 1), ''), COALESCE((SELECT LEFT(m4.text_body, 120) FROM messages m4 WHERE m4.thread_id = m.thread_id ORDER BY m4.internal_date DESC LIMIT 1), ''), BOOL_OR(m.pinned), BOOL_OR(m.archived), COALESCE((array_agg(m.importance_level ORDER BY m.importance_score DESC NULLS LAST))[1], 'normal'), COALESCE(MAX(m.importance_score), 0.0), COALESCE(BOOL_OR(ea.requires_action), false) FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id LEFT JOIN email_analysis ea ON ea.message_id = m.id WHERE mb.user_address = 'u@x' AND m.thread_id != '' GROUP BY m.thread_id HAVING BOOL_OR(m.archived) = false ORDER BY MAX(m.internal_date) DESC LIMIT 50"
SQL_EXISTS_IN_60="SELECT COUNT(*) FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id WHERE mb.user_address = 'u@x' AND EXISTS (SELECT 1 FROM email_analysis ea WHERE ea.message_id = m.id)"

# Build the 60 thread-id IN-list identical to baseline.rs.
gen_thread_ids_60() {
  local i out=""
  for i in $(seq 0 59); do
    sep=""; [ -n "$out" ] && sep=","
    out="${out}${sep}'th-$(( i * 400 ))'"
  done
  printf '%s' "$out"
}
THREAD_IDS_60=$(gen_thread_ids_60)
SQL_GET_CONVS="SELECT m.thread_id, MAX(m.internal_date), COALESCE((SELECT LEFT(ea.summary, 80) FROM email_analysis ea JOIN messages m_snip ON ea.message_id = m_snip.id WHERE m_snip.thread_id = m.thread_id AND ea.summary IS NOT NULL AND ea.summary != '' ORDER BY m_snip.internal_date DESC LIMIT 1), '') FROM messages m WHERE m.thread_id IN (${THREAD_IDS_60}) GROUP BY m.thread_id"

# ── psql client harness (reused from wire-latency-probe.sh).
psql_run() {
  docker run --rm -i --network host -e PGPASSWORD="${4:-}" "$PSQL_IMG" \
    psql -h 127.0.0.1 -p "$1" -U "$2" -d "$3" -v ON_ERROR_STOP=on -q 2>&1
}

# n=ITERS, drop first as warm-up.
measure() {
  local port="$1" user="$2" db="$3" pass="$4" label="$5" sql="$6"
  local script="\\timing on
"
  local k
  for k in $(seq 1 "$ITERS"); do script="${script}${sql};
"; done
  local times
  times=$(printf '%s' "$script" | psql_run "$port" "$user" "$db" "$pass" \
            | grep -oE 'Time: [0-9.]+ ms' | awk '{print $2}')
  local arr=()
  while IFS= read -r t; do [ -n "$t" ] && arr+=("$t"); done <<< "$times"
  local measured=("${arr[@]:1}")
  if [ "${#measured[@]}" -eq 0 ]; then printf '  %-26s NO TIMING\n' "$label"; return; fi
  local sorted
  sorted=$(printf '%s\n' "${measured[@]}" | sort -n)
  local cnt mid median min
  cnt=$(printf '%s\n' "$sorted" | wc -l | tr -d ' ')
  mid=$(( (cnt + 1) / 2 ))
  median=$(printf '%s\n' "$sorted" | sed -n "${mid}p")
  min=$(printf '%s\n' "$sorted" | head -1)
  printf '  %-26s median=%8s ms   min=%8s ms   (n=%s)\n' "$label" "$median" "$min" "$cnt"
}

echo "== seeding =="
SEED_FILE=$(mktemp); gen_seed > "$SEED_FILE"
echo "  seed: $(wc -l < "$SEED_FILE") statements, $(du -h "$SEED_FILE" | cut -f1)"

echo "== PG 18 (spg-bench-postgres :$PG_PORT) =="
psql_run "$PG_PORT" "$PG_USER" "$PG_DB" "$PG_USER" < "$SEED_FILE" | grep -iE 'error' && echo "  (pg seed errors above)" || true
measure "$PG_PORT" "$PG_USER" "$PG_DB" "bench" "select_1"               "$SQL_SELECT_1"
measure "$PG_PORT" "$PG_USER" "$PG_DB" "bench" "select_count_star"      "$SQL_SELECT_COUNT_STAR"
measure "$PG_PORT" "$PG_USER" "$PG_DB" "bench" "proj_25k"               "$SQL_PROJ_25K"
measure "$PG_PORT" "$PG_USER" "$PG_DB" "bench" "inbox_25k"              "$SQL_INBOX_25K"
measure "$PG_PORT" "$PG_USER" "$PG_DB" "bench" "exists_in_60"           "$SQL_EXISTS_IN_60"
measure "$PG_PORT" "$PG_USER" "$PG_DB" "bench" "get_conversations_in_60" "$SQL_GET_CONVS"

echo "== SPGS ($SPG_BIN, pgwire :$SPG_PORT) =="
SPG_DB_DIR=$(mktemp -d)
SPG_PG_ADDR="127.0.0.1:$SPG_PORT" "$SPG_BIN" "127.0.0.1:25491" "$SPG_DB_DIR/spg.db" - - >/tmp/spg-probe.log 2>&1 &
SPG_PID=$!
trap 'kill -9 "$SPG_PID" 2>/dev/null; rm -rf "$SPG_DB_DIR" "$SEED_FILE"' EXIT
for _ in $(seq 1 50); do grep -q "pg-wire listening" /tmp/spg-probe.log && break; sleep 0.1; done
grep -q "pg-wire listening" /tmp/spg-probe.log || { echo "spg pgwire did not start:"; cat /tmp/spg-probe.log; exit 1; }
psql_run "$SPG_PORT" spg spg "" < "$SEED_FILE" | grep -iE 'error' && echo "  (spg seed errors above)" || true
measure "$SPG_PORT" spg spg "" "select_1"               "$SQL_SELECT_1"
measure "$SPG_PORT" spg spg "" "select_count_star"      "$SQL_SELECT_COUNT_STAR"
measure "$SPG_PORT" spg spg "" "proj_25k"               "$SQL_PROJ_25K"
measure "$SPG_PORT" spg spg "" "inbox_25k"              "$SQL_INBOX_25K"
measure "$SPG_PORT" spg spg "" "exists_in_60"           "$SQL_EXISTS_IN_60"
measure "$SPG_PORT" spg spg "" "get_conversations_in_60" "$SQL_GET_CONVS"

echo "== done =="
echo "Pair this output with:"
echo "  cargo test --release -p spg-server --test perf_gate baseline_ -- --nocapture 2>&1 | grep 'SPGE:'"
echo "to populate the SPGE column in the .claude/notes/perf-baseline-v*.md ledger."
echo "Red lines: SPGS >= PG18 wire ; SPGE measurably faster than SPGS (else wire arbitrage, not real internal speed)."
