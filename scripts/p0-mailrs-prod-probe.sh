#!/usr/bin/env bash
# p0-mailrs-prod-probe.sh — v7.38 P0 reproducer for mailrs
# /api/conversations?limit=50 prod 2.5s vs staging 46ms regression.
#
# Sister to wire-latency-probe.sh but seeded to 100 k messages and
# running the EXACT prod SQL from
# `.../mailrs/.claude/notes/spg-7.37.3-prod-conversations-still-2.5s
#  -user-visible-2026-06-18.md`, with two ablation variants:
#   PROD       — verbatim prod SQL with correlated subq on MAX(m.id)
#   NO_SUBQ    — drop the COALESCE subquery, keep DISTINCT aggs
#   MINIMAL    — just GROUP BY + MAX(internal_date) + ORDER BY MAX LIMIT
#
# This isolates whether the prod 50× regression is in:
#   * the correlated subquery (PROD - NO_SUBQ delta on SPGS)
#   * the DISTINCT aggregates  (NO_SUBQ - MINIMAL delta on SPGS)
#   * fundamental plan choice  (MINIMAL ratio SPGS vs PG18)
#
# Usage: scripts/p0-mailrs-prod-probe.sh [SEED_N] [ITERS]
#   SEED_N default 100000, ITERS default 15.
#
# Requires: target/release/spg-server built; spg-bench-postgres
# container on 127.0.0.1:25432 (PG 18 oracle); docker.
set -uo pipefail
cd "$(dirname "$0")/.."

SEED_N="${1:-100000}"
ITERS="${2:-15}"
SPG_BIN="target/release/spg-server"
SPG_PORT=25490
PG_PORT=25432
PG_USER=bench
PG_DB=bench
PSQL_IMG=postgres:18

[ -x "$SPG_BIN" ] || { echo "build first: cargo build --release -p spg-server" >&2; exit 1; }

# ── seed SQL ─────────────────────────────────────────────────────
gen_seed() {
  cat <<'SQL'
DROP TABLE IF EXISTS email_analysis;
DROP TABLE IF EXISTS messages;
DROP TABLE IF EXISTS mailboxes;
CREATE TABLE mailboxes (id BIGSERIAL PRIMARY KEY, name TEXT, user_address TEXT);
CREATE TABLE messages (id BIGSERIAL PRIMARY KEY, mailbox_id BIGINT, thread_id TEXT, subject TEXT, sender TEXT, internal_date BIGINT, flags BIGINT, pinned BOOLEAN, archived BOOLEAN, importance_level TEXT, importance_score REAL, message_id TEXT, text_body TEXT);
CREATE TABLE email_analysis (message_id BIGINT PRIMARY KEY, category TEXT, summary TEXT, requires_action BOOLEAN);
CREATE INDEX idx_messages_thread ON messages(thread_id);
CREATE INDEX idx_messages_thread_date ON messages(thread_id, internal_date DESC);
CREATE INDEX idx_messages_mailbox ON messages(mailbox_id);
CREATE INDEX idx_mailboxes_user ON mailboxes(user_address, name);
DROP TABLE IF EXISTS snoozed_conversations;
CREATE TABLE snoozed_conversations (thread_id TEXT NOT NULL, account_address TEXT NOT NULL, snoozed_until BIGINT NOT NULL, PRIMARY KEY (thread_id, account_address));
SQL
  # 10 mailboxes, single user (matches mailrs prod).
  for i in $(seq 0 9); do
    echo "INSERT INTO mailboxes (name, user_address) VALUES ('mb$i', 'lihao@golia.jp');"
  done
  body=$(printf 'lorem ipsum dolor sit amet %.0s' $(seq 1 20))
  # ~ 5 msgs per thread, 600 senders.
  msgs_per_thread=5
  n_senders=600
  for batch in $(seq 0 $(( SEED_N / 500 - 1 ))); do
    vals=""
    for j in $(seq 0 499); do
      i=$(( batch * 500 + j ))
      mb=$(( i % 10 + 1 ))
      thr=$(( i / msgs_per_thread ))
      snd=$(( i % n_senders ))
      idate=$(( 1700000000 + i ))
      fl=$(( i % 10 < 3 ? 0 : 1 ))
      # message_id empty 20% of time, else 'mid-i'.
      if [ $(( i % 5 )) -eq 0 ]; then mid=""; else mid="mid-$i"; fi
      sep=""; [ -n "$vals" ] && sep=","
      vals="${vals}${sep}($mb, 'th-$thr', 'subj$i', 'sender$snd@example.com', $idate, $fl, false, false, 'normal', 0.5, '$mid', '$body $i')"
    done
    echo "INSERT INTO messages (mailbox_id, thread_id, subject, sender, internal_date, flags, pinned, archived, importance_level, importance_score, message_id, text_body) VALUES $vals;"
  done
  # email_analysis for every 4th message.
  vals=""; n=0
  ea_rows=$(( SEED_N / 4 ))
  for k in $(seq 0 $(( ea_rows - 1 ))); do
    mid=$(( k * 4 + 1 )); cat=$(( k % 5 )); ra=$([ $(( k % 2 )) -eq 0 ] && echo true || echo false)
    sep=""; [ -n "$vals" ] && sep=","
    vals="${vals}${sep}($mid, 'cat$cat', 'summary $k', $ra)"
    n=$(( n + 1 ))
    if [ "$n" -eq 500 ]; then
      echo "INSERT INTO email_analysis (message_id, category, summary, requires_action) VALUES $vals;"
      vals=""; n=0
    fi
  done
  [ -n "$vals" ] && echo "INSERT INTO email_analysis (message_id, category, summary, requires_action) VALUES $vals;"
}

# Verbatim prod SQL (correlated subquery via MAX(m.id)).
PROD="SELECT m.thread_id, MAX(m.subject), string_agg(DISTINCT m.sender, ','), COUNT(DISTINCT CASE WHEN m.message_id != '' THEN m.message_id ELSE CAST(m.id AS TEXT) END), COUNT(DISTINCT CASE WHEN (m.flags & 1) = 0 THEN CASE WHEN m.message_id != '' THEN m.message_id ELSE CAST(m.id AS TEXT) END END), MAX(m.internal_date), COALESCE((SELECT ea2.category FROM email_analysis ea2 WHERE ea2.message_id = MAX(m.id)), 'general') FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id WHERE mb.user_address = 'lihao@golia.jp' GROUP BY m.thread_id ORDER BY MAX(m.internal_date) DESC LIMIT 50"

# Ablation: drop the correlated subquery, keep DISTINCT aggs.
NO_SUBQ="SELECT m.thread_id, MAX(m.subject), string_agg(DISTINCT m.sender, ','), COUNT(DISTINCT CASE WHEN m.message_id != '' THEN m.message_id ELSE CAST(m.id AS TEXT) END), COUNT(DISTINCT CASE WHEN (m.flags & 1) = 0 THEN CASE WHEN m.message_id != '' THEN m.message_id ELSE CAST(m.id AS TEXT) END END), MAX(m.internal_date) FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id WHERE mb.user_address = 'lihao@golia.jp' GROUP BY m.thread_id ORDER BY MAX(m.internal_date) DESC LIMIT 50"

# Ablation: minimal — just the top-K shape.
MINIMAL="SELECT m.thread_id, MAX(m.internal_date) FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id WHERE mb.user_address = 'lihao@golia.jp' GROUP BY m.thread_id ORDER BY MAX(m.internal_date) DESC LIMIT 50"

psql_run() { # port user db [password]  -> reads SQL on stdin
  docker run --rm -i --network host -e PGPASSWORD="${4:-}" "$PSQL_IMG" \
    psql -h 127.0.0.1 -p "$1" -U "$2" -d "$3" -v ON_ERROR_STOP=on -q 2>&1
}

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
  local measured=("${arr[@]:1}")
  if [ "${#measured[@]}" -eq 0 ]; then echo "  $label: NO TIMING (query failed?)"; return; fi
  local sorted
  sorted=$(printf '%s\n' "${measured[@]}" | sort -n)
  local cnt mid median min max
  cnt=$(printf '%s\n' "$sorted" | wc -l | tr -d ' ')
  mid=$(( (cnt + 1) / 2 ))
  median=$(printf '%s\n' "$sorted" | sed -n "${mid}p")
  min=$(printf '%s\n' "$sorted" | head -1)
  max=$(printf '%s\n' "$sorted" | tail -1)
  printf '  %-12s median=%9s ms  min=%9s ms  max=%9s ms  (n=%s)\n' "$label" "$median" "$min" "$max" "$cnt"
}

# Capture an EXPLAIN once on SPG so we have plan-shape on the same N.
explain_on_spg() { # sql
  local sql="$1"
  echo "EXPLAIN $sql;" | psql_run "$SPG_PORT" spg spg "" | sed -n '/^[[:space:]]/p' | head -40
}

echo "== seeding (N=$SEED_N messages) =="
SEED_FILE=$(mktemp); gen_seed > "$SEED_FILE"
echo "  seed: $(wc -l < "$SEED_FILE") statements, $(du -h "$SEED_FILE" | cut -f1)"

# ── PG 18 ────────────────────────────────────────────────────────
echo "== PG 18 (spg-bench-postgres :$PG_PORT) =="
psql_run "$PG_PORT" "$PG_USER" "$PG_DB" "$PG_USER" < "$SEED_FILE" 2>&1 | grep -iE 'error' | head -3 || true
echo "  -- ANALYZE for fair planner comparison --"
echo "ANALYZE;" | psql_run "$PG_PORT" "$PG_USER" "$PG_DB" "$PG_USER" >/dev/null
measure "$PG_PORT" "$PG_USER" "$PG_DB" "bench" "PG_MINIMAL"  "$MINIMAL"
measure "$PG_PORT" "$PG_USER" "$PG_DB" "bench" "PG_NO_SUBQ"  "$NO_SUBQ"
measure "$PG_PORT" "$PG_USER" "$PG_DB" "bench" "PG_PROD"     "$PROD"

# ── SPG ──────────────────────────────────────────────────────────
echo "== SPG ($SPG_BIN, pgwire :$SPG_PORT) =="
SPG_DB_DIR=$(mktemp -d)
SPG_PG_ADDR="127.0.0.1:$SPG_PORT" "$SPG_BIN" "127.0.0.1:25491" "$SPG_DB_DIR/spg.db" - - >/tmp/spg-probe.log 2>&1 &
SPG_PID=$!
trap 'kill -9 "$SPG_PID" 2>/dev/null; rm -rf "$SPG_DB_DIR" "$SEED_FILE"' EXIT
for _ in $(seq 1 80); do grep -q "pg-wire listening" /tmp/spg-probe.log && break; sleep 0.2; done
grep -q "pg-wire listening" /tmp/spg-probe.log || { echo "spg pgwire did not start:"; cat /tmp/spg-probe.log; exit 1; }
psql_run "$SPG_PORT" spg spg "" < "$SEED_FILE" 2>&1 | grep -iE 'error' | head -3 || true
echo "  -- EXPLAIN SPG_PROD --"
explain_on_spg "$PROD"
echo "  -- EXPLAIN SPG_NO_SUBQ --"
explain_on_spg "$NO_SUBQ"
echo "  -- EXPLAIN SPG_MINIMAL --"
explain_on_spg "$MINIMAL"
measure "$SPG_PORT" spg spg "" "SPG_MINIMAL"  "$MINIMAL"
measure "$SPG_PORT" spg spg "" "SPG_NO_SUBQ"  "$NO_SUBQ"
measure "$SPG_PORT" spg spg "" "SPG_PROD"     "$PROD"

echo "== done (N=$SEED_N, ITERS=$ITERS) =="
