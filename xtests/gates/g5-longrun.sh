#!/usr/bin/env bash
#
# Gate 5 — big data, big rows, long transactions, and churn.
#
#   xtests/gates/g5-longrun.sh <image> [rows] [port]
#
# Everything else in gate 5 runs against a database of a few thousand
# rows for a few seconds. A production database is neither. Five things
# are asked of both legs, from the same recipe:
#
#   big row    a 16 MB text value and a 1 MB bytea come back byte for
#              byte — the sizes where an engine starts splitting values
#   many rows  after a bulk load, count / sum / min / max agree
#   long tx    a transaction held open across another connection's
#              commits still sees its own snapshot and still commits
#   churn      the same rows updated over and over end up with the right
#              answer, and the data directory does not grow without
#              bound — measured as growth per row rewritten, against
#              what PostgreSQL's grew
#   selective  a predicate that matches NOTHING still costs a fraction
#              of a full scan, so the index is doing the work after the
#              table got big
#
# The data directory is measured from OUTSIDE the container: the SPG
# image is distroless, so there is no `du` to exec.
#
# `SELFTEST=1` adds the negative control: the byte-comparison is asked
# about a value that was changed on the way back, and has to report it.
GATES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
# shellcheck source=xtests/gates/lib.sh
. "$GATES_DIR/lib.sh"

IMAGE=${1:?usage: g5-longrun.sh <image> [rows] [port]}
ROWS=${2:-200000}
PORT=${3:-17621}
PGPORT=$((PORT + 1))
CHURN_ROWS=2000
CHURN_PASSES=10
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"; docker rm -f g5long g5longpg >/dev/null 2>&1; docker volume rm g5longv g5longpgv >/dev/null 2>&1' EXIT

rc=0
judge() { # <name> <image answer> <pg answer>
  case "$3" in ok|ok\ *) ;; *)
    printf '  %-10s %-32s %-32s  (PostgreSQL does not either)\n' "$1" "$2" "$3"
    return ;;
  esac
  case "$2" in
    ok|ok\ *) printf '  %-10s %-32s %-32s\n' "$1" "$2" "$3" ;;
    *) printf '  %-10s %-32s %-32s  ✗\n' "$1" "$2" "$3"; rc=1 ;;
  esac
}

# Kilobytes under a named volume, read by a container that HAS a shell.
vol_kb() { docker run --rm -v "$1":/d alpine du -sk /d 2>/dev/null | awk '{print $1}'; }

# A deterministic blob of `n` bytes, generated on the HOST so the two
# legs are handed the same literal rather than each building its own.
blob() { head -c "$1" /dev/urandom | base64 | head -c "$1"; }

big_row() { # <port>
  local port=$1 got_md5 got_len want_md5 want_len bgot bwant
  want_md5=$(hashsum < "$WORK/b16"); want_len=$(wc -c < "$WORK/b16" | tr -d ' ')
  bwant=$(hashsum < "$WORK/b1")
  q "$port" -q -v ON_ERROR_STOP=1 </dev/null >/dev/null 2>&1 <<SQL
DROP TABLE IF EXISTS big;
CREATE TABLE big (id int PRIMARY KEY, t text, b bytea);
SQL
  # The literals go in through a FILE, not argv: a 16 MB argument is
  # not portable and `docker run` would refuse it.
  {
    printf "INSERT INTO big VALUES (1, '"
    cat "$WORK/b16"
    printf "', decode('"
    xxd -p < "$WORK/b1" | tr -d '\n'
    printf "', 'hex'));\n"
  } > "$WORK/bigins.sql"
  q "$port" -q -v ON_ERROR_STOP=1 -f - < "$WORK/bigins.sql" >/dev/null 2>"$WORK/big.err" \
    || { echo "refused: $(head -1 "$WORK/big.err" | cut -c1-30)"; return; }
  got_md5=$(q "$port" -tAc "SELECT md5(t) FROM big WHERE id = 1" </dev/null 2>/dev/null | tr -d '[:space:]')
  got_len=$(q "$port" -tAc "SELECT length(t) FROM big WHERE id = 1" </dev/null 2>/dev/null | tr -d '[:space:]')
  bgot=$(q "$port" -tAc "SELECT md5(b) FROM big WHERE id = 1" </dev/null 2>/dev/null | tr -d '[:space:]')
  [ "$got_len" = "$want_len" ] || { echo "text is $got_len bytes, put in $want_len"; return; }
  [ "$got_md5" = "$want_md5" ] || { echo "the text came back different"; return; }
  [ "$bgot" = "$bwant" ] || { echo "the bytea came back different"; return; }
  echo ok
}

many_rows() { # <port>
  local port=$1 a
  q "$port" -q -v ON_ERROR_STOP=1 </dev/null >/dev/null 2>"$WORK/many.err" <<SQL
DROP TABLE IF EXISTS many;
CREATE TABLE many (id int PRIMARY KEY, v bigint NOT NULL, s text NOT NULL);
INSERT INTO many SELECT g, (g * 2654435761) % 1000003, 'row' || g FROM generate_series(1, $ROWS) g;
CREATE INDEX many_v ON many (v);
SQL
  [ -s "$WORK/many.err" ] && { echo "load refused: $(head -1 "$WORK/many.err" | cut -c1-28)"; return; }
  a=$(q "$port" -tAc "SELECT count(*) || '/' || sum(v) || '/' || min(v) || '/' || max(v) FROM many" </dev/null 2>/dev/null | tr -d '[:space:]')
  echo "${a:-no answer}"
}

long_tx() { # <port>
  local port=$1 c1 c2 c3
  # Two clients, both driven from the HOST. The first version ran the
  # second one through psql's `\!` escape — which executes inside the
  # oracle container, where there is no docker — so the insert never
  # happened and the row read "the rows never showed" on both legs,
  # taking the question out of the panel entirely.
  {
    echo "BEGIN ISOLATION LEVEL REPEATABLE READ;"
    echo "SELECT count(*) FROM many;"
    echo "SELECT pg_sleep(8);"
    echo "SELECT count(*) FROM many;"
    echo "COMMIT;"
    echo "SELECT count(*) FROM many;"
  } | q "$port" -tA -q > "$WORK/ltx.$port" 2>&1 &
  local holder=$!
  sleep 3
  q "$port" -q -c "INSERT INTO many SELECT g, 0, 'late' FROM generate_series($((ROWS + 1)), $((ROWS + 500))) g" \
    </dev/null > /dev/null 2>&1
  wait "$holder" 2>/dev/null
  # Only the numeric lines: `pg_sleep` prints an empty one.
  c1=$(grep -E '^[0-9]+$' "$WORK/ltx.$port" | sed -n '1p')
  c2=$(grep -E '^[0-9]+$' "$WORK/ltx.$port" | sed -n '2p')
  c3=$(grep -E '^[0-9]+$' "$WORK/ltx.$port" | sed -n '3p')
  [ -n "$c1" ] && [ -n "$c3" ] || { echo "the transaction gave no counts"; return; }
  [ "$c1" = "$c2" ] || { echo "the snapshot moved: $c1 then $c2"; return; }
  [ "$c3" -gt "$c1" ] || { echo "the other connection's rows never showed: $c3"; return; }
  echo ok
}

churn() { # <port> <volume>
  local port=$1 vol=$2 before after grew sum p
  # CHECKPOINT on both sides of the window: a delta of 0 would otherwise
  # be indistinguishable between "this format does not bloat" and "it
  # has not written yet", and 0 is exactly the value a measurement
  # returns when it is looking at the wrong thing.
  q "$port" -q -c CHECKPOINT </dev/null >/dev/null 2>&1
  before=$(vol_kb "$vol")
  q "$port" -q -v ON_ERROR_STOP=1 </dev/null >/dev/null 2>&1 <<SQL
DROP TABLE IF EXISTS ch;
CREATE TABLE ch (id int PRIMARY KEY, n bigint NOT NULL DEFAULT 0);
INSERT INTO ch SELECT g, 0 FROM generate_series(1, $CHURN_ROWS) g;
SQL
  for ((p = 1; p <= CHURN_PASSES; p++)); do
    q "$port" -q -c "UPDATE ch SET n = n + 1" </dev/null >/dev/null 2>&1
  done
  sum=$(q "$port" -tAc "SELECT sum(n) FROM ch" </dev/null 2>/dev/null | tr -d '[:space:]')
  [ "$sum" = "$((CHURN_ROWS * CHURN_PASSES))" ] || { echo "the sum is $sum, wanted $((CHURN_ROWS * CHURN_PASSES))"; return; }
  q "$port" -q -c CHECKPOINT </dev/null >/dev/null 2>&1
  after=$(vol_kb "$vol")
  grew=$((after - before))
  # The absolute sizes ride along: a delta on its own cannot say whether
  # the directory was 3 MB or 300.
  echo "ok grew=${grew}KB of=${before}->${after}KB"
}

selective() { # <port>
  # Does the index still save reads once the table is big?
  #
  # The same zero-row predicate, timed WITH the index and then WITHOUT
  # it. Nothing else changes, so the difference is the index and only
  # the index — and putting it back is a reversible control: if the two
  # times match, the index was buying nothing.
  #
  # An earlier version compared a miss against a full scan instead, and
  # its control was wrong: with no index a miss is ALREADY cheaper than
  # a scan matching every row, because matched rows cost extra per row.
  # That measured the aggregate, not the index.
  local port=$1 with without
  with=$(miss_ms "$port")
  q "$port" -q -c "DROP INDEX many_v" </dev/null >/dev/null 2>&1
  without=$(miss_ms "$port")
  q "$port" -q -c "CREATE INDEX many_v ON many (v)" </dev/null >/dev/null 2>&1
  [ -n "$with" ] && [ -n "$without" ] || { echo "no timings came back"; return; }
  python3 -c "
import sys
w, n = float('$with'), float('$without')
if n < 1.0:
    print(f'the scan without an index took only {n} ms — too small to judge')
elif w * 2 <= n:
    print('ok')
else:
    print(f'the index saves nothing: {w} ms with it, {n} ms without')
"
}

# Milliseconds for the zero-row predicate, best of three, measured by
# psql's own timing inside ONE session — a fresh `docker run` per query
# would be timing the container, which is what the first version did.
miss_ms() { # <port>
  {
    echo '\timing on'
    echo "SELECT count(*) FROM many WHERE v = -1;"
    echo "SELECT count(*) FROM many WHERE v = -1;"
    echo "SELECT count(*) FROM many WHERE v = -1;"
  } | q "$1" -tA -q 2>/dev/null \
    | sed -n 's/^Time: \([0-9.]*\) ms.*/\1/p' | sort -n | head -1
}

echo "g5-longrun: $IMAGE vs $ORACLE — $ROWS rows, ${CHURN_ROWS}×${CHURN_PASSES} rewrites"
blob 16000000 > "$WORK/b16"
blob 1000000  > "$WORK/b1"
docker volume rm g5longv g5longpgv >/dev/null 2>&1
docker volume create g5longv   >/dev/null
docker volume create g5longpgv >/dev/null
boot "$IMAGE"  "$PORT"   g5long   -v "g5longv:$(datadir "$IMAGE")"    || exit 2
boot "$ORACLE" "$PGPORT" g5longpg -v "g5longpgv:$(datadir "$ORACLE")" || exit 2

printf '  %-10s %-32s %-32s\n' question "$IMAGE" postgres
judge "big row"  "$(big_row "$PORT")"  "$(big_row "$PGPORT")"

a=$(many_rows "$PORT"); b=$(many_rows "$PGPORT")
if [ "$a" = "$b" ]; then judge "many rows" ok ok
else judge "many rows" "$a" "$b"; fi

judge "long tx"  "$(long_tx "$PORT")"  "$(long_tx "$PGPORT")"

a=$(churn "$PORT" g5longv); b=$(churn "$PGPORT" g5longpgv)
ag=${a##*grew=}; ag=${ag%% *}; bg=${b##*grew=}; bg=${bg%% *}
asz=${a##*of=}; bsz=${b##*of=}
if [ "${a%% *}" = ok ] && [ "${b%% *}" = ok ]; then
  # Growth is compared as a RATIO, because the two formats are not the
  # same shape. What this catches is growth without bound, not a
  # difference in page layout.
  ak=${ag%KB}; bk=${bg%KB}
  [ "${bk:-1}" -lt 1 ] && bk=1
  r=$((ak * 10 / bk))
  # The numbers go in the row whatever the verdict: "ok" alone says
  # nothing about how much either engine grew.
  if [ "$r" -le 80 ]; then judge churn "ok +${ak}KB ($asz)" "ok +${bk}KB ($bsz)"
  else judge churn "grew ${ak}KB where PostgreSQL grew ${bk}KB" ok; fi
else
  judge churn "${a}" "$([ "${b%% *}" = ok ] && echo ok || echo "$b")"
fi

judge selective "$(selective "$PORT")" "$(selective "$PGPORT")"

if [ "${SELFTEST:-0}" = 1 ]; then
  q "$PORT" -q -c "UPDATE big SET t = t || 'x' WHERE id = 1" </dev/null >/dev/null 2>&1
  got=$(q "$PORT" -tAc "SELECT md5(t) FROM big WHERE id = 1" </dev/null 2>/dev/null | tr -d '[:space:]')
  want=$(hashsum < "$WORK/b16")
  [ "$got" != "$want" ] && echo "  selftest: the byte comparison reports a changed value: ok" \
                        || { echo "  selftest: ✗ it called a changed value identical"; rc=1; }
fi

[ "$rc" = 0 ] && echo "g5-longrun: $IMAGE PASS" || echo "g5-longrun: $IMAGE FAIL"
exit "$rc"
