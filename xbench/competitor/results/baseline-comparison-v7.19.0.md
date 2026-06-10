# SPG v7.19.0 vs baselines — PG 18 / MySQL 9 / MariaDB 11 / pgvector

Side-by-side numbers for both SPG deployment modes against the
three mainstream engines SPG positions as a drop-in replacement
for. All runs on the same host, same session, release builds,
`xbench/competitor` harness.

Backends:

| Backend | Transport | Durability at bench time |
|---|---|---|
| spg-embedded | in-process fn call | **in-memory engine, no WAL fsync** (see caveats) |
| spg-server | SPG native wire, host process @ 127.0.0.1:25544 | in-memory daemon (same caveat) |
| postgres | pgvector/pgvector:pg18 container @ 25432 | fsync per commit (default) |
| mysql | mysql:9 container @ 23306 | innodb_flush_log_at_trx_commit=1 |
| mariadb | mariadb:11 container @ 23307 | same flush policy |

## 1. Latency — single-row INSERT + PK SELECT (µs, 2000 iters)

| backend | ins p50 | ins p95 | ins p99 | sel p50 | sel p95 | sel p99 |
|---|---:|---:|---:|---:|---:|---:|
| **spg-embedded** | **1.0** | 1.6 | 4.8 | **1.2** | 1.4 | 4.6 |
| **spg-server** | **14.7** | 38.9 | 52.9 | **15.4** | 42.1 | 57.8 |
| postgres | 2 315.3 | 19 041.4 | 42 468.4 | 404.7 | 584.2 | 801.5 |
| mysql | 2 163.2 | 4 053.8 | 24 781.7 | 321.8 | 433.5 | 530.8 |
| mariadb | 1 255.6 | 1 967.9 | 10 076.9 | 326.5 | 494.5 | 762.0 |

Read-side (fair comparison — no durability asymmetry):

- **spg-embedded SELECT: ~270× faster than PG** (1.2 vs 405 µs)
- **spg-server SELECT: ~26× faster than PG** (15.4 vs 405 µs),
  ~21× vs MySQL/MariaDB

## 2. Throughput — 10 000-row batch INSERT + full-table scan

| backend | INSERT ms | INS rows/s | SCAN ms | SCAN rows/s |
|---|---:|---:|---:|---:|
| **spg-embedded** | **3.88** | **2 576 656** | **0.53** | **18 902 127** |
| **spg-server** | **9.63** | **1 038 880** | **1.27** | **7 844 934** |
| postgres | 3 179.34 | 3 145 | 4.16 | 2 404 641 |
| mysql | 695.09 | 14 387 | 3.20 | 3 129 318 |
| mariadb | 282.83 | 35 357 | 2.78 | 3 596 637 |

Scan-side (fair): **spg-embedded scans ~7.9× faster than PG**,
spg-server ~3.3× — same data shape, same rows.

## 3. Vector kNN — top-10 over 10 000 dim-128 vectors (HNSW)

| backend | build s | q p50 µs | q p95 µs | q p99 µs |
|---|---:|---:|---:|---:|
| **spg-embedded** | **0.80** | **39.6** | 71.2 | 85.5 |
| spg-embedded (SQ8) | 1.53 | 42.8 | 74.8 | 96.4 |
| spg-embedded (HALF) | 2.12 | 61.2 | 112.4 | 161.1 |
| **spg-server** | **0.96** | **81.2** | 114.6 | 144.2 |
| spg-server (SQ8) | 1.74 | 86.4 | 122.6 | 137.3 |
| spg-server (HALF) | 2.33 | 100.8 | 149.5 | 183.8 |
| postgres+pgvector | 4.53 | 933.5 | 1 125.4 | 1 448.6 |

- **HNSW build: SPG 5.7× faster than pgvector** (0.80 vs 4.53 s)
- **kNN query p50: spg-embedded 23.6× faster than pgvector**
  (39.6 vs 933.5 µs); spg-server 11.5×
- SQ8/HALF quantised encodings stay within ~1.5× of f32 while
  cutting storage 4× / 2× — pgvector's `halfvec` comparison not
  run this round.

## 4. Concurrent sqlx Pool (from sqlx_pool_concurrent_reads-v7.19.0.md)

| backend | pk-select @16 conn | mixed-9to1 @16 |
|---|---:|---:|
| SpgPool (embed, sqlx adapter) | 21 539 q/s | 1 617 q/s |
| PgPool (wire, pgvector-pg18) | 18 156 q/s | 17 616 q/s |
| read_handle (SPG-private API) | 63 971 q/s | — |

Concurrent reads through the stock sqlx adapter beat wire-mode
PG. Mixed write-heavy workloads remain PG's win at high
concurrency — SPG's single-writer + per-commit fsync serialises
writes by design; see the WAL group-commit item under future
work.

## Caveats — read before quoting

1. **INSERT columns are not durability-equal.** The latency /
   throughput harness builds spg-embedded with `Engine::new()`
   (in-memory, no WAL) and boots spg-server without a WAL path,
   while PG / MySQL / MariaDB fsync per commit. The INSERT
   numbers measure engine + wire cost, not group-commit disk
   behaviour. SELECT / SCAN / kNN columns have no such asymmetry
   — reads never touch the WAL in any engine.
   For durable-write apples-to-apples, the sqlx Pool bench
   (§4) runs SPG file-backed with fsync per commit; its
   mixed-9to1 row is the honest durable-write comparison.
2. **Container vs host process.** PG / MySQL / MariaDB run in
   OrbStack containers (loopback through the VM NIC);
   spg-server runs as a host process on loopback. Wire-path
   overhead is not perfectly symmetric. The ~26× SELECT gap is
   far larger than VM-NIC overhead (~10-30 µs), so the ordering
   stands, but single-digit-µs comparisons between the wire
   backends should not be over-read.
3. **spg-server numbers use the SPG native wire**, not pgwire.
   pgwire adds protocol parsing overhead; mailrs-shape clients
   connecting via `postgres://` URLs will land between the
   spg-server and postgres rows.
4. Smoke-grade parameters on a developer laptop. Run the same
   bins on prod-shape hardware before quoting externally.

## Reproduce

```bash
cd xbench/competitor && docker compose up -d && cd ../..
cargo build --release -p spg-bench-competitor \
    --bin latency --bin throughput --bin vector_knn -p spg-server
./target/release/latency
./target/release/throughput
./target/release/vector_knn
```
