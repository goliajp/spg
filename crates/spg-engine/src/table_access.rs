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

use crate::eval::{self, EvalContext, EvalError};
use crate::{Engine, EngineError};

impl Engine {
    /// v7.39 (round 295, E3 Phase 1b) — is this base row one the locking
    /// pre-pass found held by another transaction?
    ///
    /// Read-only: the locks were taken under `&mut self` before the scan
    /// ran, so no scan path needs a mutable lock table. EVERY
    /// `scan_visible` consumer over a base table has to ask — the filter
    /// went into one of them first and `SELECT … SKIP LOCKED` quietly
    /// returned locked rows through the other two.
    pub(crate) fn row_locked_elsewhere(&self, table_name: &str, idx: usize) -> bool {
        self.lock_skip_rows
            .as_ref()
            .is_some_and(|(t, set)| t.eq_ignore_ascii_case(table_name) && set.contains(&idx))
    }

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
    pub(crate) fn materialise_table_ref(
        &self,
        tref: &TableRef,
    ) -> Result<(Vec<Row<'static>>, Vec<ColumnSchema>), EngineError> {
        if let Some(expr) = tref.unnest_expr.as_deref() {
            // Multi-arg unnest(a, b, …) — parallel zip via the
            // shared builder; columns alias positionally, default
            // PG's `unnest`; ordinality rides after.
            if let Some(args) = crate::select::unnest_zip_args(expr) {
                let (dtypes, rows) = crate::select::unnest_zip_rows(args)?;
                let n_vals = dtypes.len();
                let mut cols: Vec<ColumnSchema> = dtypes
                    .iter()
                    .enumerate()
                    .map(|(i, dt)| {
                        let name = tref
                            .unnest_column_aliases
                            .get(i)
                            .cloned()
                            .unwrap_or_else(|| alloc::string::String::from("unnest"));
                        ColumnSchema::new(name, *dt, true)
                    })
                    .collect();
                let rows = if tref.with_ordinality {
                    let ord_name = tref
                        .unnest_column_aliases
                        .get(n_vals)
                        .cloned()
                        .unwrap_or_else(|| alloc::string::String::from("ordinality"));
                    cols.push(ColumnSchema::new(ord_name, DataType::BigInt, false));
                    rows.into_iter()
                        .enumerate()
                        .map(|(i, row)| {
                            let mut vals = row.values.clone();
                            vals.push(Value::BigInt(i as i64 + 1));
                            Row::new(vals)
                        })
                        .collect()
                } else {
                    rows
                };
                return Ok((rows, cols));
            }
            let empty_schema: Vec<ColumnSchema> = Vec::new();
            let ctx = EvalContext::new(&empty_schema, None);
            let dummy_row = Row::new(Vec::new());
            // v7.39 (round 236) — a multidimensional array unnests into its
            // elements in row-major order (PG); flatten before the per-variant
            // match, which only knows the 1-D forms.
            let unnest_src = {
                let v = eval::eval_expr(expr, &dummy_row, &ctx).map_err(EngineError::Eval)?;
                crate::eval::values::flatten_2d(&v).unwrap_or(v)
            };
            let (elem_dtype, rows) = match unnest_src {
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
                // v7.39 (round 759, F31-B8b) — unnest(tsvector) in a
                // JOINED FROM list (the primary-position executor got
                // this arm in round 758; `FROM unnest(tsv) t, …` comes
                // through here instead). Same PG18-measured shape:
                // lexeme | positions | weights.
                Value::TsVector(lexemes) => {
                    let mut cols = alloc::vec![
                        ColumnSchema::new("lexeme".to_string(), DataType::Text, true),
                        ColumnSchema::new("positions".to_string(), DataType::SmallIntArray, true),
                        ColumnSchema::new("weights".to_string(), DataType::TextArray, true),
                    ];
                    for (i, new_name) in tref.unnest_column_aliases.iter().enumerate() {
                        if let Some(col) = cols.get_mut(i) {
                            col.name = new_name.clone();
                        }
                    }
                    let mut rows: Vec<Row<'static>> = lexemes
                        .iter()
                        .map(|l| {
                            let (pos, wts) = if l.positions.is_empty() {
                                (Value::Null, Value::Null)
                            } else {
                                let letter = match l.weight {
                                    3 => "A",
                                    2 => "B",
                                    1 => "C",
                                    _ => "D",
                                };
                                (
                                    Value::SmallIntArray(
                                        l.positions
                                            .iter()
                                            .map(|p| Some(i16::try_from(*p).unwrap_or(i16::MAX)))
                                            .collect(),
                                    ),
                                    Value::TextArray(
                                        l.positions.iter().map(|_| Some(letter.into())).collect(),
                                    ),
                                )
                            };
                            Row::new(alloc::vec![Value::text(l.word.clone()), pos, wts])
                        })
                        .collect();
                    if tref.with_ordinality {
                        let ord_name = tref
                            .unnest_column_aliases
                            .get(3)
                            .cloned()
                            .unwrap_or_else(|| alloc::string::String::from("ordinality"));
                        cols.push(ColumnSchema::new(ord_name, DataType::BigInt, false));
                        rows = rows
                            .into_iter()
                            .enumerate()
                            .map(|(i, row)| {
                                let mut vals = row.values;
                                vals.push(Value::BigInt(i as i64 + 1));
                                Row::new(vals)
                            })
                            .collect();
                    }
                    return Ok((rows, cols));
                }
                other => {
                    // v7.39 (round 622, S05a) — a `TypeMismatch`, not an
                    // `Unsupported`: unnest IS supported, this value is
                    // the wrong type for it. The third site already
                    // spelled it that way, so the same user-visible
                    // sentence carried two SQLSTATEs depending on which
                    // of the three raised it. PG answers 42883 for all.
                    return Err(EngineError::Eval(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "unnest() expects an array argument, got {}",
                            crate::conversions::pg_type_name_for_error_opt(other.data_type())
                        ),
                    }));
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
                        let mut vals = row.values.clone();
                        vals.push(Value::BigInt(i as i64 + 1));
                        Row::new(vals)
                    })
                    .collect()
            } else {
                rows
            };
            return Ok((rows, cols));
        }
        // generate_series in a FROM list next to other tables —
        // same row builder as the primary-position executor;
        // ordinality/alias handling mirrors the unnest arm above.
        // v7.39 (read01 partitionfuncs.c) — FROM-position table functions.
        if tref.table_fn_call.is_some() {
            let (rows, mut cols) = self.table_fn_rows(tref)?;
            for (i, new_name) in tref.unnest_column_aliases.iter().enumerate() {
                if let Some(col) = cols.get_mut(i) {
                    col.name = new_name.clone();
                }
            }
            return Ok((rows, cols));
        }
        if let Some(args) = tref.generate_series_args.as_ref() {
            let (elem_dtype, rows) =
                crate::select::generate_series_rows(args, &crate::CancelToken::none())?;
            let alias = tref
                .alias
                .clone()
                .unwrap_or_else(|| alloc::string::String::from("generate_series"));
            let col_name = tref.unnest_column_aliases.first().cloned().unwrap_or(alias);
            let mut cols = alloc::vec![ColumnSchema::new(col_name, elem_dtype, true)];
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
                        let mut vals = row.values.clone();
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
        // v7.39 (round 295, E3 Phase 1b) — drop the rows the locking
        // pre-pass found held by someone else. Read-only: the locks were
        // taken under `&mut self` before this scan ran.
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
            || tref.generate_series_args.is_some()
            || tref.table_fn_call.is_some()
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
                // r1037 — the literal means whatever the COLUMN says it
                // means. This used to build the key straight from
                // `literal_to_value`, so a string literal was always TEXT:
                // on a `uuid` / `date` / `timestamp` column the seek looked
                // in a key space nothing lives in, found nothing, and the
                // JOIN returned no rows at all.
                //
                //   FROM s JOIN u ON u.id = s.uid WHERE s.k = '<uuid>'  ->  0
                //   the same with `::uuid` on the literal              ->  1
                //   the same predicate written in the ON clause        ->  1
                //
                // Round 564 fixed exactly this on the single-table seek and
                // recorded why it matters: creating an index changed the
                // answer. The JOIN peer's seek is a second copy of the same
                // decision that did not get the fix, so it now shares the
                // resolver instead of repeating it.
                let resolved = crate::index_access::resolve_col_literal_pair(
                    lhs.as_ref(),
                    rhs.as_ref(),
                    &cols,
                    alias,
                )
                .or_else(|| {
                    crate::index_access::resolve_col_literal_pair(
                        rhs.as_ref(),
                        lhs.as_ref(),
                        &cols,
                        alias,
                    )
                });
                if let Some((pos, value)) = resolved
                    && let Some(idx) = table.index_on(pos)
                    && let Some(col) = cols.get(pos)
                    && let Some(key) = spg_storage::IndexKey::from_value_for_column(&value, col.ty)
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
        // v7.39 (read01 round 53) — carry the catalog: a WHERE conjunct pushed
        // down into a JOIN peer's scan is evaluated HERE, and a `::regclass` /
        // enum / composite cast in it needs the catalog to resolve. Without it
        // `pg_class JOIN pg_index … WHERE indrelid = 't'::regclass` errored on
        // "comparison between BigInt and Text" while the same predicate on a
        // bare single-table SELECT worked.
        // v7.39 (round 525) — and the session. A WHERE pushed down to the
        // scan is the same predicate the caller wrote, so it needs what
        // the caller's context has.
        let scan_sess = self.dml_session();
        let ctx = EvalContext::new(&cols, Some(alias))
            .with_catalog(self.active_catalog())
            .with_session(&scan_sess);
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
                // v7.37.15 (Phase C.3, step 2) — MVCC visibility gate for
                // the index-seeded materialise path. The None branch below
                // was gated in Phase B via `scan_visible`, but this seeded
                // (index-seek) branch returns cloned rows straight into the
                // join pipeline, so under gate-on it must likewise drop
                // rows the reader's snapshot cannot see (an index entry can
                // outlive its tombstoned hot row). No-op under the default
                // gate-off: every hot row is frozen/alive.
                let snap = self.current_snapshot();
                for i in ids {
                    if !table.is_row_visible(i, &snap) {
                        continue;
                    }
                    if let Some(row) = table.rows().get(i) {
                        push_if(row, &mut out)?;
                    }
                }
            }
            None => {
                // v7.37.15 Phase B — visibility-gated full scan.
                let snap = self.current_snapshot();
                for (i, row) in table.scan_visible(&snap) {
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

    /// v7.35.1 — yield every cold-tier row of `table` exactly once.
    ///
    /// r944 — it now looks in every index a freeze could have filed a
    /// locator under, not one guessed index.
    ///
    /// It used to require a single-column PRIMARY KEY and then take the
    /// first BTree index on that column. The freezer picks its index
    /// separately (`freezer.rs:pick_target`), and when the two chose
    /// differently the walk found no `Cold` locators and the frozen rows
    /// simply stopped appearing — no error, a short answer. Measured on
    /// a table with a PK on `id` and a second index on `id`, frozen
    /// through the second: a plain `SELECT` returned 25 rows of 40.
    ///
    /// A row's locator lives in exactly ONE index
    /// (`register_cold_locators` takes a single index name), so the
    /// union over `cold_capable_indices` yields each row once and needs
    /// no visited-set.
    ///
    /// Restricting the union to indices with a declared UNIQUE key was
    /// tried and reverted: the freezer's own tests freeze tables whose
    /// integer index carries no such constraint, and filtering them out
    /// here hides rows that were frozen anyway — the bug, not a guard
    /// against it. Six gate tests said so.
    pub(crate) fn iter_cold_rows_of_table(&self, table: &Table) -> Vec<Row<'static>> {
        // A table that has frozen nothing pays nothing. The walk below is
        // O(index) per index, and the single-table scan runs it once per
        // query: round 942 profiled 20% of `SELECT pad FROM t400k ORDER
        // BY k LIMIT 10` inside it, over a table where nothing had ever
        // been frozen. This gate was unsafe until this round, because
        // neither freeze path marked the count — with that fixed, the
        // predicate is conservative again (stale reads as true) and the
        // skip cannot lose a row.
        if !table.has_cold_rows_fast() {
            return Vec::new();
        }
        let schema = table.schema();
        let table_name = schema.name.as_str();
        let catalog = self.active_catalog();
        let mut out = Vec::new();
        for idx in table.cold_capable_indices() {
            for (key, locators) in idx.iter_asc() {
                for loc in locators {
                    if let spg_storage::RowLocator::Cold { segment_id, .. } = loc
                        && let Some(row) =
                            catalog.resolve_cold_locator(table_name, *segment_id, key)
                    {
                        out.push(row);
                    }
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
        // r1037 — `col_pos` so the literal can be read as the COLUMN's
        // type. Without it a string literal was always TEXT, and on a
        // `uuid` / `date` / `timestamp` column the seek looked in a key
        // space nothing lives in: the JOIN driver came back empty and the
        // whole query answered zero rows with no error. Round 564 fixed
        // the same decision on the single-table seek; this is the third
        // copy of it, and the one a JOIN takes.
        //
        // r1039 — it is no longer a copy: both halves (what the literal
        // MEANS, and which key space it belongs in) come from the
        // resolver every other seek uses.
        let seek_keys =
            |col_pos: usize, idx: &spg_storage::Index, lits: &[&spg_sql::ast::Literal]| {
                let mut ids = Vec::new();
                let col = cols.get(col_pos)?;
                for l in lits {
                    let value = crate::index_access::literal_as_column_value(l, col, col_pos)?;
                    let key = spg_storage::IndexKey::from_value_for_column(&value, col.ty)?;
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
                        && let Some((pos, idx)) = indexed_col(c)
                        && let Some(ids) = seek_keys(pos, idx, &[l])
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
                        && let Some((pos, idx)) = indexed_col(c)
                        && let Some(lits) = list
                            .iter()
                            .map(|e| match e {
                                Expr::Literal(l) => Some(l),
                                _ => None,
                            })
                            .collect::<Option<Vec<_>>>()
                        && let Some(ids) = seek_keys(pos, idx, &lits)
                    {
                        seeded = Some(ids);
                        seeded_pred_idx = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        // v7.39 (round 525) — and the session, as above.
        let scan_sess = self.dml_session();
        let ctx = EvalContext::new(cols, Some(alias))
            .with_catalog(self.active_catalog())
            .with_session(&scan_sess);
        // v7.39 (round 574) — compile the conjuncts once, as the
        // single-table scan does.
        //
        // This helper feeds the deferred join's side seeds, and it
        // evaluated every conjunct INTERPRETIVELY for every row. A
        // profile of `SELECT count(*) FROM a JOIN b ON a.id = b.id
        // WHERE b.id < 100` put 32% of the connection thread inside
        // here, with `eval_expr` ~17% of it — 500k interpretive
        // evaluations to find 100 rows. `run_single_table_scan` has
        // compiled its WHERE since v7.39 round 479; the join's side
        // filter never learned to.
        //
        // A conjunct that will not compile keeps the interpretive path,
        // so the answers are the ones this helper already gave.
        let compiled: Vec<Option<eval::CompiledExpr>> = preds
            .iter()
            .map(|p| eval::fully_compilable(p).then(|| eval::compile_expr(p, &ctx)))
            .collect();
        let keep = |row: &Row<'static>,
                    stack: &mut Vec<Value<'static>>|
         -> Result<bool, EngineError> {
            for (i, p) in preds.iter().enumerate() {
                // The pred that seeded the index seek is already proven
                // true for every row in the seeded set; skip the
                // redundant per-row re-eval. Critical for large `IN`
                // lists, where re-evaluating the list interpretively per
                // row is O(rows × list).
                if Some(i) == seeded_pred_idx {
                    continue;
                }
                let ok = match &compiled[i] {
                    Some(cw) => {
                        eval::compiled::eval_compiled_pred(cw, row, &ctx, stack, ctx.mysql_dialect)
                            .map_err(EngineError::Eval)?
                    }
                    None => {
                        let v = eval::eval_expr(p, row, &ctx).map_err(EngineError::Eval)?;
                        matches!(v, Value::Bool(true))
                    }
                };
                if !ok {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        let mut eval_stack: Vec<Value<'static>> = Vec::new();
        let mut out: Vec<usize> = Vec::new();
        match seeded {
            Some(ids) => {
                // v7.37.15 (Phase C.3, step 2) — MVCC visibility gate for
                // the index-seeded branch, matching the Phase-B-gated None
                // branch below. The sole caller (deferred-join primary
                // seed) re-filters by `is_row_visible`, but keeping the two
                // branches symmetric means this helper never hands back an
                // invisible index. No-op under the default gate-off.
                let snap = self.current_snapshot();
                for i in ids {
                    if !table.is_row_visible(i, &snap) {
                        continue;
                    }
                    if let Some(row) = table.rows().get(i)
                        && keep(row, &mut eval_stack)?
                    {
                        out.push(i);
                    }
                }
            }
            None => {
                // v7.37.15 Phase B — visibility-gated predicate scan.
                let snap = self.current_snapshot();
                for (i, row) in table.scan_visible(&snap) {
                    if keep(row, &mut eval_stack)? {
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
