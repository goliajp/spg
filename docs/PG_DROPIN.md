# PG drop-in support — public commitments + how to verify

SPG is a single-database, drop-in replacement for PostgreSQL for
applications whose schema/code lives inside SPG's PG-dialect
coverage. This page is the **public-facing commitment** —
`PG_MIGRATION.md` is the full matrix; this page is the
60-second yes/no and the harness you run yourself.

## TL;DR

| Question | Answer |
|---|---|
| Is my PG app drop-in? | Run `scripts/dropin-acceptance.sh`. ✅/❌ in one minute. |
| What's the floor we promise? | The `scripts/fixtures/` files. Every published SPG image runs them green. |
| How do I make sure my app stays drop-in across SPG upgrades? | Add your schema to `scripts/fixtures/` and PR it — your file becomes a permanent regression fixture in SPG CI. |

## The one-line swap

### Network mode (any PG client — Rust / Java / Python / Go / Node / psql)

```sh
# Before:  postgres://user:pw@db-host:5432/myapp
# After:   point at goliakk/spg:7.17.0; same URL shape, same SCRAM-SHA-256
#          auth, same wire protocol.
docker run -d -p 5432:5432 -e SPG_ADMIN_PASSWORD=pw goliakk/spg:7.17.0
```

### In-process mode (Rust + sqlx — `spg-sqlx`)

```rust
use spg_sqlx::{SpgConnectOptions, SpgPool, SpgPoolOptions};

let pool: SpgPool = SpgPoolOptions::new()
    .max_connections(20)
    .connect_with(SpgConnectOptions::file("./spg.db".into()))
    .await?;
// Stock sqlx code (query / query_as / pool.begin / RETURNING /
// query!() compile-time validation) runs unchanged.
```

See [`README.md`](../README.md#sqlx-embed-in-process-no-daemon) for the full quick start and
[`PG_MIGRATION.md`](../PG_MIGRATION.md#concurrency-model--spg-sqlx-pool-semantics-v718)
for the v7.18 concurrency model (Pool scales reads, writes
serialise, transactions stay on the writer — PG read-committed
default).

## What "drop-in" means here

Drop-in = **your unmodified PG schema + your unmodified
application + your unmodified test suite, on SPG, all green**.
Not "you can rewrite to be SPG-compatible" — no rewrite.

Concretely, every PG feature in this matrix must continue to
work across SPG releases, and SPG CI gates a regression here as
release-blocking:

- Type system: every column type in [`PG_MIGRATION.md`](../PG_MIGRATION.md#data-types)
  marked ✅ — `BIGINT` / `BIGSERIAL` / `NUMERIC(p,s)` / `TEXT` /
  `BOOLEAN` / `TIMESTAMPTZ` / `JSON` / `JSONB` / `BYTEA` /
  `TEXT[]` / `INT[]` / `BIGINT[]` / `tsvector` / `tsquery` /
  `VECTOR(N)` (incl. `USING SQ8` / `USING HALF`) / `UUID` /
  `DATE` / `INTERVAL`.
- DDL: `CREATE TABLE` with FKs (4 referential actions), CHECK
  constraints, `BIGSERIAL` inline PK, multi-column indexes,
  `USING ivfflat` / `USING hnsw (… vector_cosine_ops)`,
  `CREATE EXTENSION` accepted as no-op, `CREATE TYPE … AS ENUM`,
  `CREATE DOMAIN`, `CREATE VIEW` / `CREATE MATERIALIZED VIEW`.
- DML: multi-row `INSERT ... VALUES (...), (...)`, `INSERT ...
  ON CONFLICT (cols) DO { NOTHING | UPDATE SET ... }`,
  `INSERT/UPDATE/DELETE ... RETURNING ...`.
- Queries: full `SELECT` shape — JOINs (incl. `LATERAL` /
  cross / full-outer-rewritten), CTEs incl. `WITH RECURSIVE`,
  window functions, correlated subqueries, `GROUP BY ALL`,
  `DISTINCT [ON]`, `UNION` / `UNION ALL`,
  `EXISTS` / `IN (subq)`.
- Transactions: `BEGIN` / `COMMIT` / `ROLLBACK` / `SAVEPOINT`,
  `RELEASE SAVEPOINT`, `ROLLBACK TO SAVEPOINT`. PG
  read-committed isolation by default.
- Full-text search: `to_tsvector`, the four query
  constructors (`plainto_tsquery`, `to_tsquery`,
  `phraseto_tsquery`, `websearch_to_tsquery`), `@@`, `ts_rank`,
  `ts_rank_cd`, real GIN inverted index.
- PL/pgSQL: `CREATE FUNCTION ... LANGUAGE plpgsql AS $$ ... $$`
  + `CREATE TRIGGER ... FOR EACH ROW EXECUTE FUNCTION fn()`.

## Backup + PITR

Drop-in PG users expect WAL-based PITR. SPG v7.18 ships the four
subcommands matching that expectation: `spg backup-pitr`,
`spg verify-pitr`, `spg pitr-restore`, `spg prune-pitr`. Default SLA:

- **RPO ≤ 1s** — every commit fsyncs to the WAL before returning
- **RTO ≤ 10min** — replay = snapshot read + WAL apply
- **Retention 24h** — `SPG_PITR_RETENTION_HOURS=24` default

External archival rides on `SPG_PITR_ARCHIVE_CMD` (same loud-failure
semantics PG's `archive_command` has). Full operator playbook in
[`PG_MIGRATION.md`](../PG_MIGRATION.md#backup--pitr-v718).

## Verify it yourself — `scripts/dropin-acceptance.sh`

```sh
# Default — the 35-case PG dialect panel against the latest
# published SPG image.
scripts/dropin-acceptance.sh

# With your own schema or your team's init-schema.sql added on
# top — your file passes = SPG drops in for your app.
scripts/dropin-acceptance.sh \
    --fixture path/to/your-pg-extensions.sql \
    --fixture path/to/your-init-schema.sql
```

Output: markdown report (default `./dropin-acceptance-report.md`)
with per-case pass/fail + first ERROR line on any failure.
Exit code: 0 all pass / 1 any fail / 2 harness error — wire it
straight into your CI.

## Current fixtures (regressions are release-blocking)

| Fixture | Source | What it proves |
|---|---|---|
| [`scripts/fixtures/mailrs-init-schema-v1.7.142.sql`](../scripts/fixtures/mailrs-init-schema-v1.7.142.sql) | mailrs `develop @ v1.7.142` | mailrs's live production schema (~260 lines, post-D-pre cleanup) loads cleanly on SPG. |
| [`scripts/fixtures/mailrs-pg-extensions.sql`](../scripts/fixtures/mailrs-pg-extensions.sql) | mailrs `develop @ v1.7.142` | `CREATE EXTENSION vector` accepted as no-op (SPG ships VECTOR builtin). |

When a real customer (you?) brings their PG schema, drop the
file into `scripts/fixtures/` and PR it — it becomes a
permanent regression target in SPG CI.

## Carve-outs — what's NOT in the drop-in promise

These are intentional out-of-scope and documented in
[`PG_MIGRATION.md`](../PG_MIGRATION.md) § A7. If your app depends on any
of them, ping the issue tracker and we'll discuss scope:

- Multi-master / quorum replication
- Multi-writer MVCC
- `INTERSECT` / `EXCEPT`
- Server-side cursor / partial `Execute`
- Multi-dimensional arrays beyond the v7.17 `INT[][]` /
  `BIGINT[][]` / `TEXT[][]` types
- Row-Level Security
- Full `pg_catalog.*` introspection (subset works for
  Rails/ActiveRecord/sqlx; full coverage isn't promised)

If a feature isn't in the drop-in promise AND isn't in the
carve-out list, that's a real gap — file it.

## How regressions are caught

SPG repo's `.github/workflows/ci.yml` runs
`dropin_acceptance` on every PR + push to develop / master /
release / hotfix branches. The job uploads the harness's
markdown report as a CI artifact for inspection.

Regression policy: any case that flipped from ✅ to ❌
between releases is a release blocker (the gate stops master /
release tagging until the dialect work to restore the case
lands).
