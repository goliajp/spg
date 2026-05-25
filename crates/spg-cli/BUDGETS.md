# spg-cli perf budgets

Per stone-of-truth (see workspace `PERFORMANCE.md`): ~10× headroom over
the criterion bench median; gate catches order-of-magnitude regressions
only.

`spg-cli` is a binary crate — its hot paths are thin wrappers around
the lower stones (`spg-storage` for backup/restore, `spg-wire` for
ping/query/stats). The single gate below sanity-checks the
backup-roundtrip flow including file I/O.

| Gate                                              | Budget   | Bench (criterion median) | Headroom |
|---------------------------------------------------|---------:|-------------------------:|---------:|
| `backup_roundtrip_100rows` (≤)                    | 100 ms   | **~12 ms** (v3.0.0; 5–22 ms run-to-run) | ~8× — disk-dominated, wide budget to absorb fsync noise |

Run: `cargo test -p spg-cli --test perf_gate`.

The bench includes filesystem read + write so the median picks up disk
syscall cost. Budget at 20 ms covers wide variance across machines; an
in-memory baseline would be far tighter, but the user-visible path is
disk-bound, so we measure what the user feels.
