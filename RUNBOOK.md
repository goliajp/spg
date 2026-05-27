# SPG runbook — common alerts and how to respond

For each alert below: how to confirm it's real, the most likely
cause, what to do, and what NOT to do.

---

## Alert: `spg_connections_active` near `SPG_MAX_CONNECTIONS`

**Confirm**: `curl -s http://${SPG_HTTP_ADDR}/metrics | grep spg_connections_active`.

**Likely cause**: client connection pool misconfigured (no idle
timeout on client side), or `SPG_MAX_CONNECTIONS` set too low.

**Do**:
- If the cap is too low for the workload, raise it: stop server,
  set higher `SPG_MAX_CONNECTIONS`, restart.
- If clients are leaking connections, set `SPG_IDLE_TIMEOUT_SEC`
  so the server reaps them.

**Do NOT**: kill the process to "reset connections" — pending
WAL records that haven't returned CC are at risk. Let
`SPG_QUERY_TIMEOUT_MS` and the OS shutdown drain finish first.

---

## Alert: high `spg_errors_total` rate

**Confirm**: rate increasing over baseline; correlate with stderr
log (set `SPG_LOG_FORMAT=json` for structured tail).

**Likely cause**:
- a client is sending malformed SQL repeatedly
- WAL fsync starting to fail (disk pressure)
- query timeout firing under load

**Do**:
- Check stderr for the specific error message family.
- If it's `wal quota exceeded`: the chaos knob
  `SPG_FAIL_WAL_QUOTA_BYTES` is accidentally set. Unset and
  restart.
- If it's `wal append failed` (real ENOSPC): see "Disk full".
- If it's `query timeout`: raise `SPG_QUERY_TIMEOUT_MS` or fix
  the slow query (`EXPLAIN ANALYZE`).

---

## Alert: disk full (WAL volume)

**Confirm**: `df -h $(dirname $SPG_WAL)`.

**Likely cause**: WAL hasn't been checkpointed in a long time
and/or the volume isn't sized for sustained write load.

**Do**:
1. Take a full backup immediately: `BACKUP TO '/tmp/safe.bkp'`
   (admin only). This succeeds even with the WAL near full — the
   snapshot is in-memory and the bundle is on a different volume.
2. Stop the server.
3. Checkpoint: move the WAL aside, the snapshot file is the
   current state. `mv $SPG_WAL $SPG_WAL.checkpointed-$(date +%s)`.
4. Start the server. It restarts with an empty WAL on the same
   snapshot.

**Do NOT**: delete the WAL while the server is running. Any
in-flight write that hadn't returned CC will be silently lost.

---

## Alert: replication lag growing on follower

**Confirm** (until v4.32 ships a lag metric): on the follower,
`SELECT count(*) FROM <a known table>` against follower vs
primary. Difference > expected steady-state means lag.

**Likely cause**:
- Primary is writing faster than follower's apply + fsync.
- Follower disk slower than primary.
- Network bandwidth between primary and follower throttled.

**Do**:
- Confirm follower's CPU and disk aren't saturated.
- If they are: the follower is undersized; either move to faster
  hardware or accept the lag.
- If they aren't: check `SPG_FOLLOW_OF` network round-trip
  (`ping`, `iperf3`) — it may be the link.

The 50 ms WAL-tail-poll cadence sets a floor on lag; see
`crates/spg-server/src/replication.rs::TAIL_POLL`. Lowering it
trades CPU for tighter lag.

---

## Alert: server failed to start ("WAL replay rejected")

**Confirm**: stderr says `WAL replay rejected "<SQL>": <reason>`.

**Likely cause**: WAL is corrupted or the SQL was valid against
an older binary version but rejected by current parser.

**Do**:
1. Save the current WAL aside: `cp $SPG_WAL $SPG_WAL.corrupt`.
2. Try PITR: set `SPG_REPLAY_UPTO=<offset>` where `<offset>` is
   just before the rejected record. The rejected record's
   position is in the message; the bytes before it are good.
3. After server starts, take a fresh full backup, drop the
   corrupt WAL, restart without PITR.

**Do NOT**: hand-edit the WAL. Records are length-prefixed; an
off-by-one shifts every later record.

---

## Alert: backup taking longer than expected

**Confirm**: time `BACKUP TO '<path>'`. Reference numbers in
`xtests/v4_25_backup_report.md`: 100K rows → 878 KiB → 5 ms.

**Likely cause**: very large catalog (millions of rows) — the
snapshot serialize is O(rows). Or disk write is slow.

**Do**: switch to incremental backups between full backups. A
full backup every hour + incremental every 5 minutes is a
reasonable starting policy.

---

## Alert: follower bootstrap times out

**Confirm**: follower stderr says it can't connect to
`SPG_REPL_ADDR` or hangs at "starting fresh WAL".

**Likely cause**:
- Primary isn't listening on the repl port.
- Firewall blocks the repl port from follower.

**Do**:
1. From the follower host: `nc -zv <primary> <repl-port>`.
2. If unreachable, fix the firewall.
3. Snapshot bootstrap can take time for very large catalogs;
   measured at ~240 ms for 1K rows (xtests/v4_24_repl_report.md).
   Scale linearly with row count.

---

## Alert: audit log verify fails on startup

**Confirm**: stderr says `audit log <path> rejected: <reason>`.

**Likely cause**: the audit log has been tampered with (any
edit / reorder / splice breaks the BLAKE3 hash chain).

**Do**:
1. **Do not proceed** — preserve the file for security review.
2. Start the server with `-` in the audit slot (audit disabled)
   on a fresh log file. New writes go into the new log; the old
   log is evidence.
3. Hand the corrupted file to your security team.

This is the only alert in this runbook where you may need to
involve a human investigator before resuming writes.

---

## What's NOT in this runbook

- Automated failover orchestration — manual promotion only;
  follow RESTORE_DRILL.md to promote a follower.
- TLS termination issues — TLS is out of SPG's scope; deploy
  behind nginx / stunnel / pgbouncer if needed.
- Memory pressure / OOM — there is no per-query memory cap yet
  (PROD_READY row 5.5). For now, bound query size with
  `SPG_MAX_QUERY_ROWS`.
