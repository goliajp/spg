# SPG next-steps roadmap (post v4.32)

Linear plan to close the remaining PROD_READY gaps. Six
checkpoints, dependency-ordered. Each row maps back to its
PROD_READY.md row ID so when the work lands you know exactly
which checkbox to flip.

Convention: each version is a "do-this-and-only-this" delivery.
Don't bundle. If something turns out to need v(N+1) work,
deliver v(N) as-is and move on.

---

## v4.33 — ops three-pack (graceful shutdown + slow query log + disk water-mark)

Closes the smallest, most independent, highest-operator-value
gaps. All three are isolated additions, no protocol or file-
format impact.

| # | item | est. | rows fixed |
|---|------|------|------------|
| 1 | **Graceful shutdown** — SIGTERM/SIGINT handler stops accepting new connections, drains in-flight queries up to a deadline (`SPG_SHUTDOWN_DEADLINE_SEC`, default 30), exits 0. SIGKILL bypasses (existing crash-recovery path covers it). | 1.5 d | 2.7 |
| 2 | **Slow-query log** — `SPG_SLOW_QUERY_LOG_MS` env var; queries exceeding the threshold emit a JSON line on stderr with `sql`, `elapsed_us`, `connection_id`, `role`. Defaults off. Reuses the existing `SPG_LOG_FORMAT=json` framework. | 0.5 d | 4.5 |
| 3 | **Disk water-mark** — `SPG_WAL_MIN_FREE_BYTES` env var; before each WAL append, `statvfs` checks free space on the WAL volume. Below the mark → refuse writes with a clear error, keep serving reads. Linux + Darwin both expose `statvfs`. | 1 d | 5.7 |

Dependencies: none.
Risk: low. All three are pure additions, default-off.
Test plan: each gets a `tests/e2e_*.rs` test; PROD_READY rows
flip to ✅ with [machine] tag once the row_X_Y_* shim is added.

---

## v4.34 — ENOSPC in-memory rollback (closes chaos #3 fully)

Today: the v4.30 preflight catches the chaos knob path
(`SPG_FAIL_WAL_QUOTA_BYTES`), but a *real* mid-`write_all`
ENOSPC still leaves the engine with a phantom row until restart.
This checkpoint closes that gap properly.

| # | item | est. | rows fixed |
|---|------|------|------------|
| 1 | **Auto-commit savepoint wrap** — when a write arrives outside an explicit TX, take an implicit SAVEPOINT before `engine.execute`; on WAL append success → RELEASE; on WAL append failure → ROLLBACK TO. The engine already has SAVEPOINT machinery (v1.13), so the change is in main.rs not the engine. | 2 d | 1.11 (fully ✅) |
| 2 | **Perf-regression check** — implicit savepoint per write adds engine overhead. Re-run `xbench/competitor/src/bin/latency.rs` and assert spg-server INSERT p99 stays within SLO (≤ 500 µs). If regression > 30 %, optimize before merge. | 0.5 d | maintains 10.4 |
| 3 | **Tighten chaos test** — extend `tests/e2e_chaos.rs::chaos_disk_full_…` to also assert the live in-memory count matches CC'd count *without* relying on the preflight (turn off the preflight, force the path through real WAL append failure). | 0.5 d | strengthens 1.10 |

Dependencies: v4.33 not required, but landing it first keeps
each release small.
Risk: medium — savepoint overhead could move INSERT p99. Bench
gates this.
Why not bundled into v4.33: this needs the perf gate to pass;
keeping it separate makes the regression bisect cleanly.

---

## v4.35 — per-table metrics with cardinality control

| # | item | est. | rows fixed |
|---|------|------|------------|
| 1 | **`spg_table_rows{table=…}` series** — exposed via `/metrics`. Each table contributes one gauge. | 0.5 d | 4.6 (partial) |
| 2 | **Cardinality allowlist** — `SPG_METRICS_TABLE_ALLOWLIST=t1,t2,...` env var. Default: only the 50 largest tables by row count are exported. Prevents Prometheus card blow-up for tenants with thousands of tables. | 0.5 d | completes 4.6 |
| 3 | **`spg_table_bytes{table=…}` series** — on-disk size approximation (rows × avg-row-bytes). Same allowlist applies. | 0.5 d | 4.6 |

Dependencies: none. Independent of v4.33/v4.34.
Risk: low. Pure observability addition; no SQL or wire change.
Why separate from v4.33: needs the cardinality design call,
which the v4.33 trio doesn't.

---

## v4.36 — replication: netsplit chaos + lag metric (paired delivery)

These two share the replication subsystem and benefit from
landing together — the lag metric needs the protocol extension
the netsplit test exercises.

| # | item | est. | rows fixed |
|---|------|------|------------|
| 1 | **Netsplit chaos test** — small TCP proxy in `xtests/` that sits between primary and follower; supports drop / delay / partition modes. New `tests/e2e_chaos_netsplit.rs` asserts: follower disconnect mid-stream → on reconnect, all CC'd writes arrive exactly once, no duplicates, no gaps. | 2 d | 2.9 |
| 2 | **Replication lag metric (protocol extension)** — primary's repl stream gets a small periodic status frame carrying current WAL pos (every 50 ms, alongside the existing tail-poll cadence). Follower computes `lag = primary_pos - follower_applied_pos`, exposes `spg_replication_lag_bytes` and `spg_replication_lag_seconds` via its `/metrics`. The status frame is a backwards-compat addition (new framing magic), gated by STABILITY.md. | 1.5 d | 4.7 |
| 3 | **Update STABILITY.md** — document the new status frame as part of the replication protocol's stable surface. | 0.25 d | maintains 8.5 |

Dependencies: v4.24 replication (already shipped). Netsplit
chaos doesn't depend on lag metric, but landing them together
exercises the same code path twice.
Risk: medium — TCP proxy infra needs to be reliable on macOS
+ Linux CI. Use stdlib `TcpListener`/`TcpStream` only, no extra
deps.

---

## v4.37 — file format v9: CRC32 checksums

| # | item | est. | rows fixed |
|---|------|------|------------|
| 1 | **WAL record CRC32** — append a u32 CRC32 to each WAL record (after the length prefix, before the SQL bytes). Replay verifies the CRC; mismatch → drop the record with a loud stderr warning, abort if mid-tail (vs. truncation). | 1 d | 1.8 (WAL half) |
| 2 | **Snapshot envelope CRC32** — bump `FILE_VERSION` 8 → 9. v9 envelope carries CRC32 of the catalog blob; v8 still readable (no CRC). Cross-version compat test gains a v4.37 fixture. | 1 d | 1.8 (snapshot half) |
| 3 | **Backup bundle CRC32** — bump `SPGBKUP\x02` magic with per-section CRC32. v\x01 stays readable. | 0.5 d | 1.8 (bundle) |
| 4 | **Bit-flip chaos test** — adversarial test: flip random bits in WAL records, assert CRC catches it (no silent corruption). | 0.5 d | strengthens 1.10 |

Dependencies: nothing required, but better to ship after v4.36
so the netsplit chaos infra is already there.
Risk: medium — file-format bump must keep the v8 read path
working. Cross-version test (v4.31) is the safety net; add a v4.37
fixture before merging.

---

## v5.0.0 — allocator-level memory cap + OOM survival

This is a SemVer major bump because it changes how Rust panics
work (process exit behavior) and adds a `#[global_allocator]`
that's hot-path-relevant.

| # | item | est. | rows fixed |
|---|------|------|------------|
| 1 | **Custom global allocator with per-query budget** — `#[global_allocator]` tracks per-thread bytes-allocated; `SPG_MAX_QUERY_BYTES` enforces cap; over → flip the existing CancelToken so the query loop bails at the next checkpoint. | 3 d | 5.5 (fully ✅) |
| 2 | **OOM survives** — `oom = "abort"` in Cargo.toml is the default; switch to a panic handler that returns clean error to the client when alloc fails, only abort if the panic happens during WAL replay. Stable Rust supports this via `set_alloc_error_hook` (unstable until then; use `oom_hook` or System allocator). | 2 d | 5.6 |
| 3 | **Perf-regression gate** — allocator hot path adds atomics; re-run latency bench, assert SLO ceiling still holds (the v4.32 ceiling has 6-7× headroom so should survive). | 0.5 d | maintains 10.4/10.5 |
| 4 | **STABILITY.md v2 contract** — restate frozen surfaces; explicitly note v5 cuts the SPG_FAIL_WAL_QUOTA_BYTES chaos knob now that real ENOSPC has full coverage. | 0.5 d | renews 8.5 |

Dependencies: v4.37 done (so file format v9 is stable before
v5 ships). Stable Rust feature: `set_alloc_error_hook` is
stable since 1.59 — fine.
Risk: high — global allocator change touches every allocation.
Bench gate is mandatory.
Why v5 not v4.38: SemVer says "anything that could break a
client" is major. Switching from `oom = abort` to handler is
observable from a client (they get an error instead of a closed
socket).

---

## What this roadmap does NOT include

- TLS — permanently 🚫 (see `[[spg-out-of-scope]]` memory).
- Automated failover — 🚫. Manual promotion via
  RESTORE_DRILL.md step 5 stays the supported path.
- Sharding / multi-master — 🚫. Single-master with read replicas
  is the architecture; horizontal write scaling is a v6+ topic.
- Migration framework — 🚫. DDL via standard SQL is the model.
- Multi-tenant isolation — 🚫. Run separate `spg-server`
  processes.
- Foreign keys / CHECK constraints / row-level ACL — 🚫. Cited
  in PROD_READY rows 6.4, 6.5, and out-of-scope memory.

These deferrals are intentional and don't reopen with each
roadmap pass.

---

## Effort summary

| version | what                             | est. days |
|---------|----------------------------------|----------:|
| v4.33   | ops three-pack                   |       3.0 |
| v4.34   | ENOSPC rollback                  |       3.0 |
| v4.35   | per-table metrics                |       1.5 |
| v4.36   | repl chaos + lag                 |       3.75|
| v4.37   | file format v9 + CRC             |       3.0 |
| v5.0.0  | allocator + OOM                  |       6.0 |
| **total** |                                |    **20.25 d** |

Pace: roughly 4-6 weeks of focused work to take SPG from
v4.32's prod-ready baseline to a v5.0 release that closes
every gap PROD_READY.md currently flags. Each checkpoint is
shippable in isolation.

---

## How this maps back to PROD_READY.md

After v5.0.0 lands, the audit table should look like:

| status  | count | notes |
|---------|------:|-------|
| ✅ pass | 79    | up from 68 (closed 11 rows across v4.33-v5.0) |
| ⚠️ partial | 0  | all ⚠️ rows promoted to ✅ |
| ❌ open    | 0  | all closed |
| 🚫 out-of-scope | 6 | unchanged — these are forever deferred |

At that point, SPG meets the "external SaaS / open-source user"
bar declared in the v4.28 sprint kickoff. The next sprint
target — if there is one — would be horizontal scaling (v6).
