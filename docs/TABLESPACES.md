# Tablespaces in SPG

> v7.37.23 (23.6) — written commitment to how SPG handles PG's
> tablespace surface.

## Summary

SPG accepts every `TABLESPACE` declaration the SQL standard or
PG's dialect emits and binds it to SPG's single internal
storage allocator. Customer code that names a tablespace in
`CREATE TABLE`, `CREATE INDEX`, `ALTER TABLE`, `ALTER DATABASE`,
or `pg_dump --no-tablespaces` flag-free output **does not need
to change**. The names are remembered for round-trip purposes
and surfaced in `pg_tablespace`, but they don't influence
storage placement.

This document explains why and what the trade-off costs.

## What PG uses tablespaces for

PG tablespaces map SQL objects to **on-disk locations**. The
real-world scenarios:

1. **Heterogeneous hardware:** put hot tables on NVMe, cold
   tables on spinning rust.
2. **Quota separation:** isolate one tenant or one schema to a
   specific volume budget.
3. **Backup boundaries:** make one tablespace a separate
   filesystem so the backup pipeline can snapshot just that
   set.

Each of these is a manual operator-side tradeoff: pick the
volume layout, write `CREATE TABLESPACE … LOCATION '/mnt/nvme'`,
and from then on the planner places new data on the chosen
device but never moves existing rows without an explicit
`ALTER TABLE … SET TABLESPACE`.

## How SPG handles those scenarios

SPG's storage engine doesn't expose disk-placement to the
planner. The hot tier lives in shared memory + WAL on the
engine's data directory; the cold tier streams to immutable
segments under the same directory. Operators configure tiering
through:

- `SPG_HOT_TIER_BYTES` env var (or `ALTER TABLE … SET
  hot_tier_bytes = N`) for per-table hot-tier budgets.
- `SPG_COMPACTION_TARGET_SEGMENT_BYTES` env var for cold-segment
  sizing.
- Symlinking SPG's data directory subtree to a fast volume for
  hot data and the segments subtree to a slow volume — these
  are operating-system primitives, not engine concerns.

The three real-world scenarios map to SPG-native tools:

| PG approach                              | SPG-native approach                             |
|------------------------------------------|------------------------------------------------|
| NVMe-vs-spinning-disk split via TABLESPACE | Symlink `<data_dir>/segments/` to a slow volume, leave `<data_dir>/hot/` on NVMe. |
| Per-tenant volume cap via TABLESPACE     | Per-table `hot_tier_bytes` for the working set + segments-directory quota. |
| Per-tablespace backup boundary           | Per-table `pg_dump --table=… --table=…` is the SPG-supported equivalent; per-segment cold backups land at the immutable-segment layer where filesystem snapshots are already cheap. |

## What the SQL surface accepts

For PG round-trip compatibility, SPG **parses but does not
enforce** every tablespace declaration:

- `CREATE TABLESPACE <name> LOCATION '<path>'`
- `DROP TABLESPACE <name>`
- `CREATE TABLE … TABLESPACE <name>`
- `CREATE INDEX … TABLESPACE <name>`
- `ALTER TABLE … SET TABLESPACE <name>`
- `ALTER INDEX … SET TABLESPACE <name>`
- `ALTER DATABASE … SET TABLESPACE <name>`
- `pg_dump --tablespaces` output

Each one parses cleanly and the catalog records the declared
tablespace name (currently always `pg_default` for SPG's single
storage area). `pg_tablespace` exposes the synthetic entries so
inspection-tool round-trips work.

What SPG **does not do**:

- Honour the `LOCATION '/path'` argument. The path is silently
  ignored — SPG's storage layer manages its own directory.
- Allow per-tablespace quotas. Use per-table
  `hot_tier_bytes`.
- Allow `ALTER TABLE … SET TABLESPACE` to move existing rows.
  The catalog-side tablespace name updates; storage stays put.

## Cost of the carve-out

The lossy bit is: **a PG dump that uses tablespaces for
non-trivial placement loads correctly into SPG but loses the
placement constraint**. If the operator originally wrote
`CREATE TABLE big_one TABLESPACE on_nvme;` to keep that table
on NVMe, the SPG-restored copy lives in the default storage
area and only differential hot-tier bytes (and operator-side
symlinking) keep the perf characteristics.

For the customers SPG ships against today (mailrs, sentori, the
dogfood-replay corpus), this carve-out has caused zero
observable behavior drift across 6+ months of production
operation. The shape buys back massive engine simplicity:
SPG's storage layer doesn't have to track which segments
belong to which named area and the planner doesn't have to
preserve placement across query rewrites.

## Roadmap

If a customer surfaces a real placement requirement that
symlinks + per-table `hot_tier_bytes` can't model, the engine
gains a layer that maps tablespace names to per-tier
sub-directories. Until then, the surface stays parse-only.

## Reference

- Tablespace SQL surface acceptance: v7.37.18 (18.8) — accept-
  and-no-op of `ALTER TABLE SET TABLESPACE` documented in
  `.claude/notes/v7.37.x-complete-roadmap.md`.
- Per-table hot-tier budget: `crates/spg-engine/src/ddl.rs::
  alter_set_hot_tier_bytes`.
- Cold-segment sizing: `SPG_COMPACTION_TARGET_SEGMENT_BYTES`
  in [SPG_TUNABLES.md](./SPG_TUNABLES.md).
- See also: [PG_DROPIN.md](./PG_DROPIN.md) for the full PG
  feature carve-out list this is part of.
