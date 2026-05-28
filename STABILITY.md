# SPG stability contract

What's frozen, what's not, how to read a version bump.

---

## SemVer for SPG

- **MAJOR** bump → breaking change. Old clients may not connect;
  old snapshot/WAL files may need migration. None planned in
  v4.x.
- **MINOR** bump → backwards-compatible additions. New SQL
  features, new wire opcodes, new env vars. Old behaviors
  unchanged.
- **PATCH** bump → bug fix or doc-only. Wire / SQL / file formats
  unchanged.

Pre-v1.0 caveat: SPG itself is at v4.x, but treats this as its
"v1.0 contract" for the surfaces below. A v5.0 release would be
the first formal breaking-change window.

---

## Frozen surfaces

These are stable: client code, snapshot files, and operational
config written against the contract continue to work against any
future v4.x release. v4.31's CI gates this for the snapshot
format; the others are gated by per-row `prod_ready` tests.

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
    `RowLocator`s.
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
  - `Catalog::promote_cold_row(table, index, key)` /
    `Catalog::shadow_cold_row(table, index, key)` (v5.2.3) are
    the cold-tier write-path primitives. The engine routes PK-
    targeted UPDATE through `promote_cold_row` (cold → hot for
    the update) and PK-targeted DELETE through `shadow_cold_row`
    (Cold locator retired; row body becomes garbage until a
    future compaction pass reclaims it).

`SPG_PRELOAD_COLD_SEGMENT` is the v5.1 operator surface:
`table:index:path[;table:index:path …]`. spg-server parses it at
startup and runs the preload lazily on the first Op::Query
after each spec's `(table, index)` both exist. The env var is
stable within v5.x; v5.3+ may add an optional
`CatalogManifest`-driven autoload that supersedes it for
production deployments without removing the env-var path.

v5.2.2 freezer-driven segments are written to
`<db_path>.spg/segments/seg_<id>.spg` via `tmp+rename` after
each demotion; restoring them across a restart still requires
an operator-supplied `SPG_PRELOAD_COLD_SEGMENT` entry. v5.2.x
freezes are intentionally **not** WAL-durable — a crash mid-
freeze loses the not-yet-reloaded segment on restart (data is
recovered from the WAL replay back into the hot tier; the
chaos contract is "rolled back to pre-freeze state, no
corruption"). The chaos test
`tests/e2e_chaos_freeze.rs::chaos_kill_during_freeze_recovers_
clean_state` pins this invariant.

### Env-var contract

Every env var listed in DEPLOYMENT.md's table is stable. New
env vars are additions; existing env vars keep their semantics
and acceptable values within v4.x.

Special case: the `SPG_FAIL_*` chaos knobs are explicitly
**not** stable — they exist for testing. Operators must not
depend on them; future versions may rename or remove them
without bumping major.

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
- Exact wording in PROD_READY.md / RUNBOOK.md / etc. — the docs
  evolve; the system behaviors they describe stay stable per
  the rules above.

---

## How to add a feature without breaking the contract

1. **New SQL form**: extend the parser, add an `e2e_*` test.
   Free.
2. **New wire opcode**: pick the next free byte, document it in
   the table above, add to `STABILITY.md` and the `prod_ready`
   row 8.2 expected table. Old clients don't send it — fine.
3. **New env var**: document in DEPLOYMENT.md, default-off,
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

- Not a feature list. See PROD_READY.md.
- Not a tutorial. See DEPLOYMENT.md + RESTORE_DRILL.md.
- Not a roadmap. See RUNBOOK.md / CHANGELOG.md.
