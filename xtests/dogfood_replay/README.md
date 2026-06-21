# xtests/dogfood_replay — dogfood-replay testbed

Every customer prod incident gets permanently encoded here as a
SPG-internal regression gate. Each subdirectory under `fixtures/`
is one incident.

## Adding a new fixture

1. Create `fixtures/<source>-<date>-<slug>/` (e.g.
   `fixtures/mailrs-2026-06-22-content-worker/`).
2. Write `fixture.json` per the format documented at the top of
   `src/fixture.rs`.
3. If the fixture needs a prod-shape catalog, drop the tarball as
   `snapshot.tar.gz` (gitignored) and record its SHA-256 in
   `fixture.json`.
4. Write `queries.sql` (or `scenario` steps for recovery fixtures).
5. Pin a budget in `fixture.json.expected.*`.

## Running

```sh
cargo run --release -p spg-dogfood-replay -- list
cargo run --release -p spg-dogfood-replay -- verify
cargo run --release -p spg-dogfood-replay -- all --fast
cargo run --release -p spg-dogfood-replay -- run --fixture mailrs-2026-06-22-content-worker
```

`gate.sh dogfood` runs the `all --fast` shape; the full tier
drops `--fast` and runs every fixture including those that need
the >100 MB prod snapshots (CI-only when the snapshot store is
mounted).

## Fixture kinds

| `type`               | What it does                                                              |
| -------------------- | ------------------------------------------------------------------------- |
| `query`              | Loads snapshot, runs SQL N iters, p50/p95/p99 vs. budget                  |
| `lock-hang-recovery` | Drives an `open → dirty mutate → drop-handle → reopen` and times recovery |
| `wal-replay-bounded` | Synthesises a fresh catalog, builds N indices, runs M DELETEs, reopens    |

See the design note at
`.claude/notes/v7.37.5-dogfood-replay-framework-design.md` for
the full rationale.
