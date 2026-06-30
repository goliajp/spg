//! `spg_*` introspection views and admin/stats API. Lifted out of
//! `lib.rs` (v7.32 engine modularisation). The `exec_spg_*` methods
//! materialise the `spg_statistic` / `spg_stat_*` / `spg_*_ddl` /
//! `spg_audit_*` meta-views dispatched from the meta-view SELECT path;
//! the public `memory_stats` / `set_plan_cache_max` / `query_stats` /
//! `tables_needing_analyze` methods form the embedded admin surface.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use spg_storage::{ColumnSchema, DataType, Row, Value};

use crate::{
    ActivityProvider, AuditChainProvider, AuditVerifier, Engine, MemoryStats, QueryResult,
    SlowQueryLogger, TableMemoryStats, approx_row_bytes, is_internal_table_name,
    render_create_table, render_histogram_bounds,
};
use crate::{query_stats, statistics};

impl Engine {
    /// v6.2.0 — materialise `spg_statistic` rows. One row per
    /// `(table, column)` pair tracked in `Statistics`, with
    /// `histogram_bounds` rendered as a `[v0, v1, ...]` string —
    /// the same canonical form vector literals use for round-trip.
    pub(crate) fn exec_spg_statistic(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("table_name", DataType::Text, false),
            ColumnSchema::new("column_name", DataType::Text, false),
            ColumnSchema::new("null_frac", DataType::Float, false),
            ColumnSchema::new("n_distinct", DataType::BigInt, false),
            ColumnSchema::new("histogram_bounds", DataType::Text, false),
            // v6.7.0 — appended column (v6.2.0 stability contract
            // allows APPEND to spg_statistic, not reorder/rename).
            // Reports the cached per-table cold-row count; same
            // value across every column row of the same table.
            ColumnSchema::new("cold_row_count", DataType::BigInt, false),
        ];
        let rows: Vec<Row<'static>> = self
            .statistics
            .iter()
            .map(|((t, c), s)| {
                let cold = self
                    .catalog
                    .get(t)
                    .map_or(0, |table| table.cold_row_count());
                Row::new(alloc::vec![
                    Value::text(t.clone()),
                    Value::text(c.clone()),
                    Value::Float(f64::from(s.null_frac)),
                    Value::BigInt(i64::try_from(s.n_distinct).unwrap_or(i64::MAX)),
                    Value::text(render_histogram_bounds(&s.histogram_bounds)),
                    Value::BigInt(i64::try_from(cold).unwrap_or(i64::MAX)),
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v6.5.0 — materialise `spg_stat_replication` rows. One row
    /// per subscription with `(name, conn_str, publications,
    /// last_received_pos, enabled)`. Surface mirrors
    /// `SHOW SUBSCRIPTIONS` but follows the virtual-table dispatch
    /// shape so it composes with SELECT clauses (WHERE, projection
    /// onto specific columns, etc).
    pub(crate) fn exec_spg_stat_replication(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("name", DataType::Text, false),
            ColumnSchema::new("conn_str", DataType::Text, false),
            ColumnSchema::new("publications", DataType::Text, false),
            ColumnSchema::new("last_received_pos", DataType::BigInt, false),
            ColumnSchema::new("enabled", DataType::Bool, false),
        ];
        let rows: Vec<Row<'static>> = self
            .subscriptions
            .iter()
            .map(|(name, sub)| {
                Row::new(alloc::vec![
                    Value::text(name.clone()),
                    Value::text(sub.conn_str.clone()),
                    Value::text(sub.publications.join(",")),
                    Value::BigInt(i64::try_from(sub.last_received_pos).unwrap_or(i64::MAX)),
                    Value::Bool(sub.enabled),
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v6.5.0 — materialise `spg_stat_segment` rows. One row per
    /// cold-tier segment with `(segment_id, num_rows, num_pages,
    /// total_bytes)`.
    ///
    /// v6.7.0 — appended `table_name` column resolves the v6.5.0
    /// carve-out. Walks every user table's BTree indices to find
    /// which table's Cold locators point at each segment. Empty
    /// string for orphan segments (loaded via SPG_PRELOAD_COLD_SEGMENT
    /// before any index registered a locator). The walk is
    /// O(tables × indices × keys); cached per call, not across
    /// calls — re-walked on every `SELECT * FROM spg_stat_segment`.
    /// v7.31 (memory campaign) — walk the committed catalog and
    /// build the per-bucket memory snapshot. O(rows + index
    /// entries): operator/monitoring surface, not a query path.
    pub fn memory_stats(&self) -> MemoryStats {
        let mut tables: Vec<TableMemoryStats> = Vec::new();
        let (mut total_enc, mut total_res, mut total_idx) = (0u64, 0u64, 0u64);
        for tname in self.catalog.table_names() {
            if is_internal_table_name(&tname) {
                continue;
            }
            let Some(t) = self.catalog.get(&tname) else {
                continue;
            };
            let resident: u64 = t.rows().iter().map(|r| approx_row_bytes(r) as u64).sum();
            // v7.31 C2 — each index variant accounts for its own
            // resident bytes by walking its real structure (NSW layer
            // adjacency, GIN posting lists), replacing the old inline
            // parametric estimate that mis-sized NSW and flat-tokened
            // every GIN family index.
            let mut idx_bytes: u64 = 0;
            for idx in t.indices() {
                idx_bytes += idx.kind.approx_resident_bytes();
            }
            total_enc += t.hot_bytes();
            total_res += resident;
            total_idx += idx_bytes;
            tables.push(TableMemoryStats {
                name: tname.clone(),
                hot_rows: t.rows().len() as u64,
                cold_rows: t.cold_row_count(),
                hot_encoded_bytes: t.hot_bytes(),
                approx_resident_bytes: resident,
                index_count: t.indices().len() as u64,
                approx_index_bytes: idx_bytes,
            });
        }
        MemoryStats {
            tables,
            total_hot_encoded_bytes: total_enc,
            total_approx_resident_bytes: total_res,
            total_approx_index_bytes: total_idx,
            max_query_bytes: self.max_query_bytes,
            // Bucket D belongs to the durable host (embed / server),
            // not the engine — filled in there (C2).
            wal_bytes: None,
        }
    }

    /// v7.31 — `SELECT * FROM spg_memory_stats`: one row per user
    /// table (same numbers as `Engine::memory_stats()`), so the
    /// server path gets the meter through plain SQL.
    pub(crate) fn exec_spg_memory_stats(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("table_name", DataType::Text, false),
            ColumnSchema::new("hot_rows", DataType::BigInt, false),
            ColumnSchema::new("cold_rows", DataType::BigInt, false),
            ColumnSchema::new("hot_encoded_bytes", DataType::BigInt, false),
            ColumnSchema::new("approx_resident_bytes", DataType::BigInt, false),
            ColumnSchema::new("index_count", DataType::BigInt, false),
            ColumnSchema::new("approx_index_bytes", DataType::BigInt, false),
        ];
        #[allow(clippy::cast_possible_wrap)]
        let rows: Vec<Row<'static>> = self
            .memory_stats()
            .tables
            .into_iter()
            .map(|t| {
                Row::new(alloc::vec![
                    Value::text(t.name),
                    Value::BigInt(t.hot_rows as i64),
                    Value::BigInt(t.cold_rows as i64),
                    Value::BigInt(t.hot_encoded_bytes as i64),
                    Value::BigInt(t.approx_resident_bytes as i64),
                    Value::BigInt(t.index_count as i64),
                    Value::BigInt(t.approx_index_bytes as i64),
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    pub(crate) fn exec_spg_stat_segment(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("segment_id", DataType::BigInt, false),
            ColumnSchema::new("table_name", DataType::Text, false),
            ColumnSchema::new("num_rows", DataType::BigInt, false),
            ColumnSchema::new("num_pages", DataType::BigInt, false),
            ColumnSchema::new("total_bytes", DataType::BigInt, false),
        ];
        // v6.7.0 — build a segment_id → table_name map by walking
        // every user table's BTree indices once. O(tables × indices
        // × keys) for the v6.5.0 carve-out resolution; acceptable
        // because spg_stat_segment is operator-facing (not on a
        // hot-loop path).
        let mut segment_owners: alloc::collections::BTreeMap<u32, String> = BTreeMap::new();
        for tname in self.catalog.table_names() {
            if is_internal_table_name(&tname) {
                continue;
            }
            let Some(t) = self.catalog.get(&tname) else {
                continue;
            };
            for idx in t.indices() {
                if let spg_storage::IndexKind::BTree(map) = &idx.kind {
                    for (_, locs) in map.iter() {
                        for loc in locs {
                            if let spg_storage::RowLocator::Cold { segment_id, .. } = loc {
                                segment_owners
                                    .entry(*segment_id)
                                    .or_insert_with(|| tname.clone());
                            }
                        }
                    }
                }
            }
        }
        let rows: Vec<Row<'static>> = self
            .catalog
            .cold_segment_ids_global()
            .iter()
            .filter_map(|&id| {
                let seg = self.catalog.cold_segment(id)?;
                let meta = seg.meta();
                let owner = segment_owners.get(&id).cloned().unwrap_or_default();
                Some(Row::new(alloc::vec![
                    Value::BigInt(i64::from(id)),
                    Value::text(owner),
                    Value::BigInt(i64::try_from(meta.num_rows).unwrap_or(i64::MAX)),
                    Value::BigInt(i64::from(meta.num_pages)),
                    Value::BigInt(i64::try_from(meta.total_bytes).unwrap_or(i64::MAX)),
                ]))
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v6.5.1 — materialise `spg_stat_query` rows. One row per
    /// distinct SQL text recorded since the engine booted, capped
    /// at `QUERY_STATS_MAX` (1024). Columns:
    ///   sql, exec_count, total_us, mean_us, max_us, last_seen_us
    /// mean_us = total_us / exec_count (saturating).
    pub(crate) fn exec_spg_stat_query(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("sql", DataType::Text, false),
            ColumnSchema::new("exec_count", DataType::BigInt, false),
            ColumnSchema::new("total_us", DataType::BigInt, false),
            ColumnSchema::new("mean_us", DataType::BigInt, false),
            ColumnSchema::new("max_us", DataType::BigInt, false),
            ColumnSchema::new("last_seen_us", DataType::BigInt, false),
        ];
        let rows: Vec<Row<'static>> = self
            .query_stats
            .snapshot()
            .into_iter()
            .map(|(sql, s)| {
                let mean = if s.exec_count == 0 {
                    0
                } else {
                    s.total_us / s.exec_count
                };
                Row::new(alloc::vec![
                    Value::text(sql),
                    Value::BigInt(i64::try_from(s.exec_count).unwrap_or(i64::MAX)),
                    Value::BigInt(i64::try_from(s.total_us).unwrap_or(i64::MAX)),
                    Value::BigInt(i64::try_from(mean).unwrap_or(i64::MAX)),
                    Value::BigInt(i64::try_from(s.max_us).unwrap_or(i64::MAX)),
                    Value::BigInt(i64::try_from(s.last_seen_us).unwrap_or(i64::MAX)),
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v6.5.2 — register a connection-state provider. spg-server
    /// calls this at startup with a function that snapshots its
    /// per-pgwire-connection registry. Engine reads through the
    /// callback on `SELECT * FROM spg_stat_activity`.
    #[must_use]
    pub const fn with_activity_provider(mut self, f: ActivityProvider) -> Self {
        self.activity_provider = Some(f);
        self
    }

    /// v6.5.3 — register audit chain provider + verifier.
    #[must_use]
    pub const fn with_audit_providers(
        mut self,
        chain: AuditChainProvider,
        verify: AuditVerifier,
    ) -> Self {
        self.audit_chain_provider = Some(chain);
        self.audit_verifier = Some(verify);
        self
    }

    /// v6.5.6 — register a slow-query log callback. `threshold_us`
    /// is the floor (in microseconds); only executes above the floor
    /// fire the callback. spg-server wires this from
    /// `SPG_SLOW_QUERY_THRESHOLD_MS` (default 100 ms).
    #[must_use]
    pub const fn with_slow_query_log(mut self, threshold_us: u64, logger: SlowQueryLogger) -> Self {
        self.slow_query_threshold_us = Some(threshold_us);
        self.slow_query_logger = Some(logger);
        self
    }

    /// v6.5.6 — operator knob for plan cache cap. spg-server reads
    /// `SPG_PLAN_CACHE_MAX` env at startup; uses this to override
    /// the compile-time default of 256.
    pub fn set_plan_cache_max(&mut self, n: usize) {
        self.plan_cache.set_max_entries(n);
    }

    /// v6.5.2 — materialise `spg_stat_activity` rows. Pulls a fresh
    /// snapshot from the registered `ActivityProvider`. Returns an
    /// empty result set when no provider is registered (the no_std
    /// embedded path with no pgwire layer).
    pub(crate) fn exec_spg_stat_activity(&self) -> QueryResult {
        // v7.37.14 (B6.3) — column order matches PG's
        // pg_stat_activity for `wait_event_type` immediately before
        // `wait_event` so client-side projection by ordinal stays
        // robust even before adopters update to named projection.
        let columns = alloc::vec![
            ColumnSchema::new("pid", DataType::Int, false),
            ColumnSchema::new("user", DataType::Text, false),
            ColumnSchema::new("started_at_us", DataType::BigInt, false),
            ColumnSchema::new("current_sql", DataType::Text, false),
            ColumnSchema::new("wait_event_type", DataType::Text, false),
            ColumnSchema::new("wait_event", DataType::Text, false),
            ColumnSchema::new("elapsed_us", DataType::BigInt, false),
            ColumnSchema::new("in_transaction", DataType::Bool, false),
            ColumnSchema::new("application_name", DataType::Text, false),
        ];
        let rows: Vec<Row<'static>> = self
            .activity_provider
            .map(|f| f())
            .unwrap_or_default()
            .into_iter()
            .map(|r| {
                Row::new(alloc::vec![
                    Value::Int(i32::try_from(r.pid).unwrap_or(i32::MAX)),
                    Value::text(r.user),
                    Value::BigInt(r.started_at_us),
                    Value::text(r.current_sql),
                    Value::text(r.wait_event_type),
                    Value::text(r.wait_event),
                    Value::BigInt(r.elapsed_us),
                    Value::Bool(r.in_transaction),
                    Value::text(r.application_name),
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v7.37.14 (B6.5) — materialise `pg_locks` rows. PG exposes a
    /// detailed lock table (locktype / database / relation /
    /// virtualtransaction / pid / mode / granted / fastpath /
    /// waitstart). SPG's single-writer + Arc-snapshot model means
    /// the v7.37.14 row set is structurally empty most of the time
    /// — there are no per-tuple locks to enumerate, and the global
    /// engine RwLock is either held or not (no chain to walk).
    /// v7.37.15 (per-row tuple lock implementation) populates rows
    /// from the live LockTable; the SQL surface ships now so
    /// adopters can already write monitoring queries / dashboards
    /// against the stable column set.
    pub(crate) fn exec_pg_locks(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("locktype", DataType::Text, false),
            ColumnSchema::new("database", DataType::Text, false),
            ColumnSchema::new("relation", DataType::Text, false),
            ColumnSchema::new("virtualtransaction", DataType::Text, false),
            ColumnSchema::new("pid", DataType::Int, false),
            ColumnSchema::new("mode", DataType::Text, false),
            ColumnSchema::new("granted", DataType::Bool, false),
            ColumnSchema::new("fastpath", DataType::Bool, false),
            ColumnSchema::new("waitstart_us", DataType::BigInt, false),
        ];
        // Empty row set until v7.37.15. Documented as the stable
        // SQL surface — the row content fills in once tuple locks
        // exist (B2.5 in AUDIT-3-categories).
        let rows: Vec<Row<'static>> = Vec::new();
        QueryResult::Rows { columns, rows }
    }

    /// v6.5.4 — materialise `spg_table_ddl` rows. One row per user
    /// table with `(table_name, ddl)`. Reconstructed from catalog
    /// state on demand.
    pub(crate) fn exec_spg_table_ddl(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("table_name", DataType::Text, false),
            ColumnSchema::new("ddl", DataType::Text, false),
        ];
        let rows: Vec<Row<'static>> = self
            .catalog
            .table_names()
            .into_iter()
            .filter(|n| !is_internal_table_name(n))
            .filter_map(|name| {
                let table = self.catalog.get(&name)?;
                let ddl = render_create_table(&name, &table.schema().columns);
                Some(Row::new(alloc::vec![Value::text(name), Value::text(ddl),]))
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v6.5.4 — materialise `spg_role_ddl` rows. One row per user
    /// with `(role_name, ddl)`. Password is redacted (matches the
    /// `Statement::CreateUser` Display which prints `'<redacted>'`).
    pub(crate) fn exec_spg_role_ddl(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("role_name", DataType::Text, false),
            ColumnSchema::new("ddl", DataType::Text, false),
        ];
        let rows: Vec<Row<'static>> = self
            .users
            .iter()
            .map(|(name, rec)| {
                let ddl = alloc::format!(
                    "CREATE USER {name} WITH PASSWORD '<redacted>' ROLE '{}'",
                    rec.role.as_str(),
                );
                Row::new(alloc::vec![
                    Value::text(String::from(name)),
                    Value::text(ddl)
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v6.5.4 — materialise `spg_database_ddl`: single row whose
    /// `ddl` column concatenates every user table's CREATE +
    /// every role's CREATE in deterministic catalog order. Suitable
    /// for piping back through `Engine::execute` to recreate a
    /// schema-equivalent database.
    pub(crate) fn exec_spg_database_ddl(&self) -> QueryResult {
        let columns = alloc::vec![ColumnSchema::new("ddl", DataType::Text, false)];
        let mut out = String::new();
        for (name, rec) in self.users.iter() {
            out.push_str(&alloc::format!(
                "CREATE USER {name} WITH PASSWORD '<redacted>' ROLE '{}';\n",
                rec.role.as_str(),
            ));
        }
        for name in self.catalog.table_names() {
            if is_internal_table_name(&name) {
                continue;
            }
            if let Some(table) = self.catalog.get(&name) {
                out.push_str(&render_create_table(&name, &table.schema().columns));
                out.push_str(";\n");
            }
        }
        QueryResult::Rows {
            columns,
            rows: alloc::vec![Row::new(alloc::vec![Value::text(out)])],
        }
    }

    /// v6.5.3 — materialise `spg_audit_chain` rows. Pulls a fresh
    /// snapshot from the registered provider; empty when no
    /// provider is set.
    pub(crate) fn exec_spg_audit_chain(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("seq", DataType::BigInt, false),
            ColumnSchema::new("ts_ms", DataType::BigInt, false),
            ColumnSchema::new("prev_hash", DataType::Text, false),
            ColumnSchema::new("entry_hash", DataType::Text, false),
            ColumnSchema::new("sql", DataType::Text, false),
        ];
        let rows: Vec<Row<'static>> = self
            .audit_chain_provider
            .map(|f| f())
            .unwrap_or_default()
            .into_iter()
            .map(|r| {
                Row::new(alloc::vec![
                    Value::BigInt(r.seq),
                    Value::BigInt(r.ts_ms),
                    Value::text(r.prev_hash_hex),
                    Value::text(r.entry_hash_hex),
                    Value::text(r.sql),
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v6.5.3 — materialise `spg_audit_verify` single-row result.
    /// `(verified_count, broken_at_seq)` — broken_at_seq is `-1`
    /// on a clean chain. Returns one row with both values 0 when
    /// no verifier is registered (no-data fallback for embedded
    /// callers).
    pub(crate) fn exec_spg_audit_verify(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("verified_count", DataType::BigInt, false),
            ColumnSchema::new("broken_at_seq", DataType::BigInt, false),
        ];
        let (verified, broken) = self.audit_verifier.map(|f| f()).unwrap_or((0, -1));
        let row = Row::new(alloc::vec![Value::BigInt(verified), Value::BigInt(broken),]);
        QueryResult::Rows {
            columns,
            rows: alloc::vec![row],
        }
    }

    /// v6.5.1 — read-only accessor for tests + v6.5.6 ops resets.
    pub fn query_stats(&self) -> &query_stats::QueryStats {
        &self.query_stats
    }

    /// v6.5.1 — mutable accessor (clear, etc).
    pub fn query_stats_mut(&mut self) -> &mut query_stats::QueryStats {
        &mut self.query_stats
    }

    /// v6.2.0 — read access to the per-column statistics table.
    /// Used by the planner (v6.2.2 selectivity functions read this),
    /// by `SELECT * FROM spg_statistic`, and by e2e tests.
    pub const fn statistics(&self) -> &statistics::Statistics {
        &self.statistics
    }

    /// v6.2.1 — return tables whose modified-row count crossed the
    /// auto-analyze threshold since the last ANALYZE on that table.
    /// The threshold is `0.1 × max(row_count, MIN_ROWS_FOR_AUTO_
    /// ANALYZE)` — combines PG-style fractional + absolute lower
    /// bound so a fresh / tiny table doesn't get hammered on every
    /// INSERT.
    ///
    /// Designed to be cheap: walks every user table's
    /// `Catalog::table_names()` + reads `statistics::modified_
    /// since_last_analyze()` (BTreeMap lookup). The background
    /// worker calls this under `engine.read()` then drops the lock
    /// before re-acquiring `engine.write()` for the actual ANALYZE.
    pub fn tables_needing_analyze(&self) -> Vec<String> {
        const MIN_ROWS: u64 = 100;
        let mut out = Vec::new();
        for name in self.catalog.table_names() {
            if is_internal_table_name(&name) {
                continue;
            }
            let Some(table) = self.catalog.get(&name) else {
                continue;
            };
            let row_count = table.rows().len() as u64;
            let modified = self.statistics.modified_since_last_analyze(&name);
            // Threshold: ceil(0.1 × max(row_count, MIN_ROWS)),
            // computed in integer arithmetic so spg-engine stays
            // no_std without pulling in libm. `(n + 9) / 10` is
            // `ceil(n / 10)` for non-negative `n`.
            let base = row_count.max(MIN_ROWS);
            let threshold = base.saturating_add(9) / 10;
            if modified >= threshold {
                out.push(name);
            }
        }
        out
    }
}
