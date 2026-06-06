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
| Multi-writer OLTP / triggers / stored procs | **Stay on PG** | n/a — these are explicit non-goals (axiom A7). FKs land in v7.6. |

The rest of this doc backs each row with specifics.

---

## Decision tree

```
                ┌──────────────────────────────────────────────┐
                │  Does your app rely on any of:               │
                │   - Triggers                                 │
                │   - Stored procedures (PL/pgSQL etc)         │
                │   - Row-Level Security                       │
                │   - Multiple concurrent writers              │
                │   - Multi-master replication                 │
                │  (FKs ship in v7.6 — not a blocker anymore)  │
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

## Quick start — "if your schema uses X, drop it in" (v7.12.10 ship-time)

The 60-second read for customers porting from PG. If every
item your `pg_dump` / `init-schema.sql` reaches for is on the
✅ list below, the schema runs unchanged on SPG today. The
full reference matrix follows in the next section.

**Drops in as-is — no rewrite needed**:

- `tsvector` + `tsquery` types, `to_tsvector(config, text)` +
  the four query constructors (`plainto_tsquery`,
  `to_tsquery`, `phraseto_tsquery`, `websearch_to_tsquery`),
  `@@` match, `ts_rank` / `ts_rank_cd`, real GIN inverted
  index (v7.12.0–3)
- `CREATE FUNCTION fn() RETURNS TRIGGER LANGUAGE plpgsql AS
  $$ ... $$` + `CREATE TRIGGER ... { BEFORE | AFTER } { INSERT
  | UPDATE | DELETE } [OR ...] ON tbl FOR EACH ROW EXECUTE
  FUNCTION fn()` (v7.12.4–7). Body subset: `BEGIN/END`,
  `DECLARE var TYPE [:= init];`, `NEW.col := <expr>;`
  (BEFORE only), `IF/ELSIF/ELSE/END IF`,
  `RAISE { NOTICE | WARNING | INFO | LOG | DEBUG }
  '<fmt>' [, args]*`, `RAISE EXCEPTION '<fmt>' [, args]*`,
  `RETURN NEW / OLD / NULL`, embedded `INSERT / UPDATE /
  DELETE / SELECT` referencing NEW/OLD
- `INSERT ... ON CONFLICT (col) DO NOTHING` (v7.9.8)
- `INSERT ... ON CONFLICT (col) DO UPDATE SET col =
  EXCLUDED.col [, ...] [WHERE ...] [RETURNING ...]` (v7.9.9)
- `INSERT ... ON CONFLICT (col1, col2)` composite target
  (v7.9.10)
- `INSERT / UPDATE / DELETE ... RETURNING col1, col2, ...`
  (v7.9.4) — real DataRow stream
- Foreign keys with all four `ON DELETE / ON UPDATE` actions
  (NO ACTION, RESTRICT, CASCADE, SET NULL / SET DEFAULT) —
  v7.6
- `NOW()`, `CURRENT_TIMESTAMP`, `CURRENT_DATE`,
  `INTERVAL '30 days'` literals at expression position (v7.11.3)
- `LIMIT $1` / `LIMIT $n` parameter placeholders (v7.9.24)
- Multi-column / AND-composite `WHERE` with leading-column
  index seek + caller-side filter on remaining columns
  (v7.11.3)
- `CREATE EXTENSION pg_trgm` (and other extensions) as no-op
  accepted (v7.9.15); `CREATE INDEX ... USING gin (jsonb_col)`
  loads as BTree fallback on the leading column (v7.9.26b)
- `BIGSERIAL` / `SERIAL` / `SMALLSERIAL` PRIMARY KEY inline
  (v7.9.13)
- `JSONB` (PG-wire OID 3802), `TEXT[]` / `INT[]` / `BIGINT[]`
  arrays, `TIMESTAMPTZ`, `BYTEA` (v7.9 / v7.10 / v7.11)
- pgvector `USING ivfflat` accepted as alias for `USING hnsw`;
  pgvector opclass `vector_cosine_ops` recognised (v7.11.3)

**Needs a small schema-side change** (the change itself is
mechanical; the rewrite list under your `pg_dump` output is
short):

- `RIGHT JOIN` / `FULL OUTER JOIN` → rewrite as `LEFT JOIN`
- `INTERSECT` / `EXCEPT` → application-level set ops
- `UUID` column → `TEXT(36)` with the same string format
- `SMALLINT[]` / `BOOLEAN[]` / `NUMERIC[]` → `INT[]` /
  `BIGINT[]` / `TEXT[]` (you keep the operator surface
  identically)
- `CHECK` constraints → enforce at the application layer or
  via a BEFORE trigger that runs `RAISE EXCEPTION` (v7.12.6)

**Genuine carve-outs** — if your schema reaches for these,
you're not on the SPG migration path today. See §A7 below
for the reasoning:

- Row-Level Security (`RLS`)
- Multi-writer MVCC (concurrent unrelated writers without
  app-level partition)
- `pg_hba.conf`-style auth rules
- `pg_catalog.*` system catalog parity (use
  `SHOW TABLES` / `spg_table_ddl` instead)
- Server-side cursor / partial Execute
- Multi-dimensional arrays

If your schema mixes drop-ins with one or two carve-outs,
the typical answer is "run SPG for the drop-in tables + keep
PG alongside for the carve-out tables" — they can share a
PG-wire client pool with a per-statement router.

---

## SQL compatibility matrix (v7.12.10 ship-time)

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
| Foreign keys (`REFERENCES … ON DELETE/UPDATE …`) | ✅ v7.6 | All four `ON DELETE / ON UPDATE` actions (NO ACTION / RESTRICT / CASCADE / SET NULL / SET DEFAULT) |
| `CHECK` constraints | ❌ | A7 — won't do |
| `CREATE TRIGGER` | ✅ v7.12.4 | BEFORE/AFTER row-level triggers on INSERT/UPDATE/DELETE; see [§PL/pgSQL triggers](#plpgsql-triggers) below |
| `CREATE FUNCTION ... LANGUAGE plpgsql` (trigger functions) | ✅ v7.12.4 | DECLARE / IF / RAISE / embedded SQL — full subset used by mailrs's `update_search_vector`; see [§PL/pgSQL triggers](#plpgsql-triggers) below |
| `CREATE FUNCTION` (scalar UDF, non-trigger) | ⚠️ | DDL parses, body is stored, but invocation surface ships in v7.13+. Use built-in functions for v7.12.x. |
| Partition tables (`PARTITION BY`) | ❌ | Cold-tier covers time-series natively |
| Row-Level Security (`RLS`) | ❌ | A5 — process isolation instead |

### Data types

| Type | SPG | Notes |
|---|---|---|
| `SMALLINT` / `INT` / `BIGINT` | ✅ | Integer types |
| `REAL` / `DOUBLE PRECISION` / `FLOAT` | ✅ | All map to `Float` (f64) |
| `NUMERIC(p, s)` | ✅ | Exact decimal up to p=38 |
| `BOOLEAN` | ✅ | |
| `TEXT` / `VARCHAR(n)` / `CHAR(n)` | ✅ | |
| `DATE` | ✅ | Days since epoch |
| `TIMESTAMP` | ✅ | Microseconds since epoch |
| `TIMESTAMPTZ` | ✅ v7.9.2 | Internally UTC `TIMESTAMP`; PG-wire OID 1184 |
| `INTERVAL` | ⚠️ | Runtime literals only, no column storage |
| `JSON` | ✅ | Text-backed; PG-wire OID 114 |
| `JSONB` | ✅ v7.9.0 | Same storage as JSON; PG-wire OID 3802 for sqlx-style clients |
| `SERIAL` / `BIGSERIAL` | ✅ v7.9.6 | Aliased to `INT/BIGINT NOT NULL AUTO_INCREMENT` |
| `UUID` | ❌ | Store as `TEXT(36)` |
| `BYTEA` | ✅ v7.10.4 | Native bytes; PG-wire OID 17, `\xDEADBEEF` hex literal in/out, `length()` / `octet_length()` (v7.10.4), `\|\|` / `substring` / `position` (v7.11.2) |
| `VECTOR(N)` | ✅ | pgvector-flavoured; HNSW + SQ8/HALF encodings |
| `tsvector` + `tsquery` types | ✅ v7.12.0 | PG-wire OIDs 3614 / 3615; `pg_dump`-shape `::tsvector` / `::tsquery` cast literals round-trip; see [§Full-text search](#full-text-search) below |
| `TEXT[]` | ✅ v7.10.9 | PG-wire OID 1009; external form `{a,b,NULL}` round-trips, `ARRAY[…]` literal, subscript, `ANY` / `ALL`, `array_length` / `array_position` / `unnest` / `\|\|` (v7.11.1) |
| `INT[]` / `BIGINT[]` | ✅ v7.11.2 | PG-wire OIDs 1007 / 1016; full op parity with `TEXT[]` — typed `unnest`, mixed-width `\|\|` widens to `BIGINT[]` |
| `SMALLINT[]` / `NUMERIC[]` / `BOOLEAN[]` / `FLOAT[]` | ❌ | Store as `INT[]` / `BIGINT[]` / `TEXT[]` until v7.12 |
| Multi-dimensional arrays | ❌ | Single-dim only; `array_length(_, dim>1)` returns NULL |

### Data manipulation (DML)

| Feature | SPG | Notes |
|---|---|---|
| `INSERT INTO t VALUES (…), (…)` | ✅ | Multi-row INSERT |
| `INSERT INTO t (col, …) VALUES …` | ✅ | Column-list INSERT |
| `INSERT … RETURNING` | ✅ v7.9.4 | Real DataRow stream — IMAP UID monotonic-alloc / mailrs-shape patterns work |
| `UPDATE t SET … WHERE …` | ✅ | Single-statement |
| `UPDATE t SET … WHERE … RETURNING …` | ✅ v7.9.4 | Same as INSERT RETURNING |
| `DELETE FROM t WHERE …` | ✅ | |
| `DELETE … RETURNING` | ✅ v7.9.4 | Returns the pre-delete row state (PG semantics) |
| `INSERT … ON CONFLICT (col) DO NOTHING` | ✅ v7.9.8 | BTree-fast-path conflict resolution + within-batch dedup |
| `INSERT … ON CONFLICT (col) DO UPDATE SET … EXCLUDED.col` | ✅ v7.9.9 | Includes mixed `tbl.col + EXCLUDED.col` exprs, optional WHERE, and RETURNING over the post-update row |
| `INSERT … ON CONFLICT (col1, col2)` composite target | ✅ v7.9.10 | For CalDAV / CardDAV upsert patterns |
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
| `unnest(arr)` at FROM position | ✅ v7.11.1 | TEXT[] / INT[] / BIGINT[]; uncorrelated only (no LATERAL / JOIN-position) |
| `substring(x, start [, len])` / `position(needle, hay)` | ✅ v7.11.2 | TEXT + BYTEA; function-call form (PG-spec `FROM … FOR …` / `IN …` syntax deferred) |

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
| `ivfflat` index | ✅ v7.11.3 | Accepted as a synonym for `hnsw` — both `pg_dump`-shape spellings load |
| `vector_dims()` / `vector_norm()` | ⚠️ | Some functions present; not the full pgvector function set |

### Full-text search

The full PG FTS stack landed across v7.12.0–7. Schemas using
`tsvector` + `GIN` + the `@@` query operator + trigger-
maintained search vectors run unmodified.

| Feature | SPG | Notes |
|---|---|---|
| `tsvector` / `tsquery` column types | ✅ v7.12.0 | PG-wire OIDs 3614 / 3615 |
| `'foo:1 bar:2,3A'::tsvector` cast literal (`pg_dump` shape) | ✅ v7.12.0 | Quoted + bare lexeme syntax, optional `:positions[weight]` suffix; auto sort + dedupe |
| `'cat & dog \| !fish'::tsquery` cast literal | ✅ v7.12.0 | Pratt parser over `&` `\|` `!` `()` and phrase `<N>` |
| `to_tsvector(config, text)` / `to_tsvector(text)` | ✅ v7.12.1 | English Porter stemmer + `simple` config; `default_text_search_config` honoured for the 1-arg form |
| `plainto_tsquery` / `phraseto_tsquery` / `to_tsquery` / `websearch_to_tsquery` | ✅ v7.12.1 | All four query constructors |
| `SET default_text_search_config = 'english'` | ✅ v7.12.1 | Session-scoped; `'pg_catalog.english'` qualified form also accepted |
| `@@` match operator (`tsvector @@ tsquery` either order) | ✅ v7.12.2 | NULL on either side returns NULL (3VL) |
| `ts_rank(vec, query)` / `ts_rank_cd(vec, query)` | ✅ v7.12.2 | Weight × occurrence sum normalised by `1 + ln(unique_terms)`; `cd` adds cover-density factor |
| `CREATE INDEX … USING GIN (tsvector_col)` | ✅ v7.12.3 | Real posting-list inverted index — replaces the v7.9.26b BTree fallback. `@@` query planner picks it automatically; `Term` / `And` / `Or` accelerated, `Not` / `Phrase` fall through to full scan |
| `CREATE INDEX … USING GIN (non_tsvector_col)` | ✅ v7.9.26b | Loads as BTree fallback on the leading column so `pg_dump` JSONB-GIN scripts still load |
| `ts_headline` / `ts_lexize` / other display-side FTS funcs | ❌ | Out of scope for v7.12.x; on the v7.13+ queue if customer-flagged |
| Spanish / French / German / non-English config | ❌ | `simple` and `english` only in v7.12.x — unsupported configs error with a clear message |
| Trigram (`pg_trgm` extension) | ❌ | `CREATE EXTENSION pg_trgm` parses as no-op (v7.9.15); the operators / functions don't exist yet |

### PL/pgSQL triggers

The full PL/pgSQL trigger surface landed across v7.12.4–7.
mailrs's `AFTER INSERT OR UPDATE ON messages` trigger
maintaining `search_vector` from `subject || sender ||
clean_text` runs end-to-end.

| Feature | SPG | Notes |
|---|---|---|
| `CREATE [OR REPLACE] FUNCTION fn() RETURNS TRIGGER LANGUAGE plpgsql AS $$ … $$` | ✅ v7.12.4 | Persisted in the catalog; body re-parsed on each fire |
| `CREATE [OR REPLACE] TRIGGER name { BEFORE \| AFTER } { event } [OR { event }]* ON tbl FOR EACH ROW EXECUTE FUNCTION fn()` | ✅ v7.12.4 | All event combinations (INSERT / UPDATE / DELETE); `EXECUTE PROCEDURE` legacy spelling also accepted |
| `BEGIN ... END;` body | ✅ v7.12.4 | Required outer block |
| `NEW.col := <expr>;` (BEFORE only) | ✅ v7.12.4 | AFTER triggers attempting NEW.col := … error with a clear "NEW is read-only post-write" message |
| `RETURN NEW` / `RETURN OLD` / `RETURN NULL` / bare `RETURN;` | ✅ v7.12.4 | NULL skips the row (BEFORE) / no-ops the notification (AFTER) |
| `OLD.col := <expr>;` | ❌ | PG forbids; we mirror with a clear error |
| `DECLARE var TYPE [:= init_expr];` | ✅ v7.12.6 | Block before BEGIN; earlier DECLAREs in scope for later init exprs |
| `IF cond THEN ... [ELSIF cond THEN ...]* [ELSE ...] END IF;` | ✅ v7.12.6 | Arbitrary nesting; bodies are full statement lists |
| `RAISE { NOTICE \| WARNING \| INFO \| LOG \| DEBUG } '<fmt>' [, args]*;` | ✅ v7.12.6 | PG-style `%` substitution; logged but doesn't affect outcome |
| `RAISE EXCEPTION '<fmt>' [, args]*;` | ✅ v7.12.6 | Aborts trigger function; propagates as engine-level error rolling back the firing DML |
| Embedded SQL: `INSERT / UPDATE / DELETE / SELECT` inside trigger body | ✅ v7.12.7 | NEW / OLD / DECLARE-local refs substituted into the statement's Expr tree; recursion bounded at 16 deep |
| `DROP TRIGGER [IF EXISTS] name ON tbl` | ✅ v7.12.4 | |
| `DROP FUNCTION [IF EXISTS] fn[()]` | ✅ v7.12.4 | Arg-list disambiguation deferred (no PG-style overloading in v7.12.x) |
| `LOOP / WHILE / FOR` iteration | ❌ | Carve-out; not on the mailrs critical path |
| `SELECT … INTO var` binding | ❌ | The SELECT runs (as embedded SQL) but doesn't yet bind the result into a local. Workaround: use a separate UPDATE |
| `GET DIAGNOSTICS` / `EXIT WHEN` / nested sub-blocks | ❌ | Out of scope for v7.12.x |
| `BEFORE` trigger embedded SQL with PG's strict inline-between-rows semantics | ⚠️ | Embedded SQL collected during the row-write pass + executed after the firing DML's main work completes. Functionally equivalent for audit / sync / cascade patterns; differs only if the embedded SQL needs to read its own pre-INSERT row |

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

> **v7.5 update — A7 narrowed.** Foreign keys were removed from
> the won't-do list in v7.5. SPG's single-writer model makes FK
> enforcement cheap, and downstream users surfaced FK as a hard
> requirement. The full `REFERENCES … ON DELETE …` surface ships
> in v7.6.
>
> **v7.9 update — A7 narrowed again.** `INSERT … ON CONFLICT DO
> UPDATE` was originally on the won't-do list (PG's complexity
> there is the concurrent-write race; SPG's single-writer model
> collapses that to a BTree-seek-then-branch). The mailrs
> migration evidence (47 use sites) made it worth shipping;
> landed across v7.9.7–10.
>
> **v7.12 update — A7 narrowed again.** `CREATE TRIGGER` +
> `CREATE FUNCTION ... LANGUAGE plpgsql` were originally on the
> won't-do list. The mailrs migration drove a full PG FTS
> stack (G-CRIT-3), and the customer-natural way to maintain a
> `tsvector` column is an `AFTER INSERT OR UPDATE` row-level
> trigger. The "side effects break determinism" concern is
> mitigated by SPG's single-writer model: trigger execution
> happens inside the row-write loop holding the catalog lock,
> so determinism is preserved as long as the trigger function
> is itself deterministic (which is a customer-side property,
> not an engine one). Shipped across v7.12.4–7. **The
> remaining A7 items below are still structural non-goals.**

| Won't do | Why | Workaround |
|---|---|---|
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
4. **`bytea`**. ✅ Native since v7.10.4. Wire form is PG hex
   text (`\x` prefix); inserts accept either hex or octal escape.
   Scalar ops: `\|\|`, `substring`, `position`, `length` /
   `octet_length` (all v7.11.2). No inline `'\x..'::BYTEA` cast
   yet — go through a `BYTEA` column.
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
- Operational questions: operational runbook + deployment notes.

Documents this guide depends on:
- `CHANGELOG.md` — release contract per v7.x
- `STABILITY.md` — frozen public surfaces + "Out of v7.x"
  carve-outs
- internal readiness matrix — feature ship status table
- 4-corpus regression: `xtests/sqllogictest/corpus/`
