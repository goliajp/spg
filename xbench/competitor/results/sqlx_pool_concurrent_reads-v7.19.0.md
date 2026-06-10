# sqlx Pool concurrent reads — v7.19.0 regression check

Same parameters as the v7.18 trial (`--iters 500 --rows 5000`,
file-backed SPG catalog in tmpdir, `pgvector/pgvector:pg18` at
`127.0.0.1:25432`) so the two tables compare directly. Captured
against the v7.19.0 release build (chunked WAL + retention thread
+ SRF unnest landed since the v7.18 numbers).

## v7.19.0 numbers

| Backend | Workload | Concurrency | Throughput (q/s) | p50 (us) | p99 (us) | p999 (us) |
|---|---|---:|---:|---:|---:|---:|
| SpgPool | pk-select | 1 | 22 249 | 41 | 98 | 224 |
| SpgPool | pk-select | 4 | 28 183 | 134 | 245 | 296 |
| SpgPool | pk-select | 16 | 21 539 | 717 | 1 222 | 1 676 |
| SpgPool | pk-select | 64 | 21 593 | 3 047 | 3 669 | 3 865 |
| SpgPool | range-scan | 1 | 2 249 | 438 | 506 | 657 |
| SpgPool | range-scan | 4 | 8 434 | 465 | 547 | 587 |
| SpgPool | range-scan | 16 | 20 091 | 762 | 1 289 | 1 498 |
| SpgPool | range-scan | 64 | 17 240 | 3 534 | 4 624 | 4 840 |
| SpgPool | mixed-9to1 | 1 | 1 577 | 45 | 6 500 | 9 003 |
| SpgPool | mixed-9to1 | 4 | 1 649 | 184 | 7 332 | 12 518 |
| SpgPool | mixed-9to1 | 16 | 1 617 | 7 576 | 19 874 | 24 650 |
| SpgPool | mixed-9to1 | 64 | 1 648 | 37 621 | 66 287 | 72 650 |
| read_handle | pk-select | 16 | 63 971 | 213 | 664 | 915 |
| PgPool | pk-select | 16 | 18 156 | 697 | 5 670 | 6 824 |
| PgPool | range-scan | 16 | 14 891 | 1 030 | 1 705 | 2 716 |
| PgPool | mixed-9to1 | 16 | 17 616 | 728 | 2 799 | 3 167 |

## v7.18 → v7.19 delta (SpgPool)

| Workload @ concurrency | v7.18 q/s | v7.19 q/s | Δ |
|---|---:|---:|---:|
| pk-select @ 1 | 13 257 | 22 249 | **+68%** |
| pk-select @ 4 | 18 991 | 28 183 | **+48%** |
| pk-select @ 16 | 20 561 | 21 539 | +5% |
| pk-select @ 64 | 19 346 | 21 593 | +12% |
| range-scan @ 16 | 11 517 | 20 091 | **+74%** |
| mixed-9to1 @ 1 | 669 | 1 577 | **+136%** |
| mixed-9to1 @ 16 | 1 053 | 1 617 | +54% |
| mixed-9to1 @ 64 | 1 352 | 1 648 | +22% |
| **mixed-9to1 p999 @ 1** | **123 271 us** | **9 003 us** | **−93%** |

**No regression on any cell; broad improvement.** The biggest
win is the mixed-workload tail: v7.18's checkpoint truncated the
WAL and rewrote the full catalog snapshot inline with the
triggering `execute()` call — a 123 ms p999 cliff. v7.19's
chunk rotation replaces the truncate with a close+open of two
file handles, so the snapshot write is the only remaining
inline cost and the p999 falls to 9 ms.

Reads improve because rotation no longer contends with the
snapshot-refresh path on the same file handle.

Comparator note: PgPool numbers also rose vs the v7.18 trial
(same host, lighter ambient load this run) — the SpgPool deltas
above are larger than the comparator drift on every row, and
SpgPool pk-select@16 (21.5k q/s) still leads PgPool@16 (18.2k).

## Reproduce

```bash
docker compose -f xbench/competitor/docker-compose.yml up -d postgres
cargo run --release -p spg-bench-competitor --bin sqlx_pool_concurrent_reads -- \
    --iters 500 --rows 5000
```

Caveats: smoke-grade params on a developer laptop; same caveats
as the v7.18 trial file. Replace this report when re-running at
full scale.
