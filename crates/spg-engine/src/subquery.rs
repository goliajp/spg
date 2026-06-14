//! Correlated-subquery evaluation split out of `lib.rs` (lib.rs split
//! 5): the per-row `eval_expr_with_correlated` path (clones the
//! expression, substitutes outer-row columns into each surviving
//! subquery node, runs the inner SELECT, folds the literal result back)
//! plus the `subquery_replacement` pre-walk that materialises
//! uncorrelated subquery nodes once, and the `try_batch_correlated_scalar`
//! keyed-probe optimisation (round-22 phase 3) that runs a correlated
//! scalar subquery ONCE without the correlation and folds rows into a
//! key→value map. `impl Engine` methods; the bare-SELECT / DML / join
//! row loops drive `eval_expr_with_correlated`, and `select.rs` drives
//! `subquery_replacement` / `try_batch_correlated_scalar`.

use alloc::string::String;
use alloc::vec::Vec;

use spg_sql::ast::{Expr, Literal, SelectStatement};
use spg_storage::{Row, Value};

use crate::eval::{self, EvalContext};
use crate::substitute::value_to_literal_expr;
use crate::{
    CancelToken, Engine, EngineError, QueryResult, aggregate, collect_scalar_subqueries,
    eval_with_in_sets, expr_has_subquery, expr_may_use_in_set, hollow_scalar_subqueries,
    is_correlation_error, memoize, order_by_value_cmp, reorder, select_is_correlated,
    splice_planned_subqueries, substitute_outer_columns, value_cmp,
    visit_expr_columns_and_subqueries,
};

impl Engine {
    /// v4.23: per-row eval that handles correlated subqueries.
    /// Equivalent to `eval::eval_expr` when the expression has no
    /// subqueries; otherwise clones the expression, substitutes
    /// outer-row columns into each surviving subquery node, runs
    /// the inner SELECT, and replaces the node with the literal
    /// result. Only the WHERE-filter call sites use this path so
    /// the uncorrelated fast path is preserved everywhere else.
    pub(crate) fn eval_expr_with_correlated(
        &self,
        expr: &Expr,
        row: &Row,
        ctx: &EvalContext<'_>,
        cancel: CancelToken<'_>,
        mut memo: Option<&mut memoize::MemoizeCache>,
    ) -> Result<Value, EngineError> {
        // v7.30.2 (mailrs round-25) — the has-subquery walk is
        // O(tree) and a materialised `IN (…)` list makes the tree
        // huge; cache the answer per expression address so the
        // per-row dispatch stops re-walking 24k list elements.
        let has_subq = if let Some(m) = memo.as_deref_mut() {
            let key = core::ptr::from_ref::<Expr>(expr) as usize;
            match m.has_subquery.get(&key) {
                Some(b) => *b,
                None => {
                    let b = expr_has_subquery(expr);
                    m.has_subquery.insert(key, b);
                    b
                }
            }
        } else {
            expr_has_subquery(expr)
        };
        if !has_subq {
            // A large materialised `IN (…)` list inside the WHERE
            // makes the plain eval O(rows × list); route through the
            // per-query membership set (built once, keyed by node
            // address) when one is reachable on the AND spine.
            if let Some(m) = memo.as_deref_mut()
                && expr_may_use_in_set(expr)
            {
                return eval_with_in_sets(expr, row, ctx, m);
            }
            return eval::eval_expr(expr, row, ctx).map_err(EngineError::Eval);
        }
        // v7.29 (3c) - per-expression plan: the batch maps for this
        // host expression's scalar subqueries are looked up by the
        // expression's ADDRESS (stable across the row loop), so the
        // hot path does zero AST formatting. Building the plan (and
        // its Display-keyed group maps) happens once per expression.
        if let Some(m) = memo.as_deref_mut() {
            let key = core::ptr::from_ref::<Expr>(expr) as usize;
            // Plan hit: skip the collection walk entirely (it ran
            // once per group otherwise - 70k walks per inbox query).
            // The memo is per-query and host expressions outlive it,
            // so an address that hit once stays valid.
            let plan_hit = m.expr_plans.contains_key(&key);
            let mut subs: Vec<&SelectStatement> = Vec::new();
            if !plan_hit {
                collect_scalar_subqueries(expr, &mut subs);
            }
            if !plan_hit && !subs.is_empty() {
                let mut plan: Vec<Option<alloc::rc::Rc<memoize::GroupMap>>> =
                    Vec::with_capacity(subs.len());
                for sub in &subs {
                    let repr = alloc::format!("{sub}");
                    if !m.group_maps.contains_key(&repr) {
                        let built = self
                            .try_batch_correlated_scalar(sub, None, cancel)?
                            .map(alloc::rc::Rc::new);
                        m.group_maps.insert(repr.clone(), built);
                    }
                    plan.push(m.group_maps.get(&repr).cloned().flatten());
                }
                let mut template = expr.clone();
                hollow_scalar_subqueries(&mut template);
                m.expr_plans.insert(key, (subs.len(), plan, template));
            }
            if let Some((_, plan, template)) = m.expr_plans.get(&key)
                && !plan.is_empty()
                && plan.iter().all(|p| p.is_some())
            {
                // Fast path: every scalar subquery resolves via its
                // map; clone the HOLLOW template (subquery bodies
                // emptied at plan time - cloning full subquery ASTs
                // per row was the dominant malloc load), splice map
                // values, eval. Exists/IN subqueries (if any) still
                // drop to the resolver.
                let plan = plan.clone();
                let mut e = template.clone();
                let mut idx = 0usize;
                let ok = splice_planned_subqueries(&mut e, &plan, &mut idx, row, ctx)?;
                if ok {
                    if expr_has_subquery(&e) {
                        self.resolve_correlated_in_expr(&mut e, row, ctx, cancel, memo)?;
                    }
                    return eval::eval_expr(&e, row, ctx).map_err(EngineError::Eval);
                }
            }
        }
        let mut e = expr.clone();
        self.resolve_correlated_in_expr(&mut e, row, ctx, cancel, memo)?;
        eval::eval_expr(&e, row, ctx).map_err(EngineError::Eval)
    }

    fn resolve_correlated_in_expr(
        &self,
        e: &mut Expr,
        row: &Row,
        ctx: &EvalContext<'_>,
        cancel: CancelToken<'_>,
        mut memo: Option<&mut memoize::MemoizeCache>,
    ) -> Result<(), EngineError> {
        match e {
            Expr::AggregateOrdered { call, order_by, .. } => {
                self.resolve_correlated_in_expr(call, row, ctx, cancel, memo.as_deref_mut())?;
                for o in order_by.iter_mut() {
                    self.resolve_correlated_in_expr(
                        &mut o.expr,
                        row,
                        ctx,
                        cancel,
                        memo.as_deref_mut(),
                    )?;
                }
            }
            Expr::ScalarSubquery(inner) => {
                // v7.29 (round-22 phase 3) — batch path first: a
                // correlated scalar of the `inner_col = outer_col
                // [ORDER BY … LIMIT 1]` shape evaluates ONCE as a
                // grouped scan; per-row resolution becomes a map
                // lookup. 23.5k per-group executions (~900 ms) became
                // one scan + lookups.
                if memo.is_some() {
                    let repr = alloc::format!("{}", **inner);
                    let entry_known = memo
                        .as_ref()
                        .is_some_and(|m| m.group_maps.contains_key(&repr));
                    if !entry_known {
                        let built = self
                            .try_batch_correlated_scalar(inner, None, cancel)?
                            .map(alloc::rc::Rc::new);
                        if let Some(m) = memo.as_deref_mut() {
                            m.group_maps.insert(repr.clone(), built);
                        }
                    }
                    if let Some(m) = memo.as_deref_mut()
                        && let Some(Some(gm)) = m.group_maps.get(&repr)
                    {
                        let (outer_col, map) = gm.as_ref();
                        let key_v = eval::eval_expr(&Expr::Column(outer_col.clone()), row, ctx)
                            .map_err(EngineError::Eval)?;
                        let v = if matches!(key_v, Value::Null) {
                            Value::Null
                        } else {
                            map.get(&aggregate::encode_key(core::slice::from_ref(&key_v)))
                                .cloned()
                                .unwrap_or(Value::Null)
                        };
                        *e = value_to_literal_expr(v)?;
                        return Ok(());
                    }
                }
                // v6.2.6 — Memoize: build the cache key from the
                // pre-substitution subquery repr + the outer row's
                // values. Two outer rows with identical correlated
                // values hit the same entry.
                let cache_key = memo.as_ref().map(|_| memoize::CacheKey {
                    subquery_repr: alloc::format!("{}", **inner),
                    outer_values: row.values.clone(),
                });
                if let (Some(cache), Some(k)) = (memo.as_deref_mut(), cache_key.as_ref())
                    && let Some(cached) = cache.get(k)
                {
                    *e = value_to_literal_expr(cached)?;
                    return Ok(());
                }
                let mut s = (**inner).clone();
                substitute_outer_columns(&mut s, row, ctx);
                let r = self.exec_select_cancel(&s, cancel)?;
                let QueryResult::Rows { rows, .. } = r else {
                    return Err(EngineError::Unsupported(
                        "scalar subquery: inner did not return rows".into(),
                    ));
                };
                let value = match rows.as_slice() {
                    [] => Value::Null,
                    [r0] => r0.values.first().cloned().unwrap_or(Value::Null),
                    _ => {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "scalar subquery returned {} rows; expected 0 or 1",
                            rows.len()
                        )));
                    }
                };
                if let (Some(cache), Some(k)) = (memo.as_deref_mut(), cache_key) {
                    cache.insert(k, value.clone());
                }
                *e = value_to_literal_expr(value)?;
            }
            Expr::Exists { subquery, negated } => {
                let mut s = (**subquery).clone();
                substitute_outer_columns(&mut s, row, ctx);
                let r = self.exec_select_cancel(&s, cancel)?;
                let exists = matches!(r, QueryResult::Rows { rows, .. } if !rows.is_empty());
                let bit = if *negated { !exists } else { exists };
                *e = Expr::Literal(Literal::Bool(bit));
            }
            Expr::InSubquery {
                expr: lhs,
                subquery,
                negated,
            } => {
                self.resolve_correlated_in_expr(lhs, row, ctx, cancel, memo.as_deref_mut())?;
                let lhs_val = eval::eval_expr(lhs, row, ctx).map_err(EngineError::Eval)?;
                let mut s = (**subquery).clone();
                substitute_outer_columns(&mut s, row, ctx);
                let r = self.exec_select_cancel(&s, cancel)?;
                let QueryResult::Rows { columns, rows, .. } = r else {
                    return Err(EngineError::Unsupported(
                        "IN-subquery: inner did not return rows".into(),
                    ));
                };
                if columns.len() != 1 {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "IN-subquery must project exactly one column; got {}",
                        columns.len()
                    )));
                }
                let mut found = false;
                let mut any_null = false;
                for r0 in rows {
                    let v = r0.values.into_iter().next().unwrap_or(Value::Null);
                    if v.is_null() {
                        any_null = true;
                        continue;
                    }
                    if value_cmp(&v, &lhs_val) == core::cmp::Ordering::Equal {
                        found = true;
                        break;
                    }
                }
                let bit = if found {
                    !*negated
                } else if any_null {
                    return Err(EngineError::Unsupported(
                        "IN-subquery with NULL in result and no match: NULL semantics not yet implemented".into(),
                    ));
                } else {
                    *negated
                };
                *e = Expr::Literal(Literal::Bool(bit));
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.resolve_correlated_in_expr(lhs, row, ctx, cancel, memo.as_deref_mut())?;
                self.resolve_correlated_in_expr(rhs, row, ctx, cancel, memo.as_deref_mut())?;
            }
            Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
                self.resolve_correlated_in_expr(expr, row, ctx, cancel, memo.as_deref_mut())?;
            }
            Expr::Like { expr, pattern, .. } => {
                self.resolve_correlated_in_expr(expr, row, ctx, cancel, memo.as_deref_mut())?;
                self.resolve_correlated_in_expr(pattern, row, ctx, cancel, memo.as_deref_mut())?;
            }
            Expr::FunctionCall { args, .. } => {
                for a in args {
                    self.resolve_correlated_in_expr(a, row, ctx, cancel, memo.as_deref_mut())?;
                }
            }
            Expr::Extract { source, .. } => {
                self.resolve_correlated_in_expr(source, row, ctx, cancel, memo.as_deref_mut())?;
            }
            Expr::WindowFunction { .. }
            | Expr::Literal(_)
            | Expr::Placeholder(_)
            | Expr::Column(_) => {}
            // v7.10.10 — recurse children.
            Expr::Array(items) => {
                for elem in items {
                    self.resolve_correlated_in_expr(elem, row, ctx, cancel, memo.as_deref_mut())?;
                }
            }
            Expr::ArraySubscript { target, index } => {
                self.resolve_correlated_in_expr(target, row, ctx, cancel, memo.as_deref_mut())?;
                self.resolve_correlated_in_expr(index, row, ctx, cancel, memo.as_deref_mut())?;
            }
            Expr::AnyAll { expr, array, .. } => {
                self.resolve_correlated_in_expr(expr, row, ctx, cancel, memo.as_deref_mut())?;
                self.resolve_correlated_in_expr(array, row, ctx, cancel, memo.as_deref_mut())?;
            }
            Expr::InList { expr, list, .. } => {
                self.resolve_correlated_in_expr(expr, row, ctx, cancel, memo.as_deref_mut())?;
                for item in list {
                    self.resolve_correlated_in_expr(item, row, ctx, cancel, memo.as_deref_mut())?;
                }
            }
            Expr::Case {
                operand,
                branches,
                else_branch,
            } => {
                if let Some(o) = operand {
                    self.resolve_correlated_in_expr(o, row, ctx, cancel, memo.as_deref_mut())?;
                }
                for (w, t) in branches {
                    self.resolve_correlated_in_expr(w, row, ctx, cancel, memo.as_deref_mut())?;
                    self.resolve_correlated_in_expr(t, row, ctx, cancel, memo.as_deref_mut())?;
                }
                if let Some(e) = else_branch {
                    self.resolve_correlated_in_expr(e, row, ctx, cancel, memo.as_deref_mut())?;
                }
            }
        }
        Ok(())
    }

    /// v4.10: pre-walk the WHERE / projection / etc. of a SELECT and
    /// replace every subquery node with a materialised literal. SPG
    /// only supports uncorrelated subqueries — the inner SELECT does
    /// not see outer-row columns, so the result is the same for every
    /// outer row and can be evaluated once.
    ///
    /// Returns the rewritten statement; the caller passes this to the
    /// regular row-loop executor which no longer sees Subquery nodes
    /// in its tree.
    pub(crate) fn subquery_replacement(
        &self,
        e: &Expr,
        cancel: CancelToken<'_>,
    ) -> Result<Option<Expr>, EngineError> {
        match e {
            Expr::ScalarSubquery(inner) => {
                // v7.32 (R30) — a correlated subquery is resolved by
                // the per-row / post-LIMIT correlated path; executing
                // it here only to catch the correlation error first
                // materialises (and discards) its whole inner FROM.
                if select_is_correlated(inner) {
                    return Ok(None);
                }
                let mut s = (**inner).clone();
                // Recurse into the inner SELECT first so nested
                // subqueries materialise bottom-up.
                self.resolve_select_subqueries(&mut s, cancel)?;
                let r = match self.exec_bare_select_cancel(&s, cancel) {
                    Ok(r) => r,
                    Err(e) if is_correlation_error(&e) => return Ok(None),
                    Err(e) => return Err(e),
                };
                let QueryResult::Rows { rows, .. } = r else {
                    return Err(EngineError::Unsupported(
                        "scalar subquery: inner statement did not return rows".into(),
                    ));
                };
                let value = match rows.as_slice() {
                    [] => Value::Null,
                    [row] => row.values.first().cloned().unwrap_or(Value::Null),
                    _ => {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "scalar subquery returned {} rows; expected 0 or 1",
                            rows.len()
                        )));
                    }
                };
                Ok(Some(value_to_literal_expr(value)?))
            }
            Expr::Exists { subquery, negated } => {
                if select_is_correlated(subquery) {
                    return Ok(None);
                }
                let mut s = (**subquery).clone();
                self.resolve_select_subqueries(&mut s, cancel)?;
                let r = match self.exec_bare_select_cancel(&s, cancel) {
                    Ok(r) => r,
                    Err(e) if is_correlation_error(&e) => return Ok(None),
                    Err(e) => return Err(e),
                };
                let exists = match r {
                    QueryResult::Rows { rows, .. } => !rows.is_empty(),
                    QueryResult::CommandOk { .. } => false,
                };
                let bit = if *negated { !exists } else { exists };
                Ok(Some(Expr::Literal(Literal::Bool(bit))))
            }
            Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => {
                if select_is_correlated(subquery) {
                    return Ok(None);
                }
                let mut s = (**subquery).clone();
                self.resolve_select_subqueries(&mut s, cancel)?;
                let r = match self.exec_bare_select_cancel(&s, cancel) {
                    Ok(r) => r,
                    Err(e) if is_correlation_error(&e) => return Ok(None),
                    Err(e) => return Err(e),
                };
                let QueryResult::Rows { columns, rows, .. } = r else {
                    return Err(EngineError::Unsupported(
                        "IN-subquery: inner statement did not return rows".into(),
                    ));
                };
                if columns.len() != 1 {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "IN-subquery must project exactly one column; got {}",
                        columns.len()
                    )));
                }
                // v7.30.2 (mailrs round-25) — flat InList, NOT an OR-Eq
                // chain: chain depth scaled with the inner result's ROW
                // COUNT, so one 24k-match search overflowed the worker
                // stack (recursive eval + recursive Box drop) and
                // aborted the embedding host process.
                let mut list: Vec<Expr> = Vec::with_capacity(rows.len());
                for row in rows {
                    let v = row.values.into_iter().next().unwrap_or(Value::Null);
                    list.push(value_to_literal_expr(v)?);
                }
                Ok(Some(Expr::InList {
                    expr: expr.clone(),
                    list,
                    negated: *negated,
                }))
            }
            _ => Ok(None),
        }
    }
}

impl Engine {
    /// v7.29 (round-22 phase 3) — try to batch-evaluate a correlated
    /// scalar subquery of the shape
    ///   (SELECT expr FROM … WHERE inner_preds AND inner_col = outer_col
    ///    [ORDER BY o [DESC]] [LIMIT 1])
    /// by running the subquery ONCE without the correlation and
    /// folding rows into a key→value map (group top-1 when ordered).
    /// Returns None when the shape doesn't qualify; correctness then
    /// falls back to per-row execution.
    pub(crate) fn try_batch_correlated_scalar(
        &self,
        inner: &SelectStatement,
        restrict: Option<(&[Row], &EvalContext<'_>)>,
        cancel: CancelToken<'_>,
    ) -> Result<Option<memoize::GroupMap>, EngineError> {
        use spg_sql::ast::{BinOp, SelectItem as SI};
        if !inner.ctes.is_empty()
            || !inner.unions.is_empty()
            || inner.group_by.is_some()
            || inner.having.is_some()
            || inner.distinct
            || inner.items.len() != 1
            || inner.order_by.len() > 1
            || inner.offset.is_some()
        {
            return Ok(None);
        }
        // LIMIT must be absent or literally 1 (top-1 semantics).
        if let Some(le) = inner.limit
            && le.as_literal() != Some(1)
        {
            return Ok(None);
        }
        let Some(from) = &inner.from else {
            return Ok(None);
        };
        if from.primary.lateral_subquery.is_some() || from.primary.unnest_expr.is_some() {
            return Ok(None);
        }
        // Inner alias set.
        let mut inner_aliases: Vec<String> = Vec::new();
        inner_aliases.push(
            from.primary
                .alias
                .clone()
                .unwrap_or_else(|| from.primary.name.clone()),
        );
        for j in &from.joins {
            if j.table.lateral_subquery.is_some() || j.table.unnest_expr.is_some() {
                return Ok(None);
            }
            inner_aliases.push(
                j.table
                    .alias
                    .clone()
                    .unwrap_or_else(|| j.table.name.clone()),
            );
        }
        let is_inner = |c: &spg_sql::ast::ColumnName| -> bool {
            match &c.qualifier {
                Some(q) => inner_aliases.iter().any(|a| a.eq_ignore_ascii_case(q)),
                None => false,
            }
        };
        let is_outer = |c: &spg_sql::ast::ColumnName| -> bool {
            match &c.qualifier {
                Some(q) => !inner_aliases.iter().any(|a| a.eq_ignore_ascii_case(q)),
                // Synthetic group columns arrive bare after the
                // aggregate rewrite.
                None => c.name.starts_with("__grp_") || c.name.starts_with("__agg_"),
            }
        };
        // Every expression OTHER than the correlation conjunct must be
        // fully inner (qualified to inner aliases).
        let all_inner = |e: &Expr| -> bool {
            let mut cols: Vec<spg_sql::ast::ColumnName> = Vec::new();
            let mut subs: Vec<&SelectStatement> = Vec::new();
            visit_expr_columns_and_subqueries(e, &mut |c| cols.push(c.clone()), &mut |sub| {
                subs.push(sub)
            });
            subs.is_empty() && cols.iter().all(|c| is_inner(c) && !c.name.is_empty())
        };
        let Some(w) = &inner.where_ else {
            return Ok(None);
        };
        let conjuncts = reorder::split_and_conjunctions(w);
        let mut corr: Option<(spg_sql::ast::ColumnName, spg_sql::ast::ColumnName)> = None; // (inner, outer)
        let mut rest: Vec<&Expr> = Vec::new();
        for c in conjuncts {
            if let Expr::Binary {
                lhs,
                op: BinOp::Eq,
                rhs,
            } = c
                && let (Expr::Column(a), Expr::Column(b)) = (lhs.as_ref(), rhs.as_ref())
            {
                let pair = if is_inner(a) && is_outer(b) {
                    Some((a.clone(), b.clone()))
                } else if is_inner(b) && is_outer(a) {
                    Some((b.clone(), a.clone()))
                } else {
                    None
                };
                if let Some(p) = pair {
                    if corr.is_some() {
                        return Ok(None); // more than one correlation
                    }
                    corr = Some(p);
                    continue;
                }
            }
            if !all_inner(c) {
                return Ok(None);
            }
            rest.push(c);
        }
        let Some((inner_col, outer_col)) = corr else {
            return Ok(None);
        };
        let SI::Expr { expr: out_expr, .. } = &inner.items[0] else {
            return Ok(None);
        };
        if !all_inner(out_expr) {
            return Ok(None);
        }
        let order = inner.order_by.first();
        if let Some(o) = order
            && !all_inner(&o.expr)
        {
            return Ok(None);
        }
        // Build the batch statement: SELECT inner_col, [order], expr
        // FROM … WHERE rest — no correlation, no order, no limit.
        let mut batch = inner.clone();
        batch.limit = None;
        batch.offset = None;
        batch.order_by = Vec::new();
        batch.where_ = rest
            .iter()
            .map(|e| (*e).clone())
            .reduce(|a, b| Expr::Binary {
                lhs: alloc::boxed::Box::new(a),
                op: BinOp::And,
                rhs: alloc::boxed::Box::new(b),
            });
        let mut items: Vec<SI> = alloc::vec![SI::Expr {
            expr: Expr::Column(inner_col.clone()),
            alias: None,
        }];
        if let Some(o) = order {
            items.push(SI::Expr {
                expr: o.expr.clone(),
                alias: None,
            });
        }
        items.push(SI::Expr {
            expr: out_expr.clone(),
            alias: None,
        });
        batch.items = items;
        // v7.32 (architecture v2 P3) — keyed index-probe. When the
        // caller hands a restriction set (the ≤LIMIT surviving outer
        // rows of a post-LIMIT deferred subquery) AND the correlation
        // column is backed by an index, evaluate only the surviving
        // correlation keys via per-key index seek instead of scanning
        // the whole inner relation. This is PG's SubPlan with an index
        // scan: 50 seeks of ~µs each vs a 24k-row all-keys batch
        // (~16 ms). The grouping below is shared — keyed result ≡
        // full-batch result for the covered keys, so semantics are
        // identical.
        //
        // The inner relation may itself be a join. The correlation
        // column names the *driving* table; PG, MySQL and MariaDB all
        // plan a correlated join subquery the same way — seek the
        // correlation index, then index-nested-loop to the joined
        // table. We promote that table to drive `batch` (an all-INNER
        // chain only) so the per-key `inner_col = <lit>` predicate
        // becomes a primary index seek and the existing INL path joins
        // the rest. A correlation column without a usable index, or a
        // join the promotion can't safely reorder, returns None and
        // the caller falls back to the lazy all-keys batch (no
        // regression).
        let keyed: Option<(&[Row], &EvalContext<'_>)> = restrict.and_then(|(rows, rctx)| {
            // Resolve the table that owns the correlation column.
            let driver_name: &str = if from.joins.is_empty() {
                from.primary.name.as_str()
            } else {
                let q = inner_col.qualifier.as_deref()?;
                let primary_alias = from
                    .primary
                    .alias
                    .as_deref()
                    .unwrap_or(from.primary.name.as_str());
                if primary_alias.eq_ignore_ascii_case(q) {
                    from.primary.name.as_str()
                } else {
                    from.joins
                        .iter()
                        .find(|j| {
                            j.table
                                .alias
                                .as_deref()
                                .unwrap_or(j.table.name.as_str())
                                .eq_ignore_ascii_case(q)
                        })
                        .map(|j| j.table.name.as_str())?
                }
            };
            let table = self.active_catalog().get(driver_name)?;
            let pos = table
                .schema()
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(&inner_col.name))?;
            table.index_on(pos)?;
            // For a join inner, drive the seek from the correlation
            // table so `inner_col = <lit>` lands as a primary index
            // seek (else the source-order primary scans the full
            // relation and the join hash-builds the whole peer — the
            // 12 GB all-keys hog R30 hit at prod scale).
            if !from.joins.is_empty() {
                let driver_alias = inner_col.qualifier.as_deref()?;
                if !reorder::drive_from(&mut batch, driver_alias) {
                    return None;
                }
            }
            Some((rows, rctx))
        });
        let rows = if let Some((restrict_rows, rctx)) = keyed {
            let mut seen: alloc::collections::BTreeSet<String> =
                alloc::collections::BTreeSet::new();
            let mut all_rows: Vec<Row> = Vec::new();
            for srow in restrict_rows {
                cancel.check()?;
                let kv = eval::eval_expr(&Expr::Column(outer_col.clone()), srow, rctx)
                    .map_err(EngineError::Eval)?;
                if matches!(kv, Value::Null) {
                    continue;
                }
                if !seen.insert(aggregate::encode_key(core::slice::from_ref(&kv))) {
                    continue;
                }
                let key_eq = Expr::Binary {
                    lhs: alloc::boxed::Box::new(Expr::Column(inner_col.clone())),
                    op: BinOp::Eq,
                    rhs: alloc::boxed::Box::new(value_to_literal_expr(kv)?),
                };
                let mut probe = batch.clone();
                probe.where_ = Some(match probe.where_.take() {
                    Some(w) => Expr::Binary {
                        lhs: alloc::boxed::Box::new(w),
                        op: BinOp::And,
                        rhs: alloc::boxed::Box::new(key_eq),
                    },
                    None => key_eq,
                });
                if let QueryResult::Rows { rows, .. } = self.exec_select_cancel(&probe, cancel)? {
                    all_rows.extend(rows);
                }
            }
            all_rows
        } else {
            let r = self.exec_select_cancel(&batch, cancel)?;
            let QueryResult::Rows { rows, .. } = r else {
                return Ok(None);
            };
            rows
        };
        let has_order = order.is_some();
        let (desc, nf) = order
            .map(|o| (o.desc, o.nulls_first))
            .unwrap_or((false, None));
        let mut best: alloc::collections::BTreeMap<String, (Option<Value>, Value)> =
            alloc::collections::BTreeMap::new();
        for row in rows {
            let key_v = row.values.first().cloned().unwrap_or(Value::Null);
            if matches!(key_v, Value::Null) {
                continue;
            }
            let key = aggregate::encode_key(core::slice::from_ref(&key_v));
            let (ord_v, out_v) = if has_order {
                (
                    Some(row.values.get(1).cloned().unwrap_or(Value::Null)),
                    row.values.get(2).cloned().unwrap_or(Value::Null),
                )
            } else {
                (None, row.values.get(1).cloned().unwrap_or(Value::Null))
            };
            match best.get(&key) {
                None => {
                    best.insert(key, (ord_v, out_v));
                }
                Some((cur_ord, _)) if has_order => {
                    // The sorted-first row wins: candidate beats the
                    // incumbent when it compares LESS under the key's
                    // ordering.
                    let cand = ord_v.clone().unwrap_or(Value::Null);
                    let cur = cur_ord.clone().unwrap_or(Value::Null);
                    if order_by_value_cmp(desc, nf, &cand, &cur) == core::cmp::Ordering::Less {
                        best.insert(key, (ord_v, out_v));
                    }
                }
                Some(_) => {} // unordered: first row stands (any row is valid)
            }
        }
        let map = best.into_iter().map(|(k, (_, v))| (k, v)).collect();
        Ok(Some((outer_col, map)))
    }
}
