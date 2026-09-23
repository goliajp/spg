#!/usr/bin/env bash
# Gate 7 — sentori's ingest transaction, SPG against PostgreSQL.
#
#   ./g7-perf.sh <spg-image> [rounds] [seconds]
#
# The workload is `self-hosted/server/src/pipeline.rs`'s ingest, statement
# for statement (ingest.pgb). Both legs are containers with the same
# limits, reached by the same route, driven by the same client (pgbench
# from the oracle image, `-M prepared`, which is the protocol sqlx uses).
#
# Rounds ALTERNATE the legs, and a third leg runs the SPG image against
# itself as a control: where the same binary separates from itself, the
# panel cannot call a difference between binaries. Verdicts read the
# min/max of the rounds — non-overlapping ranges, per BENCH_PROTOCOL.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]:-$0}")"
source ./lib.sh
IMG=${1:?usage: g7-perf.sh <spg-image> [rounds] [seconds]}
ROUNDS=${2:-3}
SECS=${3:-20}
ISSUES=${ISSUES:-200}
USERS=${USERS:-1000}
N="g7$$"
trap 'docker rm -f $N-s $N-p $N-c >/dev/null 2>&1' EXIT

pb() { # port conns -> "tps latency_ms"
  docker run --rm --network host -v "$PWD":/w -e PGPASSWORD=p "$ORACLE" \
    pgbench -h 127.0.0.1 -p "$1" -U u -d d -n -M prepared -f /w/ingest.pgb \
    -D issues=$ISSUES -D users=$USERS -c "$2" -j "$2" -T "$SECS" 2>&1 \
  | awk '/^tps/ {t=$3} /latency average/ {l=$4} END {printf "%.1f %s", t, l}'
}

echo "== gate 7: sentori ingest — $IMG vs $ORACLE ($ROUNDS rounds x ${SECS}s, limits: $LIMITS)"
boot "$IMG" 17301 $N-s || exit 2
boot "$ORACLE" 17302 $N-p || exit 2
boot "$IMG" 17303 $N-c || exit 2
for p in 17301 17302 17303; do load_schema $p || exit 2; seed $p "$ISSUES" || exit 2; done

# One file per cell. macOS ships bash 3.2, which has no associative
# arrays — and the first cut of this panel read the missing values as
# empty and printed `equal` for every cell of a run that measured
# nothing. A panel that cannot fail is not a panel.
SAMPLES=$(mktemp -d); trap 'docker rm -f $N-s $N-p $N-c >/dev/null 2>&1; rm -rf "$SAMPLES"' EXIT
for c in 1 4 8; do
  for r in $(seq 1 "$ROUNDS"); do
    for leg in s:17301 p:17302 c:17303; do
      name=${leg%%:*}; port=${leg##*:}
      read -r t l <<< "$(pb "$port" "$c")"
      echo "$t" >> "$SAMPLES/$name.$c"
      printf '  c=%s round=%s %s tps=%s latency=%sms\n' "$c" "$r" "$name" "$t" "$l"
    done
  done
done

# lo hi of a cell's samples, or nothing when it has fewer than it should.
rng() {
  local f=$SAMPLES/$1 n
  n=$(grep -cv '^$' "$f" 2>/dev/null || echo 0)
  [ "$n" -eq "$ROUNDS" ] || return 1
  sort -n "$f" | awk 'NR==1{lo=$1} {hi=$1} END{printf "%s %s", lo, hi}'
}
echo "== verdict"
rc=0
for c in 1 4 8; do
  if ! read -r slo shi <<< "$(rng "s.$c")" \
    || ! read -r plo phi <<< "$(rng "p.$c")" \
    || ! read -r clo chi <<< "$(rng "c.$c")" \
    || [ -z "${slo:-}" ] || [ -z "${plo:-}" ] || [ -z "${clo:-}" ]; then
    echo "  c=$c  CANNOT MEASURE — a leg produced fewer than $ROUNDS readings"
    rc=2
    continue
  fi
  # The control's own spread is the floor: a gap inside it is not a gap.
  verdict=$(awk -v slo="$slo" -v shi="$shi" -v plo="$plo" -v phi="$phi" -v clo="$clo" -v chi="$chi" 'BEGIN{
    if (clo > 0 && chi/clo > 1.5) { print "unresolved (control spread " chi/clo "x)"; exit }
    if (shi < plo) { printf "LOSS %.2fx\n", plo/shi; exit }
    if (slo > phi) { printf "win %.2fx\n", slo/phi; exit }
    print "equal"
  }')
  printf '  c=%-2s SPG %s-%s  PG %s-%s  control %s-%s  -> %s\n' "$c" "$slo" "$shi" "$plo" "$phi" "$clo" "$chi" "$verdict"
  case "$verdict" in LOSS*) rc=1;; esac
done
exit $rc
