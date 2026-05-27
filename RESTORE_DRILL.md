# Restore drill — SPG full + incremental + PITR

Operator promise: you can recover from a primary loss in under
five minutes with no data older than the last incremental
backup. This document walks every step. The companion test
`crates/spg-server/tests/e2e_restore_drill.rs` runs these same
commands end-to-end on every CI build — if the doc rots, the
test fails.

Audience: someone who has never restored an SPG node before.
No prior knowledge of the storage layout assumed.

---

## What you need

- The `spg-server` binary on the recovery host (same major
  version as the source; cross-version restore is covered by
  the v4.31 STABILITY contract).
- The bundle files the source produced via
  `BACKUP TO '<path>'`. At least one **full** bundle (kind=0) and
  zero or more **incremental** bundles (kind=1).
- An empty directory to assemble the recovered db + WAL into.

## Step 0 — establish what you have

The bundle file is self-describing. The first 41 bytes are a
fixed header. Inspect a bundle without applying it:

```bash
python3 - <<'PY'
import struct, sys, pathlib
p = pathlib.Path("full.bkp")
b = p.read_bytes()
assert b[:8] == b"SPGBKUP\x01", "not an SPG bundle"
kind = b[8]
since = struct.unpack_from("<Q", b, 9)[0]
ts_us = struct.unpack_from("<Q", b, 17)[0]
snap_len = struct.unpack_from("<Q", b, 25)[0]
wal_pos = struct.unpack_from("<Q", b, 33 + snap_len)[0]
wal_len = struct.unpack_from("<Q", b, 33 + snap_len + 8)[0]
print(f"{p.name}: kind={'full' if kind == 0 else 'incremental'}  "
      f"since={since}  wal_pos={wal_pos}  snap_len={snap_len}  wal_len={wal_len}  "
      f"ts={ts_us}us")
PY
```

A clean recovery starts from one full bundle. Find your most
recent full (the one with `kind=full`), then collect every
incremental whose `since` chains forward from it: the first
incremental's `since` must equal the full's `wal_pos`; the
second incremental's `since` must equal the first incremental's
`wal_pos`; etc. Any gap means you'll lose writes — stop and ask.

## Step 1 — assemble db + WAL from bundles

```bash
# 1. Pick a recovery directory.
REC=/var/lib/spg/recovery
mkdir -p "$REC"

# 2. Apply the full bundle. This writes snapshot into rec.db
#    and resets rec.wal to empty.
python3 - "$REC/rec.db" "$REC/rec.wal" full.bkp <<'PY'
import struct, sys
db, wal, bundle = sys.argv[1:4]
b = open(bundle, 'rb').read()
assert b[:8] == b"SPGBKUP\x01"
snap_len = struct.unpack_from("<Q", b, 25)[0]
snap_end = 33 + snap_len
wal_len = struct.unpack_from("<Q", b, snap_end + 8)[0]
wal_start = snap_end + 16
wal_slice = b[wal_start:wal_start + wal_len]
if snap_len:
    open(db, 'wb').write(b[33:33 + snap_len])
    open(wal, 'wb').write(wal_slice)
PY

# 3. Apply each incremental bundle in order. Each one appends
#    to rec.wal (no snapshot inside an incremental).
for incr in incr-*.bkp; do
  python3 - "$REC/rec.wal" "$incr" <<'PY'
import struct, sys
wal, bundle = sys.argv[1:3]
b = open(bundle, 'rb').read()
assert b[:8] == b"SPGBKUP\x01"
snap_len = struct.unpack_from("<Q", b, 25)[0]
snap_end = 33 + snap_len
wal_len = struct.unpack_from("<Q", b, snap_end + 8)[0]
wal_start = snap_end + 16
with open(wal, 'ab') as f:
    f.write(b[wal_start:wal_start + wal_len])
PY
done
```

## Step 2 — point-in-time recovery (optional)

If you want to stop replay at a specific byte offset in the
assembled WAL (rolling back a bad write), set
`SPG_REPLAY_UPTO=<n>` for **just the next startup**:

```bash
export SPG_REPLAY_UPTO=0    # = snapshot only, ignore the WAL
# or:
export SPG_REPLAY_UPTO=4096 # = replay the first 4096 WAL bytes
```

Drop the env var on subsequent restarts — leaving it set forces
the truncation every time.

## Step 3 — start the recovered server

```bash
spg-server 127.0.0.1:5544 "$REC/rec.db" - "$REC/rec.wal"
```

You should see, on stderr:

```
spg-server: restored N table(s), M user(s) from /var/lib/spg/recovery/rec.db
spg-server: replayed K WAL entries from /var/lib/spg/recovery/rec.wal
spg-server: listening on 127.0.0.1:5544
```

If `replayed K` is zero on a full+incremental recovery, that
means the incrementals' WAL slices were not appended correctly.
Check the script in step 1.

## Step 4 — verify

Connect with `psql` (if PG-wire enabled) or the native client and
run the smallest read that proves the recovery worked end-to-end:

```bash
psql -h 127.0.0.1 -p 5544 -U bench -d bench -c 'SELECT count(*) FROM <a known table>'
```

Compare against the row count you remember from the source. If
this is post-PITR, the count should match the SPG_REPLAY_UPTO
boundary you set.

## Step 5 — re-bootstrap follower (if you had replication)

The recovered node is now a fresh primary. Followers that were
pointed at the old primary must re-bootstrap:

```bash
# On each follower host:
rm -f /var/lib/spg/follower.db /var/lib/spg/follower.wal
SPG_FOLLOW_OF=<new-primary>:<repl-port> spg-server \
  127.0.0.1:5544 /var/lib/spg/follower.db - /var/lib/spg/follower.wal
```

The follower will reconnect, fetch a fresh snapshot from the new
primary, and tail its WAL.

---

## How long should this take?

Reference numbers from `xtests/v4_25_backup_report.md` on M-series
8-core, 100K-row dataset:

| step                              | wall time     |
|-----------------------------------|---------------|
| Apply one full bundle (878 KiB)   | 5 ms          |
| Apply one incremental (168 KiB)   | 5 ms          |
| Server startup + WAL replay       | 261 ms        |
| **Total restore round-trip**      | **~270 ms**   |

For larger datasets the snapshot apply is bounded by disk write
bandwidth; the WAL replay is bounded by `engine.execute` rate
(roughly the INSERT rate, ~1M/s on the same hardware).

## What this drill does NOT cover

- **Automated failover** — manual promotion only; SPG does not
  ship a coordinator.
- **TLS recovery** — TLS is out of scope; the bundle file is
  unencrypted at rest. Encrypt with `age` / `gpg` before
  archiving off-host.
- **Cross-version restore** — covered by v4.31 STABILITY contract
  (snapshot v4.0 must restore on v4.x latest).

---

The companion test
`crates/spg-server/tests/e2e_restore_drill.rs` runs this exact
sequence using the same Python snippets via Command, so the
commands above are guaranteed to keep working.
