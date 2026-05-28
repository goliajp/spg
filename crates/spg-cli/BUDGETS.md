# spg-cli perf budgets

Per stone-of-truth (see workspace `PERFORMANCE.md`): ~10× headroom over
the criterion bench median; gate catches order-of-magnitude regressions
only.

`spg-cli` is a binary crate — its hot paths are thin wrappers around
the lower stones (`spg-storage` for backup/restore, `spg-wire` for
ping/query/stats).

| Gate                                              | Budget   | Bench (criterion median) | Headroom | CI? |
|---------------------------------------------------|---------:|-------------------------:|---------:|:---:|
| `backup_codec_under_budget` (≤)                   | 5 ms     | ~0.12 ms (v5.3.5; 0.10–0.20 ms run-to-run) | ~40× — pure in-memory codec, ample headroom for cold-cache jitter | ✅ |
| `backup_disk_roundtrip_under_budget` (≤, `#[ignore]`) | 400 ms | ~120 ms (v5.3.5 on macOS Tahoe APFS) / ~10 ms (Linux ext4) | ~3× host-OS-dependent, smoke test only | ❌ release-process |

Run: `cargo test -p spg-cli --test perf_gate`.

## v5.3.5 split rationale

Until v5.3.4 a single `backup_roundtrip_under_budget` gate timed the
full `fs::read + Catalog::deserialize + Catalog::serialize + fs::write`
loop on a 100-row file. On macOS Tahoe APFS the `fs::write` truncate-
then-rewrite call alone takes ~120 ms / iter on a < 1 KB file, which
swamped the 100 ms budget and silently turned a spg regression gate
into an OS file-system benchmark.

v5.3.5 splits the two concerns:

- **`backup_codec_under_budget`** measures only the spg-storage codec
  round-trip. On the same Tahoe machine the codec runs at ~117 µs / iter
  for 100 rows — the 5 ms budget gives ~40× headroom and will catch any
  serious spg-storage regression without sensitivity to disk noise.
- **`backup_disk_roundtrip_under_budget`** keeps the original disk-aware
  loop but is `#[ignore]`-marked because its timing is dominated by the
  host filesystem rather than anything spg controls. The 400 ms budget
  is sized for macOS Tahoe APFS reality (~120 ms / iter) with ~3×
  headroom; release-process invokes it manually as a cold-cache smoke
  test.
