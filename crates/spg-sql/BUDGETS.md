# spg-sql perf budgets

Per stone-of-truth (see workspace `PERFORMANCE.md`): ~10× headroom over
the criterion bench median; gate catches order-of-magnitude regressions
only.

| Gate                                              | Budget | Bench (criterion median) | Headroom |
|---------------------------------------------------|-------:|-------------------------:|---------:|
| `parse_select_where_order_limit` (≤)              | 50 µs  | **666 ns** (v3.0.0)      | ~75×     |

Run: `cargo test -p spg-sql --test perf_gate`.

A typical PG-dialect SELECT (single table, WHERE/ORDER/LIMIT) lexes
plus parses in a few hundred nanoseconds on M-series. The 50-µs gate
trips only on real regressions (e.g. accidentally quadratic token
allocation, recursion explosion in expression precedence climbing).
