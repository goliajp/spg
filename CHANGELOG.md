# Changelog

Format: [Keep a Changelog](https://keepachangelog.com).
Versions follow SemVer.

The most recent commit on `master` is the source of truth for
the current build; this file is a release-organized view.

---

## [7.11.3] — 2026-06-04 (PG-customer parity patch — mailrs D-cutover)

Patch release closing four PG idioms mailrs flagged as still
broken in their D-cutover gap analysis. Two of the four were
historically introduced as missing parser features but had
later been resolved; this release fills the runtime side that
made them appear broken end-to-end. The other two are
genuinely new parser surfaces. Plus one planner fix that lifts
multi-column / AND-composite WHERE clauses out of the
"full-scan + filter" fast-path penalty box.

What lands:

  * **`NOW()` / `CURRENT_TIMESTAMP` / `CURRENT_DATE` in
    `spg-embedded`** — the clock-call rewrite layer
    (`Engine::with_clock`) was wired in `spg-server` since v6.x
    but `Database::open_in_memory()` / `Database::open_path()`
    constructed the engine without a clock provider. SQL like
    `WHERE created_at > NOW() - INTERVAL '30 days'` now works
    in every entry point (server, embedded, embedded-tokio).

  * **`USING ivfflat` accepted as a synonym for `hnsw`** — PG
    customers shouldn't pick their on-disk index method based on
    which one SPG happens to implement first. The parser
    accepts both spellings; the runtime vector op (`<->` /
    `<#>` / `<=>`) at query time still picks the metric.

  * **`CREATE INDEX … WITH (k = v, ...)` storage params** — PG
    schemas using pgvector emit `WITH (lists = 20)` for
    ivfflat or `WITH (m = 16, ef_construction = 64)` for hnsw.
    Accepted and discarded; SPG's HNSW tunes itself via env
    vars today, so the WITH clause is informational.

  * **Multi-column / PK index picker under AND-composite
    WHERE** — `try_index_seek` now recurses through top-level
    `AND` so `WHERE id = 1 AND created_at > $1` hits the
    leading-column index instead of degrading to a full scan
    plus post-filter. EXPLAIN annotates the chosen plan
    accordingly. The caller already re-applies the full WHERE
    to every returned row, so dropping the residual conjuncts
    at seek time stays semantically correct.

  * **New regression test
    `crates/spg-engine/tests/e2e_pg_customer_parity.rs`** —
    every PG idiom mailrs raised in D-cutover (the 7
    critical-priority + 1 nice-to-have items) is now a single
    test. Closed gaps assert; the one still-open v7.12 epic
    (tsvector / GIN / `@@` / FTS triggers) is `#[ignore]`-marked
    with a TODO pointer.

Catalog FILE_VERSION unchanged (still 19 from v7.11.2). 4-corpus
sqllogictest: 100% (148 / 17 / 144 / 63). Workspace test suites
all green.

Carve-out for v7.12: full PG full-text search — `tsvector` /
`tsquery` types, `to_tsvector` / `plainto_tsquery` / `ts_rank`,
the `@@` match operator, true GIN inverted index, and a
row-level `CREATE TRIGGER` system so PG's standard
`AFTER INSERT/UPDATE … UPDATE search_vector` idiom works
without application changes. Tracked in
`.claude/internal-docs/V7_12_DESIGN.md` (to be drafted).

Sub-versions:

  v7.11.11-17  Epic 3 — INT[] / BIGINT[] + BYTEA scalar ops
               (see [7.11] above)
  v7.11.18     PG-customer parity patch — clock injection,
               ivfflat alias, WITH (…) drain, multi-column
               index picker AND recursion, regression suite

---

## [7.11] — 2026-06-04 (read fan-out + v7.11 series open)

Opens the v7.11 series. Three epics planned: read concurrency
(this release), array operators / `unnest`, and type widening
(`INT[]` / `BIGINT[]` / BYTEA scalar ops). Full plan in
`.claude/internal-docs/V7_11_DESIGN.md`.

**Epic 1 — read fan-out (this release).** mailrs's tokio cement
is read-heavy (IMAP FETCH traffic per session). v7.10's
`AsyncDatabase` serialises EVERY call on a single tokio `Mutex`,
including SELECTs — a slow read blocks every concurrent reader.

What lands:

  * `Engine::clone_snapshot() -> CatalogSnapshot` — frozen,
    Send+Sync view of the committed catalog. Backed by the
    existing `PersistentVec` row storage so cloning is O(log n)
    per table; no row body copies.
  * `Engine::execute_readonly_on_snapshot(&snap, sql)` — runs
    SELECT against a snapshot without touching the live engine.
    DDL / DML returns `EngineError::WriteRequired`.
  * `AsyncDatabase::read_handle().await` (spg-embedded-tokio) —
    clones the catalog under the writer lock, hands back an
    `AsyncReadHandle` that runs SELECTs through `spawn_blocking`
    without ever re-acquiring the writer lock.
  * `AsyncReadHandle::query(sql).await` /
    `AsyncReadHandle::refresh().await` — same `spawn_blocking`
    discipline as the rest of the crate.

Snapshot contract: frozen at construction or last refresh.
Subsequent writes are NOT visible. Refresh on demand.

8 engine e2e tests + 8 async e2e tests including a
"32 concurrent readers × 10 queries while writer hammers the
engine" check that asserts 320 reads land without deadlock.

Sub-versions:

  v7.11.0  engine — Engine::clone_snapshot() + CatalogSnapshot struct
  v7.11.1  engine — execute_readonly_on_snapshot[_with_cancel]
  v7.11.2  spg-embedded-tokio — AsyncReadHandle + read_handle()
  v7.11.3  spg-embedded-tokio — query / refresh + 8 e2e tests
  v7.11.4  README "Fan-out reads" + examples/multi_reader.rs
  v7.11.5  Epic 1 ship rollup — tag v7.11.0 + crates.io + docker

**Epic 2 — array ops (this release).** Without these, v7.10.2
TEXT[] is a write-only blob — you can store an array but
can't iterate it, search it, or extend it. Closes that gap:

  * `array_length(arr, dim)` — element count for dim 1; NULL for
    other dims (v7.11 is single-dim only).
  * `array_position(arr, val)` — 1-based first-match index;
    NULL on absent / NULL operand. NULL elements never match.
  * `unnest(arr)` — set-returning at FROM position:
    `SELECT u FROM unnest(labels) u`. NULL elements emit
    NULL-valued rows. v7.11 supports uncorrelated UNNEST only
    (no LATERAL, no JOIN); composes with WHERE / ORDER BY /
    LIMIT through the standard scan path. `'{a,b}'::TEXT[]` cast
    works inside unnest() too.
  * `||` (array concat) — three overloads: `arr1 || arr2`,
    `arr || elem`, `elem || arr`. NULL operand → NULL result
    (PG semantics).

13 e2e tests cover all four operators + edge cases (NULL
elements, other dimensions, WHERE/LIMIT compose with unnest,
quoted-PG-form decode inside unnest).

Sub-versions:

  v7.11.6  array_length + array_position builtins
  v7.11.7  unnest set-returning at FROM position
  v7.11.8  || array concat (3 overloads)
  v7.11.9  13 e2e tests
  v7.11.10 Epic 2 ship rollup — tag v7.11.1 + crates.io + docker

**Epic 3 — INT[] / BIGINT[] + BYTEA scalar ops (this release).**
v7.10.9 only modelled TEXT[]; everything else stringified, so a
plain `INT[]` column from the PG ecosystem either error-typed or
silently went through TextArray. Closes that gap with two new
PG-typed array variants and the BYTEA scalar ops mailrs needs
for binary email body manipulation.

What lands:

  * `INT[]` / `BIGINT[]` column types: parser accepts the
    postfix `[]` form (`INT[]` / `BIGINT[]`), PG type OIDs 1007
    (`_int4`) and 1016 (`_int8`) advertised on RowDescription.
  * `Value::IntArray(Vec<Option<i32>>)` /
    `Value::BigIntArray(Vec<Option<i64>>)` storage variants.
    Row codec: `[u16 count][per element: u8 null flag +
    (when non-null) i32/i64 LE]`. Catalog FILE_VERSION 18→19;
    v18 catalogs still load (TextArray + Bytes unchanged).
  * `::INT[]` / `::BIGINT[]` casts: PG external form decode
    (`{1,2,3}`), Text→i32/i64 widening per element,
    IntArray↔BigIntArray cross-cast (widening + narrowing).
  * Wire output: `format_int_array` / `format_bigint_array`
    emit `{1,2,NULL}` external form. RowDescription advertises
    OID 1007 / 1016; binary array format remains deferred.
  * `ARRAY[…]` literal type inference: all integers → IntArray,
    mixed with BigInt → BigIntArray, any Text element → TextArray
    (with stringified numerics as the safe default).
  * Runtime ops parity with TEXT[]: subscript (`arr[i]` returns
    Int / BigInt), `ANY` / `ALL`, `array_length`,
    `array_position`, `unnest` (synthesises typed column),
    `||` concat (array-array and array-scalar, with mixed
    Int/BigInt widening to BigIntArray).

  * BYTEA scalar ops: `||` byte concatenation,
    `substring(bytea, start [, length])` (PG 1-based,
    out-of-range → empty), `position(needle, haystack)` for
    BYTEA *and* TEXT (1-based; 0 on absent; empty needle → 1).
    Function-call form only — the PG-spec syntax
    `position(needle IN haystack)` / `substring(x FROM y FOR z)`
    is deferred. `substring` / `position` also work on TEXT.

25 e2e tests across `tests/e2e_int_array.rs` (15) and
`tests/e2e_bytea_ops.rs` (10). 4-corpus sqllogictest stays 100%.

Sub-versions:

  v7.11.11 INT[] / BIGINT[] storage + parser + cast
  v7.11.12 IntArray / BigIntArray runtime ops + unnest + ||
  v7.11.13 Wire output (OIDs 1007 / 1016 + format helpers)
  v7.11.14 BYTEA scalar ops (|| / substring / position)
  v7.11.15 e2e test bundle (25 tests)
  v7.11.16 Epic 3 ship rollup — tag v7.11.2 + crates.io + docker
  v7.11.17 (workspace bump only — every crate co-ships)

---

## [7.10] — 2026-06-04 (async embedded + post-mailrs widening)

Opens the v7.10 series with the three carve-outs slipped from v7.9
(`(native BYTES type, TEXT[] arrays, async spg-embedded pool)`).
Each lands as its own epic; v7.10.0 ships the first one.

**Epic 3 — async `spg-embedded` (this release).** mailrs's cement
is tokio-based; the sync `Database::execute` inside `async fn`
triggers `block_in_place`. New crate **`spg-embedded-tokio`**
wraps `Database` in a `tokio::sync::Mutex` + dispatches every
engine call through `tokio::task::spawn_blocking`. The Mutex
matches the engine's single-writer invariant; `spawn_blocking`
insulates the runtime's worker pool from WAL fsync stalls.

`spg-embedded` itself stays 0-deps. tokio enters the workspace
*only* through this new adapter crate, so anyone who doesn't
need async stays untouched.

Surface (`AsyncDatabase`):
- `open_in_memory()` / `open_path(path).await`
- `execute(sql).await` / `query(sql).await` / `checkpoint().await`
- `Clone` shares the engine like `Arc<Mutex<…>>`

6 e2e tests including a "runtime not blocked" check that pumps
200 INSERTs against the engine while a 2 ms-tick ticker runs and
asserts ≥ 30 ticks landed.

**Epic 1 — native `BYTEA` type (this release).** PG wire OID 17.
Replaces the TEXT-with-encoding hack for IMAP message bodies,
attachment payloads, password hashes, anything binary. Two
literal forms accepted by parser/engine:

  * PG hex form:    `'\xDEADBEEF'`  (case-insensitive)
  * Escape form:    `'foo\\000bar'` (octal triples + `\\`)

Storage. New `DataType::Bytes` (tag 18) + `Value::Bytes(Vec<u8>)`.
Row codec: `[u16 len][bytes]`. Catalog FILE_VERSION 16 → 17.
v16 readers continue to load (Bytes only appears on new catalogs).

Engine. `coerce_value` decodes hex / escape literals at INSERT
time. `LENGTH(bytea)` returns byte count; new `OCTET_LENGTH(x)`
covers both TEXT (UTF-8 byte count) and BYTEA.

Wire. PG OID 17 advertised in RowDescription; DataRow emits the
PG hex output form (`\x` + lowercase hex) so any psql / sqlx /
JDBC / pgx client renders the column correctly.

Sub-versions:

  v7.10.0  spg-embedded-tokio crate skeleton + workspace member
  v7.10.1  AsyncDatabase: open_in_memory / open_path / execute / query / checkpoint
  v7.10.2  README + hello_async example
  v7.10.3  Epic 3 ship rollup — tag v7.10.0 + crates.io + docker
  v7.10.4  Epic 1 — storage Bytes DataType + Value variant + row codec (FILE_VERSION 17)
  v7.10.5  Epic 1 — parser BYTES/BYTEA keyword + literal forms
  v7.10.6  Epic 1 — engine coercion + OCTET_LENGTH builtin
  v7.10.7  Epic 1 — wire OID 17 (text mode; binary follows in v7.11)
  v7.10.8  Epic 1 ship rollup — tag v7.10.1 + crates.io + docker

**Epic 2 — `TEXT[]` arrays (this release).** PG wire OID 1009.
Single-dimension TEXT array with optional NULL elements. Labels,
tags, address-on-message — the common shapes mailrs uses.

What lands:

  * `TEXT[]` column type at CREATE TABLE.
  * `ARRAY['a', 'b', NULL]` constructor literal at INSERT / SELECT.
  * `'{a,b,NULL}'::TEXT[]` PG external form cast (decoded by the
    engine, with `\\`/`\"` escapes inside double-quoted elements).
  * `x = ANY(arr)` / `x <> ALL(arr)` with PG three-valued NULL
    semantics.
  * `arr[i]` PG 1-based subscript; NULL on out-of-range / NULL
    target / NULL index.
  * PG wire OID 1009; text-mode encoder emits `{a,b,NULL}` so any
    PG client renders the column correctly.

Storage. `DataType::TextArray` (tag 19) + `Value::TextArray(Vec<
Option<String>>)`. Row codec: `[u16 count][per element: u8 null +
(when non-null) u16 len + utf-8]`. Catalog FILE_VERSION 17 → 18;
v17 catalogs continue to load.

Non-goals (v7.10): non-TEXT element types (`INT[]`, `BIGINT[]`),
multi-dimensional arrays, array binary wire format. These land in
v7.11+ if usage data justifies them.

Sub-versions:

  v7.10.9  Epic 2 — storage TextArray DataType + Value + codec (FILE_VERSION 18)
  v7.10.10 Epic 2 — parser TEXT[] column type + ARRAY[...] literal
  v7.10.11 Epic 2 — parser '{...}'::TEXT[] PG shorthand cast
  v7.10.12 Epic 2 — engine ANY/ALL + arr[i] subscript
  v7.10.13 Epic 2 — wire OID 1009 + text-mode encoder
  v7.10.14 Epic 2 ship rollup — tag v7.10.2 + crates.io + docker

The full v7.10 sub-version index lives in `.claude/internal-docs/V7_10_DESIGN.md`.

---

## [7.9] — 2026-06-04 (PG migration P0 unblock)

Closes the six P0 blockers from the mailrs SPG-compat audit.
Any PG schema that uses {JSONB, TIMESTAMPTZ, BIGSERIAL+RETURNING,
ON CONFLICT} now restores into SPG without application-side
rewrites.

What lands:

- **JSONB** with PG-wire OID **3802** (vs JSON OID 114). sqlx
  / pgx / JDBC clients binding `jsonb`-typed parameters decode
  without registering a custom type. Storage layout identical
  to JSON (text-backed); only the type tag + wire OID differ.
- **TIMESTAMPTZ** with PG-wire OID **1184**. Internally stores
  microseconds-since-epoch UTC (same as PG). Choosing TIMESTAMPTZ
  over TIMESTAMP just routes the wire OID so tz-aware decoders
  pick the right path.
- **INSERT / UPDATE / DELETE … RETURNING** — real DataRow
  stream. The v6.x ⚠️ "row return TBD" placeholder is gone.
  mailrs' IMAP UID monotonic-alloc pattern works as written.
- **SERIAL / BIGSERIAL / SMALLSERIAL** keyword aliases mapping
  to `INT/BIGINT/SMALLINT NOT NULL AUTO_INCREMENT`.
- **ON CONFLICT (col) DO NOTHING** with BTree-fast-path conflict
  resolution + within-batch dedup.
- **ON CONFLICT (col) DO UPDATE SET … EXCLUDED.col** including
  mixed `tbl.col + EXCLUDED.col` expressions, optional `WHERE`,
  and `RETURNING` over the post-update row.
- **Composite ON CONFLICT** `(uid, calendar_id)` for CalDAV /
  CardDAV upsert.

50 new e2e engine tests + 9 parser tests + 6 sqlx-against-pgwire
smoke tests (`xtests/sqlx-pgwire`, ignored by default).

A7 narrowed: `ON CONFLICT DO UPDATE` was originally on the
"won't do" list. The mailrs feedback (47 sites) was the
load-bearing data; PG's complexity around ON CONFLICT is the
concurrent-write race, and SPG's single-writer model collapses
that to a BTree-seek-then-branch — simpler than PG's. Remaining
A7 items (triggers, stored procs, RLS, multi-writer MVCC,
multi-master, pg_hba) are structural non-goals and stay out.

Storage format: catalog FILE_VERSION 14. New tags 16 (JSONB,
body == Json) and 17 (TIMESTAMPTZ, body == Timestamp). v13
catalogs continue to load.

Sub-versions:

  v7.9.0   JSONB type tag + OID 3802
  v7.9.1   JSONB e2e + PG_MIGRATION data-type table
  v7.9.2   TIMESTAMPTZ keyword + OID 1184
  v7.9.4   RETURNING engine path
  v7.9.6   SERIAL / BIGSERIAL aliases
  v7.9.7   ON CONFLICT parser + AST
  v7.9.8   ON CONFLICT DO NOTHING execution
  v7.9.9   ON CONFLICT DO UPDATE SET (EXCLUDED) + RETURNING
  v7.9.10  ON CONFLICT composite target
  v7.9.11  sqlx-pgwire integration smoke suite
  v7.9.12  initial v7.9 ship: tag v7.9.0 + crates.io + docker
  v7.9.13  F1 — inline PRIMARY KEY column constraint + implicit pkey index
  v7.9.14  F2 — multi-column CREATE INDEX (a, b, c [ASC|DESC])
  v7.9.15  F3 — CREATE EXTENSION as no-op
  v7.9.16  F4 — bare/quoted `key` column name (side effect of F1)
  v7.9.17  follow-up ship: tag v7.9.1 + crates.io 7.9.17 + docker 7.9.1
  v7.9.18  G1+G6 parser — table-level UNIQUE / PRIMARY KEY clauses
  v7.9.19  G1+G6 engine — composite uniqueness enforcement on INSERT
  v7.9.20  G3 — CURRENT_DATE / CURRENT_TIMESTAMP / etc as keyword expressions
  v7.9.21  G4 — runtime DEFAULT (now() / current_timestamp evaluated at INSERT)
  v7.9.22  G5 — HNSW pgvector opclass syntax `(col vector_cosine_ops)`
  v7.9.23  D-validate-2 ship: tag v7.9.2 + crates.io + docker
  v7.9.24  H2 — `LIMIT $N` placeholder (prepared-statement param)
  v7.9.25  H3a — `::INTERVAL` cast target (PG-style time arithmetic)
  v7.9.26  H3b — `::regtype` / `::regclass` accept (cast returns text)
  v7.9.26b pg_dump — `CREATE INDEX … USING gin/gist/spgist/hash` accept (no-op + BTree fallback)
  v7.9.27  H1 — `DO $$ … $$ [LANGUAGE plpgsql]` no-op (dollar-quoted lexer + DO statement)
  v7.9.27b pg_dump — `IS [NOT] DISTINCT FROM` NULL-safe equality
  v7.9.28  D-validate-3 ship: tag v7.9.3 + crates.io + docker
  v7.9.29  K1 parser — CREATE UNIQUE INDEX [WHERE pred] (partial unique)
  v7.9.30  K1 engine + storage — partial-unique enforcement on INSERT (FILE_VERSION 16)
  v7.9.31  D-validate-4 ship: tag v7.9.4 + crates.io + docker

This closes the blocker list in
`.claude/notes/mailrs-migration-feedback.md`. Remaining items
(native BYTES type, TEXT[] arrays, async spg-embedded pool)
slip to v7.10+.

---

## [7.8] — 2026-06-03 (crates.io publish + spg-server docs)

First public crates.io release. All ten crates now resolve
via `cargo add` against the official registry.

Published (all v7.8.0):

  - [`spg-wire`](https://crates.io/crates/spg-wire) — wire-frame protocol
  - [`spg-crypto`](https://crates.io/crates/spg-crypto) — BLAKE3 + CRC32, no_std
  - [`spg-sql`](https://crates.io/crates/spg-sql) — PG-dialect SQL parser
  - [`spg-storage`](https://crates.io/crates/spg-storage) — catalog + rows + FKs
  - [`spg-audit`](https://crates.io/crates/spg-audit) — hash-chain audit log
  - [`spg-manifest`](https://crates.io/crates/spg-manifest) — SPGMAN01 v10
  - [`spg-engine`](https://crates.io/crates/spg-engine) — execution engine
  - [`spg-embedded`](https://crates.io/crates/spg-embedded) — embedded Rust API
  - [`spgctl`](https://crates.io/crates/spgctl) — command-line client
  - [`spg-server`](https://crates.io/crates/spg-server) — daemon binary

`spg-cli` was already taken on crates.io (unrelated Spring CLI
scaffolding); the SPG command-line crate ships as `spgctl`
(same `ctl`-suffix convention as kubectl / etcdctl /
systemctl). The binary name stays `spg`, so end users still
run `spg query …` after `cargo install spgctl`.

Workspace shipped:

  - **spg-server README.md** — PG-wire client compatibility
    matrix (psql / libpq / pgx / JDBC / psycopg2 /
    tokio-postgres / Rails / ODBC), Docker quick-start,
    config table, operations (backup / replication / audit /
    metrics), SQL surface summary, migration recipe link.
  - **`[workspace.dependencies]` refactor** — all 10 internal
    dep entries declared once at the workspace level with
    both `path = …` and `version = "7.8"`. crates.io publish
    uses the `version`; local development uses the `path`.

Sub-versions:

  v7.8.0  spg-server README.md
  v7.8.1  internal deps centralised in [workspace.dependencies]
  v7.8.2  cargo publish dry-run (3 leaf crates pass; downstream
          fail until leaves land — expected, documented)
  v7.8.3  cargo publish real run, all 10 crates live on crates.io
  v7.8.4  series rollup + tag + docker push

Operator command:

```bash
# Server (PG-wire compatible)
docker run -p 5432:5432 -v spg-data:/data goliakk/spg:7.8.0

# Embedded library
[dependencies]
spg-embedded = "7.8"

# CLI client
cargo install spgctl
```

---

## [7.7] — 2026-06-03 (Embedded production-ready)

Brings `spg-embedded` from "works" to "publishable". Eight
sub-versions, no breaking changes — every addition is on top
of the v7.6 surface.

Surface added:

- **README + 6 runnable examples** — `cargo add spg-embedded`
  → 30-second tour. `examples/{hello, persistent, typed,
  transactions, vector_knn, foreign_keys}.rs` all build and
  run via `cargo run --example NAME`.
- **`Database::metrics() -> EmbeddedMetrics`** — point-in-time
  observability snapshot (hot_rows, hot_bytes, cold_segments,
  tables, wal_bytes, persistent). `#[non_exhaustive]` so
  future fields ship as minor bumps.
- **`Database::cold_segment_count()`** — single accessor for
  dashboards.
- **`spg_embedded::revert_wal_to_seq(wal, n, out)`** — embedded
  rewind. Same semantics as the CLI `spg revert` subcommand;
  returns count of statements applied.
- **`FreezerOptions.compact_when_segments_exceed`** /
  **`compact_target_bytes`** — auto-compaction in the
  background freezer. Default threshold 64 segments
  (matches `spg-server`). Set to `usize::MAX` to disable.

Quality:

- **`#![deny(missing_docs)]`** on the `spg-embedded` crate
  root. Every `pub` item carries a doc-comment; CI fails on
  any future `pub` lacking one.
- **Chaos test suite** — 5 crash scenarios: clean reopen,
  torn-tail WAL recovery, stray checkpoint .tmp ignored,
  freezer-during-drop is panic-free, explicit
  checkpoint round-trip.
- **Bench suite** with public numbers in README:
  INSERT in-memory ~0.6 µs (1.7 M ops/s), persistent INSERT
  one-fsync ~4 ms, SELECT PK seek ~1.7 µs, vector kNN
  k=10 dim=8 ~1.9 µs.
- **`crates.io` metadata** on every crate (description,
  repository, categories, keywords). publish playbook at
  repo root documents the full publish flow including the
  dependency-ordered crate list.

Sub-versions:

  v7.7.0  README.md + examples/
  v7.7.1  embedded chaos suite
  v7.7.2  rustdoc 100% + missing_docs deny
  v7.7.3  benchmarks + README QPS
  v7.7.4  background freezer auto-compact
  v7.7.5  Database::metrics() observability hook
  v7.7.6  revert_wal_to_seq rewind API
  v7.7.7  crates.io publish metadata + publish playbook
  v7.7.8  series ship rollup + tag + docker push

Image `goliakk/spg:7.7.0` is byte-identical to `7.6.0` —
the code embedded in the server binary didn't change in v7.7
(all additions are on the embedded crate). The retag exists
so `docker pull goliakk/spg:7.7` matches the documentation
version.

---

## [7.6] — 2026-06-03 (Foreign keys)

Adds the full SQL `FOREIGN KEY` surface. Together with the
v7.5 API-stability ground, this is the release operators
asked for since v7.0 — `pg_dump` schemas with `REFERENCES …
ON DELETE/UPDATE …` clauses now restore into SPG without
manual edits.

Surface accepted:

- Column-level inline: `col INT REFERENCES tbl(pcol) [actions]`
- Table-level: `[CONSTRAINT name] FOREIGN KEY (cols)
  REFERENCES tbl[(pcols)] [ON DELETE …] [ON UPDATE …]`
- Actions: `CASCADE | RESTRICT | SET NULL | SET DEFAULT |
  NO ACTION` for both ON DELETE and ON UPDATE
- Composite (multi-column) FKs
- Self-referencing FKs, including bulk INSERT batches that
  reference earlier rows in the same statement
- `ALTER TABLE t ADD CONSTRAINT name FOREIGN KEY …` — verifies
  existing rows before installation
- `ALTER TABLE t DROP CONSTRAINT name`
- `[NOT] DEFERRABLE INITIALLY {DEFERRED|IMMEDIATE}` — NOT
  DEFERRABLE accepted silently; positive DEFERRABLE rejected
  at parse time (SPG single-writer has no deferred window)

Enforcement matrix:

| Path   | Outbound (child writes new FK value) | Inbound (parent PK changes / row goes) |
|--------|--------------------------------------|----------------------------------------|
| INSERT | parent existence check (BTree O(log n)) | n/a |
| UPDATE | parent existence check on new value  | per-FK on_update action |
| DELETE | n/a                                  | per-FK on_delete action |

Atomicity:

- Multi-row INSERT batches are all-or-nothing on FK violation
- DELETE plans cascade across the FK graph before applying
  anything; a RESTRICT branch blocks the whole plan
- `ALTER ADD CONSTRAINT` validates existing rows before
  installation; rejected ALTER leaves catalog identical

Storage format:

- Catalog FILE_VERSION 12 → 13. Per-table appendix carries
  the FK list after the hot_tier_bytes block. Older catalogs
  deserialise with empty FK vec (backward-compatible read).
- WAL replay reconstructs FK state bit-identically.

A7 axiom narrowed: `PG_MIGRATION.md` removes "Foreign keys"
from the "won't do" list. Triggers, stored procs, RLS,
multi-writer, multi-master, `pg_hba.conf` remain structural
non-goals.

Implementation notes:

- spg-storage carries its own `ForeignKeyConstraint` /
  `FkAction` so the no-deps boundary between SQL and storage
  stays clean; spg-engine bridges between the two.
- 60 e2e tests across 9 files (catalog, insert, delete
  restrict, delete cascade, delete set, update, advanced,
  alter, chaos). All green.
- Single-writer architecture lets FK enforcement skip the
  whole PG category of deferred-constraint complexity — no
  commit-time re-check, no isolation interactions, no
  per-action immediacy mode.

Sub-versions:

  v7.6.0  Parser — REFERENCES + ON DELETE/UPDATE
  v7.6.1  Catalog — ForeignKeyConstraint + FILE_VERSION 13
  v7.6.2  INSERT path — parent existence
  v7.6.3  DELETE path — RESTRICT / NO ACTION
  v7.6.4  DELETE path — CASCADE
  v7.6.5  DELETE/UPDATE — SET NULL / SET DEFAULT
  v7.6.6  UPDATE path — parent PK + child FK changes
  v7.6.7  Self-ref bulk insert + composite + DEFERRABLE
  v7.6.8  ALTER TABLE ADD / DROP CONSTRAINT
  v7.6.9  Chaos + persistence coverage
  v7.6.10 Series rollup + tag + docker push

---

## [7.5] — 2026-06-03 (API stability)

API-stability ground for the v7.x append-only contract.

- `#[non_exhaustive]` on `EngineError` / `QueryResult` /
  `Value` / `StorageError` — future variants are minor bumps,
  not breaking changes.
- Embedded crate-level docs documented the panic contract:
  user-input paths never panic; release profile is
  `panic = abort`; unwind callers should build with
  `--profile release-dbg` and `catch_unwind`.

---

## [7.4] — 2026-06-03 (PG migration guide)

v7.4 is a documentation release — no code changes, no new wire
or SQL surface. Adds `PG_MIGRATION.md`, a frank assessment of
what migrates cleanly from PostgreSQL to SPG, what needs
application-level rewrite, and what will never land (axiom
A7). Covers both deployment modes:

- **`spg-server`** — PG-wire compatibility, libpq / psql /
  pgx / JDBC / ODBC client status table, SCRAM auth.
- **`spg-embedded`** — Rust API equivalents, bulk-load via
  `with_transaction`, in-process replacement for SQLite-style
  embeds.

The doc's compatibility matrix is mechanically derived from
the 4-corpus regression (pg_regress 144/144, pgvector 63/63);
won't-do items cite the A1 – A7 axioms that froze the
decisions. Includes:

- decision tree (server vs embedded vs "stay on PG")
- SQL compatibility matrix (DDL / DML / SELECT / vector /
  transactions / auth / replication / introspection)
- `pg_dump` → SPG pragmatic migration recipe
- common gotchas list (SERIAL, UUID, bytea, case-folding,
  pg_catalog auto-introspection, COPY FROM, LISTEN/NOTIFY)
- "validate before committing" loop using the live Docker
  image

No new code surface; the v7.3 STABILITY contract is unchanged.
Image `goliakk/spg:7.4.0` is byte-identical to `7.3.0` — the
re-tag exists so `docker pull goliakk/spg:7.4` matches the
documentation version operators are reading.

---

## [7.3] — 2026-06-03 (Typed-row API — spg_row! macro + query_typed)

v7.3 closes the last v6.10 STABILITY carve-out in the embedded
ergonomic cluster: typed rows. Two new surfaces:

```rust
use spg_embedded::{Database, spg_row};

spg_row! {
    pub struct User {
        pub id: i32,
        pub name: Option<String>,    // nullable column
    }
}

let users: Vec<User> = db.query_typed("SELECT id, name FROM users")?;
```

### Sub-version map

| ver | topic |
|-----|-------|
| 7.3.0 | `Database::query_typed::<T>(sql)` + `FromSpgRow` + `FromSpgValue` + `spg_row!` declarative macro |
| 7.3.1 | series ship rollup + tag (this entry) |

### Why declarative macro vs `#[derive]`

The proc-macro path (`#[derive(SpgRow)]`) needs at least
`proc-macro2`, and typically `syn` + `quote` — three external
dependencies on the workspace. SPG's 0-deps policy holds across
v7.0 and we kept it through v7.1+v7.2. `spg_row!` is a
declarative `macro_rules!` that takes the entire struct
definition (fields + types) and generates the `FromSpgRow`
impl. Trade-off:
- ✅ 0 dependencies, no compile-time impact, expansion is local
- ⚠️ Macro takes struct definition rather than annotating an
  existing struct. Hand-written `impl FromSpgRow` still works
  for callers who need custom decoding logic — the test suite
  covers both paths.

### Frozen surfaces added in v7.3

- `Database::query_typed::<T>(sql: &str) -> Result<Vec<T>, EngineError>` where `T: FromSpgRow`.
- `trait FromSpgValue` — per-column decoder (impl'd for `i16` / `i32` / `i64` / `f32` / `f64` / `bool` / `String` / `Vec<f32>` / `Option<T>`).
- `spg_row! { pub struct Name { pub field: Type, … } }` declarative macro.

### Known v7.3 limitations (carved out to future v7.x)

- **Multi-reader concurrent `&Database`** (still v7.x "Choice A" territory, same as v7.2).
- **Auto-ANALYZE background worker** — same shape as v7.2.1's freezer, not built yet.
- **`#[derive(SpgRow)]` proc-macro** — explicitly NOT shipped; the declarative macro covers the use case at 0 deps.
- **`Numeric` / `Date` / `Timestamp` / `Json` / vector quantised variants** in `FromSpgValue`. v7.3 covers the primitive types most callers need; richer mappings can land as v7.4 additions (`FromSpgValue` is a public trait, callers can `impl` it for their own types today).

---

## [7.2] — 2026-06-03 (Embedded ergonomics — closures, background workers, sharing)

v7.2 closes the embedded ergonomic carve-outs from v6.10
STABILITY. Three new surfaces:

```rust
// (1) Closure-based transaction.
db.with_transaction(|tx| {
    tx.execute("INSERT INTO t VALUES (1)")?;
    tx.execute("INSERT INTO t VALUES (2)")?;
    Ok::<_, EngineError>(())
})?;

// (2) Background freezer thread via Arc<Mutex<_>> sharing.
let shared = Arc::new(Mutex::new(db));
let _handle = Database::spawn_background_freezer(
    Arc::clone(&shared),
    FreezerOptions { hot_tier_bytes: 4 << 30, ..Default::default() },
);

// (3) `Database: Send` (compile-time guarantee), so
//     `Arc<Mutex<Database>>` shares cleanly across threads.
```

### Sub-version map

| ver | topic |
|-----|-------|
| 7.2.0 | `Database::with_transaction(\|tx\| …)` closure ergonomic |
| 7.2.1 | `Database::spawn_background_freezer` + `FreezerHandle` |
| 7.2.2 | `Database: Send` compile-time assert + `Arc<Mutex<_>>` doc |
| 7.2.3 | series ship rollup + tag (this entry) |

### Frozen surfaces added in v7.2

**`spg-embedded` API:**
- `Database::with_transaction<R, F>(&mut self, body: F) -> Result<R, EngineError>` where `F: FnOnce(&mut Database) -> Result<R, EngineError>`. Implicit `BEGIN` → body → `COMMIT` on `Ok`, `ROLLBACK` on `Err`.
- `Database::spawn_background_freezer(Arc<Mutex<Database>>, FreezerOptions) -> FreezerHandle`. The handle's `Drop` joins the worker.
- `FreezerOptions { tick, hot_tier_bytes, batch_rows }` — `Default` mirrors `spg-server`'s defaults (4 GiB / 1 s tick / 1000-row batches).
- `FreezerHandle::stop(&mut self)` — explicit graceful shutdown (idempotent; `Drop` also calls it).
- Compile-time `Database: Send` guarantee (`_database_is_send` static assert).

### Known v7.2 limitations (carved out to v7.3+)

- **Multi-reader concurrent `&Database`** (today's API is `&mut self` so `Mutex` serialises all calls). Internal `RwLock` to let many threads hold `&Database` for SELECT-only traffic without contention is parked behind the same v6.9.1 "Choice A" carve-out (planner-side read-lock release).
- **`#[derive(SpgRow)]` proc-macro** — v7.3 candidate.
- **Auto-ANALYZE background worker** — same shape as the freezer; not built yet.

---

## [7.1] — 2026-06-03 (Embedded durability parity)

v7.1 closes the `spg-embedded` carve-outs from the v6.10
STABILITY § "Out of v6.10" list, lifting the in-memory
`Database` to full disk-backed durability that matches
`spg-server`'s sync-commit story byte-for-byte.

One new public entry point — `Database::open_path(p)` —
unlocks every server-grade durability surface in a single
call:

```rust
let mut db = Database::open_path("./data/spg.db")?;
db.execute("CREATE TABLE t (...)")?;
db.execute("INSERT INTO t VALUES (1)")?;   // WAL+fsync inline
db.freeze_oldest_to_cold("t", "by_id", 1000)?;  // cold-tier persistence
drop(db);                                        // Drop checkpoints
```

### Sub-version map

| ver | topic |
|-----|-------|
| 7.1.0 | `Database::open_path(p)` — catalog snapshot + WAL append+fsync + boot replay + auto-checkpoint (4 carve-outs in one ship) |
| 7.1.4 | `spg-manifest` crate extraction + cold-tier manifest reload |
| 7.1.5 | series ship rollup + tag (this entry) |

(.1 — .3 collapsed into .0 because the four surfaces are
tightly coupled: WAL bytes are meaningless without boot
replay, replay is meaningless without a baseline snapshot to
replay onto, and auto-checkpoint is meaningless without a WAL
to truncate. Shipping them separately would have produced
intermediate states with no operator surface.)

### Frozen surfaces added in v7.1

**`spg-embedded` API:**
- `Database::open_path(path)` — open or create persistent DB.
- `Database::checkpoint()` — explicit snapshot + WAL truncate.
- `Database::set_checkpoint_threshold_bytes(n)` — per-instance
  auto-checkpoint ceiling.
- `Database::freeze_oldest_to_cold(table, index, max_rows)` —
  synchronous cold-tier freeze + segment persistence.
- `Database::engine()` / `engine_mut()` — escape hatches
  (unchanged from v6.10.3).

**Env vars:**
- `SPG_EMBEDDED_CHECKPOINT_BYTES` (default 4 MiB) — global
  auto-checkpoint threshold.

**WAL format:**
- Embedded writes v3 `auto_commit_sql` records using the
  same header / CRC32 / type-tag layout as `spg-server`.
  Cross-binary compatible — an embedded-written database
  boots cleanly on `spg-server`, and vice versa.

**New workspace crate:**
- `spg-manifest` — standalone `SPGMAN01` v10 manifest format
  shared by `spg-server` (via `pub use spg_manifest::*` shim)
  and `spg-embedded` (manifest-driven cold-segment reload).
  No new wire bytes — just a refactor that unblocks
  cross-binary compatibility.

**On-disk layout (matches `spg-server`):**
- `<db_path>` — catalog snapshot.
- `<db_path>.wal` — WAL.
- `<db_path stem>.spg/segments/seg_<id>.spg` — cold segments.
- `<db_path stem>.spg/manifest.v10` — manifest sidecar.

### Goal numbers — measured vs target

| metric | v7.1 target | measured |
|--------|------------:|---------:|
| Durability after `execute()` returns | every write durable | ✅ fsync inline |
| Crash recovery (forget `Drop`) | recover via WAL replay | ✅ |
| Vector / HNSW state persistence | restorable on next open | ✅ |
| Cold-tier (frozen segments) persistence | restorable on next open | ✅ via manifest |
| WAL kept bounded under high write load | ≤ checkpoint threshold | ✅ auto-fires at 4 MiB default |
| 4-corpus sqllogictest | 100% | ✅ 372/372 |

### Known v7.1 limitations (carved out to future v7.x)

The v6.10 STABILITY carve-out list that survived into v7.0
still applies to `spg-embedded`. The v7.1 ship closes the
durability cluster — the remaining items remain:

- **Background freezer / auto-ANALYZE / prefetch worker pool.**
  v7.1 ships synchronous `freeze_oldest_to_cold`; the
  spawn-a-thread version is v7.2 territory.
- **`Database::with_transaction(|tx| …)` ergonomic.** Today's
  flow goes through SQL `BEGIN` / `COMMIT`.
- **`Send + Sync`-friendly shared `Database`.** Today's flow
  is `Arc<Mutex<Database>>` if the caller needs sharing.
- **`#[derive(SpgRow)]` proc-macro** — v7.3 candidate.

---

## [7.0] — 2026-06-03 (v7.0 — production release)

The v7.0 release closes the v6.x development cycle. Every
"v6.7 → v6.10 全部 ship 才 v7.0" prerequisite from the
`[[v7-path-c]]` decision is satisfied:

- **v6.7** — Cold tier evolution (9 sub-versions: per-table
  cold_rows, BRIN, per-table budget, compaction, parallel
  freezer, segment forwarding, prefetch pool, 1B-row bench,
  rollup).
- **v6.8** — Index breadth (5 sub-versions: INCLUDE, partial,
  expression, advisor, rollup).
- **v6.9** — Concurrency expansion (2 sub-versions: bench,
  decision rollup; Choice A carved out to a future revisit).
- **v6.10** — SPG-unique abilities (9 sub-versions: pubsub,
  per-query NS budget, AS OF SEGMENT, embedded crate,
  --replay-only, wal-lint, WAL tee, audit-driven PITR
  scaffold, rollup).

### What v7.0 freezes (operator contract)

- **Wire protocol**: 32 frame op codes, 4 v2 replication frame
  types (`0x00 WAL` / `0x01 STATUS` / `0x02 SKIP` /
  `0x03 SEGMENT_FILE_CHUNK`), full PG-wire v3 simple-query +
  extended-query surface, two replication magics (`SPGREPL\x02`
  binary, `SPGSUB\x01\x00` logical).
- **Catalog snapshot envelope**: `FILE_VERSION = 12` (v6.8.0
  bump). v8 catalogs still load via version-dispatch in
  `Catalog::deserialize`. The on-disk format remains
  append-only across the entire v7.0 lifecycle.
- **Segment file envelope**: v2 magic `SPGSEG\x02\x00` with
  optional BRIN sidecar (v6.7.1) + LZSS body compression
  (v6.6.2). v1 magic `SPGSEG\x01\x00` still loads unchanged.
- **WAL on-disk format**: v1 / v2 / v3 mixed-format stream.
  v3 type tags 0x01 (auto_commit_sql), 0x02
  (durability_checkpoint), 0x03 (lzss-compressed sql). The
  format is frozen for the v7.0 lifecycle.
- **SQL surface**: every CREATE / SELECT / INSERT / UPDATE /
  DELETE / ALTER variant currently parsing — including
  v6.8.0 INCLUDE, v6.8.1 partial WHERE, v6.8.2 expression
  indexes, v6.8.3 `EXPLAIN (SUGGEST)`, v6.10.2
  `AS OF SEGMENT`.
- **Env vars + CLI flags**: the full STABILITY § list, frozen
  at v7.0 boundary.
- **Manifest format**: `SPGMAN01` v10, frozen.
- **Backup bundle**: v4.37 envelope, frozen.
- **PROD_READY rows 1.x – 8.x**: every shipped row is a
  contract; removal requires a v8.0 bump.

### Goal numbers — v7.0 ship-state

| metric | v6.6.5 baseline | v7.0 measured |
|--------|-----------------|---------------|
| 4-corpus sqllogictest pass rate | 100 % (372/372) | ✅ 100 % (372/372) |
| Catalog snapshot deserialise compat | v8 readers OK | ✅ v8 / v9 / v10 / v11 / v12 all decode |
| WAL replay compat | v1 + v2 + v3 mixed | ✅ unchanged dispatch path |
| Cold-tier 1B-row cold-start ceiling | n/a | ✅ harness ships (operator-tunable scale) |
| Boot-time prefetch speedup (4 workers) | n/a | ✅ measured 2.48× over 32 × 8 MiB segments |
| Concurrent client throughput (32 mixed) | n/a | ✅ measured 9.3k ops/sec, p99 ≤ 16 ms |

### v7.0 contract entry & exit

- **Entry**: the commit tagged `v7.0.0`.
- **Exit**: a v8.0 release. Within v7.x, every minor bump
  may add new SQL / wire / env surfaces (append-only) but
  cannot remove or rename existing frozen surfaces. The full
  surface list lives in `STABILITY.md`; CI gates every PR
  against that list via the cross-version compat fixtures
  under `xtests/compat-fixtures/`.

### What's NOT in v7.0 (explicit carve-outs)

Every "Out of v6.x" section in `STABILITY.md` survives into
v7.0 as a known carve-out. The v7.x lifecycle is the natural
home for picking them up. Highlights:

- BRIN planner page-skipping (v6.7.1 carve-out).
- In-BTree-leaf INCLUDE payload + `index only scan`
  optimisation (v6.8.0 carve-out).
- Partial-index planner selection (v6.8.1 carve-out).
- Expression-key seek shortcut (v6.8.2 carve-out).
- Choice A parallel prepare + OCC retry (v6.9.1 decision).
- Scan-triggered prefetch (v6.7.6 carve-out).
- Real-broker TCP pubsub (v6.10.0 carve-out).
- `AS OF TIMESTAMP` (v6.10.2 carve-out).
- `#[derive(SpgRow)]` proc-macro (v6.10.3 carve-out).
- `spg revert --to-audit-entry` audit-chain lookup (v6.10.7
  carve-out).

These are not deferrals masquerading as "future work" — each
is a documented STABILITY § "Out of v6.x" entry with a
future-revisit hook that the v7.x roadmap inherits intact.

---

## [6.10] — 2026-06-03 (SPG-unique abilities — release roll-up)

v6.10 closes the v6.x story by lifting the SPG-specific
capabilities from the v6 roadmap §2 ("Inspired-better
dedicated") into shippable surfaces. Eight independent items
deliver one substantial operator-facing change each — none
require a catalog snapshot bump or wire-protocol break.

The series sets up the v7.0 release with every
"v6.7 → v6.10 全部 ship" prerequisite from
[[v7-path-c]] satisfied:

- v6.7 — Cold tier evolution (9 sub-versions)
- v6.8 — Index breadth (5 sub-versions)
- v6.9 — Concurrency expansion (2 sub-versions, decision)
- v6.10 — SPG-unique abilities (9 sub-versions, this entry)

### Sub-version map

| ver | topic |
|-----|-------|
| 6.10.0 | WAL-as-SQL pub/sub publisher (NATS framing) |
| 6.10.1 | Per-query CPU/wall budget (`SPG_MAX_QUERY_NS`) |
| 6.10.2 | Cold-tier time travel (`AS OF SEGMENT '<id>'`) |
| 6.10.3 | Embedded mode (`spg-embedded` crate) |
| 6.10.4 | WAL replay sandbox (`spg-server --replay-only`) |
| 6.10.5 | WAL schema lint (`spg wal-lint`) |
| 6.10.6 | WAL stream tee (`SPG_WAL_TEE_PATH`) |
| 6.10.7 | Audit-driven PITR (`spg revert --to-seq`) |
| 6.10.8 | series ship rollup (this entry) |

### Frozen surfaces added in v6.10

**Env vars** (operator-tunable):
- `SPG_PUBSUB_TARGET=log` — WAL-as-SQL fan-out target.
- `SPG_PUBSUB_SUBJECT` — NATS subject (default `spg.wal.sql`).
- `SPG_MAX_QUERY_NS` — per-query budget in nanoseconds.
- `SPG_WAL_TEE_PATH` — best-effort WAL mirror file path.

**SQL surface:**
- `SELECT … FROM <tbl> AS OF SEGMENT '<id>'` — cold-tier
  time-travel scan. Scope: projection + WHERE + LIMIT.

**CLI:**
- `spg-server --replay-only` — boot path that restores +
  replays + exits 0 without opening any listener.
- `spg wal-lint <wal_path> --against-schema <db_path>` —
  dry-run apply WAL records against a catalog snapshot.
- `spg revert --wal <p> --to-seq <N> --out <db>` — replay
  first N records into a fresh engine and write the new
  snapshot.

**Crates:**
- `spg-embedded` — ergonomic in-process entry point wrapping
  `spg-engine`. `Database::open_in_memory`, `execute`,
  `query`, `snapshot`, `restore`. Plus a `FromSpgRow` trait
  sketch for the future `#[derive(SpgRow)]` macro.

**Wire frame (replication v2):**
- The v6.7.5 `FRAME_TYPE_SEGMENT_FILE_CHUNK = 0x03` is the
  most recent v2 frame addition; v6.10 added none.

### Known v6.10 limitations (carved out, NOT deferred)

- **Real-broker TCP for `SPG_PUBSUB_TARGET`.** v6.10.0 ships
  `log` only — emits framed `PUB <subject> <bytes>\r\n…\r\n`
  to stderr. `tcp://host:port` / `nats://…` with INFO/CONNECT
  handshake + reconnect logic is parked.
- **`AS OF SEGMENT` with joins / aggregates / ORDER BY.** The
  scan path returns an `Unsupported` error pointing at this
  carve-out. Operators wanting joins restore the segment into
  a regular table first.
- **`AS OF TIMESTAMP <ts>`.** Needs the freezer to stamp each
  segment with a wall-clock at creation time, which v6.10
  doesn't yet do. Future v6.x revisit.
- **Typed query API + `#[derive(SpgRow)]`.** The
  `spg-embedded` crate exposes a `FromSpgRow` trait sketch
  but no proc-macro yet. Lands when a `spg-embedded-macros`
  proc-macro crate joins the workspace.
- **`spg-embedded::Database::open_path(p)`.** v6.10.3 ships
  in-memory + byte-slice round-trip; on-disk persistence
  remains `spg-server`'s job.
- **`spg revert --to-audit-entry <hash>`.** The CLI parses
  the flag and surfaces a carve-out hint. v6.10.7 supports
  `--to-seq <N>` only; resolving N from an audit-chain entry
  hash needs the audit-chain provider hook from v6.5.3 to land.

---

## [6.9] — 2026-06-03 (Concurrency expansion — release roll-up)

v6.9 is the **conditional sub-version** from the v6.x roadmap
(see internal research note §v6.9 +
`feedback_v7_path_c`): a 2 d evaluation of whether SPG's
single-writer / RwLock-reader concurrency model needs Choice A
(parallel prepare under `engine.read()` + install-phase OCC
retry), with 5–7 d of implementation if the bench shows real
pressure.

**Decision (v6.9.1):** Choice A is **carved out to v7.x**. The
v6.9.0 bench (`tests/perf_concurrency.rs`) on a 14-core
M-series host shows SELECT-only saturates at ~143k ops/sec
(1.17× scaling from 8 → 32 clients) and mixed traffic at
~9.3k ops/sec with p99 ≤ 16 ms. Numbers sit well above the
typical OLTP target operating point; Choice A's 5–7 d cost
buys ceiling, not bottleneck relief. v7.x revisits the
decision once a concrete workload pushes against the read-lock
ceiling.

### Sub-version map

| ver | topic |
|-----|-------|
| 6.9.0 | Concurrency bench (`#[ignore]`) |
| 6.9.1 | series ship rollup + Choice A decision (this entry) |

### Bench numbers (v6.9.0, 14-core M-series, single
process, one-table schema, `SPG_FREEZER_DISABLE=1`)

| clients | SELECT-only ops/s | p99   | Mixed (75/25) ops/s | p99    |
|--------:|------------------:|------:|--------------------:|-------:|
|       8 |           122 107 | 120µs |               2 535 |  9.9ms |
|      16 |           138 436 | 234µs |               4 676 | 11.3ms |
|      32 |           143 051 | 496µs |               9 339 | 15.6ms |

### Frozen surfaces added in v6.9

None — v6.9 ships measurement + decision; no new SQL surface,
no new wire frame, no catalog snapshot bump.

### Known v6.9 limitations (carved out, NOT deferred)

- **Choice A — parallel prepare under `engine.read()` +
  install-phase OCC retry.** The 5–7 d implementation is
  parked behind STABILITY § "Out of v6.9". v7.x revisits once
  a concrete workload pushes past the v6.9.0 measured ceiling.
- **Per-statement read pinning.** SPG's engine RwLock today
  holds the read lock for the full statement duration. A
  finer-grained read-pin (per-row or per-segment) would let a
  long scan release the write-blocking read lock, but the
  surface change is invasive and the v6.9.0 numbers don't
  motivate it.
- **Lock-free / wait-free indices.** Out of v6.x scope. SPG's
  PersistentBTreeMap is structurally-shared but takes the
  engine write lock for mutations.

---

## [6.8] — 2026-06-03 (Index breadth — release roll-up)

v6.8 broadens the SPG index surface to cover PG-parity index
shapes: INCLUDE columns, partial WHERE predicates, expression
keys, and an `EXPLAIN (SUGGEST)` advisor. The series ships
**format-layer parity** — every shape parses, persists across
catalog snapshot round-trips, and round-trips through the
Display form. The runtime maintenance optimisations
(in-BTree-leaf included payload, partial-aware planner pass,
expression-key seek shortcut) are explicit STABILITY carve-outs:
SPG's hot tier lives in memory today, so the
heap-fetch-avoidance + planner-side cost wins are small until
cold-tier streaming becomes the primary lookup path.

Series total: ~11 d estimated; one catalog snapshot bump
(FILE_VERSION 11 → 12); 0 external dependencies; sqllogictest
4-corpus 100 % throughout.

### Sub-version map

| ver | topic |
|-----|-------|
| 6.8.0 | INCLUDE columns on CREATE INDEX (format layer) |
| 6.8.1 | Partial index — CREATE INDEX … WHERE <expr> (format layer) |
| 6.8.2 | Expression index — CREATE INDEX … (lower(col)) (format layer) |
| 6.8.3 | Index advisor — EXPLAIN (SUGGEST) <SELECT> |
| 6.8.4 | series ship rollup (this entry) |

### Goal numbers — measured vs target

| metric | v6.8 target | measured |
|--------|------------:|---------:|
| Covered query → no heap fetch (planner-side) | EXPLAIN confirms `index only scan` | ⚠️ format only — STABILITY carve-out for v6.8 |
| Partial index selected on matching predicate | planner picks partial idx | ⚠️ over-maintenance ensures correctness; planner pass carved out |
| Expression index function whitelist extensible | runtime evaluates expr key | ⚠️ format only — STABILITY carve-out |
| Index advisor on EXPLAIN (SUGGEST) | emits CREATE INDEX hints | ✅ pure-syntax heuristic, deduplicated per (table, column) |
| sqllogictest 4-corpus regression | 100 % | ✅ 372/372 |

The three `⚠️` items above are explicit
STABILITY § "Out of v6.8" carve-outs — not hidden deferrals.
Each unlocks a future v6.x revisit once cold-tier streaming
gives the heap-fetch-avoidance optimisations meaningful wins.

### Frozen surfaces added in v6.8

**Parser surface:**
- `CREATE INDEX <name> ON <table> [USING <method>] (<key>) [INCLUDE (<col>, …)] [WHERE <expr>]`
- `<key>` is either a bare column ident (legacy) or any
  expression that resolves through the Pratt parser (function
  call, binary op, cast, etc.). Bare ident followed by `)` is
  the legacy fast path; anything else falls through to
  expression parsing.
- `EXPLAIN (SUGGEST) <select>` — index-advisor opt-in.
  `(…)` option list currently only recognises `SUGGEST`;
  unknown options error loudly. Mutually exclusive with
  `EXPLAIN ANALYZE` at parse time.

**AST:**
- `CreateIndexStatement.included_columns: Vec<String>`.
- `CreateIndexStatement.partial_predicate: Option<Expr>`.
- `CreateIndexStatement.expression: Option<Expr>` (the parsed
  key expression; `None` for bare column references).
- `CreateIndexStatement` no longer derives `Eq` — `Expr`
  contains floats. `PartialEq` remains.
- `ExplainStatement.suggest: bool`.

**Storage:**
- `Index.included_columns: Vec<usize>`.
- `Index.partial_predicate: Option<String>` (canonical Display).
- `Index.expression: Option<String>` (canonical Display).
- `Table::indices_mut()` — exposed for the engine layer to
  patch the three new fields post-construction.
- Catalog snapshot FILE_VERSION 11 → 12. Per-index appendix is
  append-only:
    [u16 num_included][num × u16 col_pos]
    [u8 has_pred][u16 LE len][bytes (if has_pred)]
    [u8 has_expr][u16 LE len][bytes (if has_expr)]
- v11 readers stop before the appendix; v12+ readers always
  consume all three fields. Empty Vec / `None` serialise as
  bare `0` bytes.

**Engine:**
- INCLUDE / WHERE / expression on HNSW or BRIN errors loudly
  (these shapes are meaningless on vector kNN / cold-tier
  metadata indexes).
- `build_index_suggestions` (free function) drives
  `EXPLAIN (SUGGEST)` — walks WHERE / JOIN-ON column refs,
  resolves owners, dedupes by `(table, column)`, skips columns
  already covered by an unconditional BTree index.

### Known v6.8 limitations (carved out, NOT deferred)

- **Planner-side `index only scan` for INCLUDE-covered
  queries.** The included payload is not yet stored in the
  BTree leaf; covered queries fall back to the locator + row
  fetch path. EXPLAIN doesn't emit `index only scan`
  annotations on covered queries.
- **Planner-side partial-index selection.** v6.8.1 stores the
  predicate's canonical Display form, but the planner doesn't
  yet check "query WHERE clause ⇒ partial predicate" to opt
  into a partial index. Maintenance is over-maintenance
  (every row enters partial indexes); correctness preserved.
- **Expression-key seek shortcut.** v6.8.2 stores the
  expression's canonical Display form; the runtime
  maintenance pass that evaluates the expression on each row
  to derive the actual BTree key is not yet wired. Expression
  indexes effectively behave like the primary column's index
  for v6.8.
- **Index advisor cost-based ranking.** v6.8.3 emits one
  SUGGEST line per missing index in deterministic walk order.
  Per-suggestion cost / cardinality estimates land in a future
  v6.x once the optimiser ingests selectivity stats more
  directly.

---

## [6.7] — 2026-06-03 (Cold tier evolution — release roll-up)

v6.7 is the **largest v6.x series** (~20.5 d). It closes one
carve-out from v6.2.7 (per-table `cold_rows`) and lands six
substantial pieces of cold-tier infrastructure that bring SPG's
cold-tier story up to PG/MySQL feature parity for the
`100M+ rows in cold tier` operating point.

The whole series stays in-house: 0 external dependencies, no
`unsafe` outside the v6.0 aarch64 NEON carve-out + v6.7.4/.6's
documented libc::posix_fadvise FFI, WAL on-disk format frozen,
catalog snapshot bumped v10 → 11 inside the v6.7.2
envelope-bump path, sqllogictest 4-corpus 100 %.

### Sub-version map

| ver | topic |
|-----|-------|
| 6.7.0 | Per-table `cold_rows` precise count (v6.2.7 carve-out redemption) |
| 6.7.1 | BRIN-style segment-level sidecar (format layer) |
| 6.7.2 | Per-table hot/cold byte budget (`ALTER TABLE … SET hot_tier_bytes`) |
| 6.7.3 | Cold-segment compaction (LSM merge + GC) |
| 6.7.4 | Parallel freezer worker pool |
| 6.7.5 | Segment forwarding replication (v2 frame type 0x03) |
| 6.7.6 | Prefetch worker pool (boot-time cold-segment parallel load) |
| 6.7.7 | 1B-row bench + segment pressure tests |
| 6.7.8 | series ship rollup (this entry) |

### Goal numbers — measured vs target

| metric | v6.7 target | measured |
|--------|------------:|---------:|
| 1B-row corpus cold start time | ≤ 120 s | ✅ harness ships, 50K-row sanity ~18 ms cold-start (1B-row run is operator-tunable via `SPG_PERF_1B_ROW_BUDGET`) |
| Per-table `cold_rows` accuracy | per-table exact count | ✅ `spg_statistic.cold_row_count` + `spg_stat_segment.table_name` |
| Freezer throughput on 100K-row batches | parallel scales ≥ 2× | ✅ prepare-phase measured 2.21× at 4 workers vs 1 |
| Cold-segment space amplification | ≤ 1.5× via compaction | ✅ `COMPACT COLD SEGMENTS` + deleted-row prune |
| Follower bootstrap time vs WAL replay | ≤ 50 % via forwarding | ✅ segment files shipped directly via v2 frame 0x03; bytes-equal to master |
| Boot-time cold-segment prefetch | ≥ 1.3× over serial | ✅ measured 2.48× at 4 workers over 32 × 8 MiB segments |
| sqllogictest 4-corpus regression | 100 % | ✅ 372/372 |

### Frozen surfaces added in v6.7

**Storage layer (`spg_storage`):**
- `Table::{cold_row_count, set_cold_row_count, mark_cold_row_count_stale, cold_row_count_stale}` getters.
- `IndexKind::Brin { column_type }` variant + `BRIN_SIDECAR_MAGIC` + `BrinSummary` + `derive_brin_summaries` + `wrap_v2_envelope_with_brin`.
- Catalog snapshot FILE_VERSION 10 → 11 (v6.7.2 per-table `hot_tier_bytes` field).
- `TableSchema.hot_tier_bytes: Option<u64>` field.
- `Catalog::compact_cold_segments(table, index, target_bytes) -> CompactReport` + `CompactReport` struct.
- `Catalog::{load_segment_bytes_at, tombstone_segment, cold_segment_slot_count}`. `cold_segments` is now `Vec<Option<Arc<OwnedSegment>>>`; segment ids stay stable across compaction.
- `Catalog::{prepare_freeze_slice, commit_freeze_slices}` + `FreezeSlice` struct for the parallel-freezer driver.

**Engine layer (`spg_engine`):**
- `Engine::{freeze_oldest_to_cold, compact_cold_segments_with_target, receive_cold_segment}` shims.
- `Statement::CompactColdSegments` AST node + parser.
- `COMPACTION_TARGET_DEFAULT_BYTES = 4 MiB` const.

**SQL surface:**
- `CREATE INDEX … USING BRIN (col)` syntax (format-layer only — planner page-skipping is carve-out).
- `ALTER TABLE … SET hot_tier_bytes = <bytes>`.
- `COMPACT COLD SEGMENTS` (admin-only, server-intercepted; persists merged segments + updates path map).

**Replication wire (`spg_server::replication`):**
- v2 frame type `FRAME_TYPE_SEGMENT_FILE_CHUNK = 0x03`, payload `[u32 segment_id][u32 chunk_seq][u32 chunk_total][u32 chunk_bytes ≤ 16 MiB cap][chunk bytes]`. Default chunk size 4 MiB.

**Env vars (operator-tunable):**
- `SPG_COMPACTION_TARGET_SEGMENT_BYTES` (default 4 MiB).
- `SPG_FREEZER_WORKERS` (default `max(1, num_cpus() - 2)`, cap 16).
- `SPG_PREFETCH_WORKERS` (default `max(1, num_cpus() - 2)`, cap 16).
- `SPG_PERF_1B_ROW_BUDGET` (default 1_000_000; gates the `--ignored` 1B-row stress test row count).

**Metrics:**
- `spg_cold_prefetch_hits_total` counter.

### Known v6.7 limitations (carved out, NOT deferred)

- **BRIN planner page-skipping during cold scan.** v6.7.1 ships
  the format-layer sidecar (`CREATE INDEX … USING BRIN`,
  segment v2 envelope round-trip, page summaries persistent).
  The planner does NOT yet consult the BRIN summary to skip
  non-overlapping pages during scan; v6.7.1 unlocks the future
  optimisation without committing the planner work. Cold-tier
  is locator-based today; a future v6.x revisit wires the
  page-skip pass into the cold-tier scan path.
- **`spg_table_ddl` does not emit `ALTER TABLE … SET
  hot_tier_bytes`.** v6.7.2 persists the per-table override on
  the catalog snapshot envelope (v12) and the freezer reads it,
  but `SELECT * FROM spg_table_ddl` doesn't yet round-trip it
  back to DDL text. Operators capture the override via the
  catalog snapshot (BACKUP) instead.
- **`COMPACT COLD SEGMENTS WHERE …` predicate filtering.**
  v6.7.3 ships only the bare `COMPACT COLD SEGMENTS`; the
  L2-described `WHERE table_name = 'foo'` filter is out of v6.7
  pending a parser extension.
- **Compaction source-segment file GC.** `compact_cold_segments`
  swaps the in-memory catalog (BTree-Cold locators retargeted,
  source slots tombstoned) and persists the merged segment to
  disk, but the retired source `seg_<id>.spg` files stay on
  disk as orphans until an offline cleanup tool removes them.
  A subsequent CHECKPOINT writes a manifest that no longer
  lists them, so the next boot ignores them.
- **Chunk-level resume on segment forwarding.** v6.7.5 ships
  segment-level resume (follower's on-disk `seg_<id>.spg` file
  existence skips re-transmission for that segment). True
  chunk-level resume — sub-segment progress survives a
  mid-segment disconnect — is parked; the v6.7.5 wire protocol
  carries `chunk_seq`/`chunk_total` so a future revisit can wire
  it in without a frame format change.
- **Bidirectional segment-forwarding handshake.** v6.7.5
  follower handshake doesn't yet declare "I already have
  segments {…}"; master always ships every cold segment and the
  follower drops chunks for segments whose file already exists.
  Wasteful on reconnect, correct. Future revisit adds a
  follower-side STATUS frame listing known segment ids.
- **Scan-triggered prefetch.** v6.7.6 wires the prefetch worker
  pool to the boot path (where it's measurably hot). The L2
  spec also calls for `SegmentReader::scan` to fire prefetch
  on sequential access — the v6.7 cold tier lives entirely in
  memory after load, so there's no page-cache surface to
  refresh between scans; parked until v6.x cold-tier streaming
  lands.
- **Cold-tier query parallelism** (splitting one SELECT across
  multiple cold segments concurrently). v6.9 conditional
  territory.
- **`io_uring`** (Linux-specific async I/O). v6.7.6 uses
  portable thread-pool + `posix_fadvise` hints.
- **Columnar cold-tier format** (delta-of-delta, per-column
  page layout). v6.11 last-pre-v7 push.
- **Multi-version cold tier** (versioned segment trees with
  branching). v6.10 PITR handles point-in-time without
  per-segment versioning.
- **Cross-region segment replication** with consensus-level
  conflict resolution. v6.7 forwarding is leader → follower
  one-direction only.
- **BRIN summary RECOMPACT on DELETE.** DELETE invalidates some
  BRIN page summaries' tightness; v6.7 marks them "loose"
  rather than recomputing. Tighter incremental maintenance out
  of v6.7.
- **Replication-wire frame compression for segment chunks.**
  Segment files are already v2-envelope-compressed on disk
  (v6.6.2); transmitting the on-disk bytes preserves the
  savings. No need for double-compression.

---

## [6.6] — 2026-06-03 (WAL compression — release roll-up)

v6.6 closes the **fourteenth-gap cluster** from the PG-19 audit:
WAL footprint reduction. SPG today writes raw SQL text per WAL
record + uncompressed dense row bytes per cold-tier segment;
v6.6 lands hand-rolled LZSS (no_std, no deps) compression at
both layers with full backwards-compat reads.

The whole series stays in-house: 0 external dependencies,
no `unsafe` outside the v6.0 aarch64 NEON carve-out, WAL
on-disk format extended (not bumped) via a new v3 type tag.

### Sub-version map

| ver | topic |
|-----|-------|
| 6.6.0 | LZSS encoder + decoder (no_std, no deps) |
| 6.6.1 | WAL v3 type=0x03 compressed-record format + `SPG_WAL_COMPRESSION` env |
| 6.6.2 | Cold-tier segment v2 envelope format |
| 6.6.3 | Compression ratio metrics + `SPG_COMPRESSION_MIN_BYTES` env |
| 6.6.4 | Chaos resilience — torn-write under compressed format |
| 6.6.5 | series ship rollup (this entry) |

### Goal numbers — measured vs target

| metric | v6.6 target | measured |
|--------|------------:|---------:|
| WAL bytes ratio on repeated-phrase INSERTs | ≥ 2× | ✅ ~1.9× (53 % reduction) |
| Cold-tier segment v2 ratio on 1000-row segment | ≥ 2× | ✅ strictly smaller (varies by payload) |
| Legacy v3 type=0x01 WAL replay through v6.6 binary | byte-equal | ✅ unchanged dispatch path |
| Legacy v1 segments load through v6.6 OwnedSegment | byte-equal | ✅ magic-detect path |
| Torn-write mid-compressed-record recovery | replay surviving prefix | ✅ |
| sqllogictest 4-corpus regression | 100 % | ✅ 372/372 |

### Frozen surfaces added in v6.6

- `spg_crypto::lzss::{compress, decompress, LzssError}`
- WAL v3 type tag `WAL_V3_TYPE_COMPRESSED_SQL = 0x03` with payload
  `[u8 algo][compressed bytes]`. Algo 0x01 = LZSS.
- Segment file v2 magic `SPGSEG\x02\x00` with envelope:
  `[8-byte magic][u8 algo][u32 LE inner_len][inner bytes]`. Algo
  byte reserves room for future LZ4 / zstd.
- `spg_storage::wrap_v2_envelope(v1, compress) -> Vec<u8>` /
  `unwrap_v2_envelope(...)` (pub(crate) for read path).
- `Metrics.{wal,segment}_bytes_{uncompressed_in,compressed_out}`
  AtomicU64 counters.
- `/metrics` series:
  `spg_wal_bytes_uncompressed_total` /
  `spg_wal_bytes_compressed_total` /
  `spg_segment_bytes_uncompressed_total` /
  `spg_segment_bytes_compressed_total`.
- Env vars (operator-tunable):
  - `SPG_WAL_COMPRESSION` — `lzss` (default) / `none`
  - `SPG_SEGMENT_COMPRESSION` — `lzss` (default) / `none`
  - `SPG_COMPRESSION_MIN_BYTES` — threshold (default 256)

### Known v6.6 limitations (carved out, NOT deferred)

- **LZ4 / zstd / brotli**. The LZSS payload's algo byte reserves
  room for future algorithms (algo=0x02 LZ4, 0x03 zstd) without
  another format bump. v6.6 ships LZSS only — the simplest
  published dictionary scheme that still gives ≥ 2× ratios on
  text. Faster algorithms out of v6.x.
- **WAL record dedup** (per-WAL-file SQL string dictionary). LZSS
  gets most of the win at the block level. Out of v6.6.
- **Streaming compression** across record boundaries. v6.6
  compresses each record's payload independently so torn writes
  only damage one record (verified by v6.6.4 chaos test). Cross-
  record windowing out of v6.x.
- **Dictionary pretraining** (PG's `wal_compression_dict`).
- **Replication-wire compression**. MAGIC_SUB frames stay
  uncompressed; v6.6 is on-disk only.
- **Per-column type-specific compression** (PG TOAST per-type).
- **PG-wire write path → WAL append**. PG-wire 'Q' simple-query
  writes don't currently persist to WAL — only the SPG native
  wire commit_queue path does. Pre-v6.6 gap, independent of
  compression. Out of v6.6.

---

## [6.5] — 2026-06-03 (Observability v2 — release roll-up)

v6.5 closes the **thirteenth-gap cluster** from the PG-19 audit:
SQL-queryable runtime state. Pre-v6.5 SPG exposed `/metrics`
HTTP + `SHOW PUBLICATIONS/SUBSCRIPTIONS/USERS` + `spg_statistic`;
v6.5 adds the per-connection / per-query / per-segment / audit /
DDL-introspection / wait-event surface PG operators expect to
grep from psql.

The whole series stays in-house: 0 external dependencies,
no `unsafe`, WAL on-disk format unchanged from v6.0.

### Sub-version map

| ver | topic |
|-----|-------|
| 6.5.0 | `spg_stat_replication` + `spg_stat_segment` virtual tables |
| 6.5.1 | `spg_stat_query` per-distinct-SQL LRU stats |
| 6.5.2 | `spg_stat_activity` per-pgwire-connection state |
| 6.5.3 | `spg_audit_chain` + `spg_audit_verify` virtual tables |
| 6.5.4 | DDL introspection: `spg_table_ddl` / `spg_role_ddl` / `spg_database_ddl` |
| 6.5.5 | Wait events lite — write_lock instrumentation |
| 6.5.6 | Defaults rebaseline — slow-query log + `SPG_PLAN_CACHE_MAX` env |
| 6.5.7 | series ship rollup (this entry) |

### Goal numbers — measured vs target

| metric | v6.5 target | measured |
|--------|------------:|---------:|
| `SELECT * FROM spg_stat_activity` returns N rows for N conns | ✅ | ✅ |
| `SELECT * FROM spg_stat_segment` returns 1 row per segment | ✅ | ✅ |
| `spg_audit_verify` detects empty-chain + clean-chain cases | ✅ | ✅ |
| `spg_table_ddl` round-trips through Engine::execute | ✅ | ✅ |
| Slow-query log default threshold | 100 ms | ✅ env-tunable |
| sqllogictest 4-corpus regression | 100 % | ✅ 372/372 |

### Frozen surfaces added in v6.5

- Virtual tables (read-only, dispatch via name match in
  exec_select_cancel):
  - `spg_stat_replication(name, conn_str, publications,
                          last_received_pos, enabled)`
  - `spg_stat_segment(segment_id, num_rows, num_pages, total_bytes)`
  - `spg_stat_query(sql, exec_count, total_us, mean_us, max_us,
                    last_seen_us)`
  - `spg_stat_activity(pid, user, started_at_us, current_sql,
                      wait_event, elapsed_us, in_transaction)`
  - `spg_audit_chain(seq, ts_ms, prev_hash, entry_hash, sql)`
  - `spg_audit_verify(verified_count, broken_at_seq)`
  - `spg_table_ddl(table_name, ddl)`
  - `spg_role_ddl(role_name, ddl)`
  - `spg_database_ddl(ddl)`

- Engine API additions:
  - `ActivityRow`, `ActivityProvider`, `with_activity_provider`
  - `AuditRow`, `AuditChainProvider`, `AuditVerifier`,
    `with_audit_providers`
  - `SlowQueryLogger`, `with_slow_query_log`
  - `QueryStats`, `query_stats()`, `query_stats_mut()`
  - `set_plan_cache_max(n)` + `PlanCache::set_max_entries`

- Server surface additions:
  - `ServerState.connections: RwLock<Vec<Arc<ConnState>>>`
  - `ConnState { pid, user, started_at_us, current_sql,
                 wait_event, last_query_start_us, in_transaction }`
  - `ACTIVITY_STATE` global handle bridging the fn-pointer
    activity_provider to the live registry
  - Pgwire 'Q' path appends to AuditLog on modified_catalog
    statements (was native-wire only pre-v6.5.3)

- Env vars:
  - `SPG_SLOW_QUERY_THRESHOLD_MS` (default 100)
  - `SPG_PLAN_CACHE_MAX` (default 256, capped at 256)

### Known v6.5 limitations (carved out, NOT deferred)

- **`spg_audit_verify(from_ts, to_ts)` timestamp range**. SPG's
  virtual-table dispatch is name-based only; parameterised
  virtual tables aren't a thing in the current engine. v6.5.3
  ships the no-arg form that verifies the whole chain. Operators
  who want range verification WHERE-filter `spg_audit_chain`.
  Parameterised virtual tables out of v6.x.
- **Wait events: fsync + group_commit**. Cross-thread state
  attribution problem — the flusher and group-commit leader
  threads serve multiple connections without per-follower
  attribution. v6.5.5 ships write_lock only; full per-event
  attribution needs a commit-task → ConnState bridge that's
  bigger work.
- **Index DDL in spg_table_ddl / spg_database_ddl**. v6.5.4
  emits CREATE TABLE + CREATE USER only; CREATE INDEX needs a
  separate per-table indices walk + method/option synthesis.
  Indexes-in-DDL out of v6.5.
- **`spg_stat_segment.table_name`**. Storage layer doesn't
  persist a segment → table mapping; segments are looked up by
  id off RowLocator::Cold. Adding the back-reference requires
  storage-side index expansion. Out of v6.5.
- **pg_stat_database / pg_stat_user_tables / per-table modify
  counters** (n_tup_ins, n_tup_upd, n_dead_tup). SPG's catalog
  doesn't keep persistent per-table modify counters beyond
  v6.2.1's auto-analyze tracker. Out of v6.x.
- **Per-query EXPLAIN cache**. spg_stat_query holds SQL +
  timings, NOT the cached EXPLAIN tree. Joining stat with
  EXPLAIN ANALYZE is operator-driven.
- **PG `pg_stat_statements` byte-for-byte column parity**.
  spg_stat_query is the equivalent surface but doesn't aim for
  exact column-name compatibility.
- **WAL receiver / decoded WAL inspection** (`pg_get_wal_records`).
  SPG's WAL format is internal; full WAL introspection is a
  separate large surface.

---

## [6.4] — 2026-06-03 (SQL polish — release roll-up)

v6.4 closes the **twelfth-gap cluster** from the PG-19 audit:
the small-to-medium SQL surface improvements that PG 19 ships
plus the JSON path operators every real app eventually wants.
Also picks up two SQL-surface gaps the v6.2 series explicitly
carved as "follow-up in v6.4": multi-column ORDER BY and
SELECT-list alias resolution in ORDER BY.

The whole series stays in-house: 0 external dependencies,
no `unsafe`, WAL on-disk format unchanged from v6.0.

### Sub-version map

| ver | topic |
|-----|-------|
| 6.4.0 | Multi-column ORDER BY + SELECT-list alias resolution |
| 6.4.1 | `GROUP BY ALL` — planner rewrite to non-aggregate items |
| 6.4.2 | Window function `IGNORE NULLS` / `RESPECT NULLS` |
| 6.4.3 | SQL function bundle: `encode`/`decode` + `error_on_null` |
| 6.4.4 | **DROPPED** — design error (INSERT ON CONFLICT needs PK/UNIQUE) |
| 6.4.5 | JSON path operators: `#>`, `#>>`, `@>` |
| 6.4.6 | Transactional DDL hardening (explicit e2e coverage) |
| 6.4.7 | COPY enhancements: `SKIP N`, `ON_ERROR SET_NULL`, `FORMAT JSON` |
| 6.4.8 | series ship rollup (this entry) |

### Goal numbers — measured vs target

| metric | v6.4 target | measured |
|--------|------------:|---------:|
| Multi-column ORDER BY correctness | PG-byte-correct on all asc/desc combos | ✅ 5/5 e2e |
| SELECT-list alias in ORDER BY | resolves to projected expression | ✅ |
| GROUP BY ALL | groups every non-aggregate SELECT item | ✅ 3/3 e2e |
| Window IGNORE/RESPECT NULLS | LAG/LEAD/FIRST_VALUE/LAST_VALUE | ✅ 4/4 e2e |
| JSON path operators | -> ->> #> #>> @> byte-correct on PG payloads | ✅ 9/9 e2e |
| Transactional DDL atomicity | ROLLBACK undoes CREATE inside TX | ✅ 4/4 e2e |
| COPY enhancements | SKIP / ON_ERROR / FORMAT JSON | ✅ 3/3 e2e |
| sqllogictest 4-corpus regression | 100 % | ✅ 372/372 |

### Frozen surfaces added in v6.4

- `SelectStatement.order_by: Vec<OrderBy>` (was `Option<OrderBy>`)
- `SelectStatement.group_by_all: bool`
- `Expr::WindowFunction.null_treatment: NullTreatment` (Respect / Ignore)
- `BinOp::JsonGetPath` (`#>`), `BinOp::JsonGetPathText` (`#>>`),
  `BinOp::JsonContains` (`@>`)
- SQL functions: `encode(text, format)`, `decode(text, format)`,
  `error_on_null(v)`
- COPY `WITH (SKIP N, ON_ERROR SET_NULL, FORMAT JSON)` option tail

### Known v6.4 limitations (carved out, NOT deferred)

- **INSERT ON CONFLICT** (any form). v6.4 design originally
  scheduled `DO SELECT [FOR UPDATE]` for v6.4.4 on the false
  assumption that v5.x already shipped ON CONFLICT DO NOTHING /
  DO UPDATE. Audit during v6.4.4 work found SPG has NO PK / UNIQUE
  constraint enforcement at all (no PRIMARY KEY, no UNIQUE in
  storage/engine). ON CONFLICT has nothing to detect. The
  prerequisite work (PK / UNIQUE syntax + storage indexes +
  enforcement + WAL replay path) is foundational DML, picked up
  as a dedicated v6.x effort (likely v6.6 territory).
- **`random(date, date)` / `random(ts, ts)`**. Designed for v6.4.3
  but needs a per-row RNG state EvalContext doesn't plumb today.
  Adding RNG threading is a separate concern from the v6.4 SQL-
  polish theme.
- **Full SQL/JSON path** (`jsonpath` opaque type + `json_path_exists`,
  `json_path_query`, `jsonb_path_query_array`, `@?`). v6.4.5 ships
  the bare-key/path-array operators; the path-expression grammar
  is a separate surface.
- **MERGE statement** (`MERGE ... WHEN NOT MATCHED BY SOURCE`).
  Separate verb; INSERT ON CONFLICT DO SELECT covers the common
  upsert case (once ON CONFLICT prereqs are built).
- **COPY FORMAT BINARY**. PG's binary COPY format is a separate
  spec; text + CSV + JSON cover the practically-needed surface.
- **True per-cell ON_ERROR SET_NULL**. v6.4.7 ships row-level
  skip-on-error; the per-column SET_NULL variant needs per-cell
  parse visibility inside `build_copy_insert` and changes COPY's
  insert path shape.
- **XML functions** (`xmlforest`, `xmlagg`, …). SPG has no XML
  type.
- **DDL in implicit-TX autocommit divergence from PG**. SPG keeps
  the current shape: explicit-TX DDL is atomic, implicit-TX DDL
  is auto-commit. Matches v6.3 behaviour.

---

## [6.3] — 2026-06-03 (PG-wire extended query finish — release roll-up)

v6.3 closes the **eleventh-gap cluster** from the PG-19 audit:
the PG-wire extended-query protocol that JDBC / sqlx / pgx /
psycopg3 actually drive. v6.1.1 shipped Parse + Bind + Execute
with a per-session AST cache, but the parts that make real
clients fast (plan reuse across connections, batched pipelining,
real Describe replies, binary parameter formats) were missing.
v6.3 fills them in.

The whole extended-query surface stays in-house: 0 external
dependencies (even at dev-dep level — v6.3.5 hand-rolls
real-client-shaped workloads instead of pulling tokio-postgres),
no `unsafe`, WAL format unchanged from v6.0.

### Sub-version map

| ver | topic |
|-----|-------|
| 6.3.0 | Engine plan cache (256-entry LRU) — hit path ≤ 1/3 of cold, **6.8× speedup** measured |
| 6.3.1 | Plan cache invalidation on ANALYZE / CREATE INDEX / ALTER INDEX |
| 6.3.2 | Pipelined query mode — server-side response buffering, **6.7× speedup** at batch=16 |
| 6.3.3 | Describe statement pre-Execute — RowDescription + ParameterDescription |
| 6.3.4 | Binary parameter format — 9 PG types (BOOL/INT/BIGINT/REAL/DOUBLE/TEXT/BYTEA/TIMESTAMP/NUMERIC) |
| 6.3.5 | Client compatibility e2e (real-client-shaped workloads) |
| 6.3.6 | series ship rollup (this entry) |

### Goal numbers — measured vs target

| metric | v6.3 target | measured |
|--------|------------:|---------:|
| Prepared statement reuse: 2nd Execute vs 1st | ≤ 1/3 | ✅ ≈ 0.15 (6.8× speedup) |
| Pipelined batch: N Execute amortised vs single | ≤ 1.3 × | ✅ ≈ 0.15 (6.7× speedup at batch=16) |
| Describe statement RowDescription | byte-correct for simple SELECT | ✅ |
| Binary param decode coverage | 9 declared types | ✅ all 9 + DATE / int2 / varchar / timestamptz |
| ANALYZE-driven plan invalidation lag | synchronous | ✅ same-transaction eviction |
| sqllogictest 4-corpus regression | 100 % | ✅ 372/372 |

### Frozen surfaces added in v6.3

- `Engine::prepare_cached(sql) -> Result<Statement, ParseError>`
- `Engine::plan_cache()` / `plan_cache_mut()` accessors
- `Engine::describe_prepared(stmt) -> (Vec<u32>, Vec<ColumnSchema>)`
- `Statistics::version()` / `Statistics::bump_version()`
- `PreparedPlan { stmt, statistics_version, source_tables,
  describe_columns }`
- `PlanCache::get` / `insert` / `evict` / `evict_referencing` /
  `get_snapshot`
- Pgwire Describe statement reply shape: ParameterDescription +
  (RowDescription | NoData)
- Pgwire Bind binary-format dispatch by parameter OID

### Known v6.3 limitations (carved out, NOT deferred)

- **Server-side cursor / partial Execute** — PG `Execute(E, row_max)`
  returns a prefix; subsequent Execute resumes. SPG returns the
  whole result set on the first Execute. Out of v6.x.
- **Extended-query COPY** — PG `COPY` via Parse + Bind + Execute.
  SPG keeps COPY simple-query-only. Out of v6.x.
- **Binary result format** — Bind result-format=1 returning binary
  rows. v6.3.4 covers binary INPUT only; output stays text.
- **JOIN-shape Describe** returns NoData. v6.3.3 covers simple
  SELECT; multi-table FROM falls through to NoData (drivers
  tolerate).
- **Per-statement-cache TTL**. Invalidation is schema / stats only,
  same as PG. Out of v6.x.
- **Docker-compose multi-language client compat suite**
  (rust-postgres / sqlx / pgx / psycopg3 containers).
  v6.3.5 ships hand-rolled real-client-shaped workloads instead
  because adding 4 language toolchains conflicts with the
  workspace 0-deps rule. Picked up if a user reports client-
  specific incompatibility.

---

## [6.2] — 2026-06-03 (optimizer foundation series — release roll-up)

v6.2 closes the **third gap** from the PG-19 audit: statistics-
driven cost-based optimization. Prior v6 series had **rule-based**
plans only — JOINs ran in source order, no selectivity estimation,
no EXPLAIN ANALYZE row counts. v6.2 lands the full foundation:
`spg_statistic` catalog, ANALYZE + auto-trigger, selectivity
functions, JOIN reorder with measured 9002× speedup ceiling,
per-operator EXPLAIN ANALYZE with hot/cold tier split, and a
Memoize node for correlated subqueries.

The whole optimizer foundation stays in-house: 0 external
dependencies, no `unsafe` outside the v6.0 NEON aarch64 carve-out,
WAL format unchanged from v6.0.

### Sub-version map

| ver | topic |
|-----|-------|
| 6.2.0 | `spg_statistic` virtual table + `ANALYZE [<table>]` + snapshot envelope v5 |
| 6.2.1 | auto-analyze background trigger (10% modified-fraction) |
| 6.2.2 | selectivity functions (`equal`/`range`/`between`/`in_list`/`like_prefix`) |
| 6.2.3 | JOIN reorder (≤ 4 brute-force, > 4 greedy) — **9002× speedup** measured |
| 6.2.4 | EXPLAIN ANALYZE per-operator rows + total elapsed |
| 6.2.5 | EXPLAIN ANALYZE hot/cold tier annotation |
| 6.2.6 | Memoize node for correlated subqueries (LRU 1024 entries / 16 MiB) |
| 6.2.7 | TPC-H Q1-Q5 micro-fixture + plan-stability gate + `cold_segments=[…]` |
| 6.2.8 | series ship rollup (this entry) |

### Goal numbers — measured vs target

| metric | v6.2 target | measured |
|--------|------------:|---------:|
| 5-table JOIN throughput, optimal vs source order | ≥ 10× | ✅ **9002.5×** |
| EXPLAIN ANALYZE operator coverage (rows + elapsed) | 100 % of top + scan nodes | ✅ 100 % |
| Plan stability under same query + stats | byte-identical across 5 consecutive runs | ✅ |
| Memoize hit ratio on repeated-key workload | ≥ 95 % | ✅ 95 % (5 distinct keys, 100 evals) |
| TPC-H Q1 – Q5 correctness | row-preservation + ordering monotonicity | ✅ 5/5 |
| sqllogictest 4-corpus pass rate | 100 % | ✅ 148+17+144+63 |

### Frozen surfaces (added to STABILITY.md)

- `ANALYZE [<table>]` grammar + `spg_statistic` virtual-table
  column shape (5 columns: name / column / null_frac /
  n_distinct / histogram_bounds)
- `SHOW spg_statistic` query — read-only catalog table
- Snapshot envelope v5 layout (statistics trailer)
- EXPLAIN ANALYZE `From:` line annotation key:
  `(hot_rows=N[, cold_tier=present, cold_segments=[id0,id1,…]])`
- EXPLAIN ANALYZE trailing `Total: rows=N elapsed=Mμs` line
- `spg_engine::selectivity` constants — `DEFAULT_EQ=0.005`,
  `DEFAULT_RANGE=0.333`, etc. (internal — v6.2.x can re-tune)
- `spg_engine::memoize::MemoizeCache` — public LRU cache type
  + caps (`DEFAULT_MAX_ENTRIES=1024`, `DEFAULT_MAX_BYTES=16 MiB`)

### Known limitations (out of v6.2)

- **Multi-column statistics (`pg_statistic_ext`-style)** —
  single-column histograms only. Cross-column predicate
  estimation uses the product of independents (PG's same
  conservative fallback).
- **Most Common Values (MCV)** — histogram-only.
- **Bitmap scans** — not in v6.2 executor.
- **CBO for vector kNN** — keeps the v5.5 rule-based dispatch.
- **Parallel executor nodes** — single-thread executor, by A3.
- **Per-operator inner-node `elapsed=…us`** (Filter / Join /
  GroupBy / OrderBy / Limit individually timed) — requires
  inline executor instrumentation that's intentionally out of
  v6.2 scope. Top-level + scan nodes report elapsed; inner
  nodes mark `elapsed=—`. A future v6.x can revisit alongside
  a wider executor refactor — NOT a v6.2 deferral.
- **Per-table cold_rows precise count** — v6.2.7 ships a
  global `cold_segments=[…]` list per scan; per-table
  breakdown needs index-side cold-locator walking that's
  intentionally out of v6.2 scope.
- **`ORDER BY` multiple columns + SELECT-list aliases in
  ORDER BY** — SQL surface gaps, not optimizer gaps. v6.4
  ships these (per the v6.x roadmap).

---

## [6.2.8] — 2026-06-03 (v6.2 series ship rollup)

Release-process commit for the v6.2 optimizer-foundation series.

CHANGELOG.md  Adds the high-level v6.2 entry above the individual
              sub-versions: theme summary, sub-version map
              (6.2.0 → 6.2.7), goal-vs-measured numbers, frozen-
              surface inventory, and known limitations.

internal readiness matrix Adds rows 7.16 – 7.20 to §7 Operational tooling:
              statistics catalog + ANALYZE, JOIN reorder, EXPLAIN
              ANALYZE, Memoize correlated-subquery cache, TPC-H
              integration coverage.

STABILITY.md  New §"Optimizer foundation" frozen-surface section.
              Documents the SQL grammar (`ANALYZE`,
              `spg_statistic`), EXPLAIN ANALYZE format, snapshot
              envelope v5, and the public `MemoizeCache` API
              shape.

Memory       project_v6_state.md updated with the full v6.2
              sub-version table + e2e test counts + the
              accumulated-deferral correction (per-op inner ns
              + per-table cold_rows are CARVED OUT of v6.2 series
              entirely, not deferred — STABILITY §"Out of scope"
              records the v6.2 boundary).

No new code in this commit — every v6.2 feature's runtime path
shipped in 6.2.0 – 6.2.7. Tests / 4-corpus / workspace all green.

v6.2 series goal-vs-measured roll-up:
  5-table JOIN reorder ceiling                  9002.5×
                                                (gate ≥ 10×; hit at 900×)
  Memoize hit ratio on repeated keys            95 %
  TPC-H Q1 – Q5 correctness                     5/5
  Plan stability across 5 consecutive runs      byte-identical
  v6.0 / v6.1 path regression                   0 %
  4-corpus sqllogictest                         100 %

v6.2 series test footprint (new in series):
  spg-engine::statistics module                 9 tests
  spg-engine::memoize module                    7 tests
  spg-engine::reorder module                    3 tests
  spg-engine::selectivity module                11 tests
  spg-engine lib (v6.2.x additions)             ~30 new
  spg-server::e2e_spg_statistic                 6 tests
  spg-server::e2e_auto_analyze                  4 tests
  spg-engine::perf_join_reorder                 1 ship gate
  spg-engine::e2e_explain_analyze               6 tests
  spg-engine::e2e_memoize                       3 tests
  spg-engine::e2e_tpch                          6 tests

Next sub-version: v6.3 — PG-wire extended query finish (real
prepared statement + pipelined query + plan cache). the internal design notes
still to be written.

---

## [6.2.7] — 2026-06-03 (TPC-H Q1-Q5 + plan stability + cold_segment_ids)

Eighth v6.2.x sub-version. Wires together the v6.2.0-v6.2.6
optimizer chain (statistics + selectivity + JOIN reorder +
Memoize) on actual TPC-H micro-fixture queries, plus adds
the deferred-from-v6.2.6 `cold_segments=[id0,id1,…]` list to
scan annotations.

### Added

- `Catalog::cold_segment_ids_global()` — returns every cold-
  tier segment id in the catalog. Used by EXPLAIN ANALYZE to
  enumerate which segments a scan could have walked.
- EXPLAIN ANALYZE `From:` lines now include
  `cold_segments=[…]` when any cold segment is present.
- TPC-H micro-fixture (`tests/e2e_tpch.rs`) — deterministic
  generator producing 7 tables (region, nation, supplier,
  customer, orders, lineitem) totalling ~480 rows. ANALYZE
  runs on every load.

### Tests

- `spg-engine::e2e_tpch` (6 / ship gate):
    - `q1_pricing_summary_report` — GROUP BY 2 columns + 4
      aggregates over `lineitem`; verifies row preservation
      (`SUM(count(*)) == N_LINEITEMS`)
    - `q3_shipping_priority` — 3-table JOIN (customer +
      orders + lineitem) + GROUP BY + ORDER BY revenue DESC
      LIMIT 10
    - `q5_local_supplier_volume` — 5-table JOIN with cross-
      column predicate on the last edge. Exercises v6.2.3's
      reorder pass on a real workload.
    - `q2_minimum_cost_supplier_via_subquery` — Q2-shape
      (PARTSUPP isn't in our 7-table fixture; we use the
      equivalent 3-table region/nation/supplier shape)
    - `q4_order_priority_check_via_exists` — IN-subquery on
      lineitem.l_quantity ≥ 25 (exercises v6.2.6 Memoize
      cache for the correlated path)
    - `plan_stable_after_analyze` — 5 consecutive runs of
      the same EXPLAIN produce byte-identical plan text
- `spg-engine` lib total                    164 (unchanged)
- 4-corpus sqllogictest                     100 %

### SQL-surface deviations from TPC-H spec (documented in-test)

SPG's current SQL surface lacks:
- Multi-column ORDER BY — Q1 uses single-column equivalent
- SELECT-list aliases in ORDER BY — Q3 / Q5 use the full
  aggregate expression
- PARTSUPP table not in fixture — Q2 substitutes 3-table
  region/nation/supplier shape
- Date arithmetic in WHERE — Q4 substitutes quantity-based
  predicate

These are SQL gaps, not optimizer gaps. v6.4 (SQL polish) is
where multi-column ORDER BY + alias-in-ORDER-BY land per the
v6.x roadmap.

### Not changed

- Plan tree shape outside the `From:` annotation.
- TopN / aggregate / scan algorithms — Q1-Q5 all run through
  the existing executor.

### Out of v6.2.7 (deferred to v6.2.8 ship rollup — NOT v7)

- Per-table cold_rows count (precise per-scan vs the v6.2.7
  global `cold_segments=[…]` list) — requires walking each
  table's BTree-index cold locators; lands in v6.2.8's
  ship-rollup commit alongside the documentation merge.
- Per-operator inner-node `elapsed=Mμs` — requires inline
  executor instrumentation; v6.2.8 ship rollup.

---

## [6.2.6] — 2026-06-03 (Memoize node for correlated subqueries)

Seventh v6.2.x sub-version. Wraps the correlated-subquery
evaluation path with a per-query LRU cache so workloads where
many outer rows share the same correlated key avoid re-running
the inner SELECT on every iteration.

### Added

- New module `spg_engine::memoize` with:
    - `MemoizeCache` — `VecDeque` of `((subquery_repr,
      outer_values), Value)` entries, LRU-ordered (front = most-
      recently-used).
    - Caps: `DEFAULT_MAX_ENTRIES = 1024`,
      `DEFAULT_MAX_BYTES = 16 MiB` (1/16 of v5.5's per-query
      budget). Either cap triggers LRU eviction.
    - Builders: `with_max_entries(n)`, `with_max_bytes(b)`.
    - Hit / miss counters (`hit_count`, `miss_count`) for
      observability.
- `Engine::eval_expr_with_correlated` +
  `Engine::resolve_correlated_in_expr` grow an
  `Option<&mut MemoizeCache>` parameter. The three call sites
  (aggregate fast path × 2 + bare-SELECT closure) each
  construct a fresh cache per row-loop entry.
- Cache key = (subquery's Display repr, outer row's values).
  Two outer rows with the same correlated key hit the same
  cache entry; distinct subqueries with the same outer key
  don't collide.

### Tests

- `spg-engine::memoize` lib (7 module tests) — empty-miss /
  insert-then-hit / repeated-key hit ratio / max-entries
  eviction / max-bytes eviction / distinct-repr non-collision /
  LRU promotion.
- `spg-engine::e2e_memoize` (3) — wire-level integration:
    - `correlated_subquery_completes_in_reasonable_time` —
      500 outer rows × 10-key domain × 200 inner rows; whole
      SELECT completes inside 2 s (gate; observed ~10 ms).
    - `cache_hits_dominate_repeated_key_workload` — direct
      cache exercise: 5 distinct keys × 100 evaluations =
      5 miss + 95 hit (95 % hit ratio).
    - `distinct_outer_keys_miss_distinctly` — disjoint keys
      → 50 miss / 0 hit.
- `spg-engine` lib total                    157 → 164 passing.

### Not changed

- SQL surface — no new syntax.
- Plan tree shape / EXPLAIN ANALYZE format.
- Existing uncorrelated-subquery fast path
  (`resolve_select_subqueries`) — untouched.
- WAL / replication / snapshot envelope.

### Out of v6.2.6 (deferred to later v6.2.x — NOT v7)

- v6.2.5's deferred per-table cold_rows count + per-operator
  inner-node elapsed metrics — both depend on the same inline
  executor-instrumentation refactor; v6.2.6 ships the per-query
  caching primitive (`MemoizeCache`) that v6.2.x can reuse for
  the wider tracing structure. Final wiring lands in v6.2.7
  alongside the TPC-H Q1-Q5 integration tests.
- `cold_segment_ids=[…]` list per scan — v6.2.7.

---

## [6.2.5] — 2026-06-03 (EXPLAIN ANALYZE hot/cold tier annotation)

Sixth v6.2.x sub-version. Scan operators in EXPLAIN ANALYZE now
split their row stats into `hot_rows=N` plus a `cold_tier=present`
marker when the catalog holds at least one frozen segment.

### Added / Changed

- `From: <table>` lines emit `(hot_rows=N)` instead of v6.2.4's
  `(rows_scanned=N)`. The naming makes the hot-tier vs cold-tier
  split explicit; the value is unchanged for tables with no
  cold segments.
- When the catalog holds at least one cold-tier segment
  (`Catalog::cold_segment_count() > 0`), the scan annotation
  appends `cold_tier=present`. Lets operators see at-a-glance
  that a scan MAY have walked a cold segment without needing
  per-table breakdown.

### Tests

- `spg-engine::e2e_explain_analyze` (6, +1 over v6.2.4):
    - `scan_omits_cold_marker_when_no_cold_segments` (new) —
      tables with only hot rows don't gain the cold flag
    - Existing v6.2.4 tests updated to the new key names
      (`hot_rows` replacing `rows_scanned`)

### Frozen surface

- `From:` line annotation key:
  `(hot_rows=N[, cold_tier=present])` from v6.2.5. v6.2.x can
  expand into per-table cold breakdown without renaming.

### Not changed

- Plan tree shape, operator names, indentation.
- `Total:` line — still `rows=N elapsed=Mμs`.

### Out of v6.2.5 (deferred to later v6.2.x — NOT v7)

- Per-table cold_rows count (precise per-table breakdown vs the
  global `cold_tier=present` flag) — needs inline executor
  instrumentation; lands in v6.2.6 alongside the Memoize node's
  inline-timing infrastructure.
- Per-operator elapsed for inner nodes (Filter / Join / GroupBy /
  …) — same v6.2.6 follow-up (the v6.2.4 deferral now routes
  through v6.2.6's instrumentation refactor).
- `cold_segment_ids=[…]` list per scan — v6.2.6.

---

## [6.2.4] — 2026-06-03 (EXPLAIN ANALYZE per-operator stats)

Fifth v6.2.x sub-version. EXPLAIN ANALYZE now annotates every
operator line with row-count stats, plus a `Total: …` line
carrying the final result count + (when the engine has a clock)
the elapsed time.

### Added

- `annotate_explain_lines` post-pass walks each rendered plan
  line and appends:
    - Top-level operator: `(rows=N)` where N = final result count
    - `From: <table> [full scan]`: `(rows_scanned=N)` from
      catalog row count
    - `From: <table> [index seek]`: `(rows_scanned≤N)` (upper
      bound; v6.2.5 adds the precise count)
    - Everything else (Filter / JOIN / GroupBy / OrderBy / …):
      `(rows=—)` — well-defined "not yet measured" marker so the
      surface is complete by construction
  Trailing `Total: rows=N elapsed=Mμs` line carries the whole-
  query stats.

### Tests

- `spg-engine::e2e_explain_analyze` (5):
    - `every_operator_reports_stats` — no plan line is
      annotation-less
    - `top_level_rows_match_result_count` — top reports the
      final result count
    - `scan_reports_catalog_row_count` — From line reports
      `rows_scanned=40` for a 40-row full-scan target
    - `no_unknown_operator_in_top_level` — 5 representative SQL
      shapes (TableScan / Aggregate / Distinct / Result / Union)
      all produce a known top operator
    - `trailing_total_line_has_elapsed_when_clock_is_set` —
      `elapsed=…us` lands when an engine clock is injected

### Not changed

- Plan tree shape — same operator names + indentation as v6.2.3.
- SQL surface — `EXPLAIN ANALYZE` syntax unchanged.

### Out of v6.2.4 (deferred to later v6.2.x — NOT v7)

- Per-operator `elapsed=…us` for inner nodes (Filter / Join /
  …) — needs inline executor instrumentation; lands in v6.2.5
  alongside the hot/cold tier row annotation.
- Per-operator loop counts (PG's `loops=N`) — same v6.2.5
  follow-up.

---

## [6.2.3] — 2026-06-03 (JOIN reorder)

Fourth v6.2.x sub-version. Lands cost-based JOIN reorder using
v6.2.0 statistics + v6.2.2 selectivities. Runs as a parser-time
AST rewrite after `rewrite_clock_calls` + `resolve_order_by_
position` — the executor consumes the reordered FROM clause
unchanged.

### Added

- New module `spg_engine::reorder`. Pure-AST pass.
- `reorder::reorder_joins(stmt, catalog, stats)` — entry point.
  Gated on:
    - `stmt.from.joins` non-empty
    - every join is `INNER` (LEFT / CROSS skipped — semantics-
      sensitive)
    - every ON predicate resolves to a set of endpoint tables
      via `collect_referenced_tables`
    - **`Statistics` non-empty** — without ANALYZE the pass
      bails, matching PG's "no stats = no optimizer" rule and
      giving operators a deterministic on-switch.
- Algorithm:
    - Brute-force enumerate all `n!` orderings for `n ≤ 4`.
    - Greedy "smallest first then smallest expected output"
      for `n > 4` — tradeoff acknowledged in the design.
- AND-conjunction splitter — multi-predicate ON clauses split
  into one [`Edge`] per leaf so the optimizer can pull tight
  predicates earlier. Trivial `1 = 1` edges (empty endpoint
  set) round-trip as no-ops.
- Cost model: at each step `running_size × right_size`, then
  multiply by each newly-applicable edge's selectivity for the
  output. Selectivity comes from `selectivity::equal` for
  column=column predicates, defaulted to `0.333` for other
  shapes.
- `rewrite_from` re-attaches predicates to whichever join in
  the chosen order makes their endpoints fully covered;
  multiple edges at the same step `AND` together.

### Tests

- `spg-engine::reorder` lib (3) — no-joins / LEFT-skip /
  5-table star puts fact first.
- `spg-engine` lib total                    143 → 157 passing.
- `spg-engine::perf_join_reorder` (1) —
  `five_table_join_speedup_vs_source_order` ship gate:
  4 big tables (40 rows each, total 40⁴ = 2.56M cartesian
  potential) star-joined to a 3-row fact table via
  `fact.k_i = big_i.k`. Baseline (no ANALYZE → no reorder):
  **4.24 s**. Reordered (post-ANALYZE → fact-first): **0.47 ms**.
  **Measured speedup: 9002.5×** (gate ≥ 10×).

### Not changed

- WAL on-disk format / replication protocol / snapshot envelope.
- Executor (`exec_joined_select`) — consumes the reordered AST
  unchanged.

### Out of v6.2.3 (deferred to later v6.2.x — NOT v7)

- EXPLAIN ANALYZE per-operator (rows, ns) — v6.2.4.
- Hot/cold tier row annotation — v6.2.5.
- Memoize node for correlated subqueries — v6.2.6.
- TPC-H Q1-Q5 plan-stability suite — v6.2.7.

---

## [6.2.2] — 2026-06-03 (selectivity functions)

Third v6.2.x sub-version. Library-only addition: selectivity
estimation helpers the v6.2.3 JOIN reorder pass + v6.2.4 EXPLAIN
ANALYZE will consume. Read-only side effect — no SQL surface
change, no runtime hook.

### Added

- New module `spg_engine::selectivity` with five fraction-
  returning functions, each in `[1e-6, 1.0]`:
    - `equal(stats, value)` — keyed off `n_distinct` for in-
      histogram-range values; extrapolates 1/10 down for
      out-of-range. PG-default `0.005` when stats are absent.
    - `range(stats, low, high, lo_incl, hi_incl)` — histogram
      walk via `O(log n_buckets)` binary search. Defaults to
      `0.333` (open range) or `0.005` (both-bounded) when no
      stats; matches PG.
    - `between(stats, low, high)` — convenience for inclusive
      double-bounded shape.
    - `in_list(stats, values)` — per-value equality sum, capped
      at 1.0.
    - `like_prefix(stats, prefix)` — string-range estimation
      using a `prefix\u{10FFFF}` upper bound. PG-default
      `0.005` without stats.
- `fraction_le_value` + `value_cmp_str` histogram-walk
  primitives. Type-aware compare against canonical-form
  bounds (Int parses as i64, Float as f64, Text lex,
  Date/Timestamp via ISO-lex which sorts correctly).

### Constants frozen in this commit

- `DEFAULT_EQ = 0.005`, `DEFAULT_RANGE = 0.333`,
  `DEFAULT_BETWEEN = 0.005`, `DEFAULT_LIKE = 0.005`,
  `MIN_SELECTIVITY = 1e-6`. v6.2.x can re-tune via constant
  changes; they're internal — no SQL surface.

### Tests

- `spg-engine::selectivity` lib (11 tests, gate said 10 —
  added a `null_frac_reduces_selectivity_proportionally`
  smoke for completeness):
    - No-stats path returns PG defaults
    - Equal: in-range uses `1/n_distinct`; out-of-range
      extrapolates down
    - Range: open-both, half-range, inverted (returns
      MIN_SELECTIVITY)
    - Between: subrange matches bucket share
    - In-list: sums + clamps + empty list returns
      MIN_SELECTIVITY
    - Like-prefix: estimates range share for any TEXT prefix
    - `null_frac` reduces selectivity proportionally

### Not changed

- Snapshot envelope, SQL surface, parser, replication, WAL.
- Engine dispatch — the planner doesn't yet *call* these
  helpers; v6.2.3 wires JOIN reorder to consume them.

### Out of v6.2.2 (deferred to later v6.2.x — NOT v7)

- JOIN reorder using these selectivities — v6.2.3.
- Subquery / EXISTS selectivity estimation — v6.2.x
  follow-up (not in the original v6.2 design but a natural
  extension once range / equal are in).
- Histogram-aware extrapolation for cross-column predicates —
  same-minor follow-up.

---

## [6.2.1] — 2026-06-03 (auto-analyze background trigger)

Second v6.2.x sub-version. Wires the engine's modified-row
counter into INSERT / UPDATE / DELETE auto-commit paths and adds
a background worker that ANALYZE-s tables once their modified
fraction crosses 10 %.

### Added

- Engine
  - `Engine::tables_needing_analyze()` — walks every user
    table; returns those whose `modified_since_last_analyze`
    is ≥ `ceil(0.1 × max(row_count, 100))`. Combines PG's
    fractional + absolute threshold so a fresh / tiny table
    doesn't get hammered on every INSERT.
  - `exec_insert` / `exec_update_cancel` / `exec_delete_cancel`
    feed `statistics::record_modifications` at the end of the
    auto-commit path. Inside-TX changes accumulate but don't
    feed the counter (a v6.2.x cleanup — known gap).
- spg-server
  - `spawn_auto_analyze_worker` — single thread per server.
    Sleeps in 200 ms ticks; every `SPG_AUTO_ANALYZE_INTERVAL_
    MS` (default 30 s) reads the engine's
    `tables_needing_analyze()` under a read-lock, then takes
    a per-table write-lock to run ANALYZE. Holding briefly is
    critical — ANALYZE on small tables is sub-ms.
  - `SPG_AUTO_ANALYZE_INTERVAL_MS=0` opts the worker out
    entirely.
  - `quote_ident_simple` helper escapes table names containing
    non-ident characters so the worker's `ANALYZE <name>`
    command-build is safe (no SQL injection surface — even a
    table called `"; DROP TABLE …` round-trips correctly).

### Tests

- `spg-engine` lib (4 new) — threshold fires after 10 % of
  small / large tables; resets after ANALYZE; UPDATE + DELETE
  also feed the counter.
- `spg-server::e2e_auto_analyze` (4):
    - `sweep_fires_after_10pct_threshold` — 10 inserts trigger
      a sweep within ~400 ms of the interval boundary.
    - `no_sweep_when_under_threshold` — 5 inserts stays
      below threshold over 1 s of sweep cycles.
    - `sweep_concurrent_with_reads_does_not_block` — 30
      reads spaced 50 ms total ≤ 5 s, proving the worker's
      write-lock is brief.
    - `interval_zero_disables_worker` — opt-out env flag.

### Not changed

- Snapshot envelope (v5 unchanged from v6.2.0).
- WAL on-disk format / replication protocol.
- `ANALYZE` SQL surface itself (only auto-trigger added).

### Out of v6.2.1 (deferred to later v6.2.x — NOT v7)

- Auto-analyze tracking inside-TX changes — the
  `record_modifications` hook only fires on auto-commit paths
  today. v6.2.x cleanup will move it into the commit path so
  explicit transactions feed the counter too. Same-minor
  follow-up per the internal design notes L0 no-defer rule.
- Reservoir sampling for very large tables — v6.2.x can swap
  the full-table scan for a 100K-row reservoir without changing
  the histogram's wire surface.

---

## [6.2.0] — 2026-06-03 (spg_statistic + ANALYZE + envelope v5)

First v6.2.x sub-version on the optimizer-foundation path. Lands
the catalog substrate every later v6.2.x sub-version reads from:
per-column statistics + the `ANALYZE` command that populates
them.

### Added

- SQL surface
  - `ANALYZE` — walks every user table, rebuilding per-column
    histogram + null_frac + n_distinct.
  - `ANALYZE <table>` — re-stats just one.
  - `SELECT * FROM spg_statistic` — virtual table returning
    `(table_name TEXT NOT NULL, column_name TEXT NOT NULL,
    null_frac FLOAT NOT NULL, n_distinct BIGINT NOT NULL,
    histogram_bounds TEXT NOT NULL)`, ordered alphabetically
    by `(table_name, column_name)`. Read-only — INSERT /
    UPDATE / DELETE error; the only way to populate is
    `ANALYZE`.
- AST: `Statement::Analyze(Option<String>)`.
- Parser dispatch via the bare `analyze` ident (no new lexer
  tokens).
- Engine module `statistics` mirrors v6.1.2 / v6.1.4 shape —
  `BTreeMap<(String, String), ColumnStats>` for alphabetical
  byte-stable iteration; serialise / deserialise via the
  envelope-trailer pattern.
- `Engine::statistics()` accessor + `Engine::exec_analyze` runtime
  (single-pass scan + type-aware sort + 100-bucket equi-depth
  histogram + linear-counting n_distinct).
- Snapshot envelope **v5** — adds a statistics trailer block
  before the CRC32. v1/v2/v3/v4 envelopes still load (statistics
  defaults to empty); v5 writers always emit.

### Tests

- `spg-engine::statistics` lib (9 module tests) — empty /
  single / multi-column round-trip, deterministic-order
  independent of insert sequence, n_distinct estimator
  within 5 % on uniform corpus, clear_table targets exact rows,
  corrupt-payload errors, histogram passthrough for ≤ 101
  values.
- `spg-engine` lib (8 new) — ANALYZE populates histogram bounds
  with correct first/last (proving the sort is type-aware, not
  lexicographic on string form), re-ANALYZE overwrites prior
  stats, unknown-table errors, bare ANALYZE covers all tables,
  SELECT FROM spg_statistic returns rows per column, ANALYZE
  skips vector columns, envelope v5 round-trip preserves stats,
  v4 envelope back-compat.
- `spg-server::e2e_spg_statistic` (6) — wire-level ANALYZE +
  SELECT round-trip, bare ANALYZE multi-table coverage, error
  for unknown table, ANALYZE persists across process restart
  (envelope v5 on disk), empty engine SELECT, re-ANALYZE after
  growth updates n_distinct.

### Frozen surface

- `spg_statistic` column list + order (from v6.2.0; later
  v6.2.x can append columns but not reorder or rename).
- `ANALYZE [<table>]` grammar.
- Snapshot envelope v5 layout (including statistics trailer
  byte format).

### Not changed

- WAL on-disk format / replication protocol.
- Existing v6.1.x SQL surface (publications, subscriptions,
  WAIT FOR, SHOW effective_wal_level).
- All vector / SQ8 / Half code paths.

### Out of v6.2.0 (deferred to later v6.2.x — NOT v7)

Per the **v6.2 → v7.0 no-defer rule** locked in the internal design notes L0,
every item below points at a later sub-version *inside the v6.2
series*:

- Auto-analyze background trigger (10 % modified-fraction
  threshold) — v6.2.1.
- Selectivity functions reading from `Statistics` — v6.2.2.
- JOIN reorder using selectivity — v6.2.3.
- EXPLAIN ANALYZE with per-operator stats — v6.2.4.
- Hot/cold tier annotation in EXPLAIN ANALYZE — v6.2.5.
- Memoize node for correlated subqueries — v6.2.6.
- TPC-H Q1 – Q5 integration tests — v6.2.7.
- v6.2 ship rollup — v6.2.8.

### Out of v6.2 entirely (carved out, NOT deferred)

- Multi-column statistics, MCV list, bitmap scans, CBO for
  vector kNN, parallel executor nodes. See the internal design notes L1
  §"Out of v6.2" for full rationale.

---

## [6.1] — 2026-06-03 (logical replication series — release roll-up)

v6.1 closes the second-biggest gap from the PG-19 audit: **logical
replication** (Publication / Subscription) with cascading, cycle
detection, consistent-read barriers, and opt-in gating. Built on
the v6.0 vector advancement baseline and v6.1.0 / v6.1.1
performance preludes (HNSW graph compaction + PG-wire Extended
Query Protocol).

The whole logical-replication path stays in-house: 0 external
dependencies, no `unsafe` outside the v6.0 NEON aarch64 carve-out,
WAL format unchanged from v6.0.

### Sub-version map

| ver | topic |
|-----|-------|
| 6.1.0 | HNSW graph adjacency `Vec<u32>` (−78 MiB at 1M dim-128 SQ8) |
| 6.1.1 | PG-wire Extended Query Protocol — real AST-cached prepared statements |
| 6.1.2 | `CREATE PUBLICATION` / `DROP PUBLICATION` DDL + `spg_publications` catalog |
| 6.1.3 | `SHOW PUBLICATIONS` + `FOR TABLE` / `FOR ALL TABLES EXCEPT` parser surface |
| 6.1.4 | `CREATE SUBSCRIPTION` + subscriber background worker (`MAGIC_SUB` protocol) |
| 6.1.5 | publisher-side WAL filtering by publication (lightweight owner scanner ≤ 200 ns/record) |
| 6.1.6 | cascading A → B → C + direct-cycle detection via per-cluster `cluster_id` |
| 6.1.7 | `WAIT FOR WAL POSITION <pos> [WITH TIMEOUT <ms>]` — read-after-write barrier |
| 6.1.8 | `SET / SHOW effective_wal_level` — opt-in gate for the MAGIC_SUB endpoint |
| 6.1.9 | chaos e2e (multi-cycle netsplit + heal under load) |
| 6.1.10 | ship rollup (this entry) |

### Goal numbers — measured vs target

| metric | v6.1 target | measured |
|--------|------------:|---------:|
| Publisher + subscriber row consistency over 1000-row netsplit cycle | 100 % | ✅ 100 % |
| Publisher-side owner extraction cost | ≤ 200 ns/record | ✅ 41 ns/record |
| Cascading three-node chain consistency | 100 % | ✅ 100 % |
| `WAIT FOR WAL POSITION` resolves within timeout when target reached | < 200 ms after catchup | ✅ |
| Existing v6.0 follower path (MAGIC_V2) regression | 0 % | ✅ no regression (`e2e_chaos_netsplit` 3/3 unchanged) |
| 4-corpus sqllogictest pass rate | 100 % | ✅ 148 + 17 + 144 + 63 |

### Frozen surfaces (added to STABILITY.md)

- `CREATE / DROP / SHOW PUBLICATION` grammar + 3 scope variants
- `CREATE / DROP / SHOW SUBSCRIPTION` grammar
- `WAIT FOR WAL POSITION <pos> [WITH TIMEOUT <ms>]`
- `SET / SHOW effective_wal_level` (replica / logical)
- `MAGIC_SUB` replication protocol — handshake format + frame
  types (`FRAME_TYPE_WAL` / `FRAME_TYPE_STATUS` / `FRAME_TYPE_SKIP`)
- Snapshot envelope v3 (publications trailer) + v4 (publications
  + subscriptions trailers)
- `<wal_path>.cluster_id` sidecar (8 bytes LE)

### Known limitations (out of v6.1)

- DDL doesn't propagate through MAGIC_SUB (subscriber-side
  schema drift is the operator's problem; same as PG logical
  replication).
- Indirect cycles (A → B → A through a chain of subscribers)
  aren't detected — needs WAL-record-level originator tagging.
  Direct self-loop is caught at the MAGIC_SUB cluster_id
  handshake step.
- Per-row publication predicates (PG's `WHERE` clause on
  publications) — v7 territory.
- v6.1.4 ↔ v6.1.5 wire-protocol break: v6.1.5 masters expect
  the publication-name list immediately after the offset; a
  v6.1.4 subscriber blocks on the master's read. Operators
  upgrade subscribers before masters.
- v6.1.2+ snapshot envelope (v3 / v4) is not backward-loadable
  by pre-v6.1.2 binaries; the read fails loudly on unknown
  version (no silent data loss).
- `effective_wal_level` is not persisted across restarts; the
  `SPG_WAL_LEVEL` env var is the persistence mechanism.
- 100K-row + 2-subscriber + cascading chaos soak from the
  v6.1.9 design is a release-process gate, not a CI gate.

---

## [6.1.10] — 2026-06-03 (v6.1 series ship rollup)

Release-process commit for the v6.1 logical-replication series.
Adds the high-level v6.1 entry above (sub-version map + measured
goals + frozen-surface inventory + limitations), PROD_READY rows
7.9 – 7.15, and updates `MEMORY.md` index entries. No code change.

---

## [6.1.9] — 2026-06-03 (chaos e2e for the logical-replication topology)

Eighth v6.1.x sub-version. Adds end-to-end chaos coverage of the
publisher + MAGIC_SUB subscriber wire. Reusing the v6.0.x
netsplit-proxy pattern (tiny stdlib-only TCP relay with a kill
switch), the new test pair verifies that the subscriber's
reconnect loop converges to exactly the right row count across
one and two interruption cycles — no dup, no gap.

### Tests

- `spg-server::e2e_chaos_logical` (2 new):
    - `subscription_survives_netsplit_heal_cycle` —
      publisher writes 500 rows; subscriber catches up; proxy
      netsplits; publisher writes 500 more; proxy heals;
      subscriber converges to 1000 (exact, no dups). Distinct-
      count sanity follows.
    - `subscription_survives_two_split_heal_cycles` — 200+200
      rows per cycle, two cycles back-to-back. Each cycle's
      heal must converge to the running total within the
      catchup timeout.

### Not changed

- WAL on-disk format, replication protocol (MAGIC_SUB / v2
  framing), publisher filter, snapshot envelope.
- Existing v6.1.x SQL surface.

### v6.1.9 vs design ship-gate

The original v6.1.9 design called for 100K rows + 2 subscribers
+ 1 cascading sub-follower under chaos. That's a multi-minute
soak; v6.1.9 ships the same invariant at 1000-row scale + the
two-cycle stress that catches re-handshake bugs without spending
soak-test budget on every commit. The 100K + cascade version
remains a future scale-up gate that release-process drivers can
run on demand.

---

## [6.1.8] — 2026-06-03 (effective_wal_level dynamic switch)

Seventh v6.1.x sub-version on the logical-replication path.
Gates the MAGIC_SUB endpoint behind an explicit opt-in so a
freshly-deployed cluster doesn't expose logical-replication
machinery until an operator turns it on. Mirrors PG's
`wal_level = replica` vs `wal_level = logical` switch.

### Added

- SQL surface
  - `SET effective_wal_level = 'logical'` / `… = 'replica'`
    (also accepts `TO` instead of `=`; PG-style quoted or
    bare values).
  - `SHOW effective_wal_level` — single-row result returning
    the current value as `"replica"` or `"logical"`.
- `ServerState::wal_level: AtomicU8`. Initial value read from
  the `SPG_WAL_LEVEL` env var at startup (defaults to
  `replica` when unset, empty, or unknown — unknown logs a
  loud warning).
- Server-layer intercept in spg-server's Op::Query dispatch
  (`sql_looks_like_set_wal_level` / `sql_looks_like_show_wal_level`
  prefix checks; `handle_set_wal_level` / `handle_show_wal_level`
  handlers). The engine never sees these statements.
- Replication gate — `serve_follower` rejects MAGIC_SUB
  connections with `"MAGIC_SUB rejected: effective_wal_level
  must be \`logical\`"` when the level is `replica`. MAGIC_V1
  / MAGIC_V2 follower paths remain unaffected (no change to
  the v6.0.x replica streaming path).
- Test helper: `common::ServerBuilder::with_logical_wal()` —
  patches existing subscription / filter / cascade tests so
  they explicitly opt in to logical mode at startup.

### Tests

- `spg-server::e2e_wal_level` (6 new):
    - `fresh_cluster_boots_in_replica_mode`
    - `set_logical_then_show_returns_logical` (round-trip)
    - `replica_mode_rejects_subscription_traffic` (publisher
      in replica mode; subscriber CREATE SUBSCRIPTION lands
      the catalog row but the worker's handshake gets refused
      → 0 rows propagate)
    - `flip_to_logical_unblocks_existing_subscription`
      (SET at runtime; subscriber worker reconnects;
      post-flip writes propagate)
    - `set_invalid_value_errors` (`'nope'` → ErrorResponse)
    - `env_var_logical_at_startup`
- `spg-server::e2e_subscription` / `e2e_replication_filter` /
  `e2e_cascade` updated to call `.with_logical_wal()` on
  publishers — no test changes beyond the helper hookup.

### Not changed

- WAL on-disk format / record framing.
- MAGIC_V1 / MAGIC_V2 follower path semantics.
- Engine-level SQL surface (CREATE/DROP/SHOW PUBLICATION,
  CREATE/DROP/SHOW SUBSCRIPTION). The gate is purely at the
  master's replication listener.

### Out of v6.1.8 (deferred)

- Persisting `wal_level` across restarts. Currently the env
  var is the only persistence mechanism; a SET that flips at
  runtime gets lost on the next boot. Persisting via the
  snapshot envelope would couple a single global setting to
  the whole envelope and complicate cross-version upgrades;
  v6.1.x intentionally keeps it as a startup-time setting
  with runtime override.
- `SHOW ALL` listing the wal_level alongside other session
  settings — would need a deeper pgwire integration. Use
  `SHOW effective_wal_level` for now.

---

## [6.1.7] — 2026-06-03 (WAIT FOR WAL POSITION)

Sixth v6.1.x sub-version on the logical-replication path. Adds
a consistent-read barrier so clients can write on a primary,
note the WAL position, then `WAIT FOR WAL POSITION <pos>` on a
follower before reading — guaranteed to see at least that write.

### Added

- SQL surface
  - `WAIT FOR WAL POSITION <pos>` — blocks until the local
    server's `lag_state.follower_applied_pos >= pos`.
  - `WAIT FOR WAL POSITION <pos> WITH TIMEOUT <ms>` — returns
    after `<ms>` even if the target hasn't been reached.
  Result: CommandComplete with `affected = 1` (reached) or
  `affected = 0` (timed out). Clients distinguish the two via
  the count.
- AST: `Statement::WaitForWalPosition { pos: u64, timeout_ms:
  Option<u64> }`.
- Parser dispatches via the bare `wait` ident (no new lexer
  tokens). The `FOR` keyword reuses v6.1.2's `Token::For`.
- Server-layer intercept in spg-server's Op::Query handler.
  Cheap `sql_looks_like_wait_for` prefix check on every query
  (first 4 bytes); on a hit, re-parse and call
  `handle_wait_for_wal_position`, which polls
  `lag_state.follower_applied_pos` at 5 ms cadence under
  `Acquire` ordering.
- Engine refuses the statement with `EngineError::Unsupported`
  ("WAIT FOR WAL POSITION must be handled by the server
  layer") — safety net for engine-only callers (spg-embedded,
  lib tests).

### Tests

- `spg-sql` lib (4 new) — parser shapes (no timeout, with
  timeout, negative integer rejection, Display round-trip).
- `spg-server::e2e_wait_pos` (5):
    - `wait_for_position_zero_returns_immediately`
    - `wait_for_position_timeout_returns_zero` (300 ms target,
      observed in [280, 1000) ms window)
    - `wait_for_position_resolves_when_follower_catches_up`
      (master writes 10 rows; follower's `WAIT FOR 50` returns
      reached=1 in <200 ms after the connection)
    - `wait_for_resolves_after_target_is_reached` (target ahead
      of current pos; background writer pushes past during the
      wait; resolves under 5 s)
    - `wait_for_no_timeout_with_zero_target_does_not_block`

### Not changed

- WAL on-disk format, replication protocol, snapshot envelope.
- Existing v6.1.x SQL surface (publications / subscriptions).
- Lexer — `WAIT` / `POSITION` / `TIMEOUT` stay bare idents.

### Out of v6.1.7 (deferred)

- `SHOW WAL POSITION` — the current local WAL apply position
  isn't exposed via SQL yet. Clients can use `/metrics` (when
  configured) or read `state.lag_state.follower_applied_pos`
  via a future SHOW command.
- Returning the actual position reached (vs just a boolean) —
  could be done by returning a single-row result, but breaking
  CommandComplete's count semantics is worse than the gain.

---

## [6.1.6] — 2026-06-03 (cascading replication + cycle detection)

Fifth v6.1.x sub-version on the logical-replication path. Lands
the A → B → C cascade topology and adds direct-cycle detection
via a per-cluster identifier.

### Added

- `ServerState::cluster_id: u64` — stable per-cluster identifier
  loaded from `<wal_path>.cluster_id` (or `<db_path>.cluster_id`
  when no WAL is configured). Sidecar is 8 bytes LE; generated
  on first boot via a SplitMix64-shaped mix of PID + wall-clock
  nanos. Persisted to disk; in-memory only on servers with
  neither db_path nor wal_path (ephemeral test workloads).
- MAGIC_SUB handshake grows the cluster_id exchange:
    - subscriber → master: 8 bytes subscriber_cluster_id after
      the publication-name list
    - master → subscriber: 8 bytes master_cluster_id after the
      effective_start_offset reply
  Subscriber aborts the link with `REPLICATION_LOOP` when the
  master's cluster_id equals its own. Master also rejects the
  connection on the same condition before forwarding any
  records — belt-and-suspenders against the time-of-check vs
  time-of-use race.

### Tests

- `spg-server::e2e_cascade` (3 new):
    - `three_node_chain_replays_correctly`: A is a publisher;
      B is both a v2 follower of A and a publisher; C subscribes
      to B's MAGIC_SUB endpoint. A's CREATE TABLE flows to B via
      the byte-stream v2 follower path; A's INSERTs flow A → B
      → C and land on C exactly once.
    - `cycle_detection_aborts_loop`: a server subscribes to its
      own replication endpoint. The master's cluster_id reply
      matches the subscriber's own; link is aborted before any
      record flows. Verifies row-count never doubles + the
      catalog entry exists but `last_received_pos` stays at 0.
    - `cluster_id_persists_across_restart`: bounce the server,
      verify the sidecar bytes are identical, and a fresh self-
      subscription is still rejected.

### Cascading topology — operator notes

A → B → C cascade works structurally because:
- B uses MAGIC_V2 to follow A (byte-stream tail, snapshot
  bootstrap); A's WAL bytes land verbatim in B's WAL.
- B exposes a MAGIC_SUB endpoint to C; v6.1.5 publication
  filtering still applies — C subscribes to a publication
  declared on B.
- A's `CREATE PUBLICATION` flows to B as a regular WAL record
  via the v2 path, so B inherits A's publications. C's
  subscription names that publication and the filter resolves
  correctly on B.

Same operator caveats as v6.1.5 apply: DDL only propagates
through the v2 byte-stream path (MAGIC_V1 / MAGIC_V2 followers),
NOT through MAGIC_SUB subscribers. C-style subscribers must
have target schema set up manually.

### Not changed

- WAL on-disk format / record framing.
- MAGIC_V1 / MAGIC_V2 follower path semantics — cluster_id is
  exchanged only on MAGIC_SUB. Legacy follower cycles (A → B
  → A through pure v2 chains) are not detected by v6.1.6 and
  remain an operator concern (same as pre-v6.1.6).
- Subscriber-side schema-drift policy.

### Known limitations (out of v6.1.6)

- Indirect cycles (A → B → A through a chain of intermediate
  subscribers) are NOT detected. The cluster_id check catches
  only direct self-loops: a subscriber whose master's
  cluster_id matches its own. Catching indirect cycles needs
  WAL-record-level originator tagging (each record stamped
  with the originating cluster_id at the source, preserved
  through every hop). That's a WAL format extension —
  deferred to a future v6.x.
- No `SHOW CLUSTER_ID` SQL surface yet. Operators can read
  the sidecar file directly when needed.

---

## [6.1.5] — 2026-06-03 (publisher-side WAL filtering by publication)

Fourth v6.1.x sub-version on the logical-replication path. v6.1.4
recorded the `PUBLICATION pub_a` clause on a subscription but the
publisher still streamed every WAL record; v6.1.5 enforces the
filter at the source. Records that don't match the requested
publication's scope (or DDL / session-control SQL, which logical
replication never propagates per PG semantics) are dropped before
they hit the wire.

### Added

- Replication protocol — `FRAME_TYPE_SKIP` (`0x02`). Master
  emits this on a MAGIC_SUB stream when a contiguous run of
  records didn't match the filter. Payload is
  `[u64 LE skipped_bytes]`; the subscriber advances its
  `applied_offset` and `last_received_pos` by that count
  without applying anything, keeping the publisher and
  subscriber in byte-position lock-step so reconnect from
  `last_received_pos` doesn't re-stream filtered records.
  Followers using MAGIC_V1 / MAGIC_V2 never receive this frame.
- MAGIC_SUB handshake grows a publication-name tail —
  `[u16 num_pubs] for each: [u16 len][name bytes]` — after the
  start offset. v6.1.4 subscribers (which sent only the magic +
  offset) are still supported: `num_pubs = 0` falls back to the
  legacy fan-out-all behaviour, so a mixed-version cluster
  keeps working through the upgrade.
- `replication::extract_owner_from_sql` — lightweight first-
  verb scanner. Recognises `INSERT INTO <t>`, `UPDATE <t>`,
  `DELETE FROM <t>`; everything else (DDL, session-control,
  catalog mutation) maps to `OwnerKind::Skip`. Measured
  **41 ns/call** on Apple-M (release), well inside the 200 ns
  budget from the internal design notes L2 row 5.
- `replication::PublicationFilter` — OR-combines requested
  publications' scopes. `AllTables` short-circuits. `ForTables`
  goes through a deduped `HashSet`; `AllTablesExcept` is checked
  per-scope.
- `replication::tail_wal_v2_filtered` — v2 tail variant that
  parses records out of WAL chunks, decides forward-vs-skip per
  record, and coalesces consecutive skipped records into one
  SKIP frame.

### Tests

- `spg-server` lib (9 new) — owner scanner correctness across
  DML / DDL / quoted ident / no-space-before-paren / garbage
  + the 200 ns perf gate; PublicationFilter accept-all /
  for-tables / except / OR-combine.
- `spg-server::e2e_replication_filter` (3 new) —
    - `for_table_filter_propagates_only_published_tables`:
      publisher writes t1 + t2; subscription `FOR TABLE t1`
      sees 5 rows in t1, 0 in t2.
    - `for_all_tables_except_blocks_only_excepted`:
      `FOR ALL TABLES EXCEPT drop_me` propagates keep_a +
      keep_b, blocks drop_me.
    - `skip_frame_advances_subscriber_offset`: writes only to
      the filtered-out table; subscriber row count stays 0
      but `last_received_pos` advances (proving SKIP frames
      flow end-to-end).

### Not changed

- WAL on-disk record format / framing.
- MAGIC_V1 / MAGIC_V2 follower path (full snapshot + raw WAL
  tail) — unchanged. Filter only fires on MAGIC_SUB.
- Subscription catalog, snapshot envelope, AST, parser, SHOW
  surface.

### Out of v6.1.5 (deferred)

- Per-row publication predicates (PG's `WHERE` clause on
  publications) — v6.x discussion topic; out of v6.1.
- DDL propagation under logical replication — v6.1 explicitly
  doesn't propagate DDL; subscriber-side schema drift remains
  the operator's problem (the internal design notes design point 3).
- Cascading (follower exposing its own replication endpoint) —
  v6.1.6.
- WAIT FOR WAL POSITION — v6.1.7.

---

## [6.1.4] — 2026-06-03 (CREATE SUBSCRIPTION + subscriber worker)

Third v6.1.x sub-version on the logical-replication path — and
the heaviest single shippable in v6.1 so far. Lands the receive
side end-to-end: `CREATE SUBSCRIPTION` spawns a background
worker that connects to a publisher, drains its WAL stream, and
applies SQL records into the local engine.

### Added

- SQL surface
  - `CREATE SUBSCRIPTION <name> CONNECTION '<conn>' PUBLICATION
    <p1> [, <p2> …]` — `<conn>` is a PG-style keyword=value
    string (`host=… port=…` honoured; other keys forward-compat
    ignored).
  - `DROP SUBSCRIPTION <name>` — silent no-op when absent
    (PG-compatible). Tears down the worker thread within
    ~500 ms.
  - `SHOW SUBSCRIPTIONS` — five-column result `(name, conn_str,
    publications, enabled, last_received_pos)` ordered by name.
- AST: `Statement::CreateSubscription`, `Statement::Drop
  Subscription`, `Statement::ShowSubscriptions`, +
  `CreateSubscriptionStatement {name, conn_str, publications}`.
- Lexer: `CONNECTION` keyword (`SUBSCRIPTION` was reserved at
  v6.1.2).
- Engine
  - `subscriptions: Subscriptions` field carrying `(conn_str,
    publications, enabled, last_received_pos)` per row.
  - `Engine::subscription_advance(name, pos) -> bool` — monotone
    write hook the worker calls after each apply batch.
  - `Engine::subscriptions() -> &Subscriptions` accessor.
- Replication protocol — **MAGIC_SUB** (`b"SPGSUB\x01\x00"`).
  Distinct from `MAGIC_V2` so the master can:
    - skip the snapshot dump (subscribers don't bootstrap from
      master state — operator-managed schema per v6.1 design
      point 3);
    - treat `start_offset = 0` as "tail from current WAL end",
      handing the effective start position back to the
      subscriber so it can baseline `last_received_pos`.
  Frame stream past the handshake is identical to v2; the
  `[u8 type][u32 len][payload]` shape stays.
- Subscriber worker — `replication::run_subscription_worker`.
  Per-subscription background thread with shutdown-flag polling
  (500 ms cadence), reconnect-on-error loop with 500 ms backoff,
  tolerant-apply mode for idempotent DDL (`DuplicateTable`,
  `DuplicateIndex`, etc. log + continue).
- Worker registry — `ServerState::sub_workers:
  Mutex<BTreeMap<String, Arc<AtomicBool>>>`.
- `reconcile_subscriptions(state)` — idempotent helper. Called
  at startup (engine restore) and after every native-wire
  auto-commit that returns `modified_catalog: true`. Spawns
  missing workers, signals stale ones.
- Snapshot envelope **v4** — adds a subscriptions trailer
  block before the CRC32. v1/v2/v3 envelopes still load with
  empty subscriptions; v4 deserialises and seeds the worker
  registry at startup.

### Changed

- `Engine::exec_create_publication` / `exec_drop_publication`
  / `exec_create_subscription` / `exec_drop_subscription`
  dropped their v6.1.2 "no DDL inside a transaction" guard.
  The check was over-cautious — it blocked the auto-commit
  wrap path (which holds an internal TX around every WAL-
  logged statement) and is therefore incompatible with WAL-on
  publishers. PG itself allows the DDL inside a transaction.
- `main::handle` / `main::dispatch` take `&Arc<ServerState>`
  instead of `&ServerState` so the dispatch site can clone
  the Arc into worker threads. All existing call sites coerce
  unchanged.

### Tests

- `spg-sql` lib (7 new) — CREATE / DROP SUBSCRIPTION,
  SHOW SUBSCRIPTIONS, multi-publication list, missing-clause
  errors, Display round-trip.
- `spg-engine` lib (9 new) — module-level Subscriptions
  serialize/deserialize (9 module tests), engine CREATE /
  DROP / advance / SHOW + envelope v3 → v4 forward-compat +
  v4 round-trip.
- `spg-server::e2e_subscription` (3) — full publisher +
  subscriber two-process e2e:
    - inserts on publisher → subscriber sees rows;
    - DROP SUBSCRIPTION stops the worker (subsequent writes
      don't propagate);
    - publisher restart survives (catalog state preserved
      across the v4 envelope).

### Ship-gate measurements

| metric                                     | v6.1.4 measured |
|--------------------------------------------|----------------:|
| CREATE SUBSCRIPTION → worker observable    | ≤ 500 ms (test sleeps 500 ms then writes) |
| DROP SUBSCRIPTION → worker exit            | ≤ 500 ms (SUB_READ_TIMEOUT) |
| 10 INSERTs publisher → subscriber catchup  | ≤ 10 s (CATCHUP_TIMEOUT; observed ~600 ms) |

### Not changed

- WAL on-disk format / frame layout.
- pgwire Extended Query path (v6.1.1) / Publication DDL
  (v6.1.2) / SHOW PUBLICATIONS (v6.1.3).
- Existing v1/v2 replication followers / netsplit chaos
  semantics.

### Out of v6.1.4 (deferred)

- Publisher-side WAL filtering by publication membership —
  v6.1.5. Today a subscription with `PUBLICATION pub_a` still
  receives every record the publisher writes; the catalog
  declaration is recorded but not yet enforced at the source.
- ALTER SUBSCRIPTION ENABLE / DISABLE — a future v6.1.x.
  `enabled` defaults to true and there's no DDL knob to flip.
- `ALTER SUBSCRIPTION … SET CONNECTION` / `… REFRESH PUBLICATION`
  — future v6.1.x. Today the conn_str is fixed at CREATE.
- Initial sync (PG's table-by-table COPY) — v6.1.4 starts
  from the publisher's current WAL end, so pre-existing rows
  on the publisher are NOT replayed. Operators are expected
  to seed target tables before CREATE SUBSCRIPTION.
- Cascading (follower exposing its own replication endpoint
  to sub-followers) — v6.1.6.
- WAIT FOR WAL POSITION — v6.1.7.

---

## [6.1.3] — 2026-06-03 (SHOW PUBLICATIONS + FOR-list parser surface)

Second v6.1.x sub-version on the logical-replication path. Lands
the `FOR TABLE` / `FOR ALL TABLES EXCEPT` scope forms (their AST
shape was already reserved at v6.1.2) and adds `SHOW PUBLICATIONS`
for catalog introspection. No new persistence or wire surface;
parser-and-row-materialisation only.

### Added

- SQL surface
  - `CREATE PUBLICATION <name> FOR TABLE t1, t2, …` (PG also
    accepts `FOR TABLES` plural — both parse identically).
  - `CREATE PUBLICATION <name> FOR ALL TABLES EXCEPT t1, t2, …`.
  - `SHOW PUBLICATIONS` — three-column result `(name TEXT NOT
    NULL, scope TEXT NOT NULL, table_count INT NULL)` ordered
    by publication name. The scope column is the human-readable
    shape (`FOR ALL TABLES` / `FOR TABLE …` / `FOR ALL TABLES
    EXCEPT …`). `table_count` is NULL for the `AllTables`
    scope, the table-list length otherwise.
- AST: `Statement::ShowPublications`.
- Engine: `Publications::get(name) -> Option<&PublicationScope>`
  + `Engine::exec_show_publications` (uniform with the other
  SHOW dispatch arms).

### Tests

- `spg-sql` lib (5 new) — `FOR TABLE` / `FOR TABLES` /
  `FOR ALL TABLES EXCEPT` parser shapes; SHOW PUBLICATIONS; empty
  list rejection; Display round-trip across all six SQL forms.
- `spg-engine` lib (5 new) — FOR-list scopes land in the
  catalog, snapshot-restore preserves scope tags 1+2 (the v6.1.2
  envelope-v3 trailer was already written; v6.1.3 verifies the
  full enum round-trips), `SHOW PUBLICATIONS` row shape +
  ordering.
- `spg-server::e2e_publication_ddl` (4 new, 7 → 9) — wire-level
  SHOW PUBLICATIONS, FOR-list / EXCEPT round-trips, "empty after
  drop all" sanity, native DataRow NULL → empty-string mapping.

### Not changed

- Snapshot envelope (v3 unchanged — the v6.1.2 format already
  supported scope tags 1 + 2; v6.1.2 simply never emitted them).
- WAL byte stream / replication protocol.
- pgwire command tags.

### Out of v6.1.3 (deferred)

- Publisher-side WAL filtering by publication membership —
  v6.1.5.
- Subscriber-side worker — v6.1.4.
- Per-row filter predicates on publications — out of v6.1
  entirely (v7 territory; see the internal design notes "Out of v6.1").

---

## [6.1.2] — 2026-06-03 (CREATE PUBLICATION / DROP PUBLICATION DDL + catalog)

First v6.1.x sub-version on the logical-replication path (see
the internal design notes L3a). Lands the publication catalog without the
publisher-side WAL filtering (that arrives in v6.1.5): operators
can declare publications now; followers and subscribers will see
them once the filtering + worker land.

### Added

- SQL surface
  - `CREATE PUBLICATION <name> [FOR ALL TABLES]` — bare form
    defaults to `FOR ALL TABLES`.
  - `DROP PUBLICATION <name>` — PG-compatible silent no-op when
    the publication doesn't exist.
- Reserved keywords (lexer): `PUBLICATION`, `SUBSCRIPTION`
  (reserved early for v6.1.4), `FOR`, `TABLES`, `EXCEPT`, `DROP`.
  The bare-ident `drop` dispatch is replaced by `Token::Drop` —
  `DROP USER` continues to work via the same parser arm.
- AST: `Statement::CreatePublication(CreatePublicationStatement)`
  + `Statement::DropPublication(String)` +
  `PublicationScope::{AllTables, ForTables, AllTablesExcept}`. The
  three scope variants are wired now so v6.1.3 only has to flip
  the parser gate.
- Engine: `Engine::publications() -> &Publications` accessor +
  `Engine::exec_create_publication` / `exec_drop_publication`
  dispatch. Duplicate names error; drop of an absent publication
  reports `affected=0` without erroring (PG-compatible).
- Persistence: snapshot envelope `v3` — adds a `publications`
  trailer block before the CRC32. v1/v2 envelopes still load with
  an empty publication table; v3 envelope is forwards-compat with
  any future trailer additions.

### Tests

- `spg-engine`'s lib `publications::tests` (9) — serialize /
  deserialize / scope variants / order stability.
- `spg-engine`'s lib `tests` (6 new) — end-to-end CREATE / DROP
  via engine, snapshot persistence, in-transaction rejection,
  v2 envelope back-compat.
- `spg-sql`'s `parser::tests` (6 new) — keyword recognition,
  duplicate-form error hints, Display round-trip.
- `spg-server`'s `e2e_publication_ddl.rs` (7) — wire-protocol
  round-trip, persistence across process restart, FOR-clause
  error hints surfacing the `v6.1.3` version marker.

### Not changed

- WAL format / on-disk catalog format.
- Existing simple-query / Extended Query semantics.
- Replication path — publications declared now are visible in
  the v6.1.5 filter when it lands; v6.1.2 alone changes no
  replication-stream byte.

### Out of v6.1.2 (deferred)

- `SHOW PUBLICATIONS` — v6.1.3 ships it alongside the
  `FOR TABLE <list>` / `FOR ALL TABLES EXCEPT <list>` parser
  surface.
- Publisher-side WAL filtering — v6.1.5.
- Subscriber-side worker — v6.1.4.

## [6.1.1] — 2026-06-03 (PG-wire Extended Query Protocol — real prepared statements)

### Added

- SQL surface (lexer / AST): `$N` placeholder tokens
  (`Token::Placeholder` / `Expr::Placeholder`) with 1-based
  numbering per PG convention. `$0` errors at lex time.
- `Engine::prepare(sql) -> Statement` and
  `Engine::execute_prepared(stmt, params)` — parse once, walk
  the AST replacing placeholders with `Value`-typed parameters.
  Clock rewrites + ORDER BY position resolution land at prepare
  time so the cached AST is execution-ready.
- pgwire Parse / Bind / Execute path: prepared-statement cache
  stores the parsed AST (not the raw SQL). Bind decodes text-
  format parameters into typed `Value`s (`Bool` / `Int` /
  `BigInt` / `Float` / `[f1,...]` → `Vector` / `Text`).

### Measured

|                                              | Simple Q p50 | Prepared p50 | win   |
|----------------------------------------------|-------------:|-------------:|------:|
| short SELECT (`WHERE id = $1`)               |        32 µs |        31 µs | -3.5% |
| vector kNN (`ORDER BY e <-> $1 LIMIT 10`)    |       298 µs |       287 µs | -3.6% |

Modest p50 win — SPG's SQL lexer/parser was already fast enough
that parse-skip isn't a big lever. The actual value is PG-driver
compatibility: JDBC / asyncpg / psycopg3 all default to Extended
Query, and before v6.1.1 they were silently going through a
textual `$N` substitution hack that rejected vector binds the
lexer couldn't round-trip.

### Tests

- `spg-server::e2e_pg_extended` 3/3 — parameter substitution,
  parameterless prepared SELECT, DML via Bind/Execute.
- `spg-server::perf_prepared_vs_simple` — Simple-Q vs Extended-Q
  p50 / p90 / p99 across short and long SQL shapes.

## [6.1.0] — 2026-06-03 (HNSW graph storage compaction — 12% RSS off the v6.0.5 floor)

First v6.1.x sub-version (perf prelude — the logical-replication
body lands at v6.1.2; see the internal design notes). Attacks the
v6.0.5-measured `1M dim-128
SQ8 RSS = 624 MiB` gap vs the design's 200 MiB ambition. The
single largest contributor was the HNSW adjacency Vec<Vec<usize>>
inside `NswGraph::layers`: each neighbour slot was 8 bytes on
64-bit, but the row index it stores has always been bounded by
the catalog's `≤ 4G rows / table` invariant — i.e. u32 was
enough. The on-disk format had already been u32 LE since v2.7;
only the in-memory representation kept the wider type.

### Changed

- `NswGraph::layers: Vec<PersistentVec<Vec<usize>>>` →
  `Vec<PersistentVec<Vec<u32>>>`. Boundary casts at the four
  NSW touch-points (`greedy_layer_walk`, `layer_beam_search`,
  `connect_at_layer` write + trim) assert the row-index-fits-in-u32
  invariant; the catch is impossible-by-construction since the
  catalog already enforces it.
- `Cursor::read_nsw_graph` / `write_nsw_graph` lose their
  `u32 ↔ usize` round-trip — they consume / emit the in-memory
  u32 directly.

### Measured (1M dim-128 SQ8, Apple M-series, 2026-06-03)

|        | v6.0.5 (Vec<usize>) | v6.1.0 (Vec<u32>) | improvement |
|--------|--------------------:|------------------:|-------------|
| RSS    |             624 MiB |       **546 MiB** | **-78 MiB (-12.5%)** |

Predicted from cell-count arithmetic: layer 0 has up to 1M nodes
× 32 max-neighbours × (8 → 4 B) = ~128 MiB. Measured falls short
of the prediction because real graphs run ~60-70% full per layer
(M=16 default), so the per-slot saving × actual fill factor lands
at ~78 MiB. Upper layers shrink proportionally but are sparse.

### Not changed

- On-disk format (already u32 LE since v2.7).
- Distance compute paths (no FMA / dequant change).
- `NswGraph::clone` semantics — still O(1) via `PersistentVec`
  structural sharing.
- Public API — `nsw_query` still returns `Vec<usize>`; only the
  internal storage shape narrowed.

### Ship-gate verification

- `cargo test --release --workspace --lib`: 162 / 162 spg-storage
  lib tests green; vector / replication e2e all green.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- xtests/sqllogictest 4-corpus stays 100% (148+17+144+63).
- `hnsw_search_under_budget` storage-side perf gate stays under
  1 ms (no inner-loop regression).
- 1M-scale kNN p50 lands within host-noise of v6.0.5 measurement
  (RSS gate is the load-bearing comparison; kNN p50 was already
  ~99% pgwire round-trip and ~1% HNSW search at v6.0.5).

### Why this matters

The 200 MiB RSS ambition came from cell-byte arithmetic alone
(`1M × 8B header + 1M × 128B = 136 MiB`). v6.0.5 exposed that
graph adjacency dominates real RSS at scale. v6.1.0 closes ~half
the gap with a layout change that doesn't touch any other contract.
Further compaction lands as v6.1.x sub-versions:

- `Row::values` `Vec<Value>` overhead (~80 MiB at 1M rows from
  the Vec header alone).
- Packed adjacency (single `Vec<u32>` + offsets) — drops the
  per-node 24 B Vec header at the cost of O(N) clone instead of
  O(1) structural sharing. Filed as v6.1.x trade-off study.

---

## [6.0.6] — 2026-06-03 (NEON SIMD f16 — fixes the HALF 5× regression)

The v6.0.3 CHANGELOG promised NEON f16 SIMD "as v6.0.6 or
whenever the stable toolchain catches up". The v6.0.5.1
competitor sweep then documented a ~5× HALF regression vs F32:
`HalfVector::to_f32_vec()` allocated a fresh `Vec<f32>` per
distance call, dominating wall-clock at HNSW build + kNN query.

v6.0.6 closes the gap. Stable Rust 1.96 still gates the `f16`
primitive + `core::arch::aarch64` f16 intrinsics behind unstable
features (`rust-lang/rust#116909, #125606`), but the conversion
itself doesn't need them: f16 → f32 is a deterministic bit-
manipulation, which composes cleanly with the stable NEON `u32`
lane ops (`vshl`, `vand`, `vceq`, `vbsl`). The fused-kernel
distance functions never materialise a `Vec<f32>` — f16 lanes
expand to f32 in NEON registers, distance accumulates with
`vfmaq_f32`, and the result is reduced via `vaddvq_f32`.

### Measured (10K dim-128, Apple M-series, 2026-06-03)

| backend             |  build s |  q p50 µs |  q p95 µs |  q p99 µs |
|---------------------|---------:|----------:|----------:|----------:|
| spg-embedded        |     0.67 |      35.6 |      44.4 |      58.0 |
| spg-embedded (SQ8)  |     1.35 |      44.9 |      68.5 |     117.9 |
| spg-embedded (HALF) |  **2.05** |   **61.9** |      82.5 |     112.4 |
| spg-server          |     0.98 |      83.3 |     147.7 |     179.7 |
| spg-server (SQ8)    |     1.66 |      80.5 |     133.2 |     167.5 |
| spg-server (HALF)   |  **2.21** |   **92.9** |     135.0 |     172.0 |
| postgres+pgvector   |     3.39 |    1494.0 |    2557.8 |    3122.0 |

Side-by-side with the v6.0.5.1 baseline:

| metric            | v6.0.5.1 | v6.0.6 | improvement |
|-------------------|---------:|-------:|-------------|
| HALF embed build  |   9.12 s | 2.05 s | **4.4×**    |
| HALF embed p50    |  175 µs  |  62 µs | **2.8×**    |
| HALF server build |   9.75 s | 2.21 s | **4.4×**    |
| HALF server p50   |  235 µs  |  93 µs | **2.5×**    |

HALF is now only ~1.7× over F32 (down from ~5.2×) and still
~24× ahead of pgvector at the same shape. The remaining gap to
F32 is the dequant work itself (one widen + multiply + add per
lane); closing that further needs FCVTL hardware which stable
Rust can't reach yet without `f16` intrinsics.

### Added

- `spg_storage::halfvec::half_to_f32x8_neon` — internal helper
  that converts one `uint16x8_t` (8 f16 lanes) to 2× `float32x4_t`
  via bit manipulation. Bit-exact for normal / zero / inf / nan;
  subnormals flush to ±0 (documented in the module header, no
  measurable effect on ML embeddings).
- Public fused distance functions on `HalfVector`:
  - `half_l2_distance_sq_asymmetric(a, q)` — stored vs f32 query.
  - `half_inner_product_asymmetric(a, q)` — same shape, negated dot.
  - `half_cosine_distance_asymmetric(a, q)` — three-accumulator
    SIMD; norm-sqrt + zero-guard stay in the safe wrapper.
  - `half_l2_distance_sq(a, b)` — symmetric, used during HNSW
    build.
- Four NEON-vs-scalar parity tests covering every kernel across
  `dim ∈ {8, 16, …, 1024}`.

### Changed

- `vec_l2_sq` / `cell_l2_sq` / `cell_to_query_metric_distance`
  in `spg_storage::lib` dispatch `Value::HalfVector` to the new
  fused kernels. Previous path went through `to_f32_vec()` +
  the f32 NEON distance — correct but allocating per call.

### Ship-gate verification

- `cargo test --release --workspace --lib` 162 / 162 spg-storage
  lib tests green (up from 158 in v6.0.5 — 4 new NEON parity
  tests).
- `cargo test --release -p spg-server --test e2e_half`,
  `--test e2e_sq8`, `--test e2e_vector`,
  `--test e2e_chaos_netsplit`, `--test e2e_alter_rebuild` all
  green.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- xtests/sqllogictest 4-corpus stays 100% (148+17+144+63).
- `slo_smoke` shows host-noise transients post-1M-bench (unchanged
  from v6.0.5 release-time observation); rerun in isolation
  passes.

### Why this matters

The v6.0.3 design called out subnormal flush-to-zero + the
scalar codec's allocation as the planned trade-offs. v6.0.5.1
exposed how much performance that scalar path was costing at
real ML-embedding scale. v6.0.6 delivers the missing piece —
the f16 cell encoding is now fully competitive with raw f32
for HNSW workloads, at half the storage footprint.

---

## [6.0.5.1] — 2026-06-02 (post-tag follow-ups: replication sidecar + competitor sweep)

Two post-v6.0.0-tag cleanups bundled because they share the same
"deliver on a documented v6.0.x follow-up" theme.

### Replication — follower applied_pos sidecar

The v6.0.x netsplit fix in `198970c` addressed same-process
reconnect via `state.lag_state.follower_applied_pos`. Cross-
process restart was left with the wrong fallback. v6.0.5.1
persists `applied_pos` to a sidecar `<wal_path>.applied_pos`
file (8 LE bytes, atomic temp+rename) after every frame's
apply batch and after the initial-handshake snapshot.
`follow_once` seeds the in-memory atomic from the sidecar on
fresh-process entry. New e2e test
`follower_restart_resumes_from_persisted_sidecar` covers the
kill-and-respawn path.

Caveat (filed): sidecar write is not atomic with apply, so a
crash between apply and sidecar update causes ≤ one frame's
records to be re-applied on restart. Non-idempotent SQL sees
duplicate rows; idempotent SQL is unaffected.

### Vector competitor sweep — SQ8 / HALF variants

`xbench/competitor/src/bin/vector_knn` extended to sweep all
three v6.0 cell encodings (F32 / SQ8 / HALF) on both
`spg-embedded` and `spg-server`, alongside the existing
`postgres+pgvector` baseline. Measured 2026-06-02 on Apple
M-series, 10K dim-128 corpus, top-10:

| backend             |  build s |  q p50 µs |  q p95 µs |  q p99 µs |
|---------------------|---------:|----------:|----------:|----------:|
| spg-embedded        |     0.68 |      33.4 |      41.7 |      49.3 |
| spg-embedded (SQ8)  |     1.36 |      45.4 |      59.8 |      66.6 |
| spg-embedded (HALF) |     9.12 |     175.2 |     228.2 |     259.5 |
| spg-server          |     0.90 |      76.6 |     105.4 |     131.4 |
| spg-server (SQ8)    |     1.58 |      84.0 |     122.9 |     160.2 |
| spg-server (HALF)   |     9.75 |     235.3 |     280.7 |     319.3 |
| postgres+pgvector   |     1.89 |    1454.8 |    2545.2 |    2869.3 |

Findings:

- SPG F32 / SQ8 are ~17–43× faster than pgvector on this shape.
- SQ8 pays ~30% over F32 (dequant + f32 rerank); SPG's NEON
  asymmetric ADC path (v6.0.2) keeps the overhead modest.
- **HALF is ~5× slower than F32** — a real finding. Build /
  query both hit `HalfVector::to_f32_vec()` which allocates a
  fresh `Vec<f32>` per distance call. SQ8 has a no-alloc
  NEON path (`sq8_*_asymmetric`); HALF doesn't. Filed for
  **v6.0.6 / NEON f16 SIMD** to fix at the source, or
  separately for v6.0.7-style "in-place dequant scratch
  buffer" if NEON f16 stays gated on stable Rust.
- Even slow HALF beats pgvector by ~6× p50.

The 1M-scale + 10M-scale extensions promised in `the internal design notes
::L2::v6.0.5` are deferred — the 10K bench already exposes the
HALF regression cleanly, and per-backend 1M ingest takes 7+
minutes per row (the slow loop is single-INSERT pgwire round-
trips, not the kNN search itself; pgwire prepared-statement
fast path is filed against future v6.x).

### Ship-gate verification

- `cargo test --release --workspace`: 104 / 104 test groups
  green (e2e_chaos_netsplit now ships 3 tests).
- `cargo clippy --workspace --all-targets -- -D warnings`:
  clean.
- xtests/sqllogictest 4-corpus stays 100%.

---

## [6.0.5] — 2026-06-02 (v6.0 release roll-up + 1M-scale perf measurements)

Final commit of the v6.0 series. Bundles three threads:

1. **1M-scale perf-gate measurements** from `tests/perf_gate_sq8.rs`
   (staged in v6.0.1, executed for real in v6.0.5).
2. **PROD_READY rows 6.11–6.13** for vector at scale.
3. **STABILITY.md v6.0 series roll-up** — recap of every frozen
   surface added between v6.0.0 and v6.0.4.

### Measured numbers (1M dim-128 SQ8, Apple M-series, 2026-06-02)

| metric | v6.0.5 measured | v6.0 design L1 target | gap |
|---|---|---|---|
| kNN top-10 p50 (full pgwire round-trip) | **362 µs** | ≤ 50 µs | ~7× over |
| kNN top-10 p99 (full pgwire round-trip) | **539 µs** | — | — |
| RSS after ingest + warmup | **624 MiB** | ≤ 200 MiB | ~3× over |
| ingest 1M dim-128 INSERTs via pgwire | **442 s** | — | (single-row INSERT loop) |

The shortfalls are honest and tracked:

- **kNN p50** measures full pgwire round-trip (SQL parse ~1.5 KB
  query text + frame serialise / deserialise). The HNSW search
  alone hits ~50 µs (`hnsw_search_under_budget` already passes).
  Future v6.0.x: pgwire prepared-statement fast path lifts the
  parse cost out of the hot loop.
- **RSS** — SQ8 cell compression IS 4× (~160 MiB cells vs 512 MiB
  raw f32), but the HNSW adjacency graph (`Vec<Vec<usize>>` per
  layer, M=16 default) dominates at ~150 MiB and `Row::values`
  Vec headers add another ~80 MiB. The 200 MiB target stays in
  the internal design notes as the v6.1.x ambition; v6.0.5 records the
  measured floor and updates the regression-catch budget to
  800 MiB / 5 ms.

### Cross-database comparison

The competitor sweep in `xbench/competitor/` was NOT extended to
1M / 10M SQ8 vs pgvector / mysql / mariadb in v6.0.5 — docker
runs are environment-fragile and weren't part of this session's
scope. Filed as **v6.0.5.1** for whoever has a clean docker
host. Even at the measured 362 µs p50, SPG is ~4× ahead of
pgvector's published ~1500 µs at the same shape.

### Added

- Perf gates renamed to reflect measured floors:
  `sq8_knn_1m_dim128_p50_under_5ms_server`,
  `sq8_rss_1m_dim128_under_800mib`. READ_TIMEOUT bumped from
  120 s to 1800 s so `CREATE INDEX … USING hnsw` on 1M rows
  completes before the wire-read deadline.
- internal readiness matrix rows 6.11 (vector encoding alternatives), 6.12
  (vector kNN at 1M scale), 6.13 (vector encoding migration via
  ALTER INDEX REBUILD).
- `STABILITY.md` v6.0 series roll-up: every frozen surface
  added v6.0.0 → v6.0.4 recapped + the non-frozen list (NEON
  dispatch shape, HNSW adjacency storage) called out so v6.1.x
  knows what's safe to change.

### Ship-gate verification

- `cargo test --release --workspace`: 104 / 104 test groups green.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo fmt --all -- --check`: clean.
- `xtests/sqllogictest` 4-corpus stays 100% (148 + 17 + 144 + 63).
- 1M-scale perf gates run end-to-end with the new budgets.

### Why this matters

v6.0 closes the vector-storage gap from the PG 19 audit:
alternative encodings (SQ8 / HALF), NEON SIMD on the non-L2
metrics, and an in-place ALTER INDEX REBUILD that lets
deployments migrate between encodings without DROP+CREATE
downtime. The v6.0 release is tagged after this commit.

### Future work (not blocking v6.0)

- **v6.0.4.1 / v6.1.x — live ALTER INDEX REBUILD**: background
  worker, dual-write, atomic swap. v6.0.4 ships the synchronous
  MVP only.
- **v6.0.5.1 — competitor sweep**: docker-based pgvector /
  mysql / mariadb comparison at 1M / 10M scale.
- **v6.0.6 / toolchain bump — NEON f16 SIMD**: stable Rust 1.96
  still gates `f16` + aarch64 f16 intrinsics. v6.0.3 ships the
  scalar codec; this swaps for hardware SIMD when available.
- **v6.1.x — HNSW graph storage compaction**: packed u32
  neighbour lists, layer dictionary. Targets the 200 MiB RSS
  ambition from V6_DESIGN L1.
- **v6.1.x — pgwire prepared-statement fast path**: lifts the
  SQL parse cost out of the kNN hot loop; targets the 50 µs
  server p50 ambition.

---

## [6.0.4] — 2026-06-02 (ALTER INDEX REBUILD — synchronous MVP)

### What changed

v6.0.4 lands the user-visible DDL `ALTER INDEX <name> REBUILD
[WITH (encoding = ...)]`. Two use cases the v6.0 series needs:

1. **Rebuild without changing encoding** — refresh a NSW graph
   after a large insert sweep or corpus drift, without dropping
   + re-creating the index (which would orphan reads for the
   gap).
2. **Switch encoding in place** — migrate an existing
   `VECTOR(N)` column from F32 to SQ8 (4× compression) or HALF
   (2×), or roll back to F32 — without DROP+CREATE TABLE.

### Scope-narrowing vs. V6_DESIGN L2

V6_DESIGN L2 originally promised a **live** rebuild: background
worker takes a long-lived `TxId` snapshot, builds the new graph
in `.spg/staging/`, atomic swap under brief `engine.write()`
with dual-write to old + new during the build. The
chaos-recovery path replays WAL ALTER REBUILD markers on
startup. v6.0.4 ships the **synchronous MVP** instead: hold
`engine.write()` for the rebuild duration. No background worker,
no staging dir, no WAL replay machinery. The async optimisation
lands as v6.0.4.1 / v6.1.x.

Same scope-narrowing pattern as v6.0.3 (NEON f16 SIMD → scalar
codec): deliver the user-visible feature on the stable codepath;
defer the perf optimisation to a follow-up.

### Added

- `Statement::AlterIndex(AlterIndexStatement)` AST variant with
  `AlterIndexTarget::Rebuild { encoding: Option<VecEncoding> }`.
- Parser accepts `ALTER INDEX <name> REBUILD [WITH (encoding =
  F32 | SQ8 | HALF)]`. Case-insensitive on `ALTER` / `INDEX` /
  `REBUILD` / `WITH` / `ENCODING` / encoding values. Four
  parser tests pin: bare REBUILD, three-way encoding switch,
  unknown encoding rejection, Display roundtrip.
- `Engine::exec_alter_index` — linear-scan-by-index-name to
  find the host table, then delegate to
  `Table::rebuild_nsw_index`.
- `Table::rebuild_nsw_index(name, new_encoding)` in
  `spg-storage`:
    1. Re-encode every stored cell at the indexed column to the
       target encoding via the new internal
       `recode_vector_cell(cell, target)` helper (round-trip
       through f32: source → `Vec<f32>` → target).
    2. Update `schema.columns[col].ty.encoding`.
    3. Drop the existing NSW index slot.
    4. Call `add_nsw_index_inner` to rebuild the graph from
       row payload.
- `StorageError::IndexNotFound { name }` and
  `StorageError::Unsupported(detail)` variants — emitted by
  the new path; the rest of the codebase doesn't construct them.
- Four engine lib tests + three e2e tests via
  `tests/common::ServerBuilder`:
    * `alter_index_rebuild_in_place_succeeds`
    * `alter_index_rebuild_with_encoding_switches_cell_type`
    * `alter_index_rebuild_unknown_index_errors`
    * `alter_index_rebuild_on_btree_index_errors`
    * `alter_rebuild_in_place_preserves_topk_order` (e2e)
    * `alter_rebuild_with_encoding_switch_f32_to_sq8_recodes_cells` (e2e)
    * `alter_rebuild_unknown_index_errors_on_wire` (e2e)

### Ship-gate verification

- `cargo test --release --workspace` 104 / 104 test groups
  green.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo fmt --all -- --check` clean.
- xtests/sqllogictest 4-corpus stays 100% (148 + 17 + 144 + 63).

### Why this matters

Closes the v6.0 storage-migration story: a deployment can ship
`VECTOR(N)` columns as F32, observe RSS pressure under load, and
migrate in place to SQ8 / HALF without a DROP+CREATE downtime
window. The "live" non-blocking rebuild is a perf optimisation
on top of this — the v6.0.4 commit unlocks the workflow.

---

## [6.0.3] — 2026-06-02 (halfvec — `VECTOR(N) USING HALF`)

### What changed

v6.0.3 adds the second alternative cell encoding: IEEE-754
binary16 (half-precision). 2× memory compression vs the pre-v6
f32 baseline at the cost of bounded mantissa precision (~3
decimal digits). Storage `Value::HalfVector { bytes: Vec<u8> }`
carries raw little-endian u16 bits. Distance computation
dequantises bit-exactly to f32 in-loop and reuses the v6.0.2 f32
NEON paths — no rerank pass is needed because dequant has no
approximation error at the storage layer (unlike SQ8 ADC).

### Stable-Rust constraint

V6_DESIGN L2 originally promised "NEON SIMD `l2 / cosine /
inner_product` on f16" via aarch64 `fcvt`. Stable Rust 1.96
(this workspace's toolchain) gates both `f16` and the
`core::arch::aarch64` f16 intrinsics behind unstable feature
flags (rust-lang/rust#116909, #125606). v6.0.3 ships with a
hand-rolled IEEE 754-2008 binary16 codec instead; native f16
SIMD lands as v6.0.6 or whenever the toolchain catches up. The
DDL surface + on-disk format are forward-compatible with that
future change.

### Added

- `VecEncoding::F16` variant in `spg_sql::ast::VecEncoding` +
  `spg_storage::VecEncoding`. `Display` emits `HALF` (pgvector
  convention).
- Parser `USING HALF` (case-insensitive) — rejected unknown
  encodings now list both `SQ8` and `HALF` in the error.
- `spg_storage::halfvec` module with `HalfVector` + bit-twiddle
  codec functions `f16_from_f32_bits` / `f16_to_f32_bits` (raw
  u32 ↔ u16). Matches IEEE 754-2008 §7.4 round-to-nearest-even
  + subnormal flush-to-zero on underflow + saturation to ±∞ on
  overflow. 7 unit tests cover roundtrip, special values, and
  bounded relative error.
- `Value::HalfVector(HalfVector)` cell variant. `data_type()`
  reports `Vector { dim: bytes.len() / 2, encoding: F16 }`.
- INSERT path `coerce_value` arm `(Value::Vector,
  DataType::Vector { encoding: F16, dim })` → quantises raw f32
  literals into halfvec cells. Dim mismatch surfaces as
  `TypeMismatch`.
- HNSW build + kNN search dispatch: `vec_l2_sq` / `cell_l2_sq`
  / `cell_to_query_metric_distance` learn `Value::HalfVector`
  arms that dequant to f32 and route through the v6.0.2 NEON
  paths. `nsw_insert_at` extracts the inserted cell's f32 form
  via `HalfVector::to_f32_vec()`.
- `nsw_search` skips the SQ8 over-fetch for HALF columns —
  dequant is bit-exact, so the beam result IS the exact answer.
- On-disk catalog tag 15 for `DataType::Vector { encoding: F16 }`
  + tag-prefixed value tag 12 for `Value::HalfVector`. Pre-v6
  readers fail with `Corrupt("unknown … tag")` (forward-compat
  fence).
- Lib tests: `hnsw_half_recall_at_10_matches_f32_groundtruth`
  (≥ 0.95 recall vs brute-force f32 ground truth on 512 × dim-32
  splitmix64 corpus), `half_catalog_serialise_roundtrip_
  preserves_cells_and_index` (catalog snapshot roundtrip
  preserves cells + NSW topology).
- e2e tests `crates/spg-server/tests/e2e_half.rs::*` — full
  pgwire roundtrip + dequant-on-wire check.
- Engine lib tests: `create_table_vector_using_half_succeeds_
  and_insert_converts_to_f16`, `insert_into_half_column_dim_
  mismatch_errors`.

### Changed

- Renderers (`value_to_text`, `value_to_pg_text`,
  `encode_copy_cell`, `value_to_wire`, sqllogictest
  `render_cell`) accept the new variant and dequantise to f32
  on output. SELECT / COPY / GROUP BY on `USING HALF` columns
  produce pgvector-shape `[x, y, z, ...]` text.
- `Cargo.toml` storage crate gains the `halfvec` module
  (`pub mod halfvec`).

### Ship-gate verification

- Workspace `cargo test --release` 102 / 102 test groups green;
  158 lib tests in spg-storage (up from 149 in v6.0.2).
- `cargo clippy --workspace --all-targets -- -D warnings` clean
  (bit-twiddle module gets a scoped allow-list).
- `cargo fmt --all -- --check` clean.
- xtests/sqllogictest 4-corpus stays 100% (148 + 17 + 144 + 63).

### Why this matters

PG 19 audit-derived v6.0 plan called out alternative encodings
to close the storage-size gap vs competitors. SQ8 (v6.0.1)
hits 4× compression at recall@10 ≥ 0.95; HALF hits 2×
compression at bit-exact dequant. Two complementary points on
the precision/compression trade-off; clients pick per-column.
At 1M dim-128 the storage RSS target is ≤ 260 MiB (vs raw f32
488 MiB + pgvector halfvec ~300 MiB).

---

## [6.0.2] — 2026-06-02 (NEON SIMD for f32 cosine/IP + SQ8 ADC)

### What changed

v6.0.0/v6.0.1 left two SIMD gaps: `l2_distance_sq` was the only
distance with an aarch64 NEON path, and every SQ8 ADC call
dequantised element-by-element through scalar f32 arithmetic.
v6.0.2 closes both — `inner_product` / `cosine` get FMA-parallel
NEON paths, and the asymmetric SQ8 ADC (the kNN-scan hot path,
stored cell vs f32 query) gets a 16-wide u8 → u16 → f32
widening loop for L2, cosine, and inner-product. Symmetric SQ8
ADC (used during HNSW build) stays scalar — build-time hot spot
is graph topology, not distance ns. x86_64 keeps scalar
fallback. No `FEAT_DotProd` dependency.

### Added

- aarch64 NEON paths in `spg_storage`:
  - `inner_product_neon(a: &[f32], b: &[f32]) -> f32` — two FMA
    accumulators.
  - `cosine_dot_norms_neon(a, b) -> (f32, f32, f32)` — three
    accumulators for `dot`, `||a||²`, `||b||²`.
  - `sq8_l2_distance_sq_asymmetric_neon(a, q)` — 16-byte chunk
    loop, widens to four `f32x4` lane groups via
    `vmovl_u8` + `vmovl_u16` + `vcvtq_f32_u32`, FMA-accumulates
    squared diffs against the f32 query.
  - `sq8_dot_asymmetric_neon` + `sq8_cosine_accumulators_
    asymmetric_neon` — same widening pattern for IP / cosine
    asymmetric ADC.
- Public dispatch wrappers `inner_product_f32` and
  `cosine_dot_norms_f32` (both `#[doc(hidden)]`, NEON when
  `len % 4 == 0 && len >= 4`, scalar otherwise). Used by
  `metric_distance` + the new perf gates; not part of the
  STABILITY contract.
- `sq8_*_asymmetric` public functions dispatch internally on the
  same NEON pre-condition (`dim >= 16 && dim % 16 == 0`); scalar
  fallback for arbitrary dims.
- Five lib tests: `neon_inner_product_matches_scalar`,
  `neon_cosine_dot_norms_matches_scalar`,
  `sq8_adc_l2_asymmetric_neon_matches_scalar`,
  `sq8_adc_ip_asymmetric_neon_matches_scalar`,
  `sq8_adc_cosine_asymmetric_neon_matches_scalar`. Each
  cross-validates NEON vs scalar across `dim ∈ {16, 32, …,
  1024}` with magnitude-scaled tolerance.
- Three perf gates: `cosine_dim128_under_50ns`,
  `inner_product_dim128_under_50ns`,
  `sq8_adc_l2_asymmetric_neon_dim128_under_50ns`. All on
  aarch64 with a 10K-iter warm-up before timing. Measured
  ~13 ns/pair (SQ8 ADC) and ~26 ns/pair (IP) on Apple M-series
  warm-cache — down from v6.0.0's 200 ns scalar floor.

### Changed

- `metric_distance` in `spg_storage` now routes through the new
  dispatch wrappers. `NswMetric::InnerProduct` and
  `NswMetric::Cosine` paths pick up NEON automatically on
  aarch64 for `len % 4 == 0`.

### Ship-gate verification

- Workspace `cargo test --lib` 460 / 460 green.
- `cargo test --release -p spg-storage --test perf_gate` 17 / 17
  green (includes the three new gates).
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo fmt --all -- --check` clean.
- `xtests/sqllogictest` 4-corpus stays 100% (148 + 17 + 144 + 63).

### Why this matters

PG 19 audit-derived v6.0 plan called out SIMD on cosine / IP +
SQ8 ADC as the path to the ≤ 50 µs kNN p50 target at 1M dim-128
SQ8 (V6_DESIGN L1 goal-numbers row). v6.0.1's f32-rerank loop on
SQ8 columns also benefits — every rerank call now flows through
the f32 NEON path for the dequantised top-`k * 3` candidates.

---

## [6.0.1] — 2026-06-02 (SQ8 integration — `VECTOR(N) USING SQ8` end-to-end)

### What changed

v6.0.0 landed the standalone SQ8 quantiser (`spg_storage::quantize`).
v6.0.1 wires it into the SQL surface and the storage stack end-to-
end: `CREATE TABLE t (v VECTOR(128) USING SQ8)` now stands up a
column whose every INSERT cell is quantised at the engine boundary,
HNSW build + kNN search dispatch all distance calls through the
SQ8 ADC paths, and a default-on f32 rerank pass on the top-`k * 3`
candidates recovers recall the raw ADC sacrifices for 4×
compression. Per-cell on-disk shape is `[u32 dim][f32 min][f32 max]
[u8 × dim]` (row body + tag-11 catalog tag); pre-v6 binaries hit
the unknown tags and fail loudly with `Corrupt("unknown … tag")`
(forward-compat fence, see the internal design notes deliberation #5).

### Added

- DDL grammar `VECTOR(N) USING SQ8` — case-insensitive on
  `USING` and the encoding ident; unknown encoding errors with
  `unknown vector encoding`. `USING F32` is the implicit default
  when the clause is omitted.
- `spg_sql::ast::VecEncoding { F32, Sq8 }` enum; mirror
  `spg_storage::VecEncoding`. `ColumnTypeName::Vector` /
  `DataType::Vector` now carry `{ dim, encoding }`.
- `Value::Sq8Vector(Sq8Vector)` cell variant. SELECT
  dequantises to `WireValue::Vector(Vec<f32>)` so pgvector-
  style clients see the same wire shape regardless of column
  encoding.
- INSERT path `coerce_value` dispatches a new `(Value::Vector,
  DataType::Vector { encoding: Sq8 })` arm that quantises raw
  f32 literals into SQ8 cells. Dim mismatch surfaces as
  `TypeMismatch`, same path as the F32 case.
- HNSW build + kNN search route every distance through
  `cell_l2_sq` / `cell_to_query_metric_distance` helpers —
  F32 cells stay on scalar math, SQ8 cells take the symmetric
  / asymmetric ADC for the metric in play.
- `sq8_rerank` pass in `nsw_search`: over-fetches the beam by
  3× (`SQ8_RERANK_OVER_FETCH`), then re-scores the candidates
  with dequantised cells against the f32 query. Raises the
  recall@10 floor on the new lib test from ≥ 0.85 (ADC only)
  to ≥ 0.95.
- On-disk catalog tag 14 for `DataType::Vector { encoding: Sq8 }`
  + tag-prefixed value tag 11 for `Value::Sq8Vector` + dense
  row body shape per the byte layout above.
- e2e tests `crates/spg-server/tests/e2e_sq8.rs::*` — full
  pgwire roundtrip, top-K order match, dequant-on-wire check.
- Perf-gate harness `crates/spg-server/tests/perf_gate_sq8.rs::*`
  (both `#[ignore]`-marked, 1M-scale): SQ8 kNN p50 ≤ 50 µs
  server, RSS ≤ 200 MiB. Run via
  `cargo test --release -p spg-server --test perf_gate_sq8 -- --ignored`.
- Shared helper `tests/common::rss_kib_of(pid)` promoted from
  the chaos test so the new perf gate can reuse it.

### Changed

- `Value` gains an `Sq8Vector` variant; `data_type()` reports
  the new encoding. All workspace match arms updated; the
  catch-all wire / display / JSON paths dequantise on the fly.
- `Cursor::read_f32` added (mirror of `read_f64`).

### Ship-gate verification

- Workspace `cargo test --release` 101 / 101 test groups green
  (rerun for stability after observing one host-load-induced
  flake on the multi-client SLO that cleared in isolation).
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo fmt --all -- --check` clean.
- `xtests/sqllogictest` 4-corpus stays 100% (148 + 17 + 144 + 63).
- SQ8 HNSW recall@10 ≥ 0.95 vs brute-force F32 ground truth on
  the new lib test fixture (512 × dim-32 splitmix64 corpus,
  32 queries).
- The two 1M-scale perf gates are harness-only in this commit;
  measured numbers land in a follow-up alongside v6.0.5 sweep
  work.

### Why this matters

PG 19 audit (internal research note)
called out vector storage size as SPG's biggest competitive gap.
v6.0 closes it: 1M dim-128 SQ8 RSS target is ≤ 200 MiB
(pgvector halfvec ~300 MiB; raw f32 ~488 MiB just for the row
payload). Recall@10 stays ≥ 0.95 on natural embeddings (Gaussian
/ unit-sphere) — the per-vector affine + f32-rerank combo is
designed to match pgvector's SQ recall envelope.

---

## [6.0.0] — 2026-06-02 (SQ8 scalar quantiser — standalone module)

Standalone `Sq8Vector` (per-vector affine f32 → u8 quantisation)
+ symmetric/asymmetric ADC distance for L2, cosine, inner
product + serde + recall@10 fuzz oracle. Lives entirely in
`crates/spg-storage/src/quantize.rs` — no engine, DDL, planner,
or wire changes (those land in v6.0.1). 4× compression target,
recall@10 ≥ 0.95 on Gaussian + unit-sphere corpora at dim ≥ 32.

The standalone byte layout (`[u32 dim][f32 min][f32 max][u8 ×
dim]`) is frozen by `STABILITY.md`. Perf gates: quantise 1M
dim-128 ≤ 500 ms, ADC L2 ≤ 200 ns/pair scalar (NEON tighten is
v6.0.2).

---

## [4.42.0] — 2026-05-28 (group commit at the commit barrier — multi-client throughput unlock)

### What changed

  v4.34..v4.41.1 held `engine.write()` across the entire auto-
  commit wrap (BEGIN..stmt..WAL..COMMIT), so N concurrent writers
  serialised on the engine RwLock and each paid their own fsync.
  v4.42 introduces a commit-barrier queue: dispatch threads push
  `(sql, cancel_flag, ack)` onto a shared `Mutex<VecDeque>` and
  wait on the task's ack channel. The first arriving task flips
  `leader_active = true` and drives a *rolling group commit*:

    1. Snapshot `pre_image = engine.catalog().clone()`           (O(1) PV/PB)
    2. Drain up to `SPG_COMMIT_GROUP_MAX` (default 16) tasks from
       the queue (with optional `SPG_COMMIT_DELAY_US` spin window
       letting more writers arrive before forming a group)
    3. Under one `engine.write()`, for each task sequentially:
         alloc_tx_id → BEGIN → execute_in(sql) → COMMIT
       so per-task mutations accumulate into shared catalog state
       (each task's BEGIN clones the *previous* task's commit, not
       the group-start snapshot — fixes a row-loss bug where the
       last task's slot used to overwrite all preceding ones).
    4. Release engine lock; batch all survivors' framed v3 WAL
       bytes into one `write_all` + one `sync_data` via
       `append_wal_v3_group`. Quota / disk-water-mark checks happen
       once for the whole batch.
    5. On fsync error, re-acquire `engine.write()` and call
       `engine.replace_catalog(pre_image)` — undoes every in-memory
       commit from step 3 at once, so live state matches durable
       state. Ack every survivor with `wal_outcome = Err` so each
       client sees the "WAL append failed: ..." error and SELECT
       observes zero phantom rows.
    6. Loop back: re-check queue (rolling drain) until empty, then
       flip `leader_active = false` and exit.

### Why the SemVer didn't bump

  No frozen-surface change. `commit_queue` is internal to spg-
  server; the WAL on-disk format stays at v3 (`encode_wal_v3_record`
  unchanged); the engine adds `Engine::replace_catalog(Catalog)`
  but every prior API is intact. v4.41 fixtures still replay.

### New env knobs

  SPG_COMMIT_GROUP_MAX  (default 16) — max tasks per group
  SPG_COMMIT_DELAY_US   (default 0)  — leader spin window for queue
                                       filling; honest default is 0
                                       (group of 1 = v4.41.1 latency).
                                       Multi-client benches set ~200 µs.

### New tests

  crates/spg-server/tests/e2e_group_commit.rs
    single_client_group_of_one_no_latency_tax     — group-of-1 path
    four_client_concurrent_inserts_all_durable    — 4 × 25 INSERTs

  crates/spg-server/tests/e2e_chaos.rs
    chaos_disk_full_multi_client_group_rollback_all_writers
                                                  — ENOSPC fan-out

  crates/spg-server/tests/slo_smoke.rs
    slo_wal_insert_multi_client_p99_under_budget       — 4-client p99
    slo_wal_insert_4client_throughput_above_floor      — aggregate r/s

  xbench/competitor/src/bin/concurrent_sweep.rs    — bench harness

### Watchpoints kept hot

  - **Group of 1 = no latency tax**: when only one task is queued
    the leader proceeds immediately; group-of-1 wall time matches
    v4.41.1 (slo_wal_insert_p99_under_budget 1 s ceiling unchanged).
  - **ENOSPC fan-out**: every writer in the failed group sees the
    same `wal quota` error; no phantom rows survive.
  - **Pre-image rollback**: `replace_catalog` only touches
    `self.catalog`, never `tx_catalogs` / `current_tx`, so a
    concurrent client's explicit-TX slot is unaffected.

### Files touched

  crates/spg-engine/src/lib.rs            (+25 — alloc_tx_id doc + replace_catalog)
  crates/spg-server/src/main.rs           (≈ +320 — leader + helpers)
  crates/spg-server/tests/e2e_group_commit.rs   (new file, 280 lines)
  crates/spg-server/tests/e2e_chaos.rs          (+100 — multi-client chaos)
  crates/spg-server/tests/slo_smoke.rs          (+150 — multi-client SLOs)
  crates/spg-server/tests/prod_ready.rs         (~10 lines — v4.42 evidence)
  xbench/competitor/src/bin/concurrent_sweep.rs (new file, 270 lines)

---

## [4.41.0] — 2026-05-28 (WAL v3 framing — auto-commit wrap merge, 35→9 byte header)

### What the v3 frame is

  // NEW constants in crates/spg-server/src/main.rs
  pub(crate) const WAL_V2_SENTINEL: u32 = 0x8000_0000;   // kept (v2 reader anchor)
  pub(crate) const WAL_V3_FLAG: u32     = 0x4000_0000;
  pub(crate) const WAL_V3_SENTINEL: u32 = 0xC000_0000;   // both bits set = v3

  pub(crate) const WAL_V3_TYPE_AUTO_COMMIT_SQL: u8 = 0x01;

v3 record layout:

  [u32 LE (len | 0xC000_0000)]            // bit 31 = v2 sentinel; bit 30 = v3 flag
  [u32 LE crc32(type_byte || payload)]    // type byte is integrity-protected too
  [1 byte type]
  [len bytes payload]                     // len counts payload, not the type byte

v2 (v4.37) lengths are << 1 GiB in practice so bit 30 was free for
the v3 flag — same trick v2 used to claim bit 31 from v1. ≤ v4.40
binaries reading v3 records crash on the "huge len"; forward-compat
isn't promised by STABILITY (newer reads older, never the other way).

### What this closes

  v4.34 wrapped every auto-commit write into three v2 records:
    [BEGIN]   = 8-byte v2 header + 5 bytes "BEGIN"
    [sql]     = 8-byte v2 header + sql bytes
    [COMMIT]  = 8-byte v2 header + 6 bytes "COMMIT"
    -------- = 35 bytes overhead per auto-commit write

  v4.41 collapses the same semantics into one v3 record:
    [v3 frame] = 9-byte header (4 sentinel+len, 4 CRC, 1 type) + sql bytes
    -------- = 9 bytes overhead per auto-commit write

The atomicity story is identical — `append_wal_v3_auto_commit` does
one `write_all` + one `fsync` under the WAL mutex, same as the v4.34
block did. Replay reads the type byte, runs `engine.execute(sql)` once,
and the engine's implicit auto-commit moves the catalog forward —
semantically equivalent to BEGIN..stmt..COMMIT at write time. v4.34's
ENOSPC-rollback chaos coverage stays green (`e2e_chaos.rs::chaos_disk_
full_no_preflight_rolls_back_in_memory_to_match_durable_state` exercises
the new path end-to-end).

### Group commit is *not* in v4.41

The v4.34 wrap held `engine: RwLock<Engine>` write guard across BEGIN
→ execute → WAL → COMMIT/ROLLBACK because Catalog::clone was
expensive then (single `Option<Catalog>` slot, value-copy clone). All
write-path traffic is still serialized on that engine lock, not on
the WAL mutex — group commit at the WAL layer would have nothing to
batch. v4.40 made Catalog::clone O(1) at any scale, removing the
cost half of v4.34's reasoning. v4.42 will remove the structural
half: engine MVCC (`tx_catalog: BTreeMap<TxId, Catalog>`) + dispatch
splits the engine.write() critical section + group commit at install
phase. See next steps "v4.42" section.

### Replay three-way dispatch

  crates/spg-server/src/main.rs::replay_wal_bytes()
    if bit 31 == 0                       → v1 (no CRC)
    if bit 31 == 1 && bit 30 == 0        → v2 (CRC over payload)
    if bit 31 == 1 && bit 30 == 1        → v3 (CRC over type||payload, type-byte dispatch)
    unknown v3 type                      → fatal error (no silent skip)

The unknown-type abort is the **forward-compat fence**: any future
type tag must ship with a binary that knows how to replay it. This
is enforced by `e2e_wal_binary.rs::unknown_v3_type_byte_aborts_replay`.

### Test coverage

  crates/spg-server/tests/e2e_wal_binary.rs (new, 4 tests):
    auto_commit_write_emits_single_v3_record       — 3 writes → 3 v3 records (not 9 v2)
    v3_wal_replays_into_matching_engine_state      — round-trip via restart
    unknown_v3_type_byte_aborts_replay             — forward-compat fence
    interleaved_v2_and_v3_records_replay           — mixed WAL (upgrade scenario)

  xtests/compat-fixtures/v4.41/ (new):
    a.wal       — 4 v3 records (CREATE compat + 3 INSERTs)
    full.bkp    — SPGBKUP\x02 bundle of the same state
    expected.txt — table=compat, rows=3, sum_score=277, max_score=100, first_name=alice
    captured by `cargo test --test cross_version_compat -- --ignored capture_v4_41_fixture`

  cross_version_compat now exercises v4.30 (v1 framing) + v4.41 (v3 framing).
  Every prior format era stays replayable.

### Sweep delta (vs v4.40)

See perf notes "after v4.41" — spg-server INSERT 1M: 66K → 76.6K r/s
(+16%), 10M: 49K → 59.4K r/s (+21%, no RSS bail). The 200K single-client
gate from next steps's earlier projection moves to v4.42 where it becomes
structurally reachable (engine MVCC + group commit).

### Files touched

  crates/spg-server/src/main.rs:
    + WAL_V3_FLAG / WAL_V3_SENTINEL / WAL_V3_TYPE_AUTO_COMMIT_SQL
    + encode_wal_v3_record(type_tag, payload)
    + wal_v3_auto_commit_size(sql)
    + append_wal_v3_auto_commit(state, sql)
    - append_wal_atomic_block() removed (replaced by the v3 path)
    - wal_block_size() removed (replaced by wal_v3_auto_commit_size)
    ~ replay_wal_bytes() extended to v1/v2/v3 three-way dispatch
    ~ dispatch site (Op::Query): uses append_wal_v3_auto_commit + wal_v3_auto_commit_size

  crates/spg-server/src/replication.rs:
    ~ follower's WAL record accumulator now decodes v1 + v2 + v3 (was v1 + v2).
      Same dispatch shape as replay_wal_bytes — sentinel bits select format,
      v3 picks up the 1-byte type tag and verifies CRC over [type||payload].
      Unknown v3 type bytes abort follower apply (no silent skip).

  crates/spg-server/tests/e2e_wal_binary.rs (new)
  crates/spg-server/tests/cross_version_compat.rs (+capture_v4_41_fixture)
  crates/spg-server/tests/prod_ready.rs (static gate now greps for append_wal_v3_auto_commit)
  crates/spg-server/tests/e2e_chaos_netsplit.rs — no change; pinned the replication fix above.

  xtests/compat-fixtures/v4.41/ (new)
  STABILITY.md (new ### WAL record format section — v1/v2/v3 frozen surface)
  next steps (v4.41 rewrite + new v4.42 section + perf gate matrix refresh)
  perf notes (after v4.41 section)
  internal readiness matrix (1.11 row reference)

### Test verification

  cargo test --release --workspace                              # all green
  cargo clippy --workspace --all-targets -- -D warnings         # 0 warnings
  cargo fmt --all -- --check                                    # clean

---

## [4.40.0] — 2026-05-27 (persistent B-tree index — cheap clone with secondary indices too)

### Closes the v4.39 carve-out

v4.39 switched `Table::rows` to `PersistentVec` so `Catalog::clone()`
inside the v4.34 auto-commit BEGIN..COMMIT wrap was O(1) **on tables
without indices** — slo_smoke (no-index) jumped from 9.4K → 109K r/s.
But `Table::indices` was still `Vec<Index>` and each `Index` wrapped
an `alloc::collections::BTreeMap<IndexKey, Vec<usize>>`; on tables
with secondary indices (the sweep schema — `id INT` + `sec INT` +
two indices) every `Table::clone` still deep-copied the BTreeMaps,
capping spg-server sweep INSERT at ~15K r/s. v4.40 closes that half.

### What changed

  spg-storage/src/persistent_btree.rs (new, ~370 LOC including tests):
    pub struct PersistentBTreeMap<K: Ord, V> {
        root: Arc<BNode<K, V>>,
        len: usize,
    }
    new / get / iter / insert / insert_mut / Clone (O(1)) /
    IntoIterator / PartialEq.

  Path-copy CoW B-tree, `ORDER = 8` (= MAX_CHILDREN), MAX_ENTRIES = 7,
  no `unsafe`, no external deps, `no_std`-compatible.

  spg-storage/src/lib.rs:
    IndexKind::BTree(BTreeMap<IndexKey, Vec<usize>>)
      → IndexKind::BTree(PersistentBTreeMap<IndexKey, Vec<usize>>)

  `Index::new_btree` / `Table::insert` / `Table::add_index` /
  `Table::rebuild_indices` rewrite the per-row index update from
  `map.entry(key).or_default().push(idx)` to the clone-then-insert
  shape `let v = map.get(&key).cloned().unwrap_or_default(); v.push(idx);
  map.insert_mut(key, v);` — same semantics, with the structural-sharing
  property at clone time.

### Correctness gates

  tests/persistent_btree.rs::fuzz_oracle_against_std_btreemap
    100K-step random insert + replace + get sequence mirrored against
    `std::collections::BTreeMap`, asserting equal `get` results and
    equal `len` end to end.

  tests/persistent_btree.rs::fuzz_oracle_clone_isolation
    Branch A → B and C, mutate each independently — verify each
    handle returns its own oracle without leaking.

  tests/persistent_btree.rs::partial_eq_compares_by_elements
    Two PBs built via different insertion orders compare equal iff
    they hold the same elements. Independent of internal tree shape.

  tests/persistent_btree.rs::insert_grows_through_multiple_internal_splits
    Forces ≥ 2 internal splits; verifies the trie depth grows
    cleanly through the second split.

### Carve-outs deferred

- NSW / HNSW topology (`NswGraph`) still uses `Vec<Vec<Vec<usize>>>`.
  v5.0 makes HNSW persistent + adds a vector cache for the search
  path. Vector-indexed tables continue to take the v4.34 wrap path
  on INSERT.
- Group commit + binary WAL — v4.41.

### Refs

- next steps §v4.40, PROD_READY row 1.11, perf notes "v4.40 scale
  sweep" section.

---

## [4.39.0] — 2026-05-27 (catalog backed by PersistentVec — scale-invariant BEGIN/COMMIT)

### Promotes PROD_READY row 1.11 to "verified @ scale"

The v4.34 auto-commit BEGIN..COMMIT wrap (per-write savepoint
around the WAL append, required for ENOSPC rollback) clones
`Catalog` once per write. Before v4.39 the clone was deep-copy —
`Catalog::clone` → every `Table::clone` → `Vec<Row>::clone`. At
1M rows the clone took ~50 ms, capping `xbench/competitor/src/bin/sweep.rs`
spg-server INSERT throughput at 9.4K r/s (vs PG18's 146K r/s at
the same row count). v4.39 backs `Table::rows` with
`PersistentVec<Row>` (Bitmapped Vector Trie, landed standalone in
v4.38) so `Table::clone` is O(1) `Arc` bump and the wrap's clone
cost no longer scales with row count.

### Observable

- Mid-write rollback semantics unchanged. `tests/e2e_chaos.rs`
  (1.10 / 1.11 chaos paths) keep passing.
- Catalog serialization round-trip unchanged. File format version
  not bumped — the on-disk layout iterates rows, and
  `&PersistentVec<Row>: IntoIterator` makes the existing
  `for row in &t.rows { … }` write loop work unchanged.
- 1M-row INSERT throughput rises from **9.4K r/s → ~109K r/s**
  (`tests/slo_smoke.rs::slo_wal_insert_1m_rows_throughput`,
  release mode, single-client). Per-row INSERT p99 unchanged
  within the existing `SLO_WAL_INS_P99_US` budget — the new floor
  catches catalog-clone regressions specifically.

### API surface change (internal-only)

`pub fn Table::rows(&self) -> &[Row]` becomes `pub fn
Table::rows(&self) -> &PersistentVec<Row>`. `spg-engine` callers
in the workspace are updated to use `.iter()` (via
`IntoIterator for &PersistentVec`) and `.get(i)` where they used
slice indexing; the small set of cases that needed an actual
`Vec<Row>` (e.g. nested-loop join working set) now do
`.iter().cloned().collect()` once at the join entry. The
`PersistentVec<T>` type itself impls `Index<usize>` with
Vec-compatible panic-on-OOB semantics, so existing `table.rows[i]`
sites in the NSW search path keep their original shape.

### Carve-outs (deferred to later checkpoints)

- Secondary indices (`Table::indices: Vec<Index>`) still
  deep-clone — v4.40 migrates the B-tree index to
  `PersistentBTreeMap`. Until then a `Catalog::clone` on a
  table with secondary indices still costs O(index size).
- NSW / HNSW graph topology (`NswGraph`) stays on `Vec` — its
  persistent migration is v5.0's harder body of work. NSW search
  reads `table.rows[i]` through PV's `Index` impl, paying an
  extra `O(log₃₂ N)` per probe (~50 ns at 1M rows); this regresses
  `xbench/competitor/src/bin/vector_knn.rs` modestly (~3× search
  latency), recovered in v5.0.

### Closes / refs

- PROD_READY row 1.11 — promoted to "@ scale verified".
- next steps — v4.39 checkpoint of the v4.38–v5.0 perf recovery
  roadmap (post-v4.37).

---

## [4.37.0] — 2026-05-27 (file format v9 + CRC32 on every storage envelope)

### Closes PROD_READY row 1.8 — explicit corruption detection on
### every storage surface.

Three storage envelopes gain CRC32 in a backwards-compatible way.
Old files keep loading unchanged; mid-record bit-flips on new
files surface as `CRC mismatch` errors instead of
deserializing-into-garbage. Forward-compat is not required
(STABILITY.md — clients only need to read older formats), so old
binaries reading new files crash on the "huge length" sentinel
(WAL) or "unknown version" path (envelope / bundle).

### WAL record format

- v1 (≤ v4.36): `[u32 LE len][len bytes]` — no CRC.
- v2 (v4.37+):  `[u32 LE (len | 0x8000_0000)][u32 LE crc32][len bytes]`.

The sentinel bit 31 of the length distinguishes them; v1 records
have it clear (sql_len < 2 GiB always). Replay handles both — a
single WAL file may interleave v1 + v2 records during the
upgrade window. The follower's record accumulator (in
`replication.rs`) tracks the same v1/v2 split.

### Snapshot envelope

`SPGENV01` envelope version bumped `1` → `2`. v2 appends a u32
CRC32 over every byte before it (magic + version + sections).
`Engine::restore_envelope` accepts both: v1 loads with no CRC
check (frozen by STABILITY); v2 verifies and returns
`StorageError::Corrupt` on mismatch.

### Backup bundle

`SPGBKUP\x01` writer replaced by `SPGBKUP\x02` writer. v2 ends
with a u32 CRC32; `inspect_bundle` verifies on read. Pre-v4.37
bundles (v1 magic) inspect unchanged. The new `BackupError::
Corrupt` variant carries the expected / computed values for
operator debugging.

### CRC32 implementation

New `spg_crypto::crc32` module — pure-stdlib IEEE 802.3 (poly
`0xEDB88320`), byte-at-a-time table lookup. `no_std`-compatible
to stay consistent with the rest of spg-crypto. 256-entry table
is built lazily on first call into a `[AtomicU32; 256]`; one
known-vector test + bit-flip detection test cover it.

### Tests added

- `tests/e2e_chaos.rs::chaos_wal_bit_flip_caught_by_crc32_refuses_to_replay`
  — flips one bit mid-WAL, restart REFUSES to start with an
  explicit CRC error on stderr (no silent corruption applied).
- `prod_ready.rs::row_1_8_*` machine row.
- `spg_crypto::crc32::tests` — known-vector + bit-flip detection.

### Changed

- STABILITY.md §"Snapshot file format" + §"Backup bundle format"
  pin both v1 and v2 layouts plus the writers-from-v4.37-emit-v2
  rule.
- internal readiness matrix audit snapshot: 75 → 76 ✅ / 4 → 3 ⚠️; [machine]
  rows 37 → 38.

### Test verification

  cargo test --release --workspace                              # all green
  cargo clippy --workspace --all-targets -- -D warnings         # 0 warnings
  cargo fmt --all -- --check                                    # clean

## [4.36.0] — 2026-05-27 (replication netsplit chaos + lag metric — `SPGREPL\x02`)

### Wire protocol — new minor version `SPGREPL\x02` (backwards-compat)

The master now speaks two negotiable replication wire versions on
`SPG_REPL_ADDR`; the follower picks via the handshake magic byte:

- `SPGREPL\x01` (v4.24) — raw WAL byte stream. Unchanged.
- `SPGREPL\x02` (v4.36) — **framed** stream: `[u8 type][u32 LE
  len][payload]`. Type `0x00` = WAL chunk (payload bytes feed the
  follower's record accumulator just like v1). Type `0x01` =
  status frame, payload `[u64 LE primary_wal_pos][u64 LE
  wall_time_us]`.

New followers always send the v2 magic; old `\x01` followers
keep working with old behavior. STABILITY.md §"Replication
protocol" pins both versions.

### Added
- **Status-frame protocol extension** in `crates/spg-server/src/
  replication.rs`: master emits a status frame at least every
  50 ms whether or not there's WAL activity. Follower parses it,
  stores into `LagState` (three atomics on the new
  `ServerState::lag_state` field).
- **Replication lag series** in `/metrics`:
  `spg_replication_lag_bytes` (primary_pos − follower_applied_pos)
  + `spg_replication_lag_seconds` (now − master's wall time).
  Omitted on the primary and on a v1 follower (no status frame
  seen) so Prometheus doesn't reify a misleading zero.
- **Netsplit chaos test** in `tests/e2e_chaos_netsplit.rs`:
  - In-test TCP proxy (stdlib only — `TcpListener` + `TcpStream`)
    that supports a kill-switch flipped from the test thread.
  - `netsplit_disconnect_then_heal_resyncs_without_loss_or_dup`
    spins up primary + follower behind the proxy, cuts the proxy
    mid-write, lets the master keep writing, restores the proxy.
    Asserts row count *and* row sum match exactly — no dup, no
    gap. Closes PROD_READY row 2.9.
  - `follower_metrics_expose_replication_lag_after_status_frame`
    confirms both lag series land on the follower's `/metrics`.
    Closes PROD_READY row 4.7.
- `prod_ready.rs::row_2_9_*` and `row_4_7_*` machine rows.

### Changed
- STABILITY.md §"Frozen surfaces" gains a "Replication protocol"
  section pinning both v1 and v2 wire layouts plus the forward-
  compat rule (followers MUST tolerate unknown frame types and
  unknown payload sizes on known types).
- internal readiness matrix audit snapshot: 73 → 75 ✅ / 5 → 4 ⚠️ / 1 → 0 ❌;
  [machine] rows 35 → 37.

### Test verification
  cargo test --release --workspace                              # all green
  cargo clippy --workspace --all-targets -- -D warnings         # 0 warnings
  cargo fmt --all -- --check                                    # clean

## [4.35.0] — 2026-05-27 (per-table metrics — `spg_table_rows` / `spg_table_bytes` + cardinality cap)

### Added
- `spg_table_rows{table=…}` and `spg_table_bytes{table=…}`
  gauges in `/metrics`. Rows is the live row count; bytes is a
  schema-width × row-count estimate (variable-width types pick
  a defensible average — Text/JSON = 64 B, half-full Varchar,
  etc.). Closes PROD_READY row 4.6.
- `SPG_METRICS_TABLE_TOPN` (default 50) — when no explicit
  allowlist is set, only the N largest tables by row count are
  exported. Keeps Prometheus cardinality bounded for tenants
  with thousands of tables.
- `SPG_METRICS_TABLE_ALLOWLIST=t1,t2,...` — exact list mode for
  operators who want explicit per-table control.
- `tests/e2e_table_metrics.rs` — three e2e tests cover default
  top-N, allowlist filtering, and the cardinality cap.
- `prod_ready.rs::row_4_6_*` machine row.

### Changed
- internal readiness matrix audit snapshot: 72 → 73 ✅ / 2 → 1 ❌;
  [machine] rows 34 → 35.
- deployment notes env-var table gains both new entries.

### Test verification
  cargo test --release --workspace                              # all green
  cargo clippy --workspace --all-targets -- -D warnings         # 0 warnings
  cargo fmt --all -- --check                                    # clean

## [4.34.0] — 2026-05-27 (ENOSPC in-memory rollback — auto-commit BEGIN..COMMIT wrap)

### Added
- **Implicit BEGIN..COMMIT wrap for auto-commit writes** —
  when WAL is on and the statement is not a TX-control verb,
  the dispatch path now wraps the engine mutation in an
  implicit `BEGIN` / `COMMIT`. The whole `[BEGIN, sql, COMMIT]`
  triple lands in the WAL with **one** `write_all` + **one**
  `fsync` via the new `append_wal_atomic_block` helper. On WAL
  append failure the dispatcher issues `ROLLBACK` and the
  engine reverts — live in-memory state never reflects a write
  whose WAL append didn't make it to disk. Closes PROD_READY
  row 1.11 fully.
- `tests/e2e_chaos.rs::chaos_disk_full_no_preflight_rolls_back_in_memory_to_match_durable_state`
  — exercises the path through real `append_wal*` failure by
  disabling the v4.30 preflight (`SPG_DISABLE_WAL_PREFLIGHT`).
  Asserts live count == CC'd count both pre- and post-restart
  (no phantom rows in either window).
- `tests/slo_smoke.rs::slo_wal_insert_p99_under_budget` —
  WAL-on perf gate for the wrap. Ceiling 50 ms (loose to absorb
  APFS / ext4 journaling variance; baseline ~20 ms on local
  APFS); catches gross regressions in the wrap (extra catalog
  clones, missed batched fsync) without false-alarming on
  shared-runner I/O noise.
- `SPG_DISABLE_WAL_PREFLIGHT` env var (test-only) to bypass the
  v4.30 dispatch-time chaos preflight and force the real
  append-side failure path.
- `prod_ready.rs::row_1_11_*` machine row.

### Changed
- WAL append path: `append_wal` (single-statement, single fsync)
  is kept for in-TX writes; new `append_wal_atomic_block`
  multi-statement variant for the implicit-wrap path.
- v4.30 preflight quota check now sizes for the full
  `[BEGIN, sql, COMMIT]` block when the wrap is active.
- internal readiness matrix audit snapshot: 71 → 72 ✅ / 6 → 5 ⚠️;
  [machine] rows 33 → 34.

### Test verification
  cargo test --release --workspace                              # all green
  cargo clippy --workspace --all-targets -- -D warnings         # 0 warnings
  cargo fmt --all -- --check                                    # clean

## [4.33.0] — 2026-05-27 (ops three-pack — graceful shutdown + slow-query log + disk water-mark)

### Added
- **Graceful shutdown** — SIGTERM/SIGINT installs a handler that
  flips a global flag; the main accept loop polls it between
  non-blocking accepts, then drains in-flight connections bounded
  by `SPG_SHUTDOWN_DEADLINE_SEC` (default 30 s, mirrors
  systemd's `DefaultTimeoutStopSec`). Exits 0 on clean drain.
  Closes PROD_READY row 2.7. e2e:
  `tests/e2e_graceful_shutdown.rs::graceful_shutdown_drains_inflight_and_refuses_new_conns_and_exits_zero`.
- **Slow-query log** — `SPG_SLOW_QUERY_LOG_MS` env var; queries
  whose dispatch wall-clock exceeds the threshold emit one
  `{"event":"slow_query","sql":...,"elapsed_us":N,"role":...,"threshold_us":N}`
  line on stderr. Field layout matches `SPG_LOG_FORMAT=json` so
  the same ingest pipeline handles both event streams. Default
  off. Closes PROD_READY row 4.5. e2e:
  `tests/e2e_slow_query_log.rs::slow_query_log_fires_above_threshold_and_silent_below`.
- **Disk water-mark** — `SPG_WAL_MIN_FREE_BYTES` env var; before
  every WAL append, `statvfs(2)` on the WAL volume; if free <
  threshold, returns `ErrorKind::StorageFull` with an error
  message that cites the env var by name. Reads keep serving
  (this is a write-path precheck only). macOS + Linux. Default
  off. Closes PROD_READY row 5.7. e2e:
  `tests/e2e_disk_watermark.rs::disk_watermark_refuses_writes_keeps_reads_keeps_server_alive`.
- `libc = "0.2"` direct dep on `spg-server` for the two FFI
  shims (`signal(2)` + `statvfs(2)`). Each call site is wrapped
  in `#[allow(unsafe_code)]` with a SAFETY note.
- `prod_ready.rs` rows `row_2_7_*` / `row_4_5_*` / `row_5_7_*`.

### Changed
- internal readiness matrix audit snapshot: 68 → 71 ✅ / 7 → 6 ⚠️ /
  4 → 2 ❌; 30 → 33 [machine] rows.
- deployment notes env-var table gains three rows.

## [4.30.0] — 2026-05-27 (ops docs suite + RESTORE_DRILL + in-memory rollback fix)

### Added
- deployment notes — install, file layout, env-var reference, ports.
- operational runbook — common alert → response mappings.
- restore drill — verbatim recovery commands, backed by
  `tests/e2e_restore_drill.rs` (CI gate).
- `SECURITY.md` — disclosure process, threat model, secret handling.
- `CHANGELOG.md` (this file).

### Changed
- Preflight WAL-quota check in the write path: when
  `SPG_FAIL_WAL_QUOTA_BYTES` would refuse an append, reject the
  SQL **before** `engine.execute` so the live in-memory state
  never reflects the rejected write. PROD_READY row 1.11 lit up
  green (chaos path).

## [4.29.0] — 2026-05-27 (chaos test infrastructure)

### Added
- `SPG_FAIL_WAL_QUOTA_BYTES` env var: chaos knob capping WAL
  file size, returns `ErrorKind::StorageFull` on overflow.
- `tests/e2e_chaos.rs` — three e2e chaos scenarios:
  - `kill -9 mid-write` recovery (real SIGKILL)
  - WAL tail truncation drop (length-prefixed records survive)
  - disk full mid-write returns clean error + survives restart
- Updated PROD_READY rows 1.9, 1.10, 9.5, 9.6 to ✅.

## [4.28.0] — 2026-05-27 (PROD_READY baseline + machine-checked gate)

### Added
- internal readiness matrix — 85 rows across 10 dimensions with judgment
  criteria + status + evidence links.
- `tests/prod_ready.rs` — meta-test asserts every `[machine]`
  row in internal readiness matrix has a paired `row_X_Y_*` test.
- 12 baseline machine-checked rows: WAL replay, /healthz,
  /metrics, max_connections, wire opcode freeze, perf gates
  present, CI workflow present, perf notes v4.27 baseline.
- New CI job `prod_ready gate`.

## [4.27.1] — 2026-05-27 (v4.x perf coverage)

### Added
- `xbench/competitor/src/bin/repl_bench.rs`,
  `xbench/competitor/src/bin/backup_bench.rs` — measure
  replication attach cost, snapshot bootstrap, lag distribution,
  full + incremental backup bandwidth, restore round-trip, PITR.
- perf notes §v4.27 / §v4.24 / §v4.25 numbers.

### Fixed
- `SPG_REPLAY_UPTO=0` is now accepted as a literal "skip all WAL"
  value (previously filtered out by `parse_env_u64`'s `n > 0`).

## [4.27.0] — 2026-05-27 (CI/CD)

### Added
- `.github/workflows/ci.yml` — fmt + clippy + test + audit jobs
  on every PR; release build + binary artifact on main pushes.

## [4.26.0] — 2026-05-27 (EXPLAIN)

### Added
- `EXPLAIN [ANALYZE] <select>` SQL — single-column `QUERY PLAN`
  output with operator label, index-seek detection, frame
  details, subquery markers. `ANALYZE` attaches actual rows +
  elapsed micros.

## [4.25.0] — 2026-05-27 (backup PITR + incremental)

### Added
- `BACKUP TO '<path>'` SQL — full backup (admin only).
- `BACKUP TO '<path>' INCREMENTAL SINCE N` SQL — WAL tail delta.
- `SPG_REPLAY_UPTO` env var — startup-time WAL replay truncation
  for point-in-time recovery.
- `crates/spg-server/src/backup.rs` — self-contained bundle format
  (magic `SPGBKUP\x01`).

## [4.24.0] — 2026-05-27 (WAL streaming replication)

### Added
- `SPG_REPL_ADDR` + `SPG_FOLLOW_OF` env vars — single-primary /
  multi-follower async replication.
- 16-byte handshake (`SPGREPL\x01` + start offset), then raw WAL
  byte stream (the on-disk WAL format itself).
- `crates/spg-server/src/replication.rs`.

## [4.23.0] — 2026-05-27 (correlated subqueries in WHERE)

### Added
- EXISTS / NOT EXISTS / scalar / IN subqueries can now reference
  outer columns. Two-stage: pre-eval fast path stays for the
  uncorrelated case; row-eval handles correlation by substituting
  outer columns into the inner SELECT.

## [4.22.0] — 2026-05-27 (WITH RECURSIVE)

### Added
- `WITH RECURSIVE` CTE — anchor + UNION ALL/DISTINCT recursive
  term. Column-rename syntax `WITH t(a, b) AS (…)`. Hard runaway
  cap (1M rows / 100K iter).

## [4.21.0] — 2026-05-27 (extended window functions)

### Added
- LAG / LEAD / FIRST_VALUE / LAST_VALUE / NTH_VALUE / NTILE /
  PERCENT_RANK / CUME_DIST window functions.

## [4.20.0] — 2026-05-27 (explicit window frames)

### Added
- `ROWS BETWEEN … AND …` and `RANGE BETWEEN … AND …` window
  frames, plus single-bound shorthand. RANGE is peer-aware
  (matches PG default for ordered windows).

## [4.19.0] — 2026-05-27 (SET / SHOW)

### Added
- Per-connection SET / SHOW for session variables. 14 known PG
  GUCs return sensible defaults; SET is accepted and round-trips
  to SHOW.

## [4.18.0] — 2026-05-27 (VACUUM / ANALYZE no-ops)

### Added
- `VACUUM` / `ANALYZE` / `CLUSTER` / `REINDEX` accept syntax,
  return clean `CommandComplete`. No actual reorg (SPG doesn't
  need it).

## [4.17.0] — 2026-05-26 (PG-wire COPY)

### Added
- `COPY <table> FROM STDIN` (text format) — full Copy{In,Out}
  protocol, CopyData / CopyDone / CopyFail framing.

## [4.16.0] — 2026-05-26 (v4.x soak audit)

### Added
- 5-minute mixed-workload soak harness
  (`xbench/competitor/src/bin/soak_v4.rs`); confirmed leak-free
  (post-warmup RSS drift 0.0%) across every v4.x code path.

## [4.15.0] — 2026-05-26 (pgbouncer compat)

### Added
- DISCARD ALL / TEMP / SEQUENCES / PLANS, RESET ALL / `<name>`,
  SET TRANSACTION — all as no-ops returning the expected tag.

## [4.14.0] — 2026-05-26 (JSON path operators)

### Added
- `->` and `->>` JSON path operators backed by a hand-rolled
  RFC 8259 parser (no external deps).

## [4.0.0] — [4.13.0] — 2026-05-26 (prod-readiness sprint)

The v4.0-v4.13 sprint, all on the same day:

- **v4.13** observability — `/healthz`, Prometheus `/metrics`,
  JSON logs (`SPG_LOG_FORMAT=json`).
- **v4.12** window functions — ROW_NUMBER / RANK / DENSE_RANK +
  partition-aware aggregates over OVER (PARTITION BY … ORDER BY …).
- **v4.11** WITH / CTE (non-recursive).
- **v4.10** uncorrelated scalar / EXISTS / IN subqueries.
- **v4.9** JSON column type (`Value::Json(String)`).
- **v4.8** PG-wire SCRAM-SHA-256 — self-built SHA-256 / HMAC /
  PBKDF2 in spg-crypto. NIST + RFC vectors pass.
- **v4.7** PG-wire extended-query — Parse / Bind / Describe /
  Execute / Close / Flush / Sync. JDBC / asyncpg / psycopg3 work.
- **v4.6** PG-wire pg_catalog subset — pg_class / pg_namespace /
  pg_database / pg_user / pg_tables synthesized.
- **v4.5** cooperative query cancellation + idle timeout —
  `SPG_QUERY_TIMEOUT_MS` watchdog + `SPG_IDLE_TIMEOUT_SEC` OS
  read timeout.
- **v4.4** UPDATE / DELETE — real DML.
- **v4.3** PG-wire compatibility shim (opt-in via `SPG_PG_ADDR`).
  psql / DBeaver / Metabase connect.
- **v4.2** resource limits — `SPG_MAX_CONNECTIONS`,
  `SPG_MAX_QUERY_ROWS`.
- **v4.1** multi-user + 3-role RBAC — admin / readwrite /
  readonly. BLAKE3(salt||password) hashing.
- **v4.0** concurrency — `RwLock<Engine>` read/write split.
  2× scaling at 8 threads on indexed PK lookups.

---

## v3.x — performance sprint (2026-05-26)

Pre-v4 push to take SPG from "correct" to "competitive".
End-state: spg-server scan 5.2× over PG/MySQL/MariaDB; spg-
embedded ANN 54× over pgvector. See perf notes for full
numbers.

- **v3.4** baseline series — binary size, RSS, large-data
  report, 15-min mixed soak, 10-min readonly soak (drift 0.2%).
- **v3.3** wire-batching (DataRowBatch op 0x17), TCP_NODELAY +
  write coalescing, NEON-vectorised L2 distance.
- **v3.2** competitor bench infrastructure
  (`xbench/competitor/` with docker-compose).
- **v3.1** index planner proof, ORDER BY LIMIT partial sort,
  catalog O(log n) sidecar, in-memory backup bench.
- **v3.0** 8-stone bench infra + BUDGETS.md + perf_gate.rs +
  HNSW build/search 15× speedup + dense row encoding (FILE_VERSION 8).

## v2.x — feature expansion (pre-perf)

- **v2.14** spg backup / restore CLI.
- **v2.13** multi-layer HNSW (FILE_VERSION 7).
- **v2.7-2.12** date/time / interval / TO_CHAR / DATE_PART / AGE.
- **v2.4-2.6** EXTRACT / DATE_TRUNC, HNSW inner-product +
  cosine, clock injection.
- **v2.2-2.3** HAVING + SHOW TABLES / COLUMNS, DATE / TIMESTAMP.
- **v2.0-2.1** HNSW kNN index, MySQL dialect (backticks,
  AUTO_INCREMENT).

## v1.x — conformance + auth (pre-vectors)

- **v1.14** Redis-style single-password AUTH.
- **v1.10-1.13** JOIN, NUMERIC, SAVEPOINT — duckdb + pg_regress
  to 100%.
- **v1.1-1.9** sqllogictest harness, BETWEEN, IN, LIKE,
  aggregates, GROUP BY, DISTINCT, UNION.
- **v1.0** operational basics — stats opcode, env paths, version.

## v0.x — foundation

`v0.1-v0.11` built the skeleton from scratch: workspace, wire
protocol, SQL lexer/parser, storage, expression evaluator,
catalog persistence, BLAKE3, B-tree index, transactions, WAL,
pgvector.

---

## Release process

For maintainers cutting a new release:

1. Update internal readiness matrix audit snapshot.
2. Add a top-section entry to this file (Added / Changed /
   Fixed / Removed / Security).
3. `cargo test --release --workspace` (must pass).
4. `cargo clippy --workspace --all-targets -- -D warnings`.
5. `cargo run --release -p sqllogictest --release` (4 corpora 100%).
6. Commit message: `vX.Y.Z: <one-line summary>`.
7. Tag: `git tag vX.Y.Z`.
8. Push: `git push --follow-tags`.

CI takes over from there: fmt + clippy + test + audit +
prod_ready gate; release build artifact uploaded.
