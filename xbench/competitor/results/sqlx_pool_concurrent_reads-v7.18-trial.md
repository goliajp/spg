# sqlx Pool concurrent reads — v7.18 benchmark

Trial run captured during the v7.18 epic. Parameters:
`--iters 500 --rows 5000`, file-backed SPG catalog in tmpdir,
`pgvector/pgvector:pg18` container at `127.0.0.1:25432`
(`max_connections=20` in `docker-compose.yml`, so PgPool
concurrency 64 reports `[unavailable]`).

Workloads:
- **pk-select** — `SELECT label FROM bench WHERE id = $1` (point lookup)
- **range-scan** — `SELECT id FROM bench WHERE v BETWEEN $1 AND $2` (~50 rows)
- **mixed-9to1** — 90% pk-select + 10% UPDATE

Backends:
- **SpgPool** — sqlx adapter, per-statement snapshot routing (v7.18)
- **read_handle** — `AsyncReadHandle` escape-hatch (SPG-private, bypasses sqlx)
- **PgPool** — `pgvector/pgvector:pg18` over the wire @ `127.0.0.1:25432`

| Backend | Workload | Concurrency | Throughput (q/s) | p50 (us) | p99 (us) | p999 (us) | Elapsed (ms) |
|---|---|---:|---:|---:|---:|---:|---:|
| PgPool | mixed-9to1 | 1 | 3260 | 190 | 1349 | 2745 | 153 |
| PgPool | mixed-9to1 | 4 | 8117 | 378 | 1871 | 2780 | 61 |
| PgPool | mixed-9to1 | 16 | 15250 | 836 | 4074 | 6574 | 32 |
| PgPool | mixed-9to1 | 64 | [unavailable] | — | — | — | — |
| PgPool | pk-select | 1 | 4125 | 233 | 366 | 709 | 121 |
| PgPool | pk-select | 4 | 9795 | 372 | 980 | 4701 | 51 |
| PgPool | pk-select | 16 | 13724 | 933 | 7981 | 8965 | 36 |
| PgPool | pk-select | 64 | [unavailable] | — | — | — | — |
| PgPool | range-scan | 1 | 2508 | 386 | 550 | 768 | 199 |
| PgPool | range-scan | 4 | 5010 | 695 | 1960 | 9292 | 99 |
| PgPool | range-scan | 16 | 12684 | 1213 | 2088 | 2304 | 39 |
| PgPool | range-scan | 64 | [unavailable] | — | — | — | — |
| SpgPool | mixed-9to1 | 1 | 669 | 75 | 14055 | 123271 | 747 |
| SpgPool | mixed-9to1 | 4 | 1588 | 233 | 12133 | 22797 | 314 |
| SpgPool | mixed-9to1 | 16 | 1053 | 13033 | 47494 | 51110 | 474 |
| SpgPool | mixed-9to1 | 64 | 1352 | 45725 | 79672 | 86727 | 369 |
| SpgPool | pk-select | 1 | 13257 | 70 | 144 | 263 | 37 |
| SpgPool | pk-select | 4 | 18991 | 201 | 360 | 425 | 26 |
| SpgPool | pk-select | 16 | 20561 | 750 | 1199 | 1616 | 24 |
| SpgPool | pk-select | 64 | 19346 | 3374 | 3914 | 4285 | 25 |
| SpgPool | range-scan | 1 | 2040 | 465 | 653 | 5795 | 245 |
| SpgPool | range-scan | 4 | 7608 | 506 | 694 | 747 | 65 |
| SpgPool | range-scan | 16 | 11517 | 1341 | 2166 | 2629 | 43 |
| SpgPool | range-scan | 64 | 11065 | 5613 | 7095 | 7458 | 45 |
| read_handle | pk-select | 1 | 40938 | 20 | 50 | 106 | 12 |
| read_handle | pk-select | 4 | 62905 | 55 | 147 | 222 | 7 |
| read_handle | pk-select | 16 | 60787 | 201 | 829 | 1057 | 8 |
| read_handle | pk-select | 64 | 68189 | 799 | 2332 | 3106 | 7 |

## Reading the numbers

**Pure read throughput** (pk-select):

- SpgPool's snapshot routing delivers **20.5k q/s at max_connections=16** vs PgPool's **13.7k q/s** at the same concurrency — SPG-embed wins by ~1.5× on a Rust + sqlx workload because there's no wire / TCP round-trip on the embed path.
- read_handle bare hits 60-68k q/s — that's the SPG-private upper bound (skipping the sqlx parse + Executor dispatch + per-statement snapshot refresh overhead). SpgPool is at ~30% of that ceiling; the gap is acceptable for a drop-in sqlx user since the sqlx adapter pulls its weight on type bridging, prepared-statement validation, and the Pool semantics PG users expect.

**Mixed workload** (mixed-9to1, 10% UPDATE):

- File-backed SPG's WAL fsync per UPDATE shows up as a tail-latency cliff (p999 = 123ms at SpgPool max_conn=1). PgPool's tail under the same workload is 2-7ms.
- Throughput-wise SpgPool stays at ~1k-2k q/s on mixed while PgPool reaches 15k q/s at max_conn=16. The gap is the engine's single-writer + fsync cadence — addressing that is on the PITR / WAL-group-commit track (v7.18 PITR design doc).
- For sqlx users this is the expected behaviour: writes serialise (engine invariant), reads fan out (routing on). A mailrs-shape mostly-read workload sits closer to the pk-select numbers above than to mixed.

**Range scans**:

- SpgPool 11.5k q/s vs PgPool 12.7k q/s at max_conn=16 — essentially on par. The engine's range path doesn't have a wire round-trip advantage at this row count; the comparison is just about scan efficiency, and the two engines come out within a few percent of each other.

## How to reproduce

```bash
docker compose -f xbench/competitor/docker-compose.yml up -d postgres
cargo run --release -p spg-bench-competitor --bin sqlx_pool_concurrent_reads -- \
    --iters 500 --rows 5000
```

Without docker the PgPool rows report `[unavailable]` and the SpgPool /
read_handle side still produces full numbers. PG container's
`max_connections=20` (in `docker-compose.yml`) caps PgPool concurrency
at 18 (after admin reserve); bench skips higher concurrencies with
`[unavailable]` rather than wedging on `PoolTimedOut`.

## Caveats

- `iters=500, rows=5000` is a smoke-grade run intended to validate
  the harness shape during the v7.18 epic. Full-scale numbers want
  `--iters 5000 --rows 50000` or higher; replace this report with
  the new output when re-running.
- Hardware: developer laptop (numbers will not match CI / prod).
- pg_stat_statements, autovacuum, etc are at the
  pgvector/pgvector:pg18 defaults; both sides are warm before
  measurement starts.
