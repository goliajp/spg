    Finished `release` profile [optimized] target(s) in 0.13s
     Running `/Volumes/INTEL2T/workspace-cache/cargo-target/release/repl_bench`
# v4.24 replication bench

## INSERT throughput on primary

- solo (no follower)     : 2000 rows in 8125.72ms = 246 rows/s

## Snapshot bootstrap latency (follower → caught up to N seed rows)

- baseline 1000 rows visible on follower in 4ms (wall = 240ms)

## INSERT throughput with follower attached

- with follower          : 2000 rows in 11165.19ms = 179 rows/s
- attach cost vs solo    : +27.2% throughput

## Replication lag (primary commit → follower visible)

- samples : 200
- p50     : 53334 µs
- p95     : 119572 µs
- p99     : 210869 µs
- max     : 241247 µs
