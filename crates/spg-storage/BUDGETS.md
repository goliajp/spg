# spg-storage perf budgets

Per stone-of-truth (see workspace `PERFORMANCE.md`): ~10× headroom over
the criterion bench median; gate catches order-of-magnitude regressions
only.

| Gate                                              | Budget   | Bench (criterion median) | Headroom |
|---------------------------------------------------|---------:|-------------------------:|---------:|
| `catalog_roundtrip_100rows` (≤)                   | 5 ms     | **5.3 µs** ser+deser (v3.0.0) | ~940×    |
| `hnsw_search_top10_dim8_n200` (≤)                 | 1 ms     | **4.75 µs** (v3.0.0)     | ~210×    |

Run: `cargo test -p spg-storage --test perf_gate`.

The catalog roundtrip exercises every disk format path on a small
100-row, 3-col table; on M-series the real number sits in the
hundreds-of-microseconds band. HNSW top-10 search across a built
200-node index is sub-millisecond on M-series; the 1-ms budget trips
on real regressions (e.g. accidentally rebuilding the graph per
search, or an `O(n)` per-neighbour reallocation).
