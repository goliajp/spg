#!/usr/bin/env bash
#
# Gate 5 — schema changes on a table that already holds rows.
#
#   xtests/gates/g5-migrate.sh <image> [port]
#
# Migrations are written against an empty database and run against a
# full one. That is where an engine's DDL either rewrites the rows
# correctly, refuses the way PostgreSQL refuses, or quietly loses
# something — and an application only finds out months later.
#
# Every operation runs on BOTH legs from the same recipe: a fresh table
# of 2,000 rows, one DDL step, then the same three questions asked of
# the result —
#
#   outcome   it succeeded, or it failed with PostgreSQL's SQLSTATE
#   rows      how many rows are there, and do the ids still add up
#   columns   the column list, in order, with the types
#
# An operation PostgreSQL refuses has to be refused here too, with the
# same code: a DDL step that succeeds where PostgreSQL rejects it is a
# constraint the application believes it has and does not.
#
# `SELFTEST=1` adds the negative control: one leg's table is changed
# behind the comparison's back, and the comparison has to report it.
GATES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
# shellcheck source=xtests/gates/lib.sh
. "$GATES_DIR/lib.sh"

IMAGE=${1:?usage: g5-migrate.sh <image> [port]}
PORT=${2:-17591}
PGPORT=$((PORT + 1))
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"; docker rm -f g5mig g5migpg >/dev/null 2>&1' EXIT

ROWS=2000

# The table every operation starts from. Deterministic, so the two legs
# hold byte-identical rows before the DDL runs.
recipe() {
  cat <<SQL
DROP TABLE IF EXISTS m CASCADE;
DROP TABLE IF EXISTS mp CASCADE;
CREATE TABLE m (id int PRIMARY KEY, v int NOT NULL, s text, t timestamptz);
INSERT INTO m
SELECT g, g * 2, 'x' || g,
       timestamptz '2020-01-01 00:00:00+00' + (g || ' seconds')::interval
FROM generate_series(1, $ROWS) g;
SQL
}

# name|SQL — one migration step, run as one script with ON_ERROR_STOP.
STEPS=$(cat <<'LIST'
add column with a default|ALTER TABLE m ADD COLUMN c int NOT NULL DEFAULT 7;
add a nullable column|ALTER TABLE m ADD COLUMN c text;
add a column defaulted by a function|ALTER TABLE m ADD COLUMN u uuid NOT NULL DEFAULT gen_random_uuid();
drop a column|ALTER TABLE m DROP COLUMN s;
rename a column|ALTER TABLE m RENAME COLUMN v TO w;
widen the type|ALTER TABLE m ALTER COLUMN v TYPE bigint;
narrow a type that does not fit|ALTER TABLE m ALTER COLUMN v TYPE smallint;
retype with USING|ALTER TABLE m ALTER COLUMN s TYPE int USING replace(s, 'x', '')::int;
set NOT NULL where it holds|ALTER TABLE m ALTER COLUMN s SET NOT NULL;
set NOT NULL with a null present|UPDATE m SET s = NULL WHERE id = 1; ALTER TABLE m ALTER COLUMN s SET NOT NULL;
drop NOT NULL|ALTER TABLE m ALTER COLUMN v DROP NOT NULL;
set then drop a default|ALTER TABLE m ALTER COLUMN v SET DEFAULT 99; ALTER TABLE m ALTER COLUMN v DROP DEFAULT;
add a UNIQUE that holds|ALTER TABLE m ADD CONSTRAINT mu UNIQUE (v);
add a UNIQUE that is violated|UPDATE m SET v = 2 WHERE id = 3; ALTER TABLE m ADD CONSTRAINT mu UNIQUE (v);
add a CHECK that holds|ALTER TABLE m ADD CONSTRAINT mc CHECK (v >= 0);
add a CHECK that is violated|ALTER TABLE m ADD CONSTRAINT mc CHECK (v > 100000);
drop and re-add the primary key|ALTER TABLE m DROP CONSTRAINT m_pkey; ALTER TABLE m ADD PRIMARY KEY (id);
add a FOREIGN KEY that holds|CREATE TABLE mp (id int PRIMARY KEY); INSERT INTO mp SELECT DISTINCT v FROM m; ALTER TABLE m ADD CONSTRAINT mf FOREIGN KEY (v) REFERENCES mp (id);
add a FOREIGN KEY that is violated|CREATE TABLE mp (id int PRIMARY KEY); INSERT INTO mp VALUES (2); ALTER TABLE m ADD CONSTRAINT mf FOREIGN KEY (v) REFERENCES mp (id);
index a populated table|CREATE INDEX mi ON m (v);
unique index on values that repeat|UPDATE m SET v = 2 WHERE id = 3; CREATE UNIQUE INDEX mi ON m (v);
index an expression|CREATE INDEX mie ON m (lower(s));
rename the table and back|ALTER TABLE m RENAME TO m2; ALTER TABLE m2 RENAME TO m;
add a generated column|ALTER TABLE m ADD COLUMN g2 int GENERATED ALWAYS AS (v * 3) STORED;
add an identity column|ALTER TABLE m ADD COLUMN g3 int GENERATED ALWAYS AS IDENTITY;
add NOT NULL column with no default|ALTER TABLE m ADD COLUMN c int NOT NULL;
primary key over a column that repeats|UPDATE m SET v = 2 WHERE id = 3; ALTER TABLE m DROP CONSTRAINT m_pkey; ALTER TABLE m ADD PRIMARY KEY (v);
primary key over a column holding a null|UPDATE m SET s = NULL WHERE id = 5; ALTER TABLE m DROP CONSTRAINT m_pkey; ALTER TABLE m ADD PRIMARY KEY (s);
primary key over a column that is clean|ALTER TABLE m DROP CONSTRAINT m_pkey; ALTER TABLE m ADD PRIMARY KEY (v);
drop a column an index is on|CREATE INDEX mi ON m (v); ALTER TABLE m DROP COLUMN v;
retype a column an index is on|CREATE INDEX mi ON m (v); ALTER TABLE m ALTER COLUMN v TYPE bigint;
retype where one value will not convert|UPDATE m SET s = 'not a number' WHERE id = 7; ALTER TABLE m ALTER COLUMN s TYPE int USING s::int;
drop a table another references|CREATE TABLE mp (id int PRIMARY KEY); INSERT INTO mp SELECT DISTINCT v FROM m; ALTER TABLE m ADD CONSTRAINT mf FOREIGN KEY (v) REFERENCES mp (id); DROP TABLE mp;
truncate a table another references|CREATE TABLE mp (id int PRIMARY KEY); INSERT INTO mp SELECT DISTINCT v FROM m; ALTER TABLE m ADD CONSTRAINT mf FOREIGN KEY (v) REFERENCES mp (id); TRUNCATE mp;
drop a column a view selects|CREATE VIEW mv AS SELECT id, v FROM m; ALTER TABLE m DROP COLUMN v;
retype a column a view selects|CREATE VIEW mv AS SELECT id, v FROM m; ALTER TABLE m ALTER COLUMN v TYPE bigint;
LIST
)

# Run one step and answer with `outcome|rows|idsum|columns`.
step_answer() { # <port> <sql>
  local port=$1 sql=$2 out code
  q "$port" -v ON_ERROR_STOP=1 -q >/dev/null 2>"$WORK/mig.err" <<SQL
$(recipe)
SQL
  out=$(printf '%s\n' "$sql" | q "$port" -v ON_ERROR_STOP=1 -v VERBOSITY=verbose -q 2>&1)
  if echo "$out" | grep -qiE '^(psql:)?.*(ERROR|FATAL)'; then
    # PostgreSQL prints `ERROR:  <sqlstate>: <text>` under VERBOSITY=verbose.
    # `grep -oE`, not `sed`: BSD sed has no `\|` alternation, and the
    # first version of this line therefore extracted nothing at all —
    # every refusal read `refused ?`, which made a syntax error and a
    # constraint violation compare equal.
    code=$(echo "$out" | grep -oE '(ERROR|FATAL):  ?[0-9A-Z]{5}' | head -1 | awk '{print $2}')
    echo "refused ${code:-?}|-|-|-"
    return
  fi
  local rows idsum cols
  rows=$(q "$port" -tAc "SELECT count(*) FROM m" </dev/null 2>/dev/null | tr -d '[:space:]')
  idsum=$(q "$port" -tAc "SELECT coalesce(sum(id), 0) FROM m" </dev/null 2>/dev/null | tr -d '[:space:]')
  cols=$(q "$port" -tAc "SELECT string_agg(column_name || ' ' || data_type, ', ' ORDER BY ordinal_position)
          FROM information_schema.columns WHERE table_schema = 'public' AND table_name = 'm'" </dev/null 2>/dev/null)
  echo "ok|${rows:-?}|${idsum:-?}|${cols:-?}"
}

echo "g5-migrate: $IMAGE vs $ORACLE — $(echo "$STEPS" | wc -l | tr -d ' ') steps on $ROWS rows"
boot "$IMAGE"  "$PORT"   g5mig   >/dev/null 2>&1 || { echo "✗ $IMAGE will not start"; exit 2; }
boot "$ORACLE" "$PGPORT" g5migpg >/dev/null 2>&1 || { echo "✗ $ORACLE will not start"; exit 2; }

rc=0; same=0; diff=0
while IFS='|' read -r name sql; do
  [ -n "$name" ] || continue
  a=$(step_answer "$PORT" "$sql")
  b=$(step_answer "$PGPORT" "$sql")
  if [ "$a" = "$b" ]; then
    same=$((same + 1))
    printf '  %-42s %s\n' "$name" "${a%%|*}"
  else
    diff=$((diff + 1)); rc=1
    printf '  %-42s ✗\n' "$name"
    printf '      %-9s %s\n' "$IMAGE" "$a"
    printf '      %-9s %s\n' postgres "$b"
  fi
done <<< "$STEPS"

if [ "${SELFTEST:-0}" = 1 ]; then
  q "$PORT" -q -c "DROP TABLE IF EXISTS m CASCADE" >/dev/null 2>&1
  q "$PORT" -q -c "CREATE TABLE m (id int)" -c "INSERT INTO m VALUES (1)" >/dev/null 2>&1
  a=$(step_answer "$PORT" "ALTER TABLE m ADD COLUMN zz int;")
  # The recipe rebuilds the table, so the answer must be the recipe's
  # — if a leg could keep a table the comparison would never see the
  # difference between the two legs at all.
  q "$PGPORT" -q -c "DROP TABLE IF EXISTS m CASCADE" >/dev/null 2>&1
  b=$(step_answer "$PGPORT" "ALTER TABLE m ADD COLUMN zz int; DELETE FROM m WHERE id > 1000;")
  [ "$a" != "$b" ] && echo "  selftest: the comparison reports a leg that lost rows: ok" \
                   || { echo "  selftest: ✗ it called two different tables equal"; rc=1; }
fi

echo "g5-migrate: $same agree, $diff differ"
[ "$rc" = 0 ] && echo "g5-migrate: $IMAGE PASS" || echo "g5-migrate: $IMAGE FAIL"
exit "$rc"
