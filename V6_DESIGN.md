# SPG v6.0 design — vector advancement

> Drafted 2026-06-02 after v5.5.4 ship sign-off + v5.5 → v6 trigger.
> Scope: v6.0 series (v6.0.0 → v6.0.5).
> Companion research: `.claude/researches/spg-vs-pg19-comparison.md`,
> `.claude/researches/spg-v6-roadmap-from-pg19.md`.

## L1 — Roadmap

v6.0 closes the **single biggest competitive gap** found in the
PG 19 audit: vector quantization. SPG currently leads pgvector by
10–50× on kNN search at the 10K–100K scale, but the lead is **size-
limited** — pure f32 dense storage means 1M dim-128 vectors take
488 MiB RSS for storage alone (excluding HNSW edges). At 10M dim-768
SPG runs out of RAM before pgvector hits its slowdown.

v6.0 fixes this by adding:

1. **SQ8 scalar quantization** — 4× memory compression, 2–3× search
   speedup via shorter SIMD loops. Schema-time declaration
   (`VECTOR(128) USING SQ8`) means storage + index share the same
   quantized representation; no on-the-fly type conversion.
2. **halfvec (f16) type** — 2× memory compression, optional path for
   dimensions where SQ8 precision loss is unacceptable.
3. **NEON SIMD for cosine + inner product** — closes the L2-only
   SIMD gap so all three distance metrics ship at NEON speed.
4. **Vector index live rebuild** — swap quantization / metric /
   HNSW parameters without taking the table offline. Enabled by
   `NswGraph` being on `PersistentVec` (v5.5.0 structural sharing).

Hard rule unchanged: **0 external deps, no `unsafe` (except the
already-scoped aarch64 NEON intrinsics block)**. Existing axioms
A1–A11 (see `spg-v6-roadmap-from-pg19.md` §0) all hold.

### Goal numbers (v6.0 ship-gate definition)

| metric | v5.5.4 baseline | v6.0 target | competitor reference |
|--------|----------------:|------------:|----------------------|
| 1M dim-128 f32 RSS | 488 MiB (just storage) | n/a (kept) | pgvector ~600 MiB |
| 1M dim-128 SQ8 RSS | — | **≤ 200 MiB** | pgvector halfvec ~300 MiB |
| 1M dim-128 SQ8 kNN p50 (server) | — | **≤ 50 µs** | pgvector 1500 µs |
| SQ8 recall@10 vs f32 ground truth | — | **≥ 0.95** | pgvector SQ ~0.95 |
| cosine kNN p50 (server, 10K dim128) | scalar path | **≤ 40 µs** (NEON) | pgvector ~1400 µs |
| HNSW live rebuild downtime | n/a (offline only) | **0 ms write-side, ≤ 5 µs p99 read added** | pgvector: requires `REINDEX` (offline) |

### Out of v6.0 (carved out)

- **PQ (Product Quantization)** — too much complexity for current
  scale; SQ8 + halfvec already cover the gap to ~10M dim-128 on a
  16 GiB host. PQ is v7 territory.
- **IVF index** — HNSW remains sole vector index in v6.0; IVF
  reconsidered if v6.0.5 bench shows HNSW build time blocks
  ingest > 10M scale.
- **Binary quantization (1-bit)** — extreme compression with
  significant recall loss; not on the v6 path.
- **`VECTOR(N) USING PQ8x16`** style multi-codebook DDL — schema
  language only learns `USING SQ8` and `HALF` in v6.0.
- **Per-query distance choice (CAST at query time)** — distance
  metric is graph-build-time + query-time as today; no on-the-fly
  rebuild on `<->` vs `<=>` switch.

## L2 — Version boundaries (v6.0.0 → v6.0.5)

Each row is one shippable commit with its own perf gate and chaos
coverage. Ordered by dependency.

| ver | scope (work units) | ship-gate | depends on |
|-----|--------------------|-----------|------------|
| **v6.0.0** | `crates/spg-storage/src/quantize.rs` — `Sq8Quantizer` + `Sq8Vector` standalone. f32 → u8 line quantization (per-vector affine: min/max → [0,255]). Inverse + ADC distance functions. Fuzz oracle vs f32 (recall@10 ≥ 0.95 on Gaussian + uniform corpora). **No engine touch, no DDL change.** | `tests/perf_gate.rs::sq8_quantize_1m_under_500ms` + `sq8_adc_l2_under_200ns_per_pair` + `sq8_recall_at_10_above_0_95` | v5.5.4 (✓ shipped) |
| **v6.0.1** | DDL `VECTOR(N) USING SQ8`: parser + AST + `DataType::Vector { dim, encoding: VecEncoding }`. Write path quantizes at INSERT. HNSW stores quantized neighbours. Search path computes ADC distances at beam search. Optional f32 rerank step (configurable, default on). On-disk format: v4.37 envelope `kind=VECTOR_QUANTIZED` (new sub-tag, no version bump). | `tests/e2e_sq8::insert_select_roundtrip_preserves_topk` + `tests/perf_gate.rs::sq8_kNN_1m_dim128_p50_under_50us_server` + `tests/perf_gate.rs::sq8_rss_1m_dim128_under_200mib` | v6.0.0 |
| **v6.0.2** | NEON SIMD paths for `inner_product` and `cosine` (currently scalar). Also NEON SIMD for SQ8 ADC distance (u8 dot product via `vdotq_u32` if available, else 16-wide u8 → u16 → f32). | `tests/perf_gate.rs::cosine_dim128_under_50ns` + `inner_product_dim128_under_50ns` + `sq8_adc_dim128_under_25ns` (NEON path); `neon_matches_scalar` for all three (∀ dim ∈ {64, 128, 256, 512, 1024}, ε ≤ 1e-5) | v6.0.0 (needs SQ8 type for SQ8 path) |
| **v6.0.3** | halfvec (f16) type: `VECTOR(N) HALF`. f32 ↔ f16 conversion via aarch64 native f16 (`fcvt`). NEON SIMD `l2 / cosine / inner_product` on f16. On-disk format: same envelope, new sub-tag `VECTOR_F16`. | `tests/e2e_halfvec::insert_select_roundtrip` + `tests/perf_gate.rs::halfvec_kNN_1m_dim128_p50_under_50us_server` + `halfvec_rss_1m_dim128_under_260mib` (≥ 1.9× compression) | v6.0.0 (encoding enum already there) |
| **v6.0.4** | Vector index live rebuild: `ALTER INDEX <idx> REBUILD WITH (encoding = ...)`. Background worker takes long-lived `TxId` snapshot (v4.41.1 multi-slot interface), builds new graph in `.spg/staging/idx_<id>.tmp`, atomic swap under brief `engine.write()`. Writes during rebuild append to both old and new graph (via WAL `freeze_commit`-style replay at swap time). | `tests/e2e_live_rebuild::rebuild_during_writes_consistent` + `tests/perf_gate.rs::live_rebuild_read_p99_overhead_under_5us` + `tests/e2e_chaos::chaos_kill_during_live_rebuild_recovers_old_state` | v6.0.1 + v6.0.3 |
| **v6.0.5** | Vector bench sweep extension: 1M / 10M dim-128 SQ8 + halfvec in `xbench/competitor/`. Recall@10 measurement vs f32 ground truth across all encodings. PROD_READY rows 8.x (vector at scale). CHANGELOG v6.0.0 entry. STABILITY.md frozen surface update (new envelope sub-tags). Tag `v6.0.0`. | Sweep `vector_knn_sweep` shows SPG strict win on every cell at 1M / 10M; CHANGELOG + PROD_READY merged; tag pushed. | v6.0.0 → v6.0.4 all |

### Estimated effort

| sub-version | est. days | running total |
|-------------|----------:|--------------:|
| v6.0.0 | 1.5 | 1.5 |
| v6.0.1 | 2.5 | 4.0 |
| v6.0.2 | 0.5 | 4.5 (down from 1.5 d in roadmap — multi-metric scaffolding already in place) |
| v6.0.3 | 1.5 | 6.0 |
| v6.0.4 | 2.0 | 8.0 |
| v6.0.5 | 0.5 | 8.5 |

Down from the 9.5 d roadmap estimate after auditing v5.5.4: multi-
distance SQL operators (`<->`, `<#>`, `<=>`) + `NswMetric` enum +
planner detection are already shipped (see `eval.rs:1604-1620`,
`storage.rs:1520`, `engine/lib.rs:1901-1903`). v6.0.2 thus narrows
to "NEON SIMD path for the two non-L2 metrics + SQ8 distance" rather
than "multi-distance from scratch".

## Architectural deliberations decided in this audit

1. **SQ8 is per-vector affine (not per-corpus codebook).** Each
   stored vector carries `(min: f32, max: f32, bytes: [u8; dim])`
   header. Trade-off: 8 bytes overhead per vector (negligible at
   dim ≥ 64), but no need for two-pass scan over corpus to learn
   global quantization parameters. Insertion is purely streaming.
   Per-corpus codebook can be added in v7 if profiling shows the
   8B overhead matters.

2. **Quantization choice is schema-time, not query-time.** PG /
   pgvector lets users `query_vec::halfvec` at query time. SPG
   declares it in `CREATE TABLE`: `v VECTOR(128) USING SQ8`. This
   removes runtime type-conversion overhead and makes the storage
   format match the index format. Penalty: changing the encoding
   requires `ALTER INDEX REBUILD` (which v6.0.4 makes online).

3. **f32 rerank step is on by default.** SQ8 search returns top-K
   candidates by ADC (approximate); a final rerank computes exact
   f32 distance using the original (non-quantized) vector. But
   v6.0 stores ONLY the quantized form (4× compression is the
   point) — so "rerank" means re-checking the ADC top-`K * 3`
   candidates with un-saturated distance arithmetic (uint16
   intermediate). Pure-quantized recall stays ≥ 0.95.

4. **Live rebuild reuses v4.41.1 + v5.5.0 infrastructure.** The
   multi-slot `TxId` interface (v4.41.1) provides snapshot reads;
   `NswGraph` on `PersistentVec` (v5.5.0) makes the build path
   lock-free against concurrent INSERTs. Swap step holds
   `engine.write()` for ≤ 1 ms (atomic catalog clone + swap +
   manifest append). v4.42 group commit semantics preserved.

5. **No new on-disk envelope kind.** v4.37 envelope `kind` byte
   stays; new sub-tags added under the existing `kind=NSW_GRAPH`
   block: `encoding = F32 (0) | SQ8 (1) | F16 (2)`. STABILITY.md
   gets one new row for the encoding sub-tag enum, no version
   bump. Forward-compat: pre-v6 binaries reading v6 SQ8/F16
   segments fail loudly with `UnknownVectorEncoding` (same fence
   as v3 WAL unknown-type abort).

6. **Live rebuild does NOT support concurrent encoding migrations.**
   `ALTER INDEX ... REBUILD` is one rebuild at a time per index;
   queuing requests is rejected with `RebuildInProgress`. Avoids
   double-write fan-out complexity.

7. **NEON intrinsics safety boundary unchanged.** The existing
   `#![cfg_attr(target_arch = "aarch64", allow(unsafe_code))]`
   scope (lines 7-10 of `crates/spg-storage/src/lib.rs`) absorbs
   the new cosine / inner-product / SQ8 NEON paths. No new
   `unsafe` outside this scope. x86_64 paths stay scalar (no
   SSE/AVX work in v6.0 — defer to v7 if Linux x86 deployment
   shows up).

## L3a — Hot plan for v6.0.0 (the only sub-version that's "next")

v6.0.0 is the standalone quantizer + ADC distance module. **No
engine, DDL, planner, or wire changes.** It lives entirely in
`crates/spg-storage/src/quantize.rs` (new file) + a perf gate
extension. This is the v4.38-style "land the algorithm core
before any integration" step.

Plan is linear, TDD, no branches. Each step ends with a verify
command; checkpoint to next step only after the verify is green.

### Step 1 — `Sq8Vector` type + constructor

- File: `crates/spg-storage/src/quantize.rs` (new)
- Type:
  ```rust
  pub struct Sq8Vector {
      pub min: f32,
      pub max: f32,
      pub bytes: Vec<u8>,   // length = original dim
  }
  ```
- Constructor: `pub fn quantize(v: &[f32]) -> Sq8Vector`
  - find `(min, max)` over `v` (single pass)
  - degenerate case (all-equal): `min == max` → bytes all `0`
  - linear map `f32 → u8`: `byte = round((x - min) / (max - min) * 255).clamp(0, 255) as u8`
  - constant range 1e-12 floor (avoid div-by-zero on `max - min == 0`)
- Inverse: `pub fn dequantize(&self) -> Vec<f32>`
  - `f32 = min + (byte as f32 / 255.0) * (max - min)`
  - returns degenerate `vec![min; bytes.len()]` if `min == max`

**Verify:**
```
cargo test -p spg-storage --lib quantize::quantize_dequantize_roundtrip_bounded_error
```
Asserts `|dequantize(quantize(v)) - v|_∞ ≤ (max - min) / 255 / 2 + 1e-6` on
1000 random Gaussian vectors of dim ∈ {32, 128, 512, 1024}.

### Step 2 — ADC distance functions

- Same file.
- L2 ADC: `pub fn sq8_l2_distance_sq(a: &Sq8Vector, b: &Sq8Vector) -> f32`
  - Reconstruct intermediate f32 per element (`min + byte/255 * (max-min)`),
    accumulate squared difference. Per-pair cost: 2 mul + 1 add + 1 sub.
  - Initial pure-scalar impl (no NEON yet — v6.0.2's job).
- Cosine ADC: `pub fn sq8_cosine_distance(a: &Sq8Vector, b: &Sq8Vector) -> f32`
  - Standard formula `1 - dot / (||a|| ||b||)` using dequantized intermediates.
- Inner product ADC: `pub fn sq8_inner_product(a: &Sq8Vector, b: &Sq8Vector) -> f32`
  - Negated dot for "smaller = closer" convention (matching pgvector `<#>`).
- Asymmetric (quantized vs f32 query): `pub fn sq8_l2_distance_sq_asymmetric(a: &Sq8Vector, q: &[f32]) -> f32`
  - Same shape but `b` is the un-quantized query vector. Saves the
    query-side quantization cost when the same query is hitting many
    stored vectors (the typical kNN scan case).

**Verify:**
```
cargo test -p spg-storage --lib quantize::sq8_distance_matches_f32_within_tolerance
```
Asserts max relative error ≤ 5% on 10K random vector pairs (dim ∈
{32, 128, 512, 1024}, both Gaussian and unit-sphere uniform).

### Step 3 — Recall@10 fuzz oracle

- File: same.
- Fixture: generate 10K dim-128 random f32 vectors (deterministic
  `splitmix64` seed). Pick 100 queries. Compute exact top-10 in f32
  as ground truth. Quantize the 10K + queries to SQ8. Compute top-10
  via SQ8 ADC. Recall@10 = average overlap fraction.

**Verify:**
```
cargo test -p spg-storage --lib quantize::sq8_recall_at_10_above_0_95
```
Asserts recall@10 ≥ 0.95 on Gaussian corpus, ≥ 0.93 on uniform
unit-sphere corpus.

### Step 4 — Serialise / deserialise

- Same file.
- `Sq8Vector::to_bytes(&self) -> Vec<u8>`:
  `[u32 LE dim][f32 LE min][f32 LE max][bytes...]`
- `Sq8Vector::from_bytes(input: &[u8]) -> Result<Sq8Vector, QuantizeError>`
- `QuantizeError`: `Truncated`, `DimMismatch { expected, got }`.

**Verify:**
```
cargo test -p spg-storage --lib quantize::sq8_serde_roundtrip
```
Asserts `from_bytes(to_bytes(v)) == v` for 1000 random vectors.

### Step 5 — Perf gates

- File: `crates/spg-storage/tests/perf_gate.rs` (extend existing).
- New gates:
  - `sq8_quantize_1m_under_500ms` — quantize 1M dim-128 vectors in
    ≤ 500 ms (release build). Single-threaded.
  - `sq8_adc_l2_under_200ns_per_pair` — `sq8_l2_distance_sq` ≤ 200 ns
    per call (dim 128, 1M calls average). Scalar baseline; v6.0.2
    will tighten with NEON.
  - `sq8_recall_at_10_above_0_95` — duplicate of the lib test, but
    runs as a perf gate so degradation breaks the build.

**Verify:**
```
cargo test --release -p spg-storage --test perf_gate -- sq8_
```

### Step 6 — STABILITY.md update

- Add to "Frozen on-disk surfaces":
  - `Sq8Vector` byte layout: `[u32 LE dim][f32 LE min][f32 LE max][u8 × dim]`
- Note: standalone format; not yet wired into any segment / WAL
  envelope. v6.0.1 introduces the integration.

### Step 7 — fmt + clippy + workspace test

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --release --workspace
```

All must be green before commit.

### Step 8 — Commit

```
v6.0.0-sq8-quantizer: standalone SQ8 scalar quantizer + ADC distance
```

CHANGELOG entry under `[Unreleased]` → `### Added`:
- `spg_storage::quantize::Sq8Vector`: per-vector affine f32 → u8 quantization.
- `sq8_l2_distance_sq` / `sq8_cosine_distance` / `sq8_inner_product` / `sq8_l2_distance_sq_asymmetric`.
- Recall@10 ≥ 0.95 on Gaussian + uniform-unit-sphere corpora.

## L4 — v6.0.0 todos (execution order)

1. ✅ create `crates/spg-storage/src/quantize.rs` + module decl in `lib.rs`
2. ✅ `Sq8Vector` struct + `quantize` + `dequantize`
3. ✅ unit test: roundtrip bounded error
4. ✅ `sq8_l2_distance_sq` + `sq8_cosine_distance` + `sq8_inner_product` + asymmetric variant
5. ✅ unit test: distance matches f32 within tolerance
6. ✅ recall@10 oracle (Gaussian + uniform unit sphere) → 2 unit tests
7. ✅ `to_bytes` / `from_bytes` + `QuantizeError` enum
8. ✅ unit test: serde roundtrip
9. ✅ 3 perf gates in `tests/perf_gate.rs`
10. ✅ STABILITY.md update
11. ✅ fmt + clippy + workspace test green
12. ✅ commit `v6.0.0-sq8-quantizer`

Verification at each todo via the `cargo test` commands listed in
the L3a steps. Anything red → stop and diagnose; do not soften the
test.

## L3a-v6.0.1 — Hot plan for v6.0.1 (DDL + write/read integration)

v6.0.1 wires the v6.0.0 standalone quantizer into the actual engine
surface. End state: `CREATE TABLE t (v VECTOR(128) USING SQ8)` —
INSERTs quantize at the write path, HNSW stores quantized
neighbours, kNN searches via ADC + f32 rerank, on-disk rows + NSW
graph carry the new sub-tag, and SELECT dequantizes on the wire.
**No `FILE_VERSION` bump** (the new row-tag + NSW encoding sub-tag
fit under the v4.37 envelope, per L1 deliberation #5).

Plan is linear, TDD, no branches. Each step ends with a verify
command; checkpoint to next step only after the verify is green.

### Architectural recap (decisions inherited from L1)

- **`Sq8Vector` is the in-memory cell representation, not a wire
  thing.** Row cells for `USING SQ8` columns store
  `Value::Sq8Vector(Sq8Vector)`; the PG wire protocol still
  sends/receives `Vec<f32>` (OID for vector unchanged). The
  4× compression target is in-memory + on-disk, not on the wire.
- **f32 rerank reads from dequantized SQ8, not a second copy.** v6.0
  stores ONLY the quantized form (deliberation #3). Rerank step
  dequantizes the top-`K*3` SQ8 candidates inline and runs exact
  f32 distance against the f32 query.
- **Encoding is schema-time, never query-time.** No `CAST` /
  `::sq8`. The column type carries the encoding; INSERTs accept
  `Value::Vector(Vec<f32>)` and the write path quantizes
  per-column.
- **NEON paths stay scalar in v6.0.1.** v6.0.2 owns SIMD. v6.0.1
  validates correctness + memory + functional perf at the scalar
  ADC speed already proven in v6.0.0 perf gates (≤ 200 ns/pair).

### Step 1 — `VecEncoding` enum + DDL parser

- `crates/spg-sql/src/ast.rs`:
  - New enum `pub enum VecEncoding { F32, Sq8 }` (`Copy + Eq`).
  - `ColumnTypeName::Vector(u32)` →
    `ColumnTypeName::Vector { dim: u32, encoding: VecEncoding }`.
  - `Display`: `VECTOR(N)` for `F32` (back-compat); `VECTOR(N) USING SQ8` for `Sq8`.
- `crates/spg-sql/src/parser.rs:660`: after `parse_paren_size("VECTOR")`,
  peek for ident `USING`; if present, require `SQ8` (case-insensitive,
  any other ident → `ParseError::UnknownVectorEncoding { found }`).
- `crates/spg-storage/src/lib.rs:59`: `DataType::Vector(u32)` →
  `DataType::Vector { dim: u32, encoding: VecEncoding }`. Re-export
  `VecEncoding` from `spg-storage` (`spg-sql` stays storage-free —
  define the enum twice, one per crate, with a `From` bridge in
  `spg-engine`).
- `crates/spg-engine/src/eval.rs:4482`: `ColumnTypeName::Vector { dim, encoding }`
  → `DataType::Vector { dim, encoding: encoding.into() }`.

**Verify:**
```
cargo test -p spg-sql --lib parser::parses_vector_using_sq8
cargo test -p spg-sql --lib parser::vector_default_is_f32
cargo test -p spg-sql --lib parser::rejects_unknown_vector_encoding
cargo test --workspace --lib   # ensure no callsite of DataType::Vector(_) broke
```
Parser tests must round-trip `CREATE TABLE t (v VECTOR(128) USING SQ8)`
through `Display` to the same text.

### Step 2 — `Value::Sq8Vector` variant + helpers

- `crates/spg-storage/src/lib.rs:118` (`enum Value`): add
  `Sq8Vector(Sq8Vector)` variant. Place after `Vector(Vec<f32>)`.
- `impl Value`:
  - `pub fn quantize_to_sq8(&self) -> Option<Value>` — `Value::Vector(v)` →
    `Value::Sq8Vector(quantize(v))`; other variants return `None`.
  - `pub fn dequantize_to_vec(&self) -> Option<Vec<f32>>` — symmetric.
- `impl Value::data_type` (line 163): `Value::Sq8Vector(q)` →
  `Some(DataType::Vector { dim: q.bytes.len() as u32, encoding: Sq8 })`.
- Update `IndexKey` skip (line 264) + every match arm in
  `crates/spg-storage/src/lib.rs` and `crates/spg-engine/src/eval.rs`
  that pattern-matches `Value` — add `Value::Sq8Vector(_)` arms.
  Most behave identically to `Value::Vector` (display, JSON output,
  null-handling); the points that diverge are listed in steps 3–7.

**Verify:**
```
cargo test --workspace --lib
cargo clippy --workspace --all-targets -- -D warnings
```
No new behaviour exercised yet — this step is mechanical exhaustiveness.

### Step 3 — INSERT write path quantizes f32 → SQ8

- `crates/spg-engine/src/eval.rs` INSERT execute path: before
  appending the row, scan column schemas; for each
  `DataType::Vector { encoding: Sq8, dim }` column, replace the
  incoming `Value::Vector(v)` cell with `Value::Sq8Vector(quantize(&v))`.
- Type-check: at insert-time, reject `dim` mismatch
  (`v.len() != dim`) with `EngineError::VectorDimMismatch { expected, got }`
  — same error path as f32 columns; encoding mismatch raised here.
- COPY / parameterised INSERT (pgwire bind path) flows through the
  same point; no separate quantize hook needed.

**Verify:**
```
cargo test -p spg-engine --lib eval::insert_sq8_column_quantizes
cargo test -p spg-engine --lib eval::insert_sq8_dim_mismatch_rejected
```
Both via in-process `Engine::execute`, no server boot.

### Step 4 — HNSW build/insert uses SQ8 ADC

- `crates/spg-storage/src/lib.rs:1271` (`metric_distance` / kNN
  candidate evaluation): dispatch on cell type.
  - `Value::Vector(v)` + `Value::Vector(other)` → existing f32 path.
  - `Value::Sq8Vector(q)` + `Value::Sq8Vector(other)` →
    `sq8_l2_distance_sq` / `sq8_cosine_distance` / `sq8_inner_product`
    per `NswMetric`.
  - Mixed cells (different encoding) within one column should be
    impossible (insert-time enforced) — assert and panic with a
    clear message if reached, do NOT silently dequantize.
- `crates/spg-storage/src/lib.rs:1407` / `:1488` (graph traversal
  during INSERT): same dispatch. Neighbour distance is between two
  stored Sq8Vectors → symmetric ADC.
- `crates/spg-storage/src/lib.rs:1129` / `:1459` (cell clone for
  graph operations): support `Value::Sq8Vector` clone.

**Verify:**
```
cargo test -p spg-storage --lib hnsw_sq8_insert_recall_at_10_above_0_95
```
In-process: insert 10K dim-128 random vectors into a fresh Catalog
with `encoding: Sq8`, query 100 random vectors, recall@10 ≥ 0.95
vs an `encoding: F32` Catalog built from the same corpus.

### Step 5 — kNN query path: ADC beam + f32 rerank

- Query path entry: `eval::execute_select` ordering by `<->` /
  `<#>` / `<=>` on an SQ8 column.
  - Beam search: for each candidate neighbour `n`, distance is
    `sq8_l2_distance_sq_asymmetric(n.sq8, query_f32)` (and the
    cosine / inner-product asymmetric analogues — add to
    `quantize.rs` if not already there; v6.0.0 only landed L2
    asymmetric).
  - Result candidate set carries `(row_id, adc_distance)`.
- Rerank step (configurable, default ON):
  - Take top-`K*3` candidates by ADC.
  - For each: `dequantize(stored_sq8) → Vec<f32>`, then `l2_distance_sq(deq, query)`.
  - Reorder by exact distance; truncate to top-K. Emit.
  - Opt-out path: HNSW search option `rerank: bool` (already lives on
    the HNSW search params struct? if not, add). Default `true`.
- The session-level GUC for rerank is OUT of scope — schema/index-level
  knob only in v6.0.1. (Session GUCs land in v6.0.5 sweep work.)

**Verify:**
```
cargo test -p spg-engine --lib eval::sq8_knn_topk_matches_f32_within_recall
cargo test -p spg-engine --lib eval::sq8_knn_rerank_off_is_pure_adc
```
Both in-process. Topk-match test: 10K dim-128 corpus, 100 queries,
top-10 overlap with f32 ground truth ≥ 0.97 with rerank on, ≥ 0.93
with rerank off.

### Step 6 — On-disk row segment + NSW envelope sub-tag

- `crates/spg-storage/src/lib.rs:2355`-area (row encoding tag table):
  - Tag 6 (Vector) — unchanged: `[u32 LE dim][dim×f32 LE]`.
  - **NEW tag 7 (VectorSq8)**: `[u32 LE dim][f32 LE min][f32 LE max][dim×u8]`.
    Reader for tag 7 reconstructs `Value::Sq8Vector(Sq8Vector { min, max, bytes })`.
- `crates/spg-storage/src/lib.rs:2749`-area (`write_data_type`):
  - `DataType::Vector { encoding: F32, dim }` → existing tag-6 type prefix (back-compat).
  - `DataType::Vector { encoding: Sq8, dim }` → new type-prefix byte
    (encoded inline with dim; see below). Forward-compat fence:
    pre-v6 reader hits this byte and raises `UnknownVectorEncoding`,
    matching the v3 WAL unknown-type abort behaviour.
- NSW graph block (`kind=NSW_GRAPH` envelope payload): add a 1-byte
  `encoding` sub-tag at the front of each block. `F32 = 0` (default
  for back-compat: missing → F32 via length check), `SQ8 = 1`.
  No version field bump — readers detect by NSW block size +
  presence of sub-tag header.

  *Back-compat concern*: existing v5 NSW blocks were written
  without the sub-tag byte. Resolution: NSW block header gains a
  2-byte magic `0xQ8` prefix to disambiguate; absent → assume F32
  (old format). The new prefix is the fence for "this is a v6
  NSW block".
- `crates/spg-storage/src/lib.rs:2447`-area row decoder (`FILE_VERSION 8`):
  handle tag 7 in the dense-row path. Unknown row-tag = hard abort
  (same as today).

**Verify:**
```
cargo test -p spg-storage --lib segment::sq8_row_roundtrip
cargo test -p spg-storage --lib segment::sq8_nsw_block_roundtrip
cargo test -p spg-storage --lib segment::pre_v6_nsw_block_decodes_as_f32
```
Third test guards back-compat: synthesise a v5-shape NSW block
(no `0xQ8` prefix) → decoder yields `encoding: F32`.

### Step 7 — e2e SQ8 roundtrip via `tests/common::ServerBuilder`

- File: `crates/spg-server/tests/e2e_sq8.rs` (new).
- Uses `mod common; use common::*;` (per [[tests-common-pattern]]).
- Test: `insert_select_roundtrip_preserves_topk`:
  1. `ServerBuilder::new().with_pgwire().spawn()`.
  2. `CREATE TABLE vecs (id INT PRIMARY KEY, v VECTOR(128) USING SQ8);`
  3. Insert 1024 deterministic dim-128 vectors (splitmix64 seed).
  4. `SELECT id FROM vecs ORDER BY v <-> $1 LIMIT 10;` with `$1`
     a known query → assert ID set matches f32-ground-truth top-10
     with ≥ 8/10 overlap (recall ≥ 0.8 at small N is acceptable;
     stricter recall is in the perf gate).
  5. Read one row back, dequantize on the client side
     (vector type comes through as f32), check max abs error
     ≤ `(max - min) / 255 / 2 + 1e-6`.

**Verify:**
```
cargo test --release -p spg-server --test e2e_sq8 -- --nocapture
```

### Step 8 — Perf gates: SQ8 kNN p50 + RSS

- File: `crates/spg-server/tests/perf_gate.rs` (extend; this is the
  server-side perf gate file, separate from the storage-side one
  that owns the v6.0.0 gates).
- New gates (both `#[ignore]` by default — they take minutes; run
  via `cargo test --release -p spg-server --test perf_gate -- --ignored`):
  - `sq8_kNN_1m_dim128_p50_under_50us_server`:
    spawn a `ServerBuilder` server, `CREATE TABLE … USING SQ8`,
    bulk insert 1M dim-128 (splitmix64), run 1024 distinct kNN
    queries through pgwire, capture per-query latency, assert
    p50 ≤ 50 µs.
  - `sq8_rss_1m_dim128_under_200mib`:
    same setup, after ingest + a `SELECT` warmup, sample
    `rss_kib_of(pid)` 5× spaced 1 s, assert max ≤ 204_800 KiB.
    Helper `rss_kib_of` lives in `e2e_chaos_freeze.rs:335` —
    promote it to `tests/common/mod.rs` so the perf gate can
    reuse it without duplicating.

**Verify:**
```
cargo test --release -p spg-server --test perf_gate -- --ignored sq8_
```

### Step 9 — STABILITY.md + sqllogictest + workspace green

- `STABILITY.md` — add three new frozen rows:
  1. **DDL grammar**: `VECTOR(N) USING SQ8` (case-insensitive `USING`/`SQ8`).
     Other encodings reserved (`HALF` lands in v6.0.3; `F32` is the
     omit-clause default).
  2. **Row segment tag 7 (VectorSq8)**: layout `[u32 LE dim][f32 LE min][f32 LE max][u8 × dim]`.
  3. **NSW_GRAPH encoding sub-tag**: 2-byte magic `0xQ8` prefix gates
     the encoding byte. Encoding values: `0 = F32`, `1 = SQ8`.
     Reserved: `2 = F16` (v6.0.3).
- Run `xtests/sqllogictest`: `cargo run -q -p sqllogictest --release`.
  Expectation: 4-corpus stays 100% (SQ8 is a new opt-in feature; no
  existing corpus test references it).
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --release --workspace` — full workspace including
  e2e_sq8 (non-ignored). Perf gates stay ignored.

### Step 10 — Commit v6.0.1 + CHANGELOG

```
v6.0.1-sq8-integration: VECTOR(N) USING SQ8 — DDL, write/read path, on-disk
```

CHANGELOG `[Unreleased]` entries:
- **Added**:
  - DDL `VECTOR(N) USING SQ8` (parser + AST + `DataType::Vector { dim, encoding }`).
  - Write path quantizes f32 → SQ8 at INSERT for `USING SQ8` columns.
  - HNSW build + kNN search use SQ8 ADC distances; default-on f32
    rerank pass on top-`K*3` candidates.
  - On-disk: row-tag 7 (VectorSq8) + NSW_GRAPH encoding sub-tag.
- **Changed**:
  - `Value` gains `Sq8Vector(Sq8Vector)` variant.
  - `ColumnTypeName::Vector` / `DataType::Vector` now carry `encoding`.
  - Pre-v6 binaries reading a v6 SQ8 segment now raise `UnknownVectorEncoding`
    (same fence as v3 WAL unknown-type abort).

## L4-v6.0.1 — v6.0.1 todos (execution order)

1. `VecEncoding` enum + `ColumnTypeName::Vector { dim, encoding }` in spg-sql
2. parser `USING SQ8` after `VECTOR(N)` paren close
3. mirror in `DataType::Vector { dim, encoding }` + engine bridge
4. workspace compiles green (mechanical match-arm fan-out)
5. `Value::Sq8Vector(Sq8Vector)` variant + helpers + match arms
6. INSERT path quantizes when column encoding is Sq8
7. unit test: insert quantizes; dim mismatch rejected
8. HNSW build/insert uses SQ8 ADC (cell-type dispatch)
9. unit test: hnsw SQ8 recall ≥ 0.95 vs f32 graph
10. kNN search: asymmetric ADC beam + f32 rerank (rerank default ON)
11. add `sq8_cosine_distance_asymmetric` + `sq8_inner_product_asymmetric` to `quantize.rs` if missing
12. unit test: topk overlap with f32 ground truth (rerank on / off)
13. on-disk row-tag 7 (VectorSq8) + reader/writer
14. NSW_GRAPH encoding sub-tag with `0xQ8` magic prefix
15. unit tests: row roundtrip + nsw block roundtrip + pre-v6 block decodes as F32
16. e2e: `tests/e2e_sq8::insert_select_roundtrip_preserves_topk` via ServerBuilder
17. promote `rss_kib_of` from `e2e_chaos_freeze.rs` to `tests/common/mod.rs`
18. perf gate: `sq8_kNN_1m_dim128_p50_under_50us_server` (ignored by default)
19. perf gate: `sq8_rss_1m_dim128_under_200mib` (ignored by default)
20. STABILITY.md — three new frozen rows
21. sqllogictest 4-corpus stays 100%
22. fmt + clippy + workspace test green
23. commit `v6.0.1-sq8-integration` + CHANGELOG `[Unreleased]` entries

Verification at each todo via the `cargo test` commands listed in
the L3a-v6.0.1 steps. Anything red → stop and diagnose; do not
soften the test. If a recall@K assertion is failing, *first* check
the rerank path is actually engaged — soft-fail-by-disabling-rerank
is exactly what the gate is meant to catch.

## L3a-v6.0.2 — Hot plan for v6.0.2 (NEON SIMD for cosine / IP + SQ8 ADC)

v6.0.2 closes the SIMD gap left by v6.0.0/v6.0.1: today only
`l2_distance_sq` has a NEON path; `inner_product` and `cosine`
fall back to the scalar loop, and SQ8 ADC distances dequantise
element-by-element through scalar f32 arithmetic. v6.0.2 adds
aarch64 NEON paths for both the f32 non-L2 metrics and the SQ8
asymmetric ADC variants (the kNN-scan hot path), with x86_64
keeping the scalar fallback (no SSE/AVX in v6.0 series — deferred
to v7).

Plan is linear, TDD, no branches. Each step ends with a verify
command; checkpoint to next step only after the verify is green.

### Architectural recap

- **NEON intrinsics safety boundary unchanged** (V6_DESIGN
  deliberation #7). The existing crate-level
  `#![cfg_attr(target_arch = "aarch64", allow(unsafe_code))]`
  scope covers the new functions; no new `unsafe` outside that
  scope.
- **dim ≥ 16 and multiple of 16** is the NEON pre-condition for
  SQ8 ADC (one 128-bit lane group = 16× u8 = 4× f32). dim ≥ 4 +
  multiple of 4 stays the f32 pre-condition (matching the
  existing L2 path's contract). Any other shape falls back to
  the scalar loop — same `vec_l2_sq`-style dispatch.
- **Asymmetric SQ8 first, symmetric optional.** Asymmetric ADC
  (stored SQ8 vs f32 query) is the kNN-scan hot path and gets
  every metric (L2 / cosine / IP). Symmetric ADC (stored vs
  stored, used during HNSW build / neighbour heuristic) is
  scalar in v6.0.2 — build-time cost is dominated by graph
  topology work, not distance ns. Revisit in v6.0.5 if profiling
  flags it.
- **No dotprod / FEAT_DotProd dependency.** The 16-wide
  u8 → u16 → f32 widening pattern stays portable across all
  ARMv8.0+ NEON hosts. `vdotq_u32` (FEAT_DotProd, ARMv8.2-A)
  would shave the symmetric SQ8 path further; left for v7 once
  the baseline target is locked.

### Step 1 — f32 NEON: `inner_product_neon` + `cosine_dot_norms_neon`

- File: `crates/spg-storage/src/lib.rs` (add next to
  `l2_distance_sq_neon`).
- `unsafe fn inner_product_neon(a: &[f32], b: &[f32]) -> f32` —
  two parallel `vfmaq_f32` accumulators, same shape as
  `l2_distance_sq_neon`. Caller checks `len % 4 == 0 && len >= 4`.
  Returns `Σ a[i] * b[i]` (positive dot — negation lives in
  `metric_distance`).
- `unsafe fn cosine_dot_norms_neon(a: &[f32], b: &[f32]) ->
  (f32, f32, f32)` — three parallel accumulators for `dot`,
  `na`, `nb`. Same lane-count discipline as the L2 path.
  Caller handles `na == 0 || nb == 0 → INFINITY` and the
  `sqrt_newton_f32(na) * sqrt_newton_f32(nb)` denominator.
- Update `metric_distance(metric, a, b)` to dispatch:
  - `NswMetric::L2` → `l2_distance_sq` (unchanged).
  - `NswMetric::InnerProduct` → `inner_product_neon` /
    `_scalar` via the same `#[cfg(target_arch = "aarch64")]`
    fence as `l2_distance_sq`.
  - `NswMetric::Cosine` → `cosine_dot_norms` (NEON/scalar) +
    norm-sqrt + ratio.

**Verify:**
```
cargo test -p spg-storage --lib metric_neon_matches_scalar
```
Asserts NEON and scalar agree to within ε = 1e-5 across `dim ∈
{64, 128, 256, 512, 1024}` on Gaussian random pairs.

### Step 2 — SQ8 ADC NEON: L2 asymmetric

- File: `crates/spg-storage/src/quantize.rs`.
- `unsafe fn sq8_l2_distance_sq_asymmetric_neon(a: &Sq8Vector,
  q: &[f32]) -> f32`:
  - Loop over 16-u8 chunks of `a.bytes` (4× 4-lane f32x4 of
    `q`).
  - Per chunk: `vld1q_u8` load → `vmovl_u8` widen to 2× u16x8
    → `vmovl_u16` widen each half to 2× u32x4 (4 total) →
    `vcvtq_f32_u32` → multiply by `vdupq_n_f32(step_a)` + add
    `vdupq_n_f32(a.min)` → 4× f32x4 reconstructed `xa`.
  - 4× `vld1q_f32(q.ptr.add(...))` → 4× `diff = vsubq_f32(xa,
    q)` → 2 alternating `vfmaq_f32` accumulators.
  - Final `vaddvq_f32` of summed accumulators.
- Update `sq8_l2_distance_sq_asymmetric` to dispatch via
  `#[cfg(target_arch = "aarch64")]` when `a.bytes.len() >= 16
  && a.bytes.len() % 16 == 0 && a.bytes.len() == q.len()`.
  Scalar path stays for arbitrary dims.

**Verify:**
```
cargo test -p spg-storage --lib sq8_adc_neon_matches_scalar
```
Asserts the new NEON path agrees with the existing scalar
implementation to within ε = 1e-5 across dim ∈ {32, 64, 128,
256, 512, 1024} on 1000 random Gaussian SQ8/query pairs.

### Step 3 — SQ8 ADC NEON: cosine + inner-product asymmetric

- Same file. Same 16-wide widening pattern as step 2 for the
  inner loop; metric-specific tail handles the negation
  (`-dot` for `<#>`) or norm + ratio (`1 - dot / (sqrt(na) *
  sqrt(nq))` for `<=>`).
- `sq8_inner_product_asymmetric_neon`: one accumulator for `dot`.
- `sq8_cosine_distance_asymmetric_neon`: three accumulators —
  `dot`, `na` (reconstructed-squared), `nq` (query-squared).
  Norm-sqrt + zero-guard lives in the safe wrapper, same way
  the scalar version handles it.
- Update both `sq8_inner_product_asymmetric` and
  `sq8_cosine_distance_asymmetric` to dispatch to the NEON path
  under the same pre-condition fence as step 2.

**Verify:**
```
cargo test -p spg-storage --lib sq8_adc_neon_cosine_matches_scalar
cargo test -p spg-storage --lib sq8_adc_neon_ip_matches_scalar
```

### Step 4 — Perf gates

- File: `crates/spg-storage/tests/perf_gate.rs` (extend
  existing).
- New gates:
  - `cosine_dim128_under_50ns` — `cosine_dot_norms_neon` ≤ 50 ns
    per call on dim 128.
  - `inner_product_dim128_under_50ns` — same for IP.
  - `sq8_adc_l2_asymmetric_neon_dim128_under_25ns` — SQ8 ADC L2
    asymmetric ≤ 25 ns/pair (down from the v6.0.0 scalar 200 ns
    floor; the design's L1 goal-numbers row predicted ≥ 2×, the
    25 ns target is ~8× on the assumption of cache-resident
    inputs).
  - All gates are non-`#[ignore]` (each is a 1M-call inner
    loop; the existing `sq8_adc_l2_under_200ns_per_pair` runs in
    the same harness in ~200 ms and stays).

**Verify:**
```
cargo test --release -p spg-storage --test perf_gate -- cosine_dim128 inner_product_dim128 sq8_adc_l2_asymmetric_neon
```

### Step 5 — STABILITY + CHANGELOG + ship

- STABILITY.md: NEON dispatch is implementation-internal; no
  new frozen surface (the function signatures `metric_distance`
  / `sq8_*_asymmetric` are not in the public stability contract).
  Skipped unless step 4 reveals an envelope change — none
  expected.
- `xtests/sqllogictest` 4-corpus stays 100% (no SQL-surface
  change).
- CHANGELOG `[Unreleased]` `### Added`:
  - aarch64 NEON paths for `inner_product`, `cosine`,
    `sq8_l2_distance_sq_asymmetric`,
    `sq8_inner_product_asymmetric`,
    `sq8_cosine_distance_asymmetric`.
  - Three new perf gates: cosine_dim128, inner_product_dim128,
    sq8_adc_l2_asymmetric_neon_dim128.
- `cargo fmt`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --release --workspace`.
- Commit: `v6.0.2-neon-cosine-ip-sq8-adc`.

## L4-v6.0.2 — v6.0.2 todos (execution order)

1. `inner_product_neon` + `metric_distance` dispatch
2. `cosine_dot_norms_neon` + `metric_distance` dispatch
3. unit test: `metric_neon_matches_scalar`
4. `sq8_l2_distance_sq_asymmetric_neon` + dispatch
5. unit test: `sq8_adc_neon_matches_scalar`
6. `sq8_inner_product_asymmetric_neon` + dispatch
7. `sq8_cosine_distance_asymmetric_neon` + dispatch
8. unit tests: SQ8 IP / cosine NEON matches scalar
9. 3 perf gates in `tests/perf_gate.rs`
10. fmt + clippy + workspace test green + sqllogictest 4-corpus 100%
11. CHANGELOG + commit `v6.0.2-neon-cosine-ip-sq8-adc`

Verification at each todo via the `cargo test` commands listed
in the L3a-v6.0.2 steps. Anything red → stop and diagnose; do
not soften the test.

## L3a-v6.0.3 — Hot plan for v6.0.3 (halfvec / `VECTOR(N) HALF`)

v6.0.3 adds the second alternative cell encoding: IEEE-754
binary16 (half-precision). 2× memory compression vs the pre-v6
f32 baseline (≤ 260 MiB target for 1M dim-128, vs 488 MiB raw
f32), at the cost of f32→f16 precision loss bounded by the
half-precision mantissa.

### Architectural deviation from V6_DESIGN L2 (stable-Rust constraint)

The L2 row promised "f32 ↔ f16 conversion via aarch64 native f16
(`fcvt`); NEON SIMD `l2 / cosine / inner_product` on f16."
Stable Rust 1.96 (this workspace's toolchain) does **not** yet
have a stable `f16` primitive or stable `core::arch::aarch64`
f16 intrinsics — both gated behind unstable feature flags
(rust-lang/rust#116909, rust-lang/rust#125606). v6.0.3 ships
with:

- **Manual IEEE-754 binary16 codec** in `crates/spg-storage/
  src/halfvec.rs` (~30 lines of bit manipulation). Bit-exact
  reference output verified against test fixtures matching the
  IEEE 754-2008 examples + `0.0 / -0.0 / ±∞ / NaN` edge cases.
- **Storage:** `HalfVector { bytes: Vec<u8> }` carrying raw u16
  little-endian half-precision bits. Dim = `bytes.len() / 2`.
- **Distance path:** HNSW search dequantises cells to f32
  in-loop and reuses the v6.0.2 f32 NEON paths
  (`inner_product_neon` / `cosine_dot_norms_neon` /
  `l2_distance_sq_neon`). Per-pair cost stays in the same ns
  budget as f32 because the dequantise step is amortised
  against the FMA loop body. No `f32 rerank` pass needed —
  f16 dequantisation is bit-exact to the storage, so the
  beam-search result IS the exact f16-precision answer
  (unlike SQ8 where ADC is approximate).

NEON f16 SIMD lands as v6.0.6 or a stable-Rust-toolchain bump,
whichever comes first. The on-disk format and DDL surface are
designed to accept that future change without a `FILE_VERSION`
bump (same dispatch fence as v6.0.2 NEON dispatch).

### Step 1 — `VecEncoding::F16` + DDL parser `USING HALF`

- `crates/spg-sql/src/ast.rs`: extend `VecEncoding` with `F16`
  variant. `Display` emits `F16` → `"HALF"` (PG / pgvector
  convention; `HALF` is what users type in DDL, not `F16`).
- `crates/spg-sql/src/parser.rs::parse_optional_vector_encoding`:
  accept `"half"` (case-insensitive) → `F16`. The error message
  is updated to list both `SQ8` and `HALF`.
- `crates/spg-storage/src/lib.rs`: mirror `VecEncoding::F16`,
  same Display ("HALF").
- `crates/spg-engine/src/lib.rs::column_type_to_data_type`:
  bridge `SqlVecEncoding::F16` → `VecEncoding::F16`.
- `crates/spg-engine/src/lib.rs::column_def_to_schema`: lift the
  USING-SQ8 fence's mirror to USING-HALF (which we won't have
  here because Step 3 lands the write path in the same commit
  as Step 1).

**Verify:**
```
cargo test -p spg-sql --lib parser::create_table_vector_using_half
cargo test --workspace --lib    # ensure no callsite of VecEncoding broke
```

### Step 2 — `Value::HalfVector` + f32 ↔ f16 codec

- New file `crates/spg-storage/src/halfvec.rs`:
  - `pub struct HalfVector { pub bytes: Vec<u8> }`. Invariant:
    `bytes.len() % 2 == 0`. Constructor `HalfVector::from_f32_slice(&[f32]) -> Self`.
    Inverse `HalfVector::to_f32_vec(&self) -> Vec<f32>`.
  - `f16_from_f32_bits(bits: u32) -> u16`: round-to-nearest-
    even, handles ±∞, NaN, denormals, overflow → ±∞,
    underflow → ±0.
  - `f16_to_f32_bits(bits: u16) -> u32`: inverse, exact for
    every finite f16 (denormals normalised).
- `crates/spg-storage/src/lib.rs::Value` gains
  `HalfVector(crate::halfvec::HalfVector)` variant. `data_type()`
  reports `Vector { dim: bytes.len() / 2, encoding: F16 }`.
- All match arms updated (same pattern as v6.0.1 step 2: rendering
  paths dequantise to f32; on-disk write_value_body lands in
  step 4).
- Unit tests in `halfvec.rs`:
  - Roundtrip f32 → f16 → f32 within (2 ^ -10) × |x| for finite
    normals; bit-exact for representable values
    (`{0.0, 0.25, 0.5, 1.0, 1.5}` etc.).
  - Special-value handling: `±0.0`, `±∞`, `NaN`.
  - `from_f32_slice([])` returns empty `HalfVector`.

**Verify:**
```
cargo test -p spg-storage --lib halfvec::f16_codec_roundtrip
cargo test -p spg-storage --lib halfvec::f16_special_values
cargo test --workspace --lib   # exhaustiveness fan-out
```

### Step 3 — INSERT path + HNSW dispatch

- `crates/spg-engine/src/lib.rs::coerce_value`: new arm
  `(Value::Vector, DataType::Vector { encoding: F16, dim }) if v.len() == dim`
  → `Value::HalfVector(HalfVector::from_f32_slice(&v))`.
- `crates/spg-storage/src/lib.rs::vec_l2_sq` /
  `cell_l2_sq` / `cell_to_query_metric_distance`: add
  `Value::HalfVector(h)` arms that dequantise the cell to f32
  inline then route through the existing f32 distance functions.
  No new NEON kernels — the dequantise loop is what we save when
  stable Rust gets f16 SIMD.
- `crates/spg-storage/src/lib.rs::nsw_search`: F16 columns skip
  the `sq8_rerank` over-fetch — f16 dequantisation is exact at
  storage precision, so the ADC beam IS the exact answer.
  Schema check `encoding == F16` short-circuits the over-fetch
  bump.
- `crates/spg-engine/src/eval.rs::unwrap_vec_pair`: extend
  `to_f32` closure with a `Value::HalfVector` arm (dequant to
  f32 via `to_f32_vec()`).
- `crates/spg-engine/src/aggregate.rs::encode_key`: add
  `Value::HalfVector` arm (similar to the SQ8 arm — emits a
  byte-identical group key).
- `crates/spg-server/src/main.rs::value_to_wire`,
  `pgwire.rs::value_to_pg_text` / `encode_copy_cell`,
  `eval.rs::value_to_text`, `xtests/sqllogictest/src/runner.rs::
  render_cell`: dequantise HalfVector to f32 on output (same
  pattern as SQ8).

**Verify:**
```
cargo test -p spg-engine --lib eval::insert_half_column_converts_f32
cargo test -p spg-storage --lib hnsw_half_recall_at_10_matches_f32_groundtruth
```
The recall test asserts ≥ 0.95 overlap with f32 ground truth
(stricter than SQ8 — half-precision retains ~3 decimal digits).

### Step 4 — On-disk row + DataType / Value tags

- `write_data_type` / `read_data_type`: new tag 15 for
  `DataType::Vector { encoding: F16 }`. Layout `[u32 dim]` (same
  as F32 / SQ8 type-tag side; the encoding lives in the tag
  byte itself).
- `write_value_body` / `read_value_body` dense row path: new
  arm for `Value::HalfVector` →
  `[u32 dim][u16 LE × dim]` body (`2 + 2 * dim` bytes; matches
  the v6.0.0 `2× compression` guarantee at the storage layer).
- `write_value` / `read_value` tag-prefixed catalog DEFAULT
  path: tag 12 for `Value::HalfVector`.
- Pre-v6 readers hit tags 12 / 15 in the catch-all and surface
  `Corrupt("unknown … tag")` — same forward-compat fence as
  v6.0.1 step 6.
- `value_body_encoded_len`: 4 + 2 × dim.

**Verify:**
```
cargo test -p spg-storage --lib half_catalog_serialise_roundtrip_preserves_cells_and_index
```

### Step 5 — e2e + perf gate harness + STABILITY + CHANGELOG + ship

- `crates/spg-server/tests/e2e_half.rs` (new): two tests under
  the `tests/common::ServerBuilder` pattern:
  1. `half_create_insert_select_roundtrip_preserves_topk_order`
     — `CREATE TABLE … USING HALF`, INSERT five rows, assert
     `ORDER BY <-> LIMIT 3` returns the f32 ground-truth IDs.
  2. `half_select_dequantises_to_pgvector_wire_shape` — verify
     dequant precision (≤ 2^-10 × |x| max relative error).
- `crates/spg-server/tests/perf_gate_half.rs` (new,
  `#[ignore]`-marked):
  1. `half_kNN_1m_dim128_p50_under_50us_server`
  2. `half_rss_1m_dim128_under_260mib`
- STABILITY.md: extend the v6.0.1 SQ8 row with HALF — new DDL
  grammar (`USING HALF`), new DataType tag 15, new Value tag 12,
  dense-row body shape.
- CHANGELOG `[Unreleased]`/`[6.0.3]` entry.
- `cargo fmt`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --release --workspace`, sqllogictest
  4-corpus 100%.
- Commit: `v6.0.3-halfvec-f16-integration`.

## L4-v6.0.3 — v6.0.3 todos (execution order)

1. `VecEncoding::F16` in spg-sql + spg-storage + engine bridge
2. parser `USING HALF` (case-insensitive)
3. `crates/spg-storage/src/halfvec.rs` — `HalfVector` + f32 ↔
   f16 codec
4. unit tests: f16 roundtrip + special values
5. `Value::HalfVector` variant + match-arm exhaustiveness
6. INSERT path `coerce_value` arm for `(Vector, Vector { F16 })`
7. HNSW dispatch helpers handle `Value::HalfVector` via dequant
8. unit test: HNSW half recall@10 ≥ 0.95 vs f32 ground truth
9. dense row body + DataType tag 15 + value tag 12
10. catalog roundtrip test
11. e2e_half via ServerBuilder
12. perf_gate_half harness (`#[ignore]`)
13. STABILITY.md row update
14. CHANGELOG entry
15. fmt + clippy + workspace test + sqllogictest 4-corpus 100%
16. commit `v6.0.3-halfvec-f16-integration`

Verification at each todo via the `cargo test` commands listed
in the L3a-v6.0.3 steps. Anything red → stop and diagnose; do
not soften the test.

## L3a-v6.0.4 — Hot plan for v6.0.4 (ALTER INDEX REBUILD — synchronous MVP)

v6.0.4 lands the user-visible feature `ALTER INDEX <name> REBUILD
[WITH (encoding = ...)]` — change a column's vector encoding
without `DROP INDEX` + `CREATE INDEX`, and rebuild the NSW graph
in-place when the underlying corpus has drifted (or when the
encoding needs to change to recover memory after a v6.0.1 / v6.0.3
migration).

### Architectural deviation from V6_DESIGN L2 (scope-narrowing)

The L2 row promised a **live** rebuild: background worker takes a
long-lived `TxId` snapshot, builds the new graph in
`.spg/staging/idx_<id>.tmp`, atomic swap under brief `engine.
write()` with dual-write to old + new during the build. The
chaos-recovery path replays WAL `ALTER REBUILD` markers on
startup to restore old state if killed mid-rebuild.

v6.0.4 ships the **synchronous** MVP instead:

1. `ALTER INDEX <idx> REBUILD [WITH (encoding = ...)]` takes
   `engine.write()` for the duration of the rebuild.
2. Reads + writes block until the rebuild completes — same
   semantics as `CREATE INDEX` today.
3. No background worker, no staging directory, no dual-write,
   no WAL replay machinery.

Rationale: the synchronous path delivers the semantic feature
(change encoding, rebuild topology) without touching the WAL /
freezer / chaos-recovery state machines. The "live" optimisation
(no read-side downtime) is a substantial concurrent-execution
problem that doesn't fit cleanly alongside a six-sub-version
v6.0 series in one motion. It lands as **v6.0.4.1** or
**v6.1.x** after v6.0.5 ships the v6.0.0 tag.

This mirrors the v6.0.3 scope adjustment (NEON f16 SIMD →
scalar codec) — deliver the user-visible feature on the stable
codepath; defer the performance optimisation to a follow-up.

### Step 1 — ALTER INDEX REBUILD SQL grammar

- `crates/spg-sql/src/ast.rs`:
  - New `pub struct AlterIndexStatement { name: String, target:
    AlterIndexTarget }`.
  - `pub enum AlterIndexTarget { Rebuild { encoding:
    Option<VecEncoding> } }`. `encoding = None` rebuilds the
    graph in place without changing encoding (after corpus
    drift, before a v6.0.5 sweep, etc.). `Some(F32 | Sq8 | F16)`
    re-encodes every stored cell to the target.
  - `Statement::AlterIndex(AlterIndexStatement)` variant.
  - `Display` round-trips through `parse` (matches the v5.x
    convention).
- `crates/spg-sql/src/parser.rs`:
  - `ALTER` is the SQL keyword. Followed by `INDEX <ident>
    REBUILD [WITH (encoding = <ident>)]`. `encoding` value
    reuses the v6.0.1 / v6.0.3 case-insensitive matcher
    (`F32` / `SQ8` / `HALF`).
  - Parser tests: bare REBUILD, REBUILD WITH (encoding = SQ8),
    HALF, F32; case insensitivity; unknown encoding rejected;
    missing parens / leftover tokens rejected.

**Verify:**
```
cargo test -p spg-sql --lib parser::alter_index_rebuild
```

### Step 2 — Engine handler

- `crates/spg-engine/src/lib.rs`:
  - Match `Statement::AlterIndex` in the main `execute_*`
    dispatch.
  - `exec_alter_index_rebuild(stmt, …)`:
    1. Find the table holding the index by name (linear scan
       across catalog — index names are globally unique
       within a catalog by `add_nsw_index_inner` enforcement).
    2. Snapshot the column position + current encoding + the
       NSW `m` parameter.
    3. If target encoding is `Some(new_enc)` and `new_enc !=
       current_enc`, re-encode every row's cell at the indexed
       column position via the existing `coerce_value` arms.
       Schema's `DataType::Vector { encoding }` is updated to
       the new encoding.
    4. Drop the old index slot (`indices.remove(idx_pos)`),
       then call `add_nsw_index_inner(name, column_name, m,
       None)` which re-walks rows and rebuilds the graph from
       scratch.
  - Reject ALTER on a B-tree index (unsupported for v6.0.4 —
    only NSW indexes have rebuild semantics worth exposing).
  - Reject ALTER on a non-existent index with
    `EngineError::Unsupported("index … not found")`.

**Verify:**
```
cargo test -p spg-engine --lib exec_alter_index_rebuild
```

### Step 3 — e2e + lib tests

- `crates/spg-server/tests/e2e_alter_rebuild.rs` (new) with two
  cases using the standard `tests/common::ServerBuilder`:
  1. `alter_rebuild_in_place_preserves_topk` — `CREATE TABLE …
     VECTOR(N) USING SQ8`, `CREATE INDEX … USING hnsw`, ingest,
     `ALTER INDEX … REBUILD`, assert kNN top-K matches the
     pre-rebuild result.
  2. `alter_rebuild_with_encoding_switch_sq8_to_f32` —
     starts SQ8, switches to F32 via `ALTER INDEX … REBUILD
     WITH (encoding = F32)`, asserts post-rebuild kNN is
     bit-exact (F32 vs the original SQ8 result is *not* equal,
     but the new graph's top-K matches the f32 ground truth
     within the recall envelope).
- `crates/spg-storage/src/lib.rs` lib test
  `alter_rebuild_replaces_encoding_in_schema` —
  in-process: build a table SQ8, then `Table::rebuild_nsw_index_
  with_encoding(name, F32)` (new helper), assert
  `schema().columns[col].ty.encoding == F32` and every cell is
  now `Value::Vector(_)` not `Value::Sq8Vector(_)`.

**Verify:**
```
cargo test --release -p spg-server --test e2e_alter_rebuild
cargo test -p spg-storage --lib alter_rebuild_replaces_encoding
```

### Step 4 — STABILITY + CHANGELOG + ship

- STABILITY.md DDL grammar row gains
  `ALTER INDEX <name> REBUILD [WITH (encoding = ...)]`. No new
  on-disk surfaces — the rebuilt catalog uses the existing tags
  established in v6.0.1 / v6.0.3.
- CHANGELOG `[6.0.4]` entry: lists the synchronous-MVP scope +
  the deferred async optimisation.
- `cargo fmt`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --release --workspace`, sqllogictest
  4-corpus 100%.
- Commit `v6.0.4-alter-index-rebuild-sync-mvp`.

## L4-v6.0.4 — v6.0.4 todos (execution order)

1. `AlterIndexStatement` + `AlterIndexTarget::Rebuild` + parser
2. parser tests for the new statement
3. engine `exec_alter_index_rebuild` — single function, re-uses
   `coerce_value` for re-encoding + `add_nsw_index_inner` for
   graph rebuild
4. engine lib tests (success + unknown-index + b-tree-index)
5. storage `Table::rebuild_nsw_index_with_encoding` helper for
   the lib-test fixture
6. e2e_alter_rebuild via ServerBuilder
7. STABILITY.md DDL row + CHANGELOG entry
8. fmt + clippy + workspace test green + sqllogictest 4-corpus 100%
9. commit `v6.0.4-alter-index-rebuild-sync-mvp`

Verification at each todo via the `cargo test` commands listed
in the L3a-v6.0.4 steps. Anything red → stop and diagnose; do
not soften the test.

## Risk register

| risk | mitigation |
|------|-----------|
| Recall@10 falls below 0.95 on adversarial corpus | Per-vector affine is robust on natural embeddings (CLIP / BGE / OpenAI); if a benchmark shows < 0.95, fall back to f32 rerank (v6.0.1 default-on already plans this). |
| Quantize cost dominates INSERT path | 500 ms / 1M vectors = 500 ns / vector at dim 128. Compared to existing INSERT overhead (~30 µs server), negligible. |
| NEON SIMD ADC doesn't outperform scalar at small dim | v6.0.2 keeps scalar path for `dim < 32`; NEON for `dim ≥ 32` (the practical floor). |
| f16 (halfvec) precision loss harms recall more than SQ8 | v6.0.3 measures both; either can ship independently. halfvec is an alternative encoding, not a successor — both stay available. |
| Live rebuild swap races with v4.42 group commit | Swap takes `engine.write()` (same lock group commit takes); coordination is automatic. v6.0.4 chaos test pins this. |

## Forward links

- v6.0.1 hot plan: see L3a-v6.0.1 above (drafted 2026-06-02 after v6.0.0 shipped).
- v6.0.2 hot plan: see L3a-v6.0.2 above (drafted 2026-06-02 after v6.0.1 shipped).
- v6.0.3 hot plan: see L3a-v6.0.3 above (drafted 2026-06-02 after v6.0.2 shipped).
- v6.0.4 hot plan: see L3a-v6.0.4 above (drafted 2026-06-02 after
  v6.0.3 shipped). Scoped to synchronous MVP — async "live"
  rebuild lands as v6.0.4.1 / v6.1.x.
- v6.0.5 (sweep + tag v6.0.0) design lands in this file as a new
  L3a-v6.0.5 section after v6.0.4 ships.
- v6.1 (logical replication) design starts fresh as `V6_1_DESIGN.md` after v6.0.5 tags.
- Next-version trigger for v6.0.1 → v6.0.2 is: all v6.0.1 perf
  gates green + `xtests/sqllogictest` 4-corpus still 100% +
  workspace test green + e2e_sq8 green.
