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
#   * --dialect mariadb panel. The MySQL one LANDED in v7.38.17 (see
#     "MySQL dialect panel" below); MariaDB needs its own expectations,
#     because the two engines' default collations disagree about
#     trailing spaces and this file must not assume one answer covers
#     both.
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

# v7.38.17 — the MySQL wire is served too, so the panels below can ask
# a MySQL client the same questions a psql client gets asked.
#
# The header of this file has listed `--dialect mysql|mariadb panels` as
# a Phase 2 idea since v7.18. It was a line in a wish list, and reading
# it as a capability is how the instrument plan for this version
# mis-sized the work: nothing implemented it, and `grep -- --dialect`
# matched only the comment.
MYPORT="$((PORT + 1000))"
echo "[setup] starting $CONTAINER_NAME on host port $PORT (mysql wire $MYPORT)"
docker run --rm -d --name "$CONTAINER_NAME" \
  -e SPG_MYSQLWIRE_ADDR="0.0.0.0:3307" \
  -p "$PORT:5432" -p "$MYPORT:3307" "$IMAGE" >/dev/null || {
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
# v7.27 — value-asserting variant: the statement's LAST output line
# (psql -t -A) must equal the expectation. rc=0 alone lets silently
# wrong results pass; the rounds 12-20 lesson is that seeded cases
# with value checks catch what empty-table smoke cases miss.
run_case_expect() {
  local name="$1"
  local sql="$2"
  local want="$3"
  # v7.37.16 — assert on STDOUT, never on the two streams merged.
  #
  # `2>&1` used to fold psql's diagnostics into the rows, and a psql error
  # is two lines: `ERROR: …` and `DETAIL: …`. Whether the DETAIL lands
  # before or after a row is a matter of buffering — psql block-buffers
  # stdout when it is not a tty while stderr stays unbuffered — so a case
  # whose last row is the assertion could read back `DETAIL: Key (id)=(1)
  # already exists.` instead. `round13.inline_pk_enforces` did exactly
  # that on the 7.37.15 panel, having passed on 7.37.9 and 7.37.14 with
  # no relevant change between them; five direct runs against the same
  # published image returned the expected row every time.
  #
  # Filtering more prefixes (DETAIL, HINT, CONTEXT, LINE, the caret line)
  # would be chasing the symptom. The rows come from stdout, so the
  # assertion reads stdout and the diagnostics stay where they can still
  # be reported.
  local out rc got err
  err=$(mktemp)
  out=$(echo "$sql" | $PSQL -t -A 2>"$err")
  rc=$?
  got=$(echo "$out" | grep -v '^$' | tail -1 | tr -s ' ')
  if [ "$rc" -eq 0 ] && [ "$got" = "$want" ]; then
    PASS_COUNT=$((PASS_COUNT+1))
    CASES+=("$name|PASS|")
  else
    FAIL_COUNT=$((FAIL_COUNT+1))
    local detail
    if [ "$rc" -ne 0 ]; then
      detail=$(grep -E "^(ERROR|psql: error)" "$err" | head -1 | tr -s ' ')
      [ -n "$detail" ] || detail=$(head -1 "$err" | tr -s ' ')
    else
      detail="expected [$want] got [$got]"
    fi
    CASES+=("$name|FAIL|$detail")
  fi
  rm -f "$err"
}

# Tolerant variant: statement errors do NOT stop the chunk (for
# cases whose assertion is "the bad statement is rejected and the
# final state proves it").
run_case_expect_tolerant() {
  local name="$1"
  local sql="$2"
  local want="$3"
  # An error is EXPECTED here (that is the assertion), so the diagnostics
  # are discarded rather than filtered — see run_case_expect above for why
  # filtering them out of a merged stream cannot be made reliable.
  local out got
  out=$(echo "$sql" | ${PSQL/ON_ERROR_STOP=on/ON_ERROR_STOP=off} -t -A 2>/dev/null)
  got=$(echo "$out" | grep -v '^$' | tail -1 | tr -s ' ')
  if [ "$got" = "$want" ]; then
    PASS_COUNT=$((PASS_COUNT+1))
    CASES+=("$name|PASS|")
  else
    FAIL_COUNT=$((FAIL_COUNT+1))
    CASES+=("$name|FAIL|expected [$want] got [$got]")
  fi
}

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

# --- rounds 13-20 shapes (universalised 2026-06-11) ---
# Wire-mode (pgwire) mirrors of the embed e2e suites
# crates/spg-embedded/tests/e2e/mailrs_round{13,14,15_16,17}.rs and
# crates/spg-sqlx/tests/mailrs_round20.rs. Every case is SEEDED and
# value-asserted: empty-table rc-only cases declared victory twice
# during rounds 17-19. The typed-decode axis lives in the sqlx gate
# (server-side RowDescription shares the same describe path).
echo "[panel] rounds 13-20 (seeded, value-asserted)"
run_case_expect "round13.serial_continuity_multirow" \
  "CREATE TABLE r13_a (id BIGSERIAL PRIMARY KEY, v BIGINT); INSERT INTO r13_a (v) VALUES (10), (20); INSERT INTO r13_a (v) VALUES (30); SELECT max(id) FROM r13_a;" \
  "3"
# The duplicate INSERT is EXPECTED to error (that's the assertion);
# run without ON_ERROR_STOP so the trailing count still executes.
run_case_expect_tolerant "round13.inline_pk_enforces" \
  "CREATE TABLE r13_b (id BIGINT PRIMARY KEY, v TEXT); INSERT INTO r13_b VALUES (1,'a'); INSERT INTO r13_b VALUES (1,'dup'); SELECT count(*) FROM r13_b;" \
  "1"
run_case_expect "round21.bytea_above_64k" \
  "CREATE TABLE r21_a (id BIGINT, data BYTEA); INSERT INTO r21_a VALUES (1, decode(repeat('QUFB', 30000), 'base64')); SELECT length(data) FROM r21_a;" \
  "90000"
run_case_expect "round21.text_array_elem_above_64k" \
  "CREATE TABLE r21_b (id BIGINT, uris TEXT[]); INSERT INTO r21_b VALUES (1, ARRAY[repeat('u', 80000), 'small']); SELECT length(uris[1]) FROM r21_b;" \
  "80000"
run_case_expect "round14.text_above_64k" \
  "CREATE TABLE r14_a (id BIGINT, body TEXT); INSERT INTO r14_a VALUES (1, repeat('x', 70000)); SELECT length(body) FROM r14_a;" \
  "70000"
run_case_expect "round15.string_to_array" \
  "SELECT (string_to_array('a,b,c', ','))[2];" \
  "b"
run_case_expect "round16.order_by_nulls_first" \
  "CREATE TABLE r16_a (id BIGINT, ts BIGINT); INSERT INTO r16_a VALUES (1,200),(2,NULL),(3,100); SELECT id FROM r16_a ORDER BY ts ASC NULLS FIRST LIMIT 1;" \
  "2"
run_case_expect "round16.array_agg_internal_order" \
  "CREATE TABLE r16_b (lvl TEXT, score BIGINT); INSERT INTO r16_b VALUES ('high',90),('low',10),('mid',NULL),('top',95); SELECT (array_agg(lvl ORDER BY score DESC NULLS LAST))[1] FROM r16_b;" \
  "top"
run_case_expect "round16.setweight_trigger_fires" \
  "CREATE TABLE r16_c (s TEXT, sv tsvector); CREATE OR REPLACE FUNCTION r16_f() RETURNS trigger LANGUAGE plpgsql AS \$\$ BEGIN NEW.sv := setweight(to_tsvector('simple', COALESCE(NEW.s,'')), 'A'); RETURN NEW; END; \$\$; CREATE TRIGGER r16_tr BEFORE INSERT ON r16_c FOR EACH ROW EXECUTE FUNCTION r16_f(); INSERT INTO r16_c (s) VALUES ('hello world'); SELECT count(*) FROM r16_c WHERE sv @@ plainto_tsquery('simple','hello');" \
  "1"
run_case_expect "round16.correlated_not_exists_join" \
  "CREATE TABLE r16_m (id BIGSERIAL PRIMARY KEY, size BIGINT, fid BIGINT); CREATE TABLE r16_ac (message_id BIGINT); CREATE TABLE r16_fo (id BIGINT PRIMARY KEY, name TEXT); INSERT INTO r16_m (size, fid) VALUES (10,1),(0,1),(20,1); INSERT INTO r16_ac VALUES (1); INSERT INTO r16_fo VALUES (1,'inbox'); SELECT m.id FROM r16_m m JOIN r16_fo f ON f.id = m.fid WHERE m.size > 0 AND NOT EXISTS (SELECT 1 FROM r16_ac ac WHERE ac.message_id = m.id) ORDER BY m.id DESC;" \
  "3"
run_case_expect "round17.ilike" \
  "CREATE TABLE r17_a (s TEXT); INSERT INTO r17_a VALUES ('Hello World'),('goodbye'),(NULL); SELECT count(*) FROM r17_a WHERE s ILIKE '%hello%';" \
  "1"
run_case_expect "round17.distinct_agg_case_cast" \
  "CREATE TABLE r17_b (t TEXT, sender TEXT, unread BIGINT); INSERT INTO r17_b VALUES ('t1','alice',1),('t1','alice',0),('t1','bob',1),('t2','carol',0); SELECT COUNT(DISTINCT CASE WHEN unread = 1 THEN CAST(sender AS TEXT) END) FROM r17_b WHERE t = 't1';" \
  "2"
run_case_expect "round17.cte_chain" \
  "CREATE TABLE r17_c (id BIGINT, thread BIGINT, body TEXT); INSERT INTO r17_c VALUES (1,10,'invoice'),(2,10,'other'),(3,20,'x'); WITH matched AS (SELECT thread FROM r17_c WHERE body ILIKE '%invoice%'), cands AS (SELECT id FROM r17_c WHERE thread IN (SELECT thread FROM matched)) SELECT COUNT(*) FROM cands;" \
  "2"
run_case_expect "round19.correlated_scalar_in_group_by" \
  "CREATE TABLE r19_m (id BIGINT, thread_id TEXT, internal_date BIGINT); CREATE TABLE r19_e (message_id BIGINT, category TEXT); INSERT INTO r19_m VALUES (1,'th1',100),(2,'th1',200),(3,'th2',50); INSERT INTO r19_e VALUES (1,'old'),(2,'new'),(3,'t2c'); SELECT COALESCE((SELECT e2.category FROM r19_e e2 JOIN r19_m m2 ON e2.message_id = m2.id WHERE m2.thread_id = m.thread_id ORDER BY m2.internal_date DESC LIMIT 1), 'general') FROM r19_m m GROUP BY m.thread_id ORDER BY m.thread_id LIMIT 1;" \
  "new"
run_case_expect "round20.aggregate_group_composite" \
  "CREATE TABLE r20_m (id BIGSERIAL PRIMARY KEY, t TEXT, d BIGINT, pin BOOLEAN, score REAL); INSERT INTO r20_m (t,d,pin,score) VALUES ('th',100,true,0.5),('th',200,false,0.9); SELECT COUNT(DISTINCT id) || '/' || MAX(d) || '/' || COALESCE(BOOL_OR(pin), false) || '/' || COALESCE(MAX(score), 0.0) FROM r20_m GROUP BY t;" \
  "2/200/true/0.9"
# round-26: the meili backfill keyset shape (JOIN + ORDER BY id +
# LIMIT) on the wire path — bounded execution must keep order /
# cursor / INNER-drop semantics. Seeded with an orphan row (mailbox
# 9 absent) and a NULL body. Two single-row probes because the
# harness asserts the LAST output line only.
run_case_expect "round26.backfill_keyset_offset_skips_orphan" \
  "CREATE TABLE r26_m (id BIGINT, mailbox_id BIGINT, text_body TEXT); CREATE TABLE r26_mb (id BIGINT, user_address TEXT); INSERT INTO r26_mb VALUES (1,'a@x'),(2,'b@x'); INSERT INTO r26_m VALUES (1,1,'b1'),(2,2,NULL),(3,9,'orphan'),(4,2,'b4'),(5,1,'b5'); SELECT m.id || ':' || mb.user_address FROM r26_m m JOIN r26_mb mb ON m.mailbox_id = mb.id WHERE m.id > 1 ORDER BY m.id ASC LIMIT 1 OFFSET 1;" \
  "4:b@x"
run_case_expect "round26.backfill_desc_limit_top_end" \
  "SELECT m.id || ':' || mb.user_address FROM r26_m m JOIN r26_mb mb ON m.mailbox_id = mb.id ORDER BY m.id DESC LIMIT 1;" \
  "5:a@x"

# --- Fixture mode — apply each --fixture SQL file as a single chunk ---

echo "=== MySQL dialect panel ==="

# v7.38.17 — a drop-in claim covers three engines and this harness
# probed one. These cases are the ones v7.38.16 and v7.38.17 were
# spent on, asked over the wire a MySQL client actually speaks;
# every expectation is MySQL 9.7.2's own answer at its default
# collation, read from the oracle.
MYSQL_CLI="docker run --rm -i --network host spg-oracle-mysql:v7.38 mysql -h 127.0.0.1 -P $MYPORT -u spg -N"

my_case() {
  local name="$1" sql="$2" want="$3" got
  got="$($MYSQL_CLI -e "$sql" 2>/dev/null | tr '\t' ',' | paste -sd';' - | tr -d '\r')"
  if [ "$got" = "$want" ]; then
    echo "[mysql] ok   $name"
    PASS_COUNT=$((PASS_COUNT+1))
    CASES+=("mysql.$name|PASS|")
  else
    echo "[mysql] FAIL $name — want '$want', got '$got'"
    FAIL_COUNT=$((FAIL_COUNT+1))
    CASES+=("mysql.$name|FAIL|want '$want', got '$got'")
  fi
}

if $MYSQL_CLI -e "SELECT 1" >/dev/null 2>&1; then
  my_case "wire answers" "SELECT 1" "1"

  $MYSQL_CLI -e "CREATE TABLE dp (k INT, s TEXT)" >/dev/null 2>&1
  $MYSQL_CLI -e "INSERT INTO dp VALUES (1,'alpha'),(2,'alpha  '),(3,'Beta'),(4,'beta')" >/dev/null 2>&1

  # The collation folds case...
  my_case "case folds" "SELECT k FROM dp WHERE s = 'ALPHA'" "1"
  # ...and does NOT fold trailing spaces (NO PAD).
  my_case "trailing space is data" "SELECT count(DISTINCT s) FROM dp" "3"

  # An index must not change any of it.
  $MYSQL_CLI -e "CREATE INDEX dp_s ON dp (s)" >/dev/null 2>&1
  my_case "indexed equality" "SELECT k FROM dp WHERE s = 'ALPHA'" "1"
  my_case "indexed IN" "SELECT k FROM dp WHERE s IN ('ALPHA','BETA') ORDER BY k" "1;3;4"
  my_case "indexed ORDER BY" "SELECT k FROM dp ORDER BY s LIMIT 2" "1;2"

  # The join, which is where this class took its worst shape.
  $MYSQL_CLI -e "CREATE TABLE dq (k INT, s TEXT)" >/dev/null 2>&1
  $MYSQL_CLI -e "INSERT INTO dq VALUES (10,'ALPHA'),(20,'beta')" >/dev/null 2>&1
  $MYSQL_CLI -e "CREATE INDEX dq_s ON dq (s)" >/dev/null 2>&1
  my_case "indexed join" "SELECT dp.k, dq.k FROM dp JOIN dq ON dp.s = dq.s ORDER BY dp.k, dq.k" "1,10;3,20;4,20"

  $MYSQL_CLI -e "DROP TABLE dp" >/dev/null 2>&1
  $MYSQL_CLI -e "DROP TABLE dq" >/dev/null 2>&1
else
  echo "[mysql] FAIL wire did not answer on port $MYPORT"
  FAIL_COUNT=$((FAIL_COUNT+1))
  CASES+=("mysql.wire|FAIL|no answer on port $MYPORT")
fi


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
