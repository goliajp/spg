# spg-crypto perf budgets

Per stone-of-truth (see workspace `PERFORMANCE.md`): budgets sit ~10×
above the bench median so the `tests/perf_gate.rs` gate catches
order-of-magnitude regressions but ignores micro-perf noise. Quote the
criterion bench medians in `PERFORMANCE.md` for any publishable claim;
the gate numbers here are regression-catch only.

| Gate                          | Budget        | Bench (criterion median) | Headroom |
|------------------------------|--------------:|-------------------------:|---------:|
| `hash_64b` (≤)                | 10 µs         | **67 ns** (v3.0.0)       | ~150×    |
| `hash_1kib` (≤)               | 50 µs         | **1.14 µs** (v3.0.0)     | ~44×     |

Run: `cargo test -p spg-crypto --test perf_gate`.

The budgets above represent **wall-time per call on an M-series Mac in
release mode**. Self-built BLAKE3 (single-thread, no SIMD) lives in the
low-microsecond range; budgets at 10 µs / 50 µs trip only on real
regressions (e.g. an accidental allocator-in-hot-loop introduction).
