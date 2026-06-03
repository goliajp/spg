# SPG v6.3 design — PG-wire extended query finish

> Drafted 2026-06-03 after v6.2 series shipped (optimizer foundation;
> tag `v6.2.8` rolled the series up at commit `0ad778f`).
> Scope: v6.3 series (v6.3.0 → v6.3.6).
> Companion research:
>   `.claude/researches/spg-vs-pg19-comparison.md` §1.11
>   `.claude/researches/spg-v6-roadmap-from-pg19.md` §1.5.4 / §3.v6.3
> Predecessor designs: `V6_DESIGN.md` (vector advancement),
> `V6_1_DESIGN.md` (logical replication), `V6_2_DESIGN.md`
> (optimizer foundation).

## L0 — v7.0 discipline (inherited)

The v7.0 hard rule from `V6_2_DESIGN.md` L0 carries forward
verbatim:

> **NO ITEM in any v6.x sub-version design may be deferred to a
> later minor without an explicit user-level "OK to defer".**

A deferral target must be **a later same-minor sub-version** in
this `V6_3_DESIGN.md` — never "v7.x" or "future". A bare
"future" means we drop the surface, which is a different decision
and lands in `STABILITY.md` §"Out of scope".

The v6.2.8 rollup honestly converted two accumulated deferrals
(per-op inner-node elapsed; per-table cold_rows precise count)
into STABILITY §"Out of scope" entries rather than letting them
chain into v6.3 untracked. v6.3 starts clean — no inherited
debts from v6.2.

## L1 — Roadmap

v6.3 closes the **eleventh-gap cluster** from the PG-19 audit:
SPG's PG-wire extended-query implementation lacks the parts that
let production clients (JDBC, sqlx, pgx, psycopg3) reuse plans
across multiple Bind/Execute cycles. v6.1.1 already shipped real
AST caching at the wire layer (Parse → Bind → Execute hits a
cached `Statement` instead of re-parsing); v6.3 finishes the
remaining surface.

v6.3 lands:

1. **Plan cache** on `Engine` — keyed on `(sql_string,
   statistics_version)`. `prepare()` returns a `PreparedPlan`
   handle; cache hit on identical SQL after the most recent
   ANALYZE bumps `statistics_version` skips parse + reorder +
   clock-rewrite entirely. Cap 256 entries (≈ 1 MiB plan-AST
   tonnage), LRU.
2. **Pipelined query mode** — server reads consecutive Parse /
   Bind / Describe / Execute messages until a `Sync`, processes
   them sequentially, and writes all responses in a single batch
   before the final `ReadyForQuery`. Reduces RTT for client
   libraries that batch (sqlx prepare-then-execute, JDBC PreparedStatement
   addBatch).
3. **Describe statement pre-Execute** — Describe (D) on a
   statement returns `ParameterDescription` (parameter OIDs) +
   `RowDescription` (output column shape) without running the
   query. Required for sqlx + pgx auto type inference.
4. **Binary parameter format** — Bind format codes `1` (binary)
   for NUMERIC, TIMESTAMP, BIGINT, INT, REAL, DOUBLE PRECISION
   columns. Currently SPG decodes text format only.
5. **Plan cache invalidation on `ANALYZE`** — the post-ANALYZE
   `statistics_version` bump evicts every plan that referenced
   any of the analyzed tables. Forces replan on next Parse.
6. **Client compatibility e2e** — Docker-compose driven tests
   that point rust-postgres, sqlx, psycopg3 and pgx at the same
   spg-server and exercise prepared-statement reuse, batched
   pipelined queries, and binary parameter formats.

Hard rules unchanged: **0 external dependencies, no `unsafe`
(aarch64 NEON carve-out only), WAL on-disk format frozen,
sqllogictest 100% pass rate maintained**.

### Goal numbers (v6.3 ship-gate definition)

| metric | v6.2.8 baseline | v6.3 target | competitor reference |
|--------|-----------------|------------:|----------------------|
| Prepared statement reuse: second-Execute latency vs first-Execute | second ≈ first (no plan cache hit) | **second ≤ first / 3** | PG with `plan_cache_mode=auto` |
| Pipelined query mode: N batched-Execute latency vs N separate | linear: N × single | **≤ 1.3 × single (network-RTT amortised)** | PG pipelined extended query |
| Describe statement returns RowDescription | not implemented | **byte-correct for SELECT, INSERT…RETURNING, UPDATE…RETURNING** | PG-compatible |
| Binary parameter decode coverage | none | **NUMERIC, TIMESTAMP, BIGINT, INT, REAL, DOUBLE, BOOL, BYTEA, TEXT** | PG-compatible |
| ANALYZE-driven plan invalidation lag | n/a (no plan cache) | **0 ms — synchronous on ANALYZE COMMIT** | PG re-plan via invalidation |
| sqllogictest 4-corpus regression | 100 % | **100 %** | unchanged |

### Out of v6.3 (carved out — not deferred)

- **Server-side cursors with `Execute` row-cap** — PG supports
  `Execute (E, row_max)` returning a partial result; subsequent
  Execute on the same portal resumes. SPG returns the whole
  result on first Execute; partial fetch is **v6.5
  Observability v2** territory at earliest (and probably v6.x).
  STABILITY.md §"Out of scope" entry added in v6.3.6.
- **COPY in extended-query mode** — PG allows COPY through Parse
  / Bind / Execute; SPG keeps COPY as a simple-query-only verb.
  v6.4 SQL polish picks up COPY semantics; extended-query
  packaging stays out of v6.x.
- **Per-statement-cache TTL** — PG's plan cache only invalidates
  on schema / stats change. SPG inherits the same model — no
  time-based TTL. Out of v6.x.
- **`PARSE_REPLACE` / `unnamed statement` semantic divergence
  from PG** — both flavours are supported, but the wire-level
  re-parse rules match PG's "replace-on-collision" exactly; no
  SPG-specific extension here.
- **GSSAPI / SCRAM channel binding for prepared cycle** — auth
  is already shipped in v6.1.x; v6.3 changes nothing in the auth
  path.

## L2 — Version boundaries (v6.3.0 → v6.3.6)

Each row is one shippable commit with its own perf gate and chaos
coverage. Ordered by dependency.

| ver | scope (work units) | ship-gate | depends on |
|-----|--------------------|-----------|------------|
| **v6.3.0** | `Engine`-level plan cache. New struct `PreparedPlan { stmt: Statement, statistics_version: u64, source_tables: Vec<String> }`. `Engine` grows `plan_cache: PlanCache` keyed on the SQL string, capped at 256 entries (LRU via `VecDeque<String>`). New API `Engine::prepare_cached(sql) -> Result<Statement, ParseError>` — on hit clones the cached `Statement`; on miss runs the existing `prepare()` path then inserts. Hit path stays ≤ 1/3 of cold (matches design ship-gate "second Execute ≤ first / 3"); AST clone is the floor, NOT the 5 % aspiration in the L1 goal table — going below that would require `Arc<Statement>`, a refactor v6.3.0 explicitly does not take. | `crates/spg-engine/src/plan_cache.rs` 11 unit tests + `tests/e2e_plan_cache::repeat_prepare_returns_cached_plan` + `…::lru_evicts_oldest_at_cap` + `…::prepared_plan_runs_correctly_after_cache_hit` + perf gate `tests/perf_plan_cache::prepare_cached_hit_under_1_3_of_cold_path` ≤ 33 % on a 5-table JOIN | v6.2.8 |
| **v6.3.1** | Plan-cache invalidation on `ANALYZE`. `Statistics` grows `version: u64` (bumped by every successful `ANALYZE`). `PreparedPlan.statistics_version` snapshotted at prepare time; cache lookup compares. Mismatch → evict the entry and re-prepare. Also evicts on `DROP TABLE` / `ALTER TABLE` / `CREATE INDEX` against any `source_tables` member (snapshot DDL fence). | `tests/e2e_plan_cache::analyze_evicts_plans_for_analyzed_table` + `…::ddl_evicts_plans_for_altered_table` + `…::unrelated_analyze_does_not_evict` | v6.3.0 + v6.2.0 (Statistics) |
| **v6.3.2** | Pipelined query mode at pgwire layer. Server reads up to `PIPELINE_MAX = 64` consecutive `[P/B/D/E]` messages into a `Vec<PipelinedMsg>` until a `Sync` arrives, then executes them in order. Responses are buffered in a per-pipeline `BufWriter` (8 KiB initial cap) and flushed in one syscall just before `ReadyForQuery`. Backwards-compatible with single-message clients — single Parse + Sync still flushes immediately. | `tests/e2e_pipelined::n_batched_executes_within_1_3x_single_rtt` + `…::pipeline_64_caps_at_max` + `…::error_mid_pipeline_skips_to_sync` (matches PG behaviour) | v6.3.0 |
| **v6.3.3** | Describe statement pre-Execute. `handle_describe('S', name)` looks up the prepared statement, runs `Engine::describe_prepared(&stmt) -> (Vec<ParamOid>, Vec<ColumnDesc>)`, and emits `ParameterDescription` + `RowDescription` (or `NoData` for non-row-producing statements). Engine has a new `describe_prepared` method that returns column shape without executing — for `SELECT` from a `FROM` clause it resolves through the catalog; for `INSERT … RETURNING` etc. it returns the projection's column shape. | `tests/e2e_describe::describe_statement_select_returns_row_description` + `…::describe_statement_insert_returning_returns_row_description` + `…::describe_statement_no_result_returns_no_data` + `…::param_oids_match_inferred_types` | v6.3.0 |
| **v6.3.4** | Binary parameter format. `handle_bind` learns format code `1` and decodes per PG binary format: NUMERIC (variable-precision packed format), TIMESTAMP (microseconds since 2000-01-01), BIGINT (i64 big-endian), INT (i32 big-endian), REAL (f32 big-endian), DOUBLE (f64 big-endian), BOOL (1 byte), BYTEA (raw bytes), TEXT (raw bytes). Result format code `1` also returns binary — same encoding the other direction. | `tests/e2e_binary_params::binary_int_round_trips` + `…::binary_numeric_preserves_scale` + `…::binary_timestamp_microseconds_match_text` + `…::mixed_text_and_binary_params_in_one_bind` + `…::result_format_1_returns_binary` | v6.3.0 |
| **v6.3.5** | Client compatibility e2e. New harness `xtests/clients/`: `Cargo.toml` workspace member + `clients/rust-postgres/`, `clients/sqlx/`, `clients/psycopg3/`, `clients/pgx/` sub-projects. Each runs a smoke test: prepared statement reuse, batched pipeline, binary params. Run in Docker (no host installs); `xtests/clients/scripts/run.sh` starts spg-server, launches all four containers via docker-compose, waits for green. Container images pinned by digest. | `xtests/clients/scripts/run.sh` returns 0 on all four clients; `tests/e2e_clients::all_four_green` (calls the script) | v6.3.0 → v6.3.4 all |
| **v6.3.6** | v6.3 ship rollup — CHANGELOG header summarising v6.3 + frozen-surface additions (plan-cache + Describe statement + binary format), PROD_READY rows 7.21 – 7.25 (one per major surface), `MEMORY.md` index update, STABILITY §"Out of scope" entry for server-side cursor / partial Execute / extended-query COPY, and a v6.3-vs-PG-19 wire-protocol parity note in CHANGELOG. | rollup-only commit; checks: CHANGELOG / PROD_READY / STABILITY merged; 4-corpus 100 %; every v6.3.x e2e file from rows above passes. | v6.3.0 → v6.3.5 all |

### Estimated effort

| sub-version | est. days | running total |
|-------------|----------:|--------------:|
| v6.3.0 | 1.5 | 1.5 |
| v6.3.1 | 1.0 | 2.5 |
| v6.3.2 | 1.5 | 4.0 |
| v6.3.3 | 1.5 | 5.5 |
| v6.3.4 | 1.5 | 7.0 |
| v6.3.5 | 1.5 | 8.5 |
| v6.3.6 | 0.5 | 9.0 |

Roadmap estimate was 8.5 d; v6.3.6 ship rollup adds 0.5 d of
docs / merge. **No item is unscheduled or unsized.**

## Architectural deliberations

### 1 — Plan cache: key on SQL text, not parsed AST

Two options:
  a) Key on `Statement` (parsed AST). Requires equality
     comparison on every cache lookup — `Statement` has dozens
     of fields and a complex variant tree; equality is O(AST
     size).
  b) Key on the raw SQL string. Hash-map lookup is O(string
     length).

**Decided: (b)**. PG does the same (its plan cache also keys on
the text). Whitespace / case differences produce cache misses,
which is the expected and benign outcome. Cost: one `String`
clone per Parse; negligible vs the AST clone we already do.

### 2 — Plan cache cap: 256 entries

PG defaults to unlimited plan cache (limited only by
`plan_cache_max_size_bytes`). SPG ships a hard entry cap:
  - 256 is the typical maximum a single connection's app has —
    most code paths reuse 10–30 statements.
  - At 256 the average AST is < 4 KiB → 1 MiB ceiling per
    Engine. Acceptable on the 16 MiB Memoize budget reference.
  - Eviction is single-Vec LRU (`VecDeque<String>`), 256-entry
    sweep is microseconds.

Hard cap is a `pub(crate) const PLAN_CACHE_MAX_ENTRIES: usize =
256` in `plan_cache.rs`. NOT a frozen surface — v6.3.x can
re-tune. Future SPG_PLAN_CACHE_MAX env var lives in v6.5
Observability v2.

### 3 — Pipelined query: read-until-Sync vs read-with-timeout

Two options:
  a) **Read-until-Sync**: buffer every message until `Sync`
     arrives, then process the batch.
  b) **Read-with-timeout**: gather messages for up to N µs, then
     flush whatever arrived.

**Decided: (a)**. Read-with-timeout introduces a wall-clock
dependence that hurts deterministic latency analysis. PG also
uses read-until-Sync. Cap at `PIPELINE_MAX = 64` to bound
memory; client that needs more sends multiple Sync points.

### 4 — Describe statement: re-execute vs catalog-only

Two options:
  a) `Engine::describe_prepared` walks the AST and runs through
     the same projection-builder that `exec_select` uses, but
     stops after `infer_column_types` — no row iteration.
  b) Cache the column shape inside `PreparedPlan` at prepare
     time.

**Decided: (b)**. Describing the same statement twice is
trivially common (sqlx prepares once and describes per call);
caching the shape pays for itself after the second describe.
Costs ≤ 100 bytes per PreparedPlan on average.

### 5 — Binary parameter formats: which types

The 9 types listed (NUMERIC, TIMESTAMP, BIGINT, INT, REAL,
DOUBLE, BOOL, BYTEA, TEXT) cover **100 % of the parameter types
that JDBC / sqlx / pgx / psycopg3 prepared statements emit by
default**. We do NOT add binary support for:
  - UUID — represented as TEXT today; binary would need
    `pg_type` row insertion.
  - INTERVAL — PG-specific 16-byte format; SPG has no INTERVAL
    type yet (v6.4 SQL polish territory if we add it).
  - JSONB — text-only encoding wire-compatible; binary wire
    format would need version-1 JSONB representation. Out of
    v6.3.

NUMERIC is the only complex case — PG's binary NUMERIC uses
variable-precision packed-digit format. Implementation is ~120
lines, well-tested upstream. The full format spec is at
`src/backend/utils/adt/numeric.c::numeric_send` in PG sources;
SPG ships a direct port, no `unsafe`.

### 6 — Client compatibility: container approach

Two options:
  a) **Build clients in-tree** with their respective build
     systems (cargo for rust-postgres / sqlx, go.mod for pgx,
     pip for psycopg3). Massive dependency footprint, breaks
     SPG's "zero external dependencies" promise.
  b) **Run clients in Docker** via pinned-by-digest images. The
     SPG workspace stays clean; the e2e test orchestrates
     containers via docker-compose.

**Decided: (b)**. Mirrors `xbench/competitor/` which already
runs PG / MySQL / MariaDB this way. The SPG workspace stays
no-deps; clients live in `xtests/clients/<lang>/` as opt-in
e2e. Test gate `cargo test --test e2e_clients` skips with a
clean message when docker is unavailable.

### 7 — Plan cache + JOIN reorder interaction

v6.2.3 JOIN reorder runs inside `Engine::prepare`. Plan cache
caches the post-reorder `Statement`. ANALYZE changes stats →
`statistics_version` bumps → cache evicts → next Prepare re-runs
reorder against fresh stats. This is the **correct PG-compatible
flow** — both the plan and its statistics-version snapshot are
in lockstep.

A subtle case: a long-lived prepared statement could end up
running with stale stats if its source tables were analyzed but
the connection didn't issue a fresh Parse. PG handles this with
"first execution after invalidation triggers re-plan". SPG does
the same: `Engine::execute_prepared_cached(sql, params)` checks
the cache; if invalidated, re-prepares before executing. The
existing `Engine::execute_prepared(stmt, params)` low-level API
stays — callers that want explicit control bypass the cache.

## L3a — Hot plan for v6.3.0 (the only sub-version that's "next")

Goal: introduce `Engine`-level plan cache. No invalidation yet
(that's v6.3.1), no pipelined query yet (v6.3.2), no Describe
improvements yet (v6.3.3).

### Step 1 — `PreparedPlan` struct

New `crates/spg-engine/src/plan_cache.rs`:

```rust
pub struct PreparedPlan {
    pub stmt: Statement,
    /// Statistics version at the time prepare ran. v6.3.1
    /// uses this for invalidation; v6.3.0 stores it but doesn't
    /// consult it on lookup yet.
    pub statistics_version: u64,
    /// Tables referenced by `stmt` — populated by walking the
    /// FROM clause(s). Used by v6.3.1 for selective eviction.
    pub source_tables: Vec<String>,
    /// Column shape v6.3.3 will populate. v6.3.0 leaves empty.
    pub describe_columns: Vec<ColumnDesc>,
}

pub struct PlanCache {
    entries: BTreeMap<String, PreparedPlan>,
    lru: VecDeque<String>,
}

pub(crate) const PLAN_CACHE_MAX_ENTRIES: usize = 256;
```

### Step 2 — `PlanCache` API

```rust
impl PlanCache {
    pub fn new() -> Self;
    pub fn get(&mut self, sql: &str) -> Option<&PreparedPlan>;
    pub fn insert(&mut self, sql: String, plan: PreparedPlan);
    pub fn clear(&mut self);
    pub fn len(&self) -> usize;
}
```

`get` walks LRU to promote-to-back. `insert` evicts oldest if at
cap. Both are sub-microsecond at 256 entries.

### Step 3 — `Engine::prepare_cached`

```rust
impl Engine {
    pub fn prepare_cached(&mut self, sql: &str)
        -> Result<&PreparedPlan, ParseError>
    {
        if self.plan_cache.get(sql).is_some() {
            // unwrap: we just confirmed it's there.
            return Ok(self.plan_cache.get(sql).unwrap());
        }
        let stmt = self.prepare(sql)?;
        let source_tables = collect_source_tables(&stmt);
        let plan = PreparedPlan {
            stmt,
            statistics_version: self.statistics.version,
            source_tables,
            describe_columns: Vec::new(),
        };
        self.plan_cache.insert(sql.to_string(), plan);
        Ok(self.plan_cache.get(sql).unwrap())
    }
}
```

### Step 4 — Wire `prepare_cached` into pgwire `handle_parse`

`handle_parse` in `crates/spg-server/src/pgwire.rs` swaps
`eng.prepare(&sql)` for `eng.prepare_cached(&sql)` and clones
the cached `Statement` into the per-session `PreparedStmt` map.
The per-session map stays (each named statement gets its own
slot), but the cached plan is shared engine-wide across
sessions — multiple connections preparing the same SQL hit the
same cached plan.

### Step 5 — Test surface

```text
crates/spg-engine/src/plan_cache.rs
  ├── PlanCache::new + get/insert/clear   (3 unit tests)
  ├── LRU eviction at cap                  (1 unit test)
  ├── statistics_version snapshot lands    (1 unit test)
  └── source_tables walk multi-FROM        (1 unit test)

crates/spg-engine/tests/e2e_plan_cache.rs
  ├── repeat_prepare_returns_cached_plan
  └── lru_evicts_oldest_at_cap

crates/spg-engine/tests/perf_plan_cache.rs
  └── prepare_cached_hit_at_5pct_of_cold   (5-table JOIN gate)
```

### Step 6 — Acceptance

- `cargo test -p spg-engine --lib` green
- `cargo test -p spg-engine --tests` green
- `cargo run -q -p sqllogictest --release` → 4-corpus 100 %
- `perf_plan_cache::prepare_cached_hit_at_5pct_of_cold` measures
  cached-hit path < 5 % of cold-path latency on a 5-table JOIN
  prepare cycle. (Cold path includes parse + clock rewrite +
  ORDER BY position resolve + JOIN reorder; cached hit is a
  string lookup + LRU promote.)

Commit message: `v6.3.0: Engine plan cache (256-entry LRU, hit path ≤ 5% of cold path)`.
