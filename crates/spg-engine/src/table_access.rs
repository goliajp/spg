//! Table-reference materialisation and index-seek helpers — the
//! row-level access primitives shared by the joined-SELECT path and
//! `join.rs`. Lifted out of `lib.rs` (v7.32 engine modularisation).
//! These `impl Engine` methods resolve a `TableRef` into owned rows +
//! schema, apply pushed-down predicates via index seeks, and prune
//! columns the query never references.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_sql::ast::{Expr, TableRef};
use spg_storage::{ColumnSchema, DataType, Row, StorageError, Table, Value};

use crate::eval::{self, EvalContext};
use crate::{Engine, EngineError};

impl Engine {
    /// Multi-table SELECT executor (one or more JOIN peers).
    ///
    /// v1.10 builds the joined row set up-front via nested-loop joins,
    /// then runs WHERE + projection + ORDER BY against the combined
    /// rows. No index seek. Aggregates and DISTINCT still work because
    /// the executor delegates projection through the same shared paths.
    #[allow(clippy::too_many_lines)]
    /// v7.13.2 — mailrs round-6 S5. Resolve a TableRef into an
    /// owned (rows, schema) pair. Catalog tables clone their hot
    /// rows + schema; UNNEST table refs evaluate their array
    /// expression once and synthesise a single-column row set
    /// using the same dispatch as `exec_select_unnest`. Used by
    /// the joined-select path so UNNEST can appear in any FROM
    /// position, not just as the primary.
    fn materialise_table_ref(
        &self,
        tref: &TableRef,
    ) -> Result<(Vec<Row<'static>>, Vec<ColumnSchema>), EngineError> {
        if let Some(expr) = tref.unnest_expr.as_deref() {
            let empty_schema: Vec<ColumnSchema> = Vec::new();
            let ctx = EvalContext::new(&empty_schema, None);
            let dummy_row = Row::new(Vec::new());
            let (elem_dtype, rows) =
                match eval::eval_expr(expr, &dummy_row, &ctx).map_err(EngineError::Eval)? {
                    Value::Null => (DataType::Text, Vec::new()),
                    Value::TextArray(items) => (
                        DataType::Text,
                        items
                            .into_iter()
                            .map(|item| {
                                Row::new(alloc::vec![match item {
                                    Some(s) => Value::text(s),
                                    None => Value::Null,
                                }])
                            })
                            .collect(),
                    ),
                    Value::IntArray(items) => (
                        DataType::Int,
                        items
                            .into_iter()
                            .map(|item| {
                                Row::new(alloc::vec![match item {
                                    Some(n) => Value::Int(n),
                                    None => Value::Null,
                                }])
                            })
                            .collect(),
                    ),
                    Value::BigIntArray(items) => (
                        DataType::BigInt,
                        items
                            .into_iter()
                            .map(|item| {
                                Row::new(alloc::vec![match item {
                                    Some(n) => Value::BigInt(n),
                                    None => Value::Null,
                                }])
                            })
                            .collect(),
                    ),
                    other => {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "unnest() expects an array argument, got {:?}",
                            other.data_type()
                        )));
                    }
                };
            let alias = tref.alias.clone().unwrap_or_else(|| "unnest".to_string());
            let col_name = tref.unnest_column_aliases.first().cloned().unwrap_or(alias);
            let mut cols = alloc::vec![ColumnSchema::new(col_name, elem_dtype, true)];
            // WITH ORDINALITY — trailing BIGINT counting rows from
            // 1 in element order; second column-alias entry renames.
            let rows = if tref.with_ordinality {
                let ord_name = tref
                    .unnest_column_aliases
                    .get(1)
                    .cloned()
                    .unwrap_or_else(|| alloc::string::String::from("ordinality"));
                cols.push(ColumnSchema::new(ord_name, DataType::BigInt, false));
                rows.into_iter()
                    .enumerate()
                    .map(|(i, row)| {
                        let mut vals = row.values.to_vec();
                        vals.push(Value::BigInt(i as i64 + 1));
                        Row::new(vals)
                    })
                    .collect()
            } else {
                rows
            };
            return Ok((rows, cols));
        }
        // v7.37.17 (17.6 siblings) — derived table (`FROM ( SELECT …
        // ) alias`) joined with other tables: materialise the inner
        // SELECT once through the union-aware executor. Correlated
        // (true LATERAL) refs never reach here — join.rs routes them
        // through the per-outer-row machinery before materialising.
        if let Some(inner) = tref.lateral_subquery.as_deref() {
            let crate::QueryResult::Rows { mut columns, rows } =
                self.exec_select_cancel(inner, crate::CancelToken::none())?
            else {
                return Err(EngineError::Unsupported(
                    "derived table subquery must return rows".into(),
                ));
            };
            for (i, new_name) in tref.unnest_column_aliases.iter().enumerate() {
                if let Some(col) = columns.get_mut(i) {
                    col.name = new_name.clone();
                }
            }
            return Ok((rows, columns));
        }
        let table =
            self.active_catalog()
                .get(&tref.name)
                .ok_or_else(|| StorageError::TableNotFound {
                    name: tref.name.clone(),
                })?;
        // v7.37.15 Phase B — visibility-gated materialise.
        let snap = self.current_snapshot();
        let mut rows: Vec<Row<'static>> =
            table.scan_visible(&snap).map(|(_, r)| r.clone()).collect();
        // v7.35.1 (mailrs prod #6 follow-up) — same fix as the
        // filtered variant: append every cold-tier row (one pass over
        // a unique BTree picks each row exactly once) so non-indexed
        // / no-predicate materialisations don't silently shed half
        // the table when the freezer has promoted older rows.
        rows.extend(self.iter_cold_rows_of_table(table));
        let cols = table.schema().columns.clone();
        Ok((rows, cols))
    }

    /// v7.28 (round-22) — materialise a plain table ref with
    /// single-table predicates pushed BELOW the clone: an indexed
    /// `col = literal` narrows to the matching row ids before any
    /// row is cloned, the rest filter linearly. A correlated
    /// subquery body like `… JOIN messages m2 ON …
    /// WHERE m2.thread_id = '<outer>'` runs per GROUP — without
    /// this it cloned + scanned the full 24k-row table 23.5k times.
    /// Falls back to the plain path for non-table refs.
    pub(crate) fn materialise_table_ref_filtered(
        &self,
        tref: &TableRef,
        preds: &[&Expr],
    ) -> Result<(Vec<Row<'static>>, Vec<ColumnSchema>), EngineError> {
        if preds.is_empty()
            || tref.unnest_expr.is_some()
            || tref.lateral_subquery.is_some()
            || tref.as_of_segment.is_some()
        {
            return self.materialise_table_ref(tref);
        }
        let Some(table) = self.active_catalog().get(&tref.name) else {
            return self.materialise_table_ref(tref);
        };
        let cols = table.schema().columns.clone();
        let alias = tref.alias.as_deref().unwrap_or(tref.name.as_str());
        // Index seek on the first `col = literal` predicate with a
        // BTree on that column.
        let mut seeded: Option<Vec<usize>> = None;
        for p in preds {
            if let Expr::Binary {
                lhs,
                op: spg_sql::ast::BinOp::Eq,
                rhs,
            } = p
            {
                let pair = match (lhs.as_ref(), rhs.as_ref()) {
                    (Expr::Column(c), Expr::Literal(l)) | (Expr::Literal(l), Expr::Column(c)) => {
                        Some((c, l))
                    }
                    _ => None,
                };
                if let Some((c, l)) = pair
                    && c.qualifier
                        .as_deref()
                        .is_none_or(|q| q.eq_ignore_ascii_case(alias))
                    && let Some(pos) = cols.iter().position(|s| s.name == c.name)
                    && let Some(idx) = table.index_on(pos)
                    && let Some(key) = spg_storage::IndexKey::from_value(&eval::literal_to_value(l))
                {
                    let mut ids = Vec::new();
                    let mut all_hot = true;
                    for loc in idx.lookup_eq(&key) {
                        match *loc {
                            spg_storage::RowLocator::Hot(i) => ids.push(i),
                            spg_storage::RowLocator::Cold { .. } => {
                                all_hot = false;
                                break;
                            }
                        }
                    }
                    if all_hot {
                        seeded = Some(ids);
                        break;
                    }
                }
            }
        }
        let ctx = EvalContext::new(&cols, Some(alias));
        let mut out: Vec<Row<'static>> = Vec::new();
        let push_if =
            |row: &Row<'static>, out: &mut Vec<Row<'static>>| -> Result<(), EngineError> {
                for p in preds {
                    let v = eval::eval_expr(p, row, &ctx).map_err(EngineError::Eval)?;
                    if !matches!(v, Value::Bool(true)) {
                        return Ok(());
                    }
                }
                out.push(row.clone());
                Ok(())
            };
        match seeded {
            Some(ids) => {
                for i in ids {
                    if let Some(row) = table.rows().get(i) {
                        push_if(row, &mut out)?;
                    }
                }
            }
            None => {
                // v7.37.15 Phase B — visibility-gated full scan.
                let snap = self.current_snapshot();
                for (_, row) in table.scan_visible(&snap) {
                    push_if(row, &mut out)?;
                }
                // v7.35.1 (mailrs prod #6 follow-up) — cold-tier rows
                // were silently dropped from peer / non-indexed
                // materialised paths because `Table::rows()` only
                // surfaces the hot tier. Walk the table's unique
                // BTree(s) (each cold row appears exactly once per
                // such index — dedup-free) to lift every cold-tier
                // row through `Catalog::resolve_cold_locator` and
                // re-apply the same predicates.
                for row in self.iter_cold_rows_of_table(table) {
                    push_if(&row, &mut out)?;
                }
            }
        }
        Ok((out, cols))
    }

    /// v7.35.1 — yield every cold-tier row of `table` exactly once
    /// by walking the BTree index that covers the table's PRIMARY
    /// KEY. The PK uniqueness contract gives per-row dedup without
    /// a separate visited-set. Returns an empty Vec when the table
    /// has no PK-backed BTree (pre-PK tables / ad-hoc heaps stay
    /// hot-tier-only, so this is harmless on those shapes).
    pub(crate) fn iter_cold_rows_of_table(&self, table: &Table) -> Vec<Row<'static>> {
        let schema = table.schema();
        // PK column position(s) — single-column PK only for now;
        // composite PKs would require composite-key resolution
        // through resolve_cold_locator that the current API
        // doesn't expose.
        let Some(pk_col_pos) = schema
            .uniqueness_constraints
            .iter()
            .find(|u| u.is_primary_key && u.columns.len() == 1)
            .map(|u| u.columns[0])
        else {
            return Vec::new();
        };
        let Some(idx) = table.indices().iter().find(|i| {
            i.column_position == pk_col_pos && matches!(i.kind, spg_storage::IndexKind::BTree(_))
        }) else {
            return Vec::new();
        };
        let table_name = schema.name.as_str();
        let catalog = self.active_catalog();
        let mut out = Vec::new();
        for (key, locators) in idx.iter_asc() {
            for loc in locators {
                if let spg_storage::RowLocator::Cold { segment_id, .. } = loc
                    && let Some(row) = catalog.resolve_cold_locator(table_name, *segment_id, key)
                {
                    out.push(row);
                }
            }
        }
        out
    }

    /// v7.31 (perf campaign) — `materialise_table_ref_filtered` for
    /// the deferred-join pipeline: same index-seek + linear-filter
    /// logic, but returns surviving row INDICES into the stored
    /// table instead of cloned rows. The join working set reads the
    /// table in place; survivors clone once at output time.
    pub(crate) fn filter_table_indices(
        &self,
        table: &Table,
        alias: &str,
        preds: &[&Expr],
    ) -> Result<Vec<usize>, EngineError> {
        if preds.is_empty() {
            return Ok((0..table.rows().len()).collect());
        }
        let cols = &table.schema().columns;
        let mut seeded: Option<Vec<usize>> = None;
        // v7.34.3 — record which predicate seeded the seek so the
        // post-seed `keep` loop skips it. The seeded set already
        // satisfies that predicate by construction (every id came back
        // from an index lookup keyed on the same column/literal), and
        // re-evaluating it per row pays the predicate's interpretive
        // cost a second time. For a 6 000-element `IN` list that cost
        // is O(list) per row → ~150 M wasted comparisons on a 25 k row
        // table, which is exactly the SPGE 320 ms / PG 1 ms gap the
        // 7.34.0 baseline pinned for `m.id IN (6000 lits)`.
        let mut seeded_pred_idx: Option<usize> = None;
        // Resolve an indexed column reference (qualified to this alias or
        // bare) to its `(position, index)`.
        let indexed_col = |c: &spg_sql::ast::ColumnName| {
            if !c
                .qualifier
                .as_deref()
                .is_none_or(|q| q.eq_ignore_ascii_case(alias))
            {
                return None;
            }
            let pos = cols.iter().position(|s| s.name == c.name)?;
            table.index_on(pos).map(|idx| (pos, idx))
        };
        // Seek every literal key through the index, collecting hot row
        // indices. Returns None when any key lands a cold locator (the
        // caller then falls back to the full scan rather than miss rows).
        let seek_keys = |idx: &spg_storage::Index, lits: &[&spg_sql::ast::Literal]| {
            let mut ids = Vec::new();
            for l in lits {
                let key = spg_storage::IndexKey::from_value(&eval::literal_to_value(l))?;
                for loc in idx.lookup_eq(&key) {
                    match *loc {
                        spg_storage::RowLocator::Hot(i) => ids.push(i),
                        spg_storage::RowLocator::Cold { .. } => return None,
                    }
                }
            }
            // Union order is per-literal; sort+dedup so the filtered set
            // stays in table order (matches the full-scan path, and keeps
            // any order-sensitive downstream deterministic).
            ids.sort_unstable();
            ids.dedup();
            Some(ids)
        };
        for (i, p) in preds.iter().enumerate() {
            match p {
                Expr::Binary {
                    lhs,
                    op: spg_sql::ast::BinOp::Eq,
                    rhs,
                } => {
                    let pair = match (lhs.as_ref(), rhs.as_ref()) {
                        (Expr::Column(c), Expr::Literal(l))
                        | (Expr::Literal(l), Expr::Column(c)) => Some((c, l)),
                        _ => None,
                    };
                    if let Some((c, l)) = pair
                        && let Some((_, idx)) = indexed_col(c)
                        && let Some(ids) = seek_keys(idx, &[l])
                    {
                        seeded = Some(ids);
                        seeded_pred_idx = Some(i);
                        break;
                    }
                }
                // v7.33 (mailrs 7.33.0) — `indexed_col IN (lit, …)` seeds
                // the index with one seek per literal instead of a full
                // scan + per-row membership test (PG's bitmap index scan).
                // The mailrs conversation search filters `thread_id IN
                // (60 ids)`: 60 seeks (~one thread each) vs scanning 24k
                // messages. NOT NULL keys only; a bare/qualified column on
                // the LHS and an all-literal list.
                Expr::InList {
                    expr,
                    list,
                    negated: false,
                } => {
                    if let Expr::Column(c) = expr.as_ref()
                        && let Some((_, idx)) = indexed_col(c)
                        && let Some(lits) = list
                            .iter()
                            .map(|e| match e {
                                Expr::Literal(l) => Some(l),
                                _ => None,
                            })
                            .collect::<Option<Vec<_>>>()
                        && let Some(ids) = seek_keys(idx, &lits)
                    {
                        seeded = Some(ids);
                        seeded_pred_idx = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let ctx = EvalContext::new(cols, Some(alias));
        let keep = |row: &Row<'static>| -> Result<bool, EngineError> {
            for (i, p) in preds.iter().enumerate() {
                // The pred that seeded the index seek is already proven
                // true for every row in the seeded set; skip the
                // redundant per-row re-eval. Critical for large `IN`
                // lists, where re-evaluating the list interpretively per
                // row is O(rows × list).
                if Some(i) == seeded_pred_idx {
                    continue;
                }
                let v = eval::eval_expr(p, row, &ctx).map_err(EngineError::Eval)?;
                if !matches!(v, Value::Bool(true)) {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        let mut out: Vec<usize> = Vec::new();
        match seeded {
            Some(ids) => {
                for i in ids {
                    if let Some(row) = table.rows().get(i)
                        && keep(row)?
                    {
                        out.push(i);
                    }
                }
            }
            None => {
                // v7.37.15 Phase B — visibility-gated predicate scan.
                let snap = self.current_snapshot();
                for (i, row) in table.scan_visible(&snap) {
                    if keep(row)? {
                        out.push(i);
                    }
                }
            }
        }
        Ok(out)
    }

    /// v7.17.0 Phase 3.P0-43 — materialise a `FROM` with one or more
    /// JOINs into `(combined_schema, filtered_rows)`. The combined
    /// schema uses composite `alias.col` column names so the
    /// qualifier-aware column resolver finds every join peer by
    /// exact match; the filtered rows are the join cross-product
    /// after the optional WHERE clause is applied.
    ///
    /// Shared by `exec_joined_select` and the JOIN branch of
    /// `exec_select_with_window`; both paths used to inline the
    /// same nested-loop logic and the window path rejected JOIN
    /// outright.
    /// v7.28 (round-22) — resolve a Column reference against a
    /// composite ("alias.col") schema slice. Bare names match a
    /// unique ".col" suffix.
    pub(crate) fn composite_col_pos(
        schema: &[ColumnSchema],
        c: &spg_sql::ast::ColumnName,
    ) -> Option<usize> {
        if let Some(q) = &c.qualifier {
            let composite = alloc::format!("{q}.{}", c.name);
            return schema.iter().position(|s| s.name == composite);
        }
        let suffix = alloc::format!(".{}", c.name);
        let mut hits = schema
            .iter()
            .enumerate()
            .filter(|(_, s)| s.name.ends_with(&suffix) || s.name == c.name);
        let first = hits.next();
        if hits.next().is_some() {
            return None; // ambiguous — leave to the residual evaluator
        }
        first.map(|(i, _)| i)
    }

    /// v7.28 (round-22) — resolve a Column against ONE peer's own
    /// columns (right side of a join): `alias.col` or a bare name.
    pub(crate) fn peer_col_pos(
        peer_alias: &str,
        peer_cols: &[ColumnSchema],
        c: &spg_sql::ast::ColumnName,
    ) -> Option<usize> {
        if let Some(q) = &c.qualifier
            && !q.eq_ignore_ascii_case(peer_alias)
        {
            return None;
        }
        peer_cols.iter().position(|s| s.name == c.name)
    }

    /// v7.28 (round-22) — drop the VALUES of columns the statement
    /// never references (schema and positions stay; the value
    /// becomes NULL, so a 30 KB body column costs nothing through
    /// the join pipeline instead of being cloned per row).
    pub(crate) fn null_out_unreferenced(
        rows: &mut [Row],
        cols: &[ColumnSchema],
        alias: &str,
        needed: &alloc::collections::BTreeSet<(String, String)>,
    ) {
        let keep: Vec<bool> = cols
            .iter()
            .map(|c| needed.contains(&(alias.to_string(), c.name.clone())))
            .collect();
        if keep.iter().all(|k| *k) {
            return;
        }
        for row in rows.iter_mut() {
            for (i, k) in keep.iter().enumerate() {
                if !*k && i < row.values.len() {
                    row.values[i] = Value::Null;
                }
            }
        }
    }
}
