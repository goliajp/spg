#!/usr/bin/env bash
# v7.18 — SPG drop-in acceptance harness.
#
# Goal: a PG / MySQL / MariaDB user with an unmodified stock
# application can run this single script against a target SPG image
# and immediately see whether their dialect drops in cleanly. No
# 6-month migration discovery — yes/no in under a minute.
#
# What it does right now (Phase 1, v7.18 epic T11):
#
#   * boots a goliakk/spg:7.17.0 (or user-specified) container on
#     loopback, exposes pgwire 5432 on a non-default host port
#   * feeds a PG dialect probe panel (D-pre acceptance + the v7.17
#     type matrix + common stock-PG patterns) over the wire as
#     individual psql -c invocations
#   * captures pass/fail per case + the first ERROR line on fail
#   * emits a markdown report (configurable path)
#   * exit code: 0 all pass / 1 any fail / 2 harness error
#
# Phase 2 (T11+, post v7.18):
#
#   * --app <repo-url-or-path> mode: clone target app, auto-detect
#     test runner (cargo / npm / pytest / mvn), point the app's DB
#     URL at the SPG container, run the user's own test suite, fold
#     the pass/fail into the same report.
#   * --dialect mysql|mariadb panels: SPG_MYSQLWIRE_ADDR-enabled
#     entry point.
#
# Usage:
#
#   scripts/dropin-acceptance.sh \
#       [--image IMAGE]     # default goliakk/spg:7.17.0
#       [--port PORT]       # default 25433
#       [--report PATH]     # default ./dropin-acceptance-report.md
#       [--keep-container]  # don't stop on exit (useful for debug)
#       [--no-pull]         # skip `docker pull` of the image

set -u

IMAGE="goliakk/spg:7.17.0"
PORT="25433"
REPORT="./dropin-acceptance-report.md"
KEEP=0
NO_PULL=0
CONTAINER_NAME="spg-dropin-$$"
FIXTURES=()

while [ $# -gt 0 ]; do
  case "$1" in
    --image) IMAGE="$2"; shift 2 ;;
    --port) PORT="$2"; shift 2 ;;
    --report) REPORT="$2"; shift 2 ;;
    --keep-container) KEEP=1; shift ;;
    --no-pull) NO_PULL=1; shift ;;
    --fixture)
      # SQL file to feed to SPG as one chunk; each --fixture
      # adds another file (applied in argv order, useful for
      # pg-extensions.sql before init-schema.sql).
      FIXTURES+=("$2")
      shift 2
      ;;
    -h|--help)
      sed -n '2,50p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "unknown arg: $1" >&2
      exit 2
      ;;
  esac
done

cleanup() {
  if [ "$KEEP" -eq 0 ]; then
    docker stop "$CONTAINER_NAME" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

echo "=== SPG drop-in acceptance ==="
echo "image:  $IMAGE"
echo "port:   $PORT"
echo "report: $REPORT"
echo ""

if [ "$NO_PULL" -eq 0 ]; then
  echo "[setup] docker pull $IMAGE"
  docker pull "$IMAGE" >/dev/null || {
    echo "harness error: docker pull failed for $IMAGE" >&2
    exit 2
  }
fi

echo "[setup] starting $CONTAINER_NAME on host port $PORT"
docker run --rm -d --name "$CONTAINER_NAME" -p "$PORT:5432" "$IMAGE" >/dev/null || {
  echo "harness error: docker run failed" >&2
  exit 2
}

# Wait for the pgwire listener to come up.
for _ in $(seq 1 30); do
  if docker run --rm --network host postgres:16-alpine \
      pg_isready -h localhost -p "$PORT" -U spg >/dev/null 2>&1; then
    break
  fi
  sleep 1
done

PSQL='docker run --rm -i --network host postgres:16-alpine psql -h localhost -p '"$PORT"' -U spg -d spg -v ON_ERROR_STOP=on -q'

PASS_COUNT=0
FAIL_COUNT=0
declare -a CASES   # entries: "name|status|first_err"

run_case() {
  local name="$1"
  local sql="$2"
  local out rc
  out=$(echo "$sql" | $PSQL 2>&1)
  rc=$?
  if [ "$rc" -eq 0 ]; then
    PASS_COUNT=$((PASS_COUNT+1))
    CASES+=("$name|PASS|")
  else
    FAIL_COUNT=$((FAIL_COUNT+1))
    local first_err
    first_err=$(echo "$out" | grep -E "^(ERROR|psql: error)" | head -1 | sed 's/^ERROR:\s*//' | tr -s ' ')
    CASES+=("$name|FAIL|$first_err")
  fi
}

echo ""
echo "=== PG dialect panel ==="

# --- mailrs D-pre #1 reverse (tsvector ops) ---
echo "[panel] D-pre #1 tsvector"
run_case "D-pre.1.to_tsvector" \
  "CREATE TABLE m1a (id INT, sv tsvector); INSERT INTO m1a VALUES (1, to_tsvector('english', 'hello world'));"
run_case "D-pre.1.match_plainto" \
  "CREATE TABLE m1b (id INT, sv tsvector); INSERT INTO m1b VALUES (1, to_tsvector('english', 'hello')); SELECT id FROM m1b WHERE sv @@ plainto_tsquery('hello');"
run_case "D-pre.1.match_to_tsquery" \
  "CREATE TABLE m1c (id INT, sv tsvector); INSERT INTO m1c VALUES (1, to_tsvector('english', 'hello world')); SELECT id FROM m1c WHERE sv @@ to_tsquery('hello & world');"
run_case "D-pre.1.ts_rank" \
  "CREATE TABLE m1d (id INT, sv tsvector); INSERT INTO m1d VALUES (1, to_tsvector('english', 'hello')); SELECT ts_rank(sv, plainto_tsquery('hello')) FROM m1d;"
run_case "D-pre.1.phraseto_tsquery" \
  "CREATE TABLE m1e (id INT, sv tsvector); INSERT INTO m1e VALUES (1, to_tsvector('english', 'hello world')); SELECT id FROM m1e WHERE sv @@ phraseto_tsquery('hello world');"

# --- mailrs D-pre #2 reverse (TEXT[] array surface) ---
echo "[panel] D-pre #2 TEXT[]"
run_case "D-pre.2.literal" \
  "CREATE TABLE oc_a (id INT, redirect_uris TEXT[] NOT NULL DEFAULT '{}'::text[]); INSERT INTO oc_a VALUES (1, '{https://a.com,https://b.com}');"
run_case "D-pre.2.any" \
  "CREATE TABLE oc_b (id INT, redirect_uris TEXT[]); INSERT INTO oc_b VALUES (1, '{a,b,c}'); SELECT id FROM oc_b WHERE 'a' = ANY(redirect_uris);"
run_case "D-pre.2.array_length" \
  "CREATE TABLE oc_d (id INT, redirect_uris TEXT[]); INSERT INTO oc_d VALUES (1, '{a,b,c}'); SELECT array_length(redirect_uris, 1) FROM oc_d;"
run_case "D-pre.2.subscript" \
  "CREATE TABLE oc_e (id INT, redirect_uris TEXT[]); INSERT INTO oc_e VALUES (1, '{a,b,c}'); SELECT redirect_uris[1] FROM oc_e;"
run_case "D-pre.2.array_agg" \
  "CREATE TABLE oc_f (id INT, val TEXT); INSERT INTO oc_f VALUES (1,'a'),(1,'b'); SELECT array_agg(val) FROM oc_f;"
# v7.19 P5 — projection-position unnest shipped; probe active.
run_case "D-pre.2.unnest_projection" \
  "CREATE TABLE oc_c (id INT, redirect_uris TEXT[]); INSERT INTO oc_c VALUES (1, '{a,b,c}'); SELECT unnest(redirect_uris) FROM oc_c;"

# --- mailrs D-pre #3 reverse (BYTEA wire) ---
echo "[panel] D-pre #3 BYTEA"
run_case "D-pre.3.hex_literal_pg_escape" \
  "CREATE TABLE oq_a (id INT, msg BYTEA); INSERT INTO oq_a VALUES (1, E'\\\\xdeadbeef'::bytea);"
run_case "D-pre.3.hex_literal_double_backslash" \
  "CREATE TABLE oq_b (id INT, msg BYTEA); INSERT INTO oq_b VALUES (1, '\\xdeadbeef');"
run_case "D-pre.3.cast_round_trip" \
  "CREATE TABLE oq_c (id INT, msg BYTEA); INSERT INTO oq_c VALUES (1, 'hello'::bytea); SELECT msg FROM oq_c;"
run_case "D-pre.3.octet_length_with_cast" \
  "CREATE TABLE oq_d (id INT, msg BYTEA); INSERT INTO oq_d VALUES (1, 'hello'::bytea); SELECT octet_length(msg) FROM oq_d;"

# --- mailrs D-pre #4 reverse (reserved-word columns, ivfflat, BIGSERIAL inline PK, multi-col idx) ---
echo "[panel] D-pre #4 reserved words / index access methods"
run_case "D-pre.4.reserved_col_key_unquoted" \
  "CREATE TABLE gt1 (key TEXT NOT NULL, val TEXT);"
run_case "D-pre.4.reserved_col_key_quoted" \
  "CREATE TABLE gt2 (\"key\" TEXT NOT NULL, val TEXT);"
run_case "D-pre.4.ivfflat_index" \
  "CREATE TABLE vt1 (id INT, v vector(8)); CREATE INDEX vt1_idx ON vt1 USING ivfflat (v vector_cosine_ops) WITH (lists=20);"
run_case "D-pre.4.hnsw_vector_cosine" \
  "CREATE TABLE vt2 (id INT, v vector(8)); CREATE INDEX vt2_idx ON vt2 USING hnsw (v vector_cosine_ops);"
run_case "D-pre.4.bigserial_inline_pk" \
  "CREATE TABLE bs1 (id BIGSERIAL PRIMARY KEY, name TEXT);"
run_case "D-pre.4.multi_col_index_create" \
  "CREATE TABLE mc1 (a INT, b INT, c INT); CREATE INDEX mc1_idx ON mc1 (a, b, c);"
run_case "D-pre.4.multi_col_index_seek" \
  "CREATE TABLE mc2 (a INT, b INT, c INT); CREATE INDEX mc2_idx ON mc2 (a, b, c); INSERT INTO mc2 VALUES (1,2,3); SELECT * FROM mc2 WHERE a = 1 AND b = 2 AND c = 3;"

# --- mailrs D-pre d9c5f2d (table name 'contacts') ---
echo "[panel] D-pre d9c5f2d table name 'contacts'"
run_case "D-pre.5.table_name_contacts" \
  "CREATE TABLE contacts (id INT, name TEXT);"

# --- General PG type matrix (covers gap doc's 13 columns) ---
echo "[panel] v7.17 type matrix sanity"
run_case "type.bigint" \
  "CREATE TABLE tt_bi (a BIGINT); INSERT INTO tt_bi VALUES (9223372036854775807); SELECT * FROM tt_bi;"
run_case "type.timestamptz" \
  "CREATE TABLE tt_tz (a TIMESTAMPTZ); INSERT INTO tt_tz VALUES ('2026-01-02 03:04:05+00'); SELECT * FROM tt_tz;"
run_case "type.json_jsonb" \
  "CREATE TABLE tt_j (a JSON, b JSONB); INSERT INTO tt_j VALUES ('{\"k\":1}', '{\"k\":2}'); SELECT * FROM tt_j;"
run_case "type.uuid_gen" \
  "CREATE TABLE tt_u (id UUID DEFAULT gen_random_uuid(), name TEXT); INSERT INTO tt_u (name) VALUES ('alice'); SELECT name FROM tt_u;"
run_case "type.numeric" \
  "CREATE TABLE tt_n (price NUMERIC(10,2)); INSERT INTO tt_n VALUES (123.45); SELECT * FROM tt_n;"
run_case "type.bytea_pg_escape" \
  "CREATE TABLE tt_b (a BYTEA); INSERT INTO tt_b VALUES (E'\\\\xcafe'::bytea); SELECT * FROM tt_b;"

# --- Common stock-PG patterns ---
echo "[panel] common stock-PG patterns"
run_case "stock.on_conflict_do_nothing" \
  "CREATE TABLE oc_dn (id INT PRIMARY KEY); INSERT INTO oc_dn VALUES (1) ON CONFLICT DO NOTHING; INSERT INTO oc_dn VALUES (1) ON CONFLICT DO NOTHING; SELECT count(*) FROM oc_dn;"
run_case "stock.on_conflict_do_update" \
  "CREATE TABLE oc_du (id INT PRIMARY KEY, n INT); INSERT INTO oc_du VALUES (1, 1); INSERT INTO oc_du VALUES (1, 2) ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n; SELECT n FROM oc_du WHERE id = 1;"
run_case "stock.returning" \
  "CREATE TABLE rt (id INT, name TEXT); INSERT INTO rt VALUES (1, 'alice') RETURNING id, name;"
run_case "stock.cte" \
  "CREATE TABLE cte_t (n INT); INSERT INTO cte_t VALUES (1), (2), (3); WITH s AS (SELECT n FROM cte_t WHERE n > 1) SELECT * FROM s;"
run_case "stock.fk_cascade" \
  "CREATE TABLE p (id INT PRIMARY KEY); CREATE TABLE c (id INT, p_id INT REFERENCES p(id) ON DELETE CASCADE); INSERT INTO p VALUES (1); INSERT INTO c VALUES (1, 1); DELETE FROM p WHERE id = 1; SELECT count(*) FROM c;"
run_case "stock.transaction_commit" \
  "CREATE TABLE tx1 (id INT); BEGIN; INSERT INTO tx1 VALUES (1); COMMIT; SELECT count(*) FROM tx1;"
run_case "stock.transaction_rollback" \
  "CREATE TABLE tx2 (id INT); BEGIN; INSERT INTO tx2 VALUES (1); ROLLBACK; SELECT count(*) FROM tx2;"

# --- mailrs embed round-12 shapes (active since v7.21.0) ---
# Wire-level mirrors of crates/spg-sqlx/tests/mailrs_round12.rs; see
# .claude/notes/mailrs-embed-round12-gaps-and-fixes.md.
run_case "round12.upsert_via_unique_index" \
  "CREATE TABLE r12_a (id SERIAL PRIMARY KEY, email TEXT NOT NULL, reason TEXT NOT NULL DEFAULT ''); CREATE UNIQUE INDEX r12_a_email ON r12_a (email); INSERT INTO r12_a (email, reason) VALUES ('a@x', 'first') ON CONFLICT (email) DO UPDATE SET reason = 'second'; INSERT INTO r12_a (email, reason) VALUES ('a@x', 'third') ON CONFLICT (email) DO UPDATE SET reason = 'second'; SELECT reason FROM r12_a;"
run_case "round12.bitwise_flag_math" \
  "CREATE TABLE r12_b (id INT, flags INTEGER NOT NULL DEFAULT 0); INSERT INTO r12_b VALUES (1, 5); UPDATE r12_b SET flags = flags | 2 WHERE id = 1; UPDATE r12_b SET flags = flags & ~4 WHERE id = 1; SELECT flags FROM r12_b WHERE (flags & 1) != 0;"
run_case "round12.extract_epoch" \
  "CREATE TABLE r12_c (id INT, created_at TIMESTAMPTZ); INSERT INTO r12_c VALUES (1, '2026-01-01 00:00:00+00'); SELECT EXTRACT(EPOCH FROM created_at)::BIGINT FROM r12_c;"
run_case "round12.update_where_in_subquery" \
  "CREATE TABLE r12_d (id INT, state TEXT); INSERT INTO r12_d VALUES (1,'queued'),(2,'queued'),(3,'done'); UPDATE r12_d SET state = 'claimed' WHERE id IN (SELECT id FROM r12_d WHERE state = 'queued') RETURNING id; SELECT count(*) FROM r12_d WHERE state = 'claimed';"

# --- Fixture mode — apply each --fixture SQL file as a single chunk ---
FIXTURE_REPORT=""
if [ "${#FIXTURES[@]}" -gt 0 ]; then
  echo ""
  echo "=== Fixture panel ==="
  for f in "${FIXTURES[@]}"; do
    fname=$(basename "$f")
    if [ ! -f "$f" ]; then
      echo "[fixture] $fname — MISSING file at $f"
      FAIL_COUNT=$((FAIL_COUNT+1))
      CASES+=("fixture.$fname|FAIL|fixture file not found: $f")
      continue
    fi
    echo "[fixture] $fname"
    out=$(cat "$f" | $PSQL 2>&1)
    rc=$?
    if [ "$rc" -eq 0 ]; then
      PASS_COUNT=$((PASS_COUNT+1))
      CASES+=("fixture.$fname|PASS|")
    else
      FAIL_COUNT=$((FAIL_COUNT+1))
      first_err=$(echo "$out" | grep -E "^(ERROR|psql: error)" | head -1 | sed 's/^ERROR:\s*//' | tr -s ' ')
      CASES+=("fixture.$fname|FAIL|$first_err")
    fi
  done
fi

# --- Render markdown report ---
TOTAL=$((PASS_COUNT + FAIL_COUNT))
{
  echo "# SPG drop-in acceptance report"
  echo ""
  echo "- image: \`$IMAGE\`"
  echo "- panel cases: $TOTAL  (pass $PASS_COUNT  / fail $FAIL_COUNT)"
  echo ""
  if [ "$FAIL_COUNT" -eq 0 ]; then
    echo "**Verdict: PASS — every probed PG dialect feature lands on this SPG image.**"
  else
    echo "**Verdict: FAIL — $FAIL_COUNT case(s) below show real SPG dialect gaps. See the table.**"
  fi
  echo ""
  echo "## Cases"
  echo ""
  echo "| Case | Status | First error (if FAIL) |"
  echo "|---|:-:|---|"
  for entry in "${CASES[@]}"; do
    name=$(printf '%s\n' "$entry" | awk -F'|' '{print $1}')
    status=$(printf '%s\n' "$entry" | awk -F'|' '{print $2}')
    err=$(printf '%s\n' "$entry" | awk -F'|' '{print $3}')
    if [ "$status" = "PASS" ]; then
      echo "| \`$name\` | ✅ | |"
    else
      echo "| \`$name\` | ❌ | $err |"
    fi
  done
  echo ""
  echo "## Reproducer"
  echo ""
  echo "\`\`\`bash"
  echo "scripts/dropin-acceptance.sh --image $IMAGE --port $PORT"
  echo "\`\`\`"
} > "$REPORT"

echo ""
echo "=== Summary ==="
echo "Total: $TOTAL"
echo "Pass:  $PASS_COUNT"
echo "Fail:  $FAIL_COUNT"
echo ""
echo "Report: $REPORT"

if [ "$FAIL_COUNT" -gt 0 ]; then
  exit 1
fi
exit 0
