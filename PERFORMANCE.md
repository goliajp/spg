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
| `catalog_serialize_100rows`         | **1.03 µs** | v3.0.2: was 1.13 µs; **−9%** ✅ via schema-driven dense encode (FILE_VERSION 8): per-row NULL bitmap, no per-cell tag byte. |
| `catalog_deserialize_100rows`       | **3.68 µs** | v3.0.2: was 4.19 µs; **−12%** ✅. Same change + cached `&mut Table` (skip per-row `Vec<Table>` linear scan) + `rows.reserve(row_count)`. Below the −52% target — `String` allocation for the 100 Text cells is a ~3 µs hard floor; the remaining ~700 ns is structural dispatch + Vec push. |
| `hnsw_build_200rows_dim8`           | **154 µs** | v3.0.1: was 2.41 ms; **−94% / 15.7×** ✅. Heuristic neighbour selection (HNSW paper §4) + `BinaryHeap` frontier + bitmap visited set. |
| `hnsw_search_top10_dim8_n200`       | **397 ns** | v3.0.1: was 4.75 µs; **−92% / 12×** ✅. Bonus from the same data-structure swap (search shares `layer_beam_search`). |

Run: `cargo bench -p spg-storage --bench catalog`.

### `spg-crypto` — BLAKE3 content hash (single-thread reference)

| Path        | Median       | Notes |
|-------------|-------------:|-------|
| `hash_64b`  | **74 ns**    | Single BLAKE3 block. v3.0.4 measurement-with-no-NEON; the median moves ±10 ns between runs. |
| `hash_1kib` | **1.27 µs**  | Single chunk; ~810 MB/s. |
| `hash_16kib`| **21.4 µs**  | 16 chunks; ~770 MB/s. |

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
| `execute_select_const`              | **250 ns**   | v3.0.3: was 255 ns; **−2%** (near noise — rewrite walk was already a small share of the const-SELECT total). |
| `execute_select_where_n100`         | **2.35 µs**  | v3.0.3: was 2.57 µs; **−9%** ✅. |
| `execute_select_count_group_n100`   | **3.12 µs**  | v3.0.3: was 3.33 µs; **−6%**. |
| `execute_insert_one`                | **2.26 µs**  | v3.0.3: was 2.60 µs; **−13%** ✅. INSERT's per-row rewrite loop is where the single-`match` `rewrite_expr_clock` restructure shows up most. |

Run: `cargo bench -p spg-engine --bench execute`.

### `spg-server` — TCP request/response path

Server perf is covered transitively by `spg-wire` + `spg-engine` +
`spg-storage` benches; no direct stone-level bench in v3.0.0. The
end-to-end metric for server-flavoured load is the `sqllogictest`
wall-time row at the top.

### `spg-cli` — backup / restore path

| Path                                | Median        | Notes |
|-------------------------------------|--------------:|-------|
| `backup_roundtrip_100rows`          | **~12 ms**    | Full read+deserialize+serialize+write loop, including disk syscalls. High variance (5–22 ms across runs) — disk I/O dominates; on-machine number, not portable. |

Run: `cargo bench -p spg-cli --bench backup`.

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
