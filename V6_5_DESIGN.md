# SPG v6.5 design — Observability v2

> Drafted 2026-06-03 after v6.4 series shipped (SQL polish + JSON
> path; tag `v6.4.8` rolled the series up at commit `d61f26c`).
> Scope: v6.5 series (v6.5.0 → v6.5.7).
> Companion research:
>   `.claude/researches/spg-vs-pg19-comparison.md` §1.13 / §2.3
>   `.claude/researches/spg-v6-roadmap-from-pg19.md` §3.v6.5
> Predecessor designs: `V6_DESIGN.md`, `V6_1_DESIGN.md`,
> `V6_2_DESIGN.md`, `V6_3_DESIGN.md`, `V6_4_DESIGN.md`.

## L0 — v7.0 discipline (inherited from V6_2_DESIGN)

Same rule:

> **NO ITEM in any v6.x sub-version design may be deferred to a
> later minor without an explicit user-level "OK to defer".**

Deferrals must target a later same-minor sub-version in this
file. Future means a STABILITY §"Out of scope" entry. v6.4
ship rollup demonstrated the discipline: design errors got
explicit OOS entries, not silent push-forwards.

## L1 — Roadmap

v6.5 closes the **thirteenth-gap cluster** from the PG-19
audit: SQL-queryable runtime state. SPG today exposes:
  - HTTP `/metrics` Prometheus endpoint (v5.x)
  - `SHOW PUBLICATIONS` / `SHOW SUBSCRIPTIONS`
  - `spg_statistic` virtual table (v6.2)
  - `SHOW TABLES` / `SHOW COLUMNS` / `SHOW USERS`

What's missing — PG operators expect to grep these from `psql`:
  - Per-connection activity (current SQL, wait state, elapsed)
  - Replication slot lag, recovery position
  - Cold-tier segment inventory + size
  - Slow-query log surface as SQL-queryable view
  - Audit-chain verification SQL function
  - DDL reverse-introspection (`spg_get_table_ddl(t)`)
  - Wait events at the engine.write_lock / fsync / group-commit
    boundary

v6.5 lands:

1. **`spg_stat_replication`** virtual table — one row per active
   subscription with `(name, conn_str, publications,
   last_received_pos, lag_bytes, lag_seconds, status)`.
2. **`spg_stat_recovery`** virtual table — single row:
   `(in_recovery, current_wal_pos, last_durable_pos,
   apply_lag_bytes)`.
3. **`spg_stat_segment`** virtual table — one row per cold-tier
   segment with `(segment_id, table_name, row_count, bytes,
   created_at, last_promoted_at)`.
4. **`spg_stat_query`** virtual table — one row per
   recently-seen distinct query plan with `(plan_hash, sql,
   exec_count, total_elapsed_us, mean_elapsed_us, max_elapsed_us,
   last_seen)`. Bounded LRU cap (1024 entries).
5. **`spg_stat_activity`** virtual table — one row per active
   pgwire connection with `(pid, user_name, started_at,
   current_sql, wait_event, elapsed_us, in_transaction)`.
6. **`spg_audit_verify(from_ts, to_ts)`** SQL function — re-walks
   the audit log over the timestamp range, recomputes the BLAKE3
   hash chain, returns `(verified_count, broken_at_seq)`.
7. **`spg_audit_chain`** virtual table — exposes every audit
   entry as a row (`seq`, `actor`, `verb`, `target`, `payload`,
   `prev_hash`, `entry_hash`, `ts_us`).
8. **`spg_get_role_ddl(role_name)`** / **`spg_get_database_ddl()`** /
   **`spg_get_table_ddl(t)`** SQL functions — return the CREATE
   statement(s) that would reconstruct the named object.
9. **Wait events lite** — engine.write_lock, fsync_wait,
   group_commit_wait per-thread timers exposed in
   `spg_stat_activity.wait_event`.
10. **Defaults rebaseline** — slow-query log defaults to 100 ms
    (was unlimited); WAL flusher latency surfaces via wait event.
11. **`SPG_PLAN_CACHE_MAX` env var** (carve-out from v6.3.0).
    Operator knob.

Hard rules unchanged: **0 external dependencies, no `unsafe`
(aarch64 NEON carve-out only), WAL on-disk format frozen,
sqllogictest 100 % pass rate maintained**.

### Goal numbers (v6.5 ship-gate definition)

| metric | v6.4.8 baseline | v6.5 target | competitor reference |
|--------|-----------------|------------:|----------------------|
| `SELECT * FROM spg_stat_activity` returns active conns | unsupported | **N rows for N pgwire connections** | PG `pg_stat_activity` |
| `SELECT * FROM spg_stat_segment` returns cold inventory | unsupported | **1 row per segment** | PG `pg_class` |
| `spg_audit_verify(t0, t1)` re-walks chain in O(N) | unsupported | **errors loudly on a single bit flip** | append-only-log standard |
| `spg_get_table_ddl(t)` returns CREATE TABLE round-trip | unsupported | **byte-equal re-parse** | PG `\d t` parity |
| Slow-query log default threshold | unlimited | **100 ms** | PG `log_min_duration_statement` |
| sqllogictest 4-corpus regression | 100 % | **100 %** | unchanged |

### Out of v6.5 (carved out)

- **pg_stat_database** / **pg_stat_user_tables** / per-table row
  counts. SPG's catalog doesn't keep persistent per-table modify
  counters beyond v6.2.1's auto-analyze tracker. The auto-analyze
  counter is exposed via `spg_statistic`'s `modified_since`, but
  the broader PG-shaped per-table counters (n_tup_ins, n_tup_upd,
  n_dead_tup) aren't tracked. Out of v6.x.
- **Per-query EXPLAIN cache** — `spg_stat_query` holds the SQL
  text + elapsed timings, NOT the cached EXPLAIN tree. Joining
  spg_stat_query with EXPLAIN ANALYZE is operator-driven (run
  EXPLAIN against the SQL text). Cached EXPLAIN trees would
  duplicate plan-cache memory; out of v6.x.
- **Wait-event sub-event detail** (PG has ~150 named wait events
  like `Client/ClientRead`, `LWLock/buffer_content`). v6.5 ships
  3 wait events (write_lock / fsync / group_commit). More
  granular events out of v6.x.
- **`pg_stat_statements` extension surface**. PG ships this as a
  loadable extension. SPG's `spg_stat_query` is the equivalent
  surface but doesn't aim for byte-for-byte column compatibility
  with `pg_stat_statements`.
- **Streaming `notify` of stat changes** — `spg_stat_activity`
  is point-in-time; row-version notifications are out of v6.x.
- **WAL receiver / decoded WAL inspection** (`pg_get_wal_records`).
  SPG's WAL format is internal; full WAL introspection is a
  separate large surface.

## L2 — Version boundaries (v6.5.0 → v6.5.7)

| ver | scope | ship-gate | depends on |
|-----|-------|-----------|------------|
| **v6.5.0** | `spg_stat_replication` + `spg_stat_segment` virtual tables. Read-only — same dispatch pattern as `spg_statistic` (engine recognises the bare name in FROM; routes to `exec_spg_stat_replication` / `exec_spg_stat_segment` that materialise rows from existing state — `Engine.subscriptions` + `Catalog::cold_segment_ids_global`). | `tests/e2e_spg_stat_views::replication_lists_subscriptions` + `…::segment_lists_cold_inventory` + `…::empty_when_no_subs_or_segments` | v6.4.8 |
| **v6.5.1** | `spg_stat_query` virtual table — per-distinct-SQL LRU stat collector. New struct `QueryStats` on `Engine` (1024-entry LRU, atomic counters per entry). Hook in `exec_inner_with_cancel` records the elapsed time. Virtual table dispatch returns `(plan_hash, sql, exec_count, total_us, mean_us, max_us, last_seen_us)`. | `tests/e2e_spg_stat_query::counter_increments_on_each_execute` + `…::lru_evicts_oldest_distinct_sql` + `…::stats_visible_through_pgwire` | v6.5.0 |
| **v6.5.2** | `spg_stat_activity` virtual table — per-pgwire-connection state. New `ServerState.connections: RwLock<Vec<ConnState>>` registry populated at connection accept, mutated on each Q/Execute, removed on close. `ConnState { pid, user, started_at, current_sql, wait_event, elapsed_us, in_transaction }`. Virtual table dispatch reads the registry snapshot. | `tests/e2e_spg_stat_activity::two_open_connections_each_have_a_row` + `…::current_sql_updates_during_execute` + `…::row_drops_on_close` | v6.5.0 + spg-server connection accept site |
| **v6.5.3** | `spg_audit_verify(from_ts, to_ts)` SQL function + `spg_audit_chain` virtual table. Function re-walks the audit log entries within the timestamp range, recomputes the BLAKE3 chain, returns `(verified_count, first_broken_seq)`. Virtual table exposes the full chain. | `tests/e2e_audit_verify::clean_log_verifies` + `…::tampered_entry_detected` + `…::chain_table_lists_all_entries` | v6.4.8 (uses existing audit log) |
| **v6.5.4** | DDL introspection SQL functions: `spg_get_role_ddl(role_name)`, `spg_get_database_ddl()`, `spg_get_table_ddl(t)`. Each returns a TEXT containing the CREATE … statement that round-trips through `Engine::execute` to recreate the object. | `tests/e2e_get_ddl::table_round_trip` + `…::role_round_trip` + `…::database_round_trip_includes_users_and_tables` | v6.4.8 |
| **v6.5.5** | Wait events lite. `ConnState.wait_event` populated when a thread waits at `engine.write_lock()` (event=`write_lock`), `flusher::sync_data()` (event=`fsync`), or `group_commit::wait()` (event=`group_commit`). Atomic CAS at entry / clear at exit. Virtual table `spg_stat_activity` surfaces the live value. | `tests/e2e_wait_events::write_lock_wait_appears_under_contention` + `…::fsync_wait_visible_during_durability_checkpoint` | v6.5.2 (needs ConnState) |
| **v6.5.6** | Defaults rebaseline. `SPG_SLOW_QUERY_THRESHOLD_MS` env (default 100). `SPG_PLAN_CACHE_MAX` env (default 256, carved from v6.3.0). Slow queries emit `log_event("slow_query", …)`. | `tests/e2e_slow_query::query_above_threshold_logs` + `…::query_below_does_not` + `…::plan_cache_cap_overridable_via_env` | v6.5.1 (uses QueryStats) |
| **v6.5.7** | v6.5 ship rollup — CHANGELOG header, PROD_READY rows 7.32 – 7.38, STABILITY §"Observability v2 (v6.5 series)" + carve-outs. | rollup-only; CHANGELOG / PROD_READY / STABILITY merged; 4-corpus 100 %; every v6.5.x e2e from rows above passes. | v6.5.0 → v6.5.6 all |

### Estimated effort

| sub-version | est. days | running total |
|-------------|----------:|--------------:|
| v6.5.0 | 1.0 | 1.0 |
| v6.5.1 | 1.5 | 2.5 |
| v6.5.2 | 2.0 | 4.5 |
| v6.5.3 | 1.0 | 5.5 |
| v6.5.4 | 1.5 | 7.0 |
| v6.5.5 | 1.5 | 8.5 |
| v6.5.6 | 0.5 | 9.0 |
| v6.5.7 | 0.5 | 9.5 |

Roadmap estimate was 9 d; v6.5.7 ship rollup adds 0.5 d.

## Architectural deliberations

### 1 — Virtual table dispatch pattern

v6.2.0 set the pattern with `spg_statistic`:
  - Engine recognises a bare-name FROM in `Statement::Select`
  - Routes to a private `exec_<table>` method
  - Materialises rows from internal state on demand
  - Read-only — INSERT / UPDATE / DELETE error

v6.5's new virtual tables inherit the pattern verbatim. The
short-circuit lives in `exec_select_cancel` alongside the existing
`spg_statistic` branch. Column order is **frozen from v6.5.0**
once shipped; later v6.5.x can append columns to the right but
not reorder.

### 2 — `spg_stat_query` LRU cap: 1024 entries

`pg_stat_statements` defaults to 5000. SPG ships 1024 because:
  - Most apps reuse < 200 distinct prepared statements
  - At 1024 the worst-case memory footprint is ~1 MiB
    (avg 1 KiB per entry for SQL text + counters)
  - Eviction is single-Vec LRU; sweep is microseconds

Cap is `pub(crate) const QUERY_STATS_MAX = 1024` in
`crates/spg-engine/src/query_stats.rs`. Operator-tunable knob:
`SPG_QUERY_STATS_MAX` env var (v6.5.6).

### 3 — `spg_stat_activity` thread-safety

`ServerState.connections: RwLock<Vec<ConnState>>` is the
straightforward choice. Per-connection threads hold a read lock
to mutate their own slot (each ConnState has an inner AtomicU64
for elapsed / wait event); the virtual table dispatch takes a
read lock and clones the row vector.

A subtlety: holding a read lock while writing `current_sql` (a
String) requires interior mutability. v6.5.2 uses
`Arc<RwLock<String>>` for the per-conn current_sql so the outer
Vec lock can stay shared on the read side.

### 4 — Audit chain verification re-walks O(N)

The audit log is append-only with a BLAKE3 hash chain (v4.x).
`spg_audit_verify(t0, t1)` filters entries to the time range,
re-hashes every entry, compares against the stored hash. O(N)
in the range size; for typical operator workloads (verify last
24 h) that's < 100K entries. No incremental verification —
explicit ranges only.

### 5 — DDL introspection: serialise vs reconstruct

Two approaches:
  a) Persistent CREATE-text storage. Engine stores the original
     CREATE TABLE string at table creation time; returned
     verbatim by `spg_get_table_ddl`. Lossless but bloats the
     catalog by ~512 bytes per table.
  b) Reconstruct from catalog state. Walk the columns + indexes
     + constraints, synthesise CREATE TABLE. Lossless ⇔ catalog
     captures every detail. SPG today captures column name +
     type + nullable + default + auto_increment; vector
     dimension + encoding for VECTOR; index name + method +
     column for indexes. Sufficient for byte-equal round-trip
     after parse+display normalisation.

**Decided: (b)**. The catalog already carries every detail we'd
need; persistent CREATE text would double-serialise and require
keeping the two in sync on ALTER. v6.5.4 ships pure
reconstruction.

### 6 — Wait events lite: 3 events, AtomicU8 encoding

PG has ~150 wait events. v6.5.5 ships 3 that map to SPG's actual
synchronisation points:
  - 0 = idle / no wait
  - 1 = write_lock (waiting on engine.write_lock())
  - 2 = fsync (inside flusher::sync_data)
  - 3 = group_commit (inside group_commit::wait)

`ConnState.wait_event: AtomicU8` written by the synchronisation
sites (CAS to 1/2/3 at entry, store 0 at exit). Read by
`spg_stat_activity` virtual table; rendered as the text label
("write_lock" / "fsync" / "group_commit" / "" for idle).

### 7 — Defaults rebaseline

Slow-query log default: 100 ms. Threshold-passing queries emit
a structured log line via `observability::log_event`. The full
SQL is logged (NOT redacted — operator can manage at log
ingestion). Threshold is per-process via env; 0 disables.

Lock waits default: ON. Acquiring `engine.write_lock` always
sets wait_event=1 before the lock call; clearing after the
critical section. No env var to disable (the cost is one atomic
store; not measurable in practice).

## L3a — Hot plan for v6.5.0 (the only sub-version that's "next")

Goal: ship `spg_stat_replication` + `spg_stat_segment` virtual
tables. No `spg_stat_query` yet (v6.5.1), no `spg_stat_activity`
yet (v6.5.2).

### Step 1 — Virtual-table dispatch extension

In `exec_select_cancel`, after the existing `spg_statistic`
branch:

```rust
if let Some(from) = &stmt.from
    && stmt.items.iter().any(|i| matches!(i, SelectItem::Wildcard))
    && from.joins.is_empty()
{
    match from.primary.name.to_ascii_lowercase().as_str() {
        "spg_statistic" => return Ok(self.exec_spg_statistic()),
        "spg_stat_replication" => return Ok(self.exec_spg_stat_replication()),
        "spg_stat_segment" => return Ok(self.exec_spg_stat_segment()),
        _ => {}
    }
}
```

### Step 2 — `exec_spg_stat_replication`

```rust
fn exec_spg_stat_replication(&self) -> QueryResult {
    let columns = vec![
        ColumnSchema::new("name", DataType::Text, false),
        ColumnSchema::new("conn_str", DataType::Text, false),
        ColumnSchema::new("publications", DataType::Text, false),
        ColumnSchema::new("last_received_pos", DataType::BigInt, true),
        ColumnSchema::new("enabled", DataType::Bool, false),
    ];
    let rows = self.subscriptions.iter().map(|s| {
        Row::new(vec![
            Value::Text(s.name.clone()),
            Value::Text(s.conn_str.clone()),
            Value::Text(s.publications.join(",")),
            s.last_received_pos.map(Value::BigInt).unwrap_or(Value::Null),
            Value::Bool(s.enabled),
        ])
    }).collect();
    QueryResult::Rows { columns, rows }
}
```

### Step 3 — `exec_spg_stat_segment`

Same shape but reads from `Catalog::cold_segment_ids_global()`
and walks each segment's metadata. Returns
`(segment_id, table_name, row_count, bytes, created_at)`.

### Step 4 — Tests

```text
crates/spg-server/tests/e2e_spg_stat_views.rs
  ├── replication_lists_subscriptions
  ├── segment_lists_cold_inventory
  └── empty_when_no_subs_or_segments
```

### Step 5 — Acceptance

- `cargo test -p spg-engine --lib` green
- `cargo run -q -p sqllogictest --release` → 4-corpus 100%
- New e2e tests pass

Commit message: `v6.5.0: spg_stat_replication + spg_stat_segment virtual tables`.
