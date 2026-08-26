# SPG — Small / Smart PostgreSQL

A **single-database**, **zero-runtime-dependency** RDS in pure Rust 2024 — designed
to live inside a Docker image as the in-process database for one application.
Perf, memory footprint, and binary size are first-class goals; every layer is
written from scratch on top of `std`/`alloc`/`core`.

## What it ships

| Layer | What's there |
|---|---|
| **Wire protocol** | Self-built little-endian frame: `[u32 len][u8 op][payload]`. PING/PONG, Query, RowDescription, DataRow, CommandComplete, ErrorResponse, Stats. |
| **SQL front-end (PG dialect)** | Self-built lexer + recursive-descent + Pratt parser. DDL (`CREATE TABLE` / `INDEX` / `USER` / `PUBLICATION` / `SUBSCRIPTION` / `EXTENSION`), DML (multi-row `INSERT` / `UPDATE` / `DELETE`, `RETURNING`), full `SELECT` surface (JOINs incl. `LATERAL`/cross/full-outer-rewritten, CTEs incl. `WITH RECURSIVE`, window functions with `OVER (PARTITION BY … ORDER BY … frame)`, correlated subqueries, `GROUP BY`/`HAVING`/`DISTINCT [ON]`, `UNION`/`UNION ALL`, `EXISTS`/`IN (subq)`), transactions + savepoints, prepared statements (PG-wire extended query). |
| **Type system** | `INT` / `BIGINT` / `SMALLINT` / `FLOAT` / `NUMERIC(p,s)` / `TEXT` / `BOOL` / `DATE` / `TIMESTAMP` / `TIMESTAMPTZ` / `INTERVAL` / `JSON` / `JSONB` / `BYTEA` (v7.10.4) / `TEXT[]` (v7.10.9) / `INT[]` / `BIGINT[]` (v7.11.2) / `VECTOR(N)` (pgvector-flavoured incl. `USING SQ8` and `USING HALF`). SQL three-valued NULL logic. PG-wire OIDs on RowDescription. v7.38.20: `interval` carries the two infinities PostgreSQL 17 gave it, and a pseudo-type (`cstring`, `record`, `void`, …) is reported by the name PostgreSQL reports and refused as a column with PostgreSQL's own 42P16. |
| **Storage** | In-memory page-less heap, atomic snapshot via tmpfile+rename, secondary B-tree indices (`alloc::collections::BTreeMap`), append-only catalog binary format with magic+version. |
| **Persistence** | Two modes: atomic full-snapshot per writeful query *or* append-only WAL with fsync. WAL replay handles partial transactions via auto-rollback. |
| **Executor** | Volcano-style row pipeline. WHERE filter, projection with column aliases, table aliases, ORDER BY (any expression), LIMIT, single-column-equality index seek, kNN via `<->` + ORDER BY. |
| **Collation** (v7.38.22) | 880 of PostgreSQL 18.4's collations, performed by ICU and calibrated against it. A database records the one it was created with — from `LC_COLLATE` / `LANG`, the way `initdb` reads them — and a text column that declares none inherits it. `COLLATE` on a column and in an `ORDER BY` key; a type that cannot carry one refuses it the way PostgreSQL does (`42804`). Index keys carry their collation, so a seek and a scan give the same rows. **v7.38.22**: and so do the two SORTS. One of them compared bytes whatever the collation said, so a plain `SELECT … ORDER BY s COLLATE "en_US.utf8"` — and any query on a database that DECLARED a collation — came back in byte order while `datcollate` reported the locale; every published release through 7.38.21 did this. **The image now sets `LANG=en_US.utf8`, the same default `postgres:18` ships**, so a new database collates the way the image it replaces does; an existing data directory keeps the collation it was created with, verified against running 7.38.18 and 7.38.21 images. `SPG_LC_COLLATE=C` restores byte order. **v7.38.24**: and saying so costs nothing. The spilled sort does not write its keys — it re-derives them from the row as it merges — and that half was handed no collation, so every merge comparison re-derived what the other half had already established. On 400,000 rows of lowercase hex, declaring a collation that orders them exactly as bytes do cost **2.91x**; on the image's own default, with nothing declared, **2.36x**. It is **0.99x** now, and the values that genuinely need a collator pay what they paid. |
| **Sorting** (v7.38.22) | An eight-byte key and a row number, not the rows: an integer's whole value with its sign bit flipped, or text's first eight bytes big-endian. Later ORDER BY keys are read only inside a run of equal earlier ones, and a top-N row that loses on those eight bytes never has a key built for it. The same rules reach the SPILLED sort, so a sort larger than `work_mem` gets them too. On 400,000 rows against PostgreSQL 18, **both sides ordering bytes**, three independent runs: integer **0.69–0.74x**, two keys **0.86–0.91x**, text with 26 distinct values 1.24–1.27x, short distinct text 1.02–1.34x, long distinct text 1.42–1.55x, `LIMIT 10` over long text 0.99–1.37x. (Ranges, not single samples — a cell moves between runs by more than the difference some of them describe.) **v7.38.24**: "the same rules reach the SPILLED sort" was true of the sort and not of its KEYS — that sorter re-derives them during the merge, and until this release the re-derivation was handed no collation. |
| **Transactions** | `BEGIN`/`COMMIT`/`ROLLBACK` with a clone-on-BEGIN shadow catalog. Single-writer locking; own-write visibility inside the TX. |
| **Audit log** | Append-only, BLAKE3 hash-chain. Every committed statement appears; the daemon refuses to start if the chain has been tampered. |
| **Crypto** | Self-built BLAKE3 (full reference impl, KAT-verified against the spec). No third-party crates. |
| **CLI** | `spg ping | query | stats | version`. Pretty-prints result rows as an ASCII table. |
| **Daemon** | TCP listener, per-connection thread, shared engine via `Arc<Mutex<…>>`. CLI args + `SPG_DB` / `SPG_AUDIT` / `SPG_WAL` env vars for paths. |

## Constraints

- **`forbid(unsafe_code)`** workspace-wide.
- **Zero runtime dependencies.** Business code uses only `std` / `core` / `alloc`.
  Test-only crates may use third-party deps (none currently do).
- **Self-built infrastructure.** Wire codec, SQL parser, storage format, B-tree
  index (wrapping `alloc::collections::BTreeMap` — Rust's standard B-tree),
  WAL, audit hash chain, BLAKE3 — all in-tree.
- **No PG wire protocol compatibility.** SPG defines its own wire; the SQL
  *dialect* mirrors PG so application code remains portable.

## Crates

| Crate | Role | std? |
|---|---|---|
| `spg-wire` | Wire frame codec + opcode/value types | `no_std + alloc` |
| `spg-sql` | Lexer / parser / AST | `no_std + alloc` |
| `spg-crypto` | Self-built BLAKE3 | `no_std + alloc` |
| `spg-storage` | Catalog / table / row / index / on-disk format | `no_std + alloc` |
| `spg-audit` | BLAKE3 hash-chain audit log | `no_std + alloc` |
| `spg-manifest` | Snapshot / WAL / backup envelope metadata | `no_std + alloc` |
| `spg-engine` | SQL executor + expression evaluator | `no_std + alloc` |
| `spg-embedded` | In-process embedded database (`Database::execute`) | `no_std + alloc` |
| `spg-embedded-tokio` | Async wrapper with `spawn_blocking` + read-snapshot fan-out | `std` (tokio) |
| `spg-server` | TCP + PG-wire daemon binary | `std` |
| `spgctl` | `spgctl` client binary + WAL utilities | `std` |

## Quick start

```sh
# Build everything (release).
cargo build --workspace --release

# Run the daemon (in-memory).
./target/release/spg-server 127.0.0.1:5544

# Persistent + audit + WAL.
./target/release/spg-server 127.0.0.1:5544 ./spg.db ./audit.log ./wal.log

# In another terminal:
./target/release/spg ping
./target/release/spg query "CREATE TABLE u (id INT NOT NULL, name TEXT NOT NULL)"
./target/release/spg query "INSERT INTO u VALUES (1, 'alice')"
./target/release/spg query "SELECT * FROM u"
./target/release/spg query "BEGIN"
./target/release/spg query "INSERT INTO u VALUES (2, 'bob')"
./target/release/spg query "COMMIT"
./target/release/spg query "CREATE INDEX by_id ON u (id)"
./target/release/spg query "SELECT * FROM u WHERE id = 1"
./target/release/spg stats
./target/release/spg version
```

### Docker: the image has no shell

`goliakk/spg` ships `spg-server`, `spg` and `pg_isready` on a
distroless base — there is no `/bin/sh` in it. Compose healthchecks
must therefore use the **exec form**:

```yaml
healthcheck:
  test: ["CMD", "pg_isready", "-U", "youruser"]
```

The `CMD-SHELL` spelling — which is what the `postgres` image's own
documentation uses, so it is where most people start — runs the test
through `/bin/sh` and cannot execute here. The container then never
reports healthy and `depends_on: condition: service_healthy` holds the
whole stack down before a single statement is issued. The error names
the database (`container db is unhealthy`), which is the wrong
component: the database is up and answering.

Reported by sentori, who lost an hour to it.

### SQL taste

The block below is executable documentation — the test suite's
doc-as-corpus step runs every ```sql fence in this repo against the
embedded engine, so if it stops working, CI turns red:

```sql
CREATE TABLE msg (id INT NOT NULL PRIMARY KEY, sender TEXT NOT NULL,
                  body JSONB, sent TIMESTAMP);
INSERT INTO msg VALUES
  (1, 'alice', '{"tags": ["intro", "hello"]}', '2026-01-05 09:00:00'),
  (2, 'bob',   '{"tags": ["hello"]}',          '2026-01-05 09:05:00'),
  (3, 'alice', NULL,                           '2026-01-06 10:00:00');

-- JSONB containment, PG spelling.
SELECT id FROM msg WHERE body @> '{"tags": ["hello"]}';

-- Window functions over a CTE.
WITH by_sender AS (
  SELECT sender, count(*) AS n FROM msg GROUP BY sender
)
SELECT sender, n, rank() OVER (ORDER BY n DESC) FROM by_sender;

-- expect-error
SELECT no_such_column FROM msg;
```

### sqlx embed (in-process, no daemon)

Drop-in for `sqlx::PgPool`. Stock sqlx code — `Pool` +
`query!` + transactions — runs against an in-process SPG.

```rust
use spg_sqlx::{SpgConnectOptions, SpgPool, SpgPoolOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // v7.18 — PG-shape Pool. 20 connections fan out reads through
    // a per-statement snapshot path; writes serialise (engine
    // single-writer). Read-committed isolation matches PG default.
    let pool: SpgPool = SpgPoolOptions::new()
        .max_connections(20)
        .connect_with(SpgConnectOptions::file("./spg.db".into()))
        .await?;

    sqlx::query("CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL)")
        .execute(&pool).await?;
    sqlx::query("INSERT INTO users VALUES ($1, $2)")
        .bind(1_i32).bind("alice")
        .execute(&pool).await?;

    let row = sqlx::query("SELECT name FROM users WHERE id = $1")
        .bind(1_i32).fetch_one(&pool).await?;
    let name: String = sqlx::Row::get(&row, 0);
    println!("{name}");
    Ok(())
}
```

`SpgPool` is a `Pool<Spg>` type alias — every sqlx-core API
generic over `Pool<DB>` works (transactions, savepoints,
`query!()` compile-time validation against an in-memory catalog,
`fetch_all` / `fetch_optional` / `fetch_one`). Concurrent
`SELECT`s fan out across pool connections through the engine's
snapshot path; benchmark data lives at
[`xbench/competitor/results/sqlx_pool_concurrent_reads-v7.18-trial.md`](xbench/competitor/results/sqlx_pool_concurrent_reads-v7.18-trial.md).

### kNN demo

```sh
./target/release/spg query "CREATE TABLE emb (id INT NOT NULL, v VECTOR(3) NOT NULL)"
./target/release/spg query "INSERT INTO emb VALUES (1, [1.0, 2.0, 3.0])"
./target/release/spg query "INSERT INTO emb VALUES (2, [4.0, 5.0, 6.0])"
./target/release/spg query "INSERT INTO emb VALUES (3, [1.0, 2.0, 4.0])"
./target/release/spg query "SELECT * FROM emb ORDER BY v <-> [1.0, 2.0, 3.0] LIMIT 2"
```

### Backup / restore / PITR

v7.18 ships a Point-in-Time Recovery workflow via four `spg` subcommands:

```sh
# Capture the current snapshot + WAL into a backup directory.
spg backup-pitr --src ./spg.db --dst /backups/spg-2026-06-10

# Verify the backup is intact (writes missing checksums on first run).
spg verify-pitr --dir /backups/spg-2026-06-10 --write-missing-checksums

# Restore the catalog to a specific moment.
spg pitr-restore \
    --snapshot /backups/spg-2026-06-10/snapshot.spg \
    --wal /backups/spg-2026-06-10/wal/<chunk>.wal \
    --to '2026-06-10 14:32:00' \
    --target /restored/spg-2026-06-10-1432.spg

# Drop WAL chunks past your retention window.
spg prune-pitr --dir /backups/spg-2026-06-10 --retention-hours 24
```

v7.19 — rotation, retention, and archival run **in-engine**.
Set the env vars at boot; no cron needed:

```sh
SPG_PITR_RETENTION_HOURS=24                              # retention thread on
SPG_PITR_ARCHIVE_CMD='aws s3 cp "$1" s3://my-backups/spg/'  # archive-then-delete
```

The WAL rotates into immutable chunk files at every checkpoint;
the retention thread archives + sweeps old chunks; a failed
archive command leaves the chunk on disk with a loud warning
(PG `archive_command` semantics).

SLA defaults: **RPO ≤ 1s**, **RTO ≤ 10min**, **retention 24h**.
See [`PG_MIGRATION.md`](PG_MIGRATION.md#backup--pitr-v718) for the full
operator playbook.

### Tests

Since v7.38 the suite runs as three data-driven tiers with hard time
budgets — `precommit` (≤150 s, wired into the git pre-commit hook),
`prerelease` (≤25 min, **every release must pass it**), and `full`
(the nightly find-problems tier: permutation matrix over both wire
protocols, three-master differential oracle, isolation interleavings,
a generative differ, SQL:2016 coverage, doc-as-corpus, pgbench /
sysbench scoreboards). `gate.sh`'s categories survive as components;
see [`docs/TESTING.md`](docs/TESTING.md).

```sh
scripts/suite.sh precommit       # the commit gate (also runs from the hook)
scripts/suite.sh prerelease --on-mini   # the release gate, on the LAN testbed
scripts/suite.sh full            # nightly battery
scripts/gate.sh e2e              # one legacy category, directly
```

## Status

**v7.12** (current). PG-port-ready surface, used in production
embedded scenarios. Drop-in PG replacement for the schemas
that don't reach for SPG's explicit carve-outs — see
[`PG_MIGRATION.md`](PG_MIGRATION.md) for the full matrix +
"if your schema uses X, you can drop it in as-is" quick start.

Highlights since v1.0:

- PG-wire protocol on `SPG_PG_ADDR` — simple + extended query,
  Parse / Bind / Execute / Describe, binary-format Bind params
  (13 PG types), prepared-statement plan cache.
- pgvector-flavoured `VECTOR(N)` with HNSW indices; `USING SQ8`
  8-bit and `USING HALF` (binary16) on-disk encodings.
- Full SQL: JOIN (incl. LEFT/cross), CTE incl. WITH RECURSIVE,
  window functions with frames + NULL treatment, correlated
  subqueries (memoised), `EXPLAIN ANALYZE`, optimizer with
  ANALYZE + JOIN reorder + Memoize node.
- Native types: BYTEA (v7.10.4), TEXT[] / INT[] / BIGINT[]
  arrays (v7.10.9 / v7.11.2) with full op surface (subscript,
  ANY / ALL, `array_length` / `array_position` / `unnest` /
  `||`), `substring` / `position` on TEXT + BYTEA.
- **PG full-text search (v7.12)** — `tsvector` / `tsquery`
  types, `to_tsvector` with Porter stemmer + Simple configs,
  the four query constructors (`plainto_tsquery`,
  `to_tsquery`, `phraseto_tsquery`, `websearch_to_tsquery`),
  `@@` operator, `ts_rank` / `ts_rank_cd`, and a real
  GIN inverted index. mailrs-shape `messages.search_vector
  tsvector` schemas drop in unmodified.
- **PL/pgSQL row triggers (v7.12)** —
  `CREATE FUNCTION ... LANGUAGE plpgsql AS $$ ... $$` +
  `CREATE TRIGGER ... { BEFORE | AFTER } { INSERT | UPDATE |
  DELETE } ... FOR EACH ROW EXECUTE FUNCTION fn()`. Body
  subset: `DECLARE` / `IF / ELSIF / ELSE / END IF` /
  `RAISE NOTICE | EXCEPTION` / `RETURN NEW | OLD | NULL` /
  embedded `INSERT / UPDATE / DELETE` referencing NEW/OLD.
- `INSERT … ON CONFLICT { DO NOTHING | DO UPDATE SET …
  EXCLUDED.col } [RETURNING …]` (v7.9), composite-target
  `ON CONFLICT (a, b)` for CalDAV / CardDAV upsert.
- `INSERT / UPDATE / DELETE … RETURNING` (v7.9.4) — real
  DataRow stream, IMAP UID monotonic-alloc / mailrs-shape
  patterns work as written.
- Foreign keys (v7.6) — all four `ON DELETE / ON UPDATE`
  actions, including CASCADE plan-then-apply.
- Logical replication (v6.1) — publications, subscriptions,
  segment forwarding.
- WAL compression (v6.6) — LZSS, no-deps hand-rolled, with
  torn-write resilience.
- Cold-tier segments + `AS OF SEGMENT '<id>'` time-travel
  (v6.10.2).
- Observability v2 — `spg_stat_*` virtual tables for
  replication, segments, per-query stats, per-connection
  activity, audit chain, DDL emit.
- `spg-embedded` for in-process use; `spg-embedded-tokio` for
  async with snapshot-based read fan-out (v7.11.0).

See [`STABILITY.md`](STABILITY.md) for the wire-frozen surface
matrix and [`CHANGELOG.md`](CHANGELOG.md) for the full minor /
patch history.

Carved out (genuinely not in scope, see
[`PG_MIGRATION.md`](PG_MIGRATION.md) § A7 for why):
multi-master / quorum replication, multi-writer MVCC,
INTERSECT / EXCEPT, server-side cursor / partial Execute,
multi-dimensional arrays, SMALLINT[] / NUMERIC[] / BOOLEAN[]
arrays, Row-Level Security, `pg_catalog.*` parity. The
v7.12 epic narrowed the carve-out list significantly — FTS,
`CREATE TRIGGER`, PL/pgSQL, and `ON CONFLICT` all moved out
(see CHANGELOG entries v7.9, v7.12.0–7).

## License

MIT OR Apache-2.0
