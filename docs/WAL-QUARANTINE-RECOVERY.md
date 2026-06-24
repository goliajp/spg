# WAL quarantine + recovery procedure

When an spg-embedded process cannot boot — typically because WAL records
are corrupt, or because a previous-generation lock is wedging
`Database::open_path` — an operator may need to **quarantine** the WAL
to let the database start.

**Pre-v7.37.10**, this was usually `rm -rf` of the WAL directory.
Result: the WAL records since the last successful `checkpoint()` were
**irreversibly lost**. mailrs reported this scenario in cascade-7
(2026-06-23): a 17-hour gap between `base.spg` mtime updates lost 14h
of mailbox traffic on a single deploy-failure-driven quarantine.

**v7.37.10+ ships two things that change this**:

1. Time-based auto-checkpoint (default 60 s, `SPG_EMBEDDED_CHECKPOINT_SECONDS=N`).
   The data-loss window on any quarantine is now bounded to roughly
   `N` seconds of writes, not the wall time between graceful shutdowns.
2. The procedure below: **move-not-remove** the WAL, plus a recovery
   path using `spgctl pitr-restore`.

If you've been `rm`-ing your WAL, **stop**. Use the move-not-remove
procedure below instead. With it, even quarantines you decide to
quarantine *and* never recover are forensically reviewable later (vs.
the previous "the bytes are gone" state).

---

## The safe quarantine procedure

> **Goal**: unblock `Database::open_path` while preserving every WAL
> byte for forensic review + possible recovery.

### Step 1 — stop the spg process

```bash
docker compose stop spg          # or whatever your orchestrator does
# or for a non-container: SIGTERM the pid that holds /data/spg/mailrs.spg
```

If the process won't stop cleanly (the case that drove you here in the
first place), SIGKILL is acceptable. **Do not skip step 1**; concurrent
file moves while the process is alive can break the on-disk WAL
ordering.

### Step 2 — preserve the base snapshot

```bash
ts=$(date -u +%Y%m%dT%H%M%SZ)
cp /data/spg/mailrs.spg /data/spg/base-before-quarantine-${ts}.spg
```

Future recovery uses this as the *replay starting point*. Do NOT skip
this step — without it, the WAL records in quarantine have nothing to
replay against.

### Step 3 — move (do NOT remove) the WAL

```bash
mv /data/spg/mailrs.spg.wal /data/spg/wal-quarantine-${ts}
```

Move the entire `.wal` directory. SPG identifies WAL chunks by
filename inside the directory; moving the directory atomically removes
the WAL from the live data path while preserving every chunk file.

If your `.wal` is a single file (very old layout), move it the same
way: `mv mailrs.spg.wal mailrs.spg.wal.quarantine-${ts}`.

### Step 4 — clear the on-disk lock

```bash
rm -rf /data/spg/mailrs.spg.lock
```

The `.lock` is a directory created by `Database::open_path`'s
`acquire_path_lock`. With v7.37.11+'s pid-and-start-time identity
check, a stale lock from a dead process is detected and reclaimed
automatically — `rm -rf` should not be needed for this case. But for
truly wedged states (e.g. the v7.37.5-RAII-claim-vs-reality recurrences
mailrs hit through v7.37.10), removing the lock unblocks a fresh
`open_path` call.

### Step 5 — start the process

```bash
docker compose start spg
```

The process now boots against `/data/spg/mailrs.spg` (the unchanged
base) and an empty WAL directory. No records to replay; the in-memory
state equals the base. With v7.37.10+ enabled (default), the
time-based auto-checkpoint immediately starts a 60 s window for the
next checkpoint, so the base mtime advances soon after the first
write.

At this point the **system is operational, but it has lost every
write since the last successful checkpoint that produced the base
file** (typically ≤ 60 s with v7.37.10+; up to 14 h on pre-v7.37.10).
Step 6 (optional) recovers that gap.

---

## Step 6 — recover the quarantined WAL (OPTIONAL)

This step takes the preserved base + quarantined WAL chunks and
reconstructs a database that includes all the writes the quarantine
"lost". You then choose what to merge back into prod.

> **When to skip Step 6**: if the quarantined WAL is small (≤ 60 s of
> writes via v7.37.10+) and the application can tolerate that loss,
> skip. The procedure below is for the case where the gap is large
> (pre-v7.37.10) or the writes are critical.

### Step 6a — list the quarantined chunks

```bash
ls -laSh /data/spg/wal-quarantine-${ts}/
```

Each `.chunk` file (or numbered file for the legacy layout) is a WAL
chunk. They're ordered by LSN; the smallest filename has the lowest
LSN.

### Step 6b — replay onto the preserved base

```bash
# For each quarantined WAL chunk, in LSN order:
for chunk in /data/spg/wal-quarantine-${ts}/*.chunk; do
    spg pitr-restore \
        --snapshot /data/spg/base-before-quarantine-${ts}.spg \
        --wal "${chunk}" \
        --to "$(date +%s%6N)" \
        --target /tmp/recovered-${ts}.spg
done
```

After the loop, `/tmp/recovered-${ts}.spg` is a database file that
contains the base state PLUS every well-formed record in the
quarantined WAL.

The `--to` value is a "replay up to" wall-clock timestamp in
microseconds (a Unix epoch number). Pass `$(date +%s%6N)` for "replay
everything", or any specific timestamp for point-in-time recovery.

The PITR replay path automatically:
- skips `WAL_V4_TYPE_DURABILITY_CHECKPOINT` (`0x02`) records
- skips `WAL_V4_TYPE_CHECKPOINT_MARKER` (`0x11`) records
- applies `WAL_V4_TYPE_AUTO_COMMIT_SQL` (`0x10`) records as SQL
- applies `WAL_V4_TYPE_TX_COMMIT_SQL` (`0x12`) records as SQL
- applies `WAL_V5_TYPE_ROW_REDO` (`0x13`) records as physical
  `apply_redo` (v7.37.9+; older spgctl errors on these)
- treats any other byte as a corrupt record and errors

If a chunk errors, that chunk's records past the error LSN are NOT
recovered. The replay loop above skips to the next chunk and
continues; you get partial recovery rather than nothing.

### Step 6c — compare recovered with current prod

```bash
spg query \
    --db /tmp/recovered-${ts}.spg \
    --sql "SELECT thread_id, MAX(internal_date) FROM messages GROUP BY thread_id" \
    > /tmp/recovered-summary-${ts}.txt
spg query \
    --db /data/spg/mailrs.spg \
    --sql "SELECT thread_id, MAX(internal_date) FROM messages GROUP BY thread_id" \
    > /tmp/current-summary-${ts}.txt
diff /tmp/current-summary-${ts}.txt /tmp/recovered-summary-${ts}.txt
```

The diff shows which rows the quarantine cost you. Common patterns:

- **Mailbox arrival**: `recovered` has rows `current` doesn't (mail
  received during the gap).
- **Read receipts / flag updates**: `recovered` has different `flags`
  values; `current` reset to the base-snapshot state.
- **Account changes**: `recovered` has updated `accounts` rows.

### Step 6d — merge what you want

This step depends on your application schema. For mailrs specifically,
the most common pattern is "replay the unread/received mailbox events
into prod":

```bash
spg dump --db /tmp/recovered-${ts}.spg \
    --table messages \
    --where "internal_date > '<gap_start_ts>'" \
    --format insert-statements \
    > /tmp/lost-messages-${ts}.sql
psql -h <prod_host> -f /tmp/lost-messages-${ts}.sql
```

Or, for more complex merges (deduplication, conflict resolution),
script in your application's language against the two databases.

### Step 6e — remove the quarantine

Once you've recovered what you want, you can free the disk:

```bash
rm -rf /data/spg/wal-quarantine-${ts}
rm /data/spg/base-before-quarantine-${ts}.spg
rm /tmp/recovered-${ts}.spg
```

Or keep them for compliance / forensic review.

---

## Why a script-driven recovery (not built into Database::open_path)

`Database::open_path` is the live-service boot path. It must run in
deterministic time and never block on operator decisions. Quarantine
recovery is an operator workflow with policy decisions ("which writes
do I want to merge back?"). Putting it inside `open_path` would either
force every boot to make policy decisions (wrong abstraction) or
silently make them (wrong defaults).

`spgctl pitr-restore` is the right tool for the recovery step because
it's already the engine's replay path, exposed as a CLI with full
LSN/timestamp control.

---

## What `v7.37.10`+ time-based checkpoint changes

Pre-v7.37.10, the byte-threshold auto-checkpoint (default 4 MiB)
needed the workload to accumulate that many bytes between graceful
shutdowns. For a slow-write workload like mailrs (~30 KB/hr in their
production), the threshold needed > 130 h to fire — so it almost
never did, and the base mtime stuck at the last graceful shutdown's
timestamp.

v7.37.10's time-based companion (default 60 s,
`SPG_EMBEDDED_CHECKPOINT_SECONDS`) bounds the base-mtime-staleness
window. Result: a quarantine after v7.37.10 loses ≤ 60 s of writes,
versus the 14 h mailrs experienced.

If you have a slow-write workload, accept the default. If you have a
fast-write workload and the 60 s checkpoint cost shows up in your
profile, tune `SPG_EMBEDDED_CHECKPOINT_SECONDS=N` upward (e.g. 300
for 5-minute windows). Setting it to `0` disables the timer — the
byte-threshold path remains, which is fine for fast-write workloads
where 4 MiB accumulates quickly.

---

## Known limits

- **PITR replay needs the original base**, not the current one. If
  you `rm` the base before recovery, the WAL records have nothing to
  replay against. Always preserve `base.spg` via `cp` before
  quarantine (Step 2).
- **Replay applies records in LSN order across chunks**. If you
  shuffle the quarantined chunk files, replay may apply records out
  of order and surface as constraint violations. Preserve directory
  ordering; the `spg pitr-restore` loop above iterates by sorted
  filename, which matches LSN order.
- **CRC-corrupt records halt replay at the corruption**. spgctl
  treats this as a hard stop for the current chunk; subsequent chunks
  in the loop continue. The records past the corrupt one in that
  chunk are lost. If your quarantine was triggered BY a corruption,
  this is the limit of automatic recovery.
- **Application-level migration / merge is out of scope here**. Step
  6d's `psql -f` example assumes the same schema; if your prod has
  drifted, you'll need to write a per-row merge in your application's
  language.

---

## Reporting issues

If the recovery procedure doesn't work for your workload, please file
a forensic snapshot to `gh issue new` with:

- The contents of `wal-quarantine-<ts>/` (or its size + chunk
  filenames if too large to attach).
- The `base-before-quarantine-<ts>.spg`'s size (do not attach the
  file — it likely contains customer data).
- The exact `spgctl pitr-restore` command + its full stderr output.
- The CHANGELOG line that says which spg version produced the
  quarantined WAL.
