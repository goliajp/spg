# SPG v5 design — sweep-wide superiority

> Drafted 2026-05-28 after v4.42.0 ship (commit `7461a09`).
> Replaces the v5.0 sketch in NEXT.md.

## L1 — Roadmap

Make spg-server in `xbench/competitor/src/bin/sweep.rs` strictly beat
PG 18 / MySQL 9 / MariaDB 11 on **every** column at **every** N from
10K to 100M:

  - INSERT throughput (single client + multi client)
  - full table SCAN throughput
  - PK point-lookup p50 + p99
  - secondary-index point-lookup p50 + p99
  - completes 100M without bail (RSS / throughput-drop / time budget)

Honest constraint: this requires breaking SPG's current "100 % of the
catalog lives in RAM" architecture without losing the sub-100 µs PK
p99 it currently has. The path that lets both hold is a *two-tier
catalog* (hot rows in structural-sharing PV, cold rows in immutable
sorted on-disk segment files) backed by a PG-style WAL checkpoint
manifest and, for the INSERT throughput half, a PG-style async-commit
mode.

Hard rule stays: **0 external deps, pure Rust, no `unsafe`.** Every
sub-version below conforms.

## L2 — Version boundaries (v5.0 → v5.6)

Each row below is one ship target. The "ship-gate" column is the
observable success criterion that fires the trigger to the next sub-
version (see L4).

| ver | scope (work units, not steps) | ship-gate (L4 trigger to next) | depends on |
|-----|-------------------------------|--------------------------------|------------|
| v5.0 | `Segment` file format (sorted by PK + page index + bloom sidecar) + `SegmentReader` + bounded page cache + standalone perf gates. Engine + catalog **not touched**. | `tests/perf_gate.rs::segment_*` all pass; `segment_lookup_p99_under_500us` (cold OS cache) green | v4.42 (✓ shipped) |
| v5.1 | `RowLocator::{Hot(usize), Cold{seg_id, page_off}}` enum + PB index type upgrade + read path branches (`Engine::lookup_pk` checks hot PV first, miss → segment lookup). UPDATE/DELETE not yet touched. Catalog can hand-load pre-baked segments (test fixture); no automatic freezing. | `tests/e2e_two_tier::pk_lookup_finds_row_in_either_tier` + sweep PK p99 on hand-baked 30M corpus ≤ PG p99 | v5.0 |
| v5.2 | `Freezer` background thread + MVCC slot snapshot read + atomic swap (final commit holds write lock briefly to rename staging file + batch-update PB index Hot→Cold). UPDATE/DELETE use promote-on-write. `SPG_HOT_TIER_BYTES` env knob (default 4 GiB). | INSERT loop streams to 30M without RSS bail; chaos test `chaos_kill_during_freeze_recovers_clean_state` green | v5.1 |
| v5.3 | `CatalogManifest` v10 file format: hot tier serialised snapshot + cold segment registry + `wal_baseline_offset`. WAL prefix truncation on checkpoint. Startup loads manifest, mmaps cold segments (lazy), replays WAL from baseline. Crash recovery at 100M in < 60 s. | `tests/e2e_chaos::chaos_kill_after_100m_writes_recovers_in_under_60s` green; `cross_version_compat::v5_0_fixture_replays` green | v5.2 |
| v5.4 | Async-commit mode: `SPG_SYNCHRONOUS_COMMIT=off` opt-in (default = on, durability semantics unchanged). Background flusher thread fsyncs every N µs or N records. WAL records carry "durability checkpoint" markers; client CC returns immediately after in-memory apply; durability lag exposed via `/metrics`. Doc the trade-off (window of CC'd-but-not-durable writes on crash). | Single-client INSERT @ 1M ≥ 200 K r/s **with async-commit on** (separate gate from the synchronous path which stays at v4.42 numbers) | v5.3 |
| v5.5 | HNSW persistent (`NswGraph` levels + layers backed by PV-style structural sharing → vector-table INSERT joins the hot-tier path) + `#[global_allocator]` with per-query byte budget (`SPG_MAX_QUERY_BYTES`) + `set_alloc_error_hook` for OOM-survives-as-clean-error. Vector tables can also freeze cold segments (vector bytes stored alongside row payload). | Vector-table sweep variant passes; OOM chaos test returns `EngineError::Cancelled` instead of aborting | v5.4 |
| v5.6 | Final sweep validation: run full sweep at all N including boundary `[30M, 100M]` on competitor stack. spg-server must beat PG/MySQL/MariaDB on **every** cell of the sweep table at **every** N. `PERFORMANCE.md` "v5 final sweep" + closes v5 roadmap. SemVer major bump (v5.0.0 → user-visible config knobs added: `SPG_HOT_TIER_BYTES`, `SPG_SYNCHRONOUS_COMMIT`, `SPG_MAX_QUERY_BYTES`). | Sweep table green across the board; `PERFORMANCE.md` updated; CHANGELOG v5.0.0 entry; tag `v5.0.0` | v5.5 |

### Architectural deliberations decided in this audit

1. **mmap is out.** `std` doesn't expose mmap; adding `memmap2`
   breaks the 0-deps rule, raw `libc::mmap` breaks the 0-unsafe
   rule. Cold-segment reads go through `File::seek` + `read_exact`
   into a bounded page cache (LRU over `(segment_id, page_offset)
   → Box<[u8; 4096]>`). PG also doesn't use mmap; this is the
   conventional path.

2. **Hot tier sizing is byte-budget, not row-count.** Rows have
   variable size (text, vectors). `SPG_HOT_TIER_BYTES` (default
   4 GiB) is the cap; freezer wakes when the byte budget is
   crossed and demotes oldest-by-insertion-time rows first.

3. **Freezer concurrency uses v4.41.1's multi-slot interface.**
   Freezer takes a long-lived `TxId` via `engine.alloc_tx_id()`
   to read a frozen snapshot; the segment file is built in
   `.spg/staging/seg_N.tmp` without holding the engine write
   lock. The atomic commit step takes `engine.write()` only
   long enough to rename the staging file, batch-update PB
   index entries (`Hot(i) → Cold{seg, off}`), drop the matching
   hot-tier rows, and append a `freeze_commit` record to the
   WAL. v4.41.1 thus pays off twice: enabled v4.42 group commit,
   now enables v5 freezing.

4. **UPDATE/DELETE on a cold row promotes-on-write.** The new
   row version writes to the hot tier; the index pointer flips
   `Cold → Hot`; the cold-tier entry becomes garbage. A
   periodic compaction job merges cold segments and drops
   shadowed entries (capped at 1-2× space amplification, much
   less than LSM-tree workloads).

5. **Async-commit is mandatory inclusion.** Single-client INSERT
   throughput at 1M scale is fsync-bound on every host; group
   commit (v4.42) helps multi-client but does nothing for
   single client. To beat PG on the sweep table's single-client
   1M-10M INSERT column, async-commit is the only architecturally
   honest path. Default stays synchronous (preserves v4.34
   ENOSPC rollback invariant); operators opt in to async via
   env var. v5.4 documents the durability semantic explicitly.

6. **Cold-segment file format extends v4.37 envelope + CRC32**
   rather than inventing a new envelope shape. v4.37 frozen
   surface is stable; a `Segment` envelope is a new `kind` tag
   on top of the same wrapper. STABILITY.md gets one new row
   per envelope kind, not a v10 rewrite.

7. **Manifest file is a separate v10 artifact** (next to the
   v3.x catalog snapshot + v4.37 backup envelopes), tying
   together hot-tier snapshot bytes + cold segment registry +
   WAL baseline offset. Restore flow becomes: manifest read →
   open cold segments → replay WAL from baseline. Pre-v5 db
   files (no manifest) still load through the v3.x / v4.x
   path (cross-version compat).

### What is NOT in v5 (carved out)

  - Spilling indexes themselves to disk. v5 keeps the PB index
    fully in RAM; cold tier means cold *rows*, not cold *index
    entries*. If the index alone exceeds RAM, that's v6 work
    (B-tree pagination on disk). Sweep at 100M with 2 indexes
    is estimated at ~8 GiB index in RAM, which fits the
    deployment assumption (≥ 16 GiB host RAM, documented
    explicitly in PROD_READY.md).
  - Replication of cold-tier segment files. Followers replay
    WAL → freeze independently (same eventual segment state).
    No primary→follower segment-file streaming.
  - Online schema change for cold-tier tables (DROP COLUMN, etc.).
    Cold segments are immutable; schema changes require
    re-freezing affected tables. Out of scope for v5.
  - Compaction of cold segments triggered by *space pressure*
    on disk. v5.2's compaction is shadowing-driven (run when
    shadowed-entry ratio exceeds threshold), not disk-pressure-
    driven. Disk-full handling on cold tier reuses the v4.34
    `SPG_WAL_MIN_FREE_BYTES` water-mark mechanism (extended to
    cover `.spg/segments/` directory).

## L3a — Hot plan for v5.0 (the only sub-version that's "next")

v5.0 is fully isolatable from engine + server. It adds three new
modules to `crates/spg-storage/src/`: `segment.rs`, `bloom.rs`,
`page_cache.rs`. No engine API changes. No server changes.

The plan is linear, TDD-style, no branches. Each step ends with
a verification command; checkpoint to next step only after the
verify is green.

### Step 1 — `BloomFilter` standalone (byte-keyed)

  - File: `crates/spg-storage/src/bloom.rs`
  - Struct: `pub struct BloomFilter { bits: Vec<u64>, num_bits: u64, num_hashes: u32 }`
  - Constructor: `with_target_fp_rate(num_items: usize, fp_rate: f64) -> Self`
    sizes from the standard formula `m = -(n × ln p) / (ln 2)^2`;
    rounds up to the next u64 boundary; `k = ⌈m/n × ln 2⌉` clamped
    to `[1, 32]`.
  - Methods: `insert(&mut self, key: &[u8])`,
    `contains(&self, key: &[u8]) -> bool`.
  - Hash mixing: **FNV-1a 64-bit + SplitMix64**. spg-storage is
    `#![no_std]`, so `std::collections::hash_map::DefaultHasher`
    is out of reach; pulling `ahash` / `wyhash` would break the
    0-deps rule. FNV-1a is 0-deps, no_std-safe, deterministic,
    and acceptable for bloom-filter hashing (hash quality
    requirements are bounded by the bloom's own FP rate, not by
    cryptographic distribution). Mixing:

        h1 = fnv1a_64(key)
        h2 = splitmix64(h1)
        for i in 0..num_hashes:
            bit_idx = (h1.wrapping_add((i as u64).wrapping_mul(h2))) % num_bits
            set bit

    Both functions are `const fn`-compatible u64 arithmetic.
    Double-hashing via SplitMix64-scrambled secondary is the
    Kirsch–Mitzenmacher technique — gives independent bit
    placements with one structural hash, no need for a second
    hash pass over the key bytes.

  - Serialise / deserialise: `to_bytes() -> Vec<u8>`,
    `from_bytes(&[u8]) -> Result<Self, BloomError>` — header
    `[magic u32 = 0xB100_F11E][num_bits u64][num_hashes u32]
    [crc32 u32 over body]` + raw u64 bits (little-endian).

  **Verify:**

    cargo test -p spg-storage --lib bloom -- --nocapture

  with a `fuzz_oracle` that inserts 100K deterministic u64 keys
  (seeded `splitmix64` rng) and asserts false-positive rate
  ≤ 1.2 × target on 100K disjoint probe keys.

### Step 2 — `Segment` file writer

  - File: `crates/spg-storage/src/segment.rs`
  - Layout (extends v4.37 envelope):

        [v4.37 envelope header { kind=SEGMENT, version=1, crc32 }]
        [u32 num_rows]
        [u32 num_pages]
        [BloomFilter bytes  (bloom over PKs)]
        [PageIndex bytes    (Vec<(min_pk, file_offset)>)]
        [Page 0]            (4096 bytes each, padded if last row spills)
        [Page 1]
        ...
        [v4.37 envelope footer { crc32 of full body }]

  - Page format:

        [u32 num_rows_in_page]
        [u32 row_offsets[num_rows_in_page]]  (within-page byte offsets)
        [Row bytes, concatenated]

    Rows serialise via `spg_storage::Row::to_bytes`, already used
    by Catalog::serialize.

  - Writer API: `Segment::write(path: &Path, sorted_rows: impl
    ExactSizeIterator<Item = (PrimaryKey, Row)>) -> io::Result<
    SegmentMeta>`.
  - `SegmentMeta { id: u32, path: PathBuf, min_pk, max_pk,
    num_rows: u64, crc32: u32 }`.
  - Caller's contract: input iterator yields rows in ascending PK
    order. Writer panics (debug) / errors (release) if order
    violated, since downstream binary search depends on it.

  **Verify:**

    cargo test -p spg-storage --test segment_writer
    # asserts: 100K row write produces file that:
    #   - parses via Segment::read_meta
    #   - has correct min_pk / max_pk / num_rows
    #   - body crc32 matches envelope crc32

### Step 3 — `SegmentReader` + bounded page cache

  - File: `crates/spg-storage/src/segment.rs` (same as Step 2)
  - File: `crates/spg-storage/src/page_cache.rs`
  - `PageCache { cap_bytes: usize, entries: LinkedHashMap<(u32,
    u32), Box<[u8; 4096]>> }` — simple LRU; cap configurable
    via `SPG_PAGE_CACHE_BYTES` (default 256 MiB).
  - `SegmentReader::open(path) -> Result<SegmentReader>` —
    parses header, loads bloom + page index into RAM (cheap;
    bloom ~1.2 MB for 1 M rows at 1 % FP rate, page index
    ~4 KB for 1 K pages).
  - `SegmentReader::contains(&pk) -> bool` — bloom-only check.
  - `SegmentReader::lookup(&pk, cache: &mut PageCache) ->
    Option<Row>`:
      1. bloom check (1 µs), return None if negative
      2. binary search page index → page_id, file_offset
      3. cache.get_or_load(seg_id, file_offset) → 4 KB page
      4. parse page header, binary search row offsets, return row
  - `SegmentReader::scan(&self, cache) -> impl Iterator<Item =
    (PrimaryKey, Row)>` — sequential page walk, no bloom.

  **Verify:**

    cargo test -p spg-storage --test segment_reader
    # asserts:
    #   - lookup of every inserted PK returns Some(row) with correct payload
    #   - lookup of 100K random PKs not in segment returns None
    #     with false-positive rate ≤ 1.2 %
    #   - scan yields rows in PK order
    #   - cache.size() never exceeds cap_bytes during lookup loop

### Step 4 — Perf gates

  - File: `crates/spg-storage/tests/perf_gate.rs` (extend existing)
  - New gates:
    - `segment_write_1m_under_2s` — 1 M rows, integer PK + 64-byte
      text payload, write to `/tmp` → wall time ≤ 2 s.
    - `segment_lookup_p99_under_500us` — 1 M-row segment, 10 K
      random PK lookups with warm page cache → p99 ≤ 500 µs.
      (Cold cache numbers separately; ≤ 5 ms on macOS APFS.)
    - `bloom_fp_rate_under_1pct` — 100 K inserts, 100 K disjoint
      probes, FP rate ≤ 1.0 %.

  **Verify:**

    cargo test --release -p spg-storage --test perf_gate

### Step 5 — STABILITY.md update

  - Add to "Frozen on-disk surfaces":
    - Segment envelope: `kind = SEGMENT(0x05)`, `version = 1`,
      body = `[num_rows][num_pages][BloomFilter v1][PageIndex
      v1][Pages]`.
    - BloomFilter v1 byte layout (header + raw bits).
    - PageIndex v1 byte layout (sorted Vec<(PK, file_offset)>).
  - Note: page size 4096 is **not** frozen (operator may tune
    via `SPG_SEGMENT_PAGE_BYTES` in v5.1+). Frozen surface is
    the envelope kind + body field order.

### Step 6 — fmt + clippy + workspace test

    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --release --workspace

  All three exit 0.

### Step 7 — Commit `v5.0.0: Segment file format + bloom filter standalone`

  - Commit message documents that engine + catalog are not yet
    touched; cold-tier integration lands in v5.1.
  - Tag locally? No — v5.0 is not user-visible yet; v5.6 is when
    the major-version SemVer tag fires.

## L3b — Cold plan for v5.1 → v5.6

Bullet-only; full hot plan deferred to each version's L4 trigger.

  - **v5.1 (two-tier read path)**: introduce `RowLocator` enum;
    bump PB value type; teach `Engine::lookup_pk` to branch on
    locator; add `Catalog::load_segments(&[Path])` for test
    fixtures (no freezer yet). Sweep test corpus: 30M-row
    pre-baked segment from v5.0 writer.

  - **v5.2 (freezer)**: `Freezer` thread takes long-lived
    `TxId`; builds staging segment file; atomic swap holds
    engine write lock for the rename + PB batch update only.
    UPDATE/DELETE promote-on-write to hot tier. Chaos:
    `chaos_kill_during_freeze_recovers_clean_state`.

  - **v5.3 (manifest + WAL checkpoint)**: `CatalogManifest`
    v10 file. Crash recovery starts from `wal_baseline_offset`,
    not zero. Boot at 100M < 60 s.

  - **v5.4 (async-commit)**: `SPG_SYNCHRONOUS_COMMIT=off`
    mode. Background flusher thread. Single-client INSERT
    @ 1M ≥ 200 K r/s with async on.

  - **v5.5 (HNSW + allocator + OOM)**: absorbs the legacy
    "v5.0" plan. HNSW persistent, `#[global_allocator]`,
    `set_alloc_error_hook`. Vector tables join freezer path.

  - **v5.6 (final sweep validation)**: tag v5.0.0; close
    roadmap.

## L4 — Triggers

Each Cold → Hot upgrade requires a concrete observable, not
"feels done":

  - **v5.0 → v5.1**: `cargo test --release -p spg-storage --test
    perf_gate -- segment_lookup_p99_under_500us bloom_fp_rate_
    under_1pct segment_write_1m_under_2s` all exit 0 AND
    `cargo clippy --workspace --all-targets -- -D warnings` exit 0.

  - **v5.1 → v5.2**: `cargo test --release -p spg-server --test
    e2e_two_tier` green AND sweep on a hand-baked 30M corpus
    shows spg-server PK p99 ≤ PG PK p99 at 30M.

  - **v5.2 → v5.3**: 30M INSERT loop completes without RSS bail
    (RSS stays ≤ 6 GiB throughout) AND
    `chaos_kill_during_freeze_recovers_clean_state` green.

  - **v5.3 → v5.4**: 100M INSERT loop completes; restart wall
    time ≤ 60 s (measured); v4.42 + v5.0 + v5.3 fixtures all
    cross-version-replay.

  - **v5.4 → v5.5**: `slo_wal_insert_async_commit_above_200K`
    green (single client, `SPG_SYNCHRONOUS_COMMIT=off`); doc
    in PERFORMANCE.md notes the durability window.

  - **v5.5 → v5.6**: HNSW persistent perf gate green; OOM
    chaos test returns clean error.

  - **v5.6 ship**: sweep table in PERFORMANCE.md "v5 final
    sweep" shows spg-server numerically beats PG / MySQL /
    MariaDB on **every** non-empty cell at **every** N from
    10K to 100M. Tag `v5.0.0`. Roadmap closes.

## Deployment assumptions documented in PROD_READY.md (added by v5.6)

  - **Host RAM ≥ 16 GiB** when running with 100 M-row tables
    (hot tier 4 GiB + page cache 256 MiB + indexes ~8 GiB +
    headroom).
  - **Cold-tier directory** (`<db>.spg/segments/`) on the same
    or faster volume as the WAL.
  - **`SPG_SYNCHRONOUS_COMMIT=off`** weakens durability
    semantics (window of CC'd-but-not-durable writes on
    primary crash); v5.4 documents the exact contract and
    measurement guidance.

---

*This document is the v5 contract. NEXT.md points here. Each sub-
version opens its own hot plan when its trigger fires.*
