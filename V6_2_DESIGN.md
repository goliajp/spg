# SPG v6.2 design — optimizer foundation

> Drafted 2026-06-03 after v6.1 series shipped (logical replication +
> perf preludes; tag `v6.1.10` rolled the series up).
> Scope: v6.2 series (v6.2.0 → v6.2.8).
> Companion research:
>   `.claude/researches/spg-vs-pg19-comparison.md` §1.5
>   `.claude/researches/spg-v6-roadmap-from-pg19.md` §1.5 / §3.v6.2
> Predecessor designs: `V6_DESIGN.md` (vector advancement),
> `V6_1_DESIGN.md` (logical replication).

## L0 — v7.0 discipline (NEW)

**Hard rule for the v6.2 → v7.0 stretch**:

> **NO ITEM in any v6.x sub-version design may be deferred to a
> later minor without an explicit user-level "OK to defer".**

The v7.0 milestone is "SPG fully replaces PG for our own use." Every
ship gate documented in v6.2.x ... v6.10.x lands before v7.0 is
tagged. Side-quests, refactors, and "future work" notes are still
allowed in commit messages; they do NOT push concrete v6.x items
forward.

When a sub-version's commit message says "out of vX.Y.Z (deferred)",
the deferral target must be **a later same-minor sub-version** in the
same `V6_X_DESIGN.md` — never "v7.x" or "future". A bare "future"
means we drop the surface, which is a different decision and lands
in `STABILITY.md` §"Out of scope".

This discipline applies to **all** of:
- v6.2 (this design — optimizer foundation)
- v6.3 PG-wire extended query
- v6.4 SQL polish
- v6.5 Observability v2
- v6.6 WAL compression
- v6.7 Cold tier evolution
- v6.8 Index breadth
- v6.9 Concurrency expansion
- v6.10 Inspired-better dedicated

The single exception is the **v6.9 conditional** — its existence
depends on a measurement after v6.8 (do we actually have a concurrency
bottleneck?). If we measure and conclude "no", v6.9 ships as a one-
commit "evaluation report" that explicitly says so. Either way v6.9
closes.

## L1 — Roadmap

v6.2 closes the **third gap** from the PG-19 audit: SPG has no
statistics-driven optimizer. Plans are built deterministically from
the SQL shape — no JOIN reorder, no selectivity estimation, no
EXPLAIN ANALYZE. That's fine for the v5/v6.0/v6.1 workloads (vector
search, single-table OLTP, replication-driven streaming) but breaks
the moment we add a 5-table JOIN with bad source ordering.

v6.2 lands:

1. **`spg_statistic` virtual table** — per-column `(null_frac,
   n_distinct, 100-bucket equi-depth histogram)`. SQL-queryable.
   Mirrors PG `pg_statistic` but minimalist (one histogram type, no
   MCV, no per-array stats).
2. **`ANALYZE [table_name]`** SQL command + background auto-analyze
   (fires when ≥ 10 % of a table's rows have changed since the last
   ANALYZE).
3. **Selectivity functions** — equal, range, IN, LIKE prefix,
   BETWEEN — all backed by the histogram.
4. **JOIN reorder** — full enumeration for ≤ 4 input relations,
   greedy by-selectivity for > 4.
5. **`EXPLAIN ANALYZE`** — every operator emits actual rows / actual
   nanos / total elapsed.
6. **Hot/cold tier row annotation** in EXPLAIN ANALYZE — each scan
   operator reports hot vs cold rows separately so the operator can
   diagnose freezer-related latency.
7. **`Memoize` operator** — correlated-subquery cache keyed on the
   outer-loop join key. PG-style.
8. **TPC-H Q1 – Q5 integration tests** — plan stability + result
   correctness under ANALYZE-driven stats.

Hard rules unchanged: **0 external dependencies, no `unsafe`
(aarch64 NEON carve-out only), WAL on-disk format frozen,
sqllogictest 100% pass rate maintained**.

### Goal numbers (v6.2 ship-gate definition)

| metric | v6.1.10 baseline | v6.2 target | competitor reference |
|--------|------------------|------------:|----------------------|
| 5-table JOIN throughput, optimal selectivity order vs source order | source order (linear pipeline) | **≥ 10× speedup** | PG matches with `enable_geqo` off |
| EXPLAIN ANALYZE operator coverage | none | **100 % of executor nodes report `(rows, ns)`** | PG `EXPLAIN ANALYZE` parity |
| Plan stability after `ANALYZE` | n/a | **deterministic — same SQL + same stats → same plan** | PG-compatible |
| Memoized correlated subquery, repeated outer keys | full inner-scan per outer row | **≥ 5× speedup** | PG `Memoize` parity |
| TPC-H Q1 – Q5 correctness | n/a | **100 % matching DuckDB / PG output** | reference checks |
| sqllogictest 4-corpus regression | 100 % | **100 %** | unchanged |

### Out of v6.2 (carved out)

- **Multi-column statistics (`pg_statistic_ext`)** — single-column
  histograms only in v6.2; cross-column dependencies (e.g.
  `state='CA' AND zip='90210'` joint selectivity) are estimated as
  the product. **Not deferred — explicitly out of v6.x.**
- **Most Common Values (MCV) list** — PG's optimizer maintains a
  10-value MCV table per column on top of the histogram. v6.2 uses
  histogram-only. Same out-of-scope rationale.
- **Bitmap scans** — single-column index + heap fetch. Bitmap AND/OR
  across multiple indexes is v6.8 territory at earliest. Out of
  v6.2.
- **CBO for vector kNN** — `ORDER BY <-> LIMIT k` keeps the v5.5
  rule-based dispatch (planner detects the shape and dispatches to
  `nsw_query` directly). CBO for vector ops is out of v6.x.
- **Parallel executor nodes** — `Gather` / `Parallel Seq Scan`. SPG
  is single-writer + read-parallel by connection; intra-query
  parallelism is v6.9 conditional territory.

## L2 — Version boundaries (v6.2.0 → v6.2.8)

Each row is one shippable commit with its own perf gate and chaos
coverage. Ordered by dependency.

| ver | scope (work units) | ship-gate | depends on |
|-----|--------------------|-----------|------------|
| **v6.2.0** | `spg_statistic` virtual table — `(table_name, column_name, null_frac REAL, n_distinct BIGINT, histogram_bounds TEXT)` returns one row per analyzed column. Backed by a `Statistics` field on `Engine` (alphabetical BTreeMap for snapshot stability). `ANALYZE <table>` SQL — foreground scan, builds per-column histogram via 100-bucket equi-depth sampling (single pass). 4 v3 envelope bumps to v5 — stats trailer, forwards-compat with v4 envelopes. | `tests/e2e_spg_statistic::analyze_populates_histogram_bounds` + `…::reanalyze_overwrites_prior_stats` + `…::snapshot_envelope_v5_round_trip` | v6.1.10 |
| **v6.2.1** | Background auto-analyze — `Engine` tracks per-table modified-row counter (`inserts + updates + deletes` since last ANALYZE). A background thread sweeps every `SPG_AUTO_ANALYZE_INTERVAL_MS` (default 30 s) and ANALYZE-s any table whose modified fraction ≥ 10 %. Reuses v6.1.4 reconcile pattern — thread is started/stopped from `ServerState`. | `tests/e2e_auto_analyze::sweep_fires_after_10pct_threshold` + `…::no_sweep_when_under_threshold` + `…::concurrent_with_reads_unblocked` | v6.2.0 |
| **v6.2.2** | Selectivity helpers in `spg-engine` planner. New module `spg_engine::stats::selectivity` exposes `equal(col, value)`, `range(col, low, high, lo_incl, hi_incl)`, `in_list(col, values)`, `like_prefix(col, prefix)`, `between(col, lo, hi)`. Each reads `Statistics` for the column and walks the histogram. Default = 0.005 for unknown columns (PG-compatible "no stats" guess). | `crates/spg-engine/tests/selectivity.rs` (10 unit cases: empty histogram, fully outside range, single bucket, multi-bucket, IN with N values, LIKE prefix matching one bucket, …) | v6.2.0 (needs Statistics) |
| **v6.2.3** | JOIN reorder. New planner pass `spg_engine::planner::reorder::reorder_joins(stmt)` runs after parse + clock rewrite + ORDER BY position resolution. Enumerates all permutations for `n ≤ 4` input relations, picking the lowest-`cost` ordering; for `n > 4` runs greedy "smallest selectivity first" with single-pass left-deep build. Cost function = `selectivity * sum(left_size, right_size)`. Engine catches a `Statement::Select` with `joins.is_empty()` → fast skip. | `tests/planner_reorder::four_table_join_selects_smallest_first` + `…::five_table_join_greedy_matches_optimal_within_20pct` + `tests/perf_gate_optimizer::five_table_join_speedup_vs_source_order ≥ 10×` | v6.2.0 + v6.2.2 |
| **v6.2.4** | `EXPLAIN ANALYZE` — every executor node wraps its inner loop with a `NodeStats { rows: AtomicUsize, ns: AtomicU64 }`. `exec_explain` accumulates them into a node tree; the surface displays per-operator `(actual_rows, actual_ns, total_ns)`. Coverage: every `exec_*` arm in `spg_engine::lib` (SELECT projection, WHERE filter, JOIN, GROUP BY, ORDER BY, LIMIT, sub-query, INSERT, UPDATE, DELETE). | `tests/e2e_explain_analyze::every_operator_reports_stats` + `…::join_subtree_consistent_totals` + `…::no_unknown_operator_in_corpus` (walks every `xtests/sqllogictest/corpus/*.test` SELECT, runs EXPLAIN ANALYZE, asserts each line names a known operator) | v6.2.0 (independent of v6.2.2/.3 but ships after) |
| **v6.2.5** | Hot/cold tier annotation in EXPLAIN ANALYZE. Every scan node reports `(hot_rows, cold_rows, cold_segment_ids)`. Reuses v5.1 `Catalog::scan_segment_bytes` to count cold-tier hits. | `tests/e2e_explain_analyze::scan_reports_hot_vs_cold` + `…::cold_segment_ids_round_trip` | v6.2.4 |
| **v6.2.6** | `Memoize` node. Planner detects correlated subqueries (`Expr::Subquery` referencing an outer column) and wraps them in `Memoize { inner, key_cols, cache: BTreeMap<Vec<Value>, Vec<Row>> }`. Cache key is the outer-loop binding; hit returns the prior result, miss runs `inner` and inserts. LRU cap = 1024 entries (or 16 MiB, whichever first). | `tests/e2e_memoize::correlated_subquery_5x_speedup` + `…::cap_evicts_lru` + `…::deterministic_under_repeated_keys` | v6.2.3 + v6.2.4 |
| **v6.2.7** | TPC-H Q1 – Q5 integration tests. Smallest viable TPC-H fixture (`scale 0.01` ≈ 60 K rows total across 8 tables) generated deterministically in-test; loads via `LOAD` SQL. Q1 – Q5 + their DuckDB reference outputs land at `xtests/tpch/`. Both correctness (row-by-row diff vs DuckDB output) and plan stability (same plan across 5 consecutive runs after ANALYZE) gate this commit. | `tests/e2e_tpch::q1_matches_duckdb` + `…::q2_…` + `…::q3_…` + `…::q4_…` + `…::q5_…` + `…::plan_stable_after_analyze` | v6.2.0 → v6.2.6 all |
| **v6.2.8** | v6.2 ship rollup — CHANGELOG header summarising v6.2 + frozen-surface additions (spg_statistic columns, EXPLAIN ANALYZE format, Memoize node), PROD_READY rows 7.16 – 7.20 (one per major surface), `MEMORY.md` index update, and a v6.2-vs-PG-19 bench note in CHANGELOG. | rollup-only commit; checks: CHANGELOG / PROD_READY / STABILITY merged; 4-corpus 100 %; every v6.2.x e2e file from rows above passes. | v6.2.0 → v6.2.7 all |

### Estimated effort

| sub-version | est. days | running total |
|-------------|----------:|--------------:|
| v6.2.0 | 3.0 | 3.0 |
| v6.2.1 | 1.5 | 4.5 |
| v6.2.2 | 1.5 | 6.0 |
| v6.2.3 | 2.5 | 8.5 |
| v6.2.4 | 2.0 | 10.5 |
| v6.2.5 | 0.5 | 11.0 |
| v6.2.6 | 2.0 | 13.0 |
| v6.2.7 | 1.5 | 14.5 |
| v6.2.8 | 0.5 | 15.0 |

Roadmap estimate was 14.5 d; v6.2.8 ship rollup adds 0.5 d of
docs / merge. **No item is unscheduled or unsized.**

## Architectural deliberations

### 1 — Statistics format

Histogram is **equi-depth** (every bucket holds ~`row_count /
n_buckets` rows) rather than equi-width (every bucket spans equal
domain). Equi-depth gives stable selectivity estimates for skewed
distributions without needing an MCV sidecar — the skew lives in
the bucket widths.

100 buckets is the design point. PG uses 100 by default; smaller
(50) loses too much resolution for IN-list cardinality estimates;
larger (256) blows ANALYZE wall-time without measurable gain on
≤ 1M-row tables (sweep happens in v6.2.0). The bucket count is
NOT a frozen surface — v6.2.x can re-tune.

Bucket bounds are stored as `String` for any column type — the SQL
representation that round-trips through the existing
`Literal::String` path. TEXT and DATE columns naturally use their
existing sort order; INT / BIGINT / FLOAT use the canonical decimal
form. Vector columns are **not** analyzed (their stats live in HNSW
graph state).

### 2 — Memoize cache eviction policy

LRU keyed on the tuple of outer-bound values. Cap = `max(1024
entries, 16 MiB)` whichever fires first. The 16 MiB cap is the v5.5
per-query memory budget's 1/16 share — a single Memoize node can't
starve the rest of the query. The cap is a server-side knob via
`SPG_MEMOIZE_CACHE_BYTES`; default ships at 16 MiB.

### 3 — Snapshot envelope bump v4 → v5

`Statistics` lives on the engine and must survive restart, so the
envelope grows a fifth trailer block:

```text
[8 bytes "SPGENV01"]
[u8 version = 5]
[u32 catalog_len][catalog bytes]
[u32 users_len][users bytes]
[u32 pubs_len][publications bytes]      ← v3
[u32 subs_len][subscriptions bytes]     ← v4
[u32 stats_len][statistics bytes]       ← v5 NEW
[u32 crc32]
```

v1/v2/v3/v4 envelopes load with empty statistics; v5 envelope is the
only format new writers emit. Pre-v6.2.0 binaries fail loudly on a v5
envelope (matches the v6.1.2 / v6.1.4 upgrade fences).

### 4 — EXPLAIN ANALYZE: where node stats live

Two options:
  a) Each node carries an `Arc<NodeStats>`; the engine builds a
     parallel "stats tree" alongside the plan tree during exec and
     EXPLAIN ANALYZE walks both.
  b) Each node owns its `NodeStats` inline; exec mutates them in
     place via interior mutability (`AtomicUsize`).

**Decided: (b)**. Simpler — no parallel structure to keep in sync.
Atomic ops cost ~5ns/operator-call which is below the noise floor
of any node that does real work. The single exception is
`Projection` over a constant — adds <5% in micro-benches; acceptable.

### 5 — JOIN reorder: 4-relation full enumeration boundary

PG defaults to GEQO above 12 relations. SPG ships 4 as the full-
enumeration cutoff because:
  - `4! = 24 plans` — exhaustive enumeration is microseconds
  - `5! = 120` — still cheap but selectivity-greedy gets within
    20% of optimal on most workloads (verified in v6.2.3 gate)
  - Above 5 relations the greedy quality drops; tag a follow-up in
    v6.x as needed. **NOT deferred to v7** — if v6.2.3's gate
    catches a workload where greedy underperforms badly, v6.2.x
    grows DP enumeration; that's a same-minor follow-up.

The cutoff is a constant in `planner::reorder::FULL_ENUM_MAX = 4`.
NOT a frozen surface — v6.2.x can re-tune.

### 6 — `spg_statistic` schema

```text
CREATE VIRTUAL TABLE spg_statistic (
    table_name        TEXT NOT NULL,
    column_name       TEXT NOT NULL,
    null_frac         FLOAT NOT NULL,    -- 0.0 .. 1.0
    n_distinct        BIGINT NOT NULL,   -- raw count
    histogram_bounds  TEXT NOT NULL      -- "[v0, v1, ..., v100]"
);
```

Read-only — `INSERT / UPDATE / DELETE` on `spg_statistic` errors.
The only way to populate is `ANALYZE`. Frozen surface from
v6.2.0 — the column list and order is part of `STABILITY.md`.

The histogram_bounds TEXT is the canonical `[…]` form that
`spg-sql`'s vector-literal parser already round-trips for vector
columns, repurposed here for any orderable column.

## L3a — Hot plan for v6.2.0 (the only sub-version that's "next")

Goal: introduce the `spg_statistic` virtual table + `ANALYZE
<table>` SQL + per-column histogram builder. No selectivity
functions yet (that's v6.2.2), no auto-analyze yet (v6.2.1).

### Step 1 — Lexer additions

`ANALYZE` — added as a bare ident dispatch in the parser (same
pattern as `EXPLAIN` / `ALTER`). No new lexer keyword.

### Step 2 — AST nodes

```rust
pub enum Statement {
    …,
    /// v6.2.0 — `ANALYZE [<table>]`. Bare `ANALYZE` walks every
    /// user table; `ANALYZE <name>` re-stats just one.
    Analyze(Option<String>),
}
```

### Step 3 — Parser rules

```text
ANALYZE                 -- analyze all user tables
ANALYZE <ident>         -- analyze one table
```

### Step 4 — Statistics module

New `crates/spg-engine/src/statistics.rs`:

```rust
pub struct Statistics {
    inner: BTreeMap<(String, String), ColumnStats>,
    /// Per-table modified-row counter (used by v6.2.1 auto-analyze).
    pub modified_since: BTreeMap<String, u64>,
}

pub struct ColumnStats {
    pub null_frac: f32,
    pub n_distinct: u64,
    /// 101 bounds → 100 buckets. Strings (canonical SQL form).
    pub histogram_bounds: Vec<String>,
}
```

`serialize()` / `deserialize()` mirror `Publications` v6.1.2 +
`Subscriptions` v6.1.4. BTreeMap insertion order is alphabetical →
byte-stable snapshot.

### Step 5 — `spg_statistic` virtual-table dispatch

Engine recognises `FROM spg_statistic` in `Statement::Select`
parsing → routes to `exec_show_spg_statistic` which iterates the
`Statistics::inner` BTreeMap and emits one Row per column. Mirrors
the v6.1.3 `SHOW PUBLICATIONS` rows path.

### Step 6 — `ANALYZE` runtime

`exec_analyze(name: Option<String>)`:
- Resolve target tables (single name → lookup, or all user tables).
- For each table:
  - Single-pass scan, sample every row (≤ 100 K rows; above that,
    Reservoir sampling to 100 K — v6.2.x can re-tune).
  - For each column, compute `null_frac` (running count),
    `n_distinct` (linear-counting sketch, ≥ 0.95 accuracy on
    skewed corpora), and the equi-depth histogram bounds.
- Replace the prior `ColumnStats` for those `(table, column)` keys.
- Reset `modified_since[table]` to 0.

### Step 7 — Envelope v5 bump

Add `ENVELOPE_VERSION_V5: u8 = 5`; extend `build_envelope` /
`split_envelope` for a `statistics` trailer; update
`restore_envelope` to deserialise it (empty when loading from
v1/v2/v3/v4). Frozen-surface — STABILITY.md update.

### Step 8 — Tests

```
crates/spg-engine/src/statistics.rs:
  - 6 module tests (empty roundtrip; multi-column roundtrip;
    histogram bound count = 101 for 100-bucket; deterministic
    serialise; n_distinct sketch within 5 % on uniform; envelope
    v4 → v5 forward-compat)

crates/spg-server/tests/e2e_spg_statistic.rs:
  - analyze_populates_histogram_bounds
  - reanalyze_overwrites_prior_stats
  - snapshot_envelope_v5_round_trip
  - bare_analyze_covers_all_user_tables
  - analyze_unknown_table_errors
  - select_from_spg_statistic_returns_rows_per_column
```

### Step 9 — STABILITY.md update

Add to the frozen-surface list:
- `ANALYZE [<table>]` SQL
- `spg_statistic` columns + order
- Snapshot envelope v5 layout (including statistics trailer
  byte format)

### Step 10 — CHANGELOG v6.2.0 entry

Standard v6.x format — Added / Tests / Not changed / Out of v6.2.0
(deferred). **Every deferral must point at a later v6.2.x sub-
version, never v7 or future.**

### Step 11 — fmt + clippy + workspace test + 4-corpus

Standard gates; no regression.

### Step 12 — Commit

```
v6.2.0: spg_statistic virtual table + ANALYZE SQL + envelope v5
```

---

## Forward links

- `V6_3_DESIGN.md` (PG-wire extended query, plan cache) — drafted
  after v6.2.8 ships.
- `V6_4_DESIGN.md` (SQL polish, full JSON path + COPY) — drafted
  after v6.3.x ships.
- … through v6.10 …
- `V7_0_DESIGN.md` — the v7.0 ship rollup file. Drafted after
  v6.10.x's last sub-version lands, NOT before. v7.0 is the tag
  + a high-level entry summarising v6.0 → v6.10; not a feature
  shipment of its own.

v6.11 (columnar / time-series) deliberately rolls into the v7.x
roadmap — it's our first-after-v7 push, not a v7.0 blocker.
