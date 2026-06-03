# SPG v6.7 design — Cold tier evolution

> Drafted 2026-06-03 after v6.6 series shipped (WAL compression;
> tag `v6.6.5` rolled the series up at commit `e4fd649`).
> Scope: v6.7 series (v6.7.0 → v6.7.8).
> Companion research:
>   `.claude/researches/spg-vs-pg19-comparison.md` §1.1 — §1.4 / §2.11
>   `.claude/researches/spg-v6-roadmap-from-pg19.md` §3.v6.7
> Predecessor designs: `V6_DESIGN.md`, `V6_1_DESIGN.md` through
> `V6_6_DESIGN.md`.

## L0 — v7.0 discipline (inherited)

Same rule:

> **NO ITEM in any v6.x sub-version design may be deferred to a
> later minor without an explicit user-level "OK to defer".**

Deferrals must target a later same-minor sub-version in this
file. Future means a STABILITY §"Out of scope" entry.

v6.6 demonstrated the discipline: PG-wire-write-to-WAL was
explicitly carved out (pre-v6.6 gap, separate concern) rather
than silently pushed forward.

## L1 — Roadmap

v6.7 is the **largest v6.x series**: ~18.5 days estimated. It
closes three carve-outs from earlier minors plus four
substantial new pieces of cold-tier infrastructure:

**Carve-out redemption from v6.2** (per `V6_2_DESIGN.md` L0
strict reading — these items pointed to "v6.x revisit" so v6.7
is the natural home):

1. **Per-table `cold_rows` precise count** — v6.2.7's
   `cold_segments=[…]` annotation gives operators a GLOBAL list
   of cold segments touched per query, but not per-table
   `cold_rows`. v6.7.0 adds the per-table count via index-side
   `RowLocator::Cold` walking. Surfaced through `spg_statistic`
   (new column) + `spg_stat_segment` (new column).

**v6.7 first-class new infrastructure**:

2. **BRIN-style segment-level sidecar** — Block Range INdex
   metadata per segment: per-page `(min, max)` for a chosen
   column. `CREATE INDEX … USING BRIN (col)` syntax. Planner
   skips full-segment scans on range predicates that don't
   overlap a page's `[min, max]`.

3. **Per-table hot/cold byte budget** — `ALTER TABLE t SET
   hot_tier_bytes = X` overrides the global
   `SPG_HOT_TIER_BYTES` for that specific table. Stored in
   table schema; freezer reads it; metrics expose per-table
   thresholds.

4. **Cold-segment compaction** — Shadowing-driven LSM merge:
   small segments (< threshold) merge into a single larger
   segment. Garbage rows (DELETE'd + frozen) prune during the
   merge. New env knob `SPG_COMPACTION_THRESHOLD_SEGMENTS`.

5. **Parallel freezer** — Worker pool slices the freeze
   workload by PK range. Replaces the v5.2.2 single-thread
   freezer's serial behaviour with N workers (default
   `num_cpus() - 2`). Reduces freezer wall-time and unblocks
   write throughput during long freezes.

6. **Segment forwarding replication** — New v2 replication
   frame type `0x02 = segment_file_chunk`. Followers receive
   segments in chunks (resumable on disconnect); after receive,
   the follower writes the segment file to its local
   `<db>.spg/segments/` and registers it via
   `Catalog::load_segment_bytes`. Speeds up follower
   bootstrap from minutes (WAL replay) to seconds (segment
   transfer).

7. **AIO prefetch worker pool** — Cold-segment sequential reads
   are prefetched into the OS page cache via a worker pool
   (`io_uring` on Linux is OOS; v6.7 uses thread-pool with
   `read_at` + `posix_fadvise` hints on Linux, `mmap` +
   `madvise` on macOS).

8. **1B-row bench + segment pressure tests** — Synthetic
   1-billion-row workload that exercises the whole freezer →
   cold-tier → segment-forwarding → replay loop. Locks the
   ≤ 120 s cold-start gate.

Hard rules unchanged: **0 external dependencies, no `unsafe`
(aarch64 NEON carve-out only), WAL on-disk format frozen,
sqllogictest 100 % pass rate maintained**.

### Goal numbers (v6.7 ship-gate definition)

| metric | v6.6.5 baseline | v6.7 target | competitor reference |
|--------|-----------------|------------:|----------------------|
| 1B-row corpus cold start time | n/a | **≤ 120 s** | PG with parallel WAL replay |
| Per-table `cold_rows` accuracy | global only | **per-table exact count** | PG `pg_class.reltuples` |
| Freezer throughput at 1M writes/s sustained | blocks writes | **does not block** | PG vacuum parity |
| Cold-segment space amplification | unbounded growth | **≤ 1.5×** | PG TOAST + compaction |
| Follower bootstrap time vs WAL replay | full WAL replay | **≤ 50% via segment forwarding** | PG `pg_basebackup` |
| sqllogictest 4-corpus regression | 100 % | **100 %** | unchanged |

### Out of v6.7 (carved out)

- **`io_uring`** — Linux-specific async I/O API. v6.7 uses
  portable thread-pool + `posix_fadvise` / `madvise` hints. A
  later v6.x or v7.x can add io_uring as an opt-in path.
- **Columnar cold-tier format** — Delta-of-delta encoding,
  per-column page layout, vectorised scan. v6.11 territory
  (last-pre-v7 push); explicitly out of v6.7.
- **Multi-version cold tier** (versioned segment trees with
  branching). PG's TOAST doesn't do this; SPG's audit-driven
  PITR (v6.10) handles point-in-time recovery without per-
  segment versioning.
- **Cross-region segment replication** with consensus-level
  conflict resolution. v6.7 segment forwarding is leader →
  follower one-direction only. Multi-master is outside the v6
  scope.
- **Cold-segment query parallelism** — splitting a single SELECT
  across multiple cold segments concurrently. v6.7 keeps the
  single-thread executor; intra-query parallelism is v6.9
  conditional territory.
- **BRIN summary RECOMPACT on DELETE** — DELETE invalidates
  some BRIN page summaries' tightness. v6.7 marks the affected
  pages "loose" but doesn't recompute the min/max until the
  compaction-driven rewrite. Tighter incremental maintenance
  out of v6.7.
- **Replication-wire frame compression for segment chunks** —
  Segment files are already v2-envelope-compressed (v6.6.2) on
  disk; transmitting the on-disk bytes preserves the savings.
  No need for double-compression. Out of v6.7.

## L2 — Version boundaries (v6.7.0 → v6.7.8)

| ver | scope | ship-gate | depends on |
|-----|-------|-----------|------------|
| **v6.7.0** | Per-table `cold_rows` precise count. Walk the table's BTree-index `RowLocator::Cold` entries; group by source segment_id; expose the count via new column on `spg_statistic` (column `cold_row_count: BIGINT`) and on `spg_stat_segment` (new column `table_name: TEXT` + `row_count` already there, now segment-scoped per-table). Honest carve-out resolution from v6.2.7. | `tests/e2e_cold_rows_per_table::analyze_populates_cold_row_count` + `…::cold_segment_view_includes_table_name` + `…::matches_table_row_count_for_uniform_freeze` | v6.6.5 |
| **v6.7.1** | BRIN-style segment-level sidecar. New `BRIN` index method. `CREATE INDEX ix_t_id ON t USING BRIN (id)` builds per-page `(min, max)` summaries during freeze. Planner gains a predicate-implication check: range predicates that don't overlap a page's `[min, max]` skip the page scan entirely. Sidecar layout adds to the segment v2 envelope (algo byte unchanged; `inner_len` covers BRIN sidecar + body). | `tests/e2e_brin::create_index_brin_succeeds` + `…::range_predicate_skips_non_overlapping_pages` + `…::full_table_scan_still_works_without_brin` | v6.7.0 |
| **v6.7.2** | Per-table hot/cold byte budget. `ALTER TABLE t SET hot_tier_bytes = X` (parser + AST). Catalog stores per-table override. Freezer reads it; absent value falls through to `SPG_HOT_TIER_BYTES` global. Surface via `spg_stat_segment.hot_tier_bytes_table` + spg_table_ddl emits the SET clause. | `tests/e2e_per_table_budget::alter_table_set_hot_tier_bytes` + `…::freezer_respects_table_override` + `…::ddl_round_trip_includes_set_clause` | v6.7.0 |
| **v6.7.3** | Cold-segment compaction. New ops verb `COMPACT COLD SEGMENTS [WHERE …]` (or env-driven background worker). Merges segments smaller than `SPG_COMPACTION_TARGET_SEGMENT_BYTES` (default 4 MiB) into a single larger segment. DELETE'd-but-frozen rows are pruned during the merge. Manifest atomically swaps the merged segment in and the source segments out. | `tests/e2e_compaction::compact_merges_small_segments` + `…::compaction_drops_deleted_rows` + `…::manifest_swap_is_atomic_under_crash` (chaos) | v6.7.1 (uses BRIN to choose merge targets) |
| **v6.7.4** | Parallel freezer worker pool. `SPG_FREEZER_WORKERS` env (default `max(1, num_cpus() - 2)`). Worker assignment by PK range slice; per-worker `FreezeReport`s merge into one segment via a single coordinator thread. Reduces freezer wall-time on 100K-row freezes by ~3-4× on an 8-core M1. | `tests/e2e_parallel_freezer::workload_completes_under_sustained_writes` + `tests/perf_freezer::4_worker_speedup_at_least_2x` | v6.7.2 (per-table budget feeds in) |
| **v6.7.5** | Segment forwarding replication. New v2 replication type `0x02 = segment_file_chunk`. Layout: `[u32 segment_id][u32 chunk_seq][u32 chunk_total][u32 chunk_bytes][chunk bytes]`. Resumable on reconnect (follower tracks per-segment `chunk_seq` watermark). After all chunks received, follower writes the segment file + registers via `Catalog::load_segment_bytes`. | `tests/e2e_segment_forward::follower_bootstrap_via_forwarding` + `…::resumable_after_disconnect` + `…::byte_equal_segment_after_transfer` | v6.7.0 (uses cold_row_count for progress reporting) |
| **v6.7.6** | AIO prefetch worker pool. Per-platform impl: Linux uses thread-pool + `posix_fadvise(WILLNEED)`; macOS uses thread-pool + `madvise(WILLNEED)`. Triggered by `SegmentReader::scan` when sequential access pattern detected. New metric `spg_cold_prefetch_hits_total`. | `tests/e2e_prefetch::sequential_scan_triggers_prefetch` + `tests/perf_prefetch::4_worker_pool_speedup_at_least_1_3x` | v6.7.1 (BRIN helps choose prefetch pages) |
| **v6.7.7** | 1B-row bench + segment pressure tests. Synthetic 1-billion-row generator. Whole-pipeline test: INSERT 1B → freezer churns → compaction merges → cold restart → follower bootstrap via forwarding. Single perf-gate test enforces the ≤ 120 s cold-start ceiling. | `tests/perf_1b_rows::cold_start_under_120s` (single ship gate, `--release --ignored` because the bench is ~30 minutes wall-time on CI hardware) | v6.7.5 + v6.7.6 |
| **v6.7.8** | v6.7 ship rollup — CHANGELOG header, PROD_READY rows 7.43 – 7.50, STABILITY §"Cold tier evolution (v6.7 series)" + carve-outs. | rollup-only; CHANGELOG / PROD_READY / STABILITY merged; 4-corpus 100 %; every v6.7.x e2e from rows above passes. | v6.7.0 → v6.7.7 all |

### Estimated effort

| sub-version | est. days | running total |
|-------------|----------:|--------------:|
| v6.7.0 | 1.5 | 1.5 |
| v6.7.1 | 2.5 | 4.0 |
| v6.7.2 | 1.5 | 5.5 |
| v6.7.3 | 4.0 | 9.5 |
| v6.7.4 | 3.0 | 12.5 |
| v6.7.5 | 4.0 | 16.5 |
| v6.7.6 | 2.0 | 18.5 |
| v6.7.7 | 1.5 | 20.0 |
| v6.7.8 | 0.5 | 20.5 |

Roadmap estimate was 18.5 d; v6.7.0 carve-out redemption + v6.7.7
bench + v6.7.8 rollup bring it to ~20.5 d. **No item is
unscheduled.** This is the largest v6.x series — pace
accordingly.

## Architectural deliberations

### 1 — Per-table cold_rows: walk vs cache

Two options for the v6.7.0 count:
  a) Walk every table's BTree-index `RowLocator::Cold` entries on
     every `SELECT * FROM spg_statistic` call.
  b) Cache per-table `cold_row_count` on the table itself;
     update on every freeze / unfreeze / delete.

**Decided: (b) with lazy materialisation**. The cache lives on
`Table` (a new `cold_row_count: u64` field). Freezer increments
it post-freeze; cold-segment-promote decrements; DELETE decrements
when the row was already cold. v6.7.0 implementation populates
the cache on first `ANALYZE` by walking the index — same as how
v6.2.0 populates `Statistics`. Operator runs `ANALYZE` to refresh
the count.

### 2 — BRIN sidecar format

Per-page summaries layout (one row per page in the segment):

```text
[u32 page_index]
[u8 sentinel = 0x01]  ← v6.7.x can add tightness flag, dirty bit, etc.
[8-byte min_key]
[8-byte max_key]
```

Sidecar lives in the segment v2 envelope's inner bytes, prefixed
by:

```text
[u32 brin_section_len]
[BRIN entries: page_count × 21 bytes]
```

This grows the inner_len; the segment v2 envelope's algo byte
still applies LZSS over the whole (sidecar + v1 body) section.
The BRIN parser is run BEFORE the segment v1 body parser.

### 3 — Per-table budget storage

Two options:
  a) Add a `hot_tier_bytes: Option<u64>` field to `TableSchema`.
     Serialised as part of the catalog snapshot envelope.
  b) Stash in a sidecar BTreeMap on `Engine`.

**Decided: (a)**. Other per-table settings (auto-analyze
fraction, etc) live on schema; per-table budget belongs there
too. Catalog snapshot envelope v6 includes the field with
`None` → "use global". v5 envelopes load with all `None`.

### 4 — Compaction conflict with concurrent freezer

Both compaction and freezer write to `cold_segments`. The
compaction worker takes `engine.write_lock` only briefly:
  - Build the merged segment in a temp file
  - When ready, take write_lock, atomically swap manifest +
    in-memory `cold_segments` registry, release write_lock
  - The whole I/O-heavy section runs outside the lock

Concurrent freezer behavior during compaction: freezer continues
to write NEW segments at the next segment_id; compaction operates
on the segments that existed at compaction-start time. Clean
serial-equivalent semantics.

### 5 — Segment forwarding chunk size

Each chunk = 4 MiB. Rationale:
  - TCP MSS is typically 1500 B; 4 MiB amortises the framing
  - Memory budget: follower buffers at most 1 chunk in flight
  - Replay resumption: per-chunk granularity means at most 4 MiB
    re-transfer on disconnect

Hard cap in the protocol header (`chunk_bytes: u32`) is 16 MiB
to leave room for future bumps.

### 6 — Parallel freezer coordination

Workers process disjoint PK ranges; each produces an independent
`FreezeReport`. A coordinator merges into a single segment by:
  1. Collect all worker outputs
  2. Sort by PK (workers are already PK-range-sliced, so this is
     stable concatenation)
  3. Run the standard `encode_segment` over the merged row set

Workers do NOT each write their own segment — that would
fragment the cold tier and defeat the v6.6.2 compression ratio.
The coordinator owns the single-segment output.

## L3a — Hot plan for v6.7.0 (the only sub-version that's "next")

Goal: ship per-table `cold_rows` precise count via new column on
`spg_statistic` + `spg_stat_segment`. No BRIN yet (v6.7.1), no
per-table budget yet (v6.7.2).

### Step 1 — Table struct grows `cold_row_count`

```rust
// crates/spg-storage/src/lib.rs
pub struct Table {
    schema: TableSchema,
    rows: Vec<Row>,
    indices: Vec<Index>,
    // v6.7.0 — cached count of rows whose locator is
    // RowLocator::Cold for this specific table. Populated by
    // ANALYZE (walks the indices); decremented on cold-segment
    // promote; incremented on freeze.
    cold_row_count: u64,
}
```

### Step 2 — Populate from ANALYZE

`Engine::analyze_one_table` (existing v6.2.0 path) gains a step
that walks every BTree index, counts `RowLocator::Cold` entries,
and writes the result back to the table's `cold_row_count`. This
runs alongside the existing histogram-builder pass — same row
iteration, so the cost is one extra atomic increment per row.

### Step 3 — Surface via `spg_statistic`

Add a new column `cold_row_count: BIGINT` to `spg_statistic`.
`exec_spg_statistic` reads `table.cold_row_count()` per row.

Frozen-surface impact: this is an APPEND to `spg_statistic`'s
column list (not a reorder/rename), which the v6.2.0 stability
contract allows.

### Step 4 — Surface via `spg_stat_segment` table_name

`spg_stat_segment` currently has `(segment_id, num_rows,
num_pages, total_bytes)`. v6.7.0 appends `table_name: TEXT`.
Lookup walks the BTree indices to find which table's cold
locators point at each segment. Same walk as Step 2; cached
on the engine after first call.

### Step 5 — Tests

```text
crates/spg-engine/tests/e2e_cold_rows_per_table.rs
  ├── analyze_populates_cold_row_count
  ├── cold_segment_view_includes_table_name
  └── matches_table_row_count_for_uniform_freeze
```

### Step 6 — Acceptance

- `cargo test -p spg-engine --lib` green
- `cargo test -p spg-engine --tests` green
- `cargo run -q -p sqllogictest --release` → 4-corpus 100 %
- New e2e tests pass

Commit message:
`v6.7.0: per-table cold_rows precise count (v6.2.7 carve-out redemption)`.

---

## How the next session picks this up

This design doc is the contract. The next session should:
  1. Read `MEMORY.md` index → `project_v6_state.md` for current
     status.
  2. Open `V6_7_DESIGN.md` (this file). L1 / L2 / L3a are
     self-contained; L3a is the v6.7.0 work plan.
  3. Verify `git log --oneline -1` shows `v6.6.5: v6.6 series
     ship rollup` (commit `e4fd649`). If not, the autorun has
     already moved further and this design may be partially
     superseded.
  4. Start v6.7.0 per L3a. Each subsequent sub-version follows
     the L2 row; commit with the message in L3a's "Step 6"
     pattern.
  5. v7.0 no-defer rule (L0) is in force. Any blocked sub-
     version must either ship in v6.7 or get an explicit
     STABILITY §"Out of v6.7" carve-out at the v6.7.8 rollup.
