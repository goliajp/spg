#!/usr/bin/env bash
# wire-latency-probe.sh — B3 server-wire latency probe (SPG vs PG 18).
#
# Why: the architecture-v2 acceptance line says server(wire) < 54 ms,
# but it was never actually measured post-7.32. And the remaining P4
# work (projection borrow channel, increment 3) targets the wire path
# specifically — it only pays off if the per-cell projection clone is a
# material slice of the wire-path cost. This probe measures that, on the
# SAME machine through the SAME dockerized psql client, so the SPG/PG
# ratio is robust to moderate background load (both pay it equally).
#
# Two query shapes over the seeded 25k inbox dataset (same seed as the
# inbox_25k perf gate):
#   INBOX — the production aggregate inbox query (GROUP BY + LIMIT 50).
#           Aggregate path is already borrow-channelled (P4 increment 2),
#           so this is a cross-check against the 35 ms embed line, NOT an
#           increment-3 exerciser.
#   PROJ  — a non-aggregate high-row join projection returning all 25k
#           rows. THIS is where the residual materialise()/extend_masked
#           per-cell clone bites (one owned combined Row per output row).
#           If increment 3 helps anywhere on the wire, it is here.
#
# Usage: scripts/wire-latency-probe.sh [N_ITERS]   (default 15)
#
# Requires: target/release/spg-server built; the spg-bench-postgres
# container (pgvector/pgvector:pg18) running on 127.0.0.1:25432; docker.

set -euo pipefail
cd "$(dirname "$0")/.."

ITERS="${1:-15}"
SPG_BIN="target/release/spg-server"
SPG_PORT=25490
PG_PORT=25432
PG_USER=bench
PG_DB=bench
PSQL_IMG=postgres:18

[ -x "$SPG_BIN" ] || { echo "build first: cargo build --release -p spg-server" >&2; exit 1; }

# ── seed SQL (same shape as crates/spg-server/tests/perf_gate/inbox_25k.rs)
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
  # 30 mailboxes
  for i in $(seq 0 29); do
    echo "INSERT INTO mailboxes (name, user_address) VALUES ('mb$i', 'u@x');"
  done
  # 25k messages in 50 batches of 500 — ~1 KB body each
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
  # 6000 email_analysis rows in batches of 500
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

INBOX="SELECT m.thread_id, MAX(m.subject), COUNT(DISTINCT m.id), MAX(m.internal_date), COALESCE((SELECT e2.category FROM email_analysis e2 JOIN messages m2 ON e2.message_id = m2.id WHERE m2.thread_id = m.thread_id ORDER BY m2.internal_date DESC LIMIT 1), 'general'), COALESCE((SELECT e3.summary FROM email_analysis e3 JOIN messages m3 ON e3.message_id = m3.id WHERE m3.thread_id = m.thread_id ORDER BY m3.internal_date DESC LIMIT 1), ''), COALESCE((SELECT LEFT(m4.text_body, 120) FROM messages m4 WHERE m4.thread_id = m.thread_id ORDER BY m4.internal_date DESC LIMIT 1), ''), BOOL_OR(m.pinned), BOOL_OR(m.archived), COALESCE((array_agg(m.importance_level ORDER BY m.importance_score DESC NULLS LAST))[1], 'normal'), COALESCE(MAX(m.importance_score), 0.0), COALESCE(BOOL_OR(ea.requires_action), false) FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id LEFT JOIN email_analysis ea ON ea.message_id = m.id WHERE mb.user_address = 'u@x' AND m.thread_id != '' GROUP BY m.thread_id HAVING BOOL_OR(m.archived) = false ORDER BY MAX(m.internal_date) DESC LIMIT 50"

# Non-aggregate, all-rows join projection — the materialise() exerciser.
PROJ="SELECT m.id, m.subject, m.sender, m.internal_date, mb.user_address FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id WHERE mb.user_address = 'u@x'"

# psql client via docker, --network host, against localhost:PORT.
# 4th arg = password (empty for SPG's open-mode auth).
psql_run() { # port user db [password]  -> reads SQL on stdin
  docker run --rm -i --network host -e PGPASSWORD="${4:-}" "$PSQL_IMG" \
    psql -h 127.0.0.1 -p "$1" -U "$2" -d "$3" -v ON_ERROR_STOP=on -q 2>&1
}

# Run one query ITERS times with \timing; print median + min ms.
# First iteration is warm-up (plan cache / allocator) and dropped.
measure() { # port user db pass label sql
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
  # drop first (warm-up)
  local measured=("${arr[@]:1}")
  if [ "${#measured[@]}" -eq 0 ]; then echo "  $label: NO TIMING (query failed?)"; return; fi
  local sorted
  sorted=$(printf '%s\n' "${measured[@]}" | sort -n)
  local cnt mid median min
  cnt=$(printf '%s\n' "$sorted" | wc -l | tr -d ' ')
  mid=$(( (cnt + 1) / 2 ))
  median=$(printf '%s\n' "$sorted" | sed -n "${mid}p")
  min=$(printf '%s\n' "$sorted" | head -1)
  printf '  %-6s median=%8s ms   min=%8s ms   (n=%s)\n' "$label" "$median" "$min" "$cnt"
}

echo "== seeding =="
SEED_FILE=$(mktemp); gen_seed > "$SEED_FILE"
echo "  seed: $(wc -l < "$SEED_FILE") statements, $(du -h "$SEED_FILE" | cut -f1)"

# ── PG 18 target ──────────────────────────────────────────────────
echo "== PG 18 (spg-bench-postgres :$PG_PORT) =="
psql_run "$PG_PORT" "$PG_USER" "$PG_DB" "$PG_USER" < "$SEED_FILE" | grep -iE 'error' && echo "  (pg seed errors above)" || true
measure "$PG_PORT" "$PG_USER" "$PG_DB" "bench" "INBOX" "$INBOX"
measure "$PG_PORT" "$PG_USER" "$PG_DB" "bench" "PROJ"  "$PROJ"

# ── SPG target ────────────────────────────────────────────────────
echo "== SPG ($SPG_BIN, pgwire :$SPG_PORT) =="
SPG_DB_DIR=$(mktemp -d)
SPG_PG_ADDR="127.0.0.1:$SPG_PORT" "$SPG_BIN" "127.0.0.1:25491" "$SPG_DB_DIR/spg.db" - - >/tmp/spg-probe.log 2>&1 &
SPG_PID=$!
trap 'kill -9 "$SPG_PID" 2>/dev/null; rm -rf "$SPG_DB_DIR" "$SEED_FILE"' EXIT
# wait for pgwire to bind
for _ in $(seq 1 50); do grep -q "pg-wire listening" /tmp/spg-probe.log && break; sleep 0.1; done
grep -q "pg-wire listening" /tmp/spg-probe.log || { echo "spg pgwire did not start:"; cat /tmp/spg-probe.log; exit 1; }
psql_run "$SPG_PORT" spg spg "" < "$SEED_FILE" | grep -iE 'error' && echo "  (spg seed errors above)" || true
measure "$SPG_PORT" spg spg "" "INBOX" "$INBOX"
measure "$SPG_PORT" spg spg "" "PROJ"  "$PROJ"

echo "== done (machine load matters; take authoritative numbers on a cold host) =="
