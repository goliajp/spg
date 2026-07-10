// v7.37.42 (A3 Step 1) — SCALARSQ streaming-shape detector.
//
// Identifies the SCALARSQ shape:
//
//   SELECT col1 [, col2 ...] FROM <single_table> [WHERE …] [LIMIT N]
//
// where at least one projection item is an `Expr::ScalarSubquery`. For
// this shape the engine never has to materialise a `Vec<Row<'static>>` before
// emitting — each output row depends only on its outer row + the per-
// row scalar subquery evaluation. The plan-cached PK probe fast path
// (v7.37.37) makes the per-row inner evaluation a single index seek;
// what remains is the `tagged: Vec<…>` buffer and the wire-layer
// `Vec<Row<'static>>` round trip, both of which a streaming executor (Step 2
// of the (A3) plan) will skip directly into `wbuf` via an emit
// callback.
//
// EXCLUDED (must take the generic `exec_select_cancel` path):
//   - JOINs / set-returning FROM / view / CTE / UNION / EXCEPT
//   - GROUP BY / HAVING / DISTINCT / window functions
//   - ORDER BY (forces a sort; no streaming)
//   - LIMIT WITH TIES (requires ORDER BY semantics)
//   - any Wildcard SelectItem (column resolution still needs the
//     planner; the streaming path stays narrow until Step 2 picks up
//     wildcard handling natively)
//   - lateral subquery / UNNEST / generate_series in FROM
//   - AS OF SEGMENT cold-tier scans
//   - any projection item containing a top-level UNNEST SRF
//
// See `.claude/notes/v7.37.42-scalarsq-specialized-executor-design.md`
// for the full (A3) attack design.

use alloc::borrow::Cow;
use alloc::string::ToString;
use alloc::vec::Vec;

use bumpalo::Bump;
use spg_sql::ast::{Expr, SelectItem, SelectStatement};
use spg_storage::{ColumnSchema, Row, StorageError, Value};

use crate::bytebudget::{ByteBudget, approx_values_bytes};
use crate::cancel::CancelToken;
use crate::eval;
use crate::index_access::{try_gin_jsonb_seek, try_gin_seek, try_index_seek, try_trgm_seek};
use crate::memoize::MemoizeCache;
use crate::select::build_projection;
use crate::{Engine, EngineError};

/// `true` iff `stmt` matches the SCALARSQ streaming shape — see the
/// module-level docs for the precise rules.
///
/// The detector is a pure AST inspection: ~10 boolean field checks
/// plus an items walk. Hot SELECT close paths can call it on every
/// query without measurable overhead.
pub fn is_scalarsq_streaming_shape(stmt: &SelectStatement) -> bool {
    // CTEs / DISTINCT / GROUP BY / HAVING / UNION / ORDER BY / WITH TIES —
    // any of these forces materialisation somewhere upstream of emit.
    if !stmt.ctes.is_empty() {
        return false;
    }
    if stmt.distinct {
        return false;
    }
    if stmt.group_by.is_some() || stmt.group_by_all {
        return false;
    }
    if stmt.having.is_some() {
        return false;
    }
    if !stmt.unions.is_empty() {
        return false;
    }
    if !stmt.order_by.is_empty() {
        return false;
    }
    if stmt.limit_with_ties {
        return false;
    }
    // FROM must be a regular single-table reference. UNNEST /
    // generate_series / LATERAL / AS OF SEGMENT each route through
    // their own scanners; the streaming executor stays narrow.
    let from = match &stmt.from {
        Some(f) => f,
        None => return false,
    };
    if !from.joins.is_empty() {
        return false;
    }
    let p = &from.primary;
    if p.as_of_segment.is_some()
        || p.unnest_expr.is_some()
        || p.generate_series_args.is_some()
        || p.lateral_subquery.is_some()
    {
        return false;
    }
    // Projection: no wildcard; at least one ScalarSubquery somewhere;
    // no disqualifying construct (window function, SRF, EXISTS, IN-
    // subquery — they're streaming-compatible per se but bring extra
    // executor surface the v0 streaming path won't cover).
    let mut has_scalar_sub = false;
    for item in &stmt.items {
        match item {
            SelectItem::Wildcard => return false,
            SelectItem::Expr { expr, .. } => {
                if expr_has_streaming_disqualifier(expr) {
                    return false;
                }
                if expr_has_scalar_subquery(expr) {
                    has_scalar_sub = true;
                }
            }
        }
    }
    has_scalar_sub
}

/// Walk `e` looking for an `Expr::ScalarSubquery` anywhere in the
/// tree. Sister of `subquery::expr_has_subquery` but narrowed to
/// only ScalarSubquery (Exists / InSubquery do NOT count for this
/// detector — they're already disqualifiers in `expr_has_streaming_
/// disqualifier`).
fn expr_has_scalar_subquery(e: &Expr) -> bool {
    match e {
        Expr::ScalarSubquery(_) => true,
        Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::RowInSubquery { .. }
        | Expr::RowCmpSubquery { .. } => false,
        Expr::AggregateOrdered { call, order_by, .. } => {
            expr_has_scalar_subquery(call)
                || order_by.iter().any(|o| expr_has_scalar_subquery(&o.expr))
        }
        Expr::Binary { lhs, rhs, .. } => {
            expr_has_scalar_subquery(lhs) || expr_has_scalar_subquery(rhs)
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => expr_has_scalar_subquery(expr),
        Expr::FunctionCall { args, .. } => args.iter().any(expr_has_scalar_subquery),
        Expr::Like { expr, pattern, .. } => {
            expr_has_scalar_subquery(expr) || expr_has_scalar_subquery(pattern)
        }
        Expr::Extract { source, .. } => expr_has_scalar_subquery(source),
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            args.iter().any(expr_has_scalar_subquery)
                || partition_by.iter().any(expr_has_scalar_subquery)
                || order_by.iter().any(|(e, _, _)| expr_has_scalar_subquery(e))
        }
        Expr::Literal(_) | Expr::Placeholder(_) | Expr::Column(_) => false,
        Expr::Array(items) => items.iter().any(expr_has_scalar_subquery),
        Expr::ArraySubscript { target, index } => {
            expr_has_scalar_subquery(target) || expr_has_scalar_subquery(index)
        }
        Expr::ArraySlice { target, lo, hi } => {
            expr_has_scalar_subquery(target)
                || lo.as_deref().is_some_and(expr_has_scalar_subquery)
                || hi.as_deref().is_some_and(expr_has_scalar_subquery)
        }
        Expr::AnyAll { expr, array, .. } => {
            expr_has_scalar_subquery(expr) || expr_has_scalar_subquery(array)
        }
        Expr::InList { expr, list, .. } => {
            expr_has_scalar_subquery(expr) || list.iter().any(expr_has_scalar_subquery)
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            operand.as_deref().is_some_and(expr_has_scalar_subquery)
                || branches
                    .iter()
                    .any(|(w, t)| expr_has_scalar_subquery(w) || expr_has_scalar_subquery(t))
                || else_branch.as_deref().is_some_and(expr_has_scalar_subquery)
        }
    }
}

/// Walk `e` for any construct the streaming executor (Step 2) won't
/// cover: window functions (require partition materialisation),
/// Exists / InSubquery in projection (extra subquery dispatch
/// surface), top-level SRFs (one input row → many output rows).
///
/// FunctionCall args are walked but FunctionCall itself isn't flagged
/// — the projection-eval path handles SRF detection at the SELECT
/// level via `is_top_level_unnest`; that's wired into the executor,
/// not the detector, since the detector only sees the AST.
/// Concretely the detector here treats top-level `Expr::FunctionCall
/// { name = "unnest", ... }` as a disqualifier.
fn expr_has_streaming_disqualifier(e: &Expr) -> bool {
    match e {
        Expr::WindowFunction { .. } => true,
        Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::RowInSubquery { .. }
        | Expr::RowCmpSubquery { .. } => true,
        // Top-level SRF. `is_top_level_unnest` (in select.rs) is the
        // authority on SRF detection at the SELECT level; for the
        // detector we recognise the canonical UNNEST call by name.
        Expr::FunctionCall { name, args, .. }
            if name.eq_ignore_ascii_case("unnest") && args.len() == 1 =>
        {
            true
        }
        Expr::FunctionCall { args, .. } => args.iter().any(expr_has_streaming_disqualifier),
        Expr::AggregateOrdered { call, order_by, .. } => {
            expr_has_streaming_disqualifier(call)
                || order_by
                    .iter()
                    .any(|o| expr_has_streaming_disqualifier(&o.expr))
        }
        Expr::Binary { lhs, rhs, .. } => {
            expr_has_streaming_disqualifier(lhs) || expr_has_streaming_disqualifier(rhs)
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => expr_has_streaming_disqualifier(expr),
        Expr::Like { expr, pattern, .. } => {
            expr_has_streaming_disqualifier(expr) || expr_has_streaming_disqualifier(pattern)
        }
        Expr::Extract { source, .. } => expr_has_streaming_disqualifier(source),
        Expr::ScalarSubquery(_) => false,
        Expr::Literal(_) | Expr::Placeholder(_) | Expr::Column(_) => false,
        Expr::Array(items) => items.iter().any(expr_has_streaming_disqualifier),
        Expr::ArraySubscript { target, index } => {
            expr_has_streaming_disqualifier(target) || expr_has_streaming_disqualifier(index)
        }
        Expr::ArraySlice { target, lo, hi } => {
            expr_has_streaming_disqualifier(target)
                || lo.as_deref().is_some_and(expr_has_streaming_disqualifier)
                || hi.as_deref().is_some_and(expr_has_streaming_disqualifier)
        }
        Expr::AnyAll { expr, array, .. } => {
            expr_has_streaming_disqualifier(expr) || expr_has_streaming_disqualifier(array)
        }
        Expr::InList { expr, list, .. } => {
            expr_has_streaming_disqualifier(expr)
                || list.iter().any(expr_has_streaming_disqualifier)
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            operand
                .as_deref()
                .is_some_and(expr_has_streaming_disqualifier)
                || branches.iter().any(|(w, t)| {
                    expr_has_streaming_disqualifier(w) || expr_has_streaming_disqualifier(t)
                })
                || else_branch
                    .as_deref()
                    .is_some_and(expr_has_streaming_disqualifier)
        }
    }
}

impl Engine {
    /// v7.37.42-arena Phase 2 — slim arena-aware streaming executor
    /// for the SCALARSQ shape (`SELECT col, scalar_subq FROM
    /// single_table [WHERE …] [LIMIT N]`). Mirrors
    /// `run_single_table_scan`'s hot path but without the
    /// `tagged: Vec<(order_keys, Row)>` intermediate, without the
    /// `Vec<Row<'static>>` post-loop QueryResult materialise, and
    /// with the per-row projection scratch `Vec<Value>` allocated in
    /// the caller-supplied bumpalo `Bump` arena — when the query
    /// ends, the arena drops in O(1) instead of paying N row Vec
    /// `Drop`s.
    ///
    /// The detector (`is_scalarsq_streaming_shape`) guarantees:
    ///   * single-table FROM (no joins / UNNEST / lateral / AS OF)
    ///   * no ORDER BY / GROUP BY / HAVING / DISTINCT / WITH TIES
    ///   * no UNION / EXCEPT / CTE
    ///   * no wildcard projection
    ///   * no top-level UNNEST SRF in projection
    /// so the post-loop sort/dedup/limit pipeline is unnecessary;
    /// each output row depends only on its outer row + the per-row
    /// scalar subquery evaluation.
    ///
    /// `emit` takes `&[Value<'a>]` (not `&Row`) so the per-row
    /// projection buffer can be reused via `clear()` + `push()`
    /// without a per-row outer `Vec` alloc. The arena lifetime `'a`
    /// is anchored by the caller's `&'a Bump`; the buffer + any
    /// future `&'a str` cell payloads (Phase 3) inherit it.
    ///
    /// Returns the column schema + total emitted row count
    /// (post-OFFSET, post-LIMIT).
    pub(crate) fn exec_scalarsq_streaming<'a, F>(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
        arena: &'a Bump,
        mut emit: F,
    ) -> Result<(Vec<ColumnSchema>, usize), EngineError>
    where
        F: FnMut(&[ColumnSchema], &[Value<'a>]) -> Result<(), EngineError>,
    {
        if !is_scalarsq_streaming_shape(stmt) {
            return Err(EngineError::Unsupported(
                "exec_scalarsq_streaming: not a streaming-shape SELECT".to_string(),
            ));
        }
        // Shape-check guarantees `stmt.from` is `Some(_)` with a
        // plain single-table primary.
        let from = stmt
            .from
            .as_ref()
            .expect("streaming shape requires FROM (detector enforces)");
        let primary = &from.primary;
        let catalog = self.active_catalog();
        let table = catalog.get(&primary.name).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound {
                name: primary.name.clone(),
            })
        })?;
        let schema_cols = &table.schema().columns;
        let alias = primary.alias.as_deref().unwrap_or(primary.name.as_str());
        let ctx = self.ev_ctx(schema_cols, Some(alias));
        let projection = build_projection(&stmt.items, schema_cols, alias)?;
        // v7.37.42 (docker-fair SCALARSQ attack 2) — pre-resolve every
        // projection item that's a bare `Expr::Column` to a direct
        // position into `row.values`. The hot loop then short-circuits
        // `eval_expr_with_correlated` (which walks the column-resolution
        // logic against the `EvalContext` per call) with a single
        // `row.values[pos].clone()` for these items. Bytes identical —
        // the eval interpreter's Column arm performs the same lookup.
        //
        // `None` entries here mean the projection item is NOT a bare
        // column (function call, arithmetic, scalar subquery, etc.) and
        // must go through the interpreter as before.
        let projection_direct_col: Vec<Option<usize>> = projection
            .iter()
            .map(|p| match &p.expr {
                Expr::Column(c) => {
                    // Match the eval interpreter's column-resolution
                    // semantics exactly (`resolve_column` in
                    // `eval/resolve.rs`) so the cached position points
                    // to the same cell the per-call path would have
                    // picked. Single-table scope: try composite
                    // `qualifier.name` first when a qualifier is
                    // present, then fall back to the bare column name.
                    // Case-sensitive `==` matches `resolve_column`'s
                    // `s.name == c.name` exactly. Bare-name suffix
                    // fallback (".name") is only meaningful for joined
                    // schemas which the streaming shape rejects, so we
                    // skip it here — any miss returns `None` and falls
                    // back to the interpreter path.
                    if let Some(q) = c.qualifier.as_deref() {
                        let composite = alloc::format!("{q}.{name}", name = c.name);
                        if let Some(p) = schema_cols.iter().position(|s| s.name == composite) {
                            return Some(p);
                        }
                    }
                    schema_cols.iter().position(|s| s.name == c.name)
                }
                _ => None,
            })
            .collect();
        let any_direct_col = projection_direct_col.iter().any(Option::is_some);
        // Index seek for WHERE — mirrors run_single_table_scan's
        // four-way fallback (BTree equality / GIN tsvector / trigram
        // LIKE / GIN JSONB containment). When none fires, the full
        // hot+cold scan covers the rest.
        // Phase C.3 step 2c — compute the reader's MVCC snapshot once
        // and thread it into every index-seek fast path. No-op today.
        let seek_snapshot = self.current_snapshot();
        let indexed_rows: Option<Vec<Cow<'_, Row<'static>>>> = stmt.where_.as_ref().and_then(|w| {
            try_index_seek(w, schema_cols, catalog, table, alias, &seek_snapshot)
                .or_else(|| {
                    try_gin_seek(w, schema_cols, catalog, table, alias, &ctx, &seek_snapshot)
                })
                .or_else(|| try_trgm_seek(w, schema_cols, table, alias, &seek_snapshot))
                .or_else(|| try_gin_jsonb_seek(w, schema_cols, table, alias, &seek_snapshot))
        });
        // Compile the WHERE once. For subquery-free predicates the
        // compiled path runs a flat step program; correlated /
        // subquery-bearing WHEREs fall through to the interpreter.
        let compiled_where: Option<eval::CompiledExpr> = stmt
            .where_
            .as_ref()
            .filter(|w| eval::fully_compilable(w))
            .map(|w| eval::compile_expr(w, &ctx));
        let mut eval_stack: Vec<Value<'static>> = Vec::new();
        // Pre-analyse every projection-item scalar subquery for the
        // PK-probe fast path. Per-row eval becomes one outer-column
        // read + one PK index seek — no Expr clone, no walker.
        let scalarsq_fast: Vec<Option<crate::ScalarPkProbeFastPath>> = projection
            .iter()
            .map(|p| {
                if let Expr::ScalarSubquery(inner) = &p.expr {
                    self.analyse_scalar_count_pk_eq_probe(inner, schema_cols, alias)
                } else {
                    None
                }
            })
            .collect();
        let any_scalarsq_fast = scalarsq_fast.iter().any(Option::is_some);
        // OFFSET/LIMIT — `target = OFFSET + LIMIT` short-circuits the
        // scan once enough rows are emitted.
        let lim = stmt.limit_literal().map(|n| n as usize);
        let off = stmt.offset_literal().map(|n| n as usize).unwrap_or(0);
        let target = lim.map(|n| n.saturating_add(off));
        // Skip the batch-correlated-scalar memo for tiny outer
        // counts (LIMIT 100-ish): the batch path scans the FULL inner
        // table; per-row PK seek (~5 µs × 100 = 500 µs) wins.
        let pass_memo = target.is_none_or(|cap| cap > 1000);
        let mut memo = MemoizeCache::new();
        let mut budget = ByteBudget::new(self.max_query_bytes);
        // Output columns built once; the caller (wire encoder)
        // borrows this slice for every row.
        let columns: Vec<ColumnSchema> = projection
            .iter()
            .map(|p| ColumnSchema::new(p.output_name.clone(), p.ty, p.nullable))
            .collect();
        // v7.37.42-arena Phase 2 — row projection buffer lives in
        // the bumpalo arena. The buffer is reused via `clear()` +
        // `push()` per row, but the Vec storage itself (and any
        // future `&'a str` cell payloads from Phase 3) is arena-
        // allocated, so the arena's bulk-reset on query end is what
        // replaces per-row Drop.
        let mut row_buf: bumpalo::collections::Vec<'a, Value<'a>> =
            bumpalo::collections::Vec::with_capacity_in(projection.len(), arena);
        let mut survived: usize = 0; // pre-LIMIT/OFFSET count
        let mut emitted: usize = 0; // post-LIMIT/OFFSET count
        // Per-row work: WHERE-filter + project + apply offset/limit
        // + emit. Captures `&mut emit`, `&mut row_buf`, `&mut
        // survived`, `&mut emitted`, plus the per-row tooling. Hot
        // path identical to run_single_table_scan's process_row but
        // writes to `emit` directly instead of `tagged.push`.
        #[allow(clippy::too_many_arguments)]
        fn run_one<'a, F>(
            engine: &Engine,
            row: &Row<'static>,
            loop_idx: usize,
            stmt: &SelectStatement,
            ctx: &crate::eval::EvalContext<'_>,
            compiled_where: &Option<eval::CompiledExpr>,
            projection: &[crate::select::ProjectedItem],
            scalarsq_fast: &[Option<crate::ScalarPkProbeFastPath>],
            any_scalarsq_fast: bool,
            projection_direct_col: &[Option<usize>],
            any_direct_col: bool,
            pass_memo: bool,
            off: usize,
            row_buf: &mut bumpalo::collections::Vec<'a, Value<'a>>,
            memo: &mut MemoizeCache,
            eval_stack: &mut Vec<Value<'static>>,
            survived: &mut usize,
            emitted: &mut usize,
            budget: &mut ByteBudget,
            columns: &[ColumnSchema],
            cancel: CancelToken<'_>,
            emit: &mut F,
        ) -> Result<(), EngineError>
        where
            F: FnMut(&[ColumnSchema], &[Value<'a>]) -> Result<(), EngineError>,
        {
            if loop_idx.is_multiple_of(256) {
                cancel.check()?;
            }
            if let Some(cw) = compiled_where {
                let cond =
                    eval::eval_compiled(cw, row, ctx, eval_stack).map_err(EngineError::Eval)?;
                if !matches!(cond, Value::Bool(true)) {
                    return Ok(());
                }
            } else if let Some(where_expr) = &stmt.where_ {
                let cond =
                    engine.eval_expr_with_correlated(where_expr, row, ctx, cancel, Some(memo))?;
                if !matches!(cond, Value::Bool(true)) {
                    return Ok(());
                }
            }
            // Build projection row in arena-backed buffer.
            row_buf.clear();
            for (i, p) in projection.iter().enumerate() {
                if any_scalarsq_fast && let Some(fp) = &scalarsq_fast[i] {
                    row_buf.push(engine.probe_with_pk_fast_path(fp, row));
                    continue;
                }
                // v7.37.42 attack 2 — bare `Expr::Column` projection
                // items short-circuit the eval interpreter. The
                // resolved position was computed once at prepare time
                // against the same schema_cols the row layout follows.
                if any_direct_col && let Some(pos) = projection_direct_col[i] {
                    let v = row.values.get(pos).cloned().unwrap_or(Value::Null);
                    row_buf.push(v);
                    continue;
                }
                let memo_arg = if pass_memo { Some(&mut *memo) } else { None };
                row_buf
                    .push(engine.eval_expr_with_correlated(&p.expr, row, ctx, cancel, memo_arg)?);
            }
            // OFFSET: drop the first `off` survived rows.
            *survived = survived.saturating_add(1);
            if *survived <= off {
                return Ok(());
            }
            // Budget enforcement matches run_single_table_scan: charge
            // each row before emit so a fat scan trips
            // QueryBytesExceeded at the ceiling instead of past it.
            budget.charge(approx_values_bytes(row_buf))?;
            emit(columns, row_buf)?;
            *emitted = emitted.saturating_add(1);
            Ok(())
        }
        // Hot tier first, then cold tier — matches
        // run_single_table_scan's order so result ordering is the
        // same. Streaming shape has no ORDER BY so the order isn't
        // semantically observable, but matching the generic path
        // makes the differential test byte-exact for single-tier.
        if let Some(rows) = &indexed_rows {
            for (loop_idx, cow) in rows.iter().enumerate() {
                if let Some(cap) = target
                    && emitted >= cap.saturating_sub(off)
                {
                    break;
                }
                run_one(
                    self,
                    cow.as_ref(),
                    loop_idx,
                    stmt,
                    &ctx,
                    &compiled_where,
                    &projection,
                    &scalarsq_fast,
                    any_scalarsq_fast,
                    &projection_direct_col,
                    any_direct_col,
                    pass_memo,
                    off,
                    &mut row_buf,
                    &mut memo,
                    &mut eval_stack,
                    &mut survived,
                    &mut emitted,
                    &mut budget,
                    &columns,
                    cancel,
                    &mut emit,
                )?;
            }
        } else {
            for i in 0..table.row_count() {
                if let Some(cap) = target
                    && emitted >= cap.saturating_sub(off)
                {
                    break;
                }
                // v7.37.15 (Phase C.3, step 2) — MVCC visibility gate for
                // the streaming single-table full-scan path. Mirrors the
                // gate on `run_single_table_scan`: this is a user-query
                // result path, so under gate-on (`SPG_MVCC_INPLACE`) it
                // must skip rows the reader's snapshot cannot see. Reuses
                // the `seek_snapshot` computed above. A no-op under the
                // default gate-off (every hot row is frozen/alive). Cold
                // rows are frozen (visible) by definition — left ungated.
                if !table.is_row_visible(i, &seek_snapshot) {
                    continue;
                }
                run_one(
                    self,
                    &table.rows()[i],
                    i,
                    stmt,
                    &ctx,
                    &compiled_where,
                    &projection,
                    &scalarsq_fast,
                    any_scalarsq_fast,
                    &projection_direct_col,
                    any_direct_col,
                    pass_memo,
                    off,
                    &mut row_buf,
                    &mut memo,
                    &mut eval_stack,
                    &mut survived,
                    &mut emitted,
                    &mut budget,
                    &columns,
                    cancel,
                    &mut emit,
                )?;
            }
            // Cold tier — mirrors run_single_table_scan's final loop.
            // v7.37.42 attack 3 — skip the cold-tier PK-index walk when
            // the catalog has no cold segments loaded. On a hot-only
            // database (typical bench/docker-fair shape) this saves
            // the per-query walk of the entire PK BTree (every entry
            // checked for `RowLocator::Cold`) that produced ~10-20 µs
            // of overhead on small tables.
            if self.active_catalog().has_any_cold_segments() {
                let cold_rows = self.iter_cold_rows_of_table(table);
                for (offset, row) in cold_rows.iter().enumerate() {
                    if let Some(cap) = target
                        && emitted >= cap.saturating_sub(off)
                    {
                        break;
                    }
                    run_one(
                        self,
                        row,
                        table.row_count() + offset,
                        stmt,
                        &ctx,
                        &compiled_where,
                        &projection,
                        &scalarsq_fast,
                        any_scalarsq_fast,
                        &projection_direct_col,
                        any_direct_col,
                        pass_memo,
                        off,
                        &mut row_buf,
                        &mut memo,
                        &mut eval_stack,
                        &mut survived,
                        &mut emitted,
                        &mut budget,
                        &columns,
                        cancel,
                        &mut emit,
                    )?;
                }
            }
        }
        Ok((columns, emitted))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spg_sql::ast::Statement;
    use spg_sql::parser::parse_statement;

    fn select_from(sql: &str) -> SelectStatement {
        match parse_statement(sql).expect("parse") {
            Statement::Select(s) => s,
            other => panic!("expected SELECT, got {other:?}"),
        }
    }

    // ---- POSITIVE: shapes the streaming executor should pick up ----

    #[test]
    fn p1_bare_scalar_subq_in_projection() {
        let s = select_from("SELECT id, (SELECT 1) FROM t");
        assert!(is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn p2_correlated_scalar_subq_with_pk_probe() {
        let s = select_from("SELECT id, (SELECT name FROM t2 WHERE t2.pk = t.fk) FROM t");
        assert!(is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn p3_scalar_subq_with_where() {
        let s =
            select_from("SELECT id, (SELECT name FROM t2 WHERE t2.pk = t.fk) FROM t WHERE x = 1");
        assert!(is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn p4_scalar_subq_with_limit() {
        let s = select_from("SELECT id, (SELECT name FROM t2 WHERE t2.pk = t.fk) FROM t LIMIT 100");
        assert!(is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn p5_full_scalarsq_shape_where_and_limit() {
        let s = select_from(
            "SELECT id, (SELECT name FROM t2 WHERE t2.pk = t.fk) FROM t WHERE x = 1 LIMIT 100",
        );
        assert!(is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn p6_multi_projection_with_one_scalar_subq() {
        let s = select_from("SELECT id, name, (SELECT k FROM t2 WHERE t2.pk = t.fk), email FROM t");
        assert!(is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn p7_scalar_subq_in_binary_expr() {
        let s = select_from("SELECT id, (SELECT 1) + 1 FROM t");
        assert!(is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn p8_scalar_subq_in_case_branch() {
        let s = select_from("SELECT id, CASE WHEN id > 0 THEN (SELECT 1) ELSE 0 END FROM t");
        assert!(is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn p9_scalar_subq_in_function_arg() {
        let s = select_from("SELECT id, COALESCE((SELECT 1), 0) FROM t");
        assert!(is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn p10_offset_alone_still_streaming() {
        // OFFSET without ORDER BY is well-defined as "drop first N
        // rows of the scan output"; no sort required, so the
        // streaming executor can still serve it.
        let s = select_from("SELECT id, (SELECT 1) FROM t OFFSET 5");
        assert!(is_scalarsq_streaming_shape(&s));
    }

    // ---- NEGATIVE: shapes the streaming executor must skip ----

    #[test]
    fn n1_wildcard_projection() {
        let s = select_from("SELECT * FROM t");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n2_no_scalar_subquery() {
        let s = select_from("SELECT id, name FROM t");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n3_join_rejects_even_with_scalar_subq() {
        let s = select_from("SELECT t.id, (SELECT 1) FROM t INNER JOIN t2 ON t.id = t2.id");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n4_exists_in_projection_disqualifies() {
        let s = select_from("SELECT id, EXISTS(SELECT 1 FROM t2 WHERE t2.fk = t.id) FROM t");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n5_in_subquery_in_projection_disqualifies() {
        let s = select_from("SELECT id, x IN (SELECT y FROM t2) FROM t");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n6_order_by_forces_materialise() {
        let s = select_from("SELECT id, (SELECT 1) FROM t ORDER BY id");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n7_distinct_forces_dedupe() {
        let s = select_from("SELECT DISTINCT id, (SELECT 1) FROM t");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n8_group_by_rejects() {
        let s = select_from("SELECT id, (SELECT 1) FROM t GROUP BY id");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n9_having_rejects() {
        // HAVING requires GROUP BY; the parser still accepts it
        // alone (treats as a degenerate single-group filter) — either
        // way, having forces the aggregate pipeline.
        let s = select_from("SELECT id, (SELECT 1) FROM t GROUP BY id HAVING id > 0");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n10_union_chain_rejects() {
        let s = select_from("SELECT id, (SELECT 1) FROM t UNION ALL SELECT 2, 3 FROM t2");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n11_cte_present_rejects() {
        let s = select_from("WITH c AS (SELECT 1) SELECT id, (SELECT 1) FROM t");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n12_unnest_from_rejects() {
        let s = select_from("SELECT (SELECT 1) FROM unnest(ARRAY[1,2,3])");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n13_window_function_in_projection_rejects() {
        let s = select_from("SELECT id, ROW_NUMBER() OVER (PARTITION BY x), (SELECT 1) FROM t");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n14_unnest_in_projection_rejects() {
        let s = select_from("SELECT id, unnest(arr), (SELECT 1) FROM t");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n15_no_from_rejects() {
        // `SELECT (SELECT 1)` — no FROM clause. The streaming
        // executor needs a table to scan; bare SELECT goes through
        // the generic path (it's cheap there anyway).
        let s = select_from("SELECT (SELECT 1)");
        assert!(!is_scalarsq_streaming_shape(&s));
    }
}
