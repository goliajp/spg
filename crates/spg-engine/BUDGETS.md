# spg-engine perf budgets

Per stone-of-truth (see workspace `PERFORMANCE.md`): ~10× headroom over
the criterion bench median; gate catches order-of-magnitude regressions
only.

| Gate                                              | Budget   | Bench (criterion median) | Headroom |
|---------------------------------------------------|---------:|-------------------------:|---------:|
| `execute_select_where_n100` (≤)                   | 500 µs   | **2.57 µs** (v3.0.0)     | ~190×    |
| `content_worker_not_exists_top_n` (≤)             | 100 ms   | **~3 ms** (v7.37.x)      | ~30×     |

Run: `cargo test -p spg-engine --test perf_gate`.

A typical SELECT with WHERE walks the whole 100-row table once, then
runs the WHERE expression on each row. On M-series the real number
sits in the low-microseconds band; the 500-µs budget trips only on
real regressions (e.g. accidental `O(n²)` scan, allocator-in-hot-loop).
