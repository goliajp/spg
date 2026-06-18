#!/usr/bin/env bash
# p0-mailrs-sqlx-orch.sh — drive xtests/sqlx-pgwire P0 reproducer.
#
# Why: SPG simple-query parser rejects the mailrs prod SQL
# (`unknown table qualifier: m` in correlated subquery's MAX(m.id))
# but mailrs prod runs it via sqlx extended protocol. Our sqlx test
# can run that path, but seeding via sqlx's INSERT VALUES crashes
# the sqlx client parser at ~50+ tuples (stack overflow). So:
#   1. seed PG18 + SPG via psql simple-query (proven to handle 500-tuple batches)
#   2. start SPG with the seeded db
#   3. cargo test the rust measurement (SPG_P0_SKIP_SEED=1)
#   4. teardown
#
# Usage: scripts/p0-mailrs-sqlx-orch.sh [SEED_N]   default 30000
set -uo pipefail
cd "$(dirname "$0")/.."

SEED_N="${1:-30000}"
SPG_BIN="target/release/spg-server"
SPG_PORT=25490
SPG_LISTEN_PORT=25491
PG_PORT=25432
PG_USER=bench
PG_DB=bench
PSQL_IMG=postgres:18

[ -x "$SPG_BIN" ] || { echo "build first: cargo build --release -p spg-server" >&2; exit 1; }

# Use the same seed-gen approach as the existing probe.
gen_seed() {
  cat <<'SQL'
DROP TABLE IF EXISTS email_analysis;
DROP TABLE IF EXISTS messages;
DROP TABLE IF EXISTS mailboxes;
CREATE TABLE mailboxes (id BIGSERIAL PRIMARY KEY, name TEXT, user_address TEXT);
CREATE TABLE messages (id BIGSERIAL PRIMARY KEY, mailbox_id BIGINT, thread_id TEXT, subject TEXT, sender TEXT, internal_date BIGINT, flags BIGINT, message_id TEXT);
CREATE TABLE email_analysis (message_id BIGINT PRIMARY KEY, category TEXT);
CREATE INDEX idx_messages_thread ON messages(thread_id);
CREATE INDEX idx_messages_thread_date ON messages(thread_id, internal_date DESC);
CREATE INDEX idx_messages_mailbox ON messages(mailbox_id);
CREATE INDEX idx_mailboxes_user ON mailboxes(user_address, name);
SQL
  for i in $(seq 0 9); do
    echo "INSERT INTO mailboxes (name, user_address) VALUES ('mb$i', 'lihao@golia.jp');"
  done
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
      if [ $(( i % 10 )) -lt 3 ]; then fl=0; else fl=1; fi
      if [ $(( i % 5 )) -eq 0 ]; then mid=""; else mid="mid-$i"; fi
      sep=""; [ -n "$vals" ] && sep=","
      vals="${vals}${sep}($mb, 'th-$thr', 'subj$i', 'sender$snd@example.com', $idate, $fl, '$mid')"
    done
    echo "INSERT INTO messages (mailbox_id, thread_id, subject, sender, internal_date, flags, message_id) VALUES $vals;"
  done
  vals=""; n=0
  ea_rows=$(( SEED_N / 4 ))
  for k in $(seq 0 $(( ea_rows - 1 ))); do
    mid=$(( k * 4 + 1 )); cat=$(( k % 5 ))
    sep=""; [ -n "$vals" ] && sep=","
    vals="${vals}${sep}($mid, 'cat$cat')"
    n=$(( n + 1 ))
    if [ "$n" -eq 500 ]; then
      echo "INSERT INTO email_analysis (message_id, category) VALUES $vals;"
      vals=""; n=0
    fi
  done
  [ -n "$vals" ] && echo "INSERT INTO email_analysis (message_id, category) VALUES $vals;"
}

psql_run() { # port user db pass
  docker run --rm -i --network host -e PGPASSWORD="${4:-}" "$PSQL_IMG" \
    psql -h 127.0.0.1 -p "$1" -U "$2" -d "$3" -v ON_ERROR_STOP=on -q 2>&1
}

echo "== seeding (N=$SEED_N) =="
SEED_FILE=$(mktemp); gen_seed > "$SEED_FILE"
echo "  seed: $(wc -l < "$SEED_FILE") stmts, $(du -h "$SEED_FILE" | cut -f1)"

echo "  -- seed PG18 --"
psql_run "$PG_PORT" "$PG_USER" "$PG_DB" "$PG_USER" < "$SEED_FILE" | grep -iE 'error' | head -3 || true
echo "ANALYZE;" | psql_run "$PG_PORT" "$PG_USER" "$PG_DB" "$PG_USER" >/dev/null 2>&1 || true

echo "== start SPG =="
SPG_DB_DIR=$(mktemp -d)
SPG_PG_ADDR="127.0.0.1:$SPG_PORT" "$SPG_BIN" "127.0.0.1:$SPG_LISTEN_PORT" "$SPG_DB_DIR/spg.db" - - >/tmp/spg-orch.log 2>&1 &
SPG_PID=$!
trap 'kill -9 "$SPG_PID" 2>/dev/null; rm -rf "$SPG_DB_DIR" "$SEED_FILE"' EXIT
for _ in $(seq 1 80); do grep -q "pg-wire listening" /tmp/spg-orch.log && break; sleep 0.2; done
grep -q "pg-wire listening" /tmp/spg-orch.log || { echo "spg pgwire did not start"; cat /tmp/spg-orch.log; exit 1; }

echo "  -- seed SPG --"
psql_run "$SPG_PORT" spg spg "" < "$SEED_FILE" | grep -iE 'error' | head -3 || true

echo "== sqlx measurements =="
export SPG_PG_URL="postgres://spg:@127.0.0.1:$SPG_PORT/spg"
export PG_URL="postgres://$PG_USER:$PG_USER@127.0.0.1:$PG_PORT/$PG_DB"
export SPG_P0_SEED_N="$SEED_N"
export SPG_P0_SKIP_SEED=1

echo "  -- PG18 via sqlx --"
cargo test --release --locked -p spg-sqlx-pgwire --test p0_mailrs_prod p0_mailrs_prod_via_pg18_measure -- --nocapture --include-ignored 2>&1 | tail -25
echo "  -- SPGS via sqlx --"
cargo test --release --locked -p spg-sqlx-pgwire --test p0_mailrs_prod p0_mailrs_prod_via_spgs_measure -- --nocapture --include-ignored 2>&1 | tail -25

echo "== done (N=$SEED_N) =="
