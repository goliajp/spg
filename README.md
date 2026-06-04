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
| **Type system** | `INT` / `BIGINT` / `SMALLINT` / `FLOAT` / `NUMERIC(p,s)` / `TEXT` / `BOOL` / `DATE` / `TIMESTAMP` / `TIMESTAMPTZ` / `INTERVAL` / `JSON` / `JSONB` / `BYTEA` (v7.10.4) / `TEXT[]` (v7.10.9) / `INT[]` / `BIGINT[]` (v7.11.2) / `VECTOR(N)` (pgvector-flavoured incl. `USING SQ8` and `USING HALF`). SQL three-valued NULL logic. PG-wire OIDs on RowDescription. |
| **Storage** | In-memory page-less heap, atomic snapshot via tmpfile+rename, secondary B-tree indices (`alloc::collections::BTreeMap`), append-only catalog binary format with magic+version. |
| **Persistence** | Two modes: atomic full-snapshot per writeful query *or* append-only WAL with fsync. WAL replay handles partial transactions via auto-rollback. |
| **Executor** | Volcano-style row pipeline. WHERE filter, projection with column aliases, table aliases, ORDER BY (any expression), LIMIT, single-column-equality index seek, kNN via `<->` + ORDER BY. |
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

### kNN demo

```sh
./target/release/spg query "CREATE TABLE emb (id INT NOT NULL, v VECTOR(3) NOT NULL)"
./target/release/spg query "INSERT INTO emb VALUES (1, [1.0, 2.0, 3.0])"
./target/release/spg query "INSERT INTO emb VALUES (2, [4.0, 5.0, 6.0])"
./target/release/spg query "INSERT INTO emb VALUES (3, [1.0, 2.0, 4.0])"
./target/release/spg query "SELECT * FROM emb ORDER BY v <-> [1.0, 2.0, 3.0] LIMIT 2"
```

### Tests

```sh
cargo test --workspace          # everything
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

## Status

**v7.11** (current). PG-port-ready surface, used in production
embedded scenarios. Highlights since v1.0:

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

Carved out (not currently in scope): `tsvector` + GIN (use
external FTS), multi-master / quorum replication, INTERSECT /
EXCEPT, `ON CONFLICT` upsert, server-side cursor / partial
Execute, multi-dimensional arrays, SMALLINT[] / NUMERIC[] /
BOOLEAN[] arrays. See [`PG_MIGRATION.md`](PG_MIGRATION.md) for
the full migration matrix.

## License

MIT OR Apache-2.0
