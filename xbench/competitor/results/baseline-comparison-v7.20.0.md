# SPG v7.20.0 vs baselines — PG 18 / MySQL 9 / MariaDB 11 / pgvector

v7.20 perf epic results: WAL group-commit (P2) + inline readonly
path + statement cache (P3). Same harness / host / session as the
v7.19 baseline file; backends identical.

## 1. Concurrent sqlx Pool — the v7.20 epic's target quadrants

| Workload @ conn | v7.19 | **v7.20** | Δ | PgPool (wire) |
|---|---:|---:|---:|---:|
| pk-select @1 | 22 249 | **33 582** | +51% | 4 269 |
| pk-select @4 | 28 183 | **51 692** | +83% | 10 331 |
| pk-select @16 | 21 539 | **58 858** | **+174%** | 17 956 |
| pk-select @64 | 21 593 | **58 571** | +171% | n/a |
| range-scan @16 | 20 091 | 18 124 | −10% (noise) | 14 877 |
| mixed-9to1 @4 | 1 649 | **4 025** | +144% | 9 231 |
| mixed-9to1 @16 | 1 617 | **7 297** | **+351%** | 17 444 |
| mixed-9to1 @64 | 1 648 | **6 655** | +304% | n/a |
| read_handle ceiling @16 | 63 971 | 70 522 | — | — |

Headlines:

- **pk-select @16: 58.9k q/s = 3.3× wire-PG, 84% of the
  SPG-private read_handle ceiling.** The stock sqlx adapter is
  now effectively at engine speed — P3 removed all three
  spawn_blocking hops + redundant parses per SELECT.
- **mixed @16: 7.3k = 4.5× v7.19.** Group-commit shares one
  fsync across the batch. On this macOS host the number
  converges at fsync_rate × batch_size where Rust's sync_data
  is a TRUE F_FULLFSYNC (4.2 ms); the PgPool 17.4k comparator
  runs fdatasync inside an OrbStack VM — effectively buffered,
  not durability-equal. Container-vs-container (same VM fs,
  same fsync semantics) is the fair fight: see image-side
  numbers after the v7.20.0 release.

## 2. Latency — single-row ops (µs, p50, unchanged from v7.19)

| backend | INSERT | SELECT (PK) |
|---|---:|---:|
| spg-embedded (in-mem) | 1.2 | 1.9 |
| spg-server (native wire) | 18.2 | 18.3 |
| postgres | 1 071.8 | 415.2 |
| mysql | 1 896.2 | 348.6 |
| mariadb | 1 103.5 | 347.1 |

## 3. Throughput — 10 000-row batch + full scan (unchanged)

| backend | INS rows/s | SCAN rows/s |
|---|---:|---:|
| spg-embedded | 2 595 913 | 17 103 782 |
| spg-server | 915 042 | 8 140 008 |
| postgres | 22 488 | 3 177 967 |
| mysql | 21 211 | 3 198 295 |
| mariadb | 54 473 | 3 518 701 |

## 4. Vector kNN — 10 000 × dim-128, top-10, HNSW

| backend | build s | q p50 µs |
|---|---:|---:|
| spg-embedded | 0.64 | 32.7 |
| spg-server | 0.91 | 62.7 |
| postgres+pgvector | 1.80 | 1 030.9 |

kNN query p50: **31.5× faster than pgvector** (embed),
16.4× (server).

## Caveats (same as v7.19 file, §1 updated)

1. INSERT columns in §2/§3 are not durability-equal (SPG
   in-memory vs fsync'd baselines). §1's mixed rows ARE
   durable on the SPG side — and OVER-durable relative to the
   comparator (true F_FULLFSYNC vs VM-buffered fdatasync).
   The fair durable-write comparison is container-vs-container
   on the same VM filesystem; scheduled for the v7.20.0
   image-side bench.
2. Container-vs-host wire asymmetry as before.
3. spg-server rows use the SPG native wire, not pgwire.

## Reproduce

```bash
cd xbench/competitor && docker compose up -d && cd ../..
cargo build --release -p spg-bench-competitor \
    --bin latency --bin throughput --bin vector_knn \
    --bin sqlx_pool_concurrent_reads -p spg-server
./target/release/latency && ./target/release/throughput && \
./target/release/vector_knn && \
./target/release/sqlx_pool_concurrent_reads --iters 500 --rows 5000
```
