# spg-server

Single-writer SQL daemon. PG-wire compatible (psql, libpq, pgx,
JDBC, ODBC clients connect as-is) plus a self-built tight wire
protocol for SPG-native clients. Zero external dependencies
(no `tokio`, no `serde`, no proc-macros).

```bash
docker run -p 5432:5432 -v spg-data:/data goliakk/spg:7.7.0
psql -h 127.0.0.1 -U bench bench
```

## Why

- **PG-wire compatible.** Your existing PostgreSQL clients
  connect without code changes. Auth is SCRAM-SHA-256;
  prepared statements, extended-query protocol, COPY FROM /
  COPY TO all work. `pg_dump`s import via `psql -f`.
- **Single-writer architecture.** No MVCC, no isolation-level
  matrix, no deadlocks. Writes serialize through one path,
  reads share a read lock. Group commit + WAL `fsync` per
  batch instead of per statement.
- **Built-in vector search.** `VECTOR(N)` with HNSW indexes
  and pgvector ops (`<->`, `<#>`, `<=>`). No separate
  extension to install.
- **Cold tier on disk, hot tier in RAM.** Background freezer
  moves cold rows into compressed segments;
  reads transparently span both. v7.7+ auto-compacts so the
  segment population stays bounded.
- **Foreign keys (v7.6+).** Full SQL `FOREIGN KEY … ON
  DELETE/UPDATE …` with CASCADE / RESTRICT / SET NULL /
  SET DEFAULT / NO ACTION. Multi-column, self-referencing,
  ALTER TABLE ADD / DROP CONSTRAINT.
- **4-corpus regression at 100%.** `pg_regress` 144/144,
  `pgvector` 63/63, `mysql` 100%, `duckdb` 100%.

## Quick start (Docker)

```bash
docker run -d --name spg \
    -p 5432:5432 \
    -v spg-data:/data \
    -e SPG_PG_ADDR=0.0.0.0:5432 \
    goliakk/spg:7.7.0
```

Connect with any PG client:

```bash
psql -h 127.0.0.1 -p 5432 -U bench bench
# default credentials: bench / bench
```

The image runs as a non-root distroless container. Data lives
under `/data` so the named volume persists across container
restarts.

## Configuration

All config goes through environment variables.

| Variable                     | Default                     | Purpose |
|------------------------------|-----------------------------|---------|
| `SPG_PG_ADDR`                | (disabled)                  | Bind PG-wire listener (e.g. `0.0.0.0:5432`). |
| `SPG_HOT_TIER_BYTES`         | `4_294_967_296` (4 GiB)     | Hot-tier byte budget before freezer fires. |
| `SPG_COLD_COMPACT_SEGMENT_THRESHOLD` | `64`                | Compact when cold segments exceed this count. |
| `SPG_QUERY_TIMEOUT_MS`       | (no timeout)                | Per-query watchdog. |
| `SPG_WAL_TEE`                | (disabled)                  | Second WAL destination for audit / debug. |
| `SPG_PUBSUB_TARGET`          | (disabled)                  | `log` writes successful SQL to stderr. |
| `SPG_REPLICA_LISTEN`         | (disabled)                  | Bind replica-stream listener for read replicas. |
| `SPG_FOLLOWER_OF`            | (disabled)                  | Follow this leader's replica-stream URL. |

CLI args:

```
spg-server [<wire-addr> [<db-path> [<audit-path> [<wal-path>]]]]
spg-server --replay-only <wal>     # rebuild catalog, no listen
```

Default positional: `0.0.0.0:5544 ./spg.db - ./wal.log` —
listens on 5544 for the native wire protocol; PG-wire only
when `SPG_PG_ADDR` is set.

## SQL surface

Every SQL feature SPG accepts works identically on `spg-server`
and `spg-embedded` — they share `spg-engine`. Server-only
adds:

- **Authentication**: SCRAM-SHA-256 over PG-wire.
  `CREATE USER` / `DROP USER`, role-based access (ReadOnly,
  ReadWrite, Admin).
- **Concurrency**: many connections, one writer; reads
  parallelize. Per-connection prepared-statement cache.
- **Replication**: logical replication via the
  publication / subscription pair. Cold-segment forwarding
  on top of WAL streaming so a follower catches up without
  full-snapshot transfer.
- **COPY FROM / COPY TO**: pgwire-compatible bulk load /
  export. Used by `pg_dump | psql` workflows.

For the full SQL feature matrix (DDL / DML / SELECT /
vectors / transactions / auth / introspection) see
[`PG_MIGRATION.md`](../../PG_MIGRATION.md).

## Wire protocols

Two listeners, configurable independently.

### PG-wire (`SPG_PG_ADDR`)

PostgreSQL frontend protocol v3. Compatible with:

- **psql** — `psql -h host -p 5432 -U bench bench`
- **libpq** (C API) — direct linkage
- **pgx** (Go) — `pgx.Connect(ctx, "postgres://bench@host/bench")`
- **JDBC** (Java) — `jdbc:postgresql://host:5432/bench`
- **Python psycopg2 / psycopg3** — `dsn="host=host user=bench dbname=bench"`
- **Rust `tokio-postgres` / `postgres`** — works as-is
- **Rails ActiveRecord (`pg` gem)** — works (with the
  `pg_catalog` caveats listed in `PG_MIGRATION.md`)
- **ODBC** — via the PostgreSQL ODBC driver

Auth: SCRAM-SHA-256 only (no `trust`, no `md5` legacy hash).
Set passwords via `CREATE USER … WITH PASSWORD 'x'`.

### SPG-native wire (`<wire-addr>` CLI arg)

Tight, length-prefixed frame protocol — saves the per-row
text-encoding cost on big SELECTs (~10–20% faster than PG-wire
on row-heavy workloads). Used by the `spg` CLI and any
in-process Rust caller that prefers `spg-wire` over PG.

## Operations

### Health check

```bash
docker exec spg /usr/local/bin/spg ping 127.0.0.1:5544
```

### Backup

```bash
docker exec spg /usr/local/bin/spg backup --addr 127.0.0.1:5544 > /backup/spg-$(date +%F).db
```

The output is a complete catalog snapshot (same byte format
that `Database::open_path` consumes on boot). Restore with:

```bash
docker cp /backup/spg-2026-06-03.db spg:/data/spg.db
docker restart spg
```

### Replication

Leader:

```bash
docker run -d --name spg-leader \
    -p 5432:5432 -p 5544:5544 -p 5500:5500 \
    -v spg-leader-data:/data \
    -e SPG_PG_ADDR=0.0.0.0:5432 \
    -e SPG_REPLICA_LISTEN=0.0.0.0:5500 \
    goliakk/spg:7.7.0
```

Follower:

```bash
docker run -d --name spg-follower \
    -p 25432:5432 \
    -v spg-follower-data:/data \
    -e SPG_PG_ADDR=0.0.0.0:5432 \
    -e SPG_FOLLOWER_OF=spg://spg-leader:5500 \
    goliakk/spg:7.7.0
```

The follower bootstraps from the leader's current catalog
snapshot, then streams WAL records + cold-segment files. Read
queries served from the follower see eventually-consistent
state.

### Audit

`CREATE TABLE` / DDL mutations are logged to the audit file
(positional CLI arg 3, default `-` = disabled). Each entry
chained by BLAKE3 — `spg audit verify <audit-path>` checks
the chain.

### Metrics

The server exposes a minimal text-format scrape endpoint at
`/spg-metrics` (path is hard-coded; configure the listen
addr via `SPG_METRICS_ADDR`). Output includes:

- `spg_hot_rows`           — live rows across all user tables
- `spg_hot_bytes`          — bytes against `SPG_HOT_TIER_BYTES`
- `spg_cold_segments`      — current segment count
- `spg_wal_bytes`          — current WAL size
- `spg_queries_total`      — counter of SELECT / DML statements

For deeper observability, embed `spg-embedded` + call
`Database::metrics()` from your own app code.

## Migrating from PostgreSQL

See [`PG_MIGRATION.md`](../../PG_MIGRATION.md) for the full
decision tree and SQL compatibility matrix. Short version:

- **DDL** — `CREATE TABLE` / `INDEX` / `VIEW` (limited) /
  `FOREIGN KEY` work. `CREATE TRIGGER`, stored procedures,
  `CREATE TYPE`, RLS — won't be implemented (axiom A7).
- **Migration pipeline**:
  ```bash
  pg_dump --data-only --column-inserts mydb > data.sql
  pg_dump --schema-only mydb | sed 's/SERIAL/INT NOT NULL AUTO_INCREMENT/' > schema.sql
  psql -h spg-host -p 5432 -U bench bench -f schema.sql
  psql -h spg-host -p 5432 -U bench bench -f data.sql
  ```
- **Validate before committing** — pull the image, point your
  ORM at it, run your test suite. 5 minutes gets you a
  yes/no/needs-work answer for your app.

## Companion crates

- [`spg-embedded`](https://crates.io/crates/spg-embedded) —
  in-process Rust API. Same engine, no listener. For
  single-binary services + edge / desktop apps.
- [`spg-cli`](https://crates.io/crates/spg-cli) — `spg`
  command-line client. `query`, `backup`, `wal-lint`,
  `revert`, `ping`.

The on-disk format (`SPGDB001` catalog v13, WAL v3,
`SPGMAN01` v10 manifest, segment v2 envelope) is identical
across all three crates. A database produced by `spg-embedded`
boots cleanly on `spg-server` and vice versa.

## License

MIT OR Apache-2.0
