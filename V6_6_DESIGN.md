# SPG v6.6 design — WAL compression

> Drafted 2026-06-03 after v6.5 series shipped (Observability v2;
> tag `v6.5.7` rolled the series up at commit `7c7379d`).
> Scope: v6.6 series (v6.6.0 → v6.6.5).
> Companion research:
>   `.claude/researches/spg-vs-pg19-comparison.md` §3.WAL
>   `.claude/researches/spg-v6-roadmap-from-pg19.md` §3.v6.6
> Predecessor designs: `V6_DESIGN.md` (vector advancement),
> `V6_1_DESIGN.md` (logical replication), `V6_2_DESIGN.md`
> (optimizer foundation), `V6_3_DESIGN.md` (PG-wire extended),
> `V6_4_DESIGN.md` (SQL polish), `V6_5_DESIGN.md` (observability).

## L0 — v7.0 discipline (inherited)

Same rule:

> **NO ITEM in any v6.x sub-version design may be deferred to a
> later minor without an explicit user-level "OK to defer".**

Deferrals must target a later same-minor sub-version in this
file. Future means a STABILITY §"Out of scope" entry.

## L1 — Roadmap

v6.6 closes the **fourteenth-gap cluster** from the PG-19
audit: WAL footprint reduction. SPG's WAL today stores raw SQL
text per record (v2 / v3 envelope shapes); a 32 K-row workload
puts ~40-50 KB of WAL on disk per second of replication churn
because every INSERT carries its full SQL. Cold-tier segments
also store uncompressed dense row bytes — fine for hot reads
but wasteful for archival data.

v6.6 lands:

1. **LZSS encoder + decoder** — hand-rolled, no_std, no external
   deps. Standard sliding-window dictionary compression (LZSS
   1982 spec). Lives in `spg-crypto` alongside CRC32 / BLAKE3.
2. **WAL v4 compressed-record format** — new record type
   `WAL_V4_TYPE_COMPRESSED` that wraps a compressed payload.
   Encoder applies LZSS when the SQL payload is > threshold
   (default 256 bytes); below threshold it falls back to v3
   (no compression overhead worth it on short records).
3. **Cold-tier segment payload compression** — segment v2 file
   format carries a per-segment compression flag in the header.
   `OwnedSegment::from_bytes` detects + decompresses transparently;
   `Catalog::stage_segment` compresses on write.
4. **`SPG_WAL_COMPRESSION` env knob** — `lzss` (default) /
   `none`. Per-process toggle so a chaos test can disable.
5. **Compression ratio metrics** — `Metrics.wal_bytes_in` /
   `wal_bytes_out` / `segment_bytes_in` / `segment_bytes_out`.
   `/metrics` exposes the ratios.
6. **Backwards-compat reads** — every reader path (WAL replay,
   segment open) accepts both legacy uncompressed and v6.6
   compressed forms. Pre-v6.6 binaries fail loudly on a v4
   record (matches the v6.1.2 envelope-bump upgrade fence).

Hard rules unchanged: **0 external dependencies, no `unsafe`
(aarch64 NEON carve-out only), sqllogictest 100 % pass rate
maintained**.

### Goal numbers (v6.6 ship-gate definition)

| metric | v6.5.7 baseline | v6.6 target | competitor reference |
|--------|-----------------|------------:|----------------------|
| WAL bytes per 10K INSERTs of avg 80-byte SQL | ~830 KB | **≤ 415 KB (≥ 2× ratio)** | PG `wal_compression=on` |
| Cold-tier segment file size on uniform-int payload | full uncompressed | **≤ 50 % via LZSS** | PG `toast.compression=lz4` |
| LZSS encode throughput | n/a | **≥ 50 MiB/s on M1 dev box** | LZ4 ships ~500 MiB/s — LZSS is 10× slower, OK floor |
| Legacy v3 WAL replay through v6.6 binary | works | **byte-equal recovery** | unchanged |
| sqllogictest 4-corpus regression | 100 % | **100 %** | unchanged |

### Out of v6.6 (carved out)

- **LZ4 / zstd / brotli**. Real LZ4 needs hash-chain matching
  + entropy coding (~600+ lines + careful overflow handling),
  zstd needs Huffman + Finite State Entropy (~2000+ lines).
  v6.6 ships LZSS which is the simplest published dictionary
  scheme that still gives ≥ 2× ratios on text and ≥ 1.5× on
  binary payloads. Faster algorithms out of v6.x.
- **WAL record dedup** (per-WAL-file SQL string dictionary
  back-referencing). LZSS gets most of the win at the block
  level; per-file dedup needs an extra indirection layer.
  Out of v6.6.
- **Streaming compression** (compress across record boundaries).
  v6.6 compresses each record's payload independently so torn
  writes only damage one record. Cross-record windowing out of
  v6.x.
- **Dictionary pretraining** (PG's `wal_compression_dict` / zstd
  `--train`). Statically pretrained dictionaries can boost
  ratio on highly-repetitive workloads but add a dictionary
  distribution problem. Out of v6.x.
- **Compression on the replication wire**. v6.1.4 MAGIC_SUB
  frames are still uncompressed; the v6.6 WAL compression only
  affects on-disk storage. Wire-level compression is replication-
  layer surface; out of v6.6.
- **Per-column type-specific compression**. PG TOAST has
  per-type compression (delta encoding for int columns, dictionary
  for low-cardinality strings). v6.6 ships uniform LZSS across
  every payload. Type-specific compression out of v6.x.

## L2 — Version boundaries (v6.6.0 → v6.6.5)

| ver | scope | ship-gate | depends on |
|-----|-------|-----------|------------|
| **v6.6.0** | LZSS encoder + decoder in `spg-crypto::lzss`. Hand-rolled, no_std, no deps. Standard Storer-Szymanski 1982 sliding-window scheme: 4 KiB window, 18-byte max match, (offset, length) back-references encoded as 16-bit + flag-byte bitstream. | `crates/spg-crypto/src/lzss.rs` 12 unit tests covering: empty input, all-zero, all-distinct, repeated-byte run, repeated-substring, full-buffer, max-match boundary, decode-validates-output-length, encoder-decoder round-trip on 1 KiB / 4 KiB / 64 KiB random + canonical payloads | v6.5.7 |
| **v6.6.1** | WAL v4 compressed-record format. New `WAL_V4_TYPE_COMPRESSED = 0x03` type tag. Encoder threshold-gated (default 256 bytes). `SPG_WAL_COMPRESSION` env (`lzss` / `none`, default `lzss`). Decoder dispatch on type tag. v3 records still emitted for sub-threshold payloads. Legacy v2/v3 WAL files replay unchanged through the upgraded binary. | `tests/e2e_wal_compression::small_record_skips_compression` + `…::large_record_compresses` + `…::ratio_at_least_2x_on_repetitive_sql` + `…::legacy_v3_wal_replays_through_v66` | v6.6.0 |
| **v6.6.2** | Cold-tier segment payload compression. Segment file v2 grows a `compression_flag: u8` header byte (0=none, 1=lzss). `OwnedSegment::from_bytes` detects + decompresses transparently. `Catalog::stage_segment` compresses on write. Backwards-compat: every existing segment file (v1, no flag byte) reads unchanged. | `tests/e2e_segment_compression::round_trip_byte_equal` + `…::ratio_at_least_2x_on_uniform_int_column` + `…::legacy_v1_segment_loads_via_v6_6` | v6.6.0 |
| **v6.6.3** | Compression ratio metrics. `Metrics` grows `wal_bytes_uncompressed_in` / `wal_bytes_compressed_out` / `segment_bytes_uncompressed_in` / `segment_bytes_compressed_out` AtomicU64s. `/metrics` exposes the ratios. New env knob `SPG_COMPRESSION_MIN_BYTES` (default 256) controls the v6.6.1 threshold. | `tests/e2e_compression_metrics::wal_ratio_updates_after_inserts` + `…::segment_ratio_updates_after_freeze` + `…::threshold_env_disables_for_small_payloads` | v6.6.1 + v6.6.2 |
| **v6.6.4** | Chaos resilience. Crash mid-WAL-write under compressed format must replay the surviving suffix bytes correctly (the LZSS-encoded record is self-contained per-record, so a torn write only damages its own record — recovery skips to the next sentinel). New chaos test injects a kill-9 mid-`encode_wal_record_compressed` call and verifies recovery. | `tests/chaos_wal_compression_torn_write::crash_mid_compressed_record_skips_to_next_sentinel` | v6.6.1 |
| **v6.6.5** | v6.6 ship rollup — CHANGELOG header, PROD_READY rows 7.39 – 7.42, STABILITY §"WAL compression (v6.6 series)" + carve-outs. | rollup-only; CHANGELOG / PROD_READY / STABILITY merged; 4-corpus 100 %; every v6.6.x e2e from rows above passes. | v6.6.0 → v6.6.4 all |

### Estimated effort

| sub-version | est. days | running total |
|-------------|----------:|--------------:|
| v6.6.0 | 2.0 | 2.0 |
| v6.6.1 | 2.0 | 4.0 |
| v6.6.2 | 1.5 | 5.5 |
| v6.6.3 | 1.0 | 6.5 |
| v6.6.4 | 1.0 | 7.5 |
| v6.6.5 | 0.5 | 8.0 |

Roadmap estimate was 8.5 d.

## Architectural deliberations

### 1 — Why LZSS, not LZ4 / zstd

LZ4's compression ratio on text-heavy SQL is ~2.5× and decompression
is ~500 MiB/s. zstd ratio ~3× at level 3 with ~400 MiB/s decode.
Both need ~600-2000 lines of carefully-tuned bit twiddling +
overflow handling. LZSS:
  - ~150-200 lines of compress + ~80 lines decompress
  - Compression ratio ~2× on text, ~1.5× on binary
  - Decode throughput on dev box: ~100 MiB/s (matches WAL replay
    rate floor)
  - Trivial to audit for correctness — every line is provably
    deterministic

For the v6.6 ship gate of "≥ 2× WAL ratio on repetitive INSERT
SQL", LZSS clears the bar. Future v6.x can swap to LZ4 if a
real workload demands it (the WAL_V4_TYPE_COMPRESSED tag carries
a 1-byte algorithm-id subfield so multiple algorithms can coexist
on disk).

### 2 — Per-record vs streaming compression

PG `wal_compression=on` compresses each record's FPI (full-page
image) independently. SPG follows the same shape: each WAL
record's payload is compressed alone, NOT across record
boundaries. Cost: ~5-10 % ratio loss vs streaming (dictionary
resets per record). Benefit: torn writes corrupt only their
record; recovery skips to the next sentinel and continues.
**Decided: per-record**.

### 3 — Encoder threshold

LZSS overhead is ~3-5 % header + ~1 bit per token. On short
payloads the header dominates and we lose. Threshold: 256
bytes. Below threshold, the existing v3 frame ships unchanged.
Above, the encoder applies and falls back to uncompressed if
the result isn't smaller (compression FAILED — pathological
patterns can swell). Threshold env-tunable.

### 4 — Backwards-compat read path

v6.6 binaries MUST read v1, v2, v3 WAL records + v1 segment
files. Approach:
  - WAL: type-tag dispatch already exists for v3; v4 is just a
    new tag (`0x03`). v2 records (no type tag) still bind to the
    pre-v4 sentinel and use the legacy decoder. Zero changes
    to existing decoders.
  - Segment v1: no compression flag byte. v6.6 reads the v1 magic
    + dispatches to legacy from_bytes when magic matches. v2 magic
    triggers the new "read compression flag" path.

A v2-magic segment file written by a v6.6 binary fails loudly on
a pre-v6.6 binary (the v1 deserialiser sees a bad magic). Matches
the v6.1.2 / v6.1.4 / v6.2.0 envelope-upgrade fences.

### 5 — Compression algorithm-id sub-field

WAL v4 record format:

```text
[u32 (len | WAL_V4_SENTINEL)]
[u32 crc32(type || algo || payload)]
[u8 type = 0x03]
[u8 algo = 0x01]   ← v6.6 LZSS; future LZ4=0x02, zstd=0x03
[compressed payload bytes]
```

Algo byte costs 1 byte per compressed record; lets v6.x add LZ4
without another format bump.

### 6 — Cold-tier segment v2 header

```text
[8-byte magic = "SPGSEG02"]
[u8 compression_flag = 0=none, 1=lzss]
[u8 reserved = 0]
[u8 reserved = 0]
[u8 reserved = 0]
[existing SegmentMeta layout follows]
[compressed (or raw) payload bytes follow]
```

3 reserved bytes give us room for v6.x algorithm additions
without another major bump. The current segment v1 reader sees
SPGSEG02 as a bad magic and errors out — same upgrade fence
shape as the WAL.

## L3a — Hot plan for v6.6.0 (the only sub-version that's "next")

Goal: ship LZSS encoder + decoder + unit tests in
`spg-crypto::lzss`. No WAL integration yet (v6.6.1), no cold-tier
integration yet (v6.6.2).

### Step 1 — Algorithm reference

Storer-Szymanski 1982 sliding-window LZ:
  - Window: 4 KiB ring buffer of recently-seen bytes
  - Look-ahead: 18 bytes max
  - For each position: find longest match in window
    - Match length ≥ 3 bytes → emit (offset, length) reference
    - Otherwise → emit literal byte
  - Tokens packed in 9-bit groups: 1 flag bit + (8-bit literal
    OR 12-bit offset + 4-bit length-3)
  - 8 tokens per "flag byte" + 8 data bytes (variable)

### Step 2 — Module structure

```rust
// crates/spg-crypto/src/lzss.rs
pub fn compress(input: &[u8]) -> Vec<u8> {
    // Standard LZSS encoder. Returns the compressed bytestream.
    // Caller checks output.len() vs input.len() to decide
    // whether to ship the compressed form.
}

pub fn decompress(input: &[u8]) -> Result<Vec<u8>, LzssError> {
    // Decoder. Errors on malformed input (truncated, invalid
    // reference past start, etc).
}

pub enum LzssError {
    Truncated,
    InvalidReference { at: usize },
}
```

### Step 3 — Unit tests

```text
crates/spg-crypto/src/lzss.rs (#[cfg(test)] mod tests)
  ├── empty_input_round_trips_to_empty
  ├── all_zero_64_bytes_compresses
  ├── all_distinct_64_bytes_no_compression
  ├── repeated_byte_run_compresses_dramatically
  ├── repeated_substring_compresses
  ├── max_match_18_bytes_at_window_boundary
  ├── window_wrap_around_at_4_kib_correct
  ├── decode_errors_on_truncated_input
  ├── decode_errors_on_invalid_back_reference
  ├── round_trip_1_kib_random
  ├── round_trip_64_kib_canonical_sql_corpus
  └── ratio_at_least_2x_on_repetitive_text
```

### Step 4 — Acceptance

- `cargo test -p spg-crypto --lib lzss` green (12/12)
- Compression ratio ≥ 2× on a canonical "INSERT INTO t VALUES (n, 'text')"
  corpus of 1024 lines
- Encoder throughput ≥ 50 MiB/s on dev box (criterion bench
  optional)

Commit message: `v6.6.0: LZSS encoder + decoder (no_std, no deps)`.
