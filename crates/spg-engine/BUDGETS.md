# spg-engine perf budgets

Per stone-of-truth (see workspace `PERFORMANCE.md`): ~10× headroom over
the criterion bench median; gate catches order-of-magnitude regressions
only.

| Gate                                              | Budget   | Bench (criterion median) | Headroom |
|---------------------------------------------------|---------:|-------------------------:|---------:|
| `execute_select_where_n100` (≤)                   | 500 µs   | **2.57 µs** (v3.0.0)     | ~190×    |
| `content_worker_not_exists_top_n` (≤)             | 100 ms   | **~3 ms** (v7.37.x)      | ~30×     |
| `mailrs_content_worker_100k_join` (≤)             | 500 ms   | **~32 ms** (v7.38.2)     | ~15×     |
| `mailrs_content_worker_100k_no_analyze` (≤)       | 500 ms   | **~32 ms** (v7.38.2)     | ~15×     |

Run: `cargo test -p spg-engine --test perf_gate`.

The two `mailrs_content_worker_100k` rows were 100 ms until v7.38.2 —
about 3× over the measured value, well inside the order-of-magnitude
rule this file states, and so tight that a CI runner 3.3× slower than
the calibrating box turned it red on a release commit with no
regression behind it (A/B against v7.38.1 in a separate worktree:
~32 ms both sides, spreads overlapping). The shape they catch is the
~3-5 s prod regression, which 500 ms still separates by 6-10×.

A typical SELECT with WHERE walks the whole 100-row table once, then
runs the WHERE expression on each row. On M-series the real number
sits in the low-microseconds band; the 500-µs budget trips only on
real regressions (e.g. accidental `O(n²)` scan, allocator-in-hot-loop).
