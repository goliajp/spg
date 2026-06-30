# 24h long-run verification — operator state visibility

> v7.37.22 (22.12) — checklist for verifying that every operator-visible
> state surface stays accurate across a multi-hour production run. Aimed
> at mailrs / sentori dogfood gates, but the surfaces here are
> workload-agnostic.

The goal is not "find a perf regression" (that's
[PERF_METHODOLOGY_VS_FOSS.md](./PERF_METHODOLOGY_VS_FOSS.md)). The goal
is "after 24 hours of real traffic, can an operator still see what
SPG is doing?" Each check has a runnable command and an explicit pass
condition.

Schedule with cron / systemd timers at hours 1, 6, 12, 24. Any check
that flips from PASS to FAIL between hours is a release gate.

## Catalog surfaces

| # | Surface | Check command | Pass condition |
|---|---------|---------------|----------------|
| C1 | `pg_stat_statements` | `psql -c "SELECT count(*), max(calls), max(total_exec_time) FROM pg_catalog.pg_stat_statements"` | count > 0, growing across snapshots; max(calls) monotonic |
| C2 | `pg_stat_database` | `psql -c "SELECT * FROM pg_catalog.pg_stat_database"` | 1 row; numbackends ≥ 1; counters stable / growing |
| C3 | `pg_stat_user_tables` | `psql -c "SELECT relname, seq_scan, idx_scan, n_live_tup FROM pg_catalog.pg_stat_user_tables ORDER BY n_live_tup DESC LIMIT 20"` | n_live_tup matches `SELECT count(*) FROM <table>` within 1% (autoanalyze drift tolerance) |
| C4 | `pg_stat_user_indexes` | `psql -c "SELECT count(*) FROM pg_catalog.pg_stat_user_indexes"` | count matches `SELECT count(*) FROM pg_catalog.pg_index` |
| C5 | `pg_stat_archiver` | `psql -c "SELECT * FROM pg_catalog.pg_stat_archiver"` | 1 row, archived_count growing iff WAL pubsub active |
| C6 | `pg_stat_bgwriter` | `psql -c "SELECT * FROM pg_catalog.pg_stat_bgwriter"` | 1 row, stable shape |
| C7 | `pg_stat_replication` | `psql -c "SELECT count(*) FROM pg_catalog.pg_stat_replication"` | count == number of active follow-of subscribers |
| C8 | `pg_stat_progress_vacuum` / `_create_index` / `_analyze` | `psql -c "SELECT count(*) FROM pg_catalog.pg_stat_progress_vacuum"` etc. | 0 rows when idle; > 0 only while operation in flight |
| C9 | `pg_stat_io` | `psql -c "SELECT object, sum(reads + writes) FROM pg_catalog.pg_stat_io GROUP BY object"` | rows present; sums grow with workload |
| C10 | `pg_stat_user_functions` | `psql -c "SELECT count(*) FROM pg_catalog.pg_stat_user_functions"` | matches `pg_proc` row count |

## Activity surfaces

| # | Surface | Check command | Pass condition |
|---|---------|---------------|----------------|
| A1 | `spg_stat_activity` | `psql -c "SELECT pid, application_name, state, query FROM spg_stat_activity"` | every live conn has a row; query column reflects last SQL |
| A2 | `pg_locks` | `psql -c "SELECT * FROM pg_locks"` | row per held lock; cleared on tx end |
| A3 | Wait events | `psql -c "SELECT pid, wait_event_type, wait_event FROM spg_stat_activity WHERE wait_event IS NOT NULL"` | wait_event populated for blocked tx |
| A4 | `spg top` | `spgctl top --limit 10 --once` | top template's calls / total_exec_time match `pg_stat_statements` |

## Replication surfaces (when SPG_FOLLOW_OF / SPG_PUBSUB_TARGET active)

| # | Surface | Check command | Pass condition |
|---|---------|---------------|----------------|
| R1 | `pg_publication` | `psql -c "SELECT * FROM pg_catalog.pg_publication"` | 1 row per CREATE PUBLICATION |
| R2 | `pg_subscription` | `psql -c "SELECT * FROM pg_catalog.pg_subscription"` | 1 row per CREATE SUBSCRIPTION; `subconninfo` always `[redacted]` |
| R3 | `pg_replication_slots` | `psql -c "SELECT * FROM pg_catalog.pg_replication_slots"` | empty until 21.12 persistent slot state lands |

## Storage surfaces

| # | Surface | Check command | Pass condition |
|---|---------|---------------|----------------|
| S1 | hot-tier size | `psql -c "SELECT spg_table_size(table_name) FROM information_schema.tables WHERE table_schema='public'"` | grows ≤ workload's insert rate × row size |
| S2 | WAL throughput | `tail -F <data_dir>/wal.log | wc -l` (sample 60s) | rate matches commit rate from `xact_commit` |
| S3 | quarantine dir | `ls <data_dir>/quarantine/` | empty (any presence = crash recovery left rubble; see WAL-QUARANTINE-RECOVERY.md) |

## Audit surfaces

| # | Surface | Check command | Pass condition |
|---|---------|---------------|----------------|
| AU1 | audit chain | `spgctl verify-pitr --dir <backup_dir>` | exit 0 (LSN sequence + checksums clean) |
| AU2 | row counts vs WAL replay | `spgctl wal-lint --wal <data_dir>/wal --db <snapshot>` | exit 0; row count after replay == row count from `SELECT count(*)` |

## What "FAIL" actually triggers

- **Any check goes from PASS to FAIL between hour-snapshots** → halt
  deploy, file finding under `.claude/notes/long-run-finding-<date>.md`,
  cite the failing surface + the hour boundary.
- **Surface present but counter value frozen** when traffic is live →
  per-counter wiring bug; usually means the increment site never fired.
  Trace via `pg_stat_statements` (which template was supposed to bump
  it) and `pg_stat_user_tables` (which relation).
- **Surface returns empty when it shouldn't** → catalog dispatch table
  miss in `crates/spg-engine/src/select.rs`; check the `__spg_pg_*`
  arm exists.

## What "PASS" earns

A 24-hour PASS across every row above is the gate every release must
clear before tagging. The point isn't that every counter is at the
right number — it's that every counter is *moving in the right
direction* and every surface is *reachable*. A frozen counter is the
single failure mode that hides the longest, so this checklist is
biased toward "is it incrementing" not "is the number right".

## Automation

Run as a cron:

```cron
0 * * * * spgctl query 'SELECT count(*), max(calls), max(total_exec_time) FROM pg_catalog.pg_stat_statements' >> /var/log/spg/long-run-c1.log
```

Diff the last 24 lines hour-over-hour; any row where counters decrease
or freeze (delta == 0 with workload active) → page.
