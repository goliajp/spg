# Changelog

Format: [Keep a Changelog](https://keepachangelog.com).
Versions follow SemVer; pre-v1.0 contract — minor bumps may
include compatible additions to the wire protocol and SQL
surface, never breaking changes within v4.x.

The most recent commit on `master` is the source of truth for
the current build; this file is a release-organized view.

---

## [4.36.0] — 2026-05-27 (replication netsplit chaos + lag metric — `SPGREPL\x02`)

### Wire protocol — new minor version `SPGREPL\x02` (backwards-compat)

The master now speaks two negotiable replication wire versions on
`SPG_REPL_ADDR`; the follower picks via the handshake magic byte:

- `SPGREPL\x01` (v4.24) — raw WAL byte stream. Unchanged.
- `SPGREPL\x02` (v4.36) — **framed** stream: `[u8 type][u32 LE
  len][payload]`. Type `0x00` = WAL chunk (payload bytes feed the
  follower's record accumulator just like v1). Type `0x01` =
  status frame, payload `[u64 LE primary_wal_pos][u64 LE
  wall_time_us]`.

New followers always send the v2 magic; old `\x01` followers
keep working with old behavior. STABILITY.md §"Replication
protocol" pins both versions.

### Added
- **Status-frame protocol extension** in `crates/spg-server/src/
  replication.rs`: master emits a status frame at least every
  50 ms whether or not there's WAL activity. Follower parses it,
  stores into `LagState` (three atomics on the new
  `ServerState::lag_state` field).
- **Replication lag series** in `/metrics`:
  `spg_replication_lag_bytes` (primary_pos − follower_applied_pos)
  + `spg_replication_lag_seconds` (now − master's wall time).
  Omitted on the primary and on a v1 follower (no status frame
  seen) so Prometheus doesn't reify a misleading zero.
- **Netsplit chaos test** in `tests/e2e_chaos_netsplit.rs`:
  - In-test TCP proxy (stdlib only — `TcpListener` + `TcpStream`)
    that supports a kill-switch flipped from the test thread.
  - `netsplit_disconnect_then_heal_resyncs_without_loss_or_dup`
    spins up primary + follower behind the proxy, cuts the proxy
    mid-write, lets the master keep writing, restores the proxy.
    Asserts row count *and* row sum match exactly — no dup, no
    gap. Closes PROD_READY row 2.9.
  - `follower_metrics_expose_replication_lag_after_status_frame`
    confirms both lag series land on the follower's `/metrics`.
    Closes PROD_READY row 4.7.
- `prod_ready.rs::row_2_9_*` and `row_4_7_*` machine rows.

### Changed
- STABILITY.md §"Frozen surfaces" gains a "Replication protocol"
  section pinning both v1 and v2 wire layouts plus the forward-
  compat rule (followers MUST tolerate unknown frame types and
  unknown payload sizes on known types).
- PROD_READY.md audit snapshot: 73 → 75 ✅ / 5 → 4 ⚠️ / 1 → 0 ❌;
  [machine] rows 35 → 37.

### Test verification
  cargo test --release --workspace                              # all green
  cargo clippy --workspace --all-targets -- -D warnings         # 0 warnings
  cargo fmt --all -- --check                                    # clean

## [4.35.0] — 2026-05-27 (per-table metrics — `spg_table_rows` / `spg_table_bytes` + cardinality cap)

### Added
- `spg_table_rows{table=…}` and `spg_table_bytes{table=…}`
  gauges in `/metrics`. Rows is the live row count; bytes is a
  schema-width × row-count estimate (variable-width types pick
  a defensible average — Text/JSON = 64 B, half-full Varchar,
  etc.). Closes PROD_READY row 4.6.
- `SPG_METRICS_TABLE_TOPN` (default 50) — when no explicit
  allowlist is set, only the N largest tables by row count are
  exported. Keeps Prometheus cardinality bounded for tenants
  with thousands of tables.
- `SPG_METRICS_TABLE_ALLOWLIST=t1,t2,...` — exact list mode for
  operators who want explicit per-table control.
- `tests/e2e_table_metrics.rs` — three e2e tests cover default
  top-N, allowlist filtering, and the cardinality cap.
- `prod_ready.rs::row_4_6_*` machine row.

### Changed
- PROD_READY.md audit snapshot: 72 → 73 ✅ / 2 → 1 ❌;
  [machine] rows 34 → 35.
- DEPLOYMENT.md env-var table gains both new entries.

### Test verification
  cargo test --release --workspace                              # all green
  cargo clippy --workspace --all-targets -- -D warnings         # 0 warnings
  cargo fmt --all -- --check                                    # clean

## [4.34.0] — 2026-05-27 (ENOSPC in-memory rollback — auto-commit BEGIN..COMMIT wrap)

### Added
- **Implicit BEGIN..COMMIT wrap for auto-commit writes** —
  when WAL is on and the statement is not a TX-control verb,
  the dispatch path now wraps the engine mutation in an
  implicit `BEGIN` / `COMMIT`. The whole `[BEGIN, sql, COMMIT]`
  triple lands in the WAL with **one** `write_all` + **one**
  `fsync` via the new `append_wal_atomic_block` helper. On WAL
  append failure the dispatcher issues `ROLLBACK` and the
  engine reverts — live in-memory state never reflects a write
  whose WAL append didn't make it to disk. Closes PROD_READY
  row 1.11 fully.
- `tests/e2e_chaos.rs::chaos_disk_full_no_preflight_rolls_back_in_memory_to_match_durable_state`
  — exercises the path through real `append_wal*` failure by
  disabling the v4.30 preflight (`SPG_DISABLE_WAL_PREFLIGHT`).
  Asserts live count == CC'd count both pre- and post-restart
  (no phantom rows in either window).
- `tests/slo_smoke.rs::slo_wal_insert_p99_under_budget` —
  WAL-on perf gate for the wrap. Ceiling 50 ms (loose to absorb
  APFS / ext4 journaling variance; baseline ~20 ms on local
  APFS); catches gross regressions in the wrap (extra catalog
  clones, missed batched fsync) without false-alarming on
  shared-runner I/O noise.
- `SPG_DISABLE_WAL_PREFLIGHT` env var (test-only) to bypass the
  v4.30 dispatch-time chaos preflight and force the real
  append-side failure path.
- `prod_ready.rs::row_1_11_*` machine row.

### Changed
- WAL append path: `append_wal` (single-statement, single fsync)
  is kept for in-TX writes; new `append_wal_atomic_block`
  multi-statement variant for the implicit-wrap path.
- v4.30 preflight quota check now sizes for the full
  `[BEGIN, sql, COMMIT]` block when the wrap is active.
- PROD_READY.md audit snapshot: 71 → 72 ✅ / 6 → 5 ⚠️;
  [machine] rows 33 → 34.

### Test verification
  cargo test --release --workspace                              # all green
  cargo clippy --workspace --all-targets -- -D warnings         # 0 warnings
  cargo fmt --all -- --check                                    # clean

## [4.33.0] — 2026-05-27 (ops three-pack — graceful shutdown + slow-query log + disk water-mark)

### Added
- **Graceful shutdown** — SIGTERM/SIGINT installs a handler that
  flips a global flag; the main accept loop polls it between
  non-blocking accepts, then drains in-flight connections bounded
  by `SPG_SHUTDOWN_DEADLINE_SEC` (default 30 s, mirrors
  systemd's `DefaultTimeoutStopSec`). Exits 0 on clean drain.
  Closes PROD_READY row 2.7. e2e:
  `tests/e2e_graceful_shutdown.rs::graceful_shutdown_drains_inflight_and_refuses_new_conns_and_exits_zero`.
- **Slow-query log** — `SPG_SLOW_QUERY_LOG_MS` env var; queries
  whose dispatch wall-clock exceeds the threshold emit one
  `{"event":"slow_query","sql":...,"elapsed_us":N,"role":...,"threshold_us":N}`
  line on stderr. Field layout matches `SPG_LOG_FORMAT=json` so
  the same ingest pipeline handles both event streams. Default
  off. Closes PROD_READY row 4.5. e2e:
  `tests/e2e_slow_query_log.rs::slow_query_log_fires_above_threshold_and_silent_below`.
- **Disk water-mark** — `SPG_WAL_MIN_FREE_BYTES` env var; before
  every WAL append, `statvfs(2)` on the WAL volume; if free <
  threshold, returns `ErrorKind::StorageFull` with an error
  message that cites the env var by name. Reads keep serving
  (this is a write-path precheck only). macOS + Linux. Default
  off. Closes PROD_READY row 5.7. e2e:
  `tests/e2e_disk_watermark.rs::disk_watermark_refuses_writes_keeps_reads_keeps_server_alive`.
- `libc = "0.2"` direct dep on `spg-server` for the two FFI
  shims (`signal(2)` + `statvfs(2)`). Each call site is wrapped
  in `#[allow(unsafe_code)]` with a SAFETY note.
- `prod_ready.rs` rows `row_2_7_*` / `row_4_5_*` / `row_5_7_*`.

### Changed
- PROD_READY.md audit snapshot: 68 → 71 ✅ / 7 → 6 ⚠️ /
  4 → 2 ❌; 30 → 33 [machine] rows.
- DEPLOYMENT.md env-var table gains three rows.

## [4.30.0] — 2026-05-27 (ops docs suite + RESTORE_DRILL + in-memory rollback fix)

### Added
- `DEPLOYMENT.md` — install, file layout, env-var reference, ports.
- `RUNBOOK.md` — common alert → response mappings.
- `RESTORE_DRILL.md` — verbatim recovery commands, backed by
  `tests/e2e_restore_drill.rs` (CI gate).
- `SECURITY.md` — disclosure process, threat model, secret handling.
- `CHANGELOG.md` (this file).

### Changed
- Preflight WAL-quota check in the write path: when
  `SPG_FAIL_WAL_QUOTA_BYTES` would refuse an append, reject the
  SQL **before** `engine.execute` so the live in-memory state
  never reflects the rejected write. PROD_READY row 1.11 lit up
  green (chaos path).

## [4.29.0] — 2026-05-27 (chaos test infrastructure)

### Added
- `SPG_FAIL_WAL_QUOTA_BYTES` env var: chaos knob capping WAL
  file size, returns `ErrorKind::StorageFull` on overflow.
- `tests/e2e_chaos.rs` — three e2e chaos scenarios:
  - `kill -9 mid-write` recovery (real SIGKILL)
  - WAL tail truncation drop (length-prefixed records survive)
  - disk full mid-write returns clean error + survives restart
- Updated PROD_READY rows 1.9, 1.10, 9.5, 9.6 to ✅.

## [4.28.0] — 2026-05-27 (PROD_READY baseline + machine-checked gate)

### Added
- `PROD_READY.md` — 85 rows across 10 dimensions with judgment
  criteria + status + evidence links.
- `tests/prod_ready.rs` — meta-test asserts every `[machine]`
  row in PROD_READY.md has a paired `row_X_Y_*` test.
- 12 baseline machine-checked rows: WAL replay, /healthz,
  /metrics, max_connections, wire opcode freeze, perf gates
  present, CI workflow present, PERFORMANCE.md v4.27 baseline.
- New CI job `prod_ready gate`.

## [4.27.1] — 2026-05-27 (v4.x perf coverage)

### Added
- `xbench/competitor/src/bin/repl_bench.rs`,
  `xbench/competitor/src/bin/backup_bench.rs` — measure
  replication attach cost, snapshot bootstrap, lag distribution,
  full + incremental backup bandwidth, restore round-trip, PITR.
- PERFORMANCE.md §v4.27 / §v4.24 / §v4.25 numbers.

### Fixed
- `SPG_REPLAY_UPTO=0` is now accepted as a literal "skip all WAL"
  value (previously filtered out by `parse_env_u64`'s `n > 0`).

## [4.27.0] — 2026-05-27 (CI/CD)

### Added
- `.github/workflows/ci.yml` — fmt + clippy + test + audit jobs
  on every PR; release build + binary artifact on main pushes.

## [4.26.0] — 2026-05-27 (EXPLAIN)

### Added
- `EXPLAIN [ANALYZE] <select>` SQL — single-column `QUERY PLAN`
  output with operator label, index-seek detection, frame
  details, subquery markers. `ANALYZE` attaches actual rows +
  elapsed micros.

## [4.25.0] — 2026-05-27 (backup PITR + incremental)

### Added
- `BACKUP TO '<path>'` SQL — full backup (admin only).
- `BACKUP TO '<path>' INCREMENTAL SINCE N` SQL — WAL tail delta.
- `SPG_REPLAY_UPTO` env var — startup-time WAL replay truncation
  for point-in-time recovery.
- `crates/spg-server/src/backup.rs` — self-contained bundle format
  (magic `SPGBKUP\x01`).

## [4.24.0] — 2026-05-27 (WAL streaming replication)

### Added
- `SPG_REPL_ADDR` + `SPG_FOLLOW_OF` env vars — single-primary /
  multi-follower async replication.
- 16-byte handshake (`SPGREPL\x01` + start offset), then raw WAL
  byte stream (the on-disk WAL format itself).
- `crates/spg-server/src/replication.rs`.

## [4.23.0] — 2026-05-27 (correlated subqueries in WHERE)

### Added
- EXISTS / NOT EXISTS / scalar / IN subqueries can now reference
  outer columns. Two-stage: pre-eval fast path stays for the
  uncorrelated case; row-eval handles correlation by substituting
  outer columns into the inner SELECT.

## [4.22.0] — 2026-05-27 (WITH RECURSIVE)

### Added
- `WITH RECURSIVE` CTE — anchor + UNION ALL/DISTINCT recursive
  term. Column-rename syntax `WITH t(a, b) AS (…)`. Hard runaway
  cap (1M rows / 100K iter).

## [4.21.0] — 2026-05-27 (extended window functions)

### Added
- LAG / LEAD / FIRST_VALUE / LAST_VALUE / NTH_VALUE / NTILE /
  PERCENT_RANK / CUME_DIST window functions.

## [4.20.0] — 2026-05-27 (explicit window frames)

### Added
- `ROWS BETWEEN … AND …` and `RANGE BETWEEN … AND …` window
  frames, plus single-bound shorthand. RANGE is peer-aware
  (matches PG default for ordered windows).

## [4.19.0] — 2026-05-27 (SET / SHOW)

### Added
- Per-connection SET / SHOW for session variables. 14 known PG
  GUCs return sensible defaults; SET is accepted and round-trips
  to SHOW.

## [4.18.0] — 2026-05-27 (VACUUM / ANALYZE no-ops)

### Added
- `VACUUM` / `ANALYZE` / `CLUSTER` / `REINDEX` accept syntax,
  return clean `CommandComplete`. No actual reorg (SPG doesn't
  need it).

## [4.17.0] — 2026-05-26 (PG-wire COPY)

### Added
- `COPY <table> FROM STDIN` (text format) — full Copy{In,Out}
  protocol, CopyData / CopyDone / CopyFail framing.

## [4.16.0] — 2026-05-26 (v4.x soak audit)

### Added
- 5-minute mixed-workload soak harness
  (`xbench/competitor/src/bin/soak_v4.rs`); confirmed leak-free
  (post-warmup RSS drift 0.0%) across every v4.x code path.

## [4.15.0] — 2026-05-26 (pgbouncer compat)

### Added
- DISCARD ALL / TEMP / SEQUENCES / PLANS, RESET ALL / `<name>`,
  SET TRANSACTION — all as no-ops returning the expected tag.

## [4.14.0] — 2026-05-26 (JSON path operators)

### Added
- `->` and `->>` JSON path operators backed by a hand-rolled
  RFC 8259 parser (no external deps).

## [4.0.0] — [4.13.0] — 2026-05-26 (prod-readiness sprint)

The v4.0-v4.13 sprint, all on the same day:

- **v4.13** observability — `/healthz`, Prometheus `/metrics`,
  JSON logs (`SPG_LOG_FORMAT=json`).
- **v4.12** window functions — ROW_NUMBER / RANK / DENSE_RANK +
  partition-aware aggregates over OVER (PARTITION BY … ORDER BY …).
- **v4.11** WITH / CTE (non-recursive).
- **v4.10** uncorrelated scalar / EXISTS / IN subqueries.
- **v4.9** JSON column type (`Value::Json(String)`).
- **v4.8** PG-wire SCRAM-SHA-256 — self-built SHA-256 / HMAC /
  PBKDF2 in spg-crypto. NIST + RFC vectors pass.
- **v4.7** PG-wire extended-query — Parse / Bind / Describe /
  Execute / Close / Flush / Sync. JDBC / asyncpg / psycopg3 work.
- **v4.6** PG-wire pg_catalog subset — pg_class / pg_namespace /
  pg_database / pg_user / pg_tables synthesized.
- **v4.5** cooperative query cancellation + idle timeout —
  `SPG_QUERY_TIMEOUT_MS` watchdog + `SPG_IDLE_TIMEOUT_SEC` OS
  read timeout.
- **v4.4** UPDATE / DELETE — real DML.
- **v4.3** PG-wire compatibility shim (opt-in via `SPG_PG_ADDR`).
  psql / DBeaver / Metabase connect.
- **v4.2** resource limits — `SPG_MAX_CONNECTIONS`,
  `SPG_MAX_QUERY_ROWS`.
- **v4.1** multi-user + 3-role RBAC — admin / readwrite /
  readonly. BLAKE3(salt||password) hashing.
- **v4.0** concurrency — `RwLock<Engine>` read/write split.
  2× scaling at 8 threads on indexed PK lookups.

---

## v3.x — performance sprint (2026-05-26)

Pre-v4 push to take SPG from "correct" to "competitive".
End-state: spg-server scan 5.2× over PG/MySQL/MariaDB; spg-
embedded ANN 54× over pgvector. See PERFORMANCE.md for full
numbers.

- **v3.4** baseline series — binary size, RSS, large-data
  report, 15-min mixed soak, 10-min readonly soak (drift 0.2%).
- **v3.3** wire-batching (DataRowBatch op 0x17), TCP_NODELAY +
  write coalescing, NEON-vectorised L2 distance.
- **v3.2** competitor bench infrastructure
  (`xbench/competitor/` with docker-compose).
- **v3.1** index planner proof, ORDER BY LIMIT partial sort,
  catalog O(log n) sidecar, in-memory backup bench.
- **v3.0** 8-stone bench infra + BUDGETS.md + perf_gate.rs +
  HNSW build/search 15× speedup + dense row encoding (FILE_VERSION 8).

## v2.x — feature expansion (pre-perf)

- **v2.14** spg backup / restore CLI.
- **v2.13** multi-layer HNSW (FILE_VERSION 7).
- **v2.7-2.12** date/time / interval / TO_CHAR / DATE_PART / AGE.
- **v2.4-2.6** EXTRACT / DATE_TRUNC, HNSW inner-product +
  cosine, clock injection.
- **v2.2-2.3** HAVING + SHOW TABLES / COLUMNS, DATE / TIMESTAMP.
- **v2.0-2.1** HNSW kNN index, MySQL dialect (backticks,
  AUTO_INCREMENT).

## v1.x — conformance + auth (pre-vectors)

- **v1.14** Redis-style single-password AUTH.
- **v1.10-1.13** JOIN, NUMERIC, SAVEPOINT — duckdb + pg_regress
  to 100%.
- **v1.1-1.9** sqllogictest harness, BETWEEN, IN, LIKE,
  aggregates, GROUP BY, DISTINCT, UNION.
- **v1.0** operational basics — stats opcode, env paths, version.

## v0.x — foundation

`v0.1-v0.11` built the skeleton from scratch: workspace, wire
protocol, SQL lexer/parser, storage, expression evaluator,
catalog persistence, BLAKE3, B-tree index, transactions, WAL,
pgvector.

---

## Release process

For maintainers cutting a new release:

1. Update PROD_READY.md audit snapshot.
2. Add a top-section entry to this file (Added / Changed /
   Fixed / Removed / Security).
3. `cargo test --release --workspace` (must pass).
4. `cargo clippy --workspace --all-targets -- -D warnings`.
5. `cargo run --release -p sqllogictest --release` (4 corpora 100%).
6. Commit message: `vX.Y.Z: <one-line summary>`.
7. Tag: `git tag vX.Y.Z`.
8. Push: `git push --follow-tags`.

CI takes over from there: fmt + clippy + test + audit +
prod_ready gate; release build artifact uploaded.
