#!/usr/bin/env bash
# MySQL-dialect differential corpus — SPG's mysql-wire vs live MySQL 9.7.
#
# Built to the same protocol as `xtests/diffcorpus/run.sh`, which is the
# PostgreSQL half: start our own server, prove BOTH legs answer before
# scoring anything, diff per file, and fail only on deviation from a
# recorded baseline.
#
# The reason it exists: SPG advertises a MySQL face and had ONE file of
# fifteen statements checking it, run against MariaDB. SPG reports itself
# as MySQL, and MariaDB is a different engine with different answers, so
# the only systematic corpus SPG had for its MySQL face was measuring the
# wrong oracle. See `RUNNER.md`.
set -uo pipefail
export PATH="$HOME/.orbstack/bin:/Applications/OrbStack.app/Contents/MacOS/xbin:$PATH"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
SPG_PORT="${SPG_MYSQL_PORT:-26010}"
ORACLE="${MYSQL_ORACLE_CONTAINER:-spg-bench-mysql}"

OWN_SERVER=""
if [ "${SPG_REUSE:-0}" = 1 ] && (exec 3<>/dev/tcp/127.0.0.1/"$SPG_PORT") 2>/dev/null; then
  :
else
  lsof -ti :"$SPG_PORT" 2>/dev/null | xargs kill -9 2>/dev/null || true
  (cd "$ROOT" && cargo build --release --bin spg-server -q) || exit 2
  SPG_DATA_DIR="$(mktemp -d)"
  export SPG_DATA_DIR
  SPG_MYSQLWIRE_ADDR=0.0.0.0:"$SPG_PORT" "$ROOT/target/release/spg-server" 127.0.0.1:0 \
      >/tmp/spg-mysqlcorpus.log 2>&1 &
  OWN_SERVER=$!
  for _ in $(seq 1 40); do
    (exec 3<>/dev/tcp/127.0.0.1/"$SPG_PORT") 2>/dev/null && break
    sleep 0.5
  done
fi
cleanup() {
  [ -n "$OWN_SERVER" ] || return 0
  kill "$OWN_SERVER" 2>/dev/null || true
  for _ in 1 2 3 4 5 6; do
    lsof -ti :"$SPG_PORT" >/dev/null 2>&1 || return 0
    sleep 0.5
  done
  lsof -ti :"$SPG_PORT" | xargs kill -9 2>/dev/null || true
}
trap cleanup EXIT

OUT="$HERE/out"; mkdir -p "$OUT"

# ONE client binary for both legs, so a rendering difference is the
# engine's and not the tool's. The oracle needs TLS (MySQL 9's
# caching_sha2_password refuses a plaintext password); SPG's wire has no
# password at all, so it runs with TLS disabled. That asymmetry is in the
# transport, not in any answer.
#
# `--force` is not optional. Without it the client STOPS at the first
# error, so one missing function early in a file deletes every answer
# after it and the diff reports truncation instead of findings — file 01
# scored 15 differing lines that way, of which exactly one was real. It
# is the MySQL client's `ON_ERROR_STOP=0`.
#
# `--default-character-set=utf8mb4` on BOTH legs. The container's client
# does not default to it against every server, and the first run of this
# corpus reported `CHAR_LENGTH('\u65e5\u672c')` as 6 on the oracle and 2 on SPG —
# a three-line "finding" in file 03 that was the CONNECTION charset, not
# either engine. Naming it on both legs removes the variable.
#
# The two streams are NOT merged. `mysql` writes rows to stdout and
# errors to stderr, and which lands first is a race between two file
# descriptors on one pipe: measured, the same run put a duplicate-key
# ERROR before its own marker row on one leg and after it on the other,
# scoring two differing lines that were the same two lines. The
# PostgreSQL corpus separated its streams in round 666 for exactly this,
# and the note there applies unchanged — ordering within a single stream
# is deterministic.
SPG() { docker exec -i "$ORACLE" mysql -h host.docker.internal -P "$SPG_PORT" -u root \
          --batch --raw --force --column-names=0 --default-character-set=utf8mb4 \
          --ssl-mode=DISABLED; }
MYS() { docker exec -i "$ORACLE" mysql -h 127.0.0.1 -u root -pbench -D mysqlcorpus \
          --batch --raw --force --column-names=0 --default-character-set=utf8mb4; }

# Drop the client's own chatter — it is not an engine answer, and one leg
# passes a password on the command line while the other does not.
# LC_ALL=C: some answers are not valid UTF-8 (HEX/BINARY round-trips),
# and a locale-aware sed refuses the whole stream on the first such byte
# — which silently truncates a leg instead of failing.
#
# The two legs are IN different databases — the oracle needs one to be
# named, SPG's wire serves a single built-in schema — and the name
# reaches the reader inside error messages. That is the harness's
# choice, not an engine answer.
norm() { LC_ALL=C grep -v '^mysql: \[Warning\] Using a password' \
       | LC_ALL=C sed -E "s/ at line [0-9]+: /: /; s/'(mysqlcorpus|spg)[.]/'<db>./g"; }

probe_leg() { # $1=name $2=fn
  local got
  got="$(printf 'SELECT 1;\n' | "$2" 2>/dev/null | norm | tr -d '[:space:]')"
  if [ "$got" != "1" ]; then
    echo "mysqlcorpus: the $1 leg answered '${got:-<nothing>}' to SELECT 1 — it is not up." >&2
    echo "             Scoring now would compare two broken legs and call them identical." >&2
    return 1
  fi
}

# The oracle needs a database to be IN; SPG's mysql-wire serves one
# schema and takes no `-D`. Recreating it per run is also the reset.
printf 'DROP DATABASE IF EXISTS mysqlcorpus; CREATE DATABASE mysqlcorpus;\n' \
  | docker exec -i "$ORACLE" mysql -h 127.0.0.1 -u root -pbench --batch 2>/dev/null

REBASE=0
args=()
for a in "$@"; do
  if [ "$a" = "--rebaseline" ]; then REBASE=1; else args+=("$a"); fi
done
set -- ${args[@]+"${args[@]}"}

probe_leg SPG SPG || exit 2
probe_leg MySQL MYS || exit 2

# v7.40.0 — WHICH BINARY ANSWERED.
#
# `rsync -a` preserves mtime, so cargo can decide a source file is not
# newer than the artefact and skip the rebuild — and the corpus then
# grades a binary from before the change. It happened once here: two
# files that had been IDENTICAL for three runs came back with nine and
# five differing lines, all of them answers the previous build gave, and
# the numbers were recorded as a baseline before the cause was known.
#
# So the leg says its version and it has to be the workspace's. A stale
# binary is now a loud refusal instead of a quiet re-baseline.
# NOT `VERSION()`: on the MySQL wire that answers `9.7.2-spg`, the
# version SPG emulates, which never moves. `spg_version()` is SPG's own
# and MySQL does not have it.
spg_version="$(printf 'SELECT spg_version();\n' | SPG 2>/dev/null | norm | head -1)"
want_version="$(awk -F\" '/^version/ {print $2; exit}' "$ROOT/Cargo.toml")"
# `spg_version()` answers `SPG 7.40.0`, so the version is contained, not
# a prefix.
case "$spg_version" in
  *"$want_version"*) : ;;
  *)
    echo "mysqlcorpus: the SPG leg answers spg_version() '$spg_version', and this tree" >&2
    echo "             is $want_version. That is a stale binary — cargo skipped the" >&2
    echo "             rebuild (rsync -a preserves mtime). Touch the sources and" >&2
    echo "             run again; scoring now would grade the previous build." >&2
    exit 2
    ;;
esac

# Reset the SPG leg the way the PostgreSQL corpus does: enumerate and
# drop, because SPG serves one built-in schema that cannot be dropped.
if [ "${NO_RESET:-0}" != 1 ]; then
  for _pass in 1 2; do
    SPG <<'GEN' 2>/dev/null | grep -E '^DROP' | SPG >/dev/null 2>&1
SELECT CONCAT('DROP TABLE IF EXISTS `', table_name, '`;')
  FROM information_schema.tables WHERE table_schema NOT IN
       ('information_schema','performance_schema','mysql','sys','pg_catalog');
GEN
  done
fi

: > "$OUT/actual.tsv"
files=("$@"); [ ${#files[@]} -eq 0 ] && files=("$HERE"/[0-9]*.sql)

for f in "${files[@]}"; do
  n="$(basename "$f" .sql)"
  SPG < "$f" > "$OUT/$n.spg.raw" 2> "$OUT/$n.spg.eraw"
  MYS < "$f" > "$OUT/$n.mysql.raw" 2> "$OUT/$n.mysql.eraw"
  for side in spg mysql; do
    norm < "$OUT/$n.$side.raw"  > "$OUT/$n.$side"
    norm < "$OUT/$n.$side.eraw" > "$OUT/$n.$side.err"
  done
  d_out=0; d_err=0
  diff -u "$OUT/$n.mysql" "$OUT/$n.spg" > "$OUT/$n.diff" 2>&1 \
    || d_out=$(grep -cE '^[+-][^+-]' "$OUT/$n.diff")
  diff -u "$OUT/$n.mysql.err" "$OUT/$n.spg.err" > "$OUT/$n.diff.err" 2>&1 \
    || d_err=$(grep -cE '^[+-][^+-]' "$OUT/$n.diff.err")
  d=$((d_out + d_err))
  printf '%s\t%s\n' "$n" "$d" >> "$OUT/actual.tsv"
  if [ "$d" -eq 0 ]; then
    printf '%-28s IDENTICAL\n' "$n"
  else
    printf '%-28s %s 行差异 (rows %s / errors %s)\n' "$n" "$d" "$d_out" "$d_err"
  fi
done

BASE="$HERE/baseline.tsv"
if [ "$REBASE" = 1 ] || [ "${REBASELINE:-0}" = 1 ]; then
  sort "$OUT/actual.tsv" > "$BASE"
  echo "rebaselined -> $BASE"
  exit 0
fi
if [ ! -f "$BASE" ]; then
  echo "no baseline.tsv; run with --rebaseline to record one" >&2
  exit 2
fi
# Name the oracle. The image is the rolling `mysql:9` tag, so it moves
# under a running project; a verdict names both of its sides or neither.
ORACLE_VERSION="$(printf 'SELECT VERSION();' | MYS 2>/dev/null | norm | head -1)"
[ -n "$ORACLE_VERSION" ] || ORACLE_VERSION="(the oracle did not answer SELECT VERSION())"
echo "oracle: MySQL $ORACLE_VERSION"

if diff -u "$BASE" <(sort "$OUT/actual.tsv") > "$OUT/baseline.diff" 2>&1; then
  echo "MYSQL CORPUS OK — matches baseline ($(awk -F'\t' '{s+=$2} END{print s}' "$BASE") lines)"
  exit 0
fi
echo "MYSQL CORPUS DEVIATES from baseline:"
grep -E '^[+-][^+-]' "$OUT/baseline.diff"
exit 1
