# SPG Tunables — Environment Variables

> v7.37.25 (25.4) — complete reference for every `SPG_*`
> environment variable the engine, storage, and server crates
> read at start-up. Default values are listed where the engine
> falls back to a hard-coded value when the variable is unset.

Variable names follow PG GUC conventions where possible
(`SPG_SYNCHRONOUS_COMMIT` mirrors `synchronous_commit`,
`SPG_WAL_COMPRESSION` mirrors `wal_compression`, etc.). 25.5
will close the remaining gaps so every SPG tunable maps
one-to-one to a PG GUC with the matching unit-of-measure.

## Network & access

| Variable                | Default              | Purpose |
|-------------------------|----------------------|---------|
| `SPG_ADDR`              | `0.0.0.0:5432`       | Generic listen address; alias of `SPG_PG_ADDR` when only one wire protocol is enabled. |
| `SPG_PG_ADDR`           | `0.0.0.0:5432`       | PostgreSQL wire-protocol listen address. |
| `SPG_MYSQLWIRE_ADDR`    | unset (disabled)     | MySQL wire-protocol listen address. Set to enable the second wire. |
| `SPG_MYSQLWIRE_TLS_*`   | unset                | TLS material for the MySQL wire — cert/key/ca path triplet. |
| `SPG_HTTP_ADDR`         | unset (disabled)     | HTTP admin/metrics listen address. |
| `SPG_REPL_ADDR`         | unset                | Logical-replication subscriber endpoint (when SPG is the publisher target). |
| `SPG_ADMIN_USER`        | `admin`              | Bootstrap admin user name. |
| `SPG_ADMIN_PASSWORD`    | (random)             | Bootstrap admin password. When unset, SPG generates one and prints it to stderr at first boot. |
| `SPG_PASSWORD`          | unset                | Client-side credential helper for spgctl. |
| `SPG_DB`                | `spg`                | Default database name advertised at handshake. |
| `SPG_FOLLOW_OF`         | unset                | Upstream URI for cascading replication. |
| `SPG_PUBSUB_SUBJECT`    | unset                | Logical pub/sub topic name. |
| `SPG_PUBSUB_TARGET`     | unset                | Logical pub/sub downstream URI. |

## Connection limits

| Variable                | Default | Purpose |
|-------------------------|---------|---------|
| `SPG_MAX_CONNECTIONS`   | `200`   | Per-process pgwire client cap. |
| `SPG_IDLE_TIMEOUT_SEC`  | `0` (off) | Idle connection reap timeout. |
| `SPG_SHUTDOWN_DEADLINE_SEC` | `30` | Grace period before SIGTERM-induced fast shutdown. |

## Query limits & timing

| Variable                  | Default | Purpose |
|---------------------------|---------|---------|
| `SPG_QUERY_TIMEOUT_MS`    | `0` (off) | Hard wall-clock cap per query (matches PG `statement_timeout`). |
| `SPG_MAX_QUERY_NS`        | `0`     | Per-query CPU budget. |
| `SPG_MAX_QUERY_ROWS`      | `0`     | Maximum rows a single query may return (defensive). |
| `SPG_MAX_QUERY_BYTES`     | `0`     | Maximum raw bytes returned in a single rowset. |
| `SPG_SLOW_QUERY_THRESHOLD_MS` | `0` (off) | Emit a `slow_query` event when wall-clock exceeds the threshold. |
| `SPG_SLOW_QUERY_LOG_MS`   | `0`     | Same idea, log-stream variant. |
| `SPG_PLAN_CACHE_MAX`      | `1024`  | Plan-cache entry cap. |

## Storage tiering & freeze cycle

| Variable                  | Default     | Purpose |
|---------------------------|-------------|---------|
| `SPG_HOT_TIER_BYTES`      | `512 MiB`   | Per-table hot-tier byte budget. `ALTER TABLE … SET hot_tier_bytes` overrides at the table level. |
| `SPG_FREEZER_DISABLE`     | unset (on)  | Disables the background freezer (useful for debugging hot-tier perf). |
| `SPG_FREEZER_WORKERS`     | `cpu_count` | Number of freezer threads. |
| `SPG_FREEZER_TICK_MS`     | `200`       | Freezer poll interval. |
| `SPG_FREEZER_BATCH_ROWS`  | `2048`      | Rows per freeze batch. |
| `SPG_FREEZE_BATCH_ROWS`   | (alias of `SPG_FREEZER_BATCH_ROWS`) | Legacy spelling. |
| `SPG_COMPACTION_TARGET_SEGMENT_BYTES` | `64 MiB` | Cold-segment compaction target size. |
| `SPG_COMPRESSION_MIN_BYTES` | `1024`   | Don't bother compressing payloads smaller than this. |
| `SPG_SEGMENT_COMPRESSION` | `zstd`      | Compression codec for new cold segments. |
| `SPG_PREFETCH_WORKERS`    | `cpu_count` | Background prefetch threads for cold-tier reads. |
| `SPG_PRELOAD`             | unset       | Comma-separated table names to warm at startup. |
| `SPG_PRELOAD_COLD_SEGMENT` | unset      | Specific cold-segment IDs to pre-mmap at startup. |

## WAL & durability

| Variable                      | Default       | Purpose |
|-------------------------------|---------------|---------|
| `SPG_WAL`                     | (path)        | WAL directory. |
| `SPG_WAL_LEVEL`               | `replica`     | WAL detail (`minimal` / `replica` / `logical`). Matches PG `wal_level`. |
| `SPG_WAL_COMPRESSION`         | `off`         | WAL record compression. Matches PG `wal_compression`. |
| `SPG_WAL_MIN_FREE_BYTES`      | `1 GiB`       | Disk-space floor before WAL writes are refused. |
| `SPG_FAIL_WAL_QUOTA_BYTES`    | unset         | Test hook — fail writes once this cumulative byte total is reached. |
| `SPG_WAL_TEE_PATH`            | unset         | Additional WAL mirror destination (debug). |
| `SPG_WAL_ROW_REDO`            | `on` (default since v7.37.8) | Row-redo WAL path. **DO NOT** flip off in production — mailrs cascade 7 lessons. |
| `SPG_DISABLE_WAL_PREFLIGHT`   | unset         | Skip the WAL preflight estimator (test only). |
| `SPG_REPLAY_UPTO`             | unset         | Replay WAL only up to this LSN at recovery. |
| `SPG_COMMIT_DELAY_US`         | `0`           | Group-commit delay (matches PG `commit_delay`). |
| `SPG_COMMIT_GROUP_MAX`        | `200`         | Group-commit batch cap. |
| `SPG_FLUSHER_INTERVAL_US`     | `200`         | WAL flusher tick interval. |
| `SPG_SYNCHRONOUS_COMMIT`      | `on`          | Matches PG `synchronous_commit`; `off` skips fsync, `local` skips replica wait. |

## Auto-maintenance

| Variable                       | Default       | Purpose |
|--------------------------------|---------------|---------|
| `SPG_AUTO_ANALYZE_INTERVAL_MS` | `60_000` (60s)| Cadence of the host's autoanalyze loop (`Engine::autoanalyze_pass` — v7.37.22 (22.3)). Matches PG `autovacuum_naptime`. |

## Observability & audit

| Variable                    | Default | Purpose |
|-----------------------------|---------|---------|
| `SPG_AUDIT`                 | unset   | Audit-chain enable. |
| `SPG_LOG_FORMAT`            | `text`  | `text` / `json` log format. |
| `SPG_METRICS_TABLE_ALLOWLIST` | unset | Per-table metrics export allowlist. |
| `SPG_METRICS_TABLE_TOPN`    | `100`   | Top-N rows per metrics scan. |

## Test-only

These should NEVER be set in production. Most are gated to
release-build no-ops.

| Variable                          | Purpose |
|-----------------------------------|---------|
| `SPG_TEST_COMPUTE_QUERY_ID`       | Force deterministic queryid hashing for pg_stat_statements regression. |
| `SPG_TEST_DISABLE_JOINFOLD`       | Suppress join-fold rewrites for plan-stability tests. |
| `SPG_TEST_DISABLE_TOPK`           | Suppress the streamed top-K aggregator. |
| `SPG_TEST_EXPLAIN_NO_COSTS`       | Strip wall-clock annotations from EXPLAIN output (regression-diff friendly). |
| `SPG_TEST_PLAN_DETERMINISTIC`     | Force deterministic plan ordering across runs. |
| `SPG_TEST_RANDOM_SEED`            | Seed for any test-side RNG. |
| `SPG_TEST_STATS_FROZEN`           | Treat ANALYZE as a no-op so the statistic-version snapshot stays stable. |

## PG GUC alignment (25.5 follow-up)

25.5 will close the remaining naming gaps so every SPG-specific
tunable maps 1-1 to a PG GUC:

| Today                              | Future (PG-aligned)        |
|------------------------------------|----------------------------|
| `SPG_QUERY_TIMEOUT_MS`             | `SPG_STATEMENT_TIMEOUT`    |
| `SPG_AUTO_ANALYZE_INTERVAL_MS`     | `SPG_AUTOVACUUM_NAPTIME`   |
| `SPG_SLOW_QUERY_THRESHOLD_MS`      | `SPG_LOG_MIN_DURATION`     |
| `SPG_HOT_TIER_BYTES`               | (SPG-specific — keep)      |
| `SPG_FREEZER_*`                    | (SPG-specific — keep)      |

## Discovery

`spgctl tunables` (planned 22.10 / 23.8 follow-up) will print
the live values + their defaults + the source (`env` / `default`
/ `set in spg.conf`) so operators don't have to grep this doc
at runtime.

## Reference

- Engine `EnvCfg` struct: `crates/spg-engine/src/env_cfg.rs`
- Server config loader: `crates/spg-server/src/config.rs`
- PG GUC alignment doc (PG-only-spelled tunables): PG manual
  `Chapter 20. Server Configuration`.
