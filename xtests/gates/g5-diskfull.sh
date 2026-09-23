#!/usr/bin/env bash
# sentori acceptance gate 5 — the disk fills up.
#
#   xtests/gates/g5-diskfull.sh <image> [size_mb] [port]
#
# The data directory sits on a disk image of a fixed size, so the host
# can take the space away and give it back. Five things are asked:
#
#   refuses   the write that has no room fails, and says so (PG: 53100)
#   alive     the server is still answering
#   reads     a read still works while there is no room
#   resumes   a write works once the space is back
#   intact    after a restart, every table hashes to what it held
#
# `SELFTEST=1` adds the negative control: the same write is run with
# room to spare, and has to succeed — otherwise `refuses` would be
# green for a write that never worked at all.
#
# macOS only for now: the size-limited filesystem is an hdiutil image.

GATES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
# shellcheck source=xtests/gates/lib.sh
. "$GATES_DIR/lib.sh"

IMAGE=${1:?usage: g5-diskfull.sh <image> [size_mb] [port]}
SIZE=${2:-400}
PORT=${3:-17541}
NAME=g5df
DATA=$(datadir "$IMAGE")
DMG=/tmp/g5df-$$.dmg
MNT=/tmp/g5df-$$
cleanup() { docker rm -f "$NAME" >/dev/null 2>&1; hdiutil detach "$MNT" -quiet 2>/dev/null; rm -f "$DMG" "$DMG.sparseimage"; rmdir "$MNT" 2>/dev/null; rm -rf "$WORK"; }
WORK=$(mktemp -d)
trap cleanup EXIT

command -v hdiutil >/dev/null || { echo "g5-diskfull: needs hdiutil (macOS); not measured here"; exit 2; }
mkdir -p "$MNT"
hdiutil create -size "${SIZE}m" -fs "Case-sensitive APFS" -volname g5df -quiet "$DMG" || exit 2
hdiutil attach "$DMG" -mountpoint "$MNT" -nobrowse -quiet || exit 2

free_mb() { df -m "$MNT" | awk 'NR==2 {print $4}'; }
# Take all but `keep` megabytes.
eat() { dd if=/dev/zero of="$MNT/FILLER" bs=1m count=$(( $(free_mb) - ${1:-1} )) 2>/dev/null; sync; }
give_back() { rm -f "$MNT/FILLER"; sync; }

# 64 KB of random hex, made ONCE on the host.
#
# It has to be random, and it has to travel as a literal. SPG's WAL
# records the SQL TEXT, so a row built server-side — `repeat('x', 65536)`
# — costs about seventy bytes of WAL however many megabytes of rows it
# makes, and LZSS would flatten a run of one character anyway. The first
# version of this harness wrote 600 such rows onto a filesystem with 9 MB
# free and every one of them succeeded: nothing had reached the disk.
PAD=$(LC_ALL=C tr -dc 'a-f0-9' < /dev/urandom | head -c 65536)

# `VERBOSITY=verbose` so a refusal names its SQLSTATE: PostgreSQL says
# 53100 (disk_full) and what SPG says has to be comparable, not just
# similarly worded.
write_one() { # <tag> -> the server's answer, first line only
  q "$PORT" -v VERBOSITY=verbose -tAc "INSERT INTO events (id, project_id, issue_id, kind, platform, occurred_at, payload)
     SELECT gen_random_uuid(), '00000000-0000-0000-0000-000000000001', id, 'error', 'ios', now(),
            ('{\"tag\":\"$1\",\"pad\":\"$PAD\"}')::jsonb
     FROM issues LIMIT 1 RETURNING 1" 2>&1 | head -1 | cut -c1-120
}

# The engine keeps rows in memory until a checkpoint writes them out, so
# a harness that only writes is asking whether the WAL volume fills up,
# not whether the database does.
checkpoint() { q "$PORT" -tAc "CHECKPOINT" < /dev/null > /dev/null 2>&1; }

echo "g5-diskfull: $IMAGE on a ${SIZE}MB filesystem"
boot "$IMAGE" "$PORT" "$NAME" -v "$MNT:$DATA" || { echo "  ✗ it does not start with its data directory on this filesystem"; exit 2; }
load_schema "$PORT" || exit 2
seed "$PORT" 50 || exit 2
fingerprint_db "$PORT" > "$WORK/before"
[ "$(awk 'END{print NR}' "$WORK/before")" -ge 20 ] || { echo "  ✗ fingerprinted too few tables"; exit 2; }

rc=0
if [ "${SELFTEST:-0}" = 1 ]; then
  a=$(write_one roomy)
  case "$a" in 1) echo "  selftest: the same write succeeds with room to spare: ok" ;;
                *) echo "  selftest: ✗ the write fails even with room: $a"; rc=1 ;; esac
fi

eat 1
echo "  ${SIZE}MB filesystem, $(free_mb)MB left"
# Keep writing until one write has to fail. The filesystem does not hand
# back every byte it says is free (APFS keeps a reserve), so "take all
# but one megabyte" is not by itself enough to make the next write fail
# — the rows have to actually consume what is left. Each row is ~64 KB,
# and the cap is well past the free space the harness leaves.
ans=""; wrote=0
for i in $(seq 1 600); do
  ans=$(write_one "full$i")
  case "$ans" in 1) wrote=$((wrote + 1)) ;; *) break ;; esac
  [ $((i % 25)) -eq 0 ] && checkpoint
done
case "$ans" in
  1) echo "  refuses: ✗ $wrote writes all succeeded, $(free_mb)MB still free"; rc=1 ;;
  *ERROR:*53100*) echo "  refuses: after $wrote rows, 53100 — $(echo "$ans" | head -c 70)" ;;
  *) echo "  refuses: ✗ after $wrote rows, not 53100 (PostgreSQL's disk_full) — $(echo "$ans" | head -c 70)"; rc=1 ;;
esac

alive=$(q "$PORT" -tAc "SELECT 1" 2>&1 | tr -d '[:space:]')
[ "$alive" = 1 ] && echo "  alive: ok" || { echo "  alive: ✗ $alive"; rc=1; }
r=$(q "$PORT" -tAc "SELECT count(*) FROM issues" 2>&1 | tr -d '[:space:]')
[ "$r" = 50 ] && echo "  reads: ok" || { echo "  reads: ✗ $r"; rc=1; }

give_back
echo "  space back: $(free_mb)MB free"
a=$(write_one recovered)
case "$a" in 1) echo "  resumes: ok" ;; *) echo "  resumes: ✗ $(echo "$a" | head -c 90)"; rc=1 ;; esac

docker restart "$NAME" >/dev/null 2>&1
if wait_up "$PORT"; then
  fingerprint_db "$PORT" > "$WORK/after"
  # The rows this harness wrote are expected to differ; the tables the
  # migrations and the seed filled are not.
  # `events` is where this harness wrote; every other table is the one
  # the migrations and the seed filled and nothing has touched since.
  d=$(join "$WORK/before" "$WORK/after" | awk '$1 != "events" && $2 != $3 { printf "%s ", $1 }')
  [ -z "$d" ] && echo "  intact: ok" || { echo "  intact: ✗ changed: $d"; rc=1; }
else
  echo "  intact: ✗ it did not come back after the restart"
  docker logs --tail 20 "$NAME" 2>&1 | sed 's/^/      /'; rc=1
fi

[ "$rc" = 0 ] && echo "g5-diskfull: $IMAGE PASS" || echo "g5-diskfull: $IMAGE FAIL"
exit "$rc"
