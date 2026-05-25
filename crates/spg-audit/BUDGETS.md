# spg-audit perf budgets

Per stone-of-truth (see workspace `PERFORMANCE.md`): ~10× headroom over
the criterion bench median; gate catches order-of-magnitude regressions
only.

| Gate                                       | Budget   | Bench (criterion median) | Headroom |
|--------------------------------------------|---------:|-------------------------:|---------:|
| `append_one` (≤)                           | 50 µs    | **189 ns** (v3.0.0)      | ~260×    |

Run: `cargo test -p spg-audit --test perf_gate`.

`append` does one BLAKE3 over the prev-hash || serialized entry and
pushes to a `Vec`. Median is well below 5 µs on M-series; budget at
50 µs trips on real regressions only.
