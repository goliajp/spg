# SPG next-steps roadmap (post v4.37)

Linear plan to make SPG win the `xbench/competitor/src/bin/sweep.rs`
scale sweep across all N (10K → 100M) against PG 18 / MySQL 9 /
MariaDB 11. The v4.37 baseline (commit `399dc8d`, tag
`baseline-v4.37`) collapses at 1M-row INSERTs (9.4K r/s) because
`Engine::tx_catalog: Option<Catalog>` clones the whole catalog
inside every auto-commit BEGIN..COMMIT wrap; the structural fix is
to back `Catalog`, `Table::rows`, and `Table::indices` with
persistent (CoW, structural-sharing) data structures so a wrap
that touches one row is O(log N) instead of O(N).

Five checkpoints, dependency-ordered. Each is shippable in
isolation with its own perf gate. **Hard rule: 0 external deps,
pure Rust, no `unsafe`.** spg-storage today depends only on std +
spg-crypto; that property must hold post-v5.0.

Baseline tag for diffing: `baseline-v4.37` → `399dc8d`. Every
checkpoint sweep is compared against this point.

---

## What v4.33-v4.37 already delivered (reference)

| version | what | status |
|---------|------|--------|
| v4.33 | ops three-pack (graceful shutdown + slow query log + disk water-mark) | ✅ shipped |
| v4.34 | ENOSPC in-memory rollback (auto-commit savepoint wrap) | ✅ shipped — **introduced the per-write Catalog clone that v4.38-v4.41 are fixing** |
| v4.35 | per-table metrics with cardinality control | ✅ shipped |
| v4.36 | replication: netsplit chaos + lag metric | ✅ shipped |
| v4.37 | file format v9 + CRC32 on every storage envelope | ✅ shipped — current baseline |

The v4.34 auto-commit wrap was correct for ENOSPC rollback
semantics but exposes a structural-sharing gap: today each wrap
takes a *value-copy* `Catalog::clone()`. At 1M rows this is the
bottleneck the scale sweep hits. v4.38-v4.41 close it without
reverting v4.34's safety property.

---

## v4.38 — `PersistentVec<T>` (Bitmapped Vector Trie, standalone)

Self-contained data-structure work. No Engine changes. Lets the
algorithmic core land + get perf-gated before any Catalog
migration.

| # | item | est. | rows fixed |
|---|------|------|------------|
| 1 | **`crates/spg-storage/src/persistent.rs`** — `pub struct PersistentVec<T>` with Clojure-style BVT: 32-way branching trie + tail buffer; `root: Arc<Node<T>>`, `tail: Arc<Vec<T>>`, `len: usize`, `shift: u32`. Node = `Internal(Vec<Arc<Node<T>>>)` \| `Leaf(Vec<T>)`. All API: `new` / `push` / `get` / `iter` / `len` / `Clone` (Arc-clone, O(1)). `no_std`-compatible (`alloc::sync::Arc`); zero unsafe. | 1 d | none directly; foundation for v4.39 |
| 2 | **Fuzz oracle** — `crates/spg-storage/src/persistent_tests.rs` (cfg(test)). Random `push` / `get` operation sequences ≥ 100K steps mirrored against `Vec<u64>`; verify `clone()` then mutate doesn't disturb original. | 0.5 d | strengthens 1.10 |
| 3 | **Perf gate** — extend `crates/spg-storage/tests/perf_gate.rs` with `pv_push_1m_under_200ms` + `pv_get_random_under_100ns_avg` (release). | 0.5 d | new gate |

Dependencies: none.
Risk: low — pure algorithm work, fully sandboxed.
Test plan: doctest + fuzz + perf gate; fmt + clippy + workspace.
Why isolated: lets the BVT shake out 100K-step bugs before any
real catalog data touches it.

---

## v4.39 — `Catalog` / `Table::rows` / `Catalog::tables` backed by PV

Migration step. Pub API unchanged; wire format unchanged. After
this lands, `Catalog::clone()` is O(1) Arc bump, so the v4.34
auto-commit wrap is cheap at any scale.

| # | item | est. | rows fixed |
|---|------|------|------------|
| 1 | **Catalog / Table internals on PV** — `Table::rows: Vec<Row>` → `PersistentVec<Row>`; `Catalog::tables: Vec<Table>` → `PersistentVec<Table>`. All call sites (`.push`, `.iter`, `&self.rows[i]`, `row_count`) migrate. Public method signatures unchanged. | 1.5 d | none directly |
| 2 | **`Engine::tx_catalog` benefits transparently** — `exec_begin` / `exec_commit` / `exec_rollback` logic unchanged, but `Catalog::clone()` is now O(1). v4.34 wrap stays in place and stays cheap. | 0 d | seals 1.11 at scale |
| 3 | **SLO smoke** — `crates/spg-server/tests/slo_smoke.rs::slo_wal_insert_1m_rows_throughput`: insert 1M rows, assert throughput ≥ 50K r/s (≥ 5× of the 9.4K baseline). | 0.5 d | new gate |
| 4 | **Scale sweep rerun** — `cargo run --release -p spg-bench-competitor --bin sweep`. Expect spg-server INSERT @ 1M ≥ 50K r/s, @ 10M no bail. Add "after v4.39" section to PERFORMANCE.md. | 0.25 d | PERFORMANCE.md |
| 5 | **PROD_READY / CHANGELOG** — flip 1.11 evidence to "@ scale verified" or add [machine] row in §5.x. `[4.39.0]` changelog entry. | 0.25 d | PROD_READY 1.11 |

Dependencies: v4.38.
Risk: medium — touches every caller of `Table::rows`. Catalog
serialize/deserialize must still round-trip (the PV iterates so
serialization is unchanged).
Why not bundled into v4.38: keeping algorithm work and migration
work as separate commits makes regression bisect clean.

---

## v4.40 — `PersistentBTreeMap<K, V>` for table indices (CoW B-tree)

Same structural-sharing treatment for secondary indices. NSW /
HNSW stays on the v4.34 wrap (carved out; addressed in v5.0).

| # | item | est. | rows fixed |
|---|------|------|------------|
| 1 | **`crates/spg-storage/src/persistent_btree.rs`** — `pub struct PersistentBTreeMap<K: Ord, V>`; path-copy CoW B-tree, pure std. Branching factor 8–16 (tuned for cache lines). Operations: `insert` / `get` / `range` / `len` / `Clone`. Zero unsafe. | 2 d | foundation |
| 2 | **Fuzz oracle vs `BTreeMap`** — 100K-step random `insert` / `get` / `range` sequences; verify split/merge corner cases (single-key page, root split, sibling borrow). | 0.5 d | strengthens 1.10 |
| 3 | **`Table::indices` migration** — `IndexKind::BTree(BTreeMap<…>)` → `IndexKind::BTree(PersistentBTreeMap<…>)`. NSW path untouched. | 0.5 d | none directly |
| 4 | **Scale sweep + secondary-index variant** — tables with secondary index INSERT @ 1M ≥ 65K r/s. PERFORMANCE.md "after v4.40". | 0.25 d | PERFORMANCE.md |

Dependencies: v4.39 (so the rows are already on PV).
Risk: medium — B-tree CoW is the hardest of the three persistent
structures; split/merge corner cases must be fuzz-covered.
Carve-out: vector-indexed tables (NSW/HNSW) still take the v4.34
wrap on INSERT — that path's structural fix lands in v5.0 along
with the HNSW persistent graph.

---

## v4.41 — group commit + WAL binary encoding

Throughput unlock. Once the BEGIN..COMMIT wrap is cheap, the next
bottleneck is per-statement `fsync`. Group commit batches
concurrent writers into one fsync; binary WAL drops the text-
encoded INSERT overhead.

| # | item | est. | rows fixed |
|---|------|------|------------|
| 1 | **Group commit at dispatch** — writers contending on the WAL mutex have their bytes batched and `f.sync_data()` runs once for the group. Extend the existing `append_wal_atomic_block` helper to accept N statements. | 1 d | throughput 10.4 |
| 2 | **WAL binary record** — new type byte (continues the v4.37 sentinel system): `[type=binary][u32 len][u32 crc32][u32 table_id][u32 row_count][packed binary rows...]`. Row body uses the dense schema-driven encoding `Catalog::serialize` already established (FILE_VERSION 8 layout). Replay handles text v1/v2 + binary v3. STABILITY.md documents the new tag. | 1.5 d | 1.8 (WAL extension) |
| 3 | **Cross-version compat** — `tests/cross_version_compat` gains a v4.41 fixture; v4.31–v4.40 WAL still replays. | 0.5 d | maintains 8.5 |
| 4 | **Sweep + multi-connection variant** — 4 / 8 client concurrent INSERT @ 1M ≥ MySQL same-conditions × 1.5. Single-client INSERT @ 1M ≥ 200K r/s (target: > PG's 146K). @ 10M ≥ 80K r/s. PERFORMANCE.md "after v4.41". | 0.25 d | PERFORMANCE.md |

Dependencies: v4.40 (so the structural-sharing path is the floor).
Risk: medium — `fsync` semantics under group commit need a chaos
test (one writer fails the fsync, the group's failure handling
fans out correctly). Reuse the v4.37 bit-flip chaos infra for the
binary-encoding round-trip.

---

## v5.0.0 — HNSW persistent + allocator + OOM survival

SemVer major bump (changes panic / OOM semantics + closes the
v4.38-v4.40 vector-table carve-out).

| # | item | est. | rows fixed |
|---|------|------|------------|
| 1 | **HNSW persistent** — `NswGraph`'s `levels: Vec<u8>` and `layers: Vec<Vec<Vec<usize>>>` migrate to PV-style structural sharing so vector-table INSERT joins the cheap-TX path. Edge mutation = path-copy at the affected node only. | 4 d | seals 1.11 for vector |
| 2 | **Custom global allocator with per-query budget** — `#[global_allocator]` tracks per-thread bytes-allocated; `SPG_MAX_QUERY_BYTES` enforces cap; over → flip CancelToken so the query bails at the next checkpoint. (From the legacy v5.0 plan; spec unchanged.) | 3 d | 5.5 (fully ✅) |
| 3 | **OOM survives** — `set_alloc_error_hook` (stable since 1.59) returns a clean error to the client instead of aborting, except during WAL replay where abort is still correct. | 2 d | 5.6 |
| 4 | **Perf-regression gate** — allocator hot-path atomics + HNSW persistent walk; latency bench must hold SLO ceilings. Sweep at all N including 100M with vector-indexed tables. | 0.5 d | maintains 10.4 / 10.5 |
| 5 | **STABILITY.md v2 contract** — restate frozen surfaces; v5 cuts `SPG_FAIL_WAL_QUOTA_BYTES` chaos knob (real ENOSPC has full coverage). | 0.5 d | renews 8.5 |

Dependencies: v4.41 (binary WAL stable before SemVer major).
Risk: high — allocator hook + HNSW persistent are both touchy.
Bench gate is mandatory at this step.

---

## What this roadmap does NOT include

- TLS — permanently 🚫 (see `[[spg-out-of-scope]]` memory).
- Automated failover — 🚫. Manual promotion via
  RESTORE_DRILL.md step 5 stays the supported path.
- Sharding / multi-master — 🚫. Single-master with read replicas;
  horizontal write scaling is v6+ territory.
- Migration framework — 🚫. DDL via standard SQL is the model.
- Multi-tenant isolation — 🚫. Run separate `spg-server` processes.
- Foreign keys / CHECK constraints / row-level ACL — 🚫.
- External crates for persistent data structures (`im`, `rpds`,
  …) — 🚫. Hard rule: pure Rust, no new deps, no `unsafe`.

---

## Effort summary

| version | what                                            | est. days |
|---------|-------------------------------------------------|----------:|
| v4.38   | PersistentVec<T> (BVT)                          |       2.0 |
| v4.39   | Catalog/Table internals on PV                   |       2.5 |
| v4.40   | PersistentBTreeMap + Table::indices migration   |       3.25|
| v4.41   | group commit + binary WAL                       |       3.25|
| v5.0.0  | HNSW persistent + allocator + OOM               |      10.0 |
| **total** |                                               |    **21.0 d** |

---

## Perf gate matrix

Each checkpoint must hit its gate before merging. Failure → stop
and diagnose; do not soften the gate.

| checkpoint | spg-server INSERT @ 1M r/s | @ 10M | secondary-index @ 1M | sweep position vs PG | sqllogictest 4-corpus |
|------------|---------------------------:|------:|---------------------:|----------------------|-----------------------|
| baseline-v4.37 | 9.4K (broken) | bail | n/a | far below | 100% (must hold) |
| v4.38 | n/a (no Catalog touch) | n/a | n/a | unchanged | 100% |
| v4.39 | ≥ 100K no-index (slo_smoke); ≥ 15K with-index (sweep) | bail @ 1M with-index | indices unchanged | wrap clone O(1) for no-index | 100% |
| v4.40 | ≥ 50K with-index | no bail | ≥ 65K | within 2× of PG | 100% |
| v4.41 | ≥ 200K | ≥ 80K | ≥ 100K | > PG (146K) | 100% |
| v5.0 | ≥ 200K incl. vector tables | ≥ 80K incl. vector | ≥ 100K | > PG/MySQL/MariaDB | 100% |

### v4.39 ship reality (correction to earlier projection)

NEXT.md's original v4.39 row said "≥ 50K r/s on sweep". The
2026-05-27 sweep showed this gate **needs v4.40 to land**: the
sweep schema has 2 secondary indices, and v4.39 only swapped
`Table::rows` to PV — `Table::indices` stayed `Vec<Index>` so
`Catalog::clone` still deep-copies the BTreeMaps. spg-server
sweep @ 1M = **15K r/s** (1.6× over 9.4K baseline). Index-free
`slo_smoke` confirms the rows-clone fix gives **~109K r/s**
(12×), proving the wrap-side fix is correct. v4.40 (indices to
`PersistentBTreeMap`) is required to take sweep all the way to
the ≥ 50K floor. v4.41 (group commit + binary WAL) then takes
it to ≥ 200K. See PERFORMANCE.md "v4.39 scale sweep" section
for the full diff.
