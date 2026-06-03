# SPG production-readiness — checklist + audit

This is the single source of truth for whether SPG is ready to put
in front of an external user. Each row carries a **judgment
criterion** (what "pass" means, in operational terms, not just
"feature exists"), a **status**, and an **evidence link** —
either a commit, a test, a bench report, or a doc.

The companion CI gate is `cargo test --release --test prod_ready`
in `crates/spg-server/tests/prod_ready.rs`. That test asserts the
machine-checkable subset of this file. Rows marked **[machine]**
are enforced by it; rows marked **[doc]** are reviewed manually.

Status legend:

- ✅ — pass, evidence linked
- ⚠️ — partial; gap is named and tracked
- ❌ — not done; on the roadmap
- 🚫 — intentional out-of-scope (linked to memory or rationale)

Last refresh: **v4.37 file format v9 + CRC32** (2026-05-27,
commit hash filled at commit time).

---

## 1. Data durability

Operator promise: a write that returned success is on stable
storage and survives a crash.

| # | Item | Criterion | Status | Evidence |
|---|------|-----------|:------:|----------|
| 1.1 | WAL fsync per commit | `append_wal()` calls `sync_data()` before the SQL handler returns CC. **In default sync-commit mode only.** v5.4 introduces opt-in async-commit (`SPG_SYNCHRONOUS_COMMIT=off`) which skips the per-write fsync; see row 1.12 for the bounded-loss contract. | ✅ | `crates/spg-server/src/main.rs` `fn append_wal` (covered transitively by row 1.3 [machine]); v5.4.2 conditional fsync in `append_wal_v3_group` + `append_wal` |
| 1.2 | Snapshot envelope versioned | `Engine::restore_envelope` accepts both v3.x bare-catalog and v4.1+ envelope; round-trip test exists | ✅ | `crates/spg-engine/src/lib.rs::tests` |
| 1.3 | WAL replay on startup | Server replays WAL onto restored snapshot; truncated tail dropped with stderr warning | ✅ | `crates/spg-server/src/main.rs` `fn replay_wal_bytes` |
| 1.4 | Auto-rollback open TX at end-of-WAL | If crash happened mid-TX, startup runs `ROLLBACK` automatically | ✅ | `crates/spg-server/src/main.rs:237` |
| 1.5 | Backup bundle format documented | Self-contained file with magic, version, snapshot, WAL slice | ✅ | `crates/spg-server/src/backup.rs` |
| 1.6 | Full + incremental backup | `BACKUP TO '<path>'` and `BACKUP TO '<path>' INCREMENTAL SINCE N` SQL forms | ✅ | v4.25.0, `tests/e2e_backup.rs` |
| 1.7 | PITR via `SPG_REPLAY_UPTO` | Operator can truncate WAL replay at byte offset N at startup | ✅ | v4.25.0 + v4.27.1 (parse-zero fix) |
| 1.8 | WAL/snapshot checksum [machine] | Active corruption detection on each loaded file (not just "deserialize fails") | ✅ | v4.37, CRC32 on every storage envelope: WAL v2 records (`[u32 (len|0x80000000)][u32 crc32][sql]`), snapshot envelope v2 (`SPGENV01` + version 2 + trailing CRC32), backup bundle v2 (`SPGBKUP\x02` + trailing CRC32). v1 formats stay readable; mismatch is a hard fail with explicit error. e2e: `tests/e2e_chaos.rs::chaos_wal_bit_flip_caught_by_crc32_refuses_to_replay`. |
| 1.9 | Partial-fsync recovery [machine] | If `sync_data` returns mid-write, the file's incomplete tail is detected on next boot and dropped, no half-record applied | ✅ | v4.29, `tests/e2e_chaos.rs::chaos_wal_tail_truncation_drops_partial_record_no_panic` |
| 1.10 | Disk-full handling [machine] | Out-of-space during WAL append returns clear error to client; server stays alive; previously CC'd state survives restart unchanged | ✅ | v4.29, `tests/e2e_chaos.rs::chaos_disk_full_returns_clean_error_and_keeps_serving` (+ SPG_FAIL_WAL_QUOTA_BYTES injection knob) |
| 1.12 | Async-commit durability window [machine] | When `SPG_SYNCHRONOUS_COMMIT=off`, the per-write `sync_data` is skipped; a background flusher thread emits `durability_checkpoint` WAL markers (v5.4.0 wire kind tag 0x02) every `SPG_FLUSHER_INTERVAL_US` µs (default 200) and `sync_data`s. A SIGKILL between two ticks loses only the WAL bytes appended in the current window; bytes covered by the most recent marker survive replay. Sync-commit (the default + `=on` + any non-opt-in value) preserves every v4.42 invariant byte-for-byte. | ✅ | v5.4 — `SPG_SYNCHRONOUS_COMMIT` env knob (`synchronous_commit_disabled` in `crates/spg-server/src/main.rs`); flusher thread in `crates/spg-server/src/flusher.rs`; wire format in STABILITY.md §"Async-commit mode (v5.4)" + §"WAL record format" (kind 0x02). e2e: `tests/e2e_async_commit.rs::{sync_commit_default_writes_apply_and_are_visible, async_commit_off_inserts_visible_immediately, explicit_sync_commit_on_behaves_like_default}` + `tests/e2e_flusher.rs::{flusher_metric_zero_in_default_sync_commit_mode, flusher_metric_rises_under_async_commit_off, flusher_env_var_recognizes_off_false_zero, flusher_env_var_treats_on_as_sync, durability_lag_metrics_are_zero_in_sync_mode, durability_lag_seconds_bounded_in_async_mode}` + `tests/e2e_chaos_async_commit.rs::chaos_kill_during_async_commit_window_loses_only_unflushed` (kill-mid-window → prefix-recovers). CI throughput floor: `tests/slo_smoke.rs::slo_wal_insert_async_commit_smoke_speedup_vs_sync` (host-noise-tolerant sync-vs-async ratio test). Release-process 200K r/s ship gate: `tests/slo_smoke.rs::slo_wal_insert_async_commit_above_200k` (`#[ignore]`-marked; PERFORMANCE.md §"v5.4 async commit" records the measured number). |
| 1.11 | In-memory consistency on WAL refusal [machine] | When the WAL layer refuses a write, the live in-memory state never reflects it. Caller's `SELECT` sees exactly what was CC'd. | ✅ | v4.34, auto-commit BEGIN..COMMIT wrap in `crates/spg-server/src/main.rs` (atomic WAL block + ROLLBACK on append failure); closes both the chaos path (kept v4.30 preflight) and the real ENOSPC mid-`write_all` path. v4.39 backed `Table::rows` with `PersistentVec`; v4.40 backed `Table::indices` BTreeMap with `PersistentBTreeMap` so the wrap's per-write `Catalog::clone()` is O(1) even on indexed tables — verified @ scale via `xbench/competitor/src/bin/sweep.rs` (with two secondary indices) without weakening rollback semantics. v4.41 collapsed the three-v2-record `[BEGIN, sql, COMMIT]` block into one v3 `auto_commit_sql` record (header overhead 35→9 bytes/write); chaos test still pins the rollback path on the new single-record code. v4.42 routes the wrap through a commit-barrier queue with a sequential prepare-and-commit-in-memory chain and a batched fsync, so N concurrent writers share one fsync; on fsync failure the leader calls `Engine::replace_catalog(pre_image)` to undo every in-group mutation at once — extends the v4.34 rollback invariant from single-client to multi-client. v5.1 added `Catalog::cold_segments: Vec<Arc<OwnedSegment>>` for the cold-tier read path; the field is `Arc`-wrapped so `Catalog::clone` stays the O(N segments) Arc-bump it was before v5.1, preserving the group-commit pre-image invariant. Cold-tier reads are non-mutating (all v5.1 INSERT / UPDATE / DELETE still flow through the existing hot-tier WAL path); cold-tier rows enter only via `Catalog::load_segment_bytes` + `Table::register_cold_locators`, neither of which appears on the WAL-write path. v5.2 added the background freezer thread (`crates/spg-server/src/freezer.rs`) that drives `Catalog::freeze_oldest_to_cold` — a clone-mutate-replace atomic swap that preserves the v4.42 pre-image rollback invariant because the segment build, hot-row drop, and Cold-locator registration all happen on a cloned catalog before `Engine::replace_catalog` installs them. v5.2 freezes are intentionally non-WAL-durable (the freezer skips ticks under any open TX and the freeze itself emits no `freeze_commit` WAL record — that's v5.3 manifest); a crash mid-freeze rolls the cold tier back to its pre-freeze state via the standard WAL-replay-from-snapshot path, no orphan Cold locators. v5.2.3 added PK-targeted promote-on-write through `Catalog::promote_cold_row` + `shadow_cold_row`; both are invoked inline from `Engine::exec_update_cancel` / `exec_delete_cancel` before the hot walk, so a cold row gets pulled hot under the same engine write lock that runs the rest of the statement — no separate commit, no extra fsync, no chaos surface beyond what an INSERT already has. v5.2.0 fixed a latent `Table::rebuild_indices` bug that wiped Cold locators on unrelated keys (only the freezer's manual capture-restore had been masking it); the fix is the canonical Cold-preservation in `rebuild_indices` itself, so any future caller of `delete_rows` / `update_row` on a table with cold rows stays consistent. e2e: `tests/e2e_chaos.rs::chaos_disk_full_no_preflight_rolls_back_in_memory_to_match_durable_state` (single-client) + `tests/e2e_chaos.rs::chaos_disk_full_multi_client_group_rollback_all_writers` (4-client fan-out, v4.42); group-commit correctness: `tests/e2e_group_commit.rs::single_client_group_of_one_no_latency_tax` + `four_client_concurrent_inserts_all_durable`; cold-tier read path: `crates/spg-engine/tests/e2e_two_tier.rs` (8 cases incl. v5.2.3 promote/shadow) + `crates/spg-server/tests/e2e_two_tier_server.rs` (end-to-end via SPG_PRELOAD_COLD_SEGMENT); freezer thread: `crates/spg-server/tests/e2e_freezer.rs` (2 cases) + `crates/spg-server/tests/e2e_chaos_freeze.rs::chaos_kill_during_freeze_recovers_clean_state` (v5.2 → v5.3 trigger); perf gates: `tests/slo_smoke.rs::slo_wal_insert_p99_under_budget` (single-client) + `tests/slo_smoke.rs::slo_wal_insert_multi_client_p99_under_budget` + `tests/slo_smoke.rs::slo_wal_insert_4client_throughput_above_floor` + `tests/slo_smoke.rs::slo_wal_insert_1m_rows_throughput`. |

## 2. Availability + recovery

Operator promise: the database comes back up automatically after
an unclean stop; followers catch up; restore is documented and
practiced.

| # | Item | Criterion | Status | Evidence |
|---|------|-----------|:------:|----------|
| 2.1 | Crash recovery is automatic | Restart with same db+wal paths picks up where it left off | ✅ | 1.3 + 1.4 |
| 2.2 | Replication primary→follower | Follower bootstraps from primary snapshot then tails WAL; e2e test verifies | ✅ | v4.24.0, `tests/e2e_replication.rs` |
| 2.3 | Replication lag measured | Documented numbers, p50/p95/p99 | ✅ | `xtests/v4_24_repl_report.md`, PERFORMANCE.md §v4.24 |
| 2.4 | Follower reconnect after primary restart | Follower retries with backoff; resumes from last applied offset | ✅ | `crates/spg-server/src/replication.rs` `fn run_follower` |
| 2.5 | Restore drill documented [machine] | Step-by-step commands to take backup, simulate loss, restore; reader can execute verbatim | ✅ | v4.30, `RESTORE_DRILL.md` |
| 2.6 | Restore drill automated [machine] | E2E test follows the doc commands; fails CI if doc rots | ✅ | v4.30, `tests/e2e_restore_drill.rs::restore_drill_full_plus_incremental_recovers_row_count` |
| 2.7 | Graceful shutdown [machine] | SIGTERM drains in-flight queries up to a deadline, refuses new connections, exits 0 | ✅ | v4.33, `SPG_SHUTDOWN_DEADLINE_SEC` (default 30s) + SIGTERM/SIGINT handler in `crates/spg-server/src/main.rs`; e2e: `tests/e2e_graceful_shutdown.rs::graceful_shutdown_drains_inflight_and_refuses_new_conns_and_exits_zero` |
| 2.8 | Automated failover | A follower can be promoted to primary by operator action; coordinator handles it | 🚫 | manual promotion only; HA = "stop follower, point clients at it, restart in primary mode". See [[spg-out-of-scope]] |
| 2.9 | Network partition tolerance [chaos] [machine] | Follower disconnect while primary writes → on reconnect, all writes catch up exactly once, no duplicates / no gaps | ✅ | v4.36, in-test TCP proxy + `tests/e2e_chaos_netsplit.rs::netsplit_disconnect_then_heal_resyncs_without_loss_or_dup` (asserts row count + value sum both match). |
| 2.10 | Fast restart at scale [machine] | After `CHECKPOINT`, a 100M-row catalog restarts in ≤ 60 s wall-time. Boot reads sidecar manifest, verifies snapshot CRC, auto-preloads every cold-tier segment, skips WAL bytes before `wal_baseline_offset`. | ✅ | v5.3 — manifest format `crates/spg-server/src/manifest.rs` (`SPGMAN01` magic, FILE_VERSION 10), wire-up in `main.rs::write_manifest_alongside` / `load_manifest_and_preload_cold`, `CHECKPOINT` SQL admin command + WAL `ftruncate` in `run_checkpoint_command`. CI gate: `tests/e2e_manifest.rs::manifest_restores_cold_segments_across_restart` (in no-WAL mode, auto-manifest on every snapshot) + `checkpoint_truncates_wal_and_persists_through_restart` (WAL-mode end-to-end) + `checkpoint_rejects_non_admin_caller` (RBAC). 100M boot-time gate: `tests/e2e_manifest.rs::restart_at_100m_under_60s_after_checkpoint` (`#[ignore]`-marked release-process trigger). |

## 3. Security

Operator promise: production data is not readable without
credentials; credentials are not stored in plaintext; audit trail
detects tampering.

| # | Item | Criterion | Status | Evidence |
|---|------|-----------|:------:|----------|
| 3.1 | Per-user auth (native + PG-wire) | `AUTH user password` opcode + PG-wire SCRAM-SHA-256; bad password rejected before any query | ✅ | v4.1.0 + v4.8.0, `tests/e2e_auth.rs`, `tests/e2e_pg_scram.rs` |
| 3.2 | RBAC three roles | admin / readwrite / readonly; writes from readonly are rejected | ✅ | v4.1.0, `tests/e2e_rbac.rs` |
| 3.3 | Passwords salted + hashed | Per-user 16-byte random salt + BLAKE3; SCRAM secret stored alongside | ✅ | v4.1.0 + v4.8.0, `crates/spg-engine/src/users.rs` |
| 3.4 | Audit log tamper-evident | BLAKE3 hash-chain; reorder/splice/edit caught on startup verify | ✅ | `crates/spg-audit/`, `tests/e2e_audit.rs` |
| 3.5 | Admin bootstrap from env | `SPG_ADMIN_PASSWORD` set on first run creates admin; absent = open mode with warning | ✅ | `bootstrap_admin_from_env()` |
| 3.6 | TLS / wire encryption | — | 🚫 | permanent out-of-scope per [[spg-out-of-scope]]; deploy behind stunnel / nginx / pgbouncer if needed |
| 3.7 | Secret scanning in CI [machine] | gitleaks rejects commits containing high-entropy strings / common API key formats | ✅ | v4.32, `.github/workflows/ci.yml::gitleaks` |
| 3.8 | Dependency vulnerability scanning [machine] | `cargo-audit` runs on every CI build, fails the job on advisory match | ✅ | v4.27.0, `.github/workflows/ci.yml` |
| 3.9 | `cargo-deny` license + dup check [machine] | Workspace deps respect license allowlist; no duplicate semver-incompatible dups | ✅ | v4.31, `deny.toml` + `.github/workflows/ci.yml::cargo-deny` |
| 3.10 | SQL injection surface | Server is the SQL — no client-driven query construction at the protocol level. Prepared statement parameters always bound through Bind frame, never string-interpolated. Documented. | ✅ | v4.30, `SECURITY.md` threat-model table |
| 3.11 | Input fuzz harness [machine] | Randomized SQL + wire-frame input bombards the parser/decoder under a deterministic PRNG; **no panics** allowed. Default 10K iters per run; `SPG_FUZZ_ITERS=N` for longer. | ✅ | v4.31, `crates/spg-sql/tests/fuzz.rs` + `crates/spg-server/tests/e2e_fuzz.rs` |
| 3.12 | CVE response process [machine] | SECURITY.md says where to report; maintainer commits to 7-day triage | ✅ | v4.30, `SECURITY.md` |

## 4. Observability

Operator promise: when something is wrong, you can tell within
seconds, and you have enough signal to diagnose.

| # | Item | Criterion | Status | Evidence |
|---|------|-----------|:------:|----------|
| 4.1 | Health endpoint [machine] | `GET /healthz` returns 200 + JSON; works on a fresh server | ✅ | v4.13.0, `tests/e2e_observability.rs` |
| 4.2 | Prometheus metrics [machine] | `GET /metrics` exposes connections_active / queries_total / errors_total in exposition format | ✅ | v4.13.0 |
| 4.3 | Structured logging | `SPG_LOG_FORMAT=json` switches stderr to single-line JSON per event | ✅ | v4.13.0 |
| 4.4 | EXPLAIN / EXPLAIN ANALYZE | Operator can inspect plan + actual rows for any query | ✅ | v4.26.0, `tests/e2e_explain.rs` |
| 4.5 | Slow-query log [machine] | Queries exceeding threshold logged with SQL + elapsed | ✅ | v4.33, `SPG_SLOW_QUERY_LOG_MS`; one JSON line per slow query on stderr (`sql`/`elapsed_us`/`role`/`threshold_us`); e2e: `tests/e2e_slow_query_log.rs::slow_query_log_fires_above_threshold_and_silent_below` |
| 4.6 | Per-table row count / size metrics [machine] | `spg_table_rows{table=…}` series exposed via /metrics | ✅ | v4.35, `spg_table_rows{table=…}` + `spg_table_bytes{table=…}` in `crates/spg-server/src/observability.rs`. Cardinality cap via `SPG_METRICS_TABLE_TOPN` (default 50) or exact `SPG_METRICS_TABLE_ALLOWLIST=t1,t2`. e2e: `tests/e2e_table_metrics.rs` (default top-N, allowlist filter, cardinality cap). |
| 4.7 | Replication lag metric [machine] | Lag in bytes / seconds exposed via /metrics on the follower | ✅ | v4.36, `spg_replication_lag_bytes` + `spg_replication_lag_seconds` in `/metrics`, fed by the new `SPGREPL\x02` status frames (`crates/spg-server/src/replication.rs`). Stable surface in STABILITY.md §"Replication protocol". e2e: `tests/e2e_chaos_netsplit.rs::follower_metrics_expose_replication_lag_after_status_frame`. |
| 4.8 | OpenTelemetry tracing | — | 🚫 | not in scope for v4.x — single-process tracing is what logs already provide |

## 5. Resource control

Operator promise: a single bad query / misbehaving client cannot
take down the server.

| # | Item | Criterion | Status | Evidence |
|---|------|-----------|:------:|----------|
| 5.1 | `SPG_MAX_CONNECTIONS` [machine] | Overflow gets clear error frame; socket closes immediately; counter decrements on drop | ✅ | v4.2.0, `tests/e2e_limits.rs` |
| 5.2 | `SPG_MAX_QUERY_ROWS` | Engine enforces row count cap at dispatch boundary; runaway SELECT errors clearly | ✅ | v4.2.0, `tests/e2e_limits.rs` |
| 5.3 | `SPG_QUERY_TIMEOUT_MS` | Watchdog flips cancel flag; engine row-loops check at 256-row stride; query aborts within ~1s of deadline | ✅ | v4.5.0, `tests/e2e_timeouts.rs` |
| 5.4 | `SPG_IDLE_TIMEOUT_SEC` | Connection idle past N seconds is closed by the OS read timeout | ✅ | v4.5.0 |
| 5.5 | Per-query memory cap [machine] | A query cannot allocate more than N bytes on the server heap; over-cap errors before OOM | ✅ | v5.5.1, `SPG_MAX_QUERY_BYTES` (default 256 MiB; `0` = unlimited). Custom `#[global_allocator]` (`crates/spg-server/src/alloc_budget.rs`) tracks per-thread net live bytes; on overshoot it trips the active query's cancel flag and the engine's 256-row checkpoints bail with `EngineError::Cancelled`. e2e: `tests/e2e_query_budget.rs::{over_budget_select_is_cancelled, under_budget_select_succeeds}`. Contract frozen in STABILITY.md §"Per-query memory budget (v5.5)". |
| 5.6 | Per-query memory exhaustion survives [chaos] [machine] | A query that would exhaust memory is cancelled with a clear error before OOM; the server stays up under repeated pressure, no half-applied state | ✅ | v5.5.1/2 — the per-query budget (row 5.5) is the clean-error path: a runaway query is cancelled (`EngineError::Cancelled`) before a true OOM. e2e: `tests/e2e_query_budget.rs::chaos_oom_returns_cancelled_not_panic` (repeated over-budget pressure → each cancelled, server survives, child never aborts). NOTE: under `panic = "abort"` a *true* system allocation failure still fail-fast aborts (no unwind; `set_alloc_error_hook` is nightly-only) — deliberate, to avoid half-written WAL/catalog state. Since the cap is « system RAM the budget trips first, so a real alloc failure is an ops-level condition (single oversize alloc or cap above RAM). STABILITY.md §"Per-query memory budget (v5.5)". |
| 5.7 | Disk water-mark check [machine] | Server refuses writes when WAL volume free space < N MB; serves reads | ✅ | v4.33, `SPG_WAL_MIN_FREE_BYTES`; `statvfs(2)` before each WAL append (macOS + Linux), returns `StorageFull` with explicit env-var citation, reads unaffected; e2e: `tests/e2e_disk_watermark.rs::disk_watermark_refuses_writes_keeps_reads_keeps_server_alive` |

## 6. Correctness

Operator promise: SQL semantics match documented dialect; no
silent data corruption under concurrent access.

| # | Item | Criterion | Status | Evidence |
|---|------|-----------|:------:|----------|
| 6.1 | Four-dialect corpus 100% | pgvector + duckdb + pg_regress + mysql all 100% pass | ✅ | v3.3.4 baseline, `cargo run -q -p sqllogictest --release` |
| 6.2 | Read-committed isolation | Documented; concurrent transactions don't see each other's uncommitted writes | ✅ | engine uses single-writer model with shadow catalog for TX |
| 6.3 | Concurrent read scaling [machine] | N parallel readers scale to ~N× throughput vs serial | ✅ | v4.0.0, PERFORMANCE.md §Concurrency |
| 6.4 | Foreign keys | — | 🚫 | not in v4.x scope; documented |
| 6.5 | CHECK constraints | — | 🚫 | not in v4.x scope |
| 6.6 | Window functions complete | ROW_NUMBER / RANK / DENSE_RANK / SUM / AVG / COUNT / MIN / MAX / LAG / LEAD / FIRST_VALUE / LAST_VALUE / NTH_VALUE / NTILE / PERCENT_RANK / CUME_DIST + ROWS / RANGE explicit frames | ✅ | v4.12 + v4.20 + v4.21 |
| 6.7 | Subqueries complete | scalar / EXISTS / IN, uncorrelated + correlated in WHERE | ✅ | v4.10 + v4.23 |
| 6.8 | WITH / WITH RECURSIVE | non-recursive + recursive with runaway guard | ✅ | v4.11 + v4.22 |
| 6.9 | Date/time arithmetic | INTERVAL + date + EXTRACT / DATE_TRUNC / DATE_PART / AGE | ✅ | v2.x |
| 6.10 | Vector kNN correctness | HNSW top-K results match brute-force within recall@k ≥ 0.9 over the 4-corpus baseline | ✅ | pgvector corpus 100% |
| 6.11 | Vector encoding alternatives [machine] | `VECTOR(N)` accepts `USING SQ8` (4× compression, recall@10 ≥ 0.95 via f32 rerank) and `USING HALF` (IEEE-754 binary16, 2× compression, bit-exact dequant). NEON SIMD for f32 / SQ8 ADC; halfvec rides the f32 NEON path via dequant-in-loop until stable Rust ships `f16`. | ✅ | v6.0.1 (SQ8) + v6.0.2 (NEON cos/IP + SQ8 ADC) + v6.0.3 (HALF), `tests/e2e_sq8.rs` + `tests/e2e_half.rs` |
| 6.12 | Vector kNN at 1M scale [machine] | 1M dim-128 SQ8: ingest + `CREATE INDEX … USING hnsw` succeeds end-to-end; kNN top-10 via pgwire round-trip p50 ≤ 5 ms, p99 ≤ 10 ms on Apple M-series. RSS ≤ 800 MiB. | ✅ | v6.0.5 measurement: p50 = 362 µs, p99 = 539 µs, RSS = 624 MiB. `tests/perf_gate_sq8.rs` `#[ignore]`'d (run via `--ignored`). |
| 6.13 | Vector encoding migration [machine] | `ALTER INDEX <name> REBUILD [WITH (encoding = F32 \| SQ8 \| HALF)]` recodes every stored cell + rebuilds the NSW graph in place — synchronous MVP; holds engine.write() for the rebuild duration. | ✅ | v6.0.4, `tests/e2e_alter_rebuild.rs` |

## 7. Operational tooling

Operator promise: install, configure, back up, restore, upgrade
without reading the source code.

| # | Item | Criterion | Status | Evidence |
|---|------|-----------|:------:|----------|
| 7.1 | CLI for backup/restore | `spg-cli` covers full backup + restore flows | ✅ | `crates/spg-cli/` |
| 7.2 | DEPLOYMENT.md [machine] | Install (source + binary), env vars table, file layout, port reference, recommended fs/disk | ✅ | v4.30, `DEPLOYMENT.md` |
| 7.3 | RUNBOOK.md [machine] | Common alerts (high error rate / replication lag / disk near full) + how to respond | ✅ | v4.30, `RUNBOOK.md` |
| 7.4 | RESTORE_DRILL.md + e2e [machine] | Verbatim commands to take backup → simulate loss → restore → verify; e2e test follows them | ✅ | v4.30, `RESTORE_DRILL.md` + `tests/e2e_restore_drill.rs` |
| 7.5 | SECURITY.md [machine] | CVE reporting address, response SLA, secret-handling guidance | ✅ | v4.30, `SECURITY.md` |
| 7.6 | CHANGELOG.md [machine] | SemVer-organized v4.x history; future PRs must update it (CI check) | ✅ | v4.30, `CHANGELOG.md` (CI enforcement is post-v4.32) |
| 7.7 | Migration framework | Schema migrations beyond raw DDL | 🚫 | not in scope; users run DDL via standard SQL |
| 7.8 | Config validation on startup | Server refuses to start if env vars conflict (e.g. SPG_FOLLOW_OF + SPG_REPL_ADDR on same node) | ⚠️ | (v4.30 candidate — currently warns but starts anyway) |
| 7.9 | Logical replication: CREATE / DROP / SHOW PUBLICATION | All three forms (FOR ALL TABLES / FOR TABLE list / FOR ALL TABLES EXCEPT list) parse, persist via snapshot envelope v3+, round-trip Display | ✅ | v6.1.2 + v6.1.3, `crates/spg-server/tests/e2e_publication_ddl.rs` (9/9) + STABILITY.md §"Publication DDL" |
| 7.10 | Logical replication: CREATE SUBSCRIPTION + worker | DDL lands the catalog row; reconcile spawns per-subscription worker that drains v2 frame stream from publisher and applies SQL to local engine; DROP shuts the worker within ~500 ms | ✅ | v6.1.4, `e2e_subscription.rs` (3/3) + STABILITY.md §"Subscription DDL" + §"MAGIC_SUB protocol" + §"Snapshot envelope v4" |
| 7.11 | Logical replication: publisher-side filter | DML records are filtered by the requested publication's scope before they hit the wire; DDL + session-control SQL is never propagated (PG-compatible); the lightweight owner extractor runs ≤ 200 ns/record | ✅ | v6.1.5, `e2e_replication_filter.rs` (3/3) + `replication::tests::extract_owner_perf_under_200ns` measured 41 ns/call |
| 7.12 | Logical replication: cascading + cycle detection | Three-node A → B → C chain replays correctly via MAGIC_V2 + MAGIC_SUB; direct self-loop subscriptions are aborted at handshake via the per-cluster `cluster_id` sidecar | ✅ | v6.1.6, `e2e_cascade.rs` (3/3) |
| 7.13 | Logical replication: consistent-read barrier | `WAIT FOR WAL POSITION <pos> [WITH TIMEOUT <ms>]` blocks until the local apply pos reaches the target or the timeout fires; reached=1 / timed-out=0 via CommandComplete count | ✅ | v6.1.7, `e2e_wait_pos.rs` (5/5) |
| 7.14 | Logical replication: opt-in gate | Fresh cluster boots in `replica` mode; MAGIC_SUB stays closed until `SET effective_wal_level = 'logical'` (or `SPG_WAL_LEVEL=logical` env at startup); `SHOW effective_wal_level` exposes current value | ✅ | v6.1.8, `e2e_wal_level.rs` (6/6) |
| 7.15 | Logical replication: chaos resilience | Subscription worker reconnects across multiple netsplit + heal cycles; final row count is exactly correct (no dup, no gap) | ✅ | v6.1.9, `e2e_chaos_logical.rs` (2/2) |
| 7.16 | Optimizer: per-column statistics + ANALYZE | `spg_statistic` virtual table (name/column/null_frac/n_distinct/histogram_bounds) + `ANALYZE [<table>]` foreground command + background auto-trigger (10% modified) | ✅ | v6.2.0 + v6.2.1, `e2e_spg_statistic.rs` (6/6) + `e2e_auto_analyze.rs` (4/4) + `spg_engine::statistics` module (9) |
| 7.17 | Optimizer: JOIN reorder | ≤ 4 tables brute-force, > 4 greedy; uses v6.2.0 stats; reorder skipped when no ANALYZE has run (PG-compatible opt-in); 5-table speedup ship gate measured 9002.5× vs source order | ✅ | v6.2.3, `perf_join_reorder.rs` ship gate + `spg_engine::reorder` module (3) + `spg_engine::selectivity` module (11) |
| 7.18 | Optimizer: EXPLAIN ANALYZE | Every operator line carries `(rows=N)` or `(hot_rows=N, cold_tier=present, cold_segments=[id0,…])` for scans; `Total: rows=N elapsed=Mμs` trailer for whole-query timing | ✅ | v6.2.4 + v6.2.5 + v6.2.7, `e2e_explain_analyze.rs` (6/6) |
| 7.19 | Optimizer: Memoize correlated subqueries | Per-query LRU cache (1024 entries / 16 MiB caps) sharing key on (subquery repr, outer-row values); 95 % hit ratio on repeated-key workloads | ✅ | v6.2.6, `spg_engine::memoize` module (7) + `e2e_memoize.rs` (3/3) |
| 7.20 | Optimizer: TPC-H integration | Q1 – Q5 against a deterministic 7-table micro-fixture; correctness via row-preservation + ORDER-BY-monotonicity invariants; plan stability gate (5 consecutive byte-identical EXPLAIN runs) | ✅ | v6.2.7, `e2e_tpch.rs` (6/6) |
| 7.21 | PG-wire extended query: engine plan cache | Engine-level LRU keyed on SQL text (256-entry cap), shared across pgwire sessions; hit path ≤ 1/3 of cold (measured 0.15, 6.8× speedup on 5-table JOIN prepare) | ✅ | v6.3.0, `e2e_plan_cache.rs` (3/3) + `perf_plan_cache.rs` ship gate + `spg_engine::plan_cache` module (11) |
| 7.22 | PG-wire extended query: plan cache invalidation | ANALYZE bumps `Statistics::version()`; cached entries snapshot version at prepare time; stale lookup evicts. Bare ANALYZE clears cache; named ANALYZE evicts only referencing plans. CREATE INDEX / ALTER INDEX REBUILD also evict | ✅ | v6.3.1, `e2e_plan_cache_invalidation.rs` (5/5) |
| 7.23 | PG-wire extended query: pipelined query mode | Server-side response buffering — all send_* helpers write into a per-connection Vec<u8>; drained at Sync / Flush / ReadyForQuery + 4 KiB threshold backstop. Amortised pipelined cycle ≤ 1.3 × single (measured 0.15, 6.7× speedup at batch=16) | ✅ | v6.3.2, `e2e_pgwire_pipelined.rs` (2/2) |
| 7.24 | PG-wire extended query: Describe pre-Execute | Describe('S', name) → ParameterDescription + (RowDescription \| NoData); Describe('P', name) → (RowDescription \| NoData). Byte-correct for simple SELECT; JOIN / non-SELECT degrade to NoData | ✅ | v6.3.3, `spg_engine::describe` module (5) + `e2e_pgwire_describe.rs` (4/4) |
| 7.25 | PG-wire extended query: binary parameter format + client compat | Bind format-code=1 decoder for BOOL/INT2/INT/BIGINT/REAL/DOUBLE/TEXT/VARCHAR/BYTEA/DATE/TIMESTAMP/TIMESTAMPTZ/NUMERIC (13 types via OID dispatch). Mixed per-param formats supported. NUMERIC reconstructs PG's packed-digit format to `Value::Numeric { scaled, scale }`. Real-client-shaped workloads (JDBC / concurrent pool / psycopg3 pipeline) verified hand-rolled | ✅ | v6.3.4 + v6.3.5, `e2e_pgwire_binary_params.rs` (8/8) + `e2e_pgwire_client_compat.rs` (3/3) |
| 7.26 | SQL polish: multi-column ORDER BY + alias resolution | `ORDER BY a, b DESC, c` honours every key with per-key asc/desc. SELECT-list aliases resolve against the projection before falling through to the FROM schema. Position references (`ORDER BY 2`) continue to bind to the 1-based projection index | ✅ | v6.4.0, `e2e_order_by_multi.rs` (5/5) |
| 7.27 | SQL polish: GROUP BY ALL + window NULL treatment | `GROUP BY ALL` planner pass expands to every non-aggregate SELECT-list item (DuckDB / PG 19 compat). Window functions LAG/LEAD/FIRST_VALUE/LAST_VALUE accept `IGNORE NULLS` / `RESPECT NULLS` between args and OVER | ✅ | v6.4.1 + v6.4.2, `e2e_group_by_all.rs` (3/3) + `e2e_window_null_treatment.rs` (4/4) |
| 7.28 | SQL polish: encode/decode + error_on_null | `encode(text, format)` and `decode(text, format)` for `base64` / `base64url` (RFC 4648 §5) / `base32hex` / `hex`. `error_on_null(v)` returns v or raises (inline NOT NULL assertion). NULL on any arg propagates. Unknown format errors | ✅ | v6.4.3, `e2e_sql_funcs.rs` (8/8) |
| 7.29 | SQL polish: JSON path operators | `j -> key` / `j ->> key` (v4.14 surface preserved) + `j #> path_text` / `j #>> path_text` walk PG `'{a,0,b}'` text-array literals + `j @> sub_json` structural containment (objects + arrays + scalars). NULL propagation throughout | ✅ | v6.4.5, `e2e_json_path.rs` (9/9) |
| 7.30 | SQL polish: transactional DDL hardening | `tx_catalog` shadow mechanism (v4.41.1) formally locked: BEGIN/CREATE/COMMIT is atomic; ROLLBACK undoes prior CREATE TABLE / CREATE INDEX inside the TX; in-TX queries see the shadow catalog | ✅ | v6.4.6, `e2e_transactional_ddl.rs` (4/4) |
| 7.31 | SQL polish: COPY enhancements | `COPY FROM STDIN WITH (SKIP N, ON_ERROR SET_NULL, FORMAT JSON)`. SKIP drops first N data rows (CSV header); ON_ERROR skips bad rows silently; FORMAT JSON parses each line as an object with key→column matching | ✅ | v6.4.7, `e2e_copy_options.rs` (3/3) |
| 7.32 | Observability v2: spg_stat_replication + spg_stat_segment | Two read-only virtual tables exposing subscription registry + cold-tier segment inventory; same dispatch pattern as v6.2.0 spg_statistic. table_name on segment is carved out (no persistent segment→table mapping in storage) | ✅ | v6.5.0, `e2e_spg_stat_views.rs` (3/3) |
| 7.33 | Observability v2: spg_stat_query | Per-distinct-SQL LRU stat collector (1024-entry cap). Engine records (exec_count, total_us, mean_us, max_us, last_seen_us) on every successful execute. Surface via SELECT * FROM spg_stat_query | ✅ | v6.5.1, `spg_engine::query_stats` (6) + `e2e_spg_stat_query.rs` (4/4) |
| 7.34 | Observability v2: spg_stat_activity | Per-pgwire-connection registry: pid, user, started_at, current_sql, wait_event, elapsed, in_transaction. spg-server maintains the registry (Arc<ConnState>); engine reads through ActivityProvider callback bridged via ACTIVITY_STATE OnceLock | ✅ | v6.5.2, `e2e_spg_stat_activity.rs` (3/3) |
| 7.35 | Observability v2: spg_audit_chain + spg_audit_verify | spg_audit_chain exposes every BLAKE3-chained audit entry as a row; spg_audit_verify re-walks the chain and returns (verified_count, broken_at_seq). pgwire Q-path now also appends to AuditLog on modified_catalog statements | ✅ | v6.5.3, `e2e_audit_verify.rs` (3/3) |
| 7.36 | Observability v2: DDL introspection | spg_table_ddl + spg_role_ddl + spg_database_ddl synthesise CREATE statements from catalog state. Round-trip property: piping spg_table_ddl back through Engine::execute recreates the same schema | ✅ | v6.5.4, `e2e_get_ddl.rs` (3/3) |
| 7.37 | Observability v2: wait events lite | ConnState.wait_event AtomicU8 set/cleared around engine.write() acquisitions in pgwire's Q-path; spg_stat_activity renders "write_lock" mid-execute, "" idle. fsync + group_commit attribution carved out (cross-thread state) | ✅ | v6.5.5, `e2e_wait_events.rs` (1/1) |
| 7.38 | Observability v2: defaults rebaseline | SPG_SLOW_QUERY_THRESHOLD_MS env (default 100): every execute crossing the floor fires the slow-query log callback. SPG_PLAN_CACHE_MAX env (default 256): runtime cap on v6.3.0 plan cache. Both wired through Engine builder API | ✅ | v6.5.6, `e2e_slow_query.rs` (2/2) |

## 8. Stability + compatibility

Operator promise: client code written against v1.0 still works
against v1.x. Backup files captured by v1.x restore on v1.y.

| # | Item | Criterion | Status | Evidence |
|---|------|-----------|:------:|----------|
| 8.1 | SemVer adherence | Major bump for breaking changes; documented | ✅ | v4.x kept compatible since v4.0; pre-v1.0 contract |
| 8.2 | Native wire opcode stable | Opcodes 0x00-0x17 + 0xFF documented and never changed silently | ✅ | v1-status memory §Wire opcode table |
| 8.3 | PG-wire SCRAM stable | SCRAM-SHA-256 wire spec (RFC 5802) — by spec, not by SPG | ✅ | RFC reference |
| 8.4 | Snapshot file backwards-compat [machine] | v5.x can load every snapshot ever written (FILE_VERSION 1..9) | ✅ | v4.31, `tests/cross_version_compat.rs` walks every directory under `xtests/compat-fixtures/`. v5.2.0 bumped `FILE_VERSION` 8 → 9 (tagged `RowLocator` on-disk codec); the v9 reader's version dispatch in `Catalog::deserialize` accepts v8 streams via the legacy rebuild path (`MIN_SUPPORTED_FILE_VERSION = 8`). Three fixtures replay: `v4.30` (v8) + `v4.41` (v8) + `v5.2` (v9). |
| 8.5 | STABILITY.md [machine] | What's frozen, what's not; how to read upgrade notes | ✅ | v4.31, `STABILITY.md` |
| 8.6 | Cross-version compat test [machine] | CI runs against a corpus of snapshot/WAL files from older minor versions, asserts restore + identical query results | ✅ | v4.31, `tests/cross_version_compat.rs` + `xtests/compat-fixtures/v4.30/` |

## 9. Testing rigor

Operator promise: every prod feature is exercised by an automated
test; regressions fail the build, not the customer.

| # | Item | Criterion | Status | Evidence |
|---|------|-----------|:------:|----------|
| 9.1 | Unit + e2e test count | Full workspace passes; > 300 tests across crates | ✅ | 393 tests pass workspace-wide (2026-05-27) |
| 9.2 | Perf gates [machine] | `cargo test --release --test perf_gate` is part of CI; budgets in BUDGETS.md | ✅ | PERFORMANCE.md §Perf gates |
| 9.3 | 5-min soak (leak detection) | Mixed workload for 5 minutes, post-warmup RSS drift < 2% | ✅ | v4.16, `xtests/v4_soak_report.md` |
| 9.4 | 24h sustained load | Same mixed workload for 24h, drift + throughput stable | ✅ | v4.32, `soak_v4 --minutes 1440` (release-prep gate, not per-PR). 5-min variant gated in CI via `xtests/v4_soak_report.md`. |
| 9.5 | Chaos test infrastructure | Failpoint hooks (SPG_FAIL_WAL_QUOTA_BYTES env var); kill -9 + partial fsync + disk full chaos automated | ✅ | v4.29, `tests/e2e_chaos.rs` (3 tests); netsplit chaos deferred to v4.30 |
| 9.6 | Chaos test in CI | `e2e_chaos` runs under the main `test` job since v4.29; gated, not warning. | ✅ | v4.29, `.github/workflows/ci.yml` (test job runs all e2e_*) |
| 9.7 | Fuzz harness | Randomized SQL + wire-frame inputs through deterministic SplitMix64 PRNG; 0 panics in ≥10K iters per run, `SPG_FUZZ_ITERS` raises the bound. | ✅ | v4.31 (see row 3.11) |
| 9.8 | CI on every PR | fmt + clippy + test + audit + (release build on main) all pass | ✅ | v4.27.0, `.github/workflows/ci.yml` |

## 10. Performance SLO

Operator promise: under documented load, performance stays within
documented bounds. Not "fastest possible", just "predictable".

| # | Item | Criterion | Status | Evidence |
|---|------|-----------|:------:|----------|
| 10.1 | Latency baseline doc | SEL p50/p95/p99 + INS p50/p95/p99 in PERFORMANCE.md vs competitors | ✅ | PERFORMANCE.md §v4.27 |
| 10.2 | Throughput baseline doc | INSERT + SCAN rows/s | ✅ | PERFORMANCE.md §v4.27 |
| 10.3 | ANN baseline doc | HNSW build + query p50/p95/p99 | ✅ | PERFORMANCE.md §v4.27 |
| 10.4 | SLO contract published [machine] | "spg-server SEL p99 ≤ Xµs at ≤Y conn / Z rows/s" — explicit numbers committed to | ✅ | v4.32, PERFORMANCE.md §SLO |
| 10.5 | SLO smoke test in CI [machine] | Short bench that asserts SLO numbers; fails CI on regression | ✅ | v4.32, `tests/slo_smoke.rs::slo_smoke_select_and_insert_p99_under_budget` |
| 10.6 | Replication lag SLO | Documented bound (lag p99 ≤ 500 ms; measured 211 ms) | ✅ | v4.32, PERFORMANCE.md §SLO replication table |

---

## How to use this file

**Before a release**: walk every row. Anything not ✅ that the
release claims to fix must be updated to ✅ with an evidence
link. The PR description should diff this file.

**When adding a feature that touches an item**: update the
relevant row's evidence link in the same PR.

**To run the machine-checkable subset**:

```bash
cargo test --release --test prod_ready
```

That test reads this file's `[machine]` rows and asserts each
one. A row that's marked machine-checkable in the file but
doesn't have a corresponding assertion in the test will itself
fail the test.

**To regenerate the snapshot of current state**: the test prints
a summary of pass/fail/skip counts; copy those numbers into the
"Audit snapshot" section below.

---

## Audit snapshot

Last machine run: v4.37 (2026-05-27); v5.5 incremental (rows 5.5 + 5.6 lit up, 2026-05-29).

```
Total rows in checklist : 85 (v4.37 baseline; v5.x added rows — counts below are incremental, `meta_every_machine_row_has_a_test` is authoritative)
  ✅ pass             : 78
  ⚠️ partial          : 1
  ❌ open             : 0
  🚫 out-of-scope     : 6

[machine] rows scaffolded in prod_ready.rs : 38
  row_1_3, row_1_8, row_1_9, row_1_10, row_1_11,
  row_2_5, row_2_6, row_2_7, row_2_9,
  row_3_7, row_3_8, row_3_9, row_3_10, row_3_11, row_3_12,
  row_4_1, row_4_2, row_4_5, row_4_6, row_4_7,
  row_5_1, row_5_5, row_5_6, row_5_7,
  row_6_3,
  row_7_2, row_7_3, row_7_4, row_7_5, row_7_6,
  row_8_2, row_8_4, row_8_5, row_8_6,
  row_9_2, row_9_8,
  row_10_x, row_10_4, row_10_5
```

v4.37 closes row **1.8** (WAL / snapshot / backup checksum). The
three storage envelopes now all carry CRC32:

- **WAL records**: v2 records use a sentinel bit in the length
  header (`u32 (len | 0x8000_0000)`) followed by a u32 CRC32 and
  the payload. v1 records (pre-v4.37 WAL files) replay unchanged.
- **Snapshot envelopes**: `SPGENV01` bumped from version `1` to
  `2`; v2 carries a trailing u32 CRC32 over the whole body.
  Old v1 envelopes load with no CRC check (frozen by STABILITY).
- **Backup bundles**: `SPGBKUP\x01` writers replaced by
  `SPGBKUP\x02` writers; the new bundle ends with a u32 CRC32.
  v1 bundles inspect / restore unchanged.

Bit-flips in any of the three surface as an explicit
`CRC mismatch` failure with the expected / computed values; the
operator chooses whether to discard the corrupt record or
investigate. Covered by `tests/e2e_chaos.rs::chaos_wal_bit_flip_caught_by_crc32_refuses_to_replay`.

The 1 ⚠️ item remaining is a strict-invariant row deferred per
NEXT.md:

- 7.8 startup config validation (v4.30 candidate; rolled forward)

(5.5 per-query memory cap + 5.6 OOM-survives-as-clean-error closed
in v5.5.1/2 — custom `#[global_allocator]` + `SPG_MAX_QUERY_BYTES`;
see rows 5.5 / 5.6.)

The remaining historically-tracked ⚠️ items (v4.37 snapshot;
"works today within documented limits, strict invariant deferred
per NEXT.md"):

- 1.8 explicit CRC32 (today: truncation caught by length prefix; v4.37)
- 2.9 netsplit replication chaos (v4.36)
- 7.8 startup config validation (v4.30 candidate; rolled forward)

Each subsequent release should update this snapshot and add new
`row_X_Y_*` tests for any [machine] rows it lights up.
