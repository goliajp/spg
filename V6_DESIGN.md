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
- v6.0.2 (NEON SIMD for cosine/IP + SQ8 ADC) design lands in this
  file as a new L3a-v6.0.2 section after v6.0.1 ships.
- v6.1 (logical replication) design starts fresh as `V6_1_DESIGN.md` after v6.0.5 tags.
- Next-version trigger for v6.0.1 → v6.0.2 is: all v6.0.1 perf
  gates green + `xtests/sqllogictest` 4-corpus still 100% +
  workspace test green + e2e_sq8 green.
