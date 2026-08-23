# Runtime env switches — the customer-facing ones

> Source of truth for every non-`SPG_TEST_` environment switch the
> engine, server or embedded host reads. `xtests/sigil/test-mode-gucs.md`
> does this for the test-only ones; this file does it for the switches a
> DEPLOYER can set, which are the ones that change what a customer sees.
>
> **Enforced.** `crates/spg-engine/tests/e2e/e2e_sigil_registry.rs` fails
> when the source reads a switch this table does not list. That test also
> enforces the test-mode index, whose own header has claimed a CI lint
> since it was written — nothing read the file until v7.38.17, and the
> claim was simply untrue. It had not drifted (8 symbols, 8 rows), so it
> cost nothing; it was one edit away from costing something.
>
> **`exercised` is measured AND asserted** (v7.38.18): it says whether
> the name appears OUTSIDE A COMMENT under `crates/*/tests`, `xtests`,
> `scripts` or `.github`, and `the_exercised_column_says_what_the_
> repository_does` fails when this table disagrees with the repository.
> `no` means nothing here ever sets it, so the switch's non-default path
> has never run in this tree. That is a statement about evidence, not
> about whether the switch works.
>
> v7.38.18 — a switch is a NAME and a decision, and the name is the
> half a test forgets. The four durability switches below were pinned
> by tests of their decisions that never mentioned which variable
> reaches one, and this column correctly went on reading `no`: a typo
> in `env::var("SPG_AUTOVAKUUM")` is silent, because the variable an
> operator sets is simply never found. Each call site reads a named
> const now, and the tests assert the const.
>
> Evidence inside a `#[cfg(test)]` module in `src` counts too, which
> is how the four PG-spelled aliases stopped reading `no`: the module
> that pins them, `env_knob_tests`, sits in the middle of
> `crates/spg-server/src/main.rs` rather than under `tests/`.
>
> Two words of that moved and both were load-bearing. **Asserted**: the
> column was hand-maintained prose, so it could say `yes` about a switch
> nothing ran and no one would learn — the same shape as the header
> below it claiming a CI lint that did not exist. **Outside a comment**:
> `e2e_timeouts.rs` opens with `//! - SPG_QUERY_TIMEOUT_MS: a
> long-running scan is cancelled`, and the test under it sets no such
> variable — it uses `SET statement_timeout`. The column read `yes` on
> the strength of a sentence. Four rows were wrong once measured: two
> claimed `yes` for a name that appears only in a doc comment, two
> claimed `no` for a switch that tests really do set.
>
> Today: **84 switches, 60 exercised, 24 not.**
> Adding a switch means adding a row in the same commit.

| switch | first read site | exercised |
|---|---|---|
| `SPG_ADDR` | `crates/spg-server/src/main.rs:851` | yes |
| `SPG_ADMIN_PASSWORD` | `crates/spg-server/src/main.rs:3370` | yes |
| `SPG_ADMIN_USER` | `crates/spg-server/src/main.rs:3376` | yes |
| `SPG_AUDIT` | `crates/spg-server/src/main.rs:854` | yes |
| `SPG_AUTOVACUUM` | `crates/spg-server/src/main.rs:1625` | yes |
| `SPG_AUTOVACUUM_NAPTIME` | `crates/spg-server/src/main.rs:1146` | yes |
| `SPG_AUTOVACUUM_NAPTIME_MS` | `crates/spg-server/src/autovacuum.rs:45` | yes |
| `SPG_AUTO_ANALYZE_INTERVAL_MS` | `crates/spg-server/src/main.rs:1146` | yes |
| `SPG_COMMIT_DELAY_US` | `crates/spg-server/src/wal.rs:660` | yes |
| `SPG_COMMIT_GROUP_MAX` | `crates/spg-server/src/wal.rs:645` | **no** |
| `SPG_COMMIT_TRACE` | `crates/spg-server/src/main.rs:301` | **no** |
| `SPG_COMPACTION_TARGET_SEGMENT_BYTES` | `crates/spg-server/src/commands.rs:273` | **no** |
| `SPG_COMPRESSION_MIN_BYTES` | `crates/spg-server/src/wal.rs:205` | **no** |
| `SPG_DATA_SYNC_RETRY` | `crates/spg-embedded/src/lib.rs:1616` | **no** |
| `SPG_DB` | `crates/spg-server/src/main.rs:853` | yes |
| `SPG_DISABLE_WAL_PREFLIGHT` | `crates/spg-server/src/main.rs:1700` | yes |
| `SPG_EMBEDDED_CHECKPOINT_BYTES` | `crates/spg-embedded/src/lib.rs:323` | **no** |
| `SPG_EMBEDDED_CHECKPOINT_SECONDS` | `crates/spg-embedded/src/lib.rs:336` | **no** |
| `SPG_FAIL_AUDIT_AT` | `crates/spg-server/src/main.rs:3299` | yes |
| `SPG_FAIL_FSYNC_AT` | `crates/spg-server/src/main.rs:1702` | yes |
| `SPG_FAIL_WAL_QUOTA_BYTES` | `crates/spg-server/src/main.rs:1699` | yes |
| `SPG_FAULT_RECOVERY_PAUSE_MS` | `crates/spg-server/src/wal.rs:1323` | yes |
| `SPG_FLUSHER_INTERVAL_US` | `crates/spg-server/src/flusher.rs:101` | yes |
| `SPG_FOLLOW_OF` | `crates/spg-server/src/main.rs:2114` | yes |
| `SPG_FREEZER_BATCH_ROWS` | `crates/spg-server/src/freezer.rs:72` | yes |
| `SPG_FREEZER_DISABLE` | `crates/spg-server/src/freezer.rs:66` | yes |
| `SPG_FREEZER_TICK_MS` | `crates/spg-server/src/freezer.rs:67` | yes |
| `SPG_FREEZER_WORKERS` | `crates/spg-server/src/freezer.rs:77` | yes |
| `SPG_HOT_TIER_BYTES` | `crates/spg-server/src/main.rs:1707` | yes |
| `SPG_HTTP_ADDR` | `crates/spg-server/src/main.rs:2086` | yes |
| `SPG_IDLE_TIMEOUT_SEC` | `crates/spg-server/src/main.rs:867` | yes |
| `SPG_LC_COLLATE` | `crates/spg-server/src/main.rs:2036` | yes |
| `SPG_LOG_FORMAT` | `crates/spg-server/src/observability.rs:148` | yes |
| `SPG_LOG_MIN_DURATION` | `crates/spg-server/src/main.rs:1150` | yes |
| `SPG_MATVIEW_TRACE` | `crates/spg-server/src/pgwire.rs:2432` | **no** |
| `SPG_MAX_CONNECTIONS` | `crates/spg-server/src/main.rs:861` | yes |
| `SPG_MAX_QUERY_BYTES` | `crates/spg-server/src/main.rs:864` | yes |
| `SPG_MAX_QUERY_NS` | `crates/spg-server/src/main.rs:866` | yes |
| `SPG_MAX_QUERY_ROWS` | `crates/spg-server/src/main.rs:862` | yes |
| `SPG_METRICS_TABLE_ALLOWLIST` | `crates/spg-server/src/observability.rs:525` | yes |
| `SPG_METRICS_TABLE_TOPN` | `crates/spg-server/src/observability.rs:534` | yes |
| `SPG_MVCC_INPLACE` | `crates/spg-server/src/main.rs:1606` | yes |
| `SPG_MYSQLWIRE_ADDR` | `crates/spg-server/src/main.rs:2073` | yes |
| `SPG_OPEN_PATH_LOG` | `crates/spg-embedded-tokio/src/lib.rs:190` | **no** |
| `SPG_OPEN_PATH_TIMING` | `crates/spg-embedded/src/lib.rs:1340` | **no** |
| `SPG_PARALLEL` | `crates/spg-server/src/main.rs:1636` | **no** |
| `SPG_PASSWORD` | `crates/spg-server/src/main.rs:859` | yes |
| `SPG_PGWIRE_TIMING` | `crates/spg-server/src/pgwire.rs:2508` | **no** |
| `SPG_PGWIRE_TRACE` | `crates/spg-server/src/pgwire.rs:2528` | **no** |
| `SPG_PG_ADDR` | `crates/spg-server/src/main.rs:2059` | yes |
| `SPG_PITR_ARCHIVE_CMD` | `crates/spg-embedded/src/lib.rs:1296` | yes |
| `SPG_PITR_RETENTION_CHECK_SEC` | `crates/spg-embedded/src/lib.rs:1288` | **no** |
| `SPG_PITR_RETENTION_HOURS` | `crates/spg-embedded/src/lib.rs:1281` | **no** |
| `SPG_PLAN_CACHE_MAX` | `crates/spg-server/src/main.rs:1782` | **no** |
| `SPG_PREFETCH_WORKERS` | `crates/spg-server/src/prefetch.rs:39` | yes |
| `SPG_PRELOAD_COLD_SEGMENT` | `crates/spg-server/src/main.rs:1171` | yes |
| `SPG_PUBSUB_SUBJECT` | `crates/spg-server/src/pubsub.rs:71` | **no** |
| `SPG_PUBSUB_TARGET` | `crates/spg-server/src/pubsub.rs:59` | **no** |
| `SPG_QUERY_TIMEOUT_MS` | `crates/spg-server/src/main.rs:865` | yes |
| `SPG_REPLAY_HEARTBEAT_MS` | `crates/spg-embedded/src/lib.rs:1329` | **no** |
| `SPG_REPLAY_UPTO` | `crates/spg-server/src/main.rs:1967` | yes |
| `SPG_REPL_ADDR` | `crates/spg-server/src/main.rs:2097` | yes |
| `SPG_REQUIRE_TLS` | `crates/spg-server/src/pgwire.rs:1639` | yes |
| `SPG_SEGMENT_COMPRESSION` | `crates/spg-server/src/freezer.rs:388` | **no** |
| `SPG_SHUTDOWN_DEADLINE_SEC` | `crates/spg-server/src/main.rs:898` | yes |
| `SPG_SLOW_QUERY_LOG_MS` | `crates/spg-server/src/main.rs:892` | yes |
| `SPG_SLOW_QUERY_THRESHOLD_MS` | `crates/spg-server/src/main.rs:1150` | yes |
| `SPG_SQLX_INLINE_BUDGET_MS` | `crates/spg-sqlx/src/connection.rs:415` | yes |
| `SPG_STATEMENT_TIMEOUT` | `crates/spg-server/src/main.rs:1141` | yes |
| `SPG_SYNCHRONOUS_COMMIT` | `crates/spg-server/src/wal.rs:56` | yes |
| `SPG_TEMP_DIR` | `crates/spg-server/src/tempstore.rs:31` | yes |
| `SPG_TLS_CERT` | `crates/spg-server/src/mysqlwire.rs:442` | yes |
| `SPG_TLS_KEY` | `crates/spg-server/src/mysqlwire.rs:443` | yes |
| `SPG_WAL` | `crates/spg-server/src/main.rs:855` | yes |
| `SPG_WAL_COMPRESSION` | `crates/spg-server/src/wal.rs:226` | yes |
| `SPG_WAL_FULLFSYNC` | `crates/spg-server/src/wal.rs:470` | yes |
| `SPG_WAL_HASH` | `crates/spg-embedded/src/lib.rs:1940` | **no** |
| `SPG_WAL_LEVEL` | `crates/spg-server/src/main.rs:2550` | yes |
| `SPG_WAL_MIN_FREE_BYTES` | `crates/spg-server/src/main.rs:897` | yes |
| `SPG_WAL_ROW_REDO` | `crates/spg-embedded/src/lib.rs:316` | yes |
| `SPG_WAL_TEE_PATH` | `crates/spg-server/src/wal.rs:610` | yes |
| `SPG_WAL_TRACE` | `crates/spg-server/src/wal.rs:406` | **no** |
| `SPG_WAL_WRITER_DELAY_MS` | `crates/spg-embedded/src/lib.rs:1272` | **no** |
| `SPG_WARM_UP_COLD_BUDGET_MS` | `crates/spg-embedded/src/lib.rs:180` | **no** |
