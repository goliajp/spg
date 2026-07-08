//! Index-access planning — B-tree equality seeks, PK fast paths, GIN /
//! trigram / NSW-vector lookups, and the `col = literal` helpers behind
//! them. Split out of `lib.rs` (v7.32 engine modularisation). Pure
//! planners over (WHERE/ORDER expr, schema, catalog, table): they return
//! candidate row indices/locators or `None` to fall back to a scan.

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::vec::Vec;

use spg_sql::ast::{BinOp, Expr, Literal, SelectStatement};
use spg_storage::{Catalog, ColumnSchema, IndexKey, Row, Table, Value};

use crate::eval::{self, EvalContext};
use crate::{
    CancelToken, Engine, EngineError, QueryResult, apply_offset_and_limit, build_projection,
    memoize,
};

/// Try to plan a WHERE clause as an equality lookup against an existing
/// index. Returns the candidate row indices on success; `None` means the
/// caller should fall back to a full scan.
///
/// v0.8 recognises a single top-level `col = literal` (in either operand
/// order). AND chains and range scans land in later milestones.
/// Look for `ORDER BY col <dist-op> literal LIMIT k` against an
/// NSW-indexed vector column. Recognised distance ops: `<->` (L2),
/// `<#>` (inner product), `<=>` (cosine). When a WHERE clause is
/// present, the planner does an "over-fetch and filter" pass — it
/// asks the graph for `k * over_fetch` candidates, evaluates WHERE
/// against each, and trims back to `k`. Returns the row indices in
/// ascending-distance order when the plan applies.
pub(crate) fn try_nsw_knn(
    stmt: &SelectStatement,
    table: &Table,
    schema_cols: &[ColumnSchema],
    table_alias: &str,
    snapshot: &spg_storage::snapshot::Snapshot,
) -> Option<Vec<usize>> {
    if stmt.distinct {
        return None;
    }
    let limit = usize::try_from(stmt.limit_literal()?).ok()?;
    if limit == 0 {
        return None;
    }
    // v6.4.0 — NSW kNN dispatch needs a single ORDER BY key on the
    // distance metric. Multi-key ORDER BY falls through to the
    // generic sort path.
    if stmt.order_by.len() != 1 {
        return None;
    }
    let order = &stmt.order_by[0];
    // NSW kNN returns rows ascending by distance — DESC inverts the
    // natural order, so the planner can't handle it without a sort
    // pass. Fall back to the generic ORDER BY path.
    if order.desc {
        return None;
    }
    let Expr::Binary { lhs, op, rhs } = &order.expr else {
        return None;
    };
    let metric = match op {
        BinOp::L2Distance => spg_storage::NswMetric::L2,
        BinOp::InnerProduct => spg_storage::NswMetric::InnerProduct,
        BinOp::CosineDistance => spg_storage::NswMetric::Cosine,
        _ => return None,
    };
    // Accept both `col <op> literal` and `literal <op> col`.
    let ((Expr::Column(col), literal) | (literal, Expr::Column(col))) =
        (lhs.as_ref(), rhs.as_ref())
    else {
        return None;
    };
    if let Some(q) = &col.qualifier
        && q != table_alias
    {
        return None;
    }
    let col_pos = schema_cols.iter().position(|s| s.name == col.name)?;
    let query = literal_to_vector(literal)?;
    let idx = spg_storage::nsw_index_on(table, col_pos)?;
    if let Some(where_expr) = &stmt.where_ {
        // Over-fetch and filter. The factor (10×) is a heuristic that
        // covers typical selectivity for the corpus tests; v2.x will
        // make it configurable.
        let over_fetch = limit.saturating_mul(10).max(NSW_OVER_FETCH_FLOOR);
        let candidates = spg_storage::nsw_query(table, &idx.name, &query, over_fetch, metric);
        let ctx = EvalContext::new(schema_cols, Some(table_alias));
        let mut kept: Vec<usize> = Vec::with_capacity(limit);
        for i in candidates {
            // Phase C.3 step 2c — MVCC read gate. Skip hot rows this
            // snapshot cannot see so an in-place writer's dead/old
            // version never surfaces on the NSW kNN fast path, and an
            // invisible row never counts toward LIMIT. No-op today.
            if !table.is_row_visible(i, snapshot) {
                continue;
            }
            let row = &table.rows()[i];
            let cond = eval::eval_expr(where_expr, row, &ctx).ok()?;
            if matches!(cond, Value::Bool(true)) {
                kept.push(i);
                if kept.len() >= limit {
                    break;
                }
            }
        }
        Some(kept)
    } else {
        // Phase C.3 step 2c — MVCC read gate on the WHERE-less kNN
        // path too: drop hot rows this snapshot cannot see so a
        // writer's dead/old version never surfaces. No-op today (every
        // hot header is committed-alive so `is_row_visible` is `true`).
        Some(
            spg_storage::nsw_query(table, &idx.name, &query, limit, metric)
                .into_iter()
                .filter(|&i| table.is_row_visible(i, snapshot))
                .collect(),
        )
    }
}

/// Lower bound on the over-fetch pool when WHERE is present — even
/// for tiny `LIMIT 1` queries we keep enough candidates to absorb a
/// few WHERE rejections.
const NSW_OVER_FETCH_FLOOR: usize = 32;

/// v7.34.5 — drive the row scan via a BTree index walk in the
/// requested ORDER BY direction, emitting matched row indices lazily
/// and stopping after `LIMIT + OFFSET` survive WHERE. Avoids the
/// full-table materialise + partial-sort tail that the mailrs
/// `content_worker` baseline pinned at 80 ms across 250 k rows.
/// Returns `Some(row_indices)` in the order the caller's ORDER BY
/// asked for so `materialise_in_order` can skip its own sort pass;
/// `None` falls through to the existing scan + sort path.
///
/// Eligibility (any failure → `None`):
///   * no `DISTINCT`, no `LIMIT WITH TIES` (both need a full sort).
///   * `LIMIT N` literal present, `N > 0`.
///   * `OFFSET` literal absent or small enough that scanning past it
///     stays cheap (the walker collects `N + OFFSET` rows then trims).
///   * `ORDER BY` is exactly one entry, the expression is a bare
///     column on this table, and the column has a BTree index.
///   * No GROUP BY / HAVING (those are aggregate-bound paths handled
///     elsewhere; this helper sits on `exec_bare_select`'s primary).
///   * Cold-tier locators short-circuit to `None` so the walker
///     never crosses a tier boundary mid-scan; the legacy path
///     handles cold rows.
pub(crate) fn try_pk_walk_top_n<'a>(
    stmt: &SelectStatement,
    catalog: &'a spg_storage::Catalog,
    table: &'a Table,
    schema_cols: &[ColumnSchema],
    table_alias: &str,
    engine: &Engine,
    cancel: CancelToken<'_>,
) -> Option<Vec<Cow<'a, Row<'static>>>> {
    if stmt.distinct || stmt.limit_with_ties {
        return None;
    }
    if stmt.group_by.is_some() || stmt.having.is_some() {
        return None;
    }
    let limit = usize::try_from(stmt.limit_literal()?).ok()?;
    if limit == 0 {
        return None;
    }
    let offset = stmt
        .offset_literal()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0);
    // Cap absolute walk cost: if OFFSET is huge, the walker would emit
    // `offset + limit` candidates before returning anything, which can
    // exceed the partial-sort fast path's complexity. The legacy path
    // already handles large-OFFSET sweeps with select_nth_unstable.
    const WALKER_OFFSET_CAP: usize = 65_536;
    if offset > WALKER_OFFSET_CAP {
        return None;
    }
    let want = offset.checked_add(limit)?;
    if stmt.order_by.len() != 1 {
        return None;
    }
    let order = &stmt.order_by[0];
    let Expr::Column(col) = &order.expr else {
        return None;
    };
    if let Some(q) = &col.qualifier
        && q != table_alias
    {
        return None;
    }
    let col_pos = schema_cols
        .iter()
        .position(|s| s.name.eq_ignore_ascii_case(&col.name))?;
    let index = table.index_on(col_pos)?;
    if !matches!(index.kind, spg_storage::IndexKind::BTree(_)) {
        return None;
    }
    let where_expr = stmt.where_.as_ref();
    let ctx = EvalContext::new(schema_cols, Some(table_alias));
    let table_name = table.schema().name.as_str();
    let mut kept: Vec<Cow<'a, Row>> = Vec::with_capacity(want);
    // v7.37.x (mailrs prod content_worker NOT EXISTS) — share one
    // MemoizeCache across the walk so a materialised InList (from
    // `subquery_replacement` of an uncorrelated InSubquery / the
    // NOT EXISTS pullup) hits the in-set fast path instead of an
    // O(N×M) linear scan per row. Without the memo, 25 k outer ×
    // 25 k InList on the prod messages × attachment_content shape
    // cost ~6 s.
    let mut memo = memoize::MemoizeCache::new();
    // v7.37.5-A2b (profile-guided) — content_worker prod profile
    // showed 24% self-time in `resolve_column` (linear scan of
    // ctx.columns per cell access) because the per-row WHERE eval
    // path went through `eval_with_in_sets` → `eval_expr(lhs, ...)`
    // → `Expr::Column` → `resolve_column`. The same WHERE expression
    // is fully-compilable (an `m.id NOT IN (literal_list)` after
    // `subquery_replacement`) — pre-compile it once and run
    // `eval_compiled` per row, pre-resolving the column position.
    // Falls back to the eval path for non-compilable WHEREs.
    let compiled_where: Option<eval::CompiledExpr> = where_expr
        .filter(|w| eval::fully_compilable(w))
        .map(|w| eval::compile_expr(w, &ctx));
    let mut eval_stack: Vec<spg_storage::Value<'static>> = Vec::new();
    // The walker yields `(IndexKey, &Vec<RowLocator>)` ordered by key
    // in the requested direction; per-key locator order within the
    // map preserves insertion order, which matches the legacy stable-
    // sort tie-break for equal keys.
    let walker: Box<dyn Iterator<Item = (&spg_storage::IndexKey, &Vec<spg_storage::RowLocator>)>> =
        if order.desc {
            Box::new(index.iter_desc())
        } else {
            Box::new(index.iter_asc())
        };
    // Phase C.3 step 2b — MVCC read gate. Compute the reader's
    // snapshot once; hot rows this snapshot cannot see are skipped so
    // an in-place writer's dead/old version never surfaces on the
    // BTree-walk fast path. Cold-tier locators are frozen segments =
    // always visible, so they stay ungated. No-op today: every hot
    // header is frozen/committed-alive, so `is_row_visible` is `true`.
    let scan_snapshot = engine.current_snapshot();
    for (key, locators) in walker {
        for loc in locators {
            // v7.34.7 (mailrs prod #6 follow-up) — single-table walker
            // gains the same hot/cold dispatch the JOIN walker got in
            // 7.34.6. `mailrs_prod_plain_limit` (`SELECT ... FROM
            // messages WHERE ... ORDER BY id DESC LIMIT N`) was the
            // remaining shape that bailed on the first cold locator
            // and fell back to the legacy materialise + partial-sort
            // path on the 803 MB prod catalog.
            let row_cow: Cow<'a, Row> = match *loc {
                spg_storage::RowLocator::Hot(row_idx) => {
                    if !table.is_row_visible(row_idx, &scan_snapshot) {
                        continue;
                    }
                    match table.rows().get(row_idx) {
                        Some(r) => Cow::Borrowed(r),
                        None => continue,
                    }
                }
                spg_storage::RowLocator::Cold { segment_id, .. } => {
                    match catalog.resolve_cold_locator(table_name, segment_id, key) {
                        Some(r) => Cow::Owned(r),
                        None => continue,
                    }
                }
            };
            if let Some(cw) = &compiled_where {
                // v7.37.5-A2b — compiled path: column positions resolved
                // at compile time; `eval_compiled` is allocator-free per
                // row (stack reused).
                let cond = eval::eval_compiled(cw, row_cow.as_ref(), &ctx, &mut eval_stack).ok()?;
                if !matches!(cond, Value::Bool(true)) {
                    continue;
                }
            } else if let Some(w) = where_expr {
                let cond = engine
                    .eval_expr_with_correlated(w, row_cow.as_ref(), &ctx, cancel, Some(&mut memo))
                    .ok()?;
                if !matches!(cond, Value::Bool(true)) {
                    continue;
                }
            }
            kept.push(row_cow);
            if kept.len() >= want {
                return Some(kept);
            }
        }
    }
    Some(kept)
}

/// Pull a `Vec<f32>` out of a literal-or-cast expression. Returns
/// `None` for anything we can't fold at plan time.
pub(crate) fn literal_to_vector(e: &Expr) -> Option<Vec<f32>> {
    match e {
        Expr::Literal(Literal::Vector(v)) => Some(v.clone()),
        Expr::Cast { expr, .. } => literal_to_vector(expr),
        _ => None,
    }
}

/// Materialise rows in a planner-supplied order (used by the NSW path)
/// without re-running ORDER BY. The projection + LIMIT slot mirror the
/// equivalent block in `exec_bare_select`.
pub(crate) fn materialise_in_order(
    stmt: &SelectStatement,
    schema_cols: &[ColumnSchema],
    table_alias: &str,
    ordered_rows: &[Cow<'_, Row<'static>>],
) -> Result<QueryResult, EngineError> {
    let ctx = EvalContext::new(schema_cols, Some(table_alias));
    let projection = build_projection(&stmt.items, schema_cols, table_alias)?;
    let mut output_rows: Vec<Row<'static>> = Vec::with_capacity(ordered_rows.len());
    for row_cow in ordered_rows {
        let row = row_cow.as_ref();
        let mut values = Vec::with_capacity(projection.len());
        for p in &projection {
            values.push(eval::eval_expr(&p.expr, row, &ctx)?);
        }
        output_rows.push(Row::new(values));
    }
    apply_offset_and_limit(
        &mut output_rows,
        stmt.offset_literal(),
        stmt.limit_literal(),
    );
    let columns: Vec<ColumnSchema> = projection
        .into_iter()
        .map(|p| ColumnSchema::new(p.output_name, p.ty, p.nullable))
        .collect();
    Ok(QueryResult::Rows {
        columns,
        rows: output_rows,
    })
}

/// v7.20 P4 — hot-row POSITION seek for the mutation paths
/// (UPDATE / DELETE index their planned writes by position in
/// `table.rows()`, so the Cow-row shape `try_index_seek`
/// returns doesn't fit). Same top-level-AND recursion and
/// col=literal resolution; the caller re-applies the full WHERE
/// to every returned row so the index only narrows candidates.
///
/// Returns `None` (→ caller full-scans) when no equality leaf
/// hits an index OR any matching locator lives in the cold tier
/// — the mutation paths operate on hot rows, and the PK
/// promote-then-walk upstream already handles the
/// cold-single-row case.
pub(crate) fn try_index_seek_positions(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    table: &Table,
    table_alias: &str,
) -> Option<Vec<usize>> {
    if let Expr::Binary {
        lhs,
        op: BinOp::And,
        rhs,
    } = where_expr
    {
        if let Some(p) = try_index_seek_positions(lhs, schema_cols, table, table_alias) {
            return Some(p);
        }
        return try_index_seek_positions(rhs, schema_cols, table, table_alias);
    }
    let Expr::Binary {
        lhs,
        op: BinOp::Eq,
        rhs,
    } = where_expr
    else {
        return None;
    };
    let (col_pos, value) = resolve_col_literal_pair(lhs, rhs, schema_cols, table_alias)
        .or_else(|| resolve_col_literal_pair(rhs, lhs, schema_cols, table_alias))?;
    let idx = table.index_on(col_pos)?;
    let key = IndexKey::from_value(&value)?;
    let locators = idx.lookup_eq(&key);
    let mut out = Vec::with_capacity(locators.len());
    for loc in locators {
        match *loc {
            spg_storage::RowLocator::Hot(i) => out.push(i),
            spg_storage::RowLocator::Cold { .. } => return None,
        }
    }
    Some(out)
}

pub(crate) fn try_index_seek<'a>(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    catalog: &'a Catalog,
    table: &'a Table,
    table_alias: &str,
    snapshot: &spg_storage::snapshot::Snapshot,
) -> Option<Vec<Cow<'a, Row<'static>>>> {
    // v7.11.3 — recurse through top-level `AND` so a PG-style
    // composite predicate like `WHERE id = 1 AND created_at > $1`
    // still hits the index on `id`. The caller re-applies the
    // full WHERE expression to each returned row, so dropping the
    // residual conjuncts here is correct — the index just narrows
    // the candidate set.
    if let Expr::Binary {
        lhs,
        op: BinOp::And,
        rhs,
    } = where_expr
    {
        // Try LHS first (typical convention: leading equality on
        // the indexed column comes first in user-written SQL).
        if let Some(rows) = try_index_seek(lhs, schema_cols, catalog, table, table_alias, snapshot)
        {
            return Some(rows);
        }
        return try_index_seek(rhs, schema_cols, catalog, table, table_alias, snapshot);
    }
    // v7.33 (mailrs 7.33.0) — `indexed_col IN (lit, …)` seeks each literal
    // and unions the rows (PG's bitmap index scan) instead of a full scan
    // + per-row membership test. The single-table path otherwise tested a
    // 60-element list against every row (24k × 60 string compares ~66 ms).
    if let Some(rows) =
        try_inlist_seek(where_expr, schema_cols, catalog, table, table_alias, snapshot)
    {
        return Some(rows);
    }
    let Expr::Binary {
        lhs,
        op: BinOp::Eq,
        rhs,
    } = where_expr
    else {
        return None;
    };
    let (col_pos, value) = resolve_col_literal_pair(lhs, rhs, schema_cols, table_alias)
        .or_else(|| resolve_col_literal_pair(rhs, lhs, schema_cols, table_alias))?;
    let idx = table.index_on(col_pos)?;
    let key = IndexKey::from_value(&value)?;
    let locators = idx.lookup_eq(&key);
    let table_name = table.schema().name.as_str();
    // v5.1: each locator dispatches to either the hot tier (zero-
    // copy borrow of `table.rows()[i]`) or a cold-tier segment
    // (one page read + dense row decode, ~µs scale). Cold rows are
    // returned as `Cow::Owned` so the caller's `&Row<'static>` iteration
    // doesn't see a tier distinction; pre-freezer (no cold
    // segments loaded) every locator is `Hot` and every entry is
    // `Cow::Borrowed` — identical cost to the pre-v5.1 path.
    let mut out: Vec<Cow<'a, Row>> = Vec::with_capacity(locators.len());
    for loc in locators {
        match *loc {
            spg_storage::RowLocator::Hot(i) => {
                // Phase C.3 step 2c — MVCC read gate: skip hot rows this
                // snapshot cannot see. No-op today.
                if !table.is_row_visible(i, snapshot) {
                    continue;
                }
                if let Some(row) = table.rows().get(i) {
                    out.push(Cow::Borrowed(row));
                }
            }
            spg_storage::RowLocator::Cold { segment_id, .. } => {
                if let Some(row) = catalog.resolve_cold_locator(table_name, segment_id, &key) {
                    out.push(Cow::Owned(row));
                }
            }
        }
    }
    Some(out)
}

/// v7.33 (mailrs 7.33.0) — `indexed_col IN (lit, …)` candidate seek.
/// Returns the union of per-literal index lookups when `where_expr` is a
/// non-negated IN-list whose LHS is an indexed column (qualified to this
/// table or bare) and whose elements are all literals; None otherwise, so
/// the caller falls through to its Eq seek / full scan. The caller
/// re-applies the full WHERE per row, so the exact per-literal seek set is
/// correct (duplicate keys just revisit a row, which the re-eval dedups by
/// truth, not identity — harmless for a candidate set).
fn try_inlist_seek<'a>(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    catalog: &'a Catalog,
    table: &'a Table,
    table_alias: &str,
    snapshot: &spg_storage::snapshot::Snapshot,
) -> Option<Vec<Cow<'a, Row<'static>>>> {
    let Expr::InList {
        expr,
        list,
        negated: false,
    } = where_expr
    else {
        return None;
    };
    let Expr::Column(c) = expr.as_ref() else {
        return None;
    };
    if !c
        .qualifier
        .as_deref()
        .is_none_or(|q| q.eq_ignore_ascii_case(table_alias))
    {
        return None;
    }
    let col_pos = schema_cols.iter().position(|s| s.name == c.name)?;
    let idx = table.index_on(col_pos)?;
    // Every element must be a literal; bail (full scan) otherwise.
    let mut keys: Vec<IndexKey> = Vec::with_capacity(list.len());
    for e in list {
        let Expr::Literal(l) = e else {
            return None;
        };
        keys.push(IndexKey::from_value(&eval::literal_to_value(l))?);
    }
    let table_name = table.schema().name.as_str();
    let mut out: Vec<Cow<'a, Row>> = Vec::new();
    for key in &keys {
        for loc in idx.lookup_eq(key) {
            match *loc {
                spg_storage::RowLocator::Hot(i) => {
                    // Phase C.3 step 2c — MVCC read gate. No-op today.
                    if !table.is_row_visible(i, snapshot) {
                        continue;
                    }
                    if let Some(row) = table.rows().get(i) {
                        out.push(Cow::Borrowed(row));
                    }
                }
                spg_storage::RowLocator::Cold { segment_id, .. } => {
                    if let Some(row) = catalog.resolve_cold_locator(table_name, segment_id, key) {
                        out.push(Cow::Owned(row));
                    }
                }
            }
        }
    }
    Some(out)
}

/// v7.12.3 — GIN-accelerated candidate seek for `WHERE col @@ <ts_query>`.
///
/// Recurses through top-level `AND` like [`try_index_seek`] so a
/// composite predicate `WHERE search_vector @@ q AND id > $1` still
/// hits the GIN index on `search_vector` — the caller re-applies the
/// full WHERE expression to each returned candidate, so dropping the
/// `id > $1` residual here stays semantically correct.
///
/// Returns `None` when:
///   - no leaf is a `col @@ <rhs>` shape on a GIN-indexed column;
///   - the RHS can't be const-evaluated to a `Value::TsQuery`
///     (typically because it references row columns);
///   - the resolved `TsQuery` uses query shapes the MVP doesn't
///     accelerate (`Not`, `Phrase` — those fall through to full scan).
///
/// On `Some(rows)` the caller iterates only `rows` and re-evaluates
/// the full `@@` predicate per row, so an over-approximate candidate
/// set is safe.
pub(crate) fn try_gin_seek<'a>(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    catalog: &'a Catalog,
    table: &'a Table,
    table_alias: &str,
    ctx: &eval::EvalContext<'_>,
    snapshot: &spg_storage::snapshot::Snapshot,
) -> Option<Vec<Cow<'a, Row<'static>>>> {
    if let Expr::Binary {
        lhs,
        op: BinOp::And,
        rhs,
    } = where_expr
    {
        if let Some(rows) =
            try_gin_seek(lhs, schema_cols, catalog, table, table_alias, ctx, snapshot)
        {
            return Some(rows);
        }
        return try_gin_seek(rhs, schema_cols, catalog, table, table_alias, ctx, snapshot);
    }
    // v7.17.0 Phase 3.P0-44 — MySQL `MATCH(col1, col2) AGAINST (...)`
    // desugars into `(to_tsvector(col1) @@ q) OR (to_tsvector(col2) @@ q)`
    // in the parser. To accelerate the multi-column case, walk OR the same
    // way we walk AND: only emit a candidate set if BOTH sides can seek
    // (otherwise the OR result is unbounded and we must fall through to
    // the full scan). Candidates are union'd; the caller's WHERE re-eval
    // verifies the full predicate per row, so duplicates / supersets stay
    // semantically safe.
    if let Expr::Binary {
        lhs,
        op: BinOp::Or,
        rhs,
    } = where_expr
    {
        let left = try_gin_seek(lhs, schema_cols, catalog, table, table_alias, ctx, snapshot)?;
        let right = try_gin_seek(rhs, schema_cols, catalog, table, table_alias, ctx, snapshot)?;
        let mut out: Vec<Cow<'a, Row>> = Vec::with_capacity(left.len() + right.len());
        out.extend(left);
        out.extend(right);
        return Some(out);
    }
    let Expr::Binary {
        lhs,
        op: BinOp::TsMatch,
        rhs,
    } = where_expr
    else {
        return None;
    };
    // Either side can be the column; pgvector idiom (`vec @@ q`)
    // hits the first arm, FROM-clause-derived (`plainto_tsquery($1)
    // q ... WHERE search_vector @@ q`) the same. CROSS JOIN derived
    // tables resolve `q` to a Column too.
    let (col_pos, query) = resolve_gin_col_query(lhs, rhs, schema_cols, table_alias, ctx)
        .or_else(|| resolve_gin_col_query(rhs, lhs, schema_cols, table_alias, ctx))?;
    // v7.17.0 Phase 3.P0-44 — MySQL `FULLTEXT KEY` builds a
    // `IndexKind::GinFulltext` posting list (Phase 2.2). It shares
    // the same `gin_lookup_word` shape as the tsvector-typed GIN,
    // so the MATCH-AGAINST `@@` predicate (desugared by the parser
    // into `to_tsvector(col) @@ plainto_tsquery('term')`) routes
    // through the same candidate-set seek.
    let idx = table
        .indices()
        .iter()
        .find(|i| i.column_position == col_pos && (i.is_gin() || i.is_gin_fulltext()))?;
    let candidates = gin_query_candidates(idx, &query)?;
    let _ = catalog; // cold-tier row resolution unused in MVP; see below.
    let mut out: Vec<Cow<'a, Row>> = Vec::with_capacity(candidates.len());
    for loc in candidates {
        match loc {
            spg_storage::RowLocator::Hot(i) => {
                // Phase C.3 step 2c — MVCC read gate. No-op today.
                if !table.is_row_visible(i, snapshot) {
                    continue;
                }
                if let Some(row) = table.rows().get(i) {
                    out.push(Cow::Borrowed(row));
                }
            }
            // GIN cold-tier rows in the MVP: skipped, matching the
            // full-scan `@@` path which itself only iterates
            // `table.rows()` (hot tier). When v7.13+ adds cold-tier
            // scan-time materialisation for `@@`, the parallel
            // resolution lands here; until then both paths see the
            // same hot-only candidate set so correctness is preserved.
            spg_storage::RowLocator::Cold { .. } => {}
        }
    }
    Some(out)
}

/// v7.37.8(sentori Epic 5 P2)— JSONB-GIN candidate seek for
/// `WHERE col @> <jsonb_literal>`. Mirrors `try_gin_seek`'s
/// AND walker(individual GIN seeks union safely under AND)but
/// drops OR — a non-overlapping containment query on one branch
/// vs another would broaden the candidate set unsafely without
/// the OR's full-result containment requirement that's already
/// checked per row downstream.
pub(crate) fn try_gin_jsonb_seek<'a>(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    table: &'a Table,
    table_alias: &str,
    snapshot: &spg_storage::snapshot::Snapshot,
) -> Option<Vec<Cow<'a, Row<'static>>>> {
    if let Expr::Binary {
        lhs,
        op: BinOp::And,
        rhs,
    } = where_expr
    {
        if let Some(rows) = try_gin_jsonb_seek(lhs, schema_cols, table, table_alias, snapshot) {
            return Some(rows);
        }
        return try_gin_jsonb_seek(rhs, schema_cols, table, table_alias, snapshot);
    }
    let Expr::Binary {
        lhs,
        op: BinOp::JsonContains,
        rhs,
    } = where_expr
    else {
        return None;
    };
    // Column on the left, jsonb literal on the right — sentori's
    // shape. Resolve column ↔ literal generally so `<lit> @> <col>`
    // (PG also accepts) lands here too(swap drops the seek because
    // `lit @> col` requires the literal to contain a column-defined
    // value, which can't be answered by a constant token lookup).
    let col_pos = resolve_jsonb_column(lhs, schema_cols, table_alias)?;
    let literal = resolve_jsonb_literal(rhs)?;
    let idx = table
        .indices()
        .iter()
        .find(|i| i.column_position == col_pos && i.is_gin_jsonb())?;
    let tokens = spg_storage::jsonb_gin::extract_tokens(&literal);
    if tokens.is_empty() {
        // An empty token list — `'{}' @> '{}'` — is trivially true
        // for every row. Best to let the full scan handle it so the
        // existing eval path renders the empty-containment answer.
        return None;
    }
    // Intersect posting lists.
    let mut candidates: Vec<spg_storage::RowLocator> = idx.gin_jsonb_lookup(&tokens[0]).to_vec();
    candidates.sort_by_key(locator_sort_key);
    candidates.dedup_by_key(|l| locator_sort_key(l));
    for tok in &tokens[1..] {
        let mut next: Vec<spg_storage::RowLocator> = idx.gin_jsonb_lookup(tok).to_vec();
        next.sort_by_key(locator_sort_key);
        next.dedup_by_key(|l| locator_sort_key(l));
        // Sorted-merge intersection.
        let mut out: Vec<spg_storage::RowLocator> = Vec::new();
        let (mut i, mut j) = (0usize, 0usize);
        while i < candidates.len() && j < next.len() {
            let lk = locator_sort_key(&candidates[i]);
            let rk = locator_sort_key(&next[j]);
            match lk.cmp(&rk) {
                core::cmp::Ordering::Less => i += 1,
                core::cmp::Ordering::Greater => j += 1,
                core::cmp::Ordering::Equal => {
                    out.push(candidates[i]);
                    i += 1;
                    j += 1;
                }
            }
        }
        candidates = out;
        if candidates.is_empty() {
            break;
        }
    }
    let mut out: Vec<Cow<'a, Row>> = Vec::with_capacity(candidates.len());
    for loc in candidates {
        // Phase C.3 step 2c — MVCC read gate on the Hot arm. No-op today.
        if let spg_storage::RowLocator::Hot(i) = loc
            && table.is_row_visible(i, snapshot)
            && let Some(row) = table.rows().get(i)
        {
            out.push(Cow::Borrowed(row));
        }
    }
    Some(out)
}

fn resolve_jsonb_column(
    e: &Expr,
    schema_cols: &[ColumnSchema],
    table_alias: &str,
) -> Option<usize> {
    if let Expr::Column(c) = e {
        if let Some(q) = &c.qualifier
            && !q.eq_ignore_ascii_case(table_alias)
        {
            return None;
        }
        let pos = schema_cols
            .iter()
            .position(|s| s.name.eq_ignore_ascii_case(&c.name))?;
        if matches!(
            schema_cols[pos].ty,
            spg_storage::DataType::Json | spg_storage::DataType::Jsonb
        ) {
            return Some(pos);
        }
    }
    None
}

fn resolve_jsonb_literal(e: &Expr) -> Option<alloc::string::String> {
    use spg_sql::ast::Literal;
    match e {
        // `'{"team":"ios"}'::jsonb` is the canonical sentori shape.
        // The parser surfaces it as `Cast { expr: Literal(String),
        // target: Jsonb }`. We accept any cast target since the
        // engine's full `@>` re-eval guards correctness; here we
        // only need the string body so the JSONB tokenizer can run.
        Expr::Cast { expr, .. } => match expr.as_ref() {
            Expr::Literal(Literal::String(s)) => Some(s.clone()),
            _ => None,
        },
        // Bare `Literal::String` works too — `WHERE col @> '{}'`
        // without a cast still names a JSONB-shape literal because
        // the operator's left side is JSONB-typed.
        Expr::Literal(Literal::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// v7.15.0 — trigram-GIN-accelerated candidate seek for
/// `WHERE col LIKE '<pat>'` and `WHERE col ILIKE '<pat>'` when
/// the column has a `gin_trgm_ops` GIN index.
///
/// Walks top-level `AND` so multi-predicate WHEREs (`col LIKE
/// 'foo%' AND id > 1`) still hit the trigram index; the caller
/// re-evaluates the full WHERE per candidate row, so dropping
/// non-LIKE conjuncts here stays semantically correct.
///
/// Returns `None` when:
///   - no leaf is `col LIKE/ILIKE <literal>` on a trigram-GIN-
///     indexed column;
///   - the pattern's literal runs are too short to constrain
///     (pattern decomposes into `< 3`-char runs, e.g. `%ab%`);
///   - the pattern doesn't const-evaluate to a TEXT.
pub(crate) fn try_trgm_seek<'a>(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    table: &'a Table,
    table_alias: &str,
    snapshot: &spg_storage::snapshot::Snapshot,
) -> Option<Vec<Cow<'a, Row<'static>>>> {
    if let Expr::Binary {
        lhs,
        op: BinOp::And,
        rhs,
    } = where_expr
    {
        if let Some(rows) = try_trgm_seek(lhs, schema_cols, table, table_alias, snapshot) {
            return Some(rows);
        }
        return try_trgm_seek(rhs, schema_cols, table, table_alias, snapshot);
    }
    // LIKE node is what carries the column reference + pattern.
    // ILIKE is the same AST node — PG's LIKE/ILIKE both lower
    // through `Expr::Like { expr, pattern, negated }`. The trigram
    // index posting-list keys are already lower-cased and
    // case-folded, so we only need the pattern's literal runs.
    let Expr::Like { expr, pattern, .. } = where_expr else {
        return None;
    };
    // Column side.
    let Expr::Column(c) = expr.as_ref() else {
        return None;
    };
    if let Some(q) = &c.qualifier
        && q != table_alias
    {
        return None;
    }
    let col_pos = schema_cols
        .iter()
        .position(|s| s.name.eq_ignore_ascii_case(&c.name))?;
    // Index must exist on that column AND be a trigram-GIN.
    let idx = table
        .indices()
        .iter()
        .find(|i| i.column_position == col_pos && i.is_gin_trgm())?;
    // Pattern side must be a literal TEXT — anything else (column
    // ref, function call, parameter that hasn't been bound yet)
    // falls through to full scan.
    let Expr::Literal(spg_sql::ast::Literal::String(pat)) = pattern.as_ref() else {
        return None;
    };
    let trigrams = spg_storage::trgm::trigrams_from_like_pattern(pat)?;
    // Intersect every trigram's posting list. Empty intersection
    // → empty candidate set (caller short-circuits its row loop).
    let mut iter = trigrams.iter();
    let first = iter.next()?;
    let mut acc: Vec<spg_storage::RowLocator> = {
        let mut v = idx.gin_trgm_lookup(first).to_vec();
        v.sort_by_key(locator_sort_key);
        v.dedup_by_key(|l| locator_sort_key(l));
        v
    };
    for tri in iter {
        let mut next: Vec<spg_storage::RowLocator> = idx.gin_trgm_lookup(tri).to_vec();
        next.sort_by_key(locator_sort_key);
        next.dedup_by_key(|l| locator_sort_key(l));
        // Sorted-merge intersection.
        let mut merged: Vec<spg_storage::RowLocator> =
            Vec::with_capacity(acc.len().min(next.len()));
        let (mut i, mut j) = (0usize, 0usize);
        while i < acc.len() && j < next.len() {
            let lk = locator_sort_key(&acc[i]);
            let rk = locator_sort_key(&next[j]);
            match lk.cmp(&rk) {
                core::cmp::Ordering::Less => i += 1,
                core::cmp::Ordering::Greater => j += 1,
                core::cmp::Ordering::Equal => {
                    merged.push(acc[i]);
                    i += 1;
                    j += 1;
                }
            }
        }
        acc = merged;
        if acc.is_empty() {
            break;
        }
    }
    let mut out: Vec<Cow<'a, Row>> = Vec::with_capacity(acc.len());
    for loc in acc {
        // Phase C.3 step 2c — MVCC read gate on the Hot arm. No-op today.
        if let spg_storage::RowLocator::Hot(i) = loc
            && table.is_row_visible(i, snapshot)
            && let Some(row) = table.rows().get(i)
        {
            out.push(Cow::Borrowed(row));
        }
        // Cold-tier rows: skipped in MVP (same as try_gin_seek).
    }
    Some(out)
}

/// v7.12.3 — extract `(column_position, TsQueryAst)` when one side of
/// the binary is a column reference to a GIN-indexed tsvector column
/// and the other side const-evaluates to a `Value::TsQuery`. Returns
/// `None` if the column reference is for the wrong table alias, or if
/// the RHS expression depends on row data.
pub(crate) fn resolve_gin_col_query(
    col_side: &Expr,
    query_side: &Expr,
    schema_cols: &[ColumnSchema],
    table_alias: &str,
    ctx: &eval::EvalContext<'_>,
) -> Option<(usize, spg_storage::TsQueryAst)> {
    // v7.17.0 Phase 3.P0-44 — the MATCH AGAINST desugar wraps the
    // column in `to_tsvector('simple', col)`, so we peel that wrapper
    // before the column lookup. Direct `col @@ tsquery` paths (the
    // tsvector-typed v7.12 surface) skip the wrapper entirely.
    let column = match col_side {
        Expr::Column(c) => c,
        Expr::FunctionCall { name, args }
            if name.eq_ignore_ascii_case("to_tsvector") && !args.is_empty() =>
        {
            // PG `to_tsvector` accepts either `to_tsvector(col)` or
            // `to_tsvector(config, col)`. In both shapes the column
            // we care about is the final argument.
            if let Expr::Column(c) = args.last().unwrap() {
                c
            } else {
                return None;
            }
        }
        _ => return None,
    };
    let c = column;
    if let Some(q) = &c.qualifier
        && q != table_alias
    {
        return None;
    }
    let pos = schema_cols.iter().position(|s| s.name == c.name)?;
    // Const-evaluate the query side with an empty row — fails fast
    // (with a `ColumnNotFound` / similar) if the expression actually
    // depends on row data, which is exactly the bail signal we want.
    let empty_row = Row::new(Vec::new());
    let v = eval::eval_expr(query_side, &empty_row, ctx).ok()?;
    let Value::TsQuery(q) = v else { return None };
    Some((pos, q))
}

/// v7.12.3 — walk a `TsQueryAst` against an [`IndexKind::Gin`] index
/// to produce a candidate row-locator set. Returns `None` for query
/// shapes the MVP doesn't accelerate (`Not` / `Phrase` — both bail to
/// full scan since their semantics need either complementation across
/// the whole row set or positional verification beyond what the
/// posting list carries).
///
/// Candidate sets are over-approximate — the caller re-applies the
/// full `@@` predicate per row, so reporting "row was in some
/// posting list" without verifying positions / weights stays correct.
pub(crate) fn gin_query_candidates(
    idx: &spg_storage::Index,
    query: &spg_storage::TsQueryAst,
) -> Option<Vec<spg_storage::RowLocator>> {
    use spg_storage::TsQueryAst;
    match query {
        TsQueryAst::Term { word, .. } => {
            let mut v: Vec<spg_storage::RowLocator> = idx.gin_lookup_word(word).to_vec();
            v.sort_by_key(locator_sort_key);
            v.dedup_by_key(|l| locator_sort_key(l));
            Some(v)
        }
        TsQueryAst::And(l, r) => {
            let mut left = gin_query_candidates(idx, l)?;
            let mut right = gin_query_candidates(idx, r)?;
            left.sort_by_key(locator_sort_key);
            right.sort_by_key(locator_sort_key);
            // Sorted-merge intersection.
            let mut out: Vec<spg_storage::RowLocator> = Vec::new();
            let (mut i, mut j) = (0usize, 0usize);
            while i < left.len() && j < right.len() {
                let lk = locator_sort_key(&left[i]);
                let rk = locator_sort_key(&right[j]);
                match lk.cmp(&rk) {
                    core::cmp::Ordering::Less => i += 1,
                    core::cmp::Ordering::Greater => j += 1,
                    core::cmp::Ordering::Equal => {
                        out.push(left[i]);
                        i += 1;
                        j += 1;
                    }
                }
            }
            Some(out)
        }
        TsQueryAst::Or(l, r) => {
            let mut out = gin_query_candidates(idx, l)?;
            out.extend(gin_query_candidates(idx, r)?);
            out.sort_by_key(locator_sort_key);
            out.dedup_by_key(|l| locator_sort_key(l));
            Some(out)
        }
        // Not / Phrase bail to full scan in the MVP. Not needs
        // complementation against the whole row set (not represented
        // in the posting-list view); Phrase needs positional
        // verification beyond what `word → rows` carries.
        TsQueryAst::Not(_) | TsQueryAst::Phrase { .. } => None,
    }
}

/// v7.12.3 — total ordering on `RowLocator` for sort/dedup purposes
/// inside the GIN intersection / union loops. Hot rows order by their
/// row index; Cold rows order after all Hot rows, then by
/// `(segment_id, the cold sub-key)`.
pub(crate) fn locator_sort_key(l: &spg_storage::RowLocator) -> (u8, u64, u64) {
    match *l {
        spg_storage::RowLocator::Hot(i) => (0, i as u64, 0),
        spg_storage::RowLocator::Cold {
            segment_id,
            page_offset,
        } => (1, u64::from(segment_id), u64::from(page_offset)),
    }
}

/// v5.2.3: extract `(column_position, IndexKey)` when `where_expr`
/// is a simple `col = literal` predicate suitable for a `BTree` index
/// seek. Used by `exec_update_cancel` / `exec_delete_cancel` to
/// decide whether a write touches a cold-tier row (which requires
/// promote-on-write / shadow-on-delete) before falling through to
/// the hot-tier row walk.
///
/// Returns `None` for any predicate shape the planner can't push
/// down to an index seek — complex WHERE clauses always take the
/// hot-only path (cold rows are immutable to non-indexed writes
/// until a future scan-fanout sub-version).
pub(crate) fn try_pk_predicate(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    table_alias: &str,
) -> Option<(usize, IndexKey)> {
    let Expr::Binary {
        lhs,
        op: BinOp::Eq,
        rhs,
    } = where_expr
    else {
        return None;
    };
    let (col_pos, value) = resolve_col_literal_pair(lhs, rhs, schema_cols, table_alias)
        .or_else(|| resolve_col_literal_pair(rhs, lhs, schema_cols, table_alias))?;
    let key = IndexKey::from_value(&value)?;
    Some((col_pos, key))
}

pub(crate) fn resolve_col_literal_pair(
    col_side: &Expr,
    lit_side: &Expr,
    schema_cols: &[ColumnSchema],
    table_alias: &str,
) -> Option<(usize, Value<'static>)> {
    let Expr::Column(c) = col_side else {
        return None;
    };
    if let Some(q) = &c.qualifier
        && q != table_alias
    {
        return None;
    }
    let pos = schema_cols.iter().position(|s| s.name == c.name)?;
    let Expr::Literal(l) = lit_side else {
        return None;
    };
    let v = match l {
        Literal::Integer(n) => {
            if let Ok(small) = i32::try_from(*n) {
                Value::Int(small)
            } else {
                Value::BigInt(*n)
            }
        }
        Literal::Float(x) => Value::Float(*x),
        Literal::Numeric { unscaled, scale } => Value::Numeric { scaled: *unscaled, scale: *scale , kind: spg_storage::NumericKind::Finite },
        Literal::String(s) => Value::text(s.clone()),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Null => Value::Null,
        // Vector, array and Interval literals can't be used as B-tree
        // index keys. Tell the planner to fall back to full-scan.
        Literal::Vector(_)
        | Literal::Interval { .. }
        | Literal::TextArray(_)
        | Literal::IntArray(_)
        | Literal::BigIntArray(_) => return None,
    };
    Some((pos, v))
}
