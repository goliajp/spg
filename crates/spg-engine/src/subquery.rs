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

use spg_sql::ast::{BinOp, Expr, Literal, SelectItem, SelectStatement};
use spg_storage::{Row, Value};

use crate::eval::{self, EvalContext};
use crate::substitute::value_to_literal_expr;
use crate::{
    CancelToken, Engine, EngineError, QueryResult, aggregate, memoize, order_by_value_cmp, reorder,
    value_cmp, visit_expr_columns_and_subqueries,
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
            // v7.33 (mailrs 7.32.1) — cost guard. The keyed path runs one
            // index seek (a full `exec_select_cancel` round trip) per
            // surviving correlation key. That wins when few keys survive
            // (a tight outer LIMIT leaves a handful), but a *correlated
            // select-list subquery with no outer LIMIT* leaves every group
            // alive — `restrict` is then all ~N groups, and N seeks dwarf
            // a single grouped all-keys scan of the same driver. Reproduced
            // on the conversation aggregation (`get_conversations_by_thread_ids`,
            // no LIMIT): 24k per-key seeks took 78–155 ms vs ~one scan.
            // Fall through to the all-keys batch (`keyed = None` → the
            // `else` arm below) when the survivor set is large relative to
            // the driver; the batch's group map ⊇ the keyed map for every
            // covered key, so the result is identical. Crossover ~rows/4
            // (measured per-seek exec overhead vs per-row scan cost).
            if rows.len().saturating_mul(4) >= table.row_count() {
                return None;
            }
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

// ---- subquery free-fn helpers (lib.rs split 6) ----

/// v4.23: recognise the engine errors that indicate the inner
/// SELECT couldn't be evaluated in isolation because it references
/// an outer column — used by `subquery_replacement` to skip
/// materialisation and let row-eval handle it instead.
fn is_correlation_error(e: &EngineError) -> bool {
    matches!(
        e,
        EngineError::Eval(
            eval::EvalError::ColumnNotFound { .. } | eval::EvalError::UnknownQualifier { .. }
        )
    )
}

/// v7.32 (R30 memory) — cheap static correlation pre-check.
///
/// `subquery_replacement` distinguishes a correlated subquery from an
/// uncorrelated one by *optimistically executing* it and catching the
/// resulting `ColumnNotFound` / `UnknownQualifier`. For a join-bodied
/// correlated subquery that catch fires only AFTER the inner FROM is
/// materialised — and the deferred-join pipeline clones the whole
/// driving table to do it (the inbox `… JOIN messages m2 …` body
/// clones 960k × 10 KB ≈ 10 GB at prod scale, once per outer query,
/// purely to be thrown away). A correlated subquery is always handled
/// downstream by the per-row / post-LIMIT correlated path, so spotting
/// it up front lets us skip the wasted materialisation entirely.
///
/// Sound for the `true` answer: returns true only when a qualified
/// column at the statement's own level names a qualifier that is not
/// one of its own FROM aliases — exactly the reference the inner exec
/// would fail to resolve. Everything it can't reason about cleanly
/// (lateral / derived FROM entries) returns false and falls through to
/// the existing execute-and-catch path, so behaviour is unchanged.
fn select_is_correlated(s: &SelectStatement) -> bool {
    use spg_sql::ast::SelectItem;
    let Some(from) = &s.from else {
        // No FROM: correlated iff some projected column is qualified
        // (a qualifier with nothing to bind to is necessarily outer).
        let mut qualified = false;
        for item in &s.items {
            if let SelectItem::Expr { expr, .. } = item {
                visit_expr_columns_and_subqueries(
                    expr,
                    &mut |c| {
                        if c.qualifier.is_some() {
                            qualified = true;
                        }
                    },
                    &mut |_| {},
                );
            }
        }
        return qualified;
    };
    // Lateral / derived FROM entries put scope resolution beyond this
    // cheap check — defer to execute-and-catch.
    if from.primary.lateral_subquery.is_some() {
        return false;
    }
    let mut inner: Vec<&str> = Vec::new();
    if let Some(a) = &from.primary.alias {
        inner.push(a.as_str());
    }
    if !from.primary.name.is_empty() {
        inner.push(from.primary.name.as_str());
    }
    for j in &from.joins {
        if j.table.lateral_subquery.is_some() {
            return false;
        }
        if let Some(a) = &j.table.alias {
            inner.push(a.as_str());
        }
        if !j.table.name.is_empty() {
            inner.push(j.table.name.as_str());
        }
    }
    // Gather every expression position that evaluates in this
    // statement's own scope (NOT inside nested subquery bodies — the
    // visitor reports those via the subquery callback, which we drop).
    let mut exprs: Vec<&Expr> = Vec::new();
    for item in &s.items {
        if let SelectItem::Expr { expr, .. } = item {
            exprs.push(expr);
        }
    }
    if let Some(w) = &s.where_ {
        exprs.push(w);
    }
    for j in &from.joins {
        if let Some(on) = &j.on {
            exprs.push(on);
        }
    }
    if let Some(gs) = &s.group_by {
        for g in gs {
            exprs.push(g);
        }
    }
    if let Some(h) = &s.having {
        exprs.push(h);
    }
    for o in &s.order_by {
        exprs.push(&o.expr);
    }
    let mut correlated = false;
    for e in exprs {
        visit_expr_columns_and_subqueries(
            e,
            &mut |c| {
                if let Some(q) = &c.qualifier
                    && !inner.iter().any(|a| a.eq_ignore_ascii_case(q))
                {
                    correlated = true;
                }
            },
            &mut |_| {},
        );
    }
    correlated
}

/// v7.29 (3c) — pre-order collection of SCALAR subquery nodes in a
/// host expression (no descent into subquery bodies). The splice
/// walk below uses the same order; the pair must stay in lockstep.
pub(crate) fn collect_scalar_subqueries<'a>(e: &'a Expr, out: &mut Vec<&'a SelectStatement>) {
    match e {
        Expr::ScalarSubquery(s) => out.push(s),
        Expr::Exists { .. } | Expr::InSubquery { .. } => {}
        Expr::Binary { lhs, rhs, .. } => {
            collect_scalar_subqueries(lhs, out);
            collect_scalar_subqueries(rhs, out);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            collect_scalar_subqueries(expr, out);
        }
        Expr::Like { expr, pattern, .. } => {
            collect_scalar_subqueries(expr, out);
            collect_scalar_subqueries(pattern, out);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_scalar_subqueries(a, out);
            }
        }
        Expr::AggregateOrdered { call, order_by, .. } => {
            collect_scalar_subqueries(call, out);
            for o in order_by {
                collect_scalar_subqueries(&o.expr, out);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(op) = operand {
                collect_scalar_subqueries(op, out);
            }
            for (w, t) in branches {
                collect_scalar_subqueries(w, out);
                collect_scalar_subqueries(t, out);
            }
            if let Some(eb) = else_branch {
                collect_scalar_subqueries(eb, out);
            }
        }
        Expr::ArraySubscript { target, index } => {
            collect_scalar_subqueries(target, out);
            collect_scalar_subqueries(index, out);
        }
        Expr::InList { expr, list, .. } => {
            collect_scalar_subqueries(expr, out);
            for item in list {
                collect_scalar_subqueries(item, out);
            }
        }
        _ => {}
    }
}

/// v7.29 (3d) — empty every scalar-subquery BODY in a host
/// expression (node kept so the splice pre-order still matches).
fn hollow_scalar_subqueries(e: &mut Expr) {
    match e {
        Expr::ScalarSubquery(s) => {
            let hollow = SelectStatement {
                items: Vec::new(),
                ..SelectStatement::default()
            };
            **s = hollow;
        }
        Expr::Exists { .. } | Expr::InSubquery { .. } => {}
        Expr::Binary { lhs, rhs, .. } => {
            hollow_scalar_subqueries(lhs);
            hollow_scalar_subqueries(rhs);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            hollow_scalar_subqueries(expr);
        }
        Expr::Like { expr, pattern, .. } => {
            hollow_scalar_subqueries(expr);
            hollow_scalar_subqueries(pattern);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args.iter_mut() {
                hollow_scalar_subqueries(a);
            }
        }
        Expr::AggregateOrdered { call, order_by, .. } => {
            hollow_scalar_subqueries(call);
            for o in order_by.iter_mut() {
                hollow_scalar_subqueries(&mut o.expr);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(op) = operand {
                hollow_scalar_subqueries(op);
            }
            for (w, t) in branches.iter_mut() {
                hollow_scalar_subqueries(w);
                hollow_scalar_subqueries(t);
            }
            if let Some(eb) = else_branch {
                hollow_scalar_subqueries(eb);
            }
        }
        Expr::ArraySubscript { target, index } => {
            hollow_scalar_subqueries(target);
            hollow_scalar_subqueries(index);
        }
        Expr::InList { expr, list, .. } => {
            hollow_scalar_subqueries(expr);
            for item in list.iter_mut() {
                hollow_scalar_subqueries(item);
            }
        }
        _ => {}
    }
}

/// v7.29 (3c) — splice the i-th scalar subquery's batched value into
/// the cloned tree (same pre-order as collect_scalar_subqueries).
/// Returns Ok(false) if a literal conversion fails (caller falls
/// back to the resolver path).
fn splice_planned_subqueries(
    e: &mut Expr,
    plan: &[Option<alloc::rc::Rc<memoize::GroupMap>>],
    idx: &mut usize,
    row: &Row,
    ctx: &EvalContext<'_>,
) -> Result<bool, EngineError> {
    match e {
        Expr::ScalarSubquery(_) => {
            let Some(Some(gm)) = plan.get(*idx) else {
                return Ok(false);
            };
            *idx += 1;
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
            Ok(true)
        }
        Expr::Exists { .. } | Expr::InSubquery { .. } => Ok(true),
        Expr::Binary { lhs, rhs, .. } => Ok(splice_planned_subqueries(lhs, plan, idx, row, ctx)?
            && splice_planned_subqueries(rhs, plan, idx, row, ctx)?),
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            splice_planned_subqueries(expr, plan, idx, row, ctx)
        }
        Expr::Like { expr, pattern, .. } => {
            Ok(splice_planned_subqueries(expr, plan, idx, row, ctx)?
                && splice_planned_subqueries(pattern, plan, idx, row, ctx)?)
        }
        Expr::FunctionCall { args, .. } => {
            for a in args.iter_mut() {
                if !splice_planned_subqueries(a, plan, idx, row, ctx)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Expr::AggregateOrdered { call, order_by, .. } => {
            if !splice_planned_subqueries(call, plan, idx, row, ctx)? {
                return Ok(false);
            }
            for o in order_by.iter_mut() {
                if !splice_planned_subqueries(&mut o.expr, plan, idx, row, ctx)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(op) = operand {
                if !splice_planned_subqueries(op, plan, idx, row, ctx)? {
                    return Ok(false);
                }
            }
            for (w, t) in branches.iter_mut() {
                if !splice_planned_subqueries(w, plan, idx, row, ctx)?
                    || !splice_planned_subqueries(t, plan, idx, row, ctx)?
                {
                    return Ok(false);
                }
            }
            if let Some(eb) = else_branch {
                if !splice_planned_subqueries(eb, plan, idx, row, ctx)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Expr::ArraySubscript { target, index } => {
            Ok(splice_planned_subqueries(target, plan, idx, row, ctx)?
                && splice_planned_subqueries(index, plan, idx, row, ctx)?)
        }
        Expr::InList { expr, list, .. } => {
            if !splice_planned_subqueries(expr, plan, idx, row, ctx)? {
                return Ok(false);
            }
            for item in list.iter_mut() {
                if !splice_planned_subqueries(item, plan, idx, row, ctx)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        _ => Ok(true),
    }
}

/// v7.30.2 (mailrs round-25) — minimum element count before an
/// all-literal `IN` list gets a per-query membership set. Below
/// this the linear scan wins on build cost.
const INLIST_SET_THRESHOLD: usize = 64;

/// Cheap pre-check: is a set-eligible `IN` list reachable on the
/// AND spine of this expression? Anything else keeps the plain
/// `eval_expr` path untouched.
fn expr_may_use_in_set(e: &Expr) -> bool {
    match e {
        Expr::InList { list, .. } => list.len() >= INLIST_SET_THRESHOLD,
        Expr::Binary {
            lhs,
            op: BinOp::And,
            rhs,
        } => expr_may_use_in_set(lhs) || expr_may_use_in_set(rhs),
        _ => false,
    }
}

/// Analyse an `IN` list for set eligibility: every element a literal,
/// all of one family (integer or string, NULLs tracked separately).
pub(crate) fn build_in_list_set(list: &[Expr]) -> Option<memoize::InListSetEntry> {
    let mut has_null = false;
    let mut ints: alloc::collections::BTreeSet<i64> = alloc::collections::BTreeSet::new();
    let mut texts: alloc::collections::BTreeSet<String> = alloc::collections::BTreeSet::new();
    for item in list {
        let Expr::Literal(lit) = item else {
            return None;
        };
        match lit {
            Literal::Null => has_null = true,
            Literal::Integer(i) => {
                ints.insert(*i);
            }
            Literal::String(s) => {
                texts.insert(s.clone());
            }
            _ => return None,
        }
        if !ints.is_empty() && !texts.is_empty() {
            return None;
        }
    }
    let set = if !ints.is_empty() {
        memoize::InListSet::Int(ints)
    } else if !texts.is_empty() {
        memoize::InListSet::Text(texts)
    } else {
        return None;
    };
    Some(memoize::InListSetEntry { set, has_null })
}

/// Subquery-free eval that serves large all-literal `IN` lists from
/// a per-query membership set (cached in the memo by node address).
/// Walks only the AND spine; every other node — and every needle
/// whose runtime family doesn't match the set — falls through to
/// `eval_expr`, so coercion and error semantics stay identical.
fn eval_with_in_sets(
    e: &Expr,
    row: &Row,
    ctx: &EvalContext<'_>,
    m: &mut memoize::MemoizeCache,
) -> Result<Value, EngineError> {
    match e {
        Expr::Binary {
            lhs,
            op: BinOp::And,
            rhs,
        } => {
            // Mirror eval_expr: both sides evaluate (no short
            // circuit), then SQL three-valued AND.
            let l = eval_with_in_sets(lhs, row, ctx, m)?;
            let r = eval_with_in_sets(rhs, row, ctx, m)?;
            eval::and_3vl(l, r).map_err(EngineError::Eval)
        }
        Expr::InList {
            expr: lhs,
            list,
            negated,
        } if list.len() >= INLIST_SET_THRESHOLD => {
            let key = core::ptr::from_ref::<Expr>(e) as usize;
            let Some(entry) = m
                .in_sets
                .entry(key)
                .or_insert_with(|| build_in_list_set(list))
            else {
                return eval::eval_expr(e, row, ctx).map_err(EngineError::Eval);
            };
            let needle = eval::eval_expr(lhs, row, ctx).map_err(EngineError::Eval)?;
            let contained = match (&needle, &entry.set) {
                // Non-empty list + NULL needle → NULL (negation of
                // NULL is still NULL).
                (Value::Null, _) => return Ok(Value::Null),
                (Value::SmallInt(n), memoize::InListSet::Int(s)) => s.contains(&i64::from(*n)),
                (Value::Int(n), memoize::InListSet::Int(s)) => s.contains(&i64::from(*n)),
                (Value::BigInt(n), memoize::InListSet::Int(s)) => s.contains(n),
                (Value::Text(t), memoize::InListSet::Text(s)) => s.contains(t.as_str()),
                // Cross-family needle (e.g. Float vs integer list):
                // keep apply_binary's coercion / error behaviour.
                _ => return eval::eval_expr(e, row, ctx).map_err(EngineError::Eval),
            };
            let inner = if contained {
                Value::Bool(true)
            } else if entry.has_null {
                Value::Null
            } else {
                Value::Bool(false)
            };
            Ok(match (negated, inner) {
                (true, Value::Bool(b)) => Value::Bool(!b),
                (_, v) => v,
            })
        }
        _ => eval::eval_expr(e, row, ctx).map_err(EngineError::Eval),
    }
}

fn substitute_outer_columns(stmt: &mut SelectStatement, row: &Row, ctx: &EvalContext<'_>) {
    // v7.24 (round-16 B) — joined outer contexts carry no single
    // table alias; their schemas use composite "alias.column" names
    // instead. Pass an unmatchable alias and let the composite
    // lookup in substitute_in_expr do the work (a correlated EXISTS
    // under a JOIN previously skipped substitution entirely and
    // died with "unknown table qualifier").
    let outer_alias = ctx.table_alias.unwrap_or("");
    substitute_in_select(stmt, row, ctx, outer_alias);
}

fn substitute_in_select(
    stmt: &mut SelectStatement,
    row: &Row,
    ctx: &EvalContext<'_>,
    outer_alias: &str,
) {
    for item in &mut stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            substitute_in_expr(expr, row, ctx, outer_alias);
        }
    }
    if let Some(w) = &mut stmt.where_ {
        substitute_in_expr(w, row, ctx, outer_alias);
    }
    if let Some(gs) = &mut stmt.group_by {
        for g in gs {
            substitute_in_expr(g, row, ctx, outer_alias);
        }
    }
    if let Some(h) = &mut stmt.having {
        substitute_in_expr(h, row, ctx, outer_alias);
    }
    for o in &mut stmt.order_by {
        substitute_in_expr(&mut o.expr, row, ctx, outer_alias);
    }
    for (_, peer) in &mut stmt.unions {
        substitute_in_select(peer, row, ctx, outer_alias);
    }
}

fn substitute_in_expr(e: &mut Expr, row: &Row, ctx: &EvalContext<'_>, outer_alias: &str) {
    // v7.25.2 (round-19 A) — bare synthetic columns. The aggregate
    // rewriter replaces group-key references INSIDE subquery bodies
    // with `__grp_N` so a correlated subquery in a GROUP BY select
    // list can resolve against the synthesised group row. The names
    // are engine-generated, so they can't shadow user columns.
    if let Expr::Column(c) = e
        && c.qualifier.is_none()
        && (c.name.starts_with("__grp_") || c.name.starts_with("__agg_"))
        && let Some(idx) = ctx.columns.iter().position(|sc| sc.name == c.name)
    {
        let v = row.values.get(idx).cloned().unwrap_or(Value::Null);
        if let Ok(lit) = value_to_literal_expr(v) {
            *e = lit;
            return;
        }
    }
    if let Expr::Column(c) = e
        && let Some(qual) = &c.qualifier
    {
        // Look up the column's index in the outer schema: plain name
        // when the qualifier is the outer table's alias, composite
        // "alias.column" for joined outer schemas (v7.24).
        let idx = if !outer_alias.is_empty() && qual.eq_ignore_ascii_case(outer_alias) {
            ctx.columns
                .iter()
                .position(|sc| sc.name.eq_ignore_ascii_case(&c.name))
        } else {
            None
        }
        .or_else(|| {
            let composite = alloc::format!("{qual}.{name}", name = c.name);
            ctx.columns
                .iter()
                .position(|sc| sc.name.eq_ignore_ascii_case(&composite))
        });
        if let Some(idx) = idx {
            let v = row.values.get(idx).cloned().unwrap_or(Value::Null);
            if let Ok(lit) = value_to_literal_expr(v) {
                *e = lit;
                return;
            }
        }
    }
    match e {
        Expr::AggregateOrdered { call, order_by, .. } => {
            substitute_in_expr(call, row, ctx, outer_alias);
            for o in order_by.iter_mut() {
                substitute_in_expr(&mut o.expr, row, ctx, outer_alias);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            substitute_in_expr(lhs, row, ctx, outer_alias);
            substitute_in_expr(rhs, row, ctx, outer_alias);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            substitute_in_expr(expr, row, ctx, outer_alias);
        }
        Expr::Like { expr, pattern, .. } => {
            substitute_in_expr(expr, row, ctx, outer_alias);
            substitute_in_expr(pattern, row, ctx, outer_alias);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                substitute_in_expr(a, row, ctx, outer_alias);
            }
        }
        Expr::Extract { source, .. } => substitute_in_expr(source, row, ctx, outer_alias),
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                substitute_in_expr(a, row, ctx, outer_alias);
            }
            for p in partition_by {
                substitute_in_expr(p, row, ctx, outer_alias);
            }
            for (o, _, _) in order_by {
                substitute_in_expr(o, row, ctx, outer_alias);
            }
        }
        Expr::ScalarSubquery(s) => substitute_in_select(s, row, ctx, outer_alias),
        Expr::Exists { subquery, .. } | Expr::InSubquery { subquery, .. } => {
            substitute_in_select(subquery, row, ctx, outer_alias);
        }
        Expr::Literal(_) | Expr::Placeholder(_) | Expr::Column(_) => {}
        Expr::Array(items) => {
            for elem in items {
                substitute_in_expr(elem, row, ctx, outer_alias);
            }
        }
        Expr::ArraySubscript { target, index } => {
            substitute_in_expr(target, row, ctx, outer_alias);
            substitute_in_expr(index, row, ctx, outer_alias);
        }
        Expr::AnyAll { expr, array, .. } => {
            substitute_in_expr(expr, row, ctx, outer_alias);
            substitute_in_expr(array, row, ctx, outer_alias);
        }
        Expr::InList { expr, list, .. } => {
            substitute_in_expr(expr, row, ctx, outer_alias);
            for item in list {
                substitute_in_expr(item, row, ctx, outer_alias);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                substitute_in_expr(o, row, ctx, outer_alias);
            }
            for (w, t) in branches {
                substitute_in_expr(w, row, ctx, outer_alias);
                substitute_in_expr(t, row, ctx, outer_alias);
            }
            if let Some(e) = else_branch {
                substitute_in_expr(e, row, ctx, outer_alias);
            }
        }
    }
}

/// Quick scan for any subquery-bearing node in a SELECT's WHERE /
/// projection / `order_by` — saves cloning the AST when there are
/// none (the common case).
pub(crate) fn expr_tree_has_subquery(stmt: &SelectStatement) -> bool {
    let mut any = false;
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            any = any || expr_has_subquery(expr);
        }
    }
    if let Some(w) = &stmt.where_ {
        any = any || expr_has_subquery(w);
    }
    if let Some(h) = &stmt.having {
        any = any || expr_has_subquery(h);
    }
    for o in &stmt.order_by {
        any = any || expr_has_subquery(&o.expr);
    }
    for (_, peer) in &stmt.unions {
        any = any || expr_tree_has_subquery(peer);
    }
    any
}

pub(crate) fn expr_has_subquery(e: &Expr) -> bool {
    match e {
        Expr::ScalarSubquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => true,
        Expr::AggregateOrdered { call, order_by, .. } => {
            expr_has_subquery(call) || order_by.iter().any(|o| expr_has_subquery(&o.expr))
        }
        Expr::Binary { lhs, rhs, .. } => expr_has_subquery(lhs) || expr_has_subquery(rhs),
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            expr_has_subquery(expr)
        }
        Expr::FunctionCall { args, .. } => args.iter().any(expr_has_subquery),
        Expr::Like { expr, pattern, .. } => expr_has_subquery(expr) || expr_has_subquery(pattern),
        Expr::Extract { source, .. } => expr_has_subquery(source),
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            args.iter().any(expr_has_subquery)
                || partition_by.iter().any(expr_has_subquery)
                || order_by.iter().any(|(e, _, _)| expr_has_subquery(e))
        }
        Expr::Literal(_) | Expr::Placeholder(_) | Expr::Column(_) => false,
        Expr::Array(items) => items.iter().any(expr_has_subquery),
        Expr::ArraySubscript { target, index } => {
            expr_has_subquery(target) || expr_has_subquery(index)
        }
        Expr::AnyAll { expr, array, .. } => expr_has_subquery(expr) || expr_has_subquery(array),
        Expr::InList { expr, list, .. } => {
            expr_has_subquery(expr) || list.iter().any(expr_has_subquery)
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            operand.as_deref().is_some_and(expr_has_subquery)
                || branches
                    .iter()
                    .any(|(w, t)| expr_has_subquery(w) || expr_has_subquery(t))
                || else_branch.as_deref().is_some_and(expr_has_subquery)
        }
    }
}
