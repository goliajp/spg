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

## v4.41 — WAL v3 framing + auto-commit wrap merge

Cut the per-write WAL header overhead. v4.34 wraps every auto-
commit write into `[BEGIN, sql, COMMIT]` — three v2 records, three
8-byte headers, 35 bytes of overhead per write plus the literal
`"BEGIN"` and `"COMMIT"` SQL bytes. v4.41 introduces a v3 frame
that carries the same auto-commit semantics in one record:

```
v3 record:
  [u32 LE (len | 0xC000_0000)]    ← bit 31 (v2 sentinel kept) + bit 30 (v3 flag)
  [u32 LE crc32(type_byte || payload)]
  [1 byte type]
  [len bytes payload]              ← len counts payload only, not the type byte
```

`type=0x01 auto_commit_sql` replays via a single `engine.execute(sql)`
(engine's own implicit auto-commit ≡ the BEGIN..stmt..COMMIT the
writer expressed). Same atomicity story as v4.34: one `write_all`
+ one `fsync`, identical ENOSPC-rollback chaos coverage. Header
overhead **35 → 9 bytes per write**.

Group commit / multi-writer batching is **not** in v4.41 — see
v4.42 for that. v4.34 held the engine `RwLock` write guard across
WAL append + fsync (Catalog::clone was expensive then), so today
multi-client writers contend on the *engine* lock, not the WAL
mutex. Group commit at the WAL layer would have nothing to batch
without first cutting the engine critical section — which needs
v4.42's engine MVCC work.

| # | item | est. | rows fixed |
|---|------|------|------------|
| 1 | **v3 record framing** — `WAL_V3_FLAG = 0x4000_0000`, `WAL_V3_SENTINEL = 0xC000_0000`, `encode_wal_v3_record(type, payload)`, type byte `WAL_V3_TYPE_AUTO_COMMIT_SQL = 0x01`. Replace `append_wal_atomic_block(["BEGIN", sql, "COMMIT"])` with `append_wal_v3_auto_commit(sql)` in dispatch. CRC covers `[type || payload]` so a flipped type byte fails replay. | 0.5 d | 1.8 (WAL extension) |
| 2 | **Replay three-way dispatch** — v1 (`bit 31 = 0`), v2 (`bit 31 = 1, bit 30 = 0`), v3 (`bit 31 = 1, bit 30 = 1`) in `replay_wal_bytes`. Unknown v3 type byte is fatal — never silently skipped (forward-compat fence). `tests/e2e_wal_binary.rs` covers emit, replay, mixed-version interleave, and unknown-type abort. | 0.5 d | none |
| 3 | **Cross-version compat fixture** — `xtests/compat-fixtures/v4.41/` captures a v3 WAL (CREATE + 3 INSERT). Run via `cargo test --test cross_version_compat -- --ignored capture_v4_41_fixture` at release time. v4.30 fixture (v1 framing) still replays. | 0.25 d | maintains 8.5 |
| 4 | **STABILITY.md** — v3 frame + the two sentinel bits + `auto_commit_sql` type tag enter the frozen surface. | 0.25 d | 8.5 |
| 5 | **Sweep rerun** — `cargo run --release -p spg-bench-competitor --bin sweep`. Honest measurement; spg-server INSERT @ 1M / 10M numbers + diff vs v4.40 land in PERFORMANCE.md "after v4.41". No hard gate this version — the 200K / 80K / multi-client targets carry over to v4.42 where they're structurally addressable. | 0.25 d | PERFORMANCE.md |

Dependencies: v4.40.
Risk: low — framing-only work, no engine changes. The v3 sentinel
re-uses the bit 30 of the v2 length field; v2 lengths are << 1 GiB
in practice so the bit was free. Forward-compat for ≤ v4.40
binaries reading v3 records is not required (STABILITY documents
this explicitly, same precedent as v2's break of v1 readers).

---

## v4.42 — Engine MVCC + group commit at dispatch

**The hard part of the throughput unlock.** v4.34 made the engine
`RwLock<Engine>` write guard hold across `BEGIN → execute → WAL →
COMMIT/ROLLBACK` because at that time `Engine::tx_catalog: Option<Catalog>`
was a single global slot and `Catalog::clone()` was expensive — there
was no way to let two implicit TXs be in flight without each one
seeing the other's mutation. v4.40 (PV + PBTreeMap) makes
`Catalog::clone()` O(1) at any scale, which removes the cost half
of that reasoning. v4.42 removes the structural half: the engine
gets N in-flight TX slots, dispatch lets N writers prepare in
parallel, and one fsync covers the whole batch.

| # | item | est. | rows fixed |
|---|------|------|------------|
| 1 | **`Engine::tx_catalog: BTreeMap<TxId, Catalog>`** — replace the `Option<Catalog>` slot. `Engine::execute(sql, tx_id)` threads a per-connection `TxId`. `BEGIN` → allocate `TxId`, clone catalog into the map (O(1) by v4.40); `COMMIT` → install map entry over catalog; `ROLLBACK` → drop map entry. Implicit auto-commits get a one-shot `TxId`. | 2 d | structural |
| 2 | **Dispatch — split engine.write() critical section** — engine `RwLock` guard wraps only the install phase. Writers prepare in parallel under `engine.read()`: each pulls its catalog clone into a `TxId`, runs `engine.execute(sql, tx_id)`, encodes the v3 WAL record. Then they queue on a `commit_seq` channel; the leader drains the queue, single `write_all` + single `fsync` covers the batch, then each entry's install runs under one short `engine.write()`. | 1.5 d | throughput 10.4 |
| 3 | **Group fsync failure fan-out** — `tests/e2e_chaos.rs` gains a multi-client variant of `chaos_disk_full_no_preflight_rolls_back_in_memory_to_match_durable_state`: when the leader's `fsync` errors, every queued writer's `TxId` rolls back. No phantom rows survive. | 0.75 d | 1.11 multi-client |
| 4 | **Sweep + multi-connection variant** — extend `xbench/competitor/src/bin/sweep.rs` (or new `concurrent_sweep.rs`) to drive 4 / 8 client concurrent INSERT @ 1M against all backends. Gate: spg-server ≥ MySQL × 1.5 at 4 clients; single-client @ 1M ≥ 200K r/s (this is where the 200K gate actually becomes structurally reachable); @ 10M ≥ 80K. PERFORMANCE.md "after v4.42". | 0.5 d | PERFORMANCE.md |

Dependencies: v4.41 (v3 framing is the substrate batching writes into).
Risk: high — engine MVCC touches `execute()` dispatch + the
implicit-TX path the chaos tests pin. Reuse the v4.37 bit-flip
chaos infra for fsync failures.

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

Dependencies: v4.42 (group commit + MVCC stable before SemVer major).
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
| v4.41   | WAL v3 framing + auto-commit wrap merge         |       1.75|
| v4.42   | Engine MVCC + group commit at dispatch          |       4.75|
| v5.0.0  | HNSW persistent + allocator + OOM               |      10.0 |
| **total** |                                               |    **24.25 d** |

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
| v4.41 | honest measurement (see PERFORMANCE.md "after v4.41") | honest measurement | honest measurement | header overhead 35→9 bytes/write | 100% |
| v4.42 | ≥ 200K (single client); ≥ MySQL × 1.5 (4 clients) | ≥ 80K | ≥ 100K | > PG (146K) | 100% |
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
the ≥ 50K floor. v4.41 (v3 framing + auto-commit wrap merge)
trims per-write header overhead from 35 to 9 bytes; v4.42
(engine MVCC + group commit) is where the ≥ 200K gate
becomes structurally reachable. See PERFORMANCE.md "v4.39
scale sweep" section for the full diff.
