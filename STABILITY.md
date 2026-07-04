# SPG stability contract

What's frozen, what's not, how to read a version bump.

---

## SemVer for SPG

- **MAJOR** bump → breaking change. Old clients may not connect;
  old snapshot/WAL files may need migration. **None has
  happened across v4.x → v7.x** — the contract has held all
  the way through. A v8.0 release would be the first formal
  breaking-change window.
- **MINOR** bump → backwards-compatible additions. New SQL
  features, new wire opcodes, new env vars. Old behaviors
  unchanged. The whole v7.12 epic (full PG FTS + PL/pgSQL
  row-level trigger surface + embedded SQL in trigger bodies)
  shipped as MINOR additions across v7.12.0–7.
- **PATCH** bump → bug fix or doc-only. Wire / SQL / file formats
  unchanged. The v7.12.8 / .9 / .10 / .11 patch cluster was
  pure doc + CI hygiene with byte-identical runtime to
  v7.12.7.

Pre-v1.0 caveat: SPG started at v4.x and treats that as its
"v1.0 contract" for the surfaces below. The same contract
holds across v5.x / v6.x / v7.x — the whole point of the
SemVer discipline is that no MAJOR has been needed yet.

---

## Frozen surfaces

These are stable: client code, snapshot files, and operational
config written against the contract continue to work against any
future v7.x release (the contract carries forward from v4.x
without ever having needed a MAJOR break). v4.31's CI gates
this for the snapshot format; the others are gated by per-row
`prod_ready` tests.

### Native wire protocol (port `SPG_ADDR`)

The 13 opcodes are fixed. Adding new opcodes is a MINOR bump;
changing semantics or layout of an existing opcode is MAJOR.

| op   | name             | direction | added  |
|-----:|------------------|-----------|--------|
| 0x00 | Ping             | client→   | v0.1   |
| 0x01 | Pong             | server→   | v0.1   |
| 0x02 | Auth             | client→   | v1.14  |
| 0x03 | AuthUser         | client→   | v4.1   |
| 0x10 | Query            | client→   | v0.5   |
| 0x11 | RowDescription   | server→   | v0.5   |
| 0x12 | DataRow          | server→   | v0.5   |
| 0x13 | CommandComplete  | server→   | v0.5   |
| 0x14 | ErrorResponse    | server→   | v0.5   |
| 0x15 | Stats            | client→   | v1.0   |
| 0x16 | StatsResponse    | server→   | v1.0   |
| 0x17 | DataRowBatch     | server→   | v3.3.0 |
| 0xFF | Error            | server→   | v0.1   |

Frame layout (every opcode): `[u32 LE payload_len][u8 op][payload bytes]`.

The `prod_ready` row 8.2 test reads back this table and fails
the build if any opcode byte or name changes.

### PostgreSQL wire protocol (port `SPG_PG_ADDR`)

SPG implements the documented subset of PostgreSQL v3 protocol.
The protocol itself is defined by upstream PostgreSQL; SPG never
extends it with custom messages. Stability of this surface is
the stability of PG v3 — see
<https://www.postgresql.org/docs/current/protocol.html>.

What SPG supports (won't be removed in v4.x):

- StartupMessage / R (Auth*) / p (PasswordMessage SASL)
- S (ParameterStatus), K (BackendKeyData), Z (ReadyForQuery)
- Q (simple query) + extended query (P, B, D, E, C, H, S, F)
- T / D / C / E
- d / c / f (COPY)
- X (Terminate)
- SCRAM-SHA-256 auth (RFC 5802)
- pg_catalog subset (pg_class, pg_namespace, pg_database,
  pg_user, pg_tables)

What SPG does not support and won't add in v4.x:

- TLS (`S` startup byte → SPG replies `N`)
- NOTIFY / LISTEN
- Replication protocol (SPG has its own — see `SPG_REPL_ADDR`)
- Cancellation (`CancelRequest`)
- GSSAPI

### SQL surface

Every SQL feature listed in the v3.x corpora (`pgvector`,
`duckdb`, `pg_regress`, `mysql`) and every feature added in
v4.x is part of the stable SQL surface. New features are
additions; existing features keep their semantics within v4.x.

The 4-corpus pass rate is enforced by `cargo run -p sqllogictest
--release` — `prod_ready` row 6.1 (manual eyeball; CI runs it
via the workspace test job).

### WAL record format

The WAL is a contiguous byte stream of length-prefixed records.
Three on-disk formats coexist (writers from a given version emit
one shape, replay accepts all three):

- **v1** (≤ v4.36): `[u32 LE len][len bytes SQL]`. Bit 31 of the
  length is always clear in practice — v1 records are < 2 GiB.
- **v2** (v4.37+): `[u32 LE (len | 0x8000_0000)][u32 LE crc32(payload)][len bytes SQL]`.
  Bit 31 = 1 is the sentinel; bit 30 = 0. CRC32 covers the SQL
  payload.
- **v3** (v4.41+): `[u32 LE (len | 0xC000_0000)][u32 LE crc32(type_byte || payload)][1 byte type][len bytes payload]`.
  Bit 31 = 1, bit 30 = 1. CRC32 covers `[type byte || payload]` —
  a flipped type byte fails the check. `len` counts payload only;
  the type byte is fixed header overhead, kept out of `len` so
  preflight quota math stays linear.

v3 type tags (assignable byte-wide; the namespace is the v3
frame's extension point):

- `0x01` — `auto_commit_sql`: payload = SQL bytes. Replay routes
  through `engine.execute(sql)` — the engine's implicit auto-
  commit is semantically equivalent to the v4.34 `[BEGIN, sql,
  COMMIT]` block the writer expressed, with the header overhead
  reduced from 35 to 9 bytes per write.
- `0x02` — `durability_checkpoint` (v5.4): payload = `[u64 LE
  byte_offset]`, the WAL byte position where this marker frame
  began (i.e. how many WAL bytes preceded it). Semantics: "every
  WAL byte before this marker had successfully reached `fsync`
  at the time the marker was written." Emitted by the flusher
  thread in async-commit mode (`SPG_SYNCHRONOUS_COMMIT=off`,
  v5.4.1+) every N records or N microseconds. **Replay treats
  this as a no-op** — engine state is not mutated, the user-
  SQL `applied` counter does not increment, the recorded offset
  is cross-checked against the frame's actual position in the
  WAL (mismatch logs a stderr warning but replay continues).
  Total frame size = 17 bytes (4 sentinel+len + 4 CRC + 1 type
  + 8 payload).

Backwards-compat rule: every v4.x release accepts every WAL
record format ever written. `tests/cross_version_compat.rs`
holds one fixture per format era (`xtests/compat-fixtures/v4.30/`
for v1, `xtests/compat-fixtures/v4.41/` for v3). Unknown v3 type
bytes are **fatal** during replay — never silently skipped. This
is the forward-compat fence: any new type tag must ship with a
binary that handles it (or, if the on-disk shape changes, bump to
a v4 frame).

Forward-compat is not promised: a ≤ v4.36 binary reading v2 (or
≤ v4.40 reading v3) sees a "huge" length field and aborts. This
follows the same precedent v2 set when it broke v1 readers — the
STABILITY contract is one-way (newer reads older).

### Snapshot file format

- Magic: `SPGDB001` (bare catalog) or `SPGENV01` (envelope with
  user table).
- Envelope versions supported: `1` (v4.1, no CRC) and `2`
  (v4.37, trailing CRC32 over the body). Bare-catalog file
  versions: v1 through v8 (current).
- Writers from v4.37 on emit envelope `2`; readers accept both.
- Backwards-compat rule: every v4.x release must load every
  snapshot ever written. `prod_ready` row 8.6 [machine] gates
  this via `tests/cross_version_compat.rs`.

The cross-version test holds the v4.30 snapshot of a known
catalog (`xtests/compat-fixtures/v4.30/`) and asserts the
current binary restores it and produces the same query results.
A future v4.40 would add `compat-fixtures/v4.40/`; the test
walks every fixture directory.

### Backup bundle format

Self-contained file with magic `SPGBKUP\x01` (v4.25, no CRC) or
`SPGBKUP\x02` (v4.37, trailing CRC32 over the body) — see
`crates/spg-server/src/backup.rs` doc-comment for the byte
layout. Versioned in-band via the `kind` byte (currently 0 =
full, 1 = incremental); adding new kinds is a MINOR bump,
changing the layout of an existing kind is MAJOR. Writers from
v4.37 on emit `\x02`; readers accept both magics.

### Replication protocol (port `SPG_REPL_ADDR`)

Two negotiable wire versions on the same port; the follower
picks the version with the handshake magic byte.

- `SPGREPL\x01` (v4.24) — handshake byte + raw WAL byte stream
  after snapshot. Frozen: any future change creates a new magic
  byte (as v4.36 did).
- `SPGREPL\x02` (v4.36) — same handshake + snapshot exchange,
  then **framed** stream: `[u8 type][u32 LE len][payload]`.
  - type `0x00` — WAL chunk (payload = bytes, parsed by the
    follower's record accumulator just like v1).
  - type `0x01` — status frame (payload = `[u64 LE
    primary_wal_pos][u64 LE wall_time_us]`). Drives the
    follower's `spg_replication_lag_bytes` and
    `spg_replication_lag_seconds` metrics.

Backwards-compat rule: unknown frame types and unknown payload
sizes on known types MUST be tolerated (followers skip them).
This is the extension point for future status fields without a
v3 magic bump.

The complete wire layout for both versions lives in
`crates/spg-server/src/replication.rs`'s module doc.

### Segment file format v1 (v5.0; on-disk cold-tier rows)

The cold-tier segment file written by `spg_storage::encode_segment`
holds an immutable, PK-sorted batch of `(u64_key, payload_bytes)`
rows with sidecar `BloomFilter` + page index for fast probing.
Frozen as v1; future incompatible versions get a new magic.

Layout (little-endian throughout):

  - Magic: `[8 bytes b"SPGSEG\x01\x00"]` (8-byte to align with the
    other SPG magics; the trailing `\x00` is a future-version
    nibble reserved for a v2 bump).
  - Header: `[u32 num_rows][u32 num_pages][u32 page_size_bytes]
    [u64 min_pk][u64 max_pk][u32 bloom_len_bytes]
    [bloom bytes …][u32 page_index_len_bytes][page index bytes …]`.
    `page_size_bytes` is stored in-band so future versions can
    tune without bumping magic.
  - Page index: `[u32 count][(u64 first_pk, u32 file_offset) …]`,
    one entry per page, sorted by `first_pk`. `file_offset` is
    relative to the start of the segment file.
  - Pages: exactly `num_pages × page_size_bytes` bytes. Each
    page contains `[u32 num_rows_in_page][u32 row_offsets[]]
    [row payloads concatenated][zero padding]`. Row payload
    layout: `[u64 key][u32 plen][plen bytes payload]`.
  - Footer: `[u32 crc32_body]` covering everything from the
    `num_rows` field through the last page byte. Uses the same
    `spg_crypto::crc32::crc32` impl as v4.37 envelopes and the
    Bloom v1 above.

Page size 4096 is the v5.0 default and matches APFS / ext4
filesystem page sizes (one page read = one disk I/O). Smaller
than 256 or larger than 65536 is rejected by the writer to keep
the page-granularity I/O assumption sensible.

Segments are immutable once written. Updates / deletes on cold
rows in v5.2+ are handled via promote-on-write to the hot tier;
periodic compaction (v5.x+) merges sparse segments. Neither
operation modifies an existing segment file in place.

### Bloom filter v1 (v5.0; on-disk sidecar for cold-tier segments)

The Bloom filter that prefixes a v5 cold-tier segment file (the
v5.1+ `Segment` envelope embeds one over the segment's PK column
to reject ~99 % of cross-segment probes before any page read)
ships in v5.0 as a standalone `spg-storage` module
(`crates/spg-storage/src/bloom.rs`). Layout, frozen as v1:

  - Magic: `[u32 LE 0xB100_F11E]` — distinct from every envelope
    kind so a stray `BloomFilter::from_bytes` over the wrong
    slice fails fast.
  - Header (after magic): `[u64 LE num_bits][u32 LE num_hashes]
    [u32 LE crc32_body]`. `num_bits` is a positive multiple of 64
    (the bitset is u64-packed); `num_hashes` is in `[1, 32]`;
    `crc32_body` covers `(num_bits || num_hashes || bits...)`
    using the same `crc32` impl as the v4.37 envelope.
  - Body: `[u64 LE bits...]`, exactly `num_bits / 64` words.

Hash mixing — frozen as part of v1 because changing it would
make existing serialised blooms produce different `contains()`
verdicts post-deserialise. Algorithm:

  - Primary hash: **FNV-1a 64-bit** with the canonical offset
    basis `0xcbf2_9ce4_8422_2325` and prime `0x0000_0001_0000_01b3`.
  - Secondary stream: **SplitMix64** scramble of the primary
    hash (Stafford's variant-13 constants:
    `0x9e37_79b9_7f4a_7c15`, `0xbf58_476d_1ce4_e5b9`,
    `0x94d0_49bb_1331_11eb`).
  - Per-key bit positions: Kirsch–Mitzenmacher double-hashing,
    `bit_idx_i = (h1 + i * h2) % num_bits` for `i ∈ [0, num_hashes)`.

A v2 Bloom layout (if ever needed) gets a new magic; the v1
reader rejects unknown magics.

### Catalog file format v9 (v5.2; tagged `RowLocator` on-disk codec)

`FILE_VERSION = 9` shipped in v5.2.0 alongside the v5.2.2 freezer
thread that became the first producer of `Cold` locators. The wire
layout extends v8 by serialising every BTree index entry directly:

  - Each BTree index payload after its header (`[name str][col_pos
    u16][kind u8 = 0]`) gains `[u32 entry_count]` followed by
    `entry_count` `(IndexKey, [u32 locator_count, locator*])`
    pairs. Each locator is exactly 9 bytes via
    `RowLocator::write_le` (`[u8 tag][u64 LE]` for `Hot`,
    `[u8 0x01][u32 LE segment_id][u32 LE page_offset]` for
    `Cold`). `IndexKey` itself uses a tagged codec — `0 = Int(i64
    LE)`, `1 = Text(u16 len + UTF-8)`, `2 = Bool(u8 0/1)`.
  - NSW indices are byte-identical to v8 — they don't carry
    `RowLocator`s. **NswGraph wire format frozen as of v5.5**: the
    v5.5.0 switch of `NswGraph::{levels, layers}` from plain `Vec`
    to PV-backed (`PersistentVec`) structural sharing is an in-memory
    representation change only — `write_nsw_graph` / `read_nsw_graph`
    still emit and consume the identical byte layout (`[u16 m_max_0]
    [u32 entry][u8 entry_level][u32 node_count][u8 level]*[u8
    layer_count]([u32 per_layer_len]([u16 nbr_count][u32 peer]*)*)*`),
    so `FILE_VERSION` stays at 9 and v5.2–v5.4 snapshots round-trip
    unchanged (guarded by
    `tests::nsw_index_topology_persists_through_round_trip`).
  - v8 catalogs read transparently via the version dispatch in
    `Catalog::deserialize` (`MIN_SUPPORTED_FILE_VERSION = 8`).
    Every entry on a v8 BTree decodes as `RowLocator::Hot(_)` via
    `add_index` rebuild — semantically identical to v5.1 in-memory
    state since v8 catalogs never produced `Cold` locators.
    Cross-version replay is gated by `tests/cross_version_compat.
    rs::every_fixture_restores_and_verifies` across v4.30 (v8) +
    v4.41 (v8) + v5.2 (v9) fixtures.

A v5.2 binary writes v9 snapshots; a v5.1 binary cannot read them
(no v9 dispatch). Pre-v5.2 deployments must finish their last
freeze cycle, snapshot to v8, then upgrade. Operators rolling
forward from v4.x see no migration step — the v9 reader pulls
their v8 BTree entries through the legacy rebuild path.

### Manifest file format v10 (v5.3; `<db>.spg/manifest.v10`)

The manifest is the v5.3 boot-time recovery contract. v5.0–v5.2
booted by reading the snapshot at `db_path` and replaying the
entire WAL from byte 0 — at 100M rows that's a 10-minute
operation. The manifest captures three pieces of state that let
the next boot skip the prefix of the WAL that's already been
incorporated:

  - `catalog_crc32` — CRC32 of the snapshot file at `db_path`.
    The boot reader verifies it matches a fresh CRC32 over the
    bytes it just read; a mismatch falls back to legacy
    snapshot+WAL-from-0 replay (the manifest is treated as
    "stale, fall through").
  - `cold_segments[]` — `(segment_id, path, crc32)` per cold
    segment. Each row is read at boot, its CRC re-verified,
    and the bytes loaded via `Catalog::load_segment_bytes` so
    the in-memory `cold_segments` slot is populated before WAL
    replay begins. Pre-v5.3 the operator had to pass these
    paths via `SPG_PRELOAD_COLD_SEGMENT`.
  - `wal_baseline_offset` — byte offset in the WAL where the
    replay should start. Bytes before this offset have been
    incorporated into the snapshot at write time and are safe
    to skip (or `ftruncate` away — `CHECKPOINT` does both).

Wire format (LE; verified by a trailing CRC32 over the body):

```text
[8  magic         = b"SPGMAN01"]
[1  version       = 10         ]
[4  catalog_crc32              ]
[4  num_segments               ]
  per segment:
    [4  segment_id             ]
    [4  path_byte_len          ]
    [N  path_bytes (UTF-8)     ]
    [4  segment_crc32          ]
[8  wal_baseline_offset        ]
[4  trailing_crc32             ]
```

`ManifestError` variants — `BadMagic`, `UnsupportedVersion`,
`Truncated`, `BadCrc32`, `PathNotUtf8`, `TrailingBytes` — are
part of the v10 contract; adding a new failure mode is a major
bump. The byte offsets above are pinned by
`tests/wire_format_offsets_are_stable` inside the manifest
module.

The manifest is **best-effort**: every write site logs failures
to stderr without failing the operation that triggered the write
(snapshot still lands, CHECKPOINT still truncates WAL). On the
boot side, a missing or corrupted manifest causes a graceful
fall-back to legacy snapshot+WAL replay — the only thing lost is
the boot-time optimisation.

CHECKPOINT (v5.3.2) is the explicit operator surface: admin-only
SQL command that snapshots the engine, writes a fresh manifest,
and truncates the WAL to 0 bytes. Documented in
internal readiness matrix row 2.10 alongside its CI gate
(`tests/e2e_manifest.rs`).

### Cold-tier segments (v5.1; side-loaded, not in the catalog snapshot)

Cold segments are first-class artifacts of the v5 cold-tier
plan but live **outside** the catalog snapshot until v5.3's
`CatalogManifest` lands. The operator (or, in v5.2+, the
background freezer thread that writes
`<db>.spg/segments/seg_<id>.spg` after each demotion) is
responsible for keeping the segment files reachable across
restarts; the v9 catalog snapshot itself does not record their
paths.

v5.1 / v5.2 ship four loader contracts that are frozen as long
as the v9 catalog format is in use:

  - `Catalog::load_segment_bytes(Vec<u8>) -> Result<u32, _>`
    registers a segment from caller-owned bytes, returning the
    `segment_id` that `RowLocator::Cold { segment_id, .. }`
    references. Per-segment validation cost (bloom + page-index
    parse) is paid once; the segment lives behind an `Arc` so
    `Catalog::clone` stays the O(N segments) Arc-bump it was
    before v5.1.
  - `Table::register_cold_locators(index_name, iter)` is the
    in-memory primitive for adding `RowLocator::Cold` entries to
    a BTree index — `iter` yields `(IndexKey, RowLocator)`
    pairs. v5.1 callers (the spg-server preload path) produce
    one entry per segment row; v5.2's freezer thread runs the
    same primitive under the engine write lock as part of its
    atomic swap.
  - `Catalog::freeze_oldest_to_cold(table, index, max_rows)`
    (v5.2.2) is the storage-level atomic-swap primitive: builds a
    segment from the first `max_rows` of the hot tier, loads it,
    drops the hot rows, and re-registers their `Cold` locators
    on the named BTree index. Returns the `FreezeReport`
    `(segment_id, frozen_rows, bytes_freed, segment_bytes)` so
    the caller persists `segment_bytes` to disk.

    **v5.5.3 — vector tables freeze too.** A table carrying an NSW
    (vector kNN) index alongside its integer BTree PK is freezable.
    The named freeze index must still be the integer BTree PK (NSW
    graphs have no `RowLocator`/cold concept), and the vector
    column's bytes ride into the segment with the rest of the row
    via the dense encoder, which already handles `Value::Vector`.
    Frozen vector rows stay addressable by PK lookup; the NSW graph
    is rebuilt over the rows left in the hot tier, so **kNN search
    covers the hot tier only** — cold vector rows are reachable by
    key, not by similarity search. Guarded by
    `tests/e2e_freezer::freezer_freezes_vector_table_with_nsw_index`.
  - `Catalog::promote_cold_row(table, index, key)` /
    `Catalog::shadow_cold_row(table, index, key)` (v5.2.3) are
    the cold-tier write-path primitives. The engine routes PK-
    targeted UPDATE through `promote_cold_row` (cold → hot for
    the update) and PK-targeted DELETE through `shadow_cold_row`
    (Cold locator retired; row body becomes garbage until a
    future compaction pass reclaims it).

Operator surfaces for re-attaching cold segments across a
restart:

  - **v5.3+ (preferred)**: the manifest at
    `<db>.spg/manifest.v10` auto-preloads every recorded
    segment on boot, no operator action required. Snapshot
    write sites and the `CHECKPOINT` SQL command emit the
    manifest; see the "Manifest file format v10" section
    above for the contract.
  - **v5.1 (still supported)**: `SPG_PRELOAD_COLD_SEGMENT`
    env var `table:index:path[;table:index:path …]`.
    spg-server parses it at startup and runs the preload
    lazily on the first Op::Query after each spec's
    `(table, index)` both exist. Stable within v5.x; remains
    a fallback for ops workflows where the manifest is
    absent (e.g. snapshot copied without the sibling
    `.spg/` directory).

v5.2.2 freezer-driven segments are written to
`<db_path>.spg/segments/seg_<id>.spg` via `tmp+rename` after
each demotion. In v5.2.x they relied on
`SPG_PRELOAD_COLD_SEGMENT` for the next-boot re-attach; in v5.3+
the freezer also pushes the segment_id → path into the
in-memory map that the next manifest write reads, so the
operator no longer has to wire env vars by hand. v5.2.x freezes
were intentionally **not** WAL-durable — a crash mid-freeze
loses the not-yet-persisted segment; the chaos contract is
"rolled back to pre-freeze state, no corruption". v5.3.2's
`CHECKPOINT` is the explicit operator-side WAL-truncate +
manifest-update operation. The chaos test
`tests/e2e_chaos_freeze.rs::chaos_kill_during_freeze_recovers_clean_state`
pins the bounded-loss invariant.

### Env-var contract

Every env var listed in deployment notes's table is stable. New
env vars are additions; existing env vars keep their semantics
and acceptable values within v4.x.

Special case: the `SPG_FAIL_*` chaos knobs are explicitly
**not** stable — they exist for testing. Operators must not
depend on them; future versions may rename or remove them
without bumping major.

### Async-commit mode (v5.4)

`SPG_SYNCHRONOUS_COMMIT` is the user-facing opt-in for v5.4's
async-commit write path. Stable wire contract:

- Acceptable values for "stay sync (default)":
  unset / empty / `on` / `true` / `1` / `yes` / any value not
  listed below. Sync mode preserves every v4.42 durability
  invariant byte-for-byte.
- Acceptable values for "async on": `off` / `false` / `0`
  (case-insensitive, leading/trailing whitespace trimmed). The
  opt-in keyword set is deliberately narrow so a typo lands in
  the safe direction.
- Companion knob `SPG_FLUSHER_INTERVAL_US` (default 200,
  minimum 10) tunes the **server** flusher cadence; values
  < 10 µs are silently clamped to the floor.
- The **embedded** crate (`spg-embedded`) uses a separate knob,
  `SPG_WAL_WRITER_DELAY_MS` (default 200 ms — PG's
  `wal_writer_delay` default, must be `> 0`), for its own
  background flusher (`lib.rs:3074-3086`). The embedded flusher
  calls `flush_now()` (write + fsync of the pending group
  buffer) each tick rather than emitting `durability_checkpoint`
  markers, but the loss bound is identical in shape: ≤ one
  cadence of confirmed-but-unsynced commits. Note the two
  defaults differ by 1000× (server 200 µs vs embedded 200 ms) —
  the embedded default matches PG's on-disk-WAL analogue, the
  server default is tuned for the high-throughput wire path.

In async mode the WAL write path's `sync_data` call is skipped
on the client's hot path. A background flusher thread emits
`durability_checkpoint` markers (v3 record kind tag 0x02 — see
"WAL record format" above) and runs the periodic fsync. Replay
treats markers as no-ops; the marker's recorded byte offset
lets crash recovery reason about which prefix of the WAL was
durable when the marker landed.

Durability contract under async-commit:

- A SIGKILL between two flusher ticks loses **only** the WAL
  bytes appended in the current window. Bytes covered by the
  most recent durability_checkpoint marker survive replay.
- Worst-case loss = one cadence's worth of CC'd writes (~one
  flusher interval). Operators can shrink the window by
  lowering `SPG_FLUSHER_INTERVAL_US` at the cost of more
  fsyncs / second.
- CHECKPOINT (v5.3.2) continues to force a synchronous fsync
  regardless of the env knob — it's an explicit durability
  barrier and bypassing it would defeat the manifest contract.
- The `durability_checkpoint` wire format is **frozen** as of
  v5.4: 17-byte v3 frame
  `[u32 (len=8 | 0xC000_0000)][u32 crc32][type=0x02][u64 LE byte_offset]`.

`/metrics` adds the v5.4.3 gauges
`spg_durability_lag_bytes` + `spg_durability_lag_seconds` plus
the v5.4.1 counters `spg_flusher_iterations_total` +
`spg_flusher_errors_total`. All four are always rendered; zero
values in sync mode are themselves the "sync confirmed" signal
for dashboards.

### Per-query memory budget (v5.5)

`SPG_MAX_QUERY_BYTES` caps the live heap a single query may hold,
enforced by the v5.5.1 custom `#[global_allocator]`. Stable
contract:

- Unset → default **256 MiB**. The budget is on by default as a
  runaway-query safety net.
- `0` → unlimited (explicit opt-out).
- Any other value → that many bytes.
- Enforcement is **per-query, per-thread net live bytes** (alloc
  minus free). When a query's live allocation crosses the ceiling
  its cancel flag is tripped and the engine's existing 256-row
  cancel checkpoints bail with `EngineError::Cancelled` — the same
  operator-visible surface as a `SPG_QUERY_TIMEOUT_MS` timeout.
- The load path is unaffected: the budget resets per query, so a
  long series of small writes never accumulates toward the cap,
  and high-churn-low-peak queries (allocate-then-free in a loop)
  are not punished.
- **OOM semantics under `panic = "abort"`**: the budget *is* the
  clean-error path for memory exhaustion. A query that would
  exhaust memory is cancelled (`EngineError::Cancelled`) before it
  gets there — including unbounded scans like `SELECT * FROM big
  LIMIT 1`, since LIMIT is applied after materialisation. A *true*
  system allocation failure (an infallible alloc the OS cannot
  satisfy) still aborts the process: under `panic = "abort"` it
  cannot be unwound into an error, and `set_alloc_error_hook` is
  nightly-only. Because the cap is « system RAM, the budget trips
  long before a real OOM, so an actual allocation failure means a
  single oversize allocation or a cap set above available memory —
  an ops-level condition, not a recoverable query. This is the
  deliberate fail-fast stance (no half-written WAL/catalog state
  from an unwind). Guarded by
  `tests/e2e_query_budget::chaos_oom_returns_cancelled_not_panic`.

### SQ8 vector quantization standalone format (v6.0.0)

`spg_storage::quantize::Sq8Vector::to_bytes` / `from_bytes` use
this layout (little-endian throughout):

```
[u32 dim][f32 min][f32 max][u8 × dim]
```

Encoded length = `12 + dim` bytes (`Sq8Vector::encoded_size_for`).
Quantization scheme: per-vector affine f32 → u8 with
`byte = round((x - min) / (max - min) * 255)`. Reconstruction:
`x ≈ min + byte/255 * (max - min)`. Degenerate `max == min`
vectors carry all-zero bytes and reconstruct as `min` exactly.

Reserved for v6.0.1's integration into the segment / WAL envelope
under a new vector-encoding sub-tag (`SQ8 = 1`); the standalone
byte layout above is frozen so that integration code can roundtrip
through it without reaching for a parallel encoder.

### SQ8 schema + on-disk integration (v6.0.1)

v6.0.1 wires the standalone SQ8 layout above into the SQL surface
and the catalog file format. Each of the following is now frozen.

**1. DDL grammar.** `CREATE TABLE` accepts an optional
`USING <encoding>` clause after `VECTOR(N)`:

```
column_def := name VECTOR(N) [ USING SQ8 ] [ NOT NULL | DEFAULT … ]
```

`USING` and the encoding ident are case-insensitive. Omitting the
clause is equivalent to `USING F32` (pre-v6 default — pgvector's
uncompressed `vector` layout). The only encoding recognised in
v6.0.1 is `SQ8`; `HALF` is reserved for v6.0.3, other identifiers
are rejected with `unknown vector encoding`.

**2. Catalog DataType tag (catalog file format v9, deserialise
side).** `write_data_type` / `read_data_type` extend with a new
tag:

| tag | payload     | meaning                    |
|----:|-------------|----------------------------|
|   6 | `[u32 dim]` | `VECTOR(N)` (`encoding = F32`) — pre-v6, unchanged |
|  14 | `[u32 dim]` | `VECTOR(N) USING SQ8`      |

Pre-v6 binaries reading a v6 catalog with tag 14 fail loudly with
`Corrupt("unknown data type tag: 14")` — the explicit forward-
compat fence called out in V6_DESIGN deliberation #5.

**3. Tag-prefixed value codec (catalog DEFAULT path).** `write_value`
/ `read_value` extend with:

| tag | payload                                       | maps to              |
|----:|-----------------------------------------------|----------------------|
|  11 | `[u32 dim][f32 min][f32 max][u8 × dim]`       | `Value::Sq8Vector`   |

Body shape is byte-identical to the standalone `Sq8Vector::to_bytes`
above. Pre-v6 readers hit tag 11 in `read_value`'s catch-all and
surface `Corrupt("unknown value tag: 11")`.

**4. Dense-row body (schema-driven, FILE_VERSION 8 / 9).**
`write_value_body` / `read_value_body` dispatch on the column's
declared encoding:

- `DataType::Vector { encoding: F32, dim }` → `[u32 dim][f32 * dim]`
  (pre-v6 shape, unchanged).
- `DataType::Vector { encoding: Sq8, dim }` → `[u32 dim][f32 min]
  [f32 max][u8 * dim]` — `12 + dim` bytes per cell.

No `FILE_VERSION` bump; the encoding lives in the column-type tag,
not the row header.

**5. NSW graph block.** v6.0.1 does **not** add a sub-tag to the
NSW graph block — the topology is encoding-agnostic (it only
stores adjacency lists by row index), and the cell encoding is
carried by the per-column type tag above. The `kind=NSW_GRAPH`
block byte layout is unchanged from v5.5.

### halfvec — `VECTOR(N) USING HALF` (v6.0.3)

v6.0.3 adds the second alternative encoding: IEEE-754 binary16
(half-precision). Each of the following is frozen.

**1. DDL grammar extension.** `USING <encoding>` learns the
keyword `HALF` (pgvector convention). Case-insensitive on both
`USING` and `HALF`. `HALF` selects `VecEncoding::F16`; omitting
the clause keeps the pre-v6 `F32` default.

**2. Catalog DataType tag (catalog v9, deserialise side).**
`write_data_type` / `read_data_type` gain:

| tag | payload     | meaning                       |
|----:|-------------|-------------------------------|
|  15 | `[u32 dim]` | `VECTOR(N) USING HALF` column |

Pre-v6 binaries reading tag 15 fail with `Corrupt("unknown data
type tag: 15")` — same forward-compat fence as tag 14 (SQ8).

**3. Tag-prefixed value codec.** `write_value` / `read_value`
gain:

| tag | payload                              | maps to                |
|----:|--------------------------------------|------------------------|
|  12 | `[u32 dim][u16 LE × dim]`            | `Value::HalfVector`    |

Body shape: dimension followed by raw little-endian IEEE-754
binary16 bits — `4 + 2 × dim` bytes total.

**4. Dense-row body.** Schema-driven by the column's encoding:

- `DataType::Vector { encoding: F32, dim }` → unchanged.
- `DataType::Vector { encoding: Sq8, dim }` → unchanged.
- `DataType::Vector { encoding: F16, dim }` →
  `[u32 dim][u16 LE × dim]` (`4 + 2 × dim` bytes).

**5. f16 codec.** The IEEE 754-2008 binary16 round-to-nearest-
even codec in `spg_storage::halfvec` is part of the contract —
bit-for-bit reproducible across hosts. `f16_from_f32_bits` /
`f16_to_f32_bits` (raw u32 ↔ u16) are public surface kept
stable for serialisation parity.

### v6.0 series — release roll-up (v6.0.5)

The v6.0 series ships four alternative vector-cell paths + an
in-place encoding-migration DDL. Frozen surfaces, all listed
above, are recapped here for release lookup:

- **DDL grammar**: `VECTOR(N) [USING SQ8 | USING HALF]` (v6.0.1,
  v6.0.3); `ALTER INDEX <name> REBUILD [WITH (encoding = …)]`
  (v6.0.4).
- **Catalog DataType tags**: `14` = SQ8 (v6.0.1), `15` = HALF
  (v6.0.3). Both `[u32 dim]` payload.
- **Value tags** (catalog DEFAULT path): `11` = SQ8 (v6.0.1),
  `12` = HALF (v6.0.3).
- **Dense-row bodies**: SQ8 → `[u32 dim][f32 min][f32 max]
  [u8 × dim]`; HALF → `[u32 dim][u16 LE × dim]`.
- **No FILE_VERSION bump.** Pre-v6 binaries reading any of the
  new tags surface `Corrupt("unknown … tag")` — the forward-
  compat fence declared in V6_DESIGN deliberation #5.
- **NSW graph block layout** is encoding-agnostic and unchanged
  from v5.5.
- **f16 codec** in `spg_storage::halfvec`: `f16_from_f32_bits`
  / `f16_to_f32_bits` produce bit-for-bit reproducible IEEE
  754-2008 binary16 across every host (round-to-nearest-even,
  subnormal flush-to-zero, ±∞ saturation).
- **ALTER INDEX REBUILD** is synchronous in v6.0.4. A future
  sub-version may relax it to a non-blocking ("live") path
  without breaking the grammar.

Implementation-internal surfaces NOT in the contract:

- NEON dispatch shape (`inner_product_f32`, `cosine_dot_norms_f32`,
  the SQ8 ADC asymmetric paths). v6.0.2 ships these but they're
  `#[doc(hidden)]` — internal arithmetic can evolve in any
  release.
- HNSW adjacency storage. `Vec<Vec<usize>>` per layer today; may
  pack to `Vec<u32>` with offsets in v6.1.x. Snapshot format
  carries the topology shape, so callers don't observe the
  change.

### ALTER INDEX REBUILD (v6.0.4)

v6.0.4 adds a DDL surface for synchronous index rebuild. The
grammar is frozen; the implementation strategy (synchronous
vs. "live" async) is not part of the stability contract — v6.0.4
holds the engine write-lock for the rebuild duration, but a
later sub-version may relax this to a non-blocking path without
breaking the grammar.

```text
ALTER INDEX <name> REBUILD [WITH (encoding = <enc>)]
```

- `<name>` is the NSW index name as declared by `CREATE INDEX …
  USING hnsw`.
- `<enc>` is one of `F32` / `SQ8` / `HALF` (case-insensitive),
  same set the v6.0.1 / v6.0.3 `USING <encoding>` clause
  recognises.
- Omitting the encoding clause rebuilds the graph in place
  without re-encoding cells.
- The grammar accepts only NSW indexes — BTree indexes return
  `Unsupported` (the operator has no rebuild semantics for a
  B-tree).

No new on-disk surfaces — the rebuilt catalog uses the existing
DataType / Value / dense-row tags established in v6.0.1 /
v6.0.3.

### effective_wal_level (v6.1.8)

Gate for the MAGIC_SUB endpoint. Mirrors PG's `wal_level`
config var:

```text
SET effective_wal_level = 'logical'   -- opens MAGIC_SUB
SET effective_wal_level = 'replica'   -- closes MAGIC_SUB (default)
SHOW effective_wal_level              -- "replica" or "logical"
```

- Default at startup is `replica`. Override via
  `SPG_WAL_LEVEL=logical` env var.
- The value is global (not session-local) and lives in
  `ServerState`. Operators flipping at runtime via SET affect
  every subsequent MAGIC_SUB handshake immediately.
- MAGIC_V1 / MAGIC_V2 follower paths are unaffected by this
  setting — they keep working in either mode.
- A MAGIC_SUB connection attempt while the level is `replica`
  gets an `"MAGIC_SUB rejected: effective_wal_level must be
  \`logical\`"` error response on the wire and the connection
  drops.
- `wal_level` is NOT persisted across restarts. The env var
  is the persistence mechanism; runtime SET overrides last
  until the next boot.

### WAIT FOR WAL POSITION (v6.1.7)

Consistent-read barrier for follower-based read-after-write.
The grammar is frozen; future v6.1.x sub-versions may add
companions (`SHOW WAL POSITION`, etc.) without changing this
form.

```text
WAIT FOR WAL POSITION <pos>
WAIT FOR WAL POSITION <pos> WITH TIMEOUT <ms>
```

- `<pos>` and `<ms>` are non-negative integers that fit `u64`.
- The CommandComplete response carries `affected = 1` when the
  target was reached, `affected = 0` when the optional timeout
  fired before the target. Clients distinguish via the count.
- Implementation polls `lag_state.follower_applied_pos` at
  5 ms cadence under `Acquire` ordering. Server-layer command
  — engine refuses it because `lag_state` lives on
  `ServerState`, not the engine.
- On a server with no follower configured, the apply position
  stays at 0 and `WAIT FOR WAL POSITION 0` returns immediately.
  Larger targets block until the optional timeout fires.

### WAL compression (v6.6 series)

v6.6 closes the fourteenth-gap cluster from the PG-19 audit: WAL
footprint reduction. Hand-rolled LZSS (no_std, no deps) lands
compression at both the WAL record layer and the cold-tier
segment file layer with full backwards-compat reads. Frozen
surfaces below.

#### Frozen surfaces (added v6.6.x)

- `spg_crypto::lzss::compress(input: &[u8]) -> Vec<u8>` /
  `spg_crypto::lzss::decompress(input: &[u8]) -> Result<Vec<u8>, LzssError>`.
  Storer-Szymanski 1982 algorithm: 4 KiB window, 18-byte max
  match, output stream `[u32 LE original_len][flag byte][8 tokens]*`.

- WAL v3 type tag `WAL_V3_TYPE_COMPRESSED_SQL = 0x03`. Payload
  layout `[u8 algo][compressed bytes]`. Algo 0x01 = LZSS.
  Decoder dispatch on type byte; v3 type=0x01 (uncompressed)
  still works.

- Cold-tier segment file v2 magic `SPGSEG\x02\x00`. Envelope
  layout `[8-byte magic][u8 algo: 0=none, 1=lzss][u32 LE
  inner_uncompressed_len][inner bytes]`. v1-magic
  `SPGSEG\x01\x00` files still load via `OwnedSegment::from_bytes`.

- `spg_storage::wrap_v2_envelope(v1: Vec<u8>, compress: bool) -> Vec<u8>` —
  public wrapper. Returns v1 unchanged when compress=false OR
  when LZSS output isn't strictly smaller.

- `Metrics` AtomicU64 counters:
  `wal_bytes_uncompressed_in`, `wal_bytes_compressed_out`,
  `segment_bytes_uncompressed_in`, `segment_bytes_compressed_out`.

- `/metrics` series (Prometheus counters):
  `spg_wal_bytes_uncompressed_total`,
  `spg_wal_bytes_compressed_total`,
  `spg_segment_bytes_uncompressed_total`,
  `spg_segment_bytes_compressed_total`.

- Env vars (operator-tunable):
  - `SPG_WAL_COMPRESSION` — `lzss` (default) or `none`
  - `SPG_SEGMENT_COMPRESSION` — `lzss` (default) or `none`
  - `SPG_COMPRESSION_MIN_BYTES` — threshold floor (default 256).
    SQL payloads smaller than this skip LZSS.

#### Out of scope for v6 (carved out — not deferred)

- **LZ4 / zstd / brotli**. The algo byte in both the WAL v3
  payload and the segment v2 envelope reserves the namespace
  for future algorithms (0x02 LZ4, 0x03 zstd) without another
  format bump. v6.6 ships LZSS only — simplest published
  dictionary scheme that gives ≥ 2× ratios on text.
- **WAL record dedup** (per-WAL-file SQL string dictionary
  back-referencing).
- **Streaming compression across record boundaries**. Per-record
  framing means torn writes only damage their own record;
  v6.6.4 chaos test locks this invariant.
- **Dictionary pretraining** (PG's `wal_compression_dict`).
- **Replication-wire compression**. MAGIC_SUB frames stay
  uncompressed; v6.6 is on-disk only.
- **Per-column type-specific compression** (PG TOAST style).
- **PG-wire write path → WAL append**. PG-wire 'Q' simple-query
  writes don't currently persist to WAL — only the SPG native
  wire commit_queue path does. Pre-v6.6 gap unaffected by the
  v6.6 compression work.

### Observability v2 (v6.5 series)

v6.5 closes the thirteenth-gap cluster from the PG-19 audit:
SQL-queryable runtime state. Nine new virtual tables + env knobs
+ engine builder APIs land here, all frozen from the named
sub-version.

#### Frozen surfaces (added v6.5.x)

- Virtual tables (read-only, dispatch by name match in
  `exec_select_cancel`; columns + order frozen):
  - `spg_stat_replication(name, conn_str, publications,
                          last_received_pos, enabled)`  (v6.5.0)
  - `spg_stat_segment(segment_id, num_rows, num_pages,
                      total_bytes)`                     (v6.5.0)
  - `spg_stat_query(sql, exec_count, total_us, mean_us,
                    max_us, last_seen_us)`              (v6.5.1)
  - `spg_stat_activity(pid, user, started_at_us,
                       current_sql, wait_event,
                       elapsed_us, in_transaction)`     (v6.5.2)
  - `spg_audit_chain(seq, ts_ms, prev_hash, entry_hash,
                     sql)`                              (v6.5.3)
  - `spg_audit_verify(verified_count, broken_at_seq)`   (v6.5.3)
  - `spg_table_ddl(table_name, ddl)`                    (v6.5.4)
  - `spg_role_ddl(role_name, ddl)`                      (v6.5.4)
  - `spg_database_ddl(ddl)`                             (v6.5.4)

- Engine API additions:
  - `pub struct ActivityRow { pid, user, started_at_us,
                              current_sql, wait_event,
                              elapsed_us, in_transaction }`
  - `pub type ActivityProvider = fn() -> Vec<ActivityRow>`
  - `Engine::with_activity_provider(f)` builder
  - `pub struct AuditRow { seq, ts_ms, prev_hash_hex,
                           entry_hash_hex, sql }`
  - `pub type AuditChainProvider = fn() -> Vec<AuditRow>`
  - `pub type AuditVerifier = fn() -> (i64, i64)`
  - `Engine::with_audit_providers(chain, verify)` builder
  - `pub type SlowQueryLogger = fn(&str, u64)`
  - `Engine::with_slow_query_log(threshold_us, logger)` builder
  - `Engine::query_stats()` / `query_stats_mut()` accessors
  - `Engine::set_plan_cache_max(n)` mutable knob
  - `PlanCache::set_max_entries(n)` / `max_entries()`

- Pgwire ConnState registry surface (server-internal but stable):
  `ConnState { pid: u32, user: String, started_at_us: i64,
               current_sql: RwLock<String>,
               wait_event: AtomicU8 (0=idle, 1=write_lock,
               2=fsync, 3=group_commit),
               last_query_start_us: AtomicI64,
               in_transaction: AtomicBool }`

- Env vars (operator-tunable):
  - `SPG_SLOW_QUERY_THRESHOLD_MS` — slow-query log floor (ms).
    Default 100. Crossings emit a structured log line via
    `observability::log_event("warn", "slow_query", …)`.
  - `SPG_PLAN_CACHE_MAX` — runtime cap on the v6.3.0 plan cache.
    Default 256; values above the compile-time `PLAN_CACHE_MAX_
    ENTRIES` (256) are clamped down.

- Pgwire 'Q' path now appends to AuditLog on modified_catalog
  statements (was native-wire-only pre-v6.5.3) so
  `spg_audit_chain` reflects actual usage regardless of wire.

#### Out of scope for v6 (carved out — not deferred)

- **`spg_audit_verify(from_ts, to_ts)` parameterised form**. SPG's
  virtual-table dispatch is name-based only; parameterised virtual
  tables aren't a thing in the current engine. v6.5.3 ships the
  no-arg form. Operators who want range verification WHERE-filter
  `spg_audit_chain` on ts_ms.
- **Wait events: fsync + group_commit attribution**. The flusher
  and group-commit leader threads serve multiple connections; per-
  follower attribution needs a commit-task → ConnState bridge that
  v6.5.5 doesn't take.
- **Index DDL in spg_table_ddl / spg_database_ddl**. v6.5.4 emits
  CREATE TABLE + CREATE USER only. CREATE INDEX needs per-table
  indices walk + method/option synthesis.
- **spg_stat_segment.table_name**. Storage layer doesn't persist a
  segment → table mapping; segments are looked up by id off
  `RowLocator::Cold`. Adding the back-reference needs storage-side
  index expansion.
- **pg_stat_database / pg_stat_user_tables / per-table modify
  counters** (`n_tup_ins`, `n_tup_upd`, `n_dead_tup`). SPG's
  catalog doesn't keep persistent per-table modify counters
  beyond v6.2.1's auto-analyze tracker.
- **Per-query EXPLAIN cache**. `spg_stat_query` holds SQL +
  timings, NOT the cached EXPLAIN tree.
- **PG `pg_stat_statements` byte-for-byte column parity**.
  `spg_stat_query` is the equivalent surface but doesn't aim for
  exact column-name compatibility.
- **Streaming notify of stat changes**. `spg_stat_activity` is
  point-in-time; row-version notifications are out of v6.x.
- **WAL receiver / decoded WAL inspection** (`pg_get_wal_records`).
  SPG's WAL format is internal.

### SQL polish (v6.4 series)

v6.4 ships the SQL surface-polish set PG 19 brought plus the
JSON path operators every real app eventually wants. Closes the
twelfth-gap cluster from the PG-19 audit and the two SQL-surface
gaps v6.2.7 explicitly carved as "follow-up in v6.4". Frozen
surfaces below.

#### Frozen surfaces (added v6.4.x)

- `SelectStatement.order_by: Vec<OrderBy>` (was `Option<OrderBy>`).
  Empty Vec = no ORDER BY. Multi-key sort chains comparisons
  left-to-right with per-key DESC.
- `SelectStatement.group_by_all: bool`. When true, planner expands
  `group_by` to every non-aggregate SELECT-list item before
  executor dispatch. Parser sets it on `GROUP BY ALL`.
- `Expr::WindowFunction.null_treatment: NullTreatment` (Respect /
  Ignore). Applies to LAG / LEAD / FIRST_VALUE / LAST_VALUE;
  other window funcs ignore it. Default Respect (PG / ANSI).
- New BinOp variants: `JsonGetPath` (`#>`), `JsonGetPathText`
  (`#>>`), `JsonContains` (`@>`). Same precedence rung 7 as
  `->` / `->>`.
- SQL function table additions (eval dispatch):
  - `encode(text, format)` / `decode(text, format)` — base64,
    base64url, base32hex, hex
  - `error_on_null(v)` — passthrough or raise
- COPY FROM STDIN option tail: `WITH (SKIP N, ON_ERROR SET_NULL,
  FORMAT JSON)`. Default values preserve v4.17 behaviour.

#### Out of scope for v6 (carved out — not deferred)

- **INSERT ON CONFLICT** (any form: DO NOTHING / DO SELECT / DO
  UPDATE). v6.4 design originally scheduled `DO SELECT [FOR
  UPDATE]` for v6.4.4 on the false assumption that v5.x already
  shipped ON CONFLICT DO NOTHING / DO UPDATE. Audit during v6.4.4
  work found SPG has NO PRIMARY KEY / UNIQUE constraint
  enforcement anywhere (no PRIMARY KEY syntax, no UNIQUE in
  storage or engine). ON CONFLICT has nothing to detect. The
  prerequisites — PK / UNIQUE syntax + storage indexes +
  enforcement on every INSERT + WAL replay path — are
  foundational DML work, not SQL polish. Picked up as a dedicated
  v6.x effort (most likely v6.6 territory, once the WAL-format
  work in that series can carry enforcement-ready indexes).
- **`random(date, date)` / `random(ts, ts)`**. Designed for v6.4.3
  but needs a per-row RNG state EvalContext doesn't currently
  plumb. Adding RNG threading is a separate concern from the
  SQL-polish theme.
- **Full SQL/JSON path** (`jsonpath` opaque type, `json_path_exists`,
  `json_path_query`, `jsonb_path_query_array`, `@?`). v6.4.5
  ships the bare-key/path-array operators; the path-expression
  grammar is a separate surface.
- **MERGE statement** (`MERGE ... WHEN NOT MATCHED BY SOURCE`).
  Separate verb; INSERT ON CONFLICT covers the common upsert
  case once its prereqs ship.
- **COPY FORMAT BINARY**. PG's binary COPY format is a separate
  spec; text + CSV + JSON cover the practically-needed surface.
- **True per-cell ON_ERROR SET_NULL**. v6.4.7 ships row-level
  skip-on-error semantics. Per-column SET_NULL (replace the failed
  cell with NULL, keep the row) needs per-cell parse visibility
  inside `build_copy_insert`.
- **XML functions** (`xmlforest`, `xmlagg`, …). SPG has no XML
  type.

### PG-wire extended query finish (v6.3 series)

v6.3 closes the eleventh-gap cluster from the PG-19 audit — the
PG-wire extended-query protocol that JDBC / sqlx / pgx /
psycopg3 actually drive. The series finishes what v6.1.1
started: engine-level plan cache, pipelined response buffering,
Describe statement pre-Execute returning real RowDescription,
and binary parameter-format dispatch by OID. Frozen surfaces
below.

#### Frozen surfaces (added v6.3.x)

- `Engine::prepare_cached(sql) -> Result<Statement, ParseError>`
  — engine-level LRU plan cache (256-entry cap). Hit returns a
  cloned `Statement`; miss runs `prepare()` + inserts. Used by
  pgwire `handle_parse` so cross-session repeated Parse hits the
  cache.
- `Engine::describe_prepared(stmt) -> (Vec<u32>, Vec<ColumnSchema>)`
  — describe a prepared `Statement` without executing. Returns
  (param OIDs vec of zeros, output column shape vec). Empty
  column shape = "NoData" semantics for pgwire wire reply.
- `Statistics::version()` / `Statistics::bump_version()` — monotonic
  counter bumped by every successful ANALYZE. Plan cache uses it
  to evict stale entries.
- Pgwire Describe statement reply shape:
  `ParameterDescription { count: u16, oids: [u32; count] }`
  + `RowDescription | NoData`. Simple SELECT with a single
  FROM clause emits RowDescription; JOIN / non-SELECT degrade
  to NoData (drivers tolerate). The Describe portal reply is
  `RowDescription | NoData` (no ParameterDescription — that's
  on the underlying statement).
- Pgwire Bind binary-format dispatch by parameter OID:
  - 16 BOOL, 17 BYTEA, 20 BIGINT, 21 INT2, 23 INT, 25 TEXT
  - 700 REAL, 701 DOUBLE, 1043 VARCHAR
  - 1082 DATE, 1114 TIMESTAMP, 1184 TIMESTAMPTZ
  - 1700 NUMERIC (variable-precision packed-digit)
  Per-param format codes supported (0 codes → all text; 1 code
  → applies to all; N codes → per-param). Binary RESULT format
  not implemented in v6.3 — drivers requesting binary results
  get text (matches the v6.1.1 behaviour).

#### Out of scope for v6 (carved out — not deferred)

- **Server-side cursor / partial Execute**: PG `Execute(E, row_max)`
  returns a result-set prefix; subsequent Execute on the same
  portal resumes. SPG returns the whole set on the first Execute.
  Not deferred — out of v6.x.
- **COPY in extended-query mode**: PG allows `COPY` through Parse
  + Bind + Execute. SPG keeps COPY simple-query-only. Out of v6.x.
- **Binary result format**: Bind result-format=1 returning binary
  rows. v6.3.4 covers binary INPUT only; output stays text. The
  driver-side cost of text→typed is negligible vs the v6.3.0
  cache + v6.3.2 pipelining wins. Out of v6.3.
- **JOIN-shape Describe**: multi-table SELECT FROM falls through to
  NoData rather than synthesizing the joined RowDescription. Out
  of v6.3.
- **Per-statement-cache TTL**: PG inherits no time-based plan
  TTL; invalidation is schema / stats only. Same for SPG. Out of
  v6.x.
- **Docker-compose multi-language client compat suite** (rust-
  postgres / sqlx / pgx / psycopg3 via pinned-digest containers).
  v6.3.5 ships hand-rolled real-client-shaped workloads instead
  because adding 4 language toolchains conflicts with the
  workspace 0-deps rule. Out of v6.3 — picked up as a v6.x lane
  if a user reports client-specific incompatibility.

### Optimizer foundation (v6.2 series)

v6.2 closes the third gap from the PG-19 audit — statistics-
driven cost-based optimization. The series ships a virtual
catalog table (`spg_statistic`), an `ANALYZE` SQL surface, the
cost-based JOIN reorder pass, per-operator EXPLAIN ANALYZE row +
elapsed annotations, and a Memoize cache for correlated
subqueries. Every surface below is frozen from the named
sub-version.

#### Out of scope for v6 (carved out — not deferred)

- **Per-operator inner-node `elapsed=…us`** (per-Filter,
  per-Join, per-GroupBy, per-OrderBy, per-Limit individually
  timed): EXPLAIN ANALYZE marks inner nodes with `(rows=—)`
  and the trailing `Total: elapsed=…` line carries whole-
  query wall-clock. Per-node inner-node elapsed requires
  inline executor instrumentation that v6.2 intentionally
  doesn't add — a v6.x or v7.x revisit, NOT a v6.2 deferral.
- **Per-table `cold_rows` precise count**: v6.2.7 ships a
  global `cold_segments=[id0,…]` list on scan annotations.
  Per-table breakdown needs index-side cold-locator walking
  — same v6.x revisit posture as above.
- **Multi-column statistics (`pg_statistic_ext`-style)**:
  single-column histograms only.
- **Most Common Values (MCV) list**: histogram-only.
- **Bitmap scans**: executor unchanged in v6.2.
- **CBO for vector kNN**: rule-based v5.5 dispatch retained.
- **Parallel executor nodes**: single-thread executor (A3).

### ANALYZE + spg_statistic (v6.2.0)

v6.2.0 introduces the first DDL on the optimizer-foundation
path. The grammar + virtual-table column shape are frozen;
v6.2.x can append (not reorder or rename) columns to
`spg_statistic` as later sub-versions add stats.

```text
ANALYZE                  -- analyse every user table
ANALYZE <table>          -- analyse one table
SELECT * FROM spg_statistic   -- read per-column stats
```

`spg_statistic` columns (frozen from v6.2.0):

```text
table_name        TEXT NOT NULL
column_name       TEXT NOT NULL
null_frac         FLOAT NOT NULL    -- 0.0 .. 1.0
n_distinct        BIGINT NOT NULL   -- approximate
histogram_bounds  TEXT NOT NULL     -- "[v0, v1, ...]"
```

Rows sorted alphabetically by `(table_name, column_name)`.
Read-only — INSERT / UPDATE / DELETE on `spg_statistic` errors.
The only way to populate is `ANALYZE`.

Histogram is 100-bucket equi-depth → 101 sorted bounds per
column. Values use the column's natural ordering (INT decimal,
TEXT lexicographic, DATE/TIMESTAMP ISO, etc.); the rendered
string form is for human consumption, not a re-parseable
exchange format. Vector / SQ8 / HalfVector columns are skipped
by ANALYZE (their stats live in HNSW graph state).

#### Snapshot envelope v5 (v6.2.0)

The snapshot envelope grows a statistics trailer. v1/v2/v3/v4
envelopes still load with empty statistics; writers from v6.2.0
onwards always emit v5. v6.2.0 readers parse all five versions;
pre-v6.2.0 binaries fail loudly on a v5 envelope (same upgrade
fence as v6.1.2 / v6.1.4).

```text
[8 bytes "SPGENV01"]
[u8 version = 5]
[u32 catalog_len][catalog bytes]
[u32 users_len][users bytes]
[u32 pubs_len][publications bytes]
[u32 subs_len][subscriptions bytes]
[u32 stats_len][statistics bytes]    ← new in v5
[u32 crc32]                          ← covers everything above
```

Statistics-blob format (v6.2.0):
```text
[u16 num_columns]
for each column:
  [u16 table_len][table bytes]
  [u16 column_len][column bytes]
  [f32 null_frac]
  [u64 n_distinct]
  [u16 num_bounds]
  for each bound: [u16 b_len][b bytes]
[u16 num_modified_entries]
for each: [u16 t_len][t bytes][u64 modified_count]
```

The `modified_entries` trailer feeds v6.2.1 auto-analyze's 10 %
threshold trigger.

### Subscription DDL (v6.1.4)

v6.1.4 adds the receive side of logical replication. The grammar
below is frozen; future v6.1.x sub-versions may add ALTER /
WITH-options without breaking these forms.

```text
CREATE SUBSCRIPTION <name>
  CONNECTION '<keyword=value-string>'
  PUBLICATION <pub_name> [, <pub_name> ...]
DROP SUBSCRIPTION <name>
SHOW SUBSCRIPTIONS
```

- `<conn>` is a PG-style keyword=value string. v6.1.4 consumes
  `host=…` and `port=…`; other keys are accepted but ignored
  (forward-compat for v6.1.x options like `user`, `password`,
  `application_name`).
- `<pub_name>` is one or more publication identifiers on the
  remote side. v6.1.4 records the list; v6.1.5 enforces it at
  the publisher.
- Duplicate `CREATE` errors; `DROP` of an absent subscription
  is a silent no-op.
- `SHOW SUBSCRIPTIONS` returns:
  `(name TEXT NOT NULL, conn_str TEXT NOT NULL, publications TEXT
  NOT NULL, enabled BOOL NOT NULL, last_received_pos BIGINT NOT
  NULL)` ordered by name. `publications` is comma-joined.
- Subscriptions are inside-TX-safe — they commit/roll back with
  the surrounding transaction.
- `CONNECTION` and `SUBSCRIPTION` are reserved keywords from
  v6.1.4 (v6.1.2 reserved `SUBSCRIPTION` ahead of time).

### Replication MAGIC_SUB protocol (v6.1.4 — v6.1.5)

Subscriber-side connect handshake:
```text
→  [b"SPGSUB\x01\x00"]              ← 8 bytes magic
   [u64 LE start_offset]             ← 0 = "tail from current end"
   [u16 LE num_publications]         ← v6.1.5 (0 = legacy v6.1.4 fan-out-all)
   for each publication:
     [u16 LE name_len][name bytes]
   [u64 LE subscriber_cluster_id]    ← v6.1.6 cycle-detection input
←  [u64 LE effective_start]          ← master's WAL position the
                                       subscriber records as baseline
                                       last_received_pos
   [u64 LE master_cluster_id]        ← v6.1.6 cycle-detection check
```

v6.1.6 cycle detection: if `master_cluster_id == subscriber_cluster_id`
both sides bail. The subscriber returns `REPLICATION_LOOP` from the
handshake; the master closes the connection before forwarding records.
Only direct self-loops are caught — indirect cycles (A → B → A through
a chain) require WAL-record-level originator tagging, deferred to a
future v6.x.

#### cluster_id sidecar (v6.1.6)

```text
<wal_path>.cluster_id    (or <db_path>.cluster_id when no WAL configured)
  8 bytes LE u64
```

Generated on first boot via a SplitMix64-shaped mix of PID +
wall-clock nanoseconds. Stable across restarts. Servers with
neither wal_path nor db_path get a per-process in-memory id (fine
for tests, not for production replicas).

A v6.1.4 subscriber emits only the first 16 bytes (magic +
offset) and the master reads `num_publications = 0` from the
next two bytes the client sends in its first real frame… no.
v6.1.4 subscribers DID write only 16 bytes; v6.1.5 masters
expect the publication list immediately after, so a v6.1.5
master + v6.1.4 subscriber pairing is NOT compatible — the
master will block on the read. Operators upgrading should
upgrade subscribers before masters; the reverse breaks. Pre-
v6.1.4 followers using MAGIC_V1 / MAGIC_V2 are unaffected.

The frame stream from this point uses the v2 framing:
`[u8 type][u32 len][payload]`. Subscribers never receive an
initial snapshot — target tables must exist on the subscriber
side before the worker starts.

#### Frame types on a MAGIC_SUB stream (v6.1.5)

```text
0x00 = FRAME_TYPE_WAL    — raw WAL bytes for one or more records
                           the master is forwarding. Subscriber
                           parses records and applies SQL.
0x01 = FRAME_TYPE_STATUS — 16-byte advisory `[primary_pos|wall_us]`.
                           Subscriber ignores in v6.1.4/v6.1.5.
0x02 = FRAME_TYPE_SKIP   — 8-byte `[skipped_bytes u64 LE]` payload.
                           Master emits one per contiguous run of
                           records that the publication filter
                           rejected (or DDL/session-control SQL
                           that v6.1.x never propagates).
                           Subscriber advances applied_offset by
                           skipped_bytes without applying. Lets
                           reconnect from the same last_received_pos
                           skip past already-filtered records.
```

Logical-replication policy (v6.1.5):
- DML (`INSERT INTO …`, `UPDATE …`, `DELETE FROM …`) is filtered
  by the publication's scope.
- DDL (`CREATE TABLE`, `DROP TABLE`, `ALTER INDEX`, `TRUNCATE`,
  `CREATE PUBLICATION`, `CREATE SUBSCRIPTION`, `CREATE USER`,
  …), session control (`BEGIN`, `COMMIT`, `ROLLBACK`,
  `SAVEPOINT`, `SET`), and catalog mutations are never
  propagated — subscriber-side schema drift is the operator's
  problem (the internal design notes design point 3). PG-compatible — PG
  logical decoders also drop DDL.

### Snapshot envelope v4 (v6.1.4)

The snapshot envelope grows a subscriptions trailer. v1/v2/v3
envelopes still load with empty subscriptions; writers from
v6.1.4 onwards always emit v4. v6.1.4 readers parse v1, v2, v3
and v4; pre-v6.1.4 binaries fail loudly on a v4 envelope (same
upgrade fence as v6.1.2's v3 introduction).

```text
[8 bytes "SPGENV01"]
[u8 version = 4]
[u32 catalog_len][catalog bytes]
[u32 users_len][users bytes]
[u32 pubs_len][publications bytes]
[u32 subs_len][subscriptions bytes]    ← new in v4
[u32 crc32]                            ← covers everything above
```

Subscriptions-blob format (v6.1.4):
```text
[u16 num_subscriptions]
for each:
  [u16 name_len][name bytes]
  [u32 conn_str_len][conn_str bytes]
  [u16 num_publications]
  for each: [u16 p_len][p bytes]
  [u8 enabled]
  [u64 last_received_pos]
```

Sorted alphabetically by subscription name for byte-stable
snapshots regardless of insertion order.

### Publication DDL (v6.1.2 — v6.1.3)

v6.1.2 introduced the first DDL surface on the logical-replication
path; v6.1.3 added the FOR-list variants and `SHOW PUBLICATIONS`.
All forms below are frozen.

```text
CREATE PUBLICATION <name> [FOR ALL TABLES                              -- v6.1.2
                          | FOR ALL TABLES EXCEPT t1, t2, ...          -- v6.1.3
                          | FOR TABLE  t1, t2, ...                     -- v6.1.3
                          | FOR TABLES t1, t2, ...]                    -- v6.1.3 (plural alias)
DROP PUBLICATION <name>                                                -- v6.1.2
SHOW PUBLICATIONS                                                      -- v6.1.3
```

- `<name>` is an unquoted or `"…"`-quoted identifier.
- Omitting the `FOR` clause is equivalent to `FOR ALL TABLES`.
- `FOR TABLE` (singular) and `FOR TABLES` (plural) parse
  identically — PG 19 accepts both forms.
- Empty FOR-list (`FOR TABLE` followed by nothing) is a parse
  error.
- Duplicate `CREATE` errors; `DROP` on a missing publication is
  a silent no-op (PG-compatible). Both are rejected inside an
  active transaction.
- `PUBLICATION` and `SUBSCRIPTION` (the latter reserved for
  v6.1.4) are reserved keywords from v6.1.2; existing schemas
  that used `publication` as an identifier name must quote it
  as `"publication"`.
- `SHOW PUBLICATIONS` returns three columns:
  `(name TEXT NOT NULL, scope TEXT NOT NULL, table_count INT NULL)`,
  rows ordered alphabetically by `name`. The `scope` column is
  the human-readable form (`FOR ALL TABLES` / `FOR TABLE …` /
  `FOR ALL TABLES EXCEPT …`). `table_count` is NULL when the
  scope is `AllTables` and the list length otherwise.

#### Snapshot envelope v3 (v6.1.2)

The snapshot envelope grows a publications trailer. v1/v2
envelopes still load with an empty publication table (forward-
compatible). Writers from v6.1.2 onwards always emit v3.

```text
[8 bytes "SPGENV01"]
[u8 version = 3]
[u32 catalog_len][catalog bytes]
[u32 users_len][users bytes]
[u32 pubs_len][publications bytes]   ← new in v3
[u32 crc32]                          ← covers everything above
```

Publications-blob format (v6.1.2):
```text
[u16 num_publications]
for each:
  [u16 name_len][name bytes]
  [u8 scope_tag]
    0 → FOR ALL TABLES        (no trailer)
    1 → FOR TABLE <list>      (v6.1.3 emits; v6.1.2 parses, never writes)
    2 → FOR ALL TABLES EXCEPT <list>  (v6.1.3 emits; v6.1.2 parses, never writes)
  for scope_tag ∈ {1, 2}:
    [u16 num_tables]
    for each: [u16 t_len][t bytes]
```

Sorted alphabetically by publication name for byte-stable
snapshots regardless of insertion order.

---

### Cold tier evolution (v6.7 series)

The v6.7 series adds the parts of the cold-tier story that the
v5.x / v6.[0-6] foundation skipped: per-table accounting, BRIN
sidecar format, per-table budget override, segment compaction,
parallel freezer, segment forwarding, and a boot-time prefetch
worker pool. The following surfaces are frozen as of v6.7.8.

**Catalog snapshot envelope** — `FILE_VERSION = 11` (v6.7.2
bump). Layout extension is append-only:
- After the per-table `indices` section (unchanged) comes:
- `[u8 has_value][u64 LE value (if has_value)]` for the
  per-table `hot_tier_bytes: Option<u64>`.

v10 / v11 snapshots load unchanged via version-dispatch in
`Catalog::deserialize`. v6.7.0 / v6.7.1 binaries refuse v12
snapshots loudly at the version check.

**Catalog cold-segment surface** — `Catalog::cold_segments` is
now `Vec<Option<Arc<OwnedSegment>>>` (v6.7.3). `None` slots are
compaction tombstones; segment_id stays stable across compaction
so on-disk `RowLocator::Cold { segment_id }` always resolves.
New public APIs:
- `Catalog::load_segment_bytes_at(target_id, bytes)` — register
  a segment at a specific id, padding sparse slots with `None`.
- `Catalog::tombstone_segment(segment_id)` — flip an active
  slot to `None`.
- `Catalog::cold_segment_slot_count()` — slot count including
  tombstones (next allocatable id).
- `Catalog::cold_segment_count()` returns the *active* count
  (skips tombstones). v6.7 readers see sparse cold-segment
  layouts; v6.6- binaries refuse the format because the
  `Option`-wrapped vec is a v6.7.3 in-memory shape only — it
  doesn't enter the on-disk catalog snapshot, which carries
  only `RowLocator::Cold { segment_id }` (tombstoning is recoded
  from the manifest at boot).

**Segment v2 envelope BRIN sidecar** (v6.7.1) — the v2 magic
`SPGSEG\x02\x00` body now optionally carries a BRIN per-page
summary prefix:
```text
[u32 brin_section_len]
[BRIN entries: page_count × 21 bytes each]
[u32 page_index][u8 sentinel = 0x01]
[i64 LE min_key][i64 LE max_key]
[v1 segment body (compressed with the envelope's algo)]
```
Empty `brin_section_len` (= 0) means "no sidecar"; the parser
short-circuits and the body decodes byte-identically to a v2
envelope without sidecar. Older v2 segments load unchanged.

**`IndexKind` tag bytes** (v6.7.1):
- 0 = `BTree` (unchanged)
- 1 = `Nsw` (unchanged)
- 2 = `Brin { column_type }` — body `[u8 column_type]`. Single
  column only; the BRIN data itself lives in cold segments, not
  the catalog snapshot.

**v2 replication frame type `0x03 = SEGMENT_FILE_CHUNK`**
(v6.7.5):
```text
[u32 LE segment_id]
[u32 LE chunk_seq    ]   0-based
[u32 LE chunk_total  ]
[u32 LE chunk_bytes  ]   ≤ 16 MiB cap (hard)
[chunk_bytes bytes   ]
```
Default chunk size is 4 MiB. `chunk_bytes > 16 MiB` is a wire
format error; `chunk_seq >= chunk_total` is a wire format
error. (the internal design notes L2 originally allocated 0x02 — v6.1.5
had claimed 0x02 for `FRAME_TYPE_SKIP`; v6.7.5 ships at the
next free slot. The design reference is reconciled in the
frame-type doc comment.)

**SQL surface additions** — all PG-syntax-compatible:
- `CREATE INDEX <name> ON <table> USING BRIN (<col>)` —
  v6.7.1, format-layer only (planner page-skipping is
  carve-out below).
- `ALTER TABLE <name> SET hot_tier_bytes = <n>` — v6.7.2,
  per-table freezer budget override.
- `COMPACT COLD SEGMENTS` — v6.7.3, admin-only via server
  intercept (`can_manage_users()` gate, same as CHECKPOINT).
  Bare form only; `WHERE` predicate filtering is OOS for v6.7.
  Returns a result set with columns
  `(table_name, index_name, sources_merged, merged_segment_id,
   merged_rows, deleted_rows_pruned, bytes_reclaimed_estimate)`.

**Env vars** (operator-tunable, defaults sized for typical
deployments):
- `SPG_COMPACTION_TARGET_SEGMENT_BYTES` (default 4 MiB) —
  compaction merge threshold.
- `SPG_FREEZER_WORKERS` (default `max(1, num_cpus()-2)`, cap
  16) — parallel freezer prepare pool size.
- `SPG_PREFETCH_WORKERS` (default `max(1, num_cpus()-2)`, cap
  16) — boot-time prefetch pool size.
- `SPG_PERF_1B_ROW_BUDGET` (default 1_000_000) — `#[ignore]`
  1B-row stress test row count.

**Metrics** (`/metrics` HTTP endpoint, Prometheus text
exposition):
- `spg_cold_prefetch_hits_total` — counter, increments by 1 per
  successfully prefetched cold segment at boot.

### Index breadth (v6.8 series)

v6.8 broadens the CREATE INDEX surface to cover three new
PG-parity index shapes — INCLUDE columns, partial WHERE
predicates, and expression keys — plus an `EXPLAIN (SUGGEST)`
advisor. The format-layer surfaces below are frozen as of
v6.8.4.

**Parser surface** — `CREATE INDEX` grammar extension is
backward-compatible (all v6.7- statements parse unchanged):

```text
CREATE INDEX [IF NOT EXISTS] <name>
  ON <table>
  [USING <method>]
  ( <key> )                    -- bare column ident OR any Pratt expr
  [ INCLUDE ( <col>, … ) ]
  [ WHERE <expr> ]
```

`<key>` legacy fast path: a bare ident immediately followed by
`)` parses as a column reference. Anything else falls through
to the Pratt expression parser. Expression keys without a
column reference (`(1 + 1)`) are a parse error.

`INCLUDE` / `WHERE` / expression keys on HNSW or BRIN
indexes are rejected at execution time.

**`EXPLAIN (SUGGEST) <select>`** — index advisor opt-in. The
`(…)` option list currently only recognises `SUGGEST`; unknown
options error loudly. Mutually exclusive with `EXPLAIN ANALYZE`
at parse time (the bare `ANALYZE` keyword and the `(…)` option
list use different prefix-arms in the parser).

**Catalog snapshot envelope** — `FILE_VERSION = 12` (v6.8.0
bump). Per-index payload extension is append-only:

```text
[u16 num_included][num × u16 col_pos]   -- v6.8.0
[u8 has_pred][u16 LE len][bytes (if has_pred)]   -- v6.8.1
[u8 has_expr][u16 LE len][bytes (if has_expr)]   -- v6.8.2
```

v11 catalog snapshots load with `included_columns = Vec::new()`,
`partial_predicate = None`, `expression = None` (deserialise
loop gated on `version >= 12`); v12 snapshots written by a
pre-v6.8.0 binary fail loudly at the version check. Empty Vec
/ `None` serialise as bare `0` bytes.

**`Index` struct fields** (catalog snapshot frozen):
- `Index.included_columns: Vec<usize>` — column positions of
  `INCLUDE` columns. Empty Vec = no INCLUDE clause.
- `Index.partial_predicate: Option<String>` — canonical Display
  of the partial-index `WHERE` expression. `None` =
  unconditional index.
- `Index.expression: Option<String>` — canonical Display of an
  expression-key index. `None` = bare column-reference index.

### SPG-unique abilities (v6.10 series)

The v6.10 series lifts the SPG-specific roadmap items from
§2 ("Inspired-better dedicated") into shippable surfaces.
Each ship freezes one operator-facing surface; runtime
complexity that didn't make the v6.10 budget is parked in
the "Out of v6.10" carve-out list below.

**SQL surface additions:**
- `SELECT … FROM <tbl> AS OF SEGMENT '<id>'` — v6.10.2
  cold-tier time-travel scan. `<id>` is a string literal
  (operator ergonomics) or an integer; engine resolves it to
  a `cold_segment_id`. Scope is intentionally narrow
  (projection + WHERE + LIMIT); other shapes return an
  `EngineError::Unsupported` pointing at the carve-out.

**Env vars** (operator-tunable):
- `SPG_PUBSUB_TARGET` (default unset = pubsub off) — v6.10.0.
  Currently accepts `log` only.
- `SPG_PUBSUB_SUBJECT` (default `spg.wal.sql`) — v6.10.0.
- `SPG_MAX_QUERY_NS` (default unset) — v6.10.1. Combines
  with `SPG_QUERY_TIMEOUT_MS` via `min`; tighter wins.
- `SPG_WAL_TEE_PATH` (default unset = tee off) — v6.10.6.

**CLI surfaces:**
- `spg-server --replay-only` — v6.10.4.
- `spg wal-lint <wal_path> --against-schema <db_path>` —
  v6.10.5. Output is `OK <count>` (stdout) or
  `FAIL <byte_offset>: <error>` (stderr).
- `spg revert --wal <p> --to-seq <N> --out <db>` — v6.10.7.
  Output is `OK applied=<n> → <out>` (stdout).

**Crates:**
- `spg-embedded` — v6.10.3. Public surface:
  - `Database::open_in_memory() -> Database`
  - `Database::restore(&[u8]) -> Result<Database, EngineError>`
  - `Database::snapshot() -> Vec<u8>`
  - `Database::execute(&str) -> Result<QueryResult, EngineError>`
  - `Database::query(&str) -> Result<Vec<Vec<Value>>, EngineError>`
  - `Database::engine() / engine_mut()` escape hatches.
  - `trait FromSpgRow { fn from_spg_row(&[Value]) -> Result<Self, EngineError>; }`

**Wire frame additions:**
- None. v6.10 reused existing frame types throughout (pubsub
  is a side-channel; WAL tee is a file-level mirror).

### Out of v6.10 (carved out — explicit STABILITY entries)

The v6.10 sub-versions ship the operator surface + the
in-process or format-layer plumbing. The following are
explicitly parked for future v6.x or v7.x revisits:

1. **`SPG_PUBSUB_TARGET=tcp://host:port` / `nats://…`.**
   v6.10.0 ships only the `log` target. Real-broker TCP
   needs the INFO/CONNECT handshake, TLS, reconnect, and a
   bounded outbound queue — sized work, parked.
2. **`AS OF TIMESTAMP <ts>`.** v6.10.2 ships
   `AS OF SEGMENT '<id>'` only. Timestamp lookup needs the
   freezer to stamp each segment with a wall-clock at
   creation time, a v6.7-era change carve-out.
3. **`AS OF SEGMENT` with joins / aggregates / ORDER BY.**
   Surfaces `EngineError::Unsupported` today. Workaround:
   `INSERT INTO restored SELECT … FROM <tbl> AS OF SEGMENT`,
   then query `restored` with the full SQL surface.
4. **Typed query API + `#[derive(SpgRow)]`.** The
   `spg-embedded::FromSpgRow` trait sketch freezes the
   signature; the proc-macro crate
   (`spg-embedded-macros`) lands as a separate ship.
5. **`spg-embedded::Database::open_path(p)`.** v6.10.3
   ships in-memory + byte-slice round-trip; on-disk
   persistence stays `spg-server`'s job.
6. **`spg revert --to-audit-entry <hash>`.** The CLI
   recognises the flag and emits a carve-out hint. Resolving
   `<hash>` to a sequence number needs the audit-chain
   provider hook from v6.5.3 to be live in the CLI's
   load-from-snapshot path.

### Concurrency model (v6.9 series)

The v6.9 series is **measurement + decision** — it adds no
SQL surface, no wire frame, and no catalog snapshot bump. The
single-writer / RwLock-reader concurrency model documented in
the engine docs is frozen as-is for v6.x.

Bench harness: `crates/spg-server/tests/perf_gate/concurrency.rs`
(`#[ignore]`). Runs 8 / 16 / 32 concurrent native-wire clients
under SELECT-only and mixed (75% SELECT / 25% INSERT) workloads;
emits aggregate ops/sec + p99 latency per client count + per
workload shape.

### Out of v6.9 (carved out — explicit STABILITY entries)

The v6.9.0 bench measured throughput at the operating point
SPG ships for; the v6.9.1 ship-rollup recorded the decision to
defer the bigger concurrency work to v7.x. The following items
remain unimplemented and carry future-revisit hooks:

1. **Choice A — parallel prepare under `engine.read()` +
   install-phase OCC retry.** The bench measurement
   (1.17× SELECT-only scale 8 → 32 clients) shows the read-lock
   ceiling exists, but the 5–7 d implementation cost buys
   ceiling-lift rather than relief from a real bottleneck. v7.x
   revisits when a concrete workload pushes past the v6.9.0
   numbers.
2. **Per-statement read pinning.** Finer-grained
   per-row / per-segment read pins would let long scans release
   the engine read lock; invasive change, no v6.x driver.
3. **Lock-free / wait-free indices.** Out of v6.x scope; the
   PersistentBTreeMap is structurally-shared but takes the
   engine write lock for mutations.

### Out of v6.8 (carved out — explicit STABILITY entries)

These v6.8 features ship at the format layer but their runtime
maintenance / planner optimisations are deferred to future v6.x
revisits. None block the v6.8 ship contract; all are documented
in the v6.8.4 CHANGELOG entry's "Known limitations" section.

1. **In-BTree-leaf INCLUDE payload + planner `index only
   scan`.** v6.8.0 persists the included column positions; the
   covered-query optimisation (avoid heap fetch by storing
   included values in the BTree leaf) is unimplemented. EXPLAIN
   doesn't emit `index only scan` annotations on covered
   queries.
2. **Partial-index planner selection.** v6.8.1 stores the
   predicate's canonical Display form; the planner doesn't yet
   check "query WHERE clause ⇒ partial predicate" to opt
   into a partial index. Maintenance is over-maintenance (every
   row enters partial indexes); correctness preserved.
3. **Expression-key seek shortcut.** v6.8.2 stores the
   expression's canonical Display form; the runtime maintenance
   pass that evaluates the expression on each row to derive the
   actual BTree key is not yet wired. Expression indexes
   effectively behave like the primary column's index.
4. **Index advisor cost-based ranking.** v6.8.3 emits SUGGEST
   lines in deterministic walk order; per-suggestion cost /
   cardinality estimates land once the optimiser ingests
   selectivity stats more directly.

### Out of v6.7 (carved out — explicit STABILITY entries)

These items remain unimplemented in v6.7 but are explicitly
scheduled for future v6.x revisits. None of them block the v6.7
ship contract; all of them are documented in the v6.7.8
CHANGELOG entry's "Known limitations" section.

1. **BRIN planner page-skipping during cold scan.** v6.7.1
   ships the format layer; the planner does NOT yet consult
   the BRIN summary to skip non-overlapping pages during scan.
2. **`spg_table_ddl` emission of `ALTER TABLE … SET
   hot_tier_bytes`.** v6.7.2 persists the per-table override
   on the catalog snapshot; `SELECT * FROM spg_table_ddl`
   doesn't yet round-trip it back to DDL text.
3. **`COMPACT COLD SEGMENTS WHERE …` predicate filtering.**
4. **Compaction orphan source-segment file GC.** Source
   segment files survive on disk until an offline cleanup tool
   removes them; subsequent CHECKPOINTs naturally exclude them
   from the manifest.
5. **Chunk-level resume on segment forwarding.** v6.7.5 ships
   segment-level resume via file existence; mid-segment
   disconnect re-transmits the whole segment.
6. **Bidirectional segment-forwarding handshake.** Follower
   doesn't yet declare known segment ids; master always ships
   every cold segment.
7. **Scan-triggered prefetch.** v6.7.6 wires the worker pool
   to the boot path. The L2 spec also calls for scan-time
   prefetch; parked until v6.x cold-tier streaming lands.
8. **BRIN summary RECOMPACT on DELETE.** v6.7 marks affected
   pages "loose" rather than recomputing min/max in-place.

### Native types (v7.10 / v7.11 series)

The v7.10 / v7.11 series fills the PG-type gap that left mailrs
and other PG-port apps stringifying values that should have been
first-class. Each addition is wire-frozen here.

**v7.10.4 — `BYTEA`.**
- `DataType::Bytes` + `Value::Bytes(Vec<u8>)`.
- Column type: `<col> BYTEA NOT NULL`.
- Wire OID 17. Text-mode encoding is PG hex (`\x` + lowercase
  hex pairs). Octal-escape and hex literals both decode on the
  INSERT path.
- Functions: `length(b)` / `octet_length(b)` (v7.10.4).

**v7.10.9 — `TEXT[]`.**
- `DataType::TextArray` + `Value::TextArray(Vec<Option<String>>)`.
- Wire OID 1009. Text-mode encoding is PG external array form
  `{a,b,NULL}` with double-quote + backslash escapes for
  elements containing whitespace, commas, quotes, or braces.
- Column type: `<col> TEXT[]`.
- `ARRAY[…]` literal constructs TextArray when any element is
  text or non-integer.
- Row codec: `[u32 count][per element: u8 null + (when non-null)
  u32 len + UTF-8 bytes]`.

**v7.11.0 — read fan-out (snapshot reads).**
- `Engine::clone_snapshot() -> CatalogSnapshot` is frozen Send +
  Sync; subsequent writes are NOT visible. Refresh on demand.
- `Engine::execute_readonly_on_snapshot(&snap, sql)` — DDL / DML
  return `EngineError::WriteRequired`.
- `spg-embedded-tokio::AsyncDatabase::read_handle().await ->
  AsyncReadHandle`; `AsyncReadHandle::{query, refresh}` use
  `spawn_blocking`. The read path never re-acquires the writer
  lock.

**v7.11.1 — array operators (TEXT[]).**
- `arr[i]` 1-based subscript; out-of-range / `i ≤ 0` → NULL.
- `x = ANY(arr)` / `x op ALL(arr)` with PG 3VL.
- `array_length(arr, dim)` — count for dim 1; NULL for other
  dims (single-dim only).
- `array_position(arr, val)` — 1-based first-match; NULL if
  absent; NULL elements never match.
- `unnest(arr)` at FROM position. Uncorrelated only; composes
  with WHERE / ORDER BY / LIMIT.
- `||` array concat (three overloads: `arr || arr`,
  `arr || elem`, `elem || arr`).

**v7.11.2 — `INT[]` / `BIGINT[]` + BYTEA scalar ops.**
- `DataType::IntArray` (tag 20) + `Value::IntArray(Vec<Option<i32>>)`.
- `DataType::BigIntArray` (tag 21) + `Value::BigIntArray(Vec<Option<i64>>)`.
- Wire OIDs 1007 (`_int4`) / 1016 (`_int8`); same text-mode
  external form as `TEXT[]`.
- Row codec: `[u16 count][per element: u8 null + (when non-null)
  i32/i64 LE]`.
- Casts: `::INT[]` / `::BIGINT[]` decode external form,
  IntArray↔BigIntArray cross-cast (widening + narrowing).
- `ARRAY[…]` literal type inference: all-int → IntArray; any
  i64 → BigIntArray; any text or non-int → TextArray.
- Full op parity with `TEXT[]`: subscript returns `Int` /
  `BigInt`; ANY / ALL; `array_length` / `array_position`;
  `unnest` emits typed `Int` / `BigInt` rows (column inherits
  element type instead of forcing `Text`); `||` overloads
  including `IntArray || BigIntArray` widening to `BigIntArray`.
- BYTEA scalar ops:
  - `bytea || bytea` byte concat.
  - `substring(text|bytea, start [, length])` — PG 1-based;
    out-of-range → empty result.
  - `position(needle, haystack)` — TEXT (char index) or BYTEA
    (byte index); 1-based; 0 if absent; empty needle → 1.
- Catalog `FILE_VERSION` bumps 18 → 19. v18 catalogs continue
  to load (TextArray / Bytes paths are unchanged).

**New crate (v7.10.0): `spg-embedded-tokio`.**
- Wraps `Database` in `tokio::sync::Mutex` + dispatches engine
  calls through `tokio::task::spawn_blocking`.
- Frozen surface: `AsyncDatabase::open_in_memory()`,
  `::execute(sql).await`, `::query(sql).await`,
  `::read_handle().await` (v7.11.0).

#### Out of v7.10 / v7.11 (carved out — explicit STABILITY entries)

1. **`SMALLINT[]` / `NUMERIC[]` / `BOOLEAN[]` / `FLOAT[]`.**
   Only `INT[]` / `BIGINT[]` / `TEXT[]` / `BYTEA` are first-class.
   Workaround: widen at the application layer (smallint → int,
   bool → text, numeric → text).
2. **Multi-dimensional arrays.** Single-dim only;
   `array_length(_, dim>1)` returns NULL.
3. **Binary array wire format.** Drivers requesting binary
   format get text mode (matches the legacy v6.1.1 behaviour).
4. **`::BYTEA` inline cast.** v7.10.4 BYTEA goes through column
   typing on INSERT; the inline `'\x..'::BYTEA` cast surface is
   a v7.12 parser change.
5. **PG-spec syntax for `position(needle IN haystack)` /
   `substring(x FROM y FOR z)`.** Function-call form covers the
   semantics; the special syntax requires parser changes.
6. **LATERAL / JOIN-position `unnest`.** Uncorrelated FROM-position
   only.
7. **Array subscript with slice (`arr[1:3]`).** Single-index
   subscript only.

---

## Not frozen (free to change in any release)

- Internal storage types (`Catalog`, `Table`, `Row`, `Value`)
  — only the on-disk serialization is stable.
- Bench harness output format — `xtests/v4_*.md` reports are
  not API.
- Stderr log message wording — `SPG_LOG_FORMAT=json` gives you
  a structured event stream; the field set is the contract,
  not the text rendering.
- `cargo-target-dir`, `BUDGETS.md`, perf gate budgets — these
  are internal development infrastructure, not user-facing.
- Exact wording in internal readiness matrix / operational runbook / etc. — the docs
  evolve; the system behaviors they describe stay stable per
  the rules above.

---

## How to add a feature without breaking the contract

1. **New SQL form**: extend the parser, add an `e2e_*` test.
   Free.
2. **New wire opcode**: pick the next free byte, document it in
   the table above, add to `STABILITY.md` and the `prod_ready`
   row 8.2 expected table. Old clients don't send it — fine.
3. **New env var**: document in deployment notes, default-off,
   one-line entry in next CHANGELOG section.
4. **New backup-bundle field**: bump the `kind` byte or add a
   new section *after* every existing field so old parsers stop
   reading at the same place they always did.
5. **New snapshot field**: add to the `FILE_VERSION` integer
   inside `Catalog::serialize` (currently 8 — bump to 9), keep
   the v8 read path. Add a fixture under
   `xtests/compat-fixtures/<this-version>/` so the cross-version
   test gates future regressions.

## How to remove something (you can't, in v4.x)

Anything frozen above can only be removed in a MAJOR bump. If
you need to remove a feature mid-v4.x, deprecate first: emit a
stderr warning when used, document in CHANGELOG, propose the
removal for the v5 milestone.

---

## What this document is NOT

- Not a feature list. See internal readiness matrix.
- Not a tutorial. See deployment notes + restore drill.
- Not a roadmap. See operational runbook / CHANGELOG.md.
