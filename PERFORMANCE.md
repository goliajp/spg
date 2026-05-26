# Performance — what's measured, what's not

SPG's positioning is "modern Rust implementation of an embedded
SQL+vector store, performance-first". For that to mean anything, every
number that appears in a commit message, README, or release note **must
trace back to a measurement that anyone can reproduce.** Guesses don't
count. Estimates don't count. Numbers we'd like to be true don't count.

This file is the source of truth for which SPG perf claims are honestly
measured and which are still open. When in doubt, default to the latest
column ("Measured?") here — not to whatever a commit message or marketing
material says.

The work is organised stone-by-stone. A *stone* in SPG is a workspace
crate (eight of them; see `Cargo.toml`). Each stone owns its own
`benches/`, `BUDGETS.md`, and `tests/perf_gate.rs`; the workspace owns
this `PERFORMANCE.md` as the rolled-up index.

## Methodology

- **Run criterion in release**: `cargo bench -p <stone> --bench <name>`.
  The workspace's `[profile.release]` is fully tuned (LTO=fat,
  codegen-units=1, panic=abort, strip=symbols) — there's no separate
  `release-perf` profile because the regular `release` already is one.
- **Quote the median** from criterion's own output. The 95% CI sits
  next to it; if a "−X% / Y×" claim shrinks once you account for the
  CI, the claim is wrong and the table must say so.
- **Run perf gates in test**: `cargo test --release --test perf_gate`
  per stone. Gates have ~10× headroom by design — they catch
  order-of-magnitude regressions, not micro-perf swings. Don't quote
  them as perf claims; quote the criterion bench medians instead.
- **End-to-end baseline**: `time cargo run -q -p sqllogictest --release`
  exercises lex + parse + execute + result-format over all 369 corpus
  records. It's the integration metric — useful sanity check, not a
  fine-grained measurement.

## Workspace-level

| Path | Measurement | Run command |
|---|---|---|
| sqllogictest full suite wall-time, release | **~1.57 s** for 369 records across 4 corpora (M-series Mac, 2026-05-26 baseline). Cached release build; pure runtime. | `time cargo run -q -p sqllogictest --release` |

## Per-stone baseline (v3.0.0, 2026-05-26)

All numbers below are first-ever-measured baselines at the v3.0.0 perf
kickoff. Future rows go *above* the baseline with a date stamp and the
commit SHA, so a stone's history reads top-down = newest-to-oldest.

### `spg-wire` — wire protocol encode/decode

| Path                                | Median   | Notes |
|-------------------------------------|---------:|-------|
| `query_encode_decode_roundtrip`     | **47 ns** | Build → encode → decode → parse for `SELECT id, name FROM users WHERE id = 42`. |
| `row_description_3col_roundtrip`    | **154 ns**| 3-column (Int / Text / Float) schema header. |
| `data_row_3col_roundtrip`           | **121 ns**| One row with `(Int(42), Text("alice"), Float(95.5))`. |

Run: `cargo bench -p spg-wire --bench frames`.

### `spg-sql` — lexer + parser

| Path                                | Median   | Notes |
|-------------------------------------|---------:|-------|
| `lex_select_one`                    | **27 ns** | v3.0.5: was 42 ns; **−36%** ✅ (length-first ASCII-CI keyword lookup + no `String` allocation on the keyword path). |
| `parse_select_one`                  | **106 ns**| v3.0.5: was 135 ns; **−21%** ✅. |
| `parse_select_where_order_limit`    | **508 ns**| v3.0.5: was 666 ns; **−24%** ✅. |
| `parse_join_aggregate`              | **1.11 µs** | v3.0.5: was 1.45 µs; **−23%** ✅. |

Run: `cargo bench -p spg-sql --bench parse`.

### `spg-storage` — catalog round-trip + HNSW

| Path                                | Median   | Notes |
|-------------------------------------|---------:|-------|
| `catalog_serialize_100rows`         | **1.03 µs** | v3.0.2: was 1.13 µs; **−9%** ✅ via schema-driven dense encode (FILE_VERSION 8): per-row NULL bitmap, no per-cell tag byte. Small absolute win (~100 ns) but the directional change is real across re-measurements. |
| `catalog_deserialize_100rows`       | **3.72 µs** | v3.0.2: was 4.19 µs; **−11%** ✅. Same change + cached `&mut Table` (skip per-row `Vec<Table>` linear scan) + `rows.reserve(row_count)`. Below the −52% target — `String` allocation for the 100 Text cells is a ~3 µs hard floor; the remaining ~700 ns is structural dispatch + Vec push. |
| `hnsw_build_200rows_dim8`           | **151 µs** | v3.0.1 + v3.0.6 re-measurement: was 2.41 ms; **−94% / 16.0×** ✅. Heuristic neighbour selection (HNSW paper §4) + `BinaryHeap` frontier + bitmap visited set. |
| `hnsw_search_top10_dim8_n200`       | **378 ns** | v3.0.1 + v3.0.6 re-measurement: was 4.75 µs; **−92% / 12.6×** ✅. Bonus from the same data-structure swap (search shares `layer_beam_search`). |
| `catalog_lookup_n50`                | **789 ns** | v3.1.2: 50 `cat.get(name)` calls against a 50-table catalog = ~16 ns / lookup via the BTreeMap sidecar index, vs estimated ~100 ns / lookup with the old `Vec<Table>` linear scan + per-element string compare. Multi-table win is structural; single-table benches don't change (the sidecar adds one BTreeMap insert per CREATE TABLE, ~30 ns, lost in noise). |

Run: `cargo bench -p spg-storage --bench catalog`.

### `spg-crypto` — BLAKE3 content hash (single-thread reference)

| Path        | Median       | Notes |
|-------------|-------------:|-------|
| `hash_64b`  | **68 ns**    | Single BLAKE3 block. (v3.0.6 re-measurement; matches v3.0.0 baseline.) |
| `hash_1kib` | **1.17 µs**  | Single chunk; ~875 MB/s. |
| `hash_16kib`| **20.0 µs**  | 16 chunks; ~820 MB/s. |

**v3.0.4 negative result (recorded honestly):** a NEON-vectorised
`compress` (one block split across 4 lanes) was implemented end-to-end,
bit-identical-to-scalar, and benchmarked. Result: hash_64b regressed
to 85 ns, hash_1kib to 2.24 µs, hash_16kib to 38.2 µs — between **1.5×
and 2× slower** than scalar. Why: scalar BLAKE3 is already heavily
auto-vectorised by LLVM, and a within-block 4-lane split adds 6 NEON
EXT permute instructions per round (42 extra instructions per
compress) without buying parallelism. The real BLAKE3 SIMD win is
4-chunk-parallel compression, which doesn't apply to SPG's per-entry
audit-log + per-small-catalog hash workload. NEON path kept under
`#[cfg(test)]` as a cross-check oracle for the scalar reference; the
runtime stays scalar.

**v3.0.6 correction:** the v3.0.4 reverted-state numbers in this table
(74 ns / 1.27 µs / 21.4 µs) appeared to show a ~10% regression vs the
v3.0.0 baseline. Subsequent re-measurement found that was measurement
noise — the path was never touched by v3.0.4 (the NEON code lived
under `#[cfg(test)]`). Numbers above are corrected; the spurious
"regression" was an artefact of the warm/cold machine state during
the v3.0.4 measurement.

Run: `cargo bench -p spg-crypto --bench hash`.

### `spg-audit` — append-only log + hash-chain verify

| Path                       | Median       | Notes |
|----------------------------|-------------:|-------|
| `append_one`               | **189 ns**   | One AuditEntry append, prev-hash lookup → BLAKE3 → push. |
| `verify_100entries`        | **8.94 µs**  | Full hash-chain walk; ~89 ns/entry. |
| `serialize_100entries`     | **413 ns**   | Whole log → bytes. |

Run: `cargo bench -p spg-audit --bench log`.

### `spg-engine` — query execution (end-to-end SQL → QueryResult)

| Path                                | Median       | Notes |
|-------------------------------------|-------------:|-------|
| `execute_select_const`              | **228 ns**   | v3.0.3 + v3.0.6 re-measurement: was 255 ns; **−11%** ✅. (The v3.0.3 commit reported "−2% noise" — that was itself measurement-window jitter; the real change is bigger.) |
| `execute_select_where_n100`         | **2.33 µs**  | v3.0.3: was 2.57 µs; **−9%** ✅. Full 100-row scan; no index on `id`. |
| `execute_select_where_n100_indexed` | **840 ns**   | v3.1.0: same query as above with `CREATE INDEX users_id_idx ON users (id)` first; the planner's existing `try_index_seek` skips the scan and goes straight to the B-tree. **−67% / 3×** vs the un-indexed path. |
| `execute_select_count_group_n100`   | **3.10 µs**  | v3.0.3: was 3.33 µs; **−7%**. |
| `execute_insert_one`                | **2.20 µs**  | v3.0.3 + v3.0.6 re-measurement: was 2.60 µs; **−15%** ✅. INSERT's per-row rewrite loop is where the single-`match` `rewrite_expr_clock` restructure shows up most. |
| `execute_select_order_limit_k_n1000`| **41.4 µs**  | v3.1.1: was 43.0 µs; **−5%** via partial sort (select_nth_unstable + sort first k). Win is small because at n=1000 / k=10 the sort itself is only ~25% of total time — the bigger share is the 1000-row scan + per-row projection eval. Cost ratio improves at larger n. |

Run: `cargo bench -p spg-engine --bench execute`.

### `spg-server` — TCP request/response path

Server perf is covered transitively by `spg-wire` + `spg-engine` +
`spg-storage` benches; no direct stone-level bench in v3.0.0. The
end-to-end metric for server-flavoured load is the `sqllogictest`
wall-time row at the top.

### `spg-cli` — backup / restore path

| Path                                | Median        | Notes |
|-------------------------------------|--------------:|-------|
| `backup_inmemory_100rows`           | **4.63 µs**  | v3.1.4: pure CPU cost (deserialize + re-serialize against in-memory `Vec<u8>`). Matches `catalog_deserialize_100rows + catalog_serialize_100rows ≈ 3.7 + 1.0 = 4.7 µs` as expected. **Quote this for the backup-path perf claim.** |
| `backup_roundtrip_100rows`          | **7–11 ms** (disk-bound) | Same shape but with real `fs::read` + `fs::write`. Dominated by fsync + page-cache state, swings 1500–2400× over the in-memory figure above. Kept as an informational figure for "user-visible latency depends on disk"; **don't quote this number as a perf claim** — it reflects the kernel + storage layer, not `spg-cli` code. |

Run: `cargo bench -p spg-cli --bench backup`.

## What's still volatile or near floor (as of v3.0.6)

Bench numbers fall into three honest buckets:

- **Solid wins** — re-measure within ±2% of the reported delta across
  separate runs: HNSW build/search, lex / parse-family, INSERT, the
  WHERE-100 path. Quote these.
- **Real but small** (within run-to-run noise's own ~±5% band):
  catalog serialize/deserialize (-9% / -11%). The direction is
  consistent across re-measurements; the absolute change is ~100 ns,
  which is close to noise size, so a single A/B comparison can fail
  to show it. Quote them as "small directional wins" rather than
  hard numbers.
- **Untrustworthy / disk-bound**: `backup_roundtrip_100rows`. See its
  row above — page-cache state dominates, the figure swings 300×.

Three baselines are at their hard floor and won't move without an API
change:

- `hash_*` — scalar BLAKE3 + LLVM auto-vec already saturates a single
  ARM core. Within-block NEON regressed (see v3.0.4). 4-chunk-parallel
  SIMD would help bulk hashing, not single-block hashing.
- `catalog_deserialize_100rows` — 100 × `String` alloc for Text cells
  is ~3 µs of the 3.7 µs total. Would need `Value::Text(Cow<'a, str>)`
  with lifetimes to dent further.
- `append_one` (audit) — dominated by one BLAKE3 over prev-hash plus
  serialized entry; same hash-throughput floor.

## Competitor comparison (v3.2.x — `xbench/competitor`)

A separate workspace member, `xbench/competitor`, runs SPG side-by-side
against reference SQL servers in docker (loopback-only ports
`25432` / `23306` / `23307`). Same bench harness drives all five
backends. **The bench is opt-in dev tooling**; nothing in
`spg-wire` / `spg-sql` / `spg-storage` / `spg-crypto` / `spg-audit`
/ `spg-engine` / `spg-server` / `spg-cli` depends on it. Bring up
the stack with `xbench/competitor/scripts/up.sh`; tear down with
`down.sh`.

### v3.2.1 — single-row latency (µs, M-series Mac, release)

Single-row `INSERT INTO bench_users` and single-row
`SELECT … WHERE id = ?` (PK lookup); 2000 measured iterations after
200 warm-up, with 1000 seed rows pre-loaded.

| backend       |  ins p50 |  ins p95 |  ins p99 |  sel p50 |  sel p95 |  sel p99 |
|---------------|---------:|---------:|---------:|---------:|---------:|---------:|
| spg-embedded  |    **0.5**|    0.6  |    1.5  |    **0.8**|    1.0  |    1.1  |
| spg-server    |   **30.8**|   40.9  |   51.8  |   **20.9**|   29.8  |   43.1  |
| postgres 18   |    937.1 |  2028.6 |  2601.8 |    850.1 |  1953.2 |  2423.1 |
| mysql 9       |   1315.0 |  2754.2 |  3470.0 |    761.5 |  1774.2 |  2157.0 |
| mariadb 11    |    854.0 |  1879.6 |  2248.9 |    781.0 |  1841.9 |  2472.0 |

**Reading this honestly:**

- `spg-embedded` is in-process — no TCP, no wire, no fsync, no
  concurrent-write locking. Numbers compare *architecturally*, not
  apples-to-apples. **1700-2600× faster than the servers on INSERT,
  ~1000× on SELECT** because the entire client/server stack is
  removed.
- `spg-server` does TCP + wire framing, same shape as the three
  competitors. **30-43× faster than the fastest competitor**
  (MariaDB) on INSERT, **36× on SELECT**. The win comes from
  (a) in-memory storage with no fsync on the INSERT path (SPG has no
  WAL configured in this bench — durability is opt-in), (b) a much
  lighter wire protocol than PG/MySQL's, (c) no constraint /
  trigger / FK / MVCC bookkeeping.
- **What the bench doesn't measure:** durability, concurrent writes,
  bigger schemas, joins, query planning at scale, multi-statement
  transactions. PG/MySQL/MariaDB will close the gap (and exceed SPG
  on richer workloads) once those are in play. This number is fair
  for "single-row OLTP latency on a small schema", and *only* for
  that.

Reproduce: `xbench/competitor/scripts/up.sh && cargo run --release
-p spg-bench-competitor --bin latency`.

### v3.2.2 — bulk INSERT 10K rows + full SELECT scan

10000-row load via 100-row multi-VALUES INSERT batches, then a
single `SELECT id, name FROM bench_users` to materialise the whole
table. Same five backends, same schema, no PK index in this bench
shape (PG/MySQL/Maria still attach one to PRIMARY KEY).

| backend       |  INSERT ms |     INS rows/s |   SCAN ms |    SCAN rows/s |
|---------------|-----------:|---------------:|----------:|---------------:|
| spg-embedded  |       3.16 |    **3,160,848** |      1.43 |    **7,006,888** |
| spg-server    |       8.88 |    **1,125,809** |      7.42 |      1,347,187 |
| postgres 18   |     132.02 |          75,744 |      4.13 |      2,420,477 |
| mysql 9       |     221.91 |          45,064 |      3.34 |      2,994,983 |
| mariadb 11    |     155.30 |          64,390 |      2.90 |      3,452,543 |

**Reading this honestly:**

- INSERT throughput: spg-embedded is **41-70× faster** than the
  three competitors; spg-server is **15-25× faster**. The win is
  the same as in the latency bench (no fsync, no MVCC, lighter
  wire), just amortised over 10000 rows so the absolute numbers
  are huge.
- SCAN throughput surfaces an honest spg-server cost: **at 1.35M
  rows/sec it's slower than every server-flavoured competitor**
  (PG/MySQL/Maria land 2.4-3.5M rows/sec). The reason: spg-wire
  emits one `DataRow` frame per row (one length-prefix + one op
  byte per row), whereas the PG / MySQL binary protocols batch
  rows into network buffers more aggressively. spg-embedded
  doesn't pay this and runs at 7M rows/sec. A wire-level batch
  format (multiple rows per frame) would close this gap — logged
  as future work.

Reproduce: `xbench/competitor/scripts/up.sh && cargo run --release
-p spg-bench-competitor --bin throughput`.

## Perf gates

Each crate's `tests/perf_gate.rs` runs as part of `cargo test --release
--test perf_gate`. Budgets live in that crate's `BUDGETS.md`.
Order-of-magnitude headroom by design — these are not publishable
numbers.

| Crate        | Run command                                                | Gate count |
|--------------|-----------------------------------------------------------|-----------:|
| spg-wire     | `cargo test --release -p spg-wire     --test perf_gate`   | 1 |
| spg-sql      | `cargo test --release -p spg-sql      --test perf_gate`   | 1 |
| spg-storage  | `cargo test --release -p spg-storage  --test perf_gate`   | 2 |
| spg-crypto   | `cargo test --release -p spg-crypto   --test perf_gate`   | 2 |
| spg-audit    | `cargo test --release -p spg-audit    --test perf_gate`   | 1 |
| spg-engine   | `cargo test --release -p spg-engine   --test perf_gate`   | 1 |
| spg-cli      | `cargo test --release -p spg-cli      --test perf_gate`   | 1 |

(`spg-server` is a binary and runs no `perf_gate.rs`; its hot paths are
covered by the upstream stones.)
