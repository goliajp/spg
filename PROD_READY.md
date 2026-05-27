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

Last refresh: **v4.33 ops three-pack** (2026-05-27, commit hash
filled at commit time).

---

## 1. Data durability

Operator promise: a write that returned success is on stable
storage and survives a crash.

| # | Item | Criterion | Status | Evidence |
|---|------|-----------|:------:|----------|
| 1.1 | WAL fsync per commit | `append_wal()` calls `sync_data()` before the SQL handler returns CC | ✅ | `crates/spg-server/src/main.rs` `fn append_wal` (covered transitively by row 1.3 [machine]) |
| 1.2 | Snapshot envelope versioned | `Engine::restore_envelope` accepts both v3.x bare-catalog and v4.1+ envelope; round-trip test exists | ✅ | `crates/spg-engine/src/lib.rs::tests` |
| 1.3 | WAL replay on startup | Server replays WAL onto restored snapshot; truncated tail dropped with stderr warning | ✅ | `crates/spg-server/src/main.rs` `fn replay_wal_bytes` |
| 1.4 | Auto-rollback open TX at end-of-WAL | If crash happened mid-TX, startup runs `ROLLBACK` automatically | ✅ | `crates/spg-server/src/main.rs:237` |
| 1.5 | Backup bundle format documented | Self-contained file with magic, version, snapshot, WAL slice | ✅ | `crates/spg-server/src/backup.rs` |
| 1.6 | Full + incremental backup | `BACKUP TO '<path>'` and `BACKUP TO '<path>' INCREMENTAL SINCE N` SQL forms | ✅ | v4.25.0, `tests/e2e_backup.rs` |
| 1.7 | PITR via `SPG_REPLAY_UPTO` | Operator can truncate WAL replay at byte offset N at startup | ✅ | v4.25.0 + v4.27.1 (parse-zero fix) |
| 1.8 | WAL/snapshot checksum | Active corruption detection on each loaded file (not just "deserialize fails") | ⚠️ | length-prefixed records catch truncation; mid-record bit-flips would currently surface as parse error rather than explicit checksum mismatch. Deferred to v5 file-format bump — add CRC32 to envelope + per-WAL-record. |
| 1.9 | Partial-fsync recovery [machine] | If `sync_data` returns mid-write, the file's incomplete tail is detected on next boot and dropped, no half-record applied | ✅ | v4.29, `tests/e2e_chaos.rs::chaos_wal_tail_truncation_drops_partial_record_no_panic` |
| 1.10 | Disk-full handling [machine] | Out-of-space during WAL append returns clear error to client; server stays alive; previously CC'd state survives restart unchanged | ✅ | v4.29, `tests/e2e_chaos.rs::chaos_disk_full_returns_clean_error_and_keeps_serving` (+ SPG_FAIL_WAL_QUOTA_BYTES injection knob) |
| 1.11 | In-memory consistency on WAL refusal | When the WAL layer refuses a write, the live in-memory state never reflects it. Caller's `SELECT` sees exactly what was CC'd. | ⚠️ | v4.30 added preflight WAL-quota check (chaos path) — main.rs rejects the SQL before engine.execute when the SPG_FAIL_WAL_QUOTA_BYTES knob would fire. Real ENOSPC mid-`write_all` still has the engine-mutated-but-WAL-failed window; full fix needs auto-commit-savepoint wrap, tracked for post-v4.32. |

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
| 2.9 | Network partition tolerance [chaos] | Follower disconnect while primary writes → on reconnect, all writes catch up exactly once, no duplicates / no gaps | ⚠️ | logic supports it; v4.30 chaos test will assert (also rows 1.11 in-memory rollback) |

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
| 4.6 | Per-table row count / size metrics | `spg_table_rows{table=…}` series exposed via /metrics | ❌ | Deferred — cardinality matters at scale; needs an allowlist mechanism. v5 candidate. |
| 4.7 | Replication lag metric | Lag in bytes / seconds exposed via /metrics on the follower | ❌ | Deferred — follower currently has no notion of "primary's latest offset" beyond what it's applied. Needs protocol extension to query. v5 candidate. |
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
| 5.5 | Per-query memory cap | A query cannot allocate more than N MB on the server heap; over-cap errors before OOM | ⚠️ | `SPG_MAX_QUERY_ROWS` caps result-set rows (the biggest allocator) — that's the practical answer today. True allocator-level cap needs a custom global allocator hook; deferred to v5. |
| 5.6 | OOM injection survives [chaos] | Allocator returns NULL → server emits clear error to caller, no panic, no half-applied state | ⚠️ | Rust panics on alloc fail under default config; v4.29 chaos can stress this |
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

## 8. Stability + compatibility

Operator promise: client code written against v1.0 still works
against v1.x. Backup files captured by v1.x restore on v1.y.

| # | Item | Criterion | Status | Evidence |
|---|------|-----------|:------:|----------|
| 8.1 | SemVer adherence | Major bump for breaking changes; documented | ✅ | v4.x kept compatible since v4.0; pre-v1.0 contract |
| 8.2 | Native wire opcode stable | Opcodes 0x00-0x17 + 0xFF documented and never changed silently | ✅ | v1-status memory §Wire opcode table |
| 8.3 | PG-wire SCRAM stable | SCRAM-SHA-256 wire spec (RFC 5802) — by spec, not by SPG | ✅ | RFC reference |
| 8.4 | Snapshot file backwards-compat [machine] | v4.x can load every snapshot ever written (FILE_VERSION 1..8) | ✅ | v4.31, `tests/cross_version_compat.rs` walks every directory under `xtests/compat-fixtures/` |
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

Last machine run: v4.33 (2026-05-27).

```
Total rows in checklist : 85
  ✅ pass             : 71
  ⚠️ partial          : 6
  ❌ open             : 2
  🚫 out-of-scope     : 6

[machine] rows scaffolded in prod_ready.rs : 33
  row_1_3, row_1_9, row_1_10,
  row_2_5, row_2_6, row_2_7,
  row_3_7, row_3_8, row_3_9, row_3_10, row_3_11, row_3_12,
  row_4_1, row_4_2, row_4_5,
  row_5_1, row_5_7,
  row_6_3,
  row_7_2, row_7_3, row_7_4, row_7_5, row_7_6,
  row_8_2, row_8_4, row_8_5, row_8_6,
  row_9_2, row_9_8,
  row_10_x, row_10_4, row_10_5
```

v4.33 closed three rows that had been blocking the "external SaaS
user" bar:

- 2.7 graceful shutdown — previously ⚠️ (bounded by
  `SPG_QUERY_TIMEOUT_MS`); now ✅ with `SPG_SHUTDOWN_DEADLINE_SEC`
  + SIGTERM handler.
- 4.5 slow-query log — previously ❌; now ✅ with
  `SPG_SLOW_QUERY_LOG_MS` and a JSON-line event on stderr.
- 5.7 disk water-mark — previously ❌; now ✅ with
  `SPG_WAL_MIN_FREE_BYTES` and a `statvfs(2)` precheck before
  every WAL append.

All three landed as [machine]-checked e2e tests.

The 2 remaining ❌ items are observability nice-to-haves slotted
into v4.35 / v4.36 by NEXT.md:

- 4.6 per-table row count metric: cardinality allowlist (v4.35)
- 4.7 replication lag metric: needs follower → primary RPC (v4.36)

The 6 ⚠️ items are all "works today within documented limits;
strict invariant deferred per NEXT.md":

- 1.8 explicit CRC32 (today: truncation caught by length prefix; v4.37)
- 1.11 ENOSPC mid-`write_all` window (today: preflight catches
  the chaos case; full fix needs auto-commit savepoint; v4.34)
- 2.9 netsplit replication chaos (v4.36)
- 5.5 per-query memory cap: needs custom global allocator (v5.0)
- 5.6 OOM injection survives: needs alloc-error hook (v5.0)
- 7.8 startup config validation (v4.30 candidate; rolled forward)

Each subsequent release should update this snapshot and add new
`row_X_Y_*` tests for any [machine] rows it lights up.
