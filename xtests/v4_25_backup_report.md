    Finished `release` profile [optimized] target(s) in 0.23s
     Running `/Volumes/INTEL2T/workspace-cache/cargo-target/release/backup_bench`
# v4.25 backup bench

## Seeded 100000 rows

- WAL size after seed : 1470 KiB

## Full backup

- bundle path         : /var/folders/l2/3q16m_1x20l36zbcb67gpgtc0000gn/T/spg-backup-bench-1779840428793168000/full.bkp
- bundle size         : 878 KiB
- wal_pos captured    : 1505836
- elapsed             : 5 ms
- bandwidth           : 175.4 MiB/s

## Incremental backup (10000 new rows since SINCE=1505836)

- bundle size         : 168 KiB
- wal_pos captured    : 1678636
- elapsed             : 4 ms
- bandwidth           : 40.2 MiB/s

## Restore round-trip

- bundle apply time   : 4 ms
- server startup time : 261 ms
- restored row count  : 110000 (expected 110000)

## PITR (SPG_REPLAY_UPTO truncation)

- full WAL replay startup        : 118 ms (rows=110000)
- SPG_REPLAY_UPTO=0 startup      : 146 ms (rows=100000)
                                    expected baseline 100000 + incr 10000 = 110000, then truncated to 100000
