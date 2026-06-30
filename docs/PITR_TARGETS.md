# PITR target surface — recovery.conf equivalents

> v7.37.21 (21.5-21.10) — maps PG's `recovery.conf` / `postgresql.auto.conf`
> recovery target parameters to SPG's `spgctl pitr-restore --to` surface.

PG configures point-in-time recovery via six parameters; SPG
consolidates them under the single `--to <target>` flag plus a couple
of always-on semantics.

## Mapping

| PG parameter | SPG equivalent | Status |
|--------------|----------------|--------|
| `recovery_target_lsn = '<lsn>'` | `--to <unsigned int>` | shipped (v6.10.7) |
| `recovery_target_time = '<timestamp>'` | `--to '<YYYY-MM-DD HH:MM:SS>'` UTC, or `--to <unix>s\|ms\|us` | shipped (v6.10.7) |
| `recovery_target_xid = '<xid>'` | N/A — SPG WAL records are SQL (v1/v2) or row-redo (v3); no transaction xid in record headers | N/A by storage model |
| `recovery_target_name = '<label>'` | N/A — SPG has no `pg_create_restore_point()` label sites in WAL | N/A by storage model |
| `recovery_target = 'immediate'` | `--to 0` (LSN 0 = before any record) | shipped — semantics equivalent: every record boundary in SPG WAL is consistent, so "immediate" = 0 records replay |
| `recovery_target_action = 'pause'` | N/A — `spgctl pitr-restore` writes the snapshot and exits | only `shutdown` semantics |
| `recovery_target_action = 'promote'` | N/A — same; SPG has no follower/primary mode toggle. Promote is a service-level switch outside the recovery process | N/A by deployment model |
| `recovery_target_action = 'shutdown'` | default and only behavior | shipped |
| `recovery_target_inclusive = on\|off` | always `on` (≤ semantics in `RestoreTarget::includes`) | shipped |

## Why fewer knobs

PG's six-parameter surface evolved with its 25-year recovery model
(physical WAL + standby + promote + pause-for-inspection). SPG's
recovery model is simpler:

- **Recovery process is a CLI invocation, not a server mode.**
  `spgctl pitr-restore --backup-dir <d> --to <T> --out <db.spg>` reads
  WAL chunks from `<d>/wal/`, replays through `<T>`, writes a fresh
  snapshot, exits. No `pause` because there's nothing running to
  pause; no `promote` because there's no primary/standby split.

- **WAL records have no transaction xid header.** SPG's row-redo path
  (v3 records, 0x12 / 0x13) writes the post-image directly; v1/v2
  records are full SQL statements that replay atomically. xid-level
  stop semantics would require record-format change with no
  customer-visible benefit beyond LSN-level stop.

- **No named restore points.** PG's `pg_create_restore_point('label')`
  inserts a WAL record carrying the label so `recovery_target_name`
  can find it. SPG could synthesize the same shape (a record carrying
  a label string) if a customer demand materializes; until then it's
  surface area without a use case.

## What this means for migration

`pg_dumpall` + `pg_basebackup`-style PITR scripts written for PG fall
in two buckets:

1. **LSN- or time-based stop** — direct rewrite to
   `spgctl pitr-restore --to`. The shell wrapper that resolved
   recovery.conf into `recovery_target_time = ...` becomes a
   one-line `--to "..."`.

2. **xid- or name-based stop** — needs operator review. Either map
   to an equivalent timestamp (commonly the case — the xid was just
   "right before that bug shipped") or stay on PG until SPG's WAL
   format grows the surface (no roadmap commitment).

## Always-on semantics SPG bakes in

- **Inclusive bound only.** PG's `recovery_target_inclusive = off`
  for "stop *just before* the target" is N/A; SPG replays up-to-and-
  including the target record. Workaround for "exclusive" semantics:
  set the target one LSN earlier.

- **Consistent at every record boundary.** PG can stop at a point
  where a multi-record transaction is half-applied (the
  `consistent_state` machinery). SPG's record-per-statement (v1/v2)
  and post-image redo (v3) models mean every successful record
  application leaves the catalog in a consistent state — no
  half-applied transaction window.

## See also

- [WAL-QUARANTINE-RECOVERY.md](./WAL-QUARANTINE-RECOVERY.md) — what
  to do when a WAL chunk fails verification before the PITR replay
  even starts.
- `spgctl verify-pitr --dir <d>` — pre-flight integrity check on a
  backup directory (LSN sequence + checksums + dry-run replay).
- `spgctl backup-pitr` — produce the backup layout that
  `spgctl pitr-restore` consumes.
