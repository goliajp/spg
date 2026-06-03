# PG → SPG migration guide (v7.3+)

This document is a frank assessment of what migrates cleanly,
what needs application-level rewrite, and what will never
land in SPG. It is structured so you can answer "should we
move?" without reading the rest of the documentation set.

The compatibility numbers below are mechanically derived from
`xtests/sqllogictest/corpus/pg_regress/` (144/144 passing
PG-borrowed cases) and `pgvector/` (63/63 passing pgvector
cases). Hand-marked items cite the design doc that froze the
decision.

---

## TL;DR

**SPG ships two deployment modes; pick first, then check SQL
compatibility second.**

| You want | Use | PG client compatibility |
|---|---|---|
| Replace a PG server with another network DB | `spg-server` + Docker | ✅ libpq / psql / pgx / JDBC / ODBC via PG-wire |
| Replace SQLite with a Rust-native embedded DB | `spg-embedded` crate | ❌ no network — Rust API only |
| Get vector / kNN without pgvector + PG | either | ✅ pgvector-flavoured (`<->`, `<#>`, `<=>`) |
| Multi-writer OLTP / FKs / triggers / stored procs | **Stay on PG** | n/a — these are explicit non-goals (axiom A7) |

The rest of this doc backs each row with specifics.

---

## Decision tree

```
                ┌──────────────────────────────────────────────┐
                │  Does your app rely on any of:               │
                │   - Foreign keys / REFERENCES                │
                │   - Triggers                                 │
                │   - Stored procedures (PL/pgSQL etc)         │
                │   - Row-Level Security                       │
                │   - Multiple concurrent writers              │
                │   - Multi-master replication                 │
                └────────────────────┬─────────────────────────┘
                                     │
                       ┌─── yes ─────┴─────  no ────┐
                       ▼                            ▼
              Stay on PostgreSQL.            ┌──────────────────────────────┐
              SPG won't be a fit;            │  Embedded in your binary, or │
              these are A7-axiom             │  separate service?           │
              "won't do" items.              └──────┬────────────┬──────────┘
                                                    │            │
                                            embedded            service
                                                    ▼            ▼
                                          spg-embedded     spg-server
                                          (Rust API)       (PG-wire on 5432)
                                                                 ▼
                                                       any libpq client works
                                                       (psql / pgx / JDBC / …)
```

---

## Two deployment modes (side-by-side)

| | `spg-server` | `spg-embedded` |
|---|---|---|
| **Where it runs** | Daemon process (Docker / systemd / bare binary) | Inside your Rust binary |
| **Network protocols** | SPG native wire on 5544, PG-wire on `SPG_PG_ADDR` (commonly 5432) | None — Rust function calls only |
| **Concurrent clients** | Many (single-writer engine, multi-reader RwLock) | Single process, `Arc<Mutex<Database>>` to share threads |
| **Persistence** | Auto: WAL + manifest + cold-tier files | Auto via `Database::open_path(p)` — same on-disk layout |
| **Crash recovery** | WAL replay on boot | WAL replay on `open_path` |
| **Replication** | Logical (`CREATE PUBLICATION` / `CREATE SUBSCRIPTION`) + binary (MAGIC_V2) | n/a — out of scope |
| **PG-wire (postgres:// URL)** | ✅ supported | ❌ never (in-process API) |
| **Vector / HNSW** | ✅ | ✅ |
| **Image** | `goliakk/spg:7.3.0` (multi-arch) | Cargo crate `spg-embedded = "7.3.0"` |
| **Sample boot** | `docker compose up -d` | `Database::open_path("./spg.db")?` |

---

## Connecting from existing PG clients (server mode only)

After `docker compose up -d` against the v7.3 image,
`SPG_PG_ADDR=0.0.0.0:5432` is on by default. Any libpq client
connects with the standard URL:

```sh
psql 'postgres://spg@127.0.0.1:5433/spg' -c 'SELECT 1'
# (host port 5433 in compose to dodge a local PG on 5432;
# in-container listener is still on 5432.)
```

**Confirmed-working client surfaces** (each verified by an
e2e test or by the 4-corpus sqllogictest passing 100%):

| Client | Status | Notes |
|---|---|---|
| `psql` 14 – 18 | ✅ | Simple query path; `\d` / `\dt` partial (depends on `pg_catalog`) |
| `libpq` (C) | ✅ | Direct |
| `pgx` (Go, jackc/pgx v5) | ✅ | Tested via `e2e_pgwire_*.rs` extended query |
| JDBC (`org.postgresql:postgresql:42.x`) | ✅ | Simple + extended query |
| Python `psycopg2` / `psycopg[binary]` 3.x | ✅ | Per pgwire e2e |
| ODBC (`psqlodbc`) | ⚠️ | Driver works; some `INFORMATION_SCHEMA` views absent |
| Rails / ActiveRecord PG adapter | ⚠️ | Connects fine; schema introspection partial |

**Authentication**: SPG's PG-wire supports SCRAM-SHA-256 (the
modern PG default). `password = trust` (no auth) is the
v7.3 default when `SPG_PASSWORD` is unset. Cleartext password
auth is intentionally not wired — operators set
`SPG_ADMIN_PASSWORD` to enable SCRAM.

---

## SQL compatibility matrix (v7.3 ship-time)

✅ = works through the 4-corpus regression. ⚠️ = works in
common shapes; corner cases differ from PG. ❌ = not
implemented; further breakdown follows the table.

### Data definition (DDL)

| Feature | SPG | Notes |
|---|---|---|
| `CREATE TABLE` (int / text / bool / numeric / vector) | ✅ | |
| `CREATE TABLE … (a INT NOT NULL, b TEXT DEFAULT 'x')` | ✅ | DEFAULT evaluated once at DDL time |
| `AUTO_INCREMENT` (MySQL-flavoured) | ✅ | PG would use `SERIAL`; both syntaxes parse |
| `CREATE TABLE IF NOT EXISTS` | ✅ | |
| `CREATE INDEX … (col)` (B-tree) | ✅ | |
| `CREATE INDEX … USING hnsw (vec_col)` (pgvector) | ✅ | |
| `CREATE INDEX … USING brin (col)` | ✅ | Format ships; planner page-skipping is STABILITY carve-out |
| `CREATE INDEX … (a, b, c)` multi-column | ⚠️ | Single-column on the leading column today |
| `CREATE INDEX … INCLUDE (cols)` (v6.8.0) | ✅ | Stored on snapshot; index-only-scan in planner is carve-out |
| `CREATE INDEX … WHERE pred` (partial, v6.8.1) | ✅ | Predicate stored; planner uses it as a regular index |
| `CREATE INDEX … (lower(col))` (expression, v6.8.2) | ✅ | Same caveat as partial |
| `EXPLAIN (SUGGEST) …` (v6.8.3) | ✅ | PG would use `pg_qualstats`/`hypopg` — SPG is built-in |
| `ALTER TABLE t SET hot_tier_bytes = X` | ✅ | SPG-specific cold-tier knob |
| `ALTER TABLE t ADD COLUMN` | ✅ | |
| `ALTER TABLE t DROP COLUMN` | ✅ | |
| `ALTER TABLE t RENAME COLUMN` | ✅ | |
| `ALTER INDEX … REBUILD` | ✅ | NSW / BRIN rebuild lands; B-tree no-op |
| `CREATE SCHEMA foo; foo.bar` | ❌ | Single-namespace catalog; rename-to-prefix as workaround |
| `CREATE TYPE` / domains / composite types | ❌ | A7 — out of scope |
| Foreign keys (`REFERENCES`) | ❌ | A7 — won't do |
| `CHECK` constraints | ❌ | A7 — won't do |
| `CREATE TRIGGER` | ❌ | A7 — won't do |
| `CREATE FUNCTION` / `CREATE PROCEDURE` / PL/pgSQL | ❌ | A7 — won't do |
| Partition tables (`PARTITION BY`) | ❌ | Cold-tier covers time-series natively |
| Row-Level Security (`RLS`) | ❌ | A5 — process isolation instead |

### Data manipulation (DML)

| Feature | SPG | Notes |
|---|---|---|
| `INSERT INTO t VALUES (…), (…)` | ✅ | Multi-row INSERT |
| `INSERT INTO t (col, …) VALUES …` | ✅ | Column-list INSERT |
| `INSERT … RETURNING` | ⚠️ | Parses, returns CommandComplete; row return TBD |
| `UPDATE t SET … WHERE …` | ✅ | Single-statement |
| `UPDATE t SET … WHERE … RETURNING …` | ⚠️ | Same as above |
| `DELETE FROM t WHERE …` | ✅ | |
| `INSERT … ON CONFLICT …` (UPSERT) | ❌ | Application-level upsert via `SELECT` + branch |
| `TRUNCATE TABLE` | ❌ | Use `DELETE FROM t` |
| Bulk `COPY FROM STDIN` (PG-wire) | ❌ | Use multi-row INSERT instead |

### Queries (SELECT)

| Feature | SPG | Notes |
|---|---|---|
| `SELECT col, … FROM t WHERE …` | ✅ | |
| `WHERE col = literal` via index seek | ✅ | |
| `WHERE col BETWEEN x AND y` | ✅ | Range scan, no index acceleration yet for ranges |
| `IN (…)` / `NOT IN (…)` | ✅ | |
| `LIKE` / `ILIKE` / `NOT LIKE` | ✅ | Includes wildcards `%` `_` |
| `ORDER BY` (single & multi-column) | ✅ | |
| `GROUP BY` / `HAVING` | ✅ | |
| `LIMIT n` / `OFFSET n` | ✅ | |
| `DISTINCT` / `DISTINCT ON (cols)` | ✅ | |
| `INNER JOIN` / `LEFT JOIN` / cross-join | ✅ | |
| `RIGHT JOIN` / `FULL OUTER JOIN` | ❌ | Rewrite as `LEFT JOIN` |
| `UNION` / `UNION ALL` | ✅ | |
| `INTERSECT` / `EXCEPT` | ❌ | Application-level set ops |
| CTEs (`WITH name AS (…)`) | ✅ | |
| Recursive CTEs (`WITH RECURSIVE`) | ✅ | v4.22 ship |
| Window functions (`OVER (PARTITION BY …)`) | ✅ | Including `ROW_NUMBER`, `RANK`, `LAG`, `LEAD`, `SUM` etc |
| Correlated subqueries | ✅ | Memoised by Memoize node (v6.2.6) |
| `LATERAL` joins | ❌ | |
| `EXISTS` / `NOT EXISTS` | ✅ | |
| `EXPLAIN` / `EXPLAIN ANALYZE` | ✅ | |
| `EXPLAIN (SUGGEST) …` | ✅ | SPG-specific advisor |
| `SELECT … FROM t AS OF SEGMENT '<id>'` | ✅ | SPG-specific cold-tier time travel |
| `SELECT … AS OF TIMESTAMP <ts>` | ❌ | Per-segment timestamps not yet recorded |
| `CASE WHEN … THEN … ELSE … END` | ✅ | |
| `COALESCE` / `NULLIF` / `GREATEST` / `LEAST` | ✅ | |
| Aggregates: `count` / `sum` / `avg` / `min` / `max` / `count(DISTINCT)` | ✅ | |
| Aggregates: `string_agg` / `array_agg` | ⚠️ | `string_agg` ✅; `array_agg` returns text concat |

### Vector / kNN (pgvector-compatible)

| Feature | SPG | Notes |
|---|---|---|
| `VECTOR(N)` column type | ✅ | |
| `[1.0, 2.0, …]` literal | ✅ | |
| `<->` L2 distance operator | ✅ | |
| `<#>` inner-product operator | ✅ | Returns negative IP — same as pgvector |
| `<=>` cosine-distance operator | ✅ | |
| `CREATE INDEX … USING hnsw (v)` | ✅ | |
| `ORDER BY v <-> [literal] LIMIT k` (kNN) | ✅ | HNSW picked automatically when index present |
| `USING SQ8` 8-bit quantisation | ✅ | v6.0.1 |
| `USING HALF` (binary16) | ✅ | v6.0.3 (pgvector keyword) |
| `ivfflat` index | ❌ | HNSW only |
| `vector_dims()` / `vector_norm()` | ⚠️ | Some functions present; not the full pgvector function set |

### Transactions / sessions

| Feature | SPG | Notes |
|---|---|---|
| `BEGIN` / `COMMIT` / `ROLLBACK` | ✅ | |
| `SAVEPOINT name` / `ROLLBACK TO SAVEPOINT` / `RELEASE` | ✅ | |
| `BEGIN ISOLATION LEVEL …` | ⚠️ | Parses; SPG is single-writer so isolation is effectively SERIALIZABLE |
| `SET TRANSACTION READ ONLY` | ⚠️ | Parses; SPG enforces via the read-lock fast path |
| `SET` / `RESET` session variables | ⚠️ | Limited set (search_path, application_name) accepted; ignored if unknown |

### Authentication & users

| Feature | SPG | Notes |
|---|---|---|
| SCRAM-SHA-256 auth (PG-wire) | ✅ | v4.1 |
| `CREATE USER`/`DROP USER` | ✅ | |
| Roles (`admin` / `readwrite` / `readonly`) | ✅ | Three built-in roles, not arbitrary `CREATE ROLE` |
| `GRANT` / `REVOKE` on tables | ❌ | Role-level grants only |
| `pg_hba.conf` style auth rules | ❌ | Single password / role per session |

### Replication

| PG feature | SPG | Notes |
|---|---|---|
| Streaming replication (`pg_basebackup` + standby) | ⚠️ | Different protocol: MAGIC_V2 wire frame |
| Logical replication (`CREATE PUBLICATION` / `CREATE SUBSCRIPTION`) | ✅ | SPG ships its own logical replication (v6.1.x); not wire-compatible with PG's |
| `pg_dump` / `pg_restore` | ❌ | SPG uses its own BACKUP/RESTORE format (`spg backup` / `spg restore`) |
| WAL streaming to a remote (`archive_command`) | ✅ | `SPG_WAL_TEE_PATH` writes a side-channel mirror |

### `pg_catalog` / introspection

| Feature | SPG | Notes |
|---|---|---|
| `pg_catalog.pg_class` / `pg_attribute` | ❌ | Use `SHOW TABLES` / `SHOW COLUMNS` |
| `information_schema.*` | ⚠️ | A small subset works through the simple-query path |
| `SHOW TABLES` (non-PG; SPG-specific) | ✅ | |
| `SHOW COLUMNS FROM t` | ✅ | |
| `spg_statistic` (PG-style `pg_stats`) | ✅ | Per-column histogram + n_distinct + null_frac |
| `spg_stat_segment` / `spg_stat_query` / etc | ✅ | Operator-facing v-tables |
| `spg_table_ddl` / `spg_database_ddl` / `spg_role_ddl` | ✅ | One-shot DDL re-rendering |

---

## Data migration: PG dump → SPG

There's no `pg_restore` parity. The pragmatic path is:

```sh
# 1. Dump from PG as data-only column-list INSERT statements.
pg_dump --data-only --column-inserts --inserts \
        --no-owner --no-privileges \
        -t mytable mydb \
        > mytable.sql

# 2. Hand-create the table in SPG (subset of the original
#    DDL — drop FK / CHECK / TRIGGER / DEFAULT-expression
#    clauses that SPG won't accept).

# 3. Run the dump through psql against SPG's PG-wire port:
psql 'postgres://spg@127.0.0.1:5433/spg' -f mytable.sql

# 4. Indexes — add them after the INSERTs land. HNSW indexes
#    take significantly less wall-time to build with rows
#    already present.
```

**Common pre-dump cleanups** (a sed pipeline is usually enough):

| Strip from `pg_dump` output | Reason |
|---|---|
| `SET … ;` lines at the top | Some SPG sets parse but trip on PG-specific values |
| `SELECT pg_catalog.set_config(...)` | No `pg_catalog` in SPG |
| `CREATE EXTENSION pg_trgm` etc | SPG has no extension system |
| `ALTER TABLE ... OWNER TO ...` | SPG doesn't track table owners |
| Sequence DDL (`CREATE SEQUENCE`, `nextval('…')`) | Replace with `AUTO_INCREMENT` column |

**Embedded-mode bulk load** (no PG-wire — call from Rust):

```rust
let mut db = Database::open_path("./mydata.db")?;
db.execute("CREATE TABLE mytable (id INT NOT NULL, …)")?;
db.execute("CREATE INDEX by_id ON mytable (id)")?;

// Stream the .sql file line-by-line; wrap N statements in
// a transaction to amortise the WAL+fsync.
db.with_transaction(|tx| {
    for stmt in read_sql_file("mytable.sql")? {
        tx.execute(&stmt)?;
    }
    Ok::<_, EngineError>(())
})?;
```

---

## A7 — what we won't add (axiom, not roadmap)

These items appear in user requests every release cycle. SPG's
design axioms make them explicit non-goals; they're not on any
v8 / v9 plan. If you need any of them, **stay on PG**.

| Won't do | Why | Workaround |
|---|---|---|
| Foreign keys / `REFERENCES` | Complexity explosion at the boundary; SPG targets analytics + vector workloads, not transactional integrity | Validate at the application layer |
| `CREATE TRIGGER` | Same as FK | Application-level event hooks |
| `CREATE FUNCTION` / PL/pgSQL | A7 | Application-level functions |
| Row-Level Security | Multi-tenant via process isolation (one SPG instance per tenant) | `docker compose` with N services |
| Multi-writer MVCC | Single-writer is the core architectural choice (powers group commit + zero-cost catalog clone) | Read replicas via logical replication |
| Multi-master | A3 | Single primary + read replicas |
| `pg_hba.conf` auth | One password per role | SCRAM with cleartext-disabled |
| `pg_catalog.*` parity | Different metadata model | `SHOW TABLES` / `spg_table_ddl` |

---

## Common migration gotchas (from real ports)

1. **`SERIAL` columns**. Use SPG's `AUTO_INCREMENT` flag on
   an `INT NOT NULL` column. Sequences themselves don't
   exist.
2. **`NOW()` / `CURRENT_TIMESTAMP`**. Available, but coerce
   to `TIMESTAMP`. The `DATE`/`TIMESTAMP`/`INTERVAL` set is
   complete; `TIMESTAMPTZ` is not — store + parse UTC.
3. **`UUID` columns**. No native UUID type — store as
   `TEXT(36)`. Comparison + index seek still work.
4. **`bytea`**. No `BYTEA` type yet — store as base64-encoded
   `TEXT`.
5. **Case-folding identifiers**. PG lower-folds unquoted
   identifiers; SPG preserves case. If your app sends
   `"SELECT * FROM Users"`, define the table as `Users` not
   `users`.
6. **`pg_catalog.*` queries from ORM auto-introspection**.
   ORMs that probe `pg_class` on connect (older
   ActiveRecord, sqlalchemy auto-reflect) fail at startup.
   Disable auto-introspection + hand-write the schema in
   the ORM config.
7. **`LISTEN` / `NOTIFY`**. Not implemented; use the v6.10.0
   pubsub (`SPG_PUBSUB_TARGET=log` for now, native NATS in
   a future v7.x).
8. **`COPY FROM`**. Not implemented; use multi-row INSERT in
   batches of 500–1000 rows, or call `spg-embedded` directly
   from a loader process.

---

## What you gain by moving

If your workload fits, the SPG-specific wins are:

- **Vector / kNN built-in** — no `pg_vector` extension, same
  operators + HNSW.
- **Cold-tier auto-management** — `SPG_HOT_TIER_BYTES` +
  freezer = no manual partition maintenance for time-series.
- **`spg-embedded` in-process mode** — drop the network hop,
  use SPG like SQLite. 0 external deps; full SQL surface.
- **`AS OF SEGMENT '<id>'`** — forensic time travel on cold
  tier without restoring a backup.
- **0 external dependencies** in the build — no extension
  ABI surprises across upgrades.
- **WAL stream tee** (`SPG_WAL_TEE_PATH`) — mirror the WAL
  to a sidecar in one syscall, feed into your own pipeline.

---

## How to validate compatibility before committing

```sh
# 1. Pull v7.3 image and start it locally.
docker run --rm -d --name spg-test \
    -p 5433:5432 \
    -e SPG_PG_ADDR=0.0.0.0:5432 \
    goliakk/spg:7.3.0

# 2. Point your app at it, run your existing PG test suite.
export PGURL='postgres://spg@127.0.0.1:5433/spg'

# 3. Failing queries surface in the test log as either:
#    - "parse error" / "unsupported …" → SQL incompat
#    - structural mismatches → application-level rewrite

# 4. Cleanup.
docker rm -f spg-test
```

We recommend running your existing suite against `spg-server`
*before* committing to a migration plan — most "yes / no /
needs work" decisions fall out of that one CI run.

---

## Where to ask

- Bug reports / feature requests: project repo issues.
- Compatibility questions: this doc's PR history is the
  canonical "is X supported?" record.
- Operational questions: `RUNBOOK.md` + `DEPLOYMENT.md`.

Documents this guide depends on:
- `CHANGELOG.md` — release contract per v7.x
- `STABILITY.md` — frozen public surfaces + "Out of v7.x"
  carve-outs
- `PROD_READY.md` — feature ship status table
- 4-corpus regression: `xtests/sqllogictest/corpus/`
