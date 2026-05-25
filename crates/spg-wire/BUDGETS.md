# spg-wire perf budgets

Per stone-of-truth (see workspace `PERFORMANCE.md`): ~10× headroom over
the criterion bench median; gate catches order-of-magnitude regressions
only.

| Gate                                       | Budget   | Bench (criterion median) | Headroom |
|--------------------------------------------|---------:|-------------------------:|---------:|
| `query_encode_decode_roundtrip` (≤)        | 10 µs    | **47 ns** (v3.0.0)       | ~210×    |

Run: `cargo test -p spg-wire --test perf_gate`.

Wire-level frames are pure mem-copy plus length-prefix bookkeeping; an
encode/decode roundtrip on a short Query frame is well sub-microsecond
on M-series. A regression to the 10-µs ceiling means an allocator or
copy was added to the hot path.
