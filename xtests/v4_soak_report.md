# v4.x soak audit (v4.16)

Runs `xbench/competitor/src/bin/soak_v4.rs` against current
`spg-server` for 5 minutes. The cycle body deliberately exercises
every v4.x code path that allocates per-call:

| code path        | how it's stressed |
|------------------|-------------------|
| JSON parser      | `SELECT body ->> 'k' FROM docs WHERE id = ?` per cycle |
| Subquery resolve | `SELECT count(*) FROM sales WHERE amt IN (SELECT val FROM allowed)` per cycle |
| CTE temp engine  | `WITH big AS (...) SELECT count(*) FROM big` per cycle (catalog clone) |
| Window partition | `SELECT region, amt, ROW_NUMBER() OVER (...) FROM sales` per cycle |
| SCRAM secret churn | `CREATE USER 'uN' WITH PASSWORD 'p' ROLE 'readonly'` + immediate `DROP USER` per cycle |
| Observability HTTP | `GET /metrics` every 100 cycles |

## Result (2026-05-27, M-series 8-core)

```
- cycles        : 862377
- start RSS     : 2400 KiB
- end RSS       : 3888 KiB
- raw start→end : +62.0%
- post-warmup RSS (t=60s): 3888 KiB
- post-warmup→end drift  : +0.0%

| t (s) | RSS (KiB) |
|------:|----------:|
|    0 |      2400 |
|   30 |      3872 |
|   60 |      3888 |
|   90 |      3888 |
|  120 |      3888 |
|  150 |      3888 |
|  180 |      3888 |
|  210 |      3888 |
|  240 |      3888 |
|  270 |      3888 |
|  300 |      3888 |

verdict: ✅ leak-free (drift < 2% threshold, v3.4.2 baseline was 0.2%)
```

Initial RSS jump 2.4 → 3.9 MiB is the allocator's commit phase +
catalog page-in + HTTP/PG-wire listener threads — completes
within the first 30 s and never grows after. Post-warmup drift
is bit-exact zero across 4 minutes of continuous churn.

## Reproducing

```sh
CARGO_TARGET_DIR=/Volumes/INTEL2T/workspace-cache/cargo-target \
  cargo run --release -p spg-bench-competitor --bin soak_v4 -- --minutes N
```

Defaults to 5 minutes. Pre-release recommend `--minutes 60`. CI
nightly: `--minutes 10`.

## Notes vs v3.4.2 baseline

The v3.4.2 audit (pre-v4) drove a different workload (60% indexed
SELECT, 30% INSERT, 10% HNSW search) and measured 0.2% drift
post-warmup. v4.16 changes the workload entirely (CTE + window +
JSON + SCRAM + subquery) and lands at 0.0% — meaning every v4.x
addition allocates+drops cleanly per cycle.

If a future v4.x change introduces a leak it will show up
immediately as a non-flat post-warmup curve and the verdict
flips to ❌ with exit code 2.
