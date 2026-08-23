//! Index-access planning — B-tree equality seeks, PK fast paths, GIN /
//! trigram / NSW-vector lookups, and the `col = literal` helpers behind
//! them. Split out of `lib.rs` (v7.32 engine modularisation). Pure
//! planners over (WHERE/ORDER expr, schema, catalog, table): they return
//! candidate row indices/locators or `None` to fall back to a scan.

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::vec::Vec;
use core::ops::Bound;

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
            if crate::eval::predicate_is_true(&cond, "WHERE", ctx.mysql_dialect).ok()? {
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
    mysql: bool,
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
    // The B-tree walks in BYTE order. Under MySQL a text column does not
    // sort that way — `ORDER BY s LIMIT 2` over alpha/Beta/GAMMA/delta is
    // 1,2 there and this walk answered 2,3. Ordering is the one thing a
    // walk contributes, so when it is the wrong ordering there is nothing
    // left to keep.
    // v7.38.18 (S0) — a locale-collated column's tree is keyed by ICU
    // sort keys, so the walk comes out in the LOCALE's order, which is
    // the order the query asked for. That is the one thing a walk
    // contributes, and now it contributes the right one.
    if collated_column(schema_cols.get(col_pos)?, table.db_collation()).is_none()
        && !crate::collate::column_key_is_bytewise(schema_cols.get(col_pos)?, mysql)
    {
        return None;
    }
    // v7.38.1 (L12) — a composite B-tree leading on the ORDER BY column
    // walks it too: keys sort by the whole tuple, so the leading
    // component comes out in order (see `Index::iter_asc`). This keeps
    // the walk on tables whose only index on the column IS the
    // converted composite (a multi-column PK's leading column).
    let index = table
        .index_on(col_pos)
        .filter(|i| matches!(i.kind, spg_storage::IndexKind::BTree(_)))
        .or_else(|| {
            table.indices().iter().find(|i| {
                matches!(i.kind, spg_storage::IndexKind::BTreeMulti(_))
                    && i.column_position == col_pos
                    && i.expression.is_none()
                    && i.partial_predicate.is_none()
            })
        })?;
    // r1020 — a NULL key is not in the btree, so walking the btree cannot
    // see the rows that carry one. That is a silent wrong answer, in both
    // directions, and it shipped:
    //
    //   ORDER BY k DESC LIMIT 3   PG: NULL NULL 30    us: 30 20 10
    //   ORDER BY k ASC  LIMIT 5   PG: 10 20 30 NULL NULL
    //                             us: 10 20 30          (two rows dropped)
    //
    // DESC returns the wrong rows because PG orders NULLS FIRST there;
    // ASC returns too few because the walk runs out of indexed rows and
    // has nothing to fall back on. The unbounded ORDER BY is unaffected —
    // it sorts, and the sort sees every row.
    //
    // A NOT NULL column cannot carry one, so the fast path is exact there
    // and keeps its win. A nullable column falls back to the sort. That is
    // conservative: most such queries have no NULLs at all, and reclaiming
    // them means the walker learning to emit NULL-keyed rows at the right
    // end, which is a change to what it walks rather than a guard on when.
    // DESC is unrecoverable here: PG orders NULLS FIRST, so a NULL-keyed
    // row belongs at the very front and the walk would have to know about
    // it before emitting anything. ASC is recoverable, because NULLs belong
    // last — see the short-walk check after the loop.
    let key_nullable = schema_cols
        .get(col_pos)
        .is_none_or(|c: &ColumnSchema| c.nullable);
    if key_nullable && order.desc {
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
    let walker: Box<dyn Iterator<Item = (&spg_storage::IndexKey, &spg_storage::PostingList)>> =
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
                if !crate::eval::predicate_is_true(&cond, "WHERE", ctx.mysql_dialect).ok()? {
                    continue;
                }
            } else if let Some(w) = where_expr {
                let cond = engine
                    .eval_expr_with_correlated(w, row_cow.as_ref(), &ctx, cancel, Some(&mut memo))
                    .ok()?;
                if !crate::eval::predicate_is_true(&cond, "WHERE", ctx.mysql_dialect).ok()? {
                    continue;
                }
            }
            kept.push(row_cow);
            if kept.len() >= want {
                return Some(kept);
            }
        }
    }
    // r1020 — the walk ran out of INDEXED rows before filling the request.
    // On a nullable key that is exactly the case where NULL-keyed rows —
    // which no btree holds — would have completed it, and PG places them
    // last under ASC. Returning what we have would be a short answer, so
    // hand the query back to the sort, which sees every row.
    //
    // A NOT NULL key cannot be short for that reason, so it keeps its
    // result. On a nullable key this costs the walk when the limit was not
    // satisfied anyway, and keeps the fast path for every request the
    // indexed rows do satisfy — which is the shape that wins.
    if key_nullable && kept.len() < want {
        return None;
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
    mysql: bool,
) -> Result<QueryResult, EngineError> {
    let ctx = EvalContext::new(schema_cols, Some(table_alias));
    let projection = // A free function with no engine to ask, so a user function
    // projected here still falls back to text. Reaching it needs both an
    // indexed equality predicate and a UDF in the projection; that shape
    // goes through the ordinary select path.
    build_projection(&stmt.items, schema_cols, table_alias, mysql, None)?;
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
        .map(|p| p.to_column_schema())
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
    snapshot: &spg_storage::snapshot::Snapshot,
    mysql: bool,
) -> Option<Vec<usize>> {
    // v7.38.18 (S2) — the collation an undeclared text column is
    // compared under, and so the one its index keys under.
    let db_coll = table.db_collation();
    // v7.37.16 — two-sided range (BETWEEN) position seek, mirroring the
    // SELECT path's try_range_seek (same parse, same rows/4 selectivity
    // cap so a wide range still seq-scans). Must run BEFORE the AND
    // recursion: a BETWEEN desugars to `col>=a AND col<=b`, whose halves
    // alone are one-sided and would fall through to a full scan. The
    // caller re-applies the full WHERE per candidate and sorts, exactly
    // as for the equality seek. A cold locator falls through to the
    // legacy paths (the mutation walk operates on hot rows only).
    // v7.39 (round 461 / round 490) — the cap's budget is counted in rows a
    // caller will actually look at.
    //
    // The cap exists so an index walk never costs more than the scan it
    // replaces: return more than a quarter of the table and the seek is
    // refused. But the index holds one locator per row VERSION, and a
    // churned table's index still carries the dead ones — so the count being
    // compared was inflated by exactly the rows the caller then discards.
    //
    // Measured (round 460): delete-and-reinsert 1000 rows on a 50k table
    // with autovacuum off, and by cycle 20 a 1000-row range returns ~22000
    // candidates against a cap of 17750, so the seek is refused and every
    // DELETE becomes a 71000-row scan. Round 461 bought that back by adding
    // `dead_rows()` to the budget, which stopped the refusal but still
    // carried every dead version through a `Vec`, a sort, and a per-
    // candidate visibility test — 61000 of them by cycle 60 (round 490).
    //
    // The dead versions are now dropped INSIDE the walk, by the same
    // `is_row_visible` test the caller applies, so the budget is back to its
    // original meaning and round 461's compensation is unnecessary.
    //
    // r1038 — EXACT, not permissive, and the AND recursion below is why it
    // can be. A range sitting beside a conjunct that is not one — `IS NOT
    // NULL AND scheduled_at <= X`, the shape mailrs reported — reaches the
    // walk through that recursion, which retries each conjunct on its own
    // and finds a one-sided range there. Taking it HERE instead would mean
    // `WHERE id = 1 AND created_at > $1` walks a quarter of `created_at`
    // rather than seeking the one row `id` names: a range that merely
    // passes the cap would outrank an equality that returns a single row.
    let seek_cap = table.rows().len() / 4;
    if let Some((col_pos, lo, hi)) =
        parse_range_bounds_exact(where_expr, schema_cols, table_alias, mysql, db_coll)
    {
        // `break 'range` = this range is not usable; fall through to the
        // recursion and the equality paths below.
        'range: {
            let keep = |l: spg_storage::RowLocator| match l {
                spg_storage::RowLocator::Hot(i) => table.is_row_visible(i, snapshot),
                // A cold locator makes the whole seek bail below; keep
                // it so that decision is reached rather than silently
                // dropping it here.
                spg_storage::RowLocator::Cold { .. } => true,
            };
            // v7.38.19 — see the rows variant: a composite index's
            // leading column answers a range too.
            let walked = match table.index_on(col_pos) {
                Some(idx) => {
                    idx.lookup_range_capped_by(bound_as_ref(&lo), bound_as_ref(&hi), seek_cap, keep)
                }
                None => try_leading_composite_range(
                    table,
                    schema_cols,
                    col_pos,
                    bound_as_ref(&lo),
                    bound_as_ref(&hi),
                    seek_cap,
                    db_coll,
                    keep,
                ),
            };
            let Some(locators) = walked else {
                break 'range;
            };
            let mut out = Vec::with_capacity(locators.len());
            let mut all_hot = true;
            for loc in &locators {
                match *loc {
                    spg_storage::RowLocator::Hot(i) => out.push(i),
                    spg_storage::RowLocator::Cold { .. } => {
                        all_hot = false;
                        break;
                    }
                }
            }
            if all_hot {
                // v7.39 (pg_stat knife B) — one index scan.
                table.note_index_scan(out.len() as u64);
                return Some(out);
            }
        }
    }
    if let Expr::Binary {
        lhs,
        op: BinOp::And,
        rhs,
    } = where_expr
    {
        // 7.38.1 S7 — same best-equality choice as try_index_seek:
        // probe every indexable Eq conjunct, seek the narrowest.
        let mut conjuncts: Vec<&Expr> = Vec::new();
        fn flatten_and<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
            if let Expr::Binary {
                lhs,
                op: BinOp::And,
                rhs,
            } = e
            {
                flatten_and(lhs, out);
                flatten_and(rhs, out);
            } else {
                out.push(e);
            }
        }
        flatten_and(where_expr, &mut conjuncts);
        let mut best: Option<(usize, &Expr)> = None;
        let mut eq_cols: Vec<usize> = Vec::new();
        // v7.38.1 (L12) — keep every equality's probe key so composite
        // indexes can compose them below.
        let mut eq_keys: Vec<(usize, IndexKey)> = Vec::new();
        for c in &conjuncts {
            if let Expr::Binary {
                lhs: cl,
                op: BinOp::Eq,
                rhs: cr,
            } = c
                && let Some((col_pos, value)) =
                    resolve_col_literal_pair(cl, cr, schema_cols, table_alias)
                        .or_else(|| resolve_col_literal_pair(cr, cl, schema_cols, table_alias))
                && let Some(key) = probe_key(schema_cols, col_pos, &value, mysql, db_coll)
            {
                if !eq_cols.contains(&col_pos) {
                    eq_keys.push((col_pos, key.clone()));
                }
                eq_cols.push(col_pos);
                if let Some(idx) = table.index_on(col_pos) {
                    let n = idx.lookup_eq(&key).len();
                    if best.is_none_or(|(bn, _)| n < bn) {
                        best = Some((n, c));
                    }
                }
            }
        }
        // v7.38.1 (L12) — composite candidates: for each multi-column
        // B-tree, compose the longest prefix of its column tuple out of
        // the equality keys. A full cover is a point lookup; a partial
        // cover is one descent plus a bounded prefix walk. Either
        // competes on materialised row count like everything else, and
        // the winner is still only a CANDIDATE set — the caller
        // re-evaluates the whole WHERE per row.
        let mut best_multi: Option<(usize, Vec<spg_storage::RowLocator>)> = None;
        for idx in table.indices() {
            if !matches!(idx.kind, spg_storage::IndexKind::BTreeMulti(_))
                || idx.partial_predicate.is_some()
                || idx.expression.is_some()
            {
                continue;
            }
            let mut prefix: Vec<IndexKey> = Vec::new();
            let mut cut_by_collation = false;
            for pos in core::iter::once(idx.column_position)
                .chain(idx.extra_column_positions.iter().copied())
            {
                // v7.38.18 (G3) — a component whose column collates by a
                // locale stops the prefix HERE, rather than
                // disqualifying the whole index.
                //
                // A composite tree holds tuples of raw cells, built by
                // storage, while `probe_key` encodes such a column's
                // probe as an ICU sort key: two spaces, and the seek
                // looks in the wrong one. This version first declined
                // the index outright, which cost a full scan for `WHERE
                // id = 7 AND s = 'row7'` when `id` alone narrows it to
                // one row. Seeking the components that CAN be probed and
                // letting the caller re-check the rest is what
                // PostgreSQL does with a component it cannot use.
                if schema_cols
                    .get(pos)
                    .is_some_and(|c| collated_column(c, db_coll).is_some())
                {
                    cut_by_collation = true;
                    break;
                }
                match eq_keys.iter().find(|(c, _)| *c == pos) {
                    Some((_, k)) => prefix.push(k.clone()),
                    None => break,
                }
            }
            if prefix.is_empty() {
                continue;
            }
            // A full-tuple equality is the same contract as a plain
            // `lookup_eq` — precise, so it takes no cap. The prefix walk
            // caps like the range walk (never materialise more than the
            // competition or a quarter of the table), with a small floor
            // so tiny tables still seek (rows/4 of a 1-row table is 0).
            // A prefix cut short by a collated component is never the
            // whole tuple, however many components it holds.
            let locs = if !cut_by_collation && prefix.len() == 1 + idx.extra_column_positions.len()
            {
                Some(idx.lookup_eq_multi(&prefix).to_vec())
            } else {
                let multi_cap = best
                    .map(|(n, _)| n)
                    .unwrap_or(table.rows().len() / 4)
                    .min(table.rows().len() / 4)
                    .min(
                        best_multi
                            .as_ref()
                            .map_or(usize::MAX, |(n, _)| n.saturating_sub(1)),
                    )
                    .max(64);
                idx.lookup_prefix_capped_by(&prefix, multi_cap, |_| true)
            };
            if let Some(locs) = locs
                && best_multi.as_ref().is_none_or(|(bn, _)| locs.len() < *bn)
            {
                best_multi = Some((locs.len(), locs));
            }
        }
        // 7.38.1 S7 (round two) — RANGE candidates compete too: merge
        // one-sided bounds per column across the conjuncts (a non-range
        // leaf simply contributes nothing) and count each through the
        // capped range walk. TPC-C's stock_level / delivery shapes are
        // `w_id = 1 AND d_id = ? AND o_id BETWEEN a AND b`: the o_id
        // range names ~200 rows while the best equality (d_id) names
        // 30k — exact-only range parsing never saw it next to the
        // equalities.
        let mut range_bounds: Vec<(usize, Bound<IndexKey>, Bound<IndexKey>)> = Vec::new();
        for c in &conjuncts {
            let _ = collect_range_bounds(
                c,
                schema_cols,
                table_alias,
                &mut range_bounds,
                mysql,
                db_coll,
            );
        }
        let cap = best
            .map(|(n, _)| n)
            .unwrap_or(table.rows().len() / 4)
            .min(table.rows().len() / 4);
        let mut best_range: Option<(usize, usize, Bound<IndexKey>, Bound<IndexKey>)> = None;
        for (col_pos, lo, hi) in range_bounds {
            if matches!((&lo, &hi), (Bound::Unbounded, Bound::Unbounded)) {
                continue;
            }
            // An equality on the same column already competed with its
            // O(1) lookup_eq count; counting its degenerate range twin
            // would WALK up to `cap` locators per query — measured as a
            // 34.5 -> 27.6 tps tpcc regression before this guard.
            if eq_cols.contains(&col_pos) {
                continue;
            }
            let Some(idx) = table.index_on(col_pos) else {
                continue;
            };
            let Some(locs) =
                idx.lookup_range_capped_by(bound_as_ref(&lo), bound_as_ref(&hi), cap, |_| true)
            else {
                continue;
            };
            let n = locs.len();
            if best.is_none_or(|(bn, _)| n < bn)
                && best_range.as_ref().is_none_or(|(bn, ..)| n < *bn)
            {
                best_range = Some((n, col_pos, lo, hi));
            }
        }
        // v7.38.1 (L12) — the composite wins when it names the fewest
        // rows. `<=` on purpose: at equal counts one descent over the
        // whole tuple beats a single-column lookup that the caller
        // then has to re-filter.
        if let Some((n, locs)) = &best_multi
            && best.is_none_or(|(bn, _)| *n <= bn)
            && best_range.as_ref().is_none_or(|(bn, ..)| *n <= *bn)
        {
            let mut out = Vec::with_capacity(locs.len());
            let mut all_hot = true;
            for loc in locs {
                match *loc {
                    spg_storage::RowLocator::Hot(i) => {
                        if table.is_row_visible(i, snapshot) {
                            out.push(i);
                        }
                    }
                    spg_storage::RowLocator::Cold { .. } => {
                        all_hot = false;
                        break;
                    }
                }
            }
            if all_hot {
                out.sort_unstable();
                table.note_index_scan(out.len() as u64);
                return Some(out);
            }
        }
        if let Some((_, col_pos, lo, hi)) = best_range {
            if let Some(idx) = table.index_on(col_pos)
                && let Some(locators) = idx.lookup_range_capped_by(
                    bound_as_ref(&lo),
                    bound_as_ref(&hi),
                    table.rows().len() / 4,
                    |l| match l {
                        spg_storage::RowLocator::Hot(i) => table.is_row_visible(i, snapshot),
                        spg_storage::RowLocator::Cold { .. } => true,
                    },
                )
            {
                let mut out = Vec::with_capacity(locators.len());
                let mut all_hot = true;
                for loc in &locators {
                    match *loc {
                        spg_storage::RowLocator::Hot(i) => out.push(i),
                        spg_storage::RowLocator::Cold { .. } => {
                            all_hot = false;
                            break;
                        }
                    }
                }
                if all_hot {
                    out.sort_unstable();
                    table.note_index_scan(out.len() as u64);
                    return Some(out);
                }
            }
        }
        if let Some((_, c)) = best {
            return try_index_seek_positions(c, schema_cols, table, table_alias, snapshot, mysql);
        }
        if let Some(p) =
            try_index_seek_positions(lhs, schema_cols, table, table_alias, snapshot, mysql)
        {
            return Some(p);
        }
        return try_index_seek_positions(rhs, schema_cols, table, table_alias, snapshot, mysql);
    }
    let Expr::Binary {
        lhs,
        op: BinOp::Eq,
        rhs,
    } = where_expr
    else {
        return None;
    };
    // v7.38.16 — `lower(s) = 'x42'` has no column on either side. Until
    // this version nothing could answer it but a scan: the index that
    // names exactly that expression held the leading column's values, so
    // its keys could not match, and every lookup path said
    // `expression.is_none()` to stay away from them.
    if let Some(p) = try_expression_index_seek(lhs, rhs, table, snapshot, mysql)
        .or_else(|| try_expression_index_seek(rhs, lhs, table, snapshot, mysql))
    {
        return Some(p);
    }
    let (col_pos, value) = resolve_col_literal_pair(lhs, rhs, schema_cols, table_alias)
        .or_else(|| resolve_col_literal_pair(rhs, lhs, schema_cols, table_alias))?;
    let key = probe_key(schema_cols, col_pos, &value, mysql, db_coll)?;
    // v7.38.19 — see the rows variant: same choice, same reason.
    let composite;
    let locators: &spg_storage::posting::PostingList = match table.index_on(col_pos) {
        Some(idx) => idx.lookup_eq(&key),
        None => {
            composite = spg_storage::posting::PostingList::from(try_leading_composite_prefix(
                table,
                schema_cols,
                col_pos,
                &key,
                db_coll,
            )?);
            &composite
        }
    };
    let mut out = Vec::with_capacity(locators.len());
    for loc in locators {
        match *loc {
            // v7.39 (round 490) — dead versions are dropped here for the
            // same reason as in the range walk above: the caller tests
            // exactly this and skips.
            spg_storage::RowLocator::Hot(i) => {
                if table.is_row_visible(i, snapshot) {
                    out.push(i);
                }
            }
            spg_storage::RowLocator::Cold { .. } => return None,
        }
    }
    // v7.39 (pg_stat knife B) — one index scan.
    table.note_index_scan(out.len() as u64);
    Some(out)
}

/// v7.38.19 — the composite B-tree whose LEADING column is `col_pos`,
/// walked by that one component.
///
/// `Table::index_on` answers only with `IndexKind::BTree`, so until this
/// version a bare `WHERE lead = <lit>` saw no index at all when the only
/// one covering that column was composite — and full-scanned. The walk
/// itself is not new: the `AND` branch above has composed prefixes out of
/// several equalities since v7.38.1. What was missing is that a predicate
/// with ONE equality never entered that branch, so a one-component prefix
/// was the only prefix the engine could not take.
///
/// Measured on sentori's `events (project_id, kind)` at 200k rows:
/// `project_id = 99`, which matches nothing, cost 3.7 ms against
/// PostgreSQL 18's 0.22. Not because it read the matching rows — there
/// were none — but because it read all of them.
///
/// The result is a CANDIDATE set like every other seek here: the caller
/// re-applies the whole predicate per row.
fn try_leading_composite_prefix(
    table: &Table,
    schema_cols: &[ColumnSchema],
    col_pos: usize,
    key: &IndexKey,
    db_coll: &str,
) -> Option<Vec<spg_storage::RowLocator>> {
    // A collated leading column keys the tree in a space this probe is
    // not built in — the two-spaces problem v7.38.18 (G3) handled inside
    // the AND branch by stopping the prefix at that component. Stopping
    // at the FIRST component leaves no prefix at all, so decline and let
    // the scan answer it correctly.
    if schema_cols
        .get(col_pos)
        .is_some_and(|c| collated_column(c, db_coll).is_some())
    {
        return None;
    }
    // The same bargain the prefix walk makes everywhere: never
    // materialise more than a quarter of the table, or the seek costs
    // more than the scan it replaces. The floor keeps tiny tables
    // seekable (rows/4 of a 3-row table is 0).
    let cap = (table.rows().len() / 4).max(64);
    let mut best: Option<Vec<spg_storage::RowLocator>> = None;
    for idx in table.indices() {
        if !matches!(idx.kind, spg_storage::IndexKind::BTreeMulti(_))
            || idx.column_position != col_pos
            || idx.partial_predicate.is_some()
            || idx.expression.is_some()
        {
            continue;
        }
        if let Some(locs) = idx.lookup_prefix_capped_by(core::slice::from_ref(key), cap, |_| true)
            && best.as_ref().is_none_or(|b| locs.len() < b.len())
        {
            best = Some(locs);
        }
    }
    best
}

/// v7.38.19 — the range twin of [`try_leading_composite_prefix`].
///
/// Same defect, same shape: with only `(project_id, kind)` present,
/// `project_id > 90` read all 200,000 rows to return none of them.
fn try_leading_composite_range(
    table: &Table,
    schema_cols: &[ColumnSchema],
    col_pos: usize,
    lo: Bound<&IndexKey>,
    hi: Bound<&IndexKey>,
    cap: usize,
    db_coll: &str,
    keep: impl Fn(spg_storage::RowLocator) -> bool + Copy,
) -> Option<Vec<spg_storage::RowLocator>> {
    if schema_cols
        .get(col_pos)
        .is_some_and(|c| collated_column(c, db_coll).is_some())
    {
        return None;
    }
    let mut best: Option<Vec<spg_storage::RowLocator>> = None;
    for idx in table.indices() {
        if !matches!(idx.kind, spg_storage::IndexKind::BTreeMulti(_))
            || idx.column_position != col_pos
            || idx.partial_predicate.is_some()
            || idx.expression.is_some()
        {
            continue;
        }
        if let Some(locs) = idx.lookup_leading_range_capped_by(lo, hi, cap, keep)
            && best.as_ref().is_none_or(|b| locs.len() < b.len())
        {
            best = Some(locs);
        }
    }
    best
}

/// v7.38 (perf, index range scan) — flip a comparison operator for the
/// `literal <op> column` orientation (`5 < v` ≡ `v > 5`).
fn flip_comparison(op: BinOp) -> Option<BinOp> {
    match op {
        BinOp::Gt => Some(BinOp::Lt),
        BinOp::GtEq => Some(BinOp::LtEq),
        BinOp::Lt => Some(BinOp::Gt),
        BinOp::LtEq => Some(BinOp::GtEq),
        _ => None,
    }
}

/// Parse a single `col <op> lit` / `lit <op> col` range comparison into
/// `(col_pos, lo, hi)` where exactly one of `lo` / `hi` is bounded. Returns
/// None for anything that isn't a `> >= < <=` comparison of an indexable
/// column against a literal.
fn parse_one_sided_range(
    e: &Expr,
    schema_cols: &[ColumnSchema],
    table_alias: &str,
    mysql: bool,
    db_coll: &str,
) -> Option<(usize, Bound<IndexKey>, Bound<IndexKey>)> {
    let Expr::Binary { lhs, op, rhs } = e else {
        return None;
    };
    // `col <op> lit` keeps `op`; `lit <op> col` flips it.
    let (col_pos, value, op) =
        if let Some((p, v)) = resolve_col_literal_pair(lhs, rhs, schema_cols, table_alias) {
            (p, v, *op)
        } else if let Some((p, v)) = resolve_col_literal_pair(rhs, lhs, schema_cols, table_alias) {
            (p, v, flip_comparison(*op)?)
        } else {
            return None;
        };
    let key = probe_key(schema_cols, col_pos, &value, mysql, db_coll)?;
    let bounds = match op {
        BinOp::Gt => (Bound::Excluded(key), Bound::Unbounded),
        BinOp::GtEq => (Bound::Included(key), Bound::Unbounded),
        BinOp::Lt => (Bound::Unbounded, Bound::Excluded(key)),
        BinOp::LtEq => (Bound::Unbounded, Bound::Included(key)),
        _ => return None,
    };
    Some((col_pos, bounds.0, bounds.1))
}

/// Range bounds this predicate implies, one entry per column it constrains.
///
/// Two things changed here in r1035, both because mailrs measured them
/// (`spg-reactivation-measured-2026-08-16`).
///
/// **One-sided ranges are no longer refused.** The rule used to be
/// two-sided only, on the reasoning that `col > x` alone "is usually
/// non-selective" and an index walk would lose to a tight scan. That is a
/// guess about a distribution, and the selectivity cap in the callers is a
/// MEASUREMENT of the same thing — it refuses any walk returning more than
/// a quarter of the table. The guess was also wrong in the case that
/// reached us: `scheduled_at` is NULL for almost every row, NULLs are not
/// indexed at all (`IndexKey::from_value` has no NULL), so the index holds
/// fifty entries out of twenty thousand and a one-sided walk is as
/// selective as a walk gets. Measured before the change: matching rows
/// held at fifty while the table grew 8x, and the query grew 8.19x with
/// it — a scan, at every size.
///
/// **Conjuncts that are not ranges no longer poison the parse.** The
/// reported query is `scheduled_at IS NOT NULL AND scheduled_at <= X`, and
/// the `IS NOT NULL` half made the whole thing unparseable. A residual
/// conjunct is fine for the SEEK paths, which re-apply the full WHERE to
/// every candidate, so returning a superset is correct.
///
/// It is NOT fine for `try_range_count`, which tallies index entries
/// without looking at rows. That one uses [`parse_range_bounds_exact`],
/// which insists the whole predicate is the range.
fn parse_range_candidates(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    table_alias: &str,
    mysql: bool,
    db_coll: &str,
) -> Vec<(usize, Bound<IndexKey>, Bound<IndexKey>)> {
    let mut out = Vec::new();
    collect_range_bounds(
        where_expr,
        schema_cols,
        table_alias,
        &mut out,
        mysql,
        db_coll,
    );
    // v7.39 (enum order knife) — the index orders enum labels
    // lexicographically but PG's enum order is the catalog member order, so
    // a range walk would under-select and the caller's WHERE re-eval cannot
    // restore missing rows. Eq / IN-list seeks stay: label equality is exact.
    out.retain(|(col, _, _)| {
        schema_cols
            .get(*col)
            .is_some_and(|c| c.user_enum_type.is_none())
    });
    out
}

/// The whole predicate as a range on ONE column, or `None`.
///
/// For callers that answer from the index alone and never see the rows, so
/// a residual conjunct would make the answer wrong rather than wide.
fn parse_range_bounds_exact(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    table_alias: &str,
    mysql: bool,
    db_coll: &str,
) -> Option<(usize, Bound<IndexKey>, Bound<IndexKey>)> {
    let mut out = Vec::new();
    if !collect_range_bounds(
        where_expr,
        schema_cols,
        table_alias,
        &mut out,
        mysql,
        db_coll,
    ) {
        return None; // something in there was not a range on any column
    }
    if out.len() != 1 {
        return None; // no constraint, or constraints on several columns
    }
    let bounds = out.pop()?;
    if schema_cols
        .get(bounds.0)
        .is_some_and(|c| c.user_enum_type.is_some())
    {
        return None;
    }
    Some(bounds)
}

/// Walk an AND tree, merging every range it finds by column. Returns
/// whether EVERY leaf was a range — which is what tells an exact caller
/// that nothing is left over.
fn collect_range_bounds(
    e: &Expr,
    schema_cols: &[ColumnSchema],
    table_alias: &str,
    out: &mut Vec<(usize, Bound<IndexKey>, Bound<IndexKey>)>,
    mysql: bool,
    db_coll: &str,
) -> bool {
    if let Expr::Binary {
        lhs,
        op: BinOp::And,
        rhs,
    } = e
    {
        // Both sides, and both walked: a residual on one does not stop the
        // other from contributing a usable bound.
        let l = collect_range_bounds(lhs, schema_cols, table_alias, out, mysql, db_coll);
        let r = collect_range_bounds(rhs, schema_cols, table_alias, out, mysql, db_coll);
        return l && r;
    }
    let Some((col, lo, hi)) = parse_one_sided_range(e, schema_cols, table_alias, mysql, db_coll)
    else {
        return false;
    };
    if let Some(slot) = out.iter_mut().find(|(c, _, _)| *c == col) {
        slot.1 = tighter_lo(core::mem::replace(&mut slot.1, Bound::Unbounded), lo);
        slot.2 = tighter_hi(core::mem::replace(&mut slot.2, Bound::Unbounded), hi);
    } else {
        out.push((col, lo, hi));
    }
    true
}

/// The higher of two lower bounds; `Excluded` wins a tie, being tighter.
fn tighter_lo(a: Bound<IndexKey>, b: Bound<IndexKey>) -> Bound<IndexKey> {
    match (&a, &b) {
        (Bound::Unbounded, _) => b,
        (_, Bound::Unbounded) => a,
        (Bound::Included(x) | Bound::Excluded(x), Bound::Included(y) | Bound::Excluded(y)) => {
            match x.cmp(y) {
                core::cmp::Ordering::Greater => a,
                core::cmp::Ordering::Less => b,
                core::cmp::Ordering::Equal => {
                    if matches!(a, Bound::Excluded(_)) {
                        a
                    } else {
                        b
                    }
                }
            }
        }
    }
}

/// The lower of two upper bounds; `Excluded` wins a tie.
fn tighter_hi(a: Bound<IndexKey>, b: Bound<IndexKey>) -> Bound<IndexKey> {
    match (&a, &b) {
        (Bound::Unbounded, _) => b,
        (_, Bound::Unbounded) => a,
        (Bound::Included(x) | Bound::Excluded(x), Bound::Included(y) | Bound::Excluded(y)) => {
            match x.cmp(y) {
                core::cmp::Ordering::Less => a,
                core::cmp::Ordering::Greater => b,
                core::cmp::Ordering::Equal => {
                    if matches!(a, Bound::Excluded(_)) {
                        a
                    } else {
                        b
                    }
                }
            }
        }
    }
}

fn bound_as_ref(b: &Bound<IndexKey>) -> Bound<&IndexKey> {
    match b {
        Bound::Included(k) => Bound::Included(k),
        Bound::Excluded(k) => Bound::Excluded(k),
        Bound::Unbounded => Bound::Unbounded,
    }
}

/// v7.38 (perf, index range scan) — plan `col <op> lit` / `col BETWEEN a AND b`
/// as an index range walk. Returns the candidate rows in the key range, or None
/// (→ caller seq-scans) when there's no usable index, the shape isn't a range,
/// the range isn't selective (> half the table — the cap), or the range touches
/// a cold-tier locator (whose key we'd need to resolve the segment row and the
/// flattened walk doesn't carry). The caller re-applies the full WHERE per row,
/// so returning a superset is correct.
/// v7.39 (round 560) — rebuild the column's value from its index key.
///
/// `IndexKey::Int` holds an i64 whatever the column was declared as, so
/// the DECLARED type decides what comes back — `SELECT k` on an INT
/// column must answer int4, not int8. A type whose key is lossy in the
/// other direction (a date and a timestamp both key as Int) is not
/// reconstructible and falls back to reading the row.
fn value_from_key(
    key: &spg_storage::IndexKey,
    declared: spg_storage::DataType,
) -> Option<spg_storage::Value<'static>> {
    use spg_storage::{IndexKey as K, Value};
    Some(match (key, declared) {
        (K::Int(n), spg_storage::DataType::SmallInt) => Value::SmallInt(i16::try_from(*n).ok()?),
        (K::Int(n), spg_storage::DataType::Int) => Value::Int(i32::try_from(*n).ok()?),
        (K::Int(n), spg_storage::DataType::BigInt) => Value::BigInt(*n),
        (K::Text(t), spg_storage::DataType::Text) => Value::text(t.clone()),
        (K::Bool(b), spg_storage::DataType::Bool) => Value::Bool(*b),
        (K::Uuid(u), spg_storage::DataType::Uuid) => Value::Uuid(*u),
        _ => return None,
    })
}

/// v7.39 (round 560) — an index-only range scan.
///
/// When the projection needs nothing but the indexed column, the value
/// is already in the index KEY and the row never has to be read.
/// `try_range_seek` below throws the key away, keeps the locator and
/// fetches the row for a value the walk had in hand. Measured over
/// pgwire on a 500k table, projecting `k` for a 100k-row range:
///
/// ```text
///     PG18  Index Only Scan   3.6 ms      SPG  30 ms
/// ```
///
/// 8x, widening with the row count (2x at 1k rows).
///
/// PG needs its visibility map for this — a heap tuple carries its own
/// visibility, so an index entry alone cannot say whether the row is
/// live, and PG falls back to the heap for any page the map does not
/// mark all-visible. SPG keeps a header array beside the rows, so the
/// locator answers it directly and there is no map to be stale.
///
/// No selectivity cap: the ceiling on `try_range_seek` exists because
/// its caller materialises every candidate, which this does not do.
pub(crate) fn try_index_only_range(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    table: &Table,
    table_alias: &str,
    snapshot: &spg_storage::snapshot::Snapshot,
    projected: usize,
    mysql: bool,
) -> Option<Vec<spg_storage::Value<'static>>> {
    let mut out: Vec<spg_storage::Value<'static>> = Vec::new();
    match index_only_range_each(
        where_expr,
        schema_cols,
        table,
        table_alias,
        snapshot,
        projected,
        mysql,
        &mut |v| {
            out.push(v);
            Ok(())
        },
    ) {
        Some(Ok(_)) => Some(out),
        // The walk only errors on an index that disagrees with its own
        // schema; this caller has emitted nothing, so it can still fall
        // back to the ordinary path rather than surface it.
        Some(Err(_)) | None => None,
    }
}

/// v7.39 (round 564) — the walk itself, handing each value to a sink.
///
/// `try_index_only_range` collects into a `Vec`; the streaming caller
/// wants to emit as it goes and must not have to collect first. Both go
/// through here so the shape rules and the visibility rules have one
/// copy between them.
///
/// `None` means the shape does not apply and NOTHING has been handed to
/// the sink — the caller may still fall back. `Some(Err(_))` means rows
/// may already have gone out.
pub(crate) fn index_only_range_each(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    table: &Table,
    table_alias: &str,
    snapshot: &spg_storage::snapshot::Snapshot,
    projected: usize,
    mysql: bool,
    sink: &mut dyn FnMut(spg_storage::Value<'static>) -> Result<(), EngineError>,
) -> Option<Result<usize, EngineError>> {
    let (ty, lo, hi, idx) = index_only_precheck(
        where_expr,
        schema_cols,
        table,
        table_alias,
        projected,
        mysql,
    )?;
    let entries = idx.range_keyed(bound_as_ref(&lo), bound_as_ref(&hi))?;
    let mut headers = table.header_runs();
    let mut n = 0usize;
    for (key, loc) in entries {
        let spg_storage::RowLocator::Hot(i) = loc else {
            return Some(Err(EngineError::Unsupported(
                "index-only scan met a locator outside the hot tier".into(),
            )));
        };
        if !headers.visible(i, snapshot) {
            continue;
        }
        // Unreachable given `key_restores_type` above and keys built
        // from the column's own values — an index that disagrees with
        // its schema is worth saying so about, not walking past.
        let Some(v) = value_from_key(key, ty) else {
            return Some(Err(EngineError::Unsupported(
                "index-only scan: index key does not restore the column type".into(),
            )));
        };
        if let Err(e) = sink(v) {
            return Some(Err(e));
        }
        n += 1;
    }
    Some(Ok(n))
}

/// v7.39 (round 565) — everything decidable about this scan before it
/// walks: the shape of the predicate, the tier, the type, the index.
///
/// Split out because EXPLAIN has to answer the same question and must
/// not answer it from its own copy of the rules. Round 564 named the
/// path in the executor and left EXPLAIN calling it `Index Scan`, so a
/// reader comparing two plans that run 2x apart saw one plan.
pub(crate) fn index_only_precheck<'t>(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    table: &'t Table,
    table_alias: &str,
    projected: usize,
    mysql: bool,
) -> Option<(
    spg_storage::DataType,
    Bound<IndexKey>,
    Bound<IndexKey>,
    &'t spg_storage::Index,
)> {
    let db_coll = table.db_collation();
    let (col_pos, lo, hi) =
        parse_index_only_bounds(where_expr, schema_cols, table_alias, mysql, db_coll)?;
    if col_pos != projected {
        return None;
    }
    // A cold-tier locator carries no position to test visibility with,
    // and its value lives off-heap; anything cold falls back.
    if table.has_cold_rows_fast() {
        return None;
    }
    let ty = schema_cols[col_pos].ty;
    // Decided BEFORE the walk, not per row: a streaming caller cannot
    // take a row back once it has gone out, so "this type does not come
    // back from its key" has to be a shape rejection, not a discovery
    // made halfway through.
    if !key_restores_type(ty) {
        return None;
    }
    let idx = table.index_on(col_pos)?;
    Some((ty, lo, hi, idx))
}

/// v7.39 (round 566) — the predicates an index-only scan can serve:
/// a two-sided range, or an equality, which is the degenerate range
/// `[k, k]`.
///
/// Equality was left out when round 560 built this, and it is the
/// commoner query. On 500k rows with 50 distinct values, `WHERE g = 7`
/// returns 10k of them:
///
/// ```text
///     SPG  9.5 ms      PG18  4.5 ms      2.1x
/// ```
///
/// — the same loss the range shape carried before round 564, in the
/// shape people write more often.
///
/// The enum guard `parse_range_bounds` applies does not reach here, and
/// deliberately: it exists because the index orders labels
/// lexicographically while PG orders them by catalog position, so a
/// RANGE walk would under-select. Equality does not depend on the
/// order — its own comment in `try_index_seek_positions` says so.
fn parse_index_only_bounds(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    table_alias: &str,
    mysql: bool,
    db_coll: &str,
) -> Option<(usize, Bound<IndexKey>, Bound<IndexKey>)> {
    // Exact, not permissive: this path answers from index keys and never
    // looks at the row, so a residual conjunct would go unapplied.
    if let Some(r) = parse_range_bounds_exact(where_expr, schema_cols, table_alias, mysql, db_coll)
    {
        return Some(r);
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
    // `col = NULL` is never true, and an index that stores NULL keys
    // would happily hand rows back for it.
    if value.is_null() {
        return None;
    }
    let key = probe_key(schema_cols, col_pos, &value, mysql, db_coll)?;
    Some((col_pos, Bound::Included(key.clone()), Bound::Included(key)))
}

/// Which declared types an index key comes back as unambiguously.
///
/// The pairs `value_from_key` accepts, as a question about the TYPE
/// alone. A date and a timestamp both key as `IndexKey::Int`, so
/// neither is here.
fn key_restores_type(ty: spg_storage::DataType) -> bool {
    use spg_storage::DataType as T;
    matches!(
        ty,
        T::SmallInt | T::Int | T::BigInt | T::Text | T::Bool | T::Uuid
    )
}

fn try_range_seek<'a>(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    table: &'a Table,
    table_alias: &str,
    snapshot: &spg_storage::snapshot::Snapshot,
    mysql: bool,
) -> Option<Vec<Cow<'a, Row<'static>>>> {
    let db_coll = table.db_collation();
    // r1038 — EXACT: see the note in `try_index_seek_positions`. The AND
    // recursion in `try_index_seek` retries each conjunct alone, so a
    // one-sided range beside a non-range conjunct still reaches the index
    // — without letting a merely-cap-passing range outrank an equality.
    let candidates = parse_range_bounds_exact(where_expr, schema_cols, table_alias, mysql, db_coll);
    // Selectivity cap: the caller still materialises + re-evals every candidate
    // this returns, so the index range scan only pays off when the range is a
    // small fraction of the table. Empirically ~50%-selective ranges regress
    // (index-walk + per-candidate materialise > a tight seq scan); a quarter is
    // a safe margin that keeps the clear wins and falls back (→ None → seq scan)
    // otherwise, so no endpoint regresses.
    //
    // r1035 — the cap is also what makes one-sided ranges safe to attempt:
    // a wide `col > x` returns more than a quarter and is refused here,
    // which is the measurement the old two-sided-only rule was guessing at.
    let cap = table.rows().len() / 4;
    let (col_pos, lo, hi) = candidates?;
    let keep = |l: spg_storage::RowLocator| match l {
        spg_storage::RowLocator::Hot(i) => table.is_row_visible(i, snapshot),
        spg_storage::RowLocator::Cold { .. } => true,
    };
    // v7.39 (round 490) — drop the dead versions inside the walk. This loop
    // already tested `is_row_visible` and skipped; doing it one level down
    // means the cap stops counting them too (see `lookup_range_capped_by`).
    // v7.38.19 — and when the only index covering this column is a
    // composite one, walk its leading component instead of scanning.
    let locators = match table.index_on(col_pos) {
        Some(idx) => idx.lookup_range_capped_by(bound_as_ref(&lo), bound_as_ref(&hi), cap, keep)?,
        None => try_leading_composite_range(
            table,
            schema_cols,
            col_pos,
            bound_as_ref(&lo),
            bound_as_ref(&hi),
            cap,
            db_coll,
            keep,
        )?,
    };
    let mut out: Vec<Cow<'a, Row>> = Vec::with_capacity(locators.len());
    for loc in &locators {
        match *loc {
            spg_storage::RowLocator::Hot(i) => {
                if let Some(row) = table.rows().get(i) {
                    out.push(Cow::Borrowed(row));
                }
            }
            // A range walk flattens locators without their per-key handle,
            // so a cold-tier row can't be resolved the way the Eq seek
            // does. Bail to a seq scan (correct — the caller re-applies
            // the WHERE to all rows).
            spg_storage::RowLocator::Cold { .. } => return None,
        }
    }
    // v7.39 (pg_stat knife B) — one index scan.
    table.note_index_scan(out.len() as u64);
    Some(out)
}

/// Whether the WHOLE predicate is a single indexed range — the BETWEEN
/// shape, and now a bare `col <= x` too.
///
/// For EXPLAIN's conjunct split. It walks the AND chain looking for one
/// conjunct that seeks on its own, and r1035 made each half of a BETWEEN
/// seekable by itself, so it started printing `Index Cond: (k <= 12)` with
/// `Filter: (k >= 10)` under it — half the predicate presented as a
/// re-check that does not happen. Both halves are one seek, and this is
/// how the split learns to say so.
pub(crate) fn whole_predicate_is_one_range(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    table: &Table,
    table_alias: &str,
    mysql: bool,
) -> bool {
    let db_coll = table.db_collation();
    parse_range_bounds_exact(where_expr, schema_cols, table_alias, mysql, db_coll)
        .is_some_and(|(col, _, _)| table.index_on(col).is_some())
}

/// How many index entries an indexed range in this predicate covers, when
/// that is cheap enough to ask.
///
/// For EXPLAIN. mailrs pointed out (2026-08-16) that a range's `rows=`
/// figure is `n / 3` whatever the data is: a fixture with 10,000 matching
/// rows and one with 50 produced byte-identical estimates and costs, and
/// `ANALYZE` moved neither. The guess is documented in `est_scan_rows`,
/// but a reader cannot tell a selective predicate from a wide one, which
/// is most of what the number is for.
///
/// The index already knows. Walking it under the same cap the executor
/// uses means EXPLAIN never costs more than the query would, and returns
/// `None` — leave the old guess in place — when the range is too wide to
/// count cheaply, which is exactly the case where the guess is closest to
/// right anyway.
///
/// The count is an UPPER BOUND: conjuncts that are not part of the range
/// still filter afterwards. That is what an estimate is, and it beats a
/// constant by the distance between 50 and 6,666.
pub(crate) fn count_indexed_range_capped(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    table: &Table,
    table_alias: &str,
    snapshot: &spg_storage::snapshot::Snapshot,
    mysql: bool,
) -> Option<u64> {
    let cap = table.rows().len() / 4;
    let db_coll = table.db_collation();
    for (col_pos, lo, hi) in
        parse_range_candidates(where_expr, schema_cols, table_alias, mysql, db_coll)
    {
        let Some(idx) = table.index_on(col_pos) else {
            continue;
        };
        if let Some(locators) =
            idx.lookup_range_capped_by(bound_as_ref(&lo), bound_as_ref(&hi), cap, |l| match l {
                spg_storage::RowLocator::Hot(i) => table.is_row_visible(i, snapshot),
                spg_storage::RowLocator::Cold { .. } => true,
            })
        {
            return Some(locators.len() as u64);
        }
    }
    None
}

/// v7.38 (perf, exact-range count) — `count(*)` over a WHERE that is EXACTLY a
/// two-sided indexed range: the visible in-range locators ARE the matching
/// rows (`parse_range_bounds` only matches a pure two-sided range on the
/// indexed column, so there's no residual predicate), so we tally them without
/// materialising a single row or re-evaluating the WHERE. Returns None (→ the
/// caller runs the general aggregate path) when the shape doesn't fit or a
/// cold-tier locator is in range (its visibility needs the resolved row). No
/// selectivity cap — counting is cheap and always ≤ a full scan.
pub(crate) fn try_range_count(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    table: &Table,
    table_alias: &str,
    snapshot: &spg_storage::snapshot::Snapshot,
    mysql: bool,
) -> Option<i64> {
    let db_coll = table.db_collation();
    // Exact: the tally IS the answer, so a residual conjunct would make it
    // wrong. See `parse_range_bounds_exact`.
    let (col_pos, lo, hi) =
        parse_range_bounds_exact(where_expr, schema_cols, table_alias, mysql, db_coll)?;
    let idx = table.index_on(col_pos)?;
    let locators = idx.lookup_range_capped(bound_as_ref(&lo), bound_as_ref(&hi), usize::MAX)?;
    let mut count: i64 = 0;
    for loc in &locators {
        match *loc {
            spg_storage::RowLocator::Hot(i) => {
                if table.is_row_visible(i, snapshot) {
                    count += 1;
                }
            }
            spg_storage::RowLocator::Cold { .. } => return None,
        }
    }
    Some(count)
}

pub(crate) fn try_index_seek<'a>(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    catalog: &'a Catalog,
    table: &'a Table,
    table_alias: &str,
    snapshot: &spg_storage::snapshot::Snapshot,
    mysql: bool,
) -> Option<Vec<Cow<'a, Row<'static>>>> {
    // v7.38.18 (S2) — the collation an undeclared text column is
    // compared under, and so the one its index keys under.
    let db_coll = table.db_collation();
    // v7.38 (perf) — a range predicate (`col BETWEEN a AND b`, `col > x`) walks
    // the index range instead of full-scanning. Tried before the `AND` recurse
    // so a two-sided BETWEEN is caught as one range; a mixed predicate like
    // `id = 1 AND created > $1` isn't a pure range, so this returns None and the
    // Eq path below still seeks on `id`.
    // r1038 — "isn't a pure range" is now a decision this function makes on
    // purpose rather than a limit of the parser: one-sided ranges became
    // seekable, so without the exactness test here the mixed predicate above
    // would take the range and leave the equality unused.
    if let Some(rows) = try_range_seek(where_expr, schema_cols, table, table_alias, snapshot, mysql)
    {
        return Some(rows);
    }
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
        // 7.38.1 S7 (tpcc decomposition) — among ALL indexable
        // equality conjuncts of the AND chain, seek through the one
        // with the FEWEST index entries, not the first one found.
        // First-hit-wins took TPC-C's leading key (c_w_id = 1 —
        // every row at scale 1, a 19.9 ms "index scan" of 30k rows)
        // when the c_id conjunct two spots over matched ten. The
        // probe is one lookup_eq per candidate — map reads, no row
        // materialisation — and the winner re-enters the ordinary
        // single-equality path below.
        let mut conjuncts: Vec<&Expr> = Vec::new();
        fn flatten_and<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
            if let Expr::Binary {
                lhs,
                op: BinOp::And,
                rhs,
            } = e
            {
                flatten_and(lhs, out);
                flatten_and(rhs, out);
            } else {
                out.push(e);
            }
        }
        flatten_and(where_expr, &mut conjuncts);
        let mut best: Option<(usize, &Expr)> = None;
        let mut eq_cols: Vec<usize> = Vec::new();
        // v7.38.1 (L12) — keep every equality's probe key so composite
        // indexes can compose them below.
        let mut eq_keys: Vec<(usize, IndexKey)> = Vec::new();
        for c in &conjuncts {
            if let Expr::Binary {
                lhs: cl,
                op: BinOp::Eq,
                rhs: cr,
            } = c
                && let Some((col_pos, value)) =
                    resolve_col_literal_pair(cl, cr, schema_cols, table_alias)
                        .or_else(|| resolve_col_literal_pair(cr, cl, schema_cols, table_alias))
                && let Some(key) = probe_key(schema_cols, col_pos, &value, mysql, db_coll)
            {
                if !eq_cols.contains(&col_pos) {
                    eq_keys.push((col_pos, key.clone()));
                }
                eq_cols.push(col_pos);
                if let Some(idx) = table.index_on(col_pos) {
                    let n = idx.lookup_eq(&key).len();
                    if best.is_none_or(|(bn, _)| n < bn) {
                        best = Some((n, c));
                    }
                }
            }
        }
        // v7.38.1 (L12) — composite candidates: for each multi-column
        // B-tree, compose the longest prefix of its column tuple out of
        // the equality keys. A full cover is a point lookup; a partial
        // cover is one descent plus a bounded prefix walk. Either
        // competes on materialised row count like everything else, and
        // the winner is still only a CANDIDATE set — the caller
        // re-evaluates the whole WHERE per row.
        let mut best_multi: Option<(usize, Vec<spg_storage::RowLocator>)> = None;
        for idx in table.indices() {
            if !matches!(idx.kind, spg_storage::IndexKind::BTreeMulti(_))
                || idx.partial_predicate.is_some()
                || idx.expression.is_some()
            {
                continue;
            }
            let mut prefix: Vec<IndexKey> = Vec::new();
            let mut cut_by_collation = false;
            for pos in core::iter::once(idx.column_position)
                .chain(idx.extra_column_positions.iter().copied())
            {
                // v7.38.18 (G3) — a component whose column collates by a
                // locale stops the prefix HERE, rather than
                // disqualifying the whole index.
                //
                // A composite tree holds tuples of raw cells, built by
                // storage, while `probe_key` encodes such a column's
                // probe as an ICU sort key: two spaces, and the seek
                // looks in the wrong one. This version first declined
                // the index outright, which cost a full scan for `WHERE
                // id = 7 AND s = 'row7'` when `id` alone narrows it to
                // one row. Seeking the components that CAN be probed and
                // letting the caller re-check the rest is what
                // PostgreSQL does with a component it cannot use.
                if schema_cols
                    .get(pos)
                    .is_some_and(|c| collated_column(c, db_coll).is_some())
                {
                    cut_by_collation = true;
                    break;
                }
                match eq_keys.iter().find(|(c, _)| *c == pos) {
                    Some((_, k)) => prefix.push(k.clone()),
                    None => break,
                }
            }
            if prefix.is_empty() {
                continue;
            }
            // A full-tuple equality is the same contract as a plain
            // `lookup_eq` — precise, so it takes no cap. The prefix walk
            // caps like the range walk (never materialise more than the
            // competition or a quarter of the table), with a small floor
            // so tiny tables still seek (rows/4 of a 1-row table is 0).
            // A prefix cut short by a collated component is never the
            // whole tuple, however many components it holds.
            let locs = if !cut_by_collation && prefix.len() == 1 + idx.extra_column_positions.len()
            {
                Some(idx.lookup_eq_multi(&prefix).to_vec())
            } else {
                let multi_cap = best
                    .map(|(n, _)| n)
                    .unwrap_or(table.rows().len() / 4)
                    .min(table.rows().len() / 4)
                    .min(
                        best_multi
                            .as_ref()
                            .map_or(usize::MAX, |(n, _)| n.saturating_sub(1)),
                    )
                    .max(64);
                idx.lookup_prefix_capped_by(&prefix, multi_cap, |_| true)
            };
            if let Some(locs) = locs
                && best_multi.as_ref().is_none_or(|(bn, _)| locs.len() < *bn)
            {
                best_multi = Some((locs.len(), locs));
            }
        }
        // 7.38.1 S7 (round two) — RANGE candidates compete with the
        // equalities on the same count (see the positions variant).
        let mut range_bounds: Vec<(usize, Bound<IndexKey>, Bound<IndexKey>)> = Vec::new();
        for c in &conjuncts {
            let _ = collect_range_bounds(
                c,
                schema_cols,
                table_alias,
                &mut range_bounds,
                mysql,
                db_coll,
            );
        }
        let cap = best
            .map(|(n, _)| n)
            .unwrap_or(table.rows().len() / 4)
            .min(table.rows().len() / 4);
        let mut best_range: Option<(usize, usize, Bound<IndexKey>, Bound<IndexKey>)> = None;
        for (col_pos, lo, hi) in range_bounds {
            if matches!((&lo, &hi), (Bound::Unbounded, Bound::Unbounded)) {
                continue;
            }
            // An equality on the same column already competed with its
            // O(1) lookup_eq count; counting its degenerate range twin
            // would WALK up to `cap` locators per query — measured as a
            // 34.5 -> 27.6 tps tpcc regression before this guard.
            if eq_cols.contains(&col_pos) {
                continue;
            }
            let Some(idx) = table.index_on(col_pos) else {
                continue;
            };
            let Some(locs) =
                idx.lookup_range_capped_by(bound_as_ref(&lo), bound_as_ref(&hi), cap, |_| true)
            else {
                continue;
            };
            let n = locs.len();
            if best.is_none_or(|(bn, _)| n < bn)
                && best_range.as_ref().is_none_or(|(bn, ..)| n < *bn)
            {
                best_range = Some((n, col_pos, lo, hi));
            }
        }
        // v7.38.1 (L12) — the composite wins when it names the fewest
        // rows (see the positions variant for the `<=` rationale).
        if let Some((n, locs)) = &best_multi
            && best.is_none_or(|(bn, _)| *n <= bn)
            && best_range.as_ref().is_none_or(|(bn, ..)| *n <= *bn)
        {
            let mut out: Vec<Cow<'a, Row>> = Vec::with_capacity(locs.len());
            let mut all_hot = true;
            for loc in locs {
                match *loc {
                    spg_storage::RowLocator::Hot(i) => {
                        if !table.is_row_visible(i, snapshot) {
                            continue;
                        }
                        if let Some(row) = table.rows().get(i) {
                            out.push(Cow::Borrowed(row));
                        }
                    }
                    spg_storage::RowLocator::Cold { .. } => {
                        all_hot = false;
                        break;
                    }
                }
            }
            if all_hot {
                table.note_index_scan(out.len() as u64);
                return Some(out);
            }
        }
        if let Some((_, col_pos, lo, hi)) = best_range
            && let Some(idx) = table.index_on(col_pos)
            && let Some(locators) = idx.lookup_range_capped_by(
                bound_as_ref(&lo),
                bound_as_ref(&hi),
                table.rows().len() / 4,
                |l| match l {
                    spg_storage::RowLocator::Hot(i) => table.is_row_visible(i, snapshot),
                    spg_storage::RowLocator::Cold { .. } => true,
                },
            )
        {
            let mut out: Vec<Cow<'a, Row>> = Vec::with_capacity(locators.len());
            let mut all_hot = true;
            for loc in &locators {
                match *loc {
                    spg_storage::RowLocator::Hot(i) => {
                        if let Some(row) = table.rows().get(i) {
                            out.push(Cow::Borrowed(row));
                        }
                    }
                    spg_storage::RowLocator::Cold { .. } => {
                        all_hot = false;
                        break;
                    }
                }
            }
            if all_hot {
                table.note_index_scan(out.len() as u64);
                return Some(out);
            }
        }
        if let Some((_, c)) = best {
            return try_index_seek(c, schema_cols, catalog, table, table_alias, snapshot, mysql);
        }
        // No equality conjunct is indexable — keep the old recursion
        // for the range / IN-list shapes hiding inside the AND.
        if let Some(rows) = try_index_seek(
            lhs,
            schema_cols,
            catalog,
            table,
            table_alias,
            snapshot,
            mysql,
        ) {
            return Some(rows);
        }
        return try_index_seek(
            rhs,
            schema_cols,
            catalog,
            table,
            table_alias,
            snapshot,
            mysql,
        );
    }
    // v7.38.19 — `a = 1 OR a = 2` is `a IN (1, 2)` written the other way,
    // and this engine has seeked the IN form since v7.33 while scanning
    // the OR one. Measured on 200k rows, the predicate matching nothing,
    // with an ORDINARY single-column index in place so nothing else was
    // in the way:
    //
    //     project_id IN (98, 99)              0.192 ms
    //     project_id = 99 OR project_id = 98  6.560 ms
    //
    // The union is only sound when EVERY disjunct seeks: a disjunct that
    // falls through to a scan contributes rows this set would then be
    // missing, and the caller re-applies the predicate to candidates
    // rather than finding more. That is the same rule the GIN OR walk
    // below states, and the reason both use `?` on every arm.
    //
    // Disjuncts need not share a column, and need not be equalities —
    // each side goes back through this function, so `a = 1 OR b > 9`
    // unions an equality seek with a range walk. What bounds the cost is
    // that each arm already refuses to return more than a quarter of the
    // table.
    if let Expr::Binary {
        lhs,
        op: BinOp::Or,
        rhs,
    } = where_expr
    {
        let left = try_index_seek(
            lhs,
            schema_cols,
            catalog,
            table,
            table_alias,
            snapshot,
            mysql,
        )?;
        let right = try_index_seek(
            rhs,
            schema_cols,
            catalog,
            table,
            table_alias,
            snapshot,
            mysql,
        )?;
        // A union wider than the scan it replaces is not a win. Each arm
        // is capped on its own, so two of them can still add up past the
        // table; refuse there rather than materialise it.
        if left.len() + right.len() > table.rows().len() {
            return None;
        }
        // A row satisfying BOTH disjuncts appears in both arms, and the
        // caller counts what it is given — so the duplicate has to go
        // here. Hot rows are borrowed out of `table.rows()`, so identity
        // is the address and the test is exact.
        //
        // A COLD row is decoded into a fresh allocation per arm, so two
        // copies of one row have two addresses and no cheap test tells
        // them apart. Rather than dedup something this cannot identify,
        // decline the union and let the scan answer it — the same
        // decision the range walk makes about cold locators.
        let mut seen: BTreeSet<usize> = BTreeSet::new();
        let mut out: Vec<Cow<'a, Row>> = Vec::with_capacity(left.len() + right.len());
        for row in left.into_iter().chain(right) {
            let Cow::Borrowed(hot) = row else {
                return None;
            };
            if seen.insert(core::ptr::from_ref::<Row>(hot) as usize) {
                out.push(Cow::Borrowed(hot));
            }
        }
        return Some(out);
    }
    // v7.33 (mailrs 7.33.0) — `indexed_col IN (lit, …)` seeks each literal
    // and unions the rows (PG's bitmap index scan) instead of a full scan
    // + per-row membership test. The single-table path otherwise tested a
    // 60-element list against every row (24k × 60 string compares ~66 ms).
    if let Some(rows) = try_inlist_seek(
        where_expr,
        schema_cols,
        catalog,
        table,
        table_alias,
        snapshot,
        mysql,
    ) {
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
    // v7.38.16 — `lower(s) = 'x42'` has no column on either side. Until
    // this version nothing could answer it but a scan: the index that
    // names exactly that expression held the leading column's values, so
    // its keys could not match, and every lookup path said
    // `expression.is_none()` to stay away from them.
    if let Some(positions) = try_expression_index_seek(lhs, rhs, table, snapshot, mysql)
        .or_else(|| try_expression_index_seek(rhs, lhs, table, snapshot, mysql))
    {
        let mut out: Vec<Cow<'a, Row>> = Vec::with_capacity(positions.len());
        for i in positions {
            out.push(Cow::Borrowed(table.rows().get(i)?));
        }
        return Some(out);
    }
    let (col_pos, value) = resolve_col_literal_pair(lhs, rhs, schema_cols, table_alias)
        .or_else(|| resolve_col_literal_pair(rhs, lhs, schema_cols, table_alias))?;
    let key = probe_key(schema_cols, col_pos, &value, mysql, db_coll)?;
    // v7.38.19 — a single-column index answers directly; otherwise the
    // leading column of a composite one is walked by prefix. Note the
    // ORDER: `index_on` deliberately refuses a composite (its keys are
    // tuples, and a one-component `lookup_eq` against them answers
    // nothing while looking like "no rows matched" — the shape this
    // codebase has been bitten by twice), so the composite is asked
    // through the primitive built for tuples instead.
    let composite;
    let locators: &spg_storage::posting::PostingList = match table.index_on(col_pos) {
        Some(idx) => idx.lookup_eq(&key),
        None => {
            composite = spg_storage::posting::PostingList::from(try_leading_composite_prefix(
                table,
                schema_cols,
                col_pos,
                &key,
                db_coll,
            )?);
            &composite
        }
    };
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
    // v7.39 (pg_stat knife B) — one index scan.
    table.note_index_scan(out.len() as u64);
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
    mysql: bool,
) -> Option<Vec<Cow<'a, Row<'static>>>> {
    // v7.38.18 (S2) — the collation an undeclared text column is
    // compared under, and so the one its index keys under.
    let db_coll = table.db_collation();
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
    // v7.38.19 — a composite index's leading column answers an IN list
    // too, and for the same reason it answers an equality: the list is
    // N equalities. Measured with only `(project_id, kind)` present,
    // `project_id IN (98, 99)` — matching nothing — cost 3.578 ms
    // against PostgreSQL's 0.215. Given a single-column index the same
    // query took 0.192, which is what says the list was never the
    // problem.
    let single = table.index_on(col_pos);
    // Every element must be a literal; bail (full scan) otherwise.
    //
    // r1039 — through the SAME resolver the equality seek uses. This
    // built its key straight from `literal_to_value`, so a string
    // literal was always TEXT and `d IN ('2026-01-02')` on a DATE column
    // answered 0 rows with an index and 1 without it. Round 564 fixed
    // that decision on the equality seek and r1037 on the two JOIN
    // seeks; this was the fourth copy.
    let col = schema_cols.get(col_pos)?;
    // And the same question the equality seek asks: a byte probe cannot
    // answer a folded comparison. `s IN ('ALPHA','BETA')` is 1,2 in
    // MySQL and was nothing at all here.
    // v7.38.18 (S0) — through the same funnel as the equality probe and
    // the range bounds, so a collated column's IN list is encoded the
    // way its entries were. Building the key here with
    // `from_value_for_column` was a fourth copy of that decision and
    // would have probed a sort-key tree with raw text.
    let mut keys: Vec<IndexKey> = Vec::with_capacity(list.len());
    for e in list {
        let Expr::Literal(l) = e else {
            return None;
        };
        let v = literal_as_column_value(l, col, col_pos)?;
        keys.push(probe_key(schema_cols, col_pos, &v, mysql, db_coll)?);
    }
    let table_name = table.schema().name.as_str();
    let mut out: Vec<Cow<'a, Row>> = Vec::new();
    for key in &keys {
        let composite;
        let locators: &spg_storage::posting::PostingList =
            match single {
                Some(idx) => idx.lookup_eq(key),
                None => {
                    composite = spg_storage::posting::PostingList::from(
                        try_leading_composite_prefix(table, schema_cols, col_pos, key, db_coll)?,
                    );
                    &composite
                }
            };
        for loc in locators {
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
    // v7.39 (pg_stat knife B) — one index scan.
    table.note_index_scan(out.len() as u64);
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
    let (col_pos, query, expr_key) = resolve_gin_col_query(lhs, rhs, schema_cols, table_alias, ctx)
        .map(|(p, q)| (p, q, lhs.as_ref()))
        .or_else(|| {
            resolve_gin_col_query(rhs, lhs, schema_cols, table_alias, ctx)
                .map(|(p, q)| (p, q, rhs.as_ref()))
        })?;
    // v7.17.0 Phase 3.P0-44 — MySQL `FULLTEXT KEY` builds a
    // `IndexKind::GinFulltext` posting list (Phase 2.2). It shares
    // the same `gin_lookup_word` shape as the tsvector-typed GIN,
    // so the MATCH-AGAINST `@@` predicate (desugared by the parser
    // into `to_tsvector(col) @@ plainto_tsquery('term')`) routes
    // through the same candidate-set seek.
    // v7.38.16 — an index whose key IS this expression answers first.
    // `column_position` on such an index is only an anchor, so matching
    // on it alone would pick a full-text index built over a different
    // expression — or a different text-search configuration, which is
    // how `to_tsvector('english', body) @@ to_tsquery('english','lazy')`
    // came to return NO ROWS with an index and one row without: the
    // index held `lazy` from the `simple` tokeniser and the query asked
    // for the English stem `lazi`.
    let idx = crate::expr_index::index_for_expression(table, expr_key)
        .and_then(|name| table.indices().iter().find(|i| i.name == name))
        .or_else(|| {
            let col_pos = col_pos?;
            table.indices().iter().find(|i| {
                i.column_position == col_pos
                    && (i.is_gin() || i.is_gin_fulltext())
                    && i.expression.is_none()
            })
        })?;
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
    // v7.38.16 — count it. This seek never did, so `idx_scan` on a table
    // whose only index is a GIN read 0 whether the index answered the
    // query or nothing did — and a test could not tell those apart. The
    // B-tree seeks have counted since they were written.
    table.note_index_scan(out.len() as u64);
    Some(out)
}

/// v7.37.8(sentori Epic 5 P2)— JSONB-GIN candidate seek for
/// `WHERE col @> <jsonb_literal>`. Mirrors `try_gin_seek`'s
/// AND walker(individual GIN seeks union safely under AND)but
/// drops OR — a non-overlapping containment query on one branch
/// vs another would broaden the candidate set unsafely without
/// the OR's full-result containment requirement that's already
/// checked per row downstream.
/// v7.38.12 — is slot `i` in a range the BRIN summary could not rule
/// out? `None` means no summary had an opinion, and then every slot is
/// kept.
fn slot_kept(slots: Option<&[core::ops::Range<usize>]>, i: usize) -> bool {
    slots.is_none_or(|rs| rs.iter().any(|r| r.contains(&i)))
}

pub(crate) fn try_gin_jsonb_seek<'a>(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    table: &'a Table,
    table_alias: &str,
    snapshot: &spg_storage::snapshot::Snapshot,
) -> Option<Vec<Cow<'a, Row<'static>>>> {
    // v7.38.12 — the two indexes meet here.
    //
    // A GIN containment seek hands back row locators, and this is the
    // only moment they exist before the rows are materialised. If the
    // same WHERE also bounds a BRIN-indexed column, a locator whose
    // slot the summary ruled out cannot satisfy that bound — so it is
    // dropped before the row is touched, which is PG combining two
    // index results into one bitmap, done at the locator level.
    //
    // Computed from the WHOLE predicate at the top call, not from the
    // sub-expression the recursion below descends into: the range that
    // prunes lives in a different conjunct from the containment that
    // seeks.
    let brin_slots = crate::brin::candidate_slots(where_expr, table);
    try_gin_jsonb_seek_in(
        where_expr,
        schema_cols,
        table,
        table_alias,
        snapshot,
        &brin_slots,
    )
}

fn try_gin_jsonb_seek_in<'a>(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    table: &'a Table,
    table_alias: &str,
    snapshot: &spg_storage::snapshot::Snapshot,
    brin_slots: &Option<Vec<core::ops::Range<usize>>>,
) -> Option<Vec<Cow<'a, Row<'static>>>> {
    if let Expr::Binary {
        lhs,
        op: BinOp::And,
        rhs,
    } = where_expr
    {
        if let Some(rows) =
            try_gin_jsonb_seek_in(lhs, schema_cols, table, table_alias, snapshot, brin_slots)
        {
            return Some(rows);
        }
        return try_gin_jsonb_seek_in(rhs, schema_cols, table, table_alias, snapshot, brin_slots);
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
            && slot_kept(brin_slots.as_deref(), i)
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
        let mut v = idx
            .gin_trgm_lookup(spg_storage::trgm::trigram_str(first))
            .to_vec();
        v.sort_by_key(locator_sort_key);
        v.dedup_by_key(|l| locator_sort_key(l));
        v
    };
    for tri in iter {
        let mut next: Vec<spg_storage::RowLocator> = idx
            .gin_trgm_lookup(spg_storage::trgm::trigram_str(tri))
            .to_vec();
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
) -> Option<(Option<usize>, spg_storage::TsQueryAst)> {
    // v7.17.0 Phase 3.P0-44 — the MATCH AGAINST desugar wraps the
    // column in `to_tsvector('simple', col)`, so we peel that wrapper
    // before the column lookup. Direct `col @@ tsquery` paths (the
    // tsvector-typed v7.12 surface) skip the wrapper entirely.
    let column = match col_side {
        Expr::Column(c) => Some(c),
        Expr::FunctionCall { name, args }
            if name.eq_ignore_ascii_case("to_tsvector") && !args.is_empty() =>
        {
            // PG `to_tsvector` accepts either `to_tsvector(col)` or
            // `to_tsvector(config, col)`. In both shapes the column
            // we care about is the final argument — when there IS one.
            //
            // v7.38.16 — and often there is not: `to_tsvector('english',
            // title || ' ' || body)` is PG's ordinary full-text
            // spelling. There is no single column to name, so the caller
            // matches the index by the EXPRESSION instead and this
            // returns `None` for the position rather than refusing the
            // whole seek.
            match args.last().unwrap() {
                Expr::Column(c) => Some(c),
                _ => None,
            }
        }
        _ => return None,
    };
    let pos = match column {
        Some(c) => {
            if let Some(q) = &c.qualifier
                && q != table_alias
            {
                return None;
            }
            Some(schema_cols.iter().position(|s| s.name == c.name)?)
        }
        None => None,
    };
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
    mysql: bool,
    // v7.38.18 (S2) — the collation an undeclared text column is
    // compared under, and so the one its index keys under. Passed in
    // here: this helper never sees the table.
    db_coll: &str,
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
    let key = probe_key(schema_cols, col_pos, &value, mysql, db_coll)?;
    Some((col_pos, key))
}

/// r1039 — the probe key for `col_pos`, built in THAT COLUMN'S key space.
///
/// See [`spg_storage::IndexKey::from_value_for_column`]: every key under
/// one index comes from one column, so a probe built in another space
/// finds nothing — and nothing reads exactly like "no matching rows".
/// Answer `<expression> = <literal>` from an index keyed on exactly that
/// expression.
///
/// Declines — leaving the caller to scan — unless the index is complete
/// (its B-tree really holds the expression's values; see
/// `Table::expr_index_is_complete`) and the probe key has the same shape
/// as the keys already stored. That second test is the load-bearing one:
/// a candidate set that is too LARGE only costs the re-check the caller
/// runs anyway, but one that is too SMALL is a silently wrong answer, and
/// a probe of the wrong shape misses every entry there is.
fn try_expression_index_seek(
    expr_side: &Expr,
    lit_side: &Expr,
    table: &Table,
    snapshot: &spg_storage::snapshot::Snapshot,
    mysql: bool,
) -> Option<Vec<usize>> {
    // Under MySQL a text comparison folds case, and this B-tree is keyed
    // by bytes: `lower(s) = 'X42'` is TRUE there and would find nothing
    // here. Decline and let the scan answer it. Non-text keys do not
    // fold, so `(k + 0) = 42` keeps its seek in both dialects.
    if mysql && !matches!(expr_side, Expr::Column(_) | Expr::Literal(_)) {
        let probe = match lit_side {
            Expr::Literal(l) => crate::eval::literal_to_value(l),
            _ => return None,
        };
        if matches!(probe, Value::Text(_) | Value::BpChar(_)) {
            return None;
        }
    }
    if matches!(expr_side, Expr::Column(_) | Expr::Literal(_)) {
        return None;
    }
    let Expr::Literal(l) = lit_side else {
        return None;
    };
    let name = crate::expr_index::index_for_expression(table, expr_side)?;
    let idx = table.indices().iter().find(|i| i.name == name)?;
    let key = IndexKey::from_value(&crate::eval::literal_to_value(l))?;
    if idx
        .sample_key()
        .is_some_and(|k| core::mem::discriminant(k) != core::mem::discriminant(&key))
    {
        return None;
    }
    let mut out = Vec::new();
    for loc in idx.lookup_eq(&key) {
        match *loc {
            spg_storage::RowLocator::Hot(i) => {
                if table.is_row_visible(i, snapshot) {
                    out.push(i);
                }
            }
            // A cold body is not readable from here; hand the whole
            // query back to the scan rather than answer it short.
            spg_storage::RowLocator::Cold { .. } => return None,
        }
    }
    out.sort_unstable();
    table.note_index_scan(out.len() as u64);
    Some(out)
}

fn probe_key(
    schema_cols: &[ColumnSchema],
    col_pos: usize,
    value: &Value<'_>,
    mysql: bool,
    db_coll: &str,
) -> Option<IndexKey> {
    let col = schema_cols.get(col_pos)?;
    // v7.38.18 (S0) — a locale-collated column's tree holds ICU sort
    // keys, so the probe is encoded the same way and the seek answers
    // the same question the scan would. This is the ONE funnel for both
    // equality probes and range bounds, so encoding it here is what
    // makes `x = 'apple'` and `x > 'b'` agree with each other.
    //
    // The tree being COMPLETE is the caller's question, not this one's:
    // see `index_is_usable`. An empty tree reads as no rows, which is
    // the failure this whole layer exists to prevent.
    if let Some(coll) = collated_column(col, db_coll) {
        return collated_probe(&coll, value);
    }
    // The one place a probe is built is the one place to ask whether a
    // byte probe can answer the question at all. Under MySQL a text
    // comparison folds case and this B-tree does not, so declining here
    // sends every seek that would have dropped rows back to the scan.
    if !crate::collate::column_key_is_bytewise(col, mysql) {
        return None;
    }
    IndexKey::from_value_for_column(value, col.ty)
}

/// v7.38.18 (S0) — this column's collation when its index keys under
/// one, i.e. when it is text and the collation is not byte order.
fn collated_column(col: &ColumnSchema, db_coll: &str) -> Option<alloc::string::String> {
    if !matches!(
        col.ty,
        spg_storage::DataType::Text
            | spg_storage::DataType::Varchar(_)
            | spg_storage::DataType::Char(_)
    ) {
        return None;
    }
    // v7.38.18 (S2) — the column's own, or the DATABASE's when it
    // declares none. This must give the same answer as
    // `Table::index_collation` gives on the storage side, because that
    // one built the entries and this one builds the probe. They
    // disagreed for exactly one commit, and the symptom was `x =
    // 'apple'` returning nothing at all.
    // Same exclusion `Table::index_collation` makes, and it has to be
    // the same or the probe lands in a different space from the entries.
    if matches!(col.collation, spg_storage::Collation::CaseInsensitive) {
        return None;
    }
    col.collation_name
        .as_deref()
        .or(Some(db_coll))
        .filter(|n| spg_storage::collation_uses_sort_key(n))
        .map(alloc::string::String::from)
}

/// The probe for a collated tree: the same ICU sort key the entries were
/// built from. `None` for a non-text value or an unperformable
/// collation, which declines the seek rather than probing in the wrong
/// space — a probe of the wrong shape finds nothing, and nothing reads
/// exactly like "no matching rows".
fn collated_probe(coll: &str, value: &Value<'_>) -> Option<IndexKey> {
    let text = match value {
        Value::Text(t) => t.as_ref(),
        Value::BpChar(t) => t.as_ref(),
        _ => return None,
    };
    crate::collate::sort_key(coll, text).map(IndexKey::Bytes)
}

/// v7.38.18 (S0) — may this index be READ right now?
///
/// A collated index's tree is filled by the engine, not by storage, so
/// between `CREATE INDEX` and the refresh that fills it — and after any
/// row rewrite that retires it — it is EMPTY. Reading an empty tree
/// returns no rows, which is indistinguishable from a correct answer.
/// Every seek asks this before using one.
pub(crate) fn index_is_usable(table: &Table, idx: &spg_storage::Index) -> bool {
    table.index_collation(idx).is_none() || table.expr_index_is_complete(&idx.name)
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
    Some((pos, literal_as_column_value(l, &schema_cols[pos], pos)?))
}

/// What a literal MEANS as a value of column `col` — the one place that
/// decision is made.
///
/// r1039 — there were four copies of it, and they disagreed. Round 564
/// fixed the single-table equality seek, r1037 fixed the JOIN driver's
/// and the JOIN peer's, and this one — the IN-list seek — was still
/// reading every string literal as TEXT. Measured on `develop` before
/// this change, one row in the table either way:
///
/// ```text
///                        no index   with index
/// WHERE d = '2026-01-02'     1           1
/// WHERE d IN ('2026-01-02')  1           0     <- DATE column
/// WHERE d IN ('<a uuid>')    1           0     <- UUID column
/// ```
///
/// Creating an index changed the answer, silently, which is the one thing
/// an index may never do.
pub(crate) fn literal_as_column_value(
    l: &Literal,
    col: &ColumnSchema,
    col_pos: usize,
) -> Option<Value<'static>> {
    let v = match l {
        Literal::Integer(n) => {
            if let Ok(small) = i32::try_from(*n) {
                Value::Int(small)
            } else {
                Value::BigInt(*n)
            }
        }
        Literal::Float(x) => Value::Float(*x),
        Literal::Numeric { unscaled, scale } => Value::Numeric {
            scaled: *unscaled,
            scale: *scale,
            kind: spg_storage::NumericKind::Finite,
        },
        Literal::NumericBig(s) => crate::conversions::big_literal_to_value(s),
        // v7.38.8 — a decoded temporal constant is an ordinary scalar
        // key. Declining it here would turn every seek on a timestamp
        // or date column into a full scan the moment the constant
        // started being carried decoded — a silent perf regression
        // wearing the shape of a planner decision.
        Literal::Timestamp { micros, .. } => Value::Timestamp(*micros),
        Literal::Date { days, .. } => Value::Date(*days),
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
    // v7.39 (round 564) — a string literal means whatever the COLUMN
    // says it means, and until here it always meant text.
    //
    // `WHERE d = '2026-01-02'` on a `DATE` column built an
    // `IndexKey::Text`, while the rows under that index are keyed
    // `IndexKey::Int` — days. The seek looked in a key space nothing
    // lives in and found nothing, so:
    //
    //     no index     d = '2026-01-02'   ->  1 row   (PG: 1)
    //     with index   d = '2026-01-02'   ->  0 rows  (PG: 1)
    //
    // Creating an index changed the answer, which is the one thing an
    // index may never do. Two-sided ranges went the same way; a
    // one-sided `d > '…'` survived only because it is not seeked at all.
    // Every string-literal comparison against a date, timestamp, time,
    // uuid or bool column was affected.
    //
    // Only string literals, and only against a non-text column: parsing
    // a string to its column type is exact or it raises, so a
    // coercion that succeeds cannot seek to the wrong key. Numeric
    // coercions are deliberately NOT done here — `WHERE i = 1.5` on an
    // integer column must not round its way to the rows holding 2.
    // A coercion that fails means the ordinary path re-raises it.
    let ty = col.ty;
    if matches!(l, Literal::String(_))
        && !matches!(
            ty,
            spg_storage::DataType::Text
                | spg_storage::DataType::Varchar(_)
                | spg_storage::DataType::Char(_)
        )
    {
        return crate::conversions::coerce_value(v, ty, &col.name, col_pos).ok();
    }
    Some(v)
}
