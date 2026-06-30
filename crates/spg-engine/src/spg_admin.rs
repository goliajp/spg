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
    ActivityProvider, AuditChainProvider, AuditVerifier, Engine, EngineError, MemoryStats,
    QueryResult, SlowQueryLogger, TableMemoryStats, approx_row_bytes, is_internal_table_name,
    render_create_table, render_histogram_bounds,
};
use crate::{query_stats, statistics};

/// v7.37.16 (16.11) — render a PartitionBound for the
/// `spg_partition_health.bound_desc` column. Mirrors
/// `crate::partition::bound_to_diag` but lives here so this
/// crate's `spg_admin` module doesn't need to depend on the
/// engine-private partition helpers.
fn partition_bound_diag(b: &spg_storage::PartitionBound) -> String {
    use spg_storage::PartitionBound;
    match b {
        PartitionBound::MinValue => "MINVALUE".into(),
        PartitionBound::MaxValue => "MAXVALUE".into(),
        PartitionBound::TimestampTz(m) => alloc::format!("'{m}'::timestamptz"),
        PartitionBound::BigInt(n) => alloc::format!("{n}::bigint"),
        PartitionBound::Int(n) => alloc::format!("{n}::integer"),
        PartitionBound::SmallInt(n) => alloc::format!("{n}::smallint"),
        PartitionBound::Date(d) => alloc::format!("{d}::date"),
        PartitionBound::Text(s) => alloc::format!("'{}'", s.replace('\'', "''")),
    }
}

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

    /// v7.37.15 (Phase F) — MVCC diagnostic view. Single-row
    /// snapshot of the engine's per-row visibility state so
    /// `spgctl` / monitoring can observe vacuum lag + in-flight
    /// transaction count without reaching into engine internals.
    ///
    /// Columns:
    /// - `current_version` — the live monotonic writer-version
    ///   cursor (next allocated version comes after this).
    /// - `active_writer_count` — number of writer versions in
    ///   flight (= concurrent transactions). 0 means quiescent.
    /// - `oldest_active_version` — floor of the active set;
    ///   vacuum can reclaim any row whose `xmax < this`.
    pub(crate) fn exec_spg_stat_mvcc(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("current_version", DataType::BigInt, false),
            ColumnSchema::new("active_writer_count", DataType::Int, false),
            ColumnSchema::new("oldest_active_version", DataType::BigInt, false),
        ];
        let cv = spg_storage::row_header::current_version() as i64;
        let active = self.active_writer_versions.len() as i32;
        let oldest =
            self.active_writer_versions.iter().next().copied().unwrap_or(cv as u64) as i64;
        let rows = alloc::vec![Row::new(alloc::vec![
            Value::BigInt(cv),
            Value::Int(active),
            Value::BigInt(oldest),
        ])];
        QueryResult::Rows { columns, rows }
    }

    /// v7.37.16 (16.11 [PG+]) — materialise `spg_partition_health`
    /// rows: one row per partition (Range / List / Hash / Default /
    /// Parent), plus a "row_count" / "bound" diag column so dashboard
    /// queries can size a partitioned table at a glance without
    /// joining catalog tables. PG provides `pg_partitioned_table` +
    /// `pg_inherits` + per-child `pg_class.reltuples`; SPG bundles
    /// them into one easy view because dogfood / sentori dashboards
    /// kept reaching for it.
    ///
    /// Columns:
    ///   parent_name TEXT NOT NULL      -- parent table name, or
    ///                                     the partition name itself
    ///                                     when role == 'Parent'
    ///   partition_name TEXT NOT NULL   -- the partition (or
    ///                                     parent) name
    ///   role TEXT NOT NULL             -- 'Parent' | 'Range'
    ///                                     | 'List' | 'Hash'
    ///                                     | 'Default'
    ///   row_count BIGINT NOT NULL      -- live row count
    ///   bound_desc TEXT NOT NULL       -- human-readable bound for
    ///                                     diagnostics ('' for
    ///                                     Parent + DEFAULT)
    pub(crate) fn exec_spg_partition_health(&self) -> QueryResult {
        use spg_storage::PartitionRole;
        let columns = alloc::vec![
            ColumnSchema::new("parent_name", DataType::Text, false),
            ColumnSchema::new("partition_name", DataType::Text, false),
            ColumnSchema::new("role", DataType::Text, false),
            ColumnSchema::new("row_count", DataType::BigInt, false),
            ColumnSchema::new("bound_desc", DataType::Text, false),
        ];
        let mut rows: Vec<Row<'static>> = Vec::new();
        for name in self.catalog.table_names() {
            let Some(t) = self.catalog.get(&name) else {
                continue;
            };
            let role = match &t.schema().partition_role {
                None => continue,
                Some(r) => r,
            };
            let row_count = t.rows().len() as i64;
            let (parent, role_str, bound) = match role {
                PartitionRole::Parent { kind, .. } => {
                    let kind_str = match kind {
                        spg_storage::PartitionKind::Range => "RANGE",
                        spg_storage::PartitionKind::List => "LIST",
                        spg_storage::PartitionKind::Hash => "HASH",
                    };
                    (
                        name.clone(),
                        alloc::string::String::from("Parent"),
                        alloc::format!("PARTITION BY {kind_str}"),
                    )
                }
                PartitionRole::Range {
                    parent_name,
                    lower,
                    upper,
                } => (
                    parent_name.clone(),
                    alloc::string::String::from("Range"),
                    alloc::format!(
                        "FROM ({}) TO ({})",
                        partition_bound_diag(lower),
                        partition_bound_diag(upper)
                    ),
                ),
                PartitionRole::List {
                    parent_name,
                    values,
                } => {
                    let mut diag = alloc::string::String::from("IN (");
                    for (i, v) in values.iter().enumerate() {
                        if i > 0 {
                            diag.push_str(", ");
                        }
                        diag.push_str(&partition_bound_diag(v));
                    }
                    diag.push(')');
                    (parent_name.clone(), alloc::string::String::from("List"), diag)
                }
                PartitionRole::Hash {
                    parent_name,
                    modulus,
                    remainder,
                } => (
                    parent_name.clone(),
                    alloc::string::String::from("Hash"),
                    alloc::format!("WITH (MODULUS {modulus}, REMAINDER {remainder})"),
                ),
                PartitionRole::Default { parent_name } => (
                    parent_name.clone(),
                    alloc::string::String::from("Default"),
                    alloc::string::String::new(),
                ),
            };
            rows.push(Row::new(alloc::vec![
                Value::Text(alloc::borrow::Cow::Owned(parent)),
                Value::Text(alloc::borrow::Cow::Owned(name)),
                Value::Text(alloc::borrow::Cow::Owned(role_str)),
                Value::BigInt(row_count),
                Value::Text(alloc::borrow::Cow::Owned(bound)),
            ]));
        }
        QueryResult::Rows { columns, rows }
    }

    /// v7.37.22 (22.1) — materialise `pg_stat_statements` rows with
    /// PG's exact column shape. The data source is the same
    /// `query_stats` registry that backs `spg_stat_query`, but the
    /// surface is PG-compatible so dashboards/queries written
    /// against `SELECT … FROM pg_stat_statements ORDER BY
    /// total_exec_time DESC LIMIT 10` keep working.
    ///
    /// SPG ↔ PG mapping:
    ///   query            ← stats.sql
    ///   calls            ← stats.exec_count
    ///   total_exec_time  ← stats.total_us / 1000 (ms)
    ///   min_exec_time    ← 0 (no per-call min tracked yet)
    ///   max_exec_time    ← stats.max_us / 1000
    ///   mean_exec_time   ← derived
    ///   stddev_exec_time ← 0
    ///   rows             ← 0 (per-row count tracking lands later)
    ///   userid           ← 10 (PG's "postgres" superuser oid)
    ///   dbid             ← 16384 (SPG single-db OID)
    ///   queryid          ← hash of sql
    ///   plans            ← stats.exec_count (one plan per call)
    ///   shared_blks_*    ← 0 (no shared-buffer accounting)
    ///   local_blks_*     ← 0
    ///   temp_blks_*      ← 0
    ///   *_blk_*_time     ← 0
    ///   wal_records / wal_fpi / wal_bytes ← 0 (per-stmt accounting)
    ///   jit_*            ← 0 (no JIT)
    ///   stats_since / minmax_stats_since ← stats.last_seen_us
    ///
    /// 38 columns total to cover PG 18's pg_stat_statements view.
    pub(crate) fn exec_pg_stat_statements(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("userid", DataType::BigInt, false),
            ColumnSchema::new("dbid", DataType::BigInt, false),
            ColumnSchema::new("toplevel", DataType::Bool, false),
            ColumnSchema::new("queryid", DataType::BigInt, false),
            ColumnSchema::new("query", DataType::Text, false),
            ColumnSchema::new("plans", DataType::BigInt, false),
            ColumnSchema::new("total_plan_time", DataType::Float, false),
            ColumnSchema::new("min_plan_time", DataType::Float, false),
            ColumnSchema::new("max_plan_time", DataType::Float, false),
            ColumnSchema::new("mean_plan_time", DataType::Float, false),
            ColumnSchema::new("stddev_plan_time", DataType::Float, false),
            ColumnSchema::new("calls", DataType::BigInt, false),
            ColumnSchema::new("total_exec_time", DataType::Float, false),
            ColumnSchema::new("min_exec_time", DataType::Float, false),
            ColumnSchema::new("max_exec_time", DataType::Float, false),
            ColumnSchema::new("mean_exec_time", DataType::Float, false),
            ColumnSchema::new("stddev_exec_time", DataType::Float, false),
            ColumnSchema::new("rows", DataType::BigInt, false),
            ColumnSchema::new("shared_blks_hit", DataType::BigInt, false),
            ColumnSchema::new("shared_blks_read", DataType::BigInt, false),
            ColumnSchema::new("shared_blks_dirtied", DataType::BigInt, false),
            ColumnSchema::new("shared_blks_written", DataType::BigInt, false),
            ColumnSchema::new("local_blks_hit", DataType::BigInt, false),
            ColumnSchema::new("local_blks_read", DataType::BigInt, false),
            ColumnSchema::new("local_blks_dirtied", DataType::BigInt, false),
            ColumnSchema::new("local_blks_written", DataType::BigInt, false),
            ColumnSchema::new("temp_blks_read", DataType::BigInt, false),
            ColumnSchema::new("temp_blks_written", DataType::BigInt, false),
            ColumnSchema::new("blk_read_time", DataType::Float, false),
            ColumnSchema::new("blk_write_time", DataType::Float, false),
            ColumnSchema::new("wal_records", DataType::BigInt, false),
            ColumnSchema::new("wal_fpi", DataType::BigInt, false),
            ColumnSchema::new("wal_bytes", DataType::BigInt, false),
            ColumnSchema::new("jit_functions", DataType::BigInt, false),
            ColumnSchema::new("jit_generation_time", DataType::Float, false),
            ColumnSchema::new("jit_inlining_count", DataType::BigInt, false),
            ColumnSchema::new("jit_inlining_time", DataType::Float, false),
            ColumnSchema::new("jit_emission_count", DataType::BigInt, false),
        ];
        let rows: Vec<Row<'static>> = self
            .query_stats
            .snapshot()
            .into_iter()
            .map(|(sql, s)| {
                let calls = i64::try_from(s.exec_count).unwrap_or(i64::MAX);
                let total_ms = (s.total_us as f64) / 1000.0;
                let max_ms = (s.max_us as f64) / 1000.0;
                let mean_ms = if s.exec_count == 0 {
                    0.0
                } else {
                    (s.total_us as f64) / 1000.0 / (s.exec_count as f64)
                };
                // queryid: PG uses a 64-bit hash of the normalised
                // query text. SPG hashes the raw sql with FNV-1a-64
                // (matches what pg_compatible_hash uses for HASH
                // partitions). Stable across runs as long as the
                // sql text is byte-identical.
                let queryid =
                    crate::partition::pg_compatible_hash(&spg_storage::Value::Text(
                        alloc::borrow::Cow::Borrowed(&sql),
                    )) as i64;
                Row::new(alloc::vec![
                    Value::BigInt(10),     // userid (PG superuser)
                    Value::BigInt(16384),  // dbid
                    Value::Bool(true),     // toplevel
                    Value::BigInt(queryid),
                    Value::Text(alloc::borrow::Cow::Owned(sql)),
                    Value::BigInt(calls),  // plans
                    Value::Float(0.0),     // total_plan_time
                    Value::Float(0.0),     // min_plan_time
                    Value::Float(0.0),     // max_plan_time
                    Value::Float(0.0),     // mean_plan_time
                    Value::Float(0.0),     // stddev_plan_time
                    Value::BigInt(calls),  // calls
                    Value::Float(total_ms),
                    Value::Float(0.0),     // min_exec_time
                    Value::Float(max_ms),
                    Value::Float(mean_ms),
                    Value::Float(0.0),     // stddev_exec_time
                    // v7.37.22 (22.9) — total rows produced /
                    // affected, mapped from query_stats.total_rows.
                    Value::BigInt(i64::try_from(s.total_rows).unwrap_or(i64::MAX)),
                    // 8 shared_blks_*, 4 local_blks_*, 2 temp_blks_*
                    Value::BigInt(0),
                    Value::BigInt(0),
                    Value::BigInt(0),
                    Value::BigInt(0),
                    Value::BigInt(0),
                    Value::BigInt(0),
                    Value::BigInt(0),
                    Value::BigInt(0),
                    Value::BigInt(0),
                    Value::BigInt(0),
                    Value::Float(0.0),     // blk_read_time
                    Value::Float(0.0),     // blk_write_time
                    Value::BigInt(0),      // wal_records
                    Value::BigInt(0),      // wal_fpi
                    Value::BigInt(0),      // wal_bytes
                    Value::BigInt(0),      // jit_functions
                    Value::Float(0.0),     // jit_generation_time
                    Value::BigInt(0),      // jit_inlining_count
                    Value::Float(0.0),     // jit_inlining_time
                    Value::BigInt(0),      // jit_emission_count
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v7.37.22 (22.2) — materialise `pg_statio_user_tables` rows.
    /// PG exposes per-relation I/O counters that monitoring tools
    /// (pgwatch / pganalyze / Datadog) query routinely. SPG's
    /// storage model is hot-tier rows + cold-tier segments, both
    /// of which the engine tracks at finer granularity than PG's
    /// shared-buffer hit/read split. v7.37.22 (22.2) ships the
    /// SQL shape with the columns PG dashboards expect; the
    /// `heap_blks_*` / `idx_blks_*` numbers map to SPG's
    /// hot/cold accounting where the mapping is unambiguous and
    /// stay 0 otherwise.
    ///
    /// Columns (PG-exact order):
    ///   relid OID NOT NULL              -- monotonic per table
    ///   schemaname TEXT NOT NULL        -- always 'public'
    ///   relname TEXT NOT NULL           -- table name
    ///   heap_blks_read BIGINT NOT NULL  -- cold-tier reads (stub: 0)
    ///   heap_blks_hit BIGINT NOT NULL   -- hot-tier reads (live row count)
    ///   idx_blks_read BIGINT NOT NULL   -- cold-tier index reads (0)
    ///   idx_blks_hit BIGINT NOT NULL    -- hot-tier index hits (sum of NSW + BTree probe counters, future)
    ///   toast_blks_read BIGINT NOT NULL -- 0 (SPG has no TOAST)
    ///   toast_blks_hit BIGINT NOT NULL  -- 0
    ///   tidx_blks_read BIGINT NOT NULL  -- 0
    ///   tidx_blks_hit BIGINT NOT NULL   -- 0
    pub(crate) fn exec_pg_statio_user_tables(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("relid", DataType::BigInt, false),
            ColumnSchema::new("schemaname", DataType::Text, false),
            ColumnSchema::new("relname", DataType::Text, false),
            ColumnSchema::new("heap_blks_read", DataType::BigInt, false),
            ColumnSchema::new("heap_blks_hit", DataType::BigInt, false),
            ColumnSchema::new("idx_blks_read", DataType::BigInt, false),
            ColumnSchema::new("idx_blks_hit", DataType::BigInt, false),
            ColumnSchema::new("toast_blks_read", DataType::BigInt, false),
            ColumnSchema::new("toast_blks_hit", DataType::BigInt, false),
            ColumnSchema::new("tidx_blks_read", DataType::BigInt, false),
            ColumnSchema::new("tidx_blks_hit", DataType::BigInt, false),
        ];
        let mut rows: Vec<Row<'static>> = Vec::new();
        let mut relid: i64 = 16384; // PG starts user-relation OIDs above 16384
        for name in self.catalog.table_names() {
            if is_internal_table_name(&name) {
                continue;
            }
            let Some(t) = self.catalog.get(&name) else {
                continue;
            };
            let live_rows = t.rows().len() as i64;
            rows.push(Row::new(alloc::vec![
                Value::BigInt(relid),
                Value::text::<String>("public".into()),
                Value::Text(alloc::borrow::Cow::Owned(name)),
                Value::BigInt(0),
                Value::BigInt(live_rows),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
            ]));
            relid += 1;
        }
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

    /// v7.37.22 (22.3) — autoanalyze pass.
    ///
    /// PG runs autovacuum + autoanalyze on a background timer.
    /// SPG's spg-embedded / spg-server hosts call this from their
    /// maintenance loop on a configurable cadence (default 60s,
    /// matching PG's `autovacuum_naptime`). Each call:
    ///
    /// 1. Walks `tables_needing_analyze()` (same threshold as the
    ///    existing introspection API).
    /// 2. Runs `ANALYZE <table>` on each candidate.
    /// 3. Returns the names that were analyzed so the host can
    ///    log / emit metrics.
    ///
    /// Internally identical to `ANALYZE name1; ANALYZE name2; …`
    /// but bundled so the plan-cache invalidation runs once at the
    /// end (cheaper than invalidating per-table). The host can call
    /// this under the engine write-lock without splicing extra
    /// SQL through the parser.
    ///
    /// Returns the (possibly empty) list of tables analyzed.
    pub fn autoanalyze_pass(&mut self) -> Result<Vec<String>, EngineError> {
        let candidates = self.tables_needing_analyze();
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        for name in &candidates {
            // `exec_analyze` for a single table also bumps
            // version + evicts that table's plans. Doing it
            // per-table here matches `ANALYZE a; ANALYZE b;`
            // semantics — a host that wants the bundled
            // optimisation can call `exec_analyze(None)` for the
            // bare ANALYZE-all path instead.
            self.exec_analyze(Some(name))?;
        }
        Ok(candidates)
    }
}
