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
| `SPG_MAX_QUERY_BYTES`     | `268435456` (256 MiB) | Maximum raw bytes returned in a single rowset. Unlike the rest of this table, unset does NOT mean unlimited — the server applies 256 MiB and refuses a larger result with `query materialisation exceeded max_query_bytes=…`. Set it to `0` for no limit — **honoured from v7.37.13; before that the zero was dropped by the env reader and the 256 MiB default stayed in force**. |
| `SPG_SLOW_QUERY_THRESHOLD_MS` | `100` | Emit a `slow_query` event when wall-clock reaches the threshold. Rides PG's `log_min_duration_statement` scale: `-1` off, `0` reports every statement, `>0` is the floor in ms. **This table said `0` (off) through v7.37.15 — wrong on both counts.** The default has always been 100 ms, its zero has always reported everything, and `-1` did not parse: it fell back to the same 100 ms as unset, so before v7.37.16 there was no way to turn this off. |
| `SPG_SLOW_QUERY_LOG_MS`   | `1000`  | Same idea, log-stream variant. Also unset-is-not-off: queries over one second are logged unless this is set to `0` — **honoured from v7.37.16; between v7.37.7 and v7.37.15 the zero was dropped and the one-second default silently stayed on**. |
| `SPG_PLAN_CACHE_MAX`      | `256`   | Plan-cache entry cap. |

### A note on knobs whose zero means something

Most variables here read "unset" and "0" the same way, and their reader
drops a zero on purpose — for a timeout or an interval, zero and absent do
mean the same thing.

Three do not, and no two of them agree:

- `SPG_MAX_QUERY_BYTES` and `SPG_SLOW_QUERY_LOG_MS` — zero turns the feature
  OFF, and the default is ON. Both were documented that way for versions
  before they behaved that way, because both went through the zero-dropping
  reader; both now use `parse_env_u64_allow_zero`.
- `SPG_SLOW_QUERY_THRESHOLD_MS` — zero reports EVERY statement, and the off
  switch is `-1`. This is PG's `log_min_duration_statement` scale, and the
  reason it is worth the asymmetry: an operator moving a PG config across
  should not have the same number mean the opposite thing.

Each is pinned separately in `main.rs`'s `env_knob_tests` — the two slow-query
knobs in particular sit one line apart in this table and mean opposite things
by zero, so a shared pin would just encode whichever one was written second.

The audit that found the third was mechanical, and worth repeating when a
knob is added: grep this table for the variables whose documented default is
`0`, then check what each one's reader does with an explicit zero. Most of
them are safe for a boring reason — their default already IS off, so a
dropped zero lands on the same behaviour. The bugs are only ever in the ones
whose default is not zero.

If a new knob's zero is a setting rather than an absence, it needs the
zero-preserving reader — and for a threshold, decide which way its zero
points before wiring it, because `Some(0)` fires the comparison on
everything and `None` fires it on nothing.

## Storage tiering & freeze cycle

| Variable                  | Default     | Purpose |
|---------------------------|-------------|---------|
| `SPG_HOT_TIER_BYTES`      | `512 MiB`   | Per-table hot-tier byte budget. `ALTER TABLE … SET hot_tier_bytes` overrides at the table level. |
| `SPG_FREEZER_DISABLE`     | unset (on)  | Disables the background freezer (useful for debugging hot-tier perf). |
| `SPG_FREEZER_WORKERS`     | `cpu_count` | Number of freezer threads. |
| `SPG_FREEZER_TICK_MS`     | `1000`      | Freezer poll interval. Values below 10 ms are ignored. |
| `SPG_FREEZER_BATCH_ROWS`  | `1000`      | Rows per freeze batch. |
| `SPG_COMPACTION_TARGET_SEGMENT_BYTES` | `64 MiB` | Cold-segment compaction target size. |
| `SPG_COMPRESSION_MIN_BYTES` | `256`    | Don't bother compressing payloads smaller than this. |
| `SPG_SEGMENT_COMPRESSION` | `zstd`      | Compression codec for new cold segments. |
| `SPG_PREFETCH_WORKERS`    | `cpu_count` | Background prefetch threads for cold-tier reads. |
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
| `SPG_FLUSHER_INTERVAL_US`     | `200`         | **Server** async-commit flusher tick interval (µs). Minimum 10 µs; values below are clamped. Only consulted when `SPG_SYNCHRONOUS_COMMIT=off`. |
| `SPG_WAL_WRITER_DELAY_MS`     | `200`         | **Embedded** async-commit flusher tick interval (ms). Matches PG `wal_writer_delay`. Must be `> 0`; unset/0 falls back to 200. Only consulted when `SPG_SYNCHRONOUS_COMMIT=off`. |
| `SPG_SYNCHRONOUS_COMMIT`      | `on`          | Matches PG `synchronous_commit`; `off` skips the per-commit fsync (bounded-loss async mode — see below), `local` skips replica wait. |

### Durability / crash-loss window

| Mode | Bound | Loss on crash (SIGKILL / power-cut) |
|------|-------|--------------------------------------|
| **`SPG_SYNCHRONOUS_COMMIT=on` (DEFAULT)** | — | **Nothing.** Every acknowledged commit is `fsync`ed before the client/caller is told it committed. Server: `crates/spg-server/src/wal.rs:384` + `:791` (`f.sync_data()` on the commit path). Embedded: `crates/spg-embedded/src/lib.rs:1057-1062` (`WalTicket::wait` → `group.wait_flushed`, blocks until fsynced). |
| **`SPG_SYNCHRONOUS_COMMIT=off` (server / pgwire)** | ≤ one `SPG_FLUSHER_INTERVAL_US` (default **200 µs**) | Up to one flusher interval of acknowledged-but-unflushed commits. A background flusher fsyncs and emits a `durability_checkpoint` marker every interval; only WAL bytes appended since the last marker can be lost. Bound in code: `crates/spg-server/src/flusher.rs:42,46`. |
| **`SPG_SYNCHRONOUS_COMMIT=off` (embedded)** | ≤ one `SPG_WAL_WRITER_DELAY_MS` (default **200 ms**) | Up to one flusher interval of acknowledged-but-unflushed commits. A background thread calls `flush_now()` (write + fsync of the pending group buffer) every interval. Bound in code: `crates/spg-embedded/src/lib.rs:1201-1210` (cadence) + `:3074-3086` (loop). |

Async mode never loses a commit across a **clean** shutdown: `Drop`/`CHECKPOINT` always force a final flush (embedded `lib.rs:4841-4860`; server `CHECKPOINT` fsyncs regardless of the knob). The loss window above applies **only** to an abrupt kill between two flusher ticks. Operators can shrink an async window by lowering the interval at the cost of more fsyncs/sec; the default (sync) mode has no window at all.

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

## PG GUC alignment (25.5 — shipped)

PG-spelled aliases are live for the three keys that map 1-1 to a
named GUC. When both spellings are exported, the PG-spelled alias
wins on the assumption that the operator wrote the PG-style name
deliberately. Unit-of-measure stays the same as the legacy key
(no unit-suffix parsing — `'5s'` still parses as 5 not 5000).

| Legacy SPG name                    | PG-aligned alias (preferred) |
|------------------------------------|------------------------------|
| `SPG_QUERY_TIMEOUT_MS`             | `SPG_STATEMENT_TIMEOUT`      |
| `SPG_AUTO_ANALYZE_INTERVAL_MS`     | `SPG_AUTOVACUUM_NAPTIME`     |
| `SPG_SLOW_QUERY_THRESHOLD_MS`      | `SPG_LOG_MIN_DURATION`       |
| `SPG_HOT_TIER_BYTES`               | (SPG-specific — keep)        |
| `SPG_FREEZER_*`                    | (SPG-specific — keep)        |

Resolution path: `crates/spg-server/src/main.rs::env_resolve`. Add
a new alias as one line in the `ALIASES` table; downstream readers
inherit it automatically.

## How a boolean switch is read (v7.38.18)

`0` / `off` / `false` / `no` are off; `1` / `on` / `true` / `yes` are
on; case does not matter and a blank value means *nothing was said*, so
each switch keeps its own default rather than inheriting one.

Before v7.38.18 SPG read a boolean **two ways**. `SPG_AUTOVACUUM` took
`0`, `false` or `off`; `SPG_WAL_FULLFSYNC`, `SPG_PGWIRE_TIMING`,
`SPG_PGWIRE_TRACE` and `SPG_FREEZER_DISABLE` took only `0`, so every
other spelling — the word `false` included — meant ON. An operator
writing `SPG_FREEZER_DISABLE=false`, meaning *do not disable it*,
disabled it. One reader now, with PostgreSQL's spellings.

Three switches still read their own way and are marked in the tables
below: `SPG_DATA_SYNC_RETRY` is on only for the exact word `on`,
`SPG_SEGMENT_COMPRESSION` is off only for the word `none`, and
`SPG_WAL_HASH` names a scheme rather than a boolean.

## Missing from this page until v7.38.18

These thirty-one were read by shipped code and documented nowhere. The
tables below now carry them; this note stays so the gap is on the
record rather than quietly closed.

### Access and transport

| Env | Default | Effect |
|---|---|---|
| `SPG_HBA_FILE` | unset | A `pg_hba.conf`-format file. First matching line decides; a file that will not parse **refuses the start**. See `PG_MIGRATION.md`. |
| `SPG_REQUIRE_TLS` | off | Refuse a plaintext pg-wire connection (SQLSTATE `08P01`). |
| `SPG_TLS_CERT` / `SPG_TLS_KEY` | unset | PEM paths. Both or neither. |
| `SPG_PG_ADDR` | unset | `host:port` for the PostgreSQL wire. Unset means the wire is not started. |

### Collation and text

| Env | Default | Effect |
|---|---|---|
| `SPG_LC_COLLATE` | unset | The collation a NEW database is created with, ahead of `LC_ALL` / `LC_COLLATE` / `LANG`. Set once, at creation; it cannot be changed afterwards, exactly as in PostgreSQL. Absent leaves `C`. |

### Durability and WAL

| Env | Default | Effect |
|---|---|---|
| `SPG_WAL_FULLFSYNC` | off | macOS: force `F_FULLFSYNC` instead of `F_BARRIERFSYNC`. Measured at r702-SW: 433 ms against 43 ms. |
| `SPG_WAL_HASH` | `crc32c` | `blake3` selects the stronger WAL frame hash. **Not a boolean** — it names a scheme. |
| `SPG_WAL_TRACE` | off | Per-frame WAL tracing. |
| `SPG_DATA_SYNC_RETRY` | off | Retry a failed data fsync instead of failing the write. **Reads only the exact word `on`.** |
| `SPG_WAL_WRITER_DELAY_MS` | unset (off) | Group-commit delay for the WAL writer; a zero is ignored. |
| `SPG_COMMIT_TRACE` | off | Per-commit tracing. |
| `SPG_FAULT_RECOVERY_PAUSE_MS` | `0` | Pause injected in the recovery path; a test aid that ships enabled. |

### Storage and segments

| Env | Default | Effect |
|---|---|---|
| `SPG_TEMP_DIR` | the OS temp dir | Where a spilling sort writes its runs. A blank value falls back to the OS directory rather than the empty path. |
| `SPG_COMPACTION_TARGET_SEGMENT_BYTES` | `COMPACTION_TARGET_DEFAULT_BYTES` | Target size a compaction aims for. |
| `SPG_COMPRESSION_MIN_BYTES` | `WAL_COMPRESS_MIN_BYTES` | Below this, a WAL record is stored uncompressed. |
| `SPG_SEGMENT_COMPRESSION` | on | `none` turns cold-segment compression off. **Reads only the word `none`.** |
| `SPG_WARM_UP_COLD_BUDGET_MS` | unset (no budget) | Milliseconds spent pre-loading cold segments at open; `0` disables the warm-up. |

### Embedded host

| Env | Default | Effect |
|---|---|---|
| `SPG_EMBEDDED_CHECKPOINT_BYTES` | adaptive | WAL bytes between checkpoints; a zero is ignored. Setting it turns adaptive mode OFF. |
| `SPG_EMBEDDED_CHECKPOINT_SECONDS` | adaptive | The time half of the same. |
| `SPG_REPLAY_HEARTBEAT_MS` | `5000` | How often a long replay reports progress. |
| `SPG_OPEN_PATH_LOG` / `SPG_OPEN_PATH_TIMING` | off | Trace and time `Database::open_path`. |
| `SPG_SQLX_INLINE_BUDGET_MS` | `1000` | Budget for the `spg-sqlx` inline path before it hands off. |

### PITR

| Env | Default | Effect |
|---|---|---|
| `SPG_PITR_RETENTION_HOURS` | `0` (off) | Delete WAL chunks older than this. |
| `SPG_PITR_RETENTION_CHECK_SEC` | `60` | How often the retention sweep wakes. |
| `SPG_PITR_ARCHIVE_CMD` | unset | Command run per chunk before deletion; a non-zero exit records `archive=FAILED` and keeps the chunk. |

### Maintenance and planner

| Env | Default | Effect |
|---|---|---|
| `SPG_AUTOVACUUM` | on | Turning it off leaves dead rows unreclaimed. |
| `SPG_AUTOVACUUM_NAPTIME_MS` | see `SPG_AUTO_ANALYZE_INTERVAL_MS` | The SPG-spelled twin of the PG name. |
| `SPG_PARALLEL` | on | Parallel execution. Reads `0`/`false`/`off`. |
| `SPG_MVCC_INPLACE` | on | In-place MVCC. Reads `0`/`false`/`off`. |
| `SPG_PLAN_CACHE_MAX` | engine default | Plan-cache entry cap; an unparseable value is ignored. |
| `SPG_MATVIEW_TRACE` | off | Materialised-view maintenance tracing. |
| `SPG_PGWIRE_TRACE` / `SPG_PGWIRE_TIMING` | off | Per-message trace and timing on the PG wire. |
| `SPG_PUBSUB_SUBJECT` / `SPG_PUBSUB_TARGET` | built-in defaults | WAL pub/sub routing. |

### Fault injection (ships enabled — these are not `SPG_TEST_`)

| Env | Default | Effect |
|---|---|---|
| `SPG_FAIL_FSYNC_AT` / `SPG_FAIL_AUDIT_AT` | unset | Fail the Nth fsync or audit append. Used by the crash matrix; they are named here because a deployer can set them, which makes them part of the interface. |

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
