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

use spg_sql::ast::{
    BinOp, ColumnName, Cte, Expr, FromJoin, JoinKind, LimitExpr, Literal, SelectItem,
    SelectStatement, TableRef, UnOp,
};

/// v7.37.4 — fire counter for the LIMIT 1 pullup pass. Tests inspect
/// this to confirm whether the rewrite actually triggered on a given
/// SQL shape (semantic-equivalence tests pass either way). Relaxed
/// ordering is fine: tests synchronize on full query execution.
pub static PULLUP_LIMIT1_FIRE_COUNT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// v7.37.4 A' — per-keyed-probe / per-fallback counters for the
/// batched scalar subquery resolver. Used by perf-gate instrumentation
/// to distinguish "keyed path fires but per-probe is slow" from
/// "keyed path never fires" — A' targets the former.
pub static BATCHED_SCALAR_KEYED_FIRE_COUNT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static BATCHED_SCALAR_KEYED_PROBE_COUNT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static BATCHED_SCALAR_FALL_THROUGH_COUNT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// v7.37.4 A' — EXISTS path counters. Distinguish whether mailrs
/// prod's 2-column NOT EXISTS goes through the cheap
/// `try_batch_correlated_exists` (one inner scan + per-row hash
/// probe) or the slow `pull_up_exists_sublinks` rewrite (rejects
/// multi-column correlation today). Ablation finding 2026-06-19:
/// the NOT EXISTS conjunct in `/api/conversations` costs ~165 ms
/// per 100k bench iteration — figure out which path is actually
/// being taken.
/// v7.37.7 round-2 — counts every entry into `try_pull_up_exists_sublink`.
/// Paired with `EXISTS_PULLUP_FIRE_COUNT` (which only fires on successful
/// rewrite) and `EXISTS_PULLUP_BAIL_*` (per-guard rejection) so we can
/// see WHICH guard rejects the mailrs Class B prod shape on a stress run.
pub static EXISTS_PULLUP_CANDIDATE_COUNT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
/// Bail at line 2369: inner has CTE / UNION / GROUP BY / HAVING / DISTINCT
/// / ORDER BY / LIMIT / OFFSET.
pub static EXISTS_PULLUP_BAIL_INNER_SHAPE: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
/// Bail at line 2380: inner from has joins / lateral / unnest / generate_series / as_of.
pub static EXISTS_PULLUP_BAIL_INNER_FROM: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
/// Bail at line 2405: inner has no WHERE.
pub static EXISTS_PULLUP_BAIL_NO_WHERE: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
/// Bail at line 2446: a WHERE conjunct is not `outer=inner` Eq AND not all-inner.
pub static EXISTS_PULLUP_BAIL_RESIDUAL_NOT_INNER: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
/// Bail at line 2451: no correlation pair found.
pub static EXISTS_PULLUP_BAIL_NO_CORR: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
/// Bail at line 2459: multi-col + EXISTS_PULLUP_MULTICOL_DISABLE knob.
pub static EXISTS_PULLUP_BAIL_MULTICOL_DISABLED: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
/// Bail at line 2475: positive EXISTS + inner key not single-col UNIQUE.
pub static EXISTS_PULLUP_BAIL_UNIQUE_KEY_MISSING: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static EXISTS_PULLUP_FIRE_COUNT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static EXISTS_BATCH_FIRE_COUNT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static EXISTS_BATCH_FALL_THROUGH_COUNT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// v7.37.4 A'' — differential knob. When true, the multi-column
/// branch of `try_pull_up_exists_sublink` rejects (falling back to
/// the v7.34.2 batch resolver path); single-column EXISTS pullup
/// still fires. Lets the differential e2e prove byte-equal results
/// between the new pullup path and the legacy batch path. Default
/// false — production never sets this.
pub static EXISTS_PULLUP_MULTICOL_DISABLE: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

use spg_storage::{Row, Value};

use crate::eval::{self, EvalContext};
use crate::substitute::value_to_literal_expr;
use crate::{
    CancelToken, Engine, EngineError, QueryResult, aggregate, memoize, order_by_value_cmp, reorder,
    value_cmp, visit_expr_columns_and_subqueries,
};

/// Build the boolean expression for `(row) <op> (rhs)`, mirroring the
/// parser's literal-row lowering: `=` is an AND of per-column equalities,
/// `<>` its negation, and the ordering operators lower to the standard
/// lexicographic `a<x OR (a=x AND (b<y OR …))` form. Evaluating the result
/// carries SQL three-valued logic for free (NULL propagates through
/// `=` / `<` / AND / OR / NOT). Used to resolve `RowCmpSubquery` once the
/// subquery's single row is known.
/// v7.39 (round 341, V66) — a scalar subquery must project exactly ONE
/// column. Nothing checked, so `SELECT (SELECT a, b FROM t LIMIT 1)`
/// silently answered the FIRST column where PG 18.4 raises
/// `subquery must return only one column` — a wrong answer, not a
/// missing feature. Zero columns became reachable in this round
/// (PG allows an empty target list), which is what surfaced it.
fn scalar_subquery_arity(ncols: usize) -> Result<(), EngineError> {
    if ncols == 1 {
        Ok(())
    } else {
        Err(EngineError::Unsupported(
            "subquery must return only one column".into(),
        ))
    }
}

fn build_row_comparison(row: &[Expr], op: spg_sql::ast::BinOp, rhs: &[Expr]) -> Expr {
    use alloc::boxed::Box;
    use spg_sql::ast::{BinOp, UnOp};
    fn row_eq(lhs: &[Expr], rhs: &[Expr]) -> Expr {
        let mut it = lhs.iter().zip(rhs.iter()).map(|(l, r)| Expr::Binary {
            lhs: Box::new(l.clone()),
            op: BinOp::Eq,
            rhs: Box::new(r.clone()),
        });
        let first = it.next().expect("row has >= 1 element");
        it.fold(first, |acc, e| Expr::Binary {
            lhs: Box::new(acc),
            op: BinOp::And,
            rhs: Box::new(e),
        })
    }
    fn row_lex(lhs: &[Expr], rhs: &[Expr], strict: BinOp, last: BinOp) -> Expr {
        if lhs.len() == 1 {
            return Expr::Binary {
                lhs: Box::new(lhs[0].clone()),
                op: last,
                rhs: Box::new(rhs[0].clone()),
            };
        }
        let head_strict = Expr::Binary {
            lhs: Box::new(lhs[0].clone()),
            op: strict,
            rhs: Box::new(rhs[0].clone()),
        };
        let head_eq = Expr::Binary {
            lhs: Box::new(lhs[0].clone()),
            op: BinOp::Eq,
            rhs: Box::new(rhs[0].clone()),
        };
        Expr::Binary {
            lhs: Box::new(head_strict),
            op: BinOp::Or,
            rhs: Box::new(Expr::Binary {
                lhs: Box::new(head_eq),
                op: BinOp::And,
                rhs: Box::new(row_lex(&lhs[1..], &rhs[1..], strict, last)),
            }),
        }
    }
    match op {
        BinOp::Eq => row_eq(row, rhs),
        BinOp::NotEq => Expr::Unary {
            op: UnOp::Not,
            expr: Box::new(row_eq(row, rhs)),
        },
        BinOp::Lt => row_lex(row, rhs, BinOp::Lt, BinOp::Lt),
        BinOp::LtEq => row_lex(row, rhs, BinOp::Lt, BinOp::LtEq),
        BinOp::Gt => row_lex(row, rhs, BinOp::Gt, BinOp::Gt),
        BinOp::GtEq => row_lex(row, rhs, BinOp::Gt, BinOp::GtEq),
        _ => Expr::Literal(Literal::Bool(false)), // parser restricts op to the six above
    }
}

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
        row: &Row<'static>,
        ctx: &EvalContext<'_>,
        cancel: CancelToken<'_>,
        mut memo: Option<&mut memoize::MemoizeCache>,
    ) -> Result<Value<'static>, EngineError> {
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
            let exists_plan_hit = m.exists_plans.contains_key(&key);
            let mut subs: Vec<&SelectStatement> = Vec::new();
            let mut exists_subs: Vec<&SelectStatement> = Vec::new();
            if !plan_hit {
                collect_scalar_subqueries(expr, &mut subs);
            }
            if !exists_plan_hit {
                collect_exists_subqueries(expr, &mut exists_subs);
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
            // v7.34.2 — parallel EXISTS plan. Walk host ONCE in pre-order,
            // build a decorrelated key-set for each EXISTS subquery via
            // `try_batch_correlated_exists`, and cache the vec by host_ptr.
            // Per-row dispatch below uses `splice_planned_exists` which
            // increments an ordinal cursor — no `alloc::format!` per row.
            if !exists_plan_hit && !exists_subs.is_empty() {
                let mut eplan: Vec<Option<alloc::rc::Rc<memoize::ExistsSet>>> =
                    Vec::with_capacity(exists_subs.len());
                for sub in &exists_subs {
                    let built = self
                        .try_batch_correlated_exists(sub, cancel)?
                        .map(alloc::rc::Rc::new);
                    if built.is_some() {
                        EXISTS_BATCH_FIRE_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    } else {
                        EXISTS_BATCH_FALL_THROUGH_COUNT
                            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    }
                    eplan.push(built);
                }
                m.exists_plans.insert(key, eplan);
            }
            // Fast-path gate: take it if we have a planned scalar set, a
            // planned EXISTS set, or both — anything that lets us skip
            // the per-row `expr.clone()` + `resolve_correlated_in_expr`
            // dispatch for the corresponding subquery class.
            let scalar_ready = m
                .expr_plans
                .get(&key)
                .map(|(_, plan, _)| !plan.is_empty() && plan.iter().all(|p| p.is_some()))
                .unwrap_or(false);
            let exists_ready = m
                .exists_plans
                .get(&key)
                .map(|plan| !plan.is_empty() && plan.iter().all(|p| p.is_some()))
                .unwrap_or(false);
            if scalar_ready || exists_ready {
                // Fast path: every planned subquery resolves via its
                // map; clone the (hollowed-where-scalar) template,
                // splice map values, eval. EXISTS bodies are NOT
                // hollowed (we don't traverse into them during splice —
                // `splice_planned_exists` consumes the EXISTS node
                // wholesale), so cloning the original `expr` works for
                // the EXISTS-only path.
                let scalar_plan = m
                    .expr_plans
                    .get(&key)
                    .map(|(_, plan, template)| (plan.clone(), template.clone()));
                let exists_plan = m.exists_plans.get(&key).cloned();
                let mut e = match &scalar_plan {
                    Some((_, template)) => template.clone(),
                    None => expr.clone(),
                };
                let mut all_ok = true;
                if let Some((plan, _)) = &scalar_plan {
                    let mut idx = 0usize;
                    all_ok &= splice_planned_subqueries(&mut e, plan, &mut idx, row, ctx)?;
                }
                if all_ok && let Some(plan) = &exists_plan {
                    let mut idx = 0usize;
                    all_ok &= splice_planned_exists(&mut e, plan, &mut idx, row, ctx)?;
                }
                if all_ok {
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

    /// Quantified `op ANY / ALL (SELECT …)` — materialise every
    /// row of the single-column subquery into an ARRAY[…] literal
    /// the existing AnyAll three-valued eval consumes (empty
    /// result → empty array: ANY false, ALL true — PG semantics).
    pub(crate) fn materialize_quantified_rows(
        &self,
        inner: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<Expr, EngineError> {
        let r = self.exec_select_cancel(inner, cancel)?;
        let QueryResult::Rows { rows, .. } = r else {
            return Err(EngineError::Unsupported(
                "ANY/ALL subquery: inner did not return rows".into(),
            ));
        };
        let mut items = alloc::vec::Vec::with_capacity(rows.len());
        for r0 in rows {
            let v = r0.values.into_iter().next().unwrap_or(Value::Null);
            items.push(value_to_literal_expr(v)?);
        }
        Ok(Expr::Array(items))
    }

    fn resolve_correlated_in_expr(
        &self,
        e: &mut Expr,
        row: &Row<'static>,
        ctx: &EvalContext<'_>,
        cancel: CancelToken<'_>,
        mut memo: Option<&mut memoize::MemoizeCache>,
    ) -> Result<(), EngineError> {
        match e {
            Expr::NamedArg { expr, .. } | Expr::Variadic(expr) => {
                self.resolve_correlated_in_expr(expr, row, ctx, cancel, memo.as_deref_mut())?;
            }
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
                // v7.37.x (docker-fair SCALARSQ attack) — pointer-keyed
                // fast cache. The inner SelectStatement is stable for
                // the duration of the query, so its address makes a
                // unique key that costs nothing to compute (vs
                // `alloc::format!("{}", inner)` ~ 500 ns × N outer
                // rows of pure repr churn).
                if memo.is_some() {
                    let ptr_key = core::ptr::from_ref::<SelectStatement>(&**inner) as usize;
                    let entry_known = memo
                        .as_ref()
                        .is_some_and(|m| m.group_maps_by_ptr.contains_key(&ptr_key));
                    if !entry_known {
                        let built = self
                            .try_batch_correlated_scalar(inner, None, cancel)?
                            .map(alloc::rc::Rc::new);
                        if let Some(m) = memo.as_deref_mut() {
                            m.group_maps_by_ptr.insert(ptr_key, built);
                        }
                    }
                    if let Some(m) = memo.as_deref_mut()
                        && let Some(Some(gm)) = m.group_maps_by_ptr.get(&ptr_key)
                    {
                        let (outer_col, map, empty_default) = gm.as_ref();
                        let key_v = eval::eval_expr(&Expr::Column(outer_col.clone()), row, ctx)
                            .map_err(EngineError::Eval)?;
                        // v7.37.x — scalar subquery empty-set semantics:
                        // `COUNT(*)` / `COUNT(col)` over no rows = 0,
                        // every other aggregate = NULL. The batched
                        // GroupMap omits keys whose inner-table partition
                        // was empty; treat such misses as the per-
                        // aggregate empty-default.
                        let v = if matches!(key_v, Value::Null) {
                            Value::Null
                        } else {
                            map.get(&aggregate::encode_key(core::slice::from_ref(&key_v)))
                                .cloned()
                                .unwrap_or_else(|| empty_default.clone())
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
                    outer_values: row.values.iter().cloned().map(Value::into_owned).collect(),
                });
                if let (Some(cache), Some(k)) = (memo.as_deref_mut(), cache_key.as_ref())
                    && let Some(cached) = cache.get(k)
                {
                    *e = value_to_literal_expr(cached)?;
                    return Ok(());
                }
                // v7.37.x (docker-fair SCALARSQ attack) — direct PK probe
                // fast path. The shape
                //   (SELECT COUNT(*) FROM T WHERE T.pk = outer.col)
                // — common SCALARSQ shape and what the docker-fair
                // SCALARSQ benchmark exercises — is a 1-bit lookup:
                // the probe either finds 1 row or 0. Skip
                // `exec_select_cancel`'s parse / resolve / plan /
                // aggregate roundtrip; do an index seek on T.pk
                // directly and return `Int(0)` or `Int(1)`. PG with a
                // cached prepared plan does roughly this; SCALARSQ
                // drops from per-row ~3 µs to per-row ~100 ns.
                if let Some(v) = self.try_scalar_count_pk_eq_probe(inner, row, ctx)? {
                    *e = value_to_literal_expr(v)?;
                    return Ok(());
                }
                let mut s = (**inner).clone();
                substitute_outer_columns(&mut s, row, ctx);
                let r = self.exec_select_cancel(&s, cancel)?;
                let QueryResult::Rows { columns, rows, .. } = r else {
                    return Err(EngineError::Unsupported(
                        "scalar subquery: inner did not return rows".into(),
                    ));
                };
                scalar_subquery_arity(columns.len())?;
                let value = match rows.as_slice() {
                    [] => Value::Null,
                    [r0] => r0.values.first().cloned().unwrap_or(Value::Null),
                    _ => {
                        return Err(EngineError::CardinalityViolation);
                    }
                };
                if let (Some(cache), Some(k)) = (memo.as_deref_mut(), cache_key) {
                    cache.insert(k, value.clone());
                }
                *e = value_to_literal_expr(value)?;
            }
            Expr::Exists { subquery, negated } => {
                // v7.34 (mailrs conn-pool P0) — semi/anti-join batch path
                // first: a correlated `[NOT] EXISTS` of the
                // `inner.k = outer.col [AND inner-preds]` shape builds its
                // inner key-set ONCE (keyed by repr in the per-query memo);
                // per-row resolution becomes a membership test. 24k per-row
                // inner executions became one scan + 24k lookups.
                if memo.is_some() {
                    let repr = alloc::format!("{}", **subquery);
                    let known = memo
                        .as_ref()
                        .is_some_and(|m| m.exists_sets.contains_key(&repr));
                    if !known {
                        let built = self
                            .try_batch_correlated_exists(subquery, cancel)?
                            .map(alloc::rc::Rc::new);
                        if let Some(m) = memo.as_deref_mut() {
                            m.exists_sets.insert(repr.clone(), built);
                        }
                    }
                    if let Some(m) = memo.as_deref_mut()
                        && let Some(Some(es)) = m.exists_sets.get(&repr)
                    {
                        let (outer_cols, set) = es.as_ref();
                        let mut key_vals: Vec<Value<'static>> =
                            Vec::with_capacity(outer_cols.len());
                        let mut any_null = false;
                        for oc in outer_cols {
                            let v = eval::eval_expr(&Expr::Column(oc.clone()), row, ctx)
                                .map_err(EngineError::Eval)?;
                            if matches!(v, Value::Null) {
                                any_null = true;
                            }
                            key_vals.push(v);
                        }
                        // NULL key component → never matches → not present.
                        let present = !any_null && set.contains(&aggregate::encode_key(&key_vals));
                        let bit = if *negated { !present } else { present };
                        *e = Expr::Literal(Literal::Bool(bit));
                        return Ok(());
                    }
                }
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
                    // v7.39 (round 341, V66) — PG's two wordings, measured
                    // on 18.4: `subquery has too few columns` /
                    // `subquery has too many columns`. SPG named its own
                    // internal shape ("IN-subquery must project exactly
                    // one column; got 0").
                    return Err(EngineError::Unsupported(
                        if columns.is_empty() {
                            "subquery has too few columns"
                        } else {
                            "subquery has too many columns"
                        }
                        .into(),
                    ));
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
                if !found && any_null {
                    // SQL three-valued logic: no match but the IN-list held a
                    // NULL → the predicate is UNKNOWN (NULL), not false. This is
                    // the classic `x NOT IN (… NULL …)` gotcha — every non-match
                    // row evaluates to NULL and is filtered. PG-verified.
                    *e = Expr::Literal(Literal::Null);
                    return Ok(());
                }
                let bit = if found { !*negated } else { *negated };
                *e = Expr::Literal(Literal::Bool(bit));
            }
            Expr::RowInSubquery {
                row: row_exprs,
                subquery,
                negated,
            } => {
                // `(a, b, …) [NOT] IN (SELECT x, y, …)` with PG's row
                // three-valued logic: the result is OR over subquery rows
                // of the per-row AND of column equalities. A row is a
                // definite mismatch as soon as one column is unequal (both
                // non-NULL); if no column is definitely unequal but some
                // comparison involved a NULL, that row is UNKNOWN. So the
                // predicate is TRUE if any row fully matches, else NULL if
                // any row was UNKNOWN, else FALSE.
                for el in row_exprs.iter_mut() {
                    self.resolve_correlated_in_expr(el, row, ctx, cancel, memo.as_deref_mut())?;
                }
                let lhs_vals: Vec<Value> = row_exprs
                    .iter()
                    .map(|el| eval::eval_expr(el, row, ctx).map_err(EngineError::Eval))
                    .collect::<Result<_, _>>()?;
                let mut s = (**subquery).clone();
                substitute_outer_columns(&mut s, row, ctx);
                let r = self.exec_select_cancel(&s, cancel)?;
                let QueryResult::Rows { columns, rows, .. } = r else {
                    return Err(EngineError::Unsupported(
                        "row IN-subquery: inner did not return rows".into(),
                    ));
                };
                if columns.len() != lhs_vals.len() {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "row IN-subquery: left side has {} column(s), subquery returns {}",
                        lhs_vals.len(),
                        columns.len()
                    )));
                }
                let mut found = false;
                let mut any_null = false;
                'rows: for r0 in rows {
                    let mut has_null = false;
                    for (j, sub_v) in r0.values.iter().enumerate() {
                        let lv = &lhs_vals[j];
                        if lv.is_null() || sub_v.is_null() {
                            has_null = true;
                        } else if value_cmp(lv, sub_v) != core::cmp::Ordering::Equal {
                            continue 'rows; // one column unequal → row is FALSE
                        }
                    }
                    if has_null {
                        any_null = true; // all non-NULL columns matched → UNKNOWN
                    } else {
                        found = true; // full definite match
                        break;
                    }
                }
                if !found && any_null {
                    *e = Expr::Literal(Literal::Null);
                    return Ok(());
                }
                let bit = if found { !*negated } else { *negated };
                *e = Expr::Literal(Literal::Bool(bit));
            }
            Expr::RowCmpSubquery {
                row: row_exprs,
                op,
                subquery,
            } => {
                // `(a, b, …) <op> (correlated SELECT)` — run the subquery for
                // this outer row, then compare the tuple. Zero rows → NULL
                // (PG scalar-subquery rule); more than one row is an error.
                for el in row_exprs.iter_mut() {
                    self.resolve_correlated_in_expr(el, row, ctx, cancel, memo.as_deref_mut())?;
                }
                let mut s = (**subquery).clone();
                substitute_outer_columns(&mut s, row, ctx);
                let r = self.exec_select_cancel(&s, cancel)?;
                let QueryResult::Rows {
                    columns, mut rows, ..
                } = r
                else {
                    return Err(EngineError::Unsupported(
                        "row comparison subquery: inner did not return rows".into(),
                    ));
                };
                if rows.is_empty() {
                    *e = Expr::Literal(Literal::Null);
                    return Ok(());
                }
                if rows.len() > 1 {
                    return Err(EngineError::CardinalityViolation);
                }
                if columns.len() != row_exprs.len() {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "row comparison: left side has {} column(s), subquery returns {}",
                        row_exprs.len(),
                        columns.len()
                    )));
                }
                let rhs: Vec<Expr> = rows
                    .remove(0)
                    .values
                    .into_iter()
                    .map(value_to_literal_expr)
                    .collect::<Result<_, _>>()?;
                let cmp = build_row_comparison(row_exprs, *op, &rhs);
                let v = eval::eval_expr(&cmp, row, ctx).map_err(EngineError::Eval)?;
                *e = value_to_literal_expr(v)?;
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.resolve_correlated_in_expr(lhs, row, ctx, cancel, memo.as_deref_mut())?;
                self.resolve_correlated_in_expr(rhs, row, ctx, cancel, memo.as_deref_mut())?;
            }
            Expr::Unary { expr, .. }
            | Expr::Cast { expr, .. }
            | Expr::IsNull { expr, .. }
            | Expr::BoolTest { expr, .. }
            | Expr::FieldAccess { base: expr, .. } => {
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
            Expr::ArraySlice { target, lo, hi } => {
                self.resolve_correlated_in_expr(target, row, ctx, cancel, memo.as_deref_mut())?;
                if let Some(l) = lo {
                    self.resolve_correlated_in_expr(l, row, ctx, cancel, memo.as_deref_mut())?;
                }
                if let Some(h) = hi {
                    self.resolve_correlated_in_expr(h, row, ctx, cancel, memo.as_deref_mut())?;
                }
            }
            Expr::AnyAll { expr, array, .. } => {
                self.resolve_correlated_in_expr(expr, row, ctx, cancel, memo.as_deref_mut())?;
                // Quantified subquery — substitute the outer row's
                // values and materialise all rows into an ARRAY.
                if let Expr::ScalarSubquery(inner) = array.as_mut() {
                    let mut s = (**inner).clone();
                    substitute_outer_columns(&mut s, row, ctx);
                    **array = self.materialize_quantified_rows(&s, cancel)?;
                } else {
                    self.resolve_correlated_in_expr(array, row, ctx, cancel, memo.as_deref_mut())?;
                }
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
                let r = match self.exec_select_cancel(&s, cancel) {
                    Ok(r) => r,
                    Err(e) if is_correlation_error(&e) => return Ok(None),
                    Err(e) => return Err(e),
                };
                let QueryResult::Rows { columns, rows, .. } = r else {
                    return Err(EngineError::Unsupported(
                        "scalar subquery: inner statement did not return rows".into(),
                    ));
                };
                scalar_subquery_arity(columns.len())?;
                let value = match rows.as_slice() {
                    [] => Value::Null,
                    [row] => row.values.first().cloned().unwrap_or(Value::Null),
                    _ => {
                        return Err(EngineError::CardinalityViolation);
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
                let r = match self.exec_select_cancel(&s, cancel) {
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
                let r = match self.exec_select_cancel(&s, cancel) {
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
                    // v7.39 (round 341, V66) — PG's two wordings, measured
                    // on 18.4: `subquery has too few columns` /
                    // `subquery has too many columns`. SPG named its own
                    // internal shape ("IN-subquery must project exactly
                    // one column; got 0").
                    return Err(EngineError::Unsupported(
                        if columns.is_empty() {
                            "subquery has too few columns"
                        } else {
                            "subquery has too many columns"
                        }
                        .into(),
                    ));
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
            Expr::RowInSubquery {
                row,
                subquery,
                negated,
            } => {
                use alloc::boxed::Box;
                // Correlated → per-row `resolve_correlated_in_expr` handles
                // it; leave the node in place.
                if select_is_correlated(subquery) {
                    return Ok(None);
                }
                let mut s = (**subquery).clone();
                self.resolve_select_subqueries(&mut s, cancel)?;
                let r = match self.exec_select_cancel(&s, cancel) {
                    Ok(r) => r,
                    Err(e) if is_correlation_error(&e) => return Ok(None),
                    Err(e) => return Err(e),
                };
                let QueryResult::Rows { columns, rows, .. } = r else {
                    return Err(EngineError::Unsupported(
                        "row IN-subquery: inner statement did not return rows".into(),
                    ));
                };
                if columns.len() != row.len() {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "row IN-subquery: left side has {} column(s), subquery returns {}",
                        row.len(),
                        columns.len()
                    )));
                }
                // Uncorrelated: the subquery's rows are now constants, so fold
                // to `(a=r1c1 AND …) OR (a=r2c1 AND …) …`. This defers the
                // left row's evaluation to the per-row loop and reproduces
                // PG's row-IN three-valued logic for free (`=` / AND / OR all
                // propagate NULL). An empty result is `false`.
                let mut alts: Vec<Expr> = Vec::with_capacity(rows.len());
                for r0 in rows {
                    let mut conj: Option<Expr> = None;
                    for (lhs_el, v) in row.iter().zip(r0.values) {
                        let eq = Expr::Binary {
                            lhs: Box::new(lhs_el.clone()),
                            op: BinOp::Eq,
                            rhs: Box::new(value_to_literal_expr(v)?),
                        };
                        conj = Some(match conj {
                            None => eq,
                            Some(prev) => Expr::Binary {
                                lhs: Box::new(prev),
                                op: BinOp::And,
                                rhs: Box::new(eq),
                            },
                        });
                    }
                    if let Some(c) = conj {
                        alts.push(c);
                    }
                }
                let combined = match alts.into_iter().reduce(|acc, e| Expr::Binary {
                    lhs: Box::new(acc),
                    op: BinOp::Or,
                    rhs: Box::new(e),
                }) {
                    Some(c) => c,
                    None => Expr::Literal(Literal::Bool(false)),
                };
                let result = if *negated {
                    Expr::Unary {
                        op: UnOp::Not,
                        expr: Box::new(combined),
                    }
                } else {
                    combined
                };
                Ok(Some(result))
            }
            Expr::RowCmpSubquery { row, op, subquery } => {
                if select_is_correlated(subquery) {
                    return Ok(None);
                }
                let mut s = (**subquery).clone();
                self.resolve_select_subqueries(&mut s, cancel)?;
                let r = match self.exec_select_cancel(&s, cancel) {
                    Ok(r) => r,
                    Err(e) if is_correlation_error(&e) => return Ok(None),
                    Err(e) => return Err(e),
                };
                let QueryResult::Rows {
                    columns, mut rows, ..
                } = r
                else {
                    return Err(EngineError::Unsupported(
                        "row comparison subquery: inner statement did not return rows".into(),
                    ));
                };
                // Zero rows → NULL (scalar-subquery rule); >1 rows is an error.
                if rows.is_empty() {
                    return Ok(Some(Expr::Literal(Literal::Null)));
                }
                if rows.len() > 1 {
                    return Err(EngineError::CardinalityViolation);
                }
                if columns.len() != row.len() {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "row comparison: left side has {} column(s), subquery returns {}",
                        row.len(),
                        columns.len()
                    )));
                }
                let rhs: Vec<Expr> = rows
                    .remove(0)
                    .values
                    .into_iter()
                    .map(value_to_literal_expr)
                    .collect::<Result<_, _>>()?;
                // Defer the left row's evaluation to the row loop by returning
                // the built comparison expression (its 3VL is correct).
                Ok(Some(build_row_comparison(row, *op, &rhs)))
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
        restrict: Option<(&[Row<'static>], &EvalContext<'_>)>,
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
        if let Some(le) = &inner.limit
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
        // v7.37.x (docker-fair SCALARSQ-aggregate path) — when the
        // inner output expression is an aggregate (e.g. `COUNT(*)`
        // for the `(SELECT COUNT(*) FROM inner WHERE inner.k =
        // outer.k)` scalar subquery shape), the batch query
        // `SELECT inner.k, COUNT(*) FROM inner` is invalid SQL
        // without `GROUP BY inner.k`. Inject the GROUP BY so the
        // aggregate executor produces (key → count) pairs, matching
        // the per-key scalar-subquery semantics. Pre-7.37.x this
        // case mis-executed as a single anonymous group and either
        // returned a wrong total or surfaced an `UnknownQualifier`
        // (when the rewriter couldn't bind the bare column ref).
        if aggregate::contains_aggregate(out_expr) {
            batch.group_by = Some(alloc::vec![Expr::Column(inner_col.clone())]);
        }
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
        let keyed: Option<(&[Row<'static>], &EvalContext<'_>)> =
            restrict.and_then(|(rows, rctx)| {
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
            BATCHED_SCALAR_KEYED_FIRE_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            // v7.37.4 A' — collect the deduped surviving correlation
            // keys, then issue ONE `inner.k IN (lit1, …, litN)` probe
            // instead of N separate `inner.k = lit` probes. The v7.34.3
            // IN-list seek path treats the literal list as a bitmap-
            // style index sweep (single index lookup per literal,
            // unioned), so the total cost is O(N seeks + matched rows)
            // — same asymptotic as the N-probe loop but without N
            // rounds of stmt clone + plan + executor stack overhead.
            //
            // Per-probe overhead measured on mailrs prod 100k:
            //   - sequential: 50 probes × ~1.7 ms = ~85 ms per subq
            //   - 3 subqueries × ~85 ms = ~255 ms of the 388 ms total
            // IN-list batched probe is one stmt + N IN-list literals,
            // amortising the plan + setup over all keys.
            let mut seen: alloc::collections::BTreeSet<String> =
                alloc::collections::BTreeSet::new();
            let mut key_lits: Vec<Expr> = Vec::new();
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
                key_lits.push(value_to_literal_expr(kv)?);
            }
            if key_lits.is_empty() {
                Vec::new()
            } else {
                let in_pred = Expr::InList {
                    expr: alloc::boxed::Box::new(Expr::Column(inner_col.clone())),
                    list: key_lits,
                    negated: false,
                };
                let mut probe = batch.clone();
                probe.where_ = Some(match probe.where_.take() {
                    Some(w) => Expr::Binary {
                        lhs: alloc::boxed::Box::new(w),
                        op: BinOp::And,
                        rhs: alloc::boxed::Box::new(in_pred),
                    },
                    None => in_pred,
                });
                BATCHED_SCALAR_KEYED_PROBE_COUNT
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                if let QueryResult::Rows { rows, .. } = self.exec_select_cancel(&probe, cancel)? {
                    rows
                } else {
                    Vec::new()
                }
            }
        } else {
            BATCHED_SCALAR_FALL_THROUGH_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
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
        // v7.37.x (docker-fair SCALARSQ attack) — empty-default per
        // PG scalar-subquery aggregate semantics. Captured here so the
        // splice path doesn't have to re-introspect a possibly-hollowed
        // inner template.
        let empty_default = scalar_subquery_empty_default(inner);
        Ok(Some((outer_col, map, empty_default)))
    }
}

impl Engine {
    /// v7.34 (mailrs conn-pool-exhaustion P0) — decorrelate a correlated
    /// `[NOT] EXISTS` into a hash semi/anti-join. Recognise
    ///   EXISTS (SELECT … FROM t [joins]
    ///           WHERE k1 = o1 AND … AND kN = oN AND <inner-preds>)
    /// run the inner ONCE without the correlation, collect the set of
    /// inner key-tuples `(k1,…,kN)` that satisfy the inner-preds; an outer
    /// row's EXISTS then reduces to a membership test on `(o1,…,oN)`. The
    /// reported `count_unseen` ran two correlated `NOT EXISTS` per ~24k
    /// join survivors (~48k inner executions, 98.7% of a 1.4 s query);
    /// this turns each into one scan + 24k lookups.
    ///
    /// Multi-column correlation is supported (the prod `snoozed` anti-join
    /// correlates on both `thread_id` and `account_address`). NULL is
    /// exact: an outer key with any NULL component is never present
    /// (`NULL = k` is never true), so EXISTS=false / NOT EXISTS=true,
    /// identical to the per-row resolver. Returns None when the shape
    /// doesn't qualify — the caller falls back to per-row execution, so
    /// there is no regression.
    pub(crate) fn try_batch_correlated_exists(
        &self,
        inner: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<Option<memoize::ExistsSet>, EngineError> {
        use spg_sql::ast::SelectItem as SI;
        if !inner.ctes.is_empty()
            || !inner.unions.is_empty()
            || inner.group_by.is_some()
            || inner.having.is_some()
            || inner.distinct
        {
            return Ok(None);
        }
        let Some(from) = &inner.from else {
            return Ok(None);
        };
        if from.primary.lateral_subquery.is_some()
            || from.primary.unnest_expr.is_some()
            || from.primary.generate_series_args.is_some()
            || from.primary.as_of_segment.is_some()
        {
            return Ok(None);
        }
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
                None => c.name.starts_with("__grp_") || c.name.starts_with("__agg_"),
            }
        };
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
        let mut inner_keys: Vec<spg_sql::ast::ColumnName> = Vec::new();
        let mut outer_cols: Vec<spg_sql::ast::ColumnName> = Vec::new();
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
                if let Some((ic, oc)) = pair {
                    inner_keys.push(ic);
                    outer_cols.push(oc);
                    continue;
                }
            }
            // A non-correlation conjunct must be purely inner (carried
            // into the build scan). Anything else (outer-only filter,
            // mixed expression) is beyond this rewrite.
            if !all_inner(c) {
                return Ok(None);
            }
            rest.push(c);
        }
        if inner_keys.is_empty() {
            return Ok(None); // uncorrelated — materialised elsewhere
        }
        // Build: SELECT k1,…,kN FROM <inner from> WHERE <rest> — no
        // correlation, no order/limit. The inner relation may be a join;
        // exec handles it.
        let mut batch = inner.clone();
        batch.limit = None;
        batch.offset = None;
        batch.order_by = Vec::new();
        batch.distinct = false;
        batch.where_ = rest
            .iter()
            .map(|e| (*e).clone())
            .reduce(|a, b| Expr::Binary {
                lhs: alloc::boxed::Box::new(a),
                op: BinOp::And,
                rhs: alloc::boxed::Box::new(b),
            });
        batch.items = inner_keys
            .iter()
            .map(|c| SI::Expr {
                expr: Expr::Column(c.clone()),
                alias: None,
            })
            .collect();
        let r = self.exec_select_cancel(&batch, cancel)?;
        let QueryResult::Rows { rows, .. } = r else {
            return Ok(None);
        };
        let n = inner_keys.len();
        let mut set: alloc::collections::BTreeSet<String> = alloc::collections::BTreeSet::new();
        for row in rows {
            let keys = row.values.get(..n).unwrap_or(&row.values);
            // A NULL key component can never satisfy `k = outer`, so the
            // tuple matches no outer row — drop it from the set.
            if keys.iter().any(|v| matches!(v, Value::Null)) {
                continue;
            }
            set.insert(aggregate::encode_key(keys));
        }
        Ok(Some((outer_cols, set)))
    }
}

impl Engine {
    /// v7.33 (mailrs 7.32.1) — sublink pull-up for aggregate-wrapped
    /// correlated scalar subqueries. Rewrite
    ///   AGG( (SELECT j_col FROM t j WHERE j.key = outer.col [AND inner preds]) )
    /// into a LEFT JOIN plus a plain column reference:
    ///   AGG(j.j_col) … LEFT JOIN t AS j ON j.key = outer.col [AND inner preds]
    /// when `t.key` carries a single-column UNIQUE / PRIMARY KEY constraint.
    /// That constraint guarantees the join matches AT MOST ONE inner row
    /// per outer row, which is exactly the scalar subquery's at-most-one
    /// contract (NULL on no match), so the aggregate folds an identical
    /// per-row value stream — only now the executor streams one join
    /// instead of splicing a per-row subplan (the R31 path cloned a hollow
    /// template per outer row: ~24k clones for the mailrs conversation
    /// aggregation).
    ///
    /// Scoped tightly for safety: the subquery must sit inside an aggregate
    /// argument (so the joined column is always folded, never a bare
    /// select-list column a GROUP BY would reject); the inner must be a
    /// single plain-table scan projecting one inner column with exactly one
    /// `inner.key = outer.col` correlation (both qualified) plus optional
    /// all-inner predicates; and the select list must have no bare wildcard
    /// (a join would widen `*`). Anything else is left for the existing
    /// per-row / batch resolver. Returns true when it rewrote at least one.
    /// v7.37.4 (A — correlated LIMIT 1 ORDER BY DESC subquery pullup) —
    /// plan-time rewrite of the "per-key latest" select-list scalar
    /// subquery pattern:
    ///
    /// ```sql
    /// SELECT outer.k,
    ///        (SELECT proj_expr FROM inner
    ///          WHERE inner.k = outer.k AND <non_corr_preds>
    ///          ORDER BY sort_key DESC LIMIT 1) AS latest_proj
    ///   FROM outer
    /// ```
    ///
    /// becomes (semantically equivalent, executor-friendly):
    ///
    /// ```sql
    /// WITH __cl1_N AS (
    ///   SELECT inner.k AS jk,
    ///          (array_agg(proj_expr ORDER BY sort_key DESC NULLS LAST))[1] AS pj
    ///     FROM <inner.from>
    ///    WHERE <non_corr_preds>
    ///    GROUP BY inner.k
    /// )
    /// SELECT outer.k, MAX(__cl1_N.pj) AS latest_proj
    ///   FROM outer LEFT JOIN __cl1_N ON __cl1_N.jk = outer.k
    /// ```
    ///
    /// The CTE materialises once for the whole outer scan; LEFT JOIN
    /// on the GROUP-BY-unique `jk` column never multiplies outer rows.
    /// The `array_agg(... ORDER BY ...)[1]` form reuses the v7.33
    /// `first_ordered` argmax executor (per-group keep the first row,
    /// no array build).
    ///
    /// Common shape across inbox / feed / timeline applications:
    /// thread latest message, user latest transaction, device latest
    /// heartbeat. **Not a mailrs-specific patch** — any client query
    /// in this shape gets the rewrite.
    ///
    /// Acceptance (`try_pull_up_limit_one`):
    /// - inner: single SELECT, LIMIT 1 + ORDER BY <expr>, no GROUP BY /
    ///   HAVING / DISTINCT / CTE / UNION / OFFSET, single projection
    /// - inner FROM: may contain JOINs (INNER) over plain tables; no
    ///   LATERAL / UNNEST / generate_series / AS OF; no outer reference
    ///   inside join ON
    /// - WHERE: exactly one `inner.k = outer.col` (qualified columns)
    ///   + non-correlated all-inner predicates
    /// - projection: scalar expression, no aggregates / windows
    /// - outer: SelectStatement with FROM, no wildcards
    ///
    /// Returns true when at least one ScalarSubquery was rewritten.
    /// Returns false (no-op) when nothing in the statement matches —
    /// the existing per-row resolver then handles whatever's left.
    pub(crate) fn pull_up_correlated_limit_one_subqueries(
        &self,
        stmt: &mut SelectStatement,
    ) -> bool {
        // Phase 5 differential knob: an `AtomicBool` switch will land
        // alongside the byte-equal differential e2e (no_std rules out
        // std::env::var here). Production keeps the pass default-on.
        //
        // Outer FROM required (no FROM → nothing to JOIN against);
        // outer wildcards (`SELECT *`) widen the projection and would
        // surface the joined CTE's columns — refuse for safety.
        if stmt.from.is_none() || stmt.items.iter().any(|i| matches!(i, SelectItem::Wildcard)) {
            return false;
        }
        // Aliases an outer-correlation column may qualify to. Same
        // collection rule as `pull_up_unique_correlated_agg_subqueries`.
        let outer_aliases: alloc::collections::BTreeSet<String> = {
            let from = stmt.from.as_ref().expect("from present");
            let mut s = alloc::collections::BTreeSet::new();
            let push = |s: &mut alloc::collections::BTreeSet<String>, t: &TableRef| {
                s.insert(
                    t.alias
                        .clone()
                        .unwrap_or_else(|| t.name.clone())
                        .to_ascii_lowercase(),
                );
            };
            push(&mut s, &from.primary);
            for j in &from.joins {
                push(&mut s, &j.table);
            }
            s
        };
        let outer_has_group_by = stmt.group_by.is_some() || stmt.group_by_all;
        let mut new_ctes: Vec<Cte> = Vec::new();
        let mut new_joins: Vec<FromJoin> = Vec::new();
        let cte_seed = stmt.ctes.len();
        for item in &mut stmt.items {
            if let SelectItem::Expr { expr, .. } = item {
                self.pull_up_walk_limit_one(
                    expr,
                    false,
                    &outer_aliases,
                    outer_has_group_by,
                    cte_seed,
                    &mut new_ctes,
                    &mut new_joins,
                );
            }
        }
        if new_ctes.is_empty() {
            return false;
        }
        PULLUP_LIMIT1_FIRE_COUNT
            .fetch_add(new_ctes.len() as u64, core::sync::atomic::Ordering::Relaxed);
        stmt.ctes.extend(new_ctes);
        stmt.from
            .as_mut()
            .expect("from present")
            .joins
            .extend(new_joins);
        true
    }

    /// v7.37.4 — recursive mutable walk over a select-list expression
    /// for the LIMIT 1 pullup. Tracks `in_agg` so a ScalarSubquery
    /// already inside an aggregate doesn't get a redundant MAX wrapper
    /// (the outer aggregate folds whatever cell value the join supplies).
    #[allow(clippy::too_many_arguments)]
    fn pull_up_walk_limit_one(
        &self,
        e: &mut Expr,
        in_agg: bool,
        outer_aliases: &alloc::collections::BTreeSet<String>,
        outer_has_group_by: bool,
        cte_seed: usize,
        ctes_out: &mut Vec<Cte>,
        joins_out: &mut Vec<FromJoin>,
    ) {
        match e {
            Expr::ScalarSubquery(inner) => {
                if let Some((cte, join, cte_col)) =
                    self.try_pull_up_limit_one(inner, outer_aliases, cte_seed + ctes_out.len())
                {
                    ctes_out.push(cte);
                    joins_out.push(join);
                    // Outer needs a single scalar per outer row. With a
                    // LEFT JOIN against the CTE (sq.jk UNIQUE by GROUP
                    // BY), sq.pj is functionally a single value per
                    // join key — but a strict GROUP BY checker won't
                    // know that. When the outer query has its own
                    // GROUP BY and this position isn't already wrapped
                    // in an aggregate, wrap in MAX(sq.pj) so the
                    // checker sees an aggregate; MAX over a single
                    // value equals the value (any aggregate would).
                    let col_expr = Expr::Column(cte_col);
                    *e = if outer_has_group_by && !in_agg {
                        Expr::FunctionCall {
                            name: "max".into(),
                            args: alloc::vec![col_expr],
                        }
                    } else {
                        col_expr
                    };
                }
                // Otherwise leave for the existing per-row resolver.
                // The subquery body is a separate scope — don't descend.
            }
            Expr::FunctionCall { name, args } => {
                let child = in_agg || aggregate::is_aggregate_name(name);
                for a in args.iter_mut() {
                    self.pull_up_walk_limit_one(
                        a,
                        child,
                        outer_aliases,
                        outer_has_group_by,
                        cte_seed,
                        ctes_out,
                        joins_out,
                    );
                }
            }
            Expr::AggregateOrdered {
                call,
                order_by,
                filter,
                ..
            } => {
                self.pull_up_walk_limit_one(
                    call,
                    true,
                    outer_aliases,
                    outer_has_group_by,
                    cte_seed,
                    ctes_out,
                    joins_out,
                );
                for o in order_by.iter_mut() {
                    self.pull_up_walk_limit_one(
                        &mut o.expr,
                        true,
                        outer_aliases,
                        outer_has_group_by,
                        cte_seed,
                        ctes_out,
                        joins_out,
                    );
                }
                if let Some(f) = filter {
                    self.pull_up_walk_limit_one(
                        f,
                        true,
                        outer_aliases,
                        outer_has_group_by,
                        cte_seed,
                        ctes_out,
                        joins_out,
                    );
                }
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.pull_up_walk_limit_one(
                    lhs,
                    in_agg,
                    outer_aliases,
                    outer_has_group_by,
                    cte_seed,
                    ctes_out,
                    joins_out,
                );
                self.pull_up_walk_limit_one(
                    rhs,
                    in_agg,
                    outer_aliases,
                    outer_has_group_by,
                    cte_seed,
                    ctes_out,
                    joins_out,
                );
            }
            Expr::Unary { expr, .. }
            | Expr::Cast { expr, .. }
            | Expr::IsNull { expr, .. }
            | Expr::BoolTest { expr, .. }
            | Expr::FieldAccess { base: expr, .. } => {
                self.pull_up_walk_limit_one(
                    expr,
                    in_agg,
                    outer_aliases,
                    outer_has_group_by,
                    cte_seed,
                    ctes_out,
                    joins_out,
                );
            }
            Expr::Like { expr, pattern, .. } => {
                self.pull_up_walk_limit_one(
                    expr,
                    in_agg,
                    outer_aliases,
                    outer_has_group_by,
                    cte_seed,
                    ctes_out,
                    joins_out,
                );
                self.pull_up_walk_limit_one(
                    pattern,
                    in_agg,
                    outer_aliases,
                    outer_has_group_by,
                    cte_seed,
                    ctes_out,
                    joins_out,
                );
            }
            Expr::InList { expr, list, .. } => {
                self.pull_up_walk_limit_one(
                    expr,
                    in_agg,
                    outer_aliases,
                    outer_has_group_by,
                    cte_seed,
                    ctes_out,
                    joins_out,
                );
                for it in list.iter_mut() {
                    self.pull_up_walk_limit_one(
                        it,
                        in_agg,
                        outer_aliases,
                        outer_has_group_by,
                        cte_seed,
                        ctes_out,
                        joins_out,
                    );
                }
            }
            Expr::Case {
                operand,
                branches,
                else_branch,
            } => {
                if let Some(o) = operand {
                    self.pull_up_walk_limit_one(
                        o,
                        in_agg,
                        outer_aliases,
                        outer_has_group_by,
                        cte_seed,
                        ctes_out,
                        joins_out,
                    );
                }
                for (w, t) in branches.iter_mut() {
                    self.pull_up_walk_limit_one(
                        w,
                        in_agg,
                        outer_aliases,
                        outer_has_group_by,
                        cte_seed,
                        ctes_out,
                        joins_out,
                    );
                    self.pull_up_walk_limit_one(
                        t,
                        in_agg,
                        outer_aliases,
                        outer_has_group_by,
                        cte_seed,
                        ctes_out,
                        joins_out,
                    );
                }
                if let Some(eb) = else_branch {
                    self.pull_up_walk_limit_one(
                        eb,
                        in_agg,
                        outer_aliases,
                        outer_has_group_by,
                        cte_seed,
                        ctes_out,
                        joins_out,
                    );
                }
            }
            // Same boundary policy as `pull_up_walk` — don't descend
            // into window calls, EXISTS, etc.
            _ => {}
        }
    }

    /// v7.37.4 — decide whether a correlated scalar subquery qualifies
    /// for the LIMIT 1 → CTE pullup. Returns the CTE to add to outer
    /// `WITH`, the LEFT JOIN to append, and the (qualified) column
    /// that replaces the subquery node. None means: leave it for the
    /// per-row resolver.
    fn try_pull_up_limit_one(
        &self,
        inner: &SelectStatement,
        outer_aliases: &alloc::collections::BTreeSet<String>,
        alias_n: usize,
    ) -> Option<(Cte, FromJoin, ColumnName)> {
        // v7.37.4 A phase-2 finding (2026-06-19): the CTE rewrite
        // fires correctly on the mailrs prod subq 3 shape (verified
        // via PULLUP_LIMIT1_FIRE_COUNT in `pullup_fires_on_mailrs_subq3_shape`)
        // but PRODUCES A REGRESSION on the full prod SQL — mini cold
        // 100k SPGE 388.5 → 523.8 ms (+35%). Root cause:
        //   1. SPG's existing `try_batch_correlated_scalar` already
        //      handles the LIMIT 1 + ORDER BY 1 shape via post-LIMIT
        //      defer + keyed index seek (~ µs per surfaced outer key).
        //   2. The CTE form forces a full inner-table GROUP BY scan
        //      (~ 100 ms for 100k messages), then exec_with_ctes
        //      strips ctes + re-enters the body — extra catalog
        //      clone + double scan.
        //   3. Outer LIMIT 50 + GROUP BY thread_id means only ~50
        //      outer keys ultimately matter; CTE pre-aggregates ALL
        //      keys eagerly, wasting work for the unsurfaced 99 %.
        //
        // The CTE rewrite is right shape FOR the wrong root cause.
        // Real ceiling-first target is to make the existing batch
        // resolver's keyed-restriction path fire for the mailrs
        // GROUP BY + LIMIT shape, not to bypass it with a CTE.
        //
        // Keep the implementation dormant — the walker + gate
        // analysis stays as reference; turning this back on requires
        // a cost gate that proves CTE materialise + LEFT JOIN beats
        // the batch resolver for the SHAPE AT HAND (rare in practice).
        return None;
        #[allow(unreachable_code)]
        // Inner shape gates.
        if !inner.ctes.is_empty()
            || !inner.unions.is_empty()
            || inner.group_by.is_some()
            || inner.group_by_all
            || inner.having.is_some()
            || inner.distinct
            || inner.offset.is_some()
            || inner.items.len() != 1
            || inner.order_by.is_empty()
        {
            return None;
        }
        // LIMIT must be the literal 1 (placeholders bind late; we
        // can't guarantee the value here).
        match inner.limit {
            Some(LimitExpr::Literal(1)) => {}
            _ => return None,
        }
        let from = inner.from.as_ref()?;
        // Phase 2: single plain-table inner. Phase 3 lifts this gate
        // to allow inner INNER JOINs whose ON clauses are all-inner.
        if !from.joins.is_empty()
            || from.primary.lateral_subquery.is_some()
            || from.primary.unnest_expr.is_some()
            || from.primary.generate_series_args.is_some()
            || from.primary.as_of_segment.is_some()
        {
            return None;
        }
        let inner_table = from.primary.name.clone();
        let inner_alias = from
            .primary
            .alias
            .clone()
            .unwrap_or_else(|| inner_table.clone());
        let is_inner = |c: &ColumnName| -> bool {
            c.qualifier
                .as_deref()
                .is_some_and(|q| q.eq_ignore_ascii_case(&inner_alias))
        };
        let is_outer = |c: &ColumnName| -> bool {
            c.qualifier
                .as_deref()
                .is_some_and(|q| outer_aliases.contains(&q.to_ascii_lowercase()))
        };
        // Projection: scalar expression; reject aggregates / windows /
        // nested subqueries / outer references (the pulled-up SELECT
        // is uncorrelated GROUP BY — an outer column reference would
        // dangle).
        let SelectItem::Expr {
            expr: proj_expr,
            alias: _,
        } = &inner.items[0]
        else {
            return None;
        };
        if proj_has_disqualifying_shape(proj_expr, &inner_alias, outer_aliases) {
            return None;
        }
        // WHERE: exactly one `inner.k = outer.col`, plus all-inner
        // residual predicates.
        let where_ = inner.where_.as_ref()?;
        let mut corr: Option<(String, ColumnName)> = None;
        let mut non_corr: Vec<Expr> = Vec::new();
        for c in reorder::split_and_conjunctions(where_) {
            if let Expr::Binary {
                lhs,
                op: BinOp::Eq,
                rhs,
            } = c
                && let (Expr::Column(a), Expr::Column(b)) = (lhs.as_ref(), rhs.as_ref())
            {
                let pair = if is_inner(a) && is_outer(b) {
                    Some((a.name.clone(), b.clone()))
                } else if is_inner(b) && is_outer(a) {
                    Some((b.name.clone(), a.clone()))
                } else {
                    None
                };
                if let Some(p) = pair {
                    if corr.is_some() {
                        return None; // more than one correlation key
                    }
                    corr = Some(p);
                    continue;
                }
            }
            if !expr_is_all_inner(c, &inner_alias) {
                return None;
            }
            non_corr.push(c.clone());
        }
        let (inner_key, outer_col) = corr?;
        // ORDER BY: every key must be all-inner. Outer-referencing
        // sort keys would dangle after pullup.
        for ob in &inner.order_by {
            if !expr_is_all_inner(&ob.expr, &inner_alias) {
                return None;
            }
        }
        // Proj must also be all-inner (uncorrelated CTE body).
        if !expr_is_all_inner(proj_expr, &inner_alias) {
            return None;
        }
        // Build the CTE body:
        //   SELECT <inner.k> AS jk,
        //          (array_agg(<proj> ORDER BY <sort_keys>))[1] AS pj
        //     FROM <inner.from> WHERE <non_corr_AND_chain>
        //    GROUP BY <inner.k>
        let cte_name = alloc::format!("__cl1_{alias_n}");
        let jk_expr = Expr::Column(ColumnName {
            qualifier: Some(inner_alias.clone()),
            name: inner_key.clone(),
        });
        let argmax = Expr::ArraySubscript {
            target: alloc::boxed::Box::new(Expr::AggregateOrdered {
                call: alloc::boxed::Box::new(Expr::FunctionCall {
                    name: "array_agg".into(),
                    args: alloc::vec![proj_expr.clone()],
                }),
                order_by: inner.order_by.clone(),
                distinct: false,
                filter: None,
            }),
            index: alloc::boxed::Box::new(Expr::Literal(Literal::Integer(1))),
        };
        let body_where = if non_corr.is_empty() {
            None
        } else {
            let mut iter = non_corr.into_iter();
            let head = iter.next().expect("non_corr nonempty in this branch");
            Some(iter.fold(head, |acc, p| Expr::Binary {
                lhs: alloc::boxed::Box::new(acc),
                op: BinOp::And,
                rhs: alloc::boxed::Box::new(p),
            }))
        };
        let body = SelectStatement {
            locking: None,
            ctes: Vec::new(),
            distinct: false,
            distinct_on: Vec::new(),
            items: alloc::vec![
                SelectItem::Expr {
                    expr: jk_expr.clone(),
                    alias: Some("jk".into()),
                },
                SelectItem::Expr {
                    expr: argmax,
                    alias: Some("pj".into()),
                },
            ],
            from: Some(from.clone()),
            where_: body_where,
            group_by: Some(alloc::vec![jk_expr]),
            group_by_all: false,
            having: None,
            unions: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            limit_with_ties: false,
        };
        let cte = Cte {
            name: cte_name.clone(),
            body: spg_sql::ast::CteBody::Select(body),
            recursive: false,
            column_overrides: Vec::new(),
            search: None,
            cycle: None,
        };
        // LEFT JOIN __cl1_N ON __cl1_N.jk = <outer_col>
        let join = FromJoin {
            kind: JoinKind::Left,
            table: TableRef {
                name: cte_name.clone(),
                alias: None,
                as_of_segment: None,
                unnest_expr: None,
                unnest_column_aliases: Vec::new(),
                with_ordinality: false,
                generate_series_args: None,
                lateral_subquery: None,
                jsonb_each_text_arg: None,
                table_fn_call: None,
                rows_from: None,
                json_table: None,
                scalar_fn_item: false,
            },
            on: Some(Expr::Binary {
                lhs: alloc::boxed::Box::new(Expr::Column(ColumnName {
                    qualifier: Some(cte_name.clone()),
                    name: "jk".into(),
                })),
                op: BinOp::Eq,
                rhs: alloc::boxed::Box::new(Expr::Column(outer_col)),
            }),
            using_cols: None,
            natural: false,
        };
        let repl = ColumnName {
            qualifier: Some(cte_name),
            name: "pj".into(),
        };
        Some((cte, join, repl))
    }

    pub(crate) fn pull_up_unique_correlated_agg_subqueries(
        &self,
        stmt: &mut SelectStatement,
    ) -> bool {
        if stmt.from.is_none() || stmt.items.iter().any(|i| matches!(i, SelectItem::Wildcard)) {
            return false;
        }
        // Aliases an outer-correlation column may qualify to.
        let outer_aliases: alloc::collections::BTreeSet<String> = {
            let from = stmt.from.as_ref().expect("from present");
            let mut s = alloc::collections::BTreeSet::new();
            let push = |s: &mut alloc::collections::BTreeSet<String>, t: &TableRef| {
                s.insert(
                    t.alias
                        .clone()
                        .unwrap_or_else(|| t.name.clone())
                        .to_ascii_lowercase(),
                );
            };
            push(&mut s, &from.primary);
            for j in &from.joins {
                push(&mut s, &j.table);
            }
            s
        };
        let mut new_joins: Vec<FromJoin> = Vec::new();
        for item in &mut stmt.items {
            if let SelectItem::Expr { expr, .. } = item {
                self.pull_up_walk(expr, false, &outer_aliases, &mut new_joins);
            }
        }
        if new_joins.is_empty() {
            return false;
        }
        stmt.from
            .as_mut()
            .expect("from present")
            .joins
            .extend(new_joins);
        true
    }

    /// Recursive mutable walk over an expression tracking whether we are
    /// inside an aggregate argument. A correlated scalar subquery found in
    /// aggregate context that `try_pull_up_join` accepts is replaced in
    /// place by the joined column; the join is queued in `joins_out`.
    fn pull_up_walk(
        &self,
        e: &mut Expr,
        in_agg: bool,
        outer_aliases: &alloc::collections::BTreeSet<String>,
        joins_out: &mut Vec<FromJoin>,
    ) {
        match e {
            Expr::ScalarSubquery(inner) => {
                if in_agg
                    && let Some((join, col)) =
                        self.try_pull_up_join(inner, outer_aliases, joins_out.len())
                {
                    joins_out.push(join);
                    *e = Expr::Column(col);
                }
                // Otherwise leave for the existing resolver; the subquery
                // body is a separate scope, so don't descend into it.
            }
            Expr::FunctionCall { name, args } => {
                let child = in_agg || aggregate::is_aggregate_name(name);
                for a in args.iter_mut() {
                    self.pull_up_walk(a, child, outer_aliases, joins_out);
                }
            }
            Expr::AggregateOrdered {
                call,
                order_by,
                filter,
                ..
            } => {
                self.pull_up_walk(call, true, outer_aliases, joins_out);
                for o in order_by.iter_mut() {
                    self.pull_up_walk(&mut o.expr, true, outer_aliases, joins_out);
                }
                if let Some(f) = filter {
                    self.pull_up_walk(f, true, outer_aliases, joins_out);
                }
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.pull_up_walk(lhs, in_agg, outer_aliases, joins_out);
                self.pull_up_walk(rhs, in_agg, outer_aliases, joins_out);
            }
            Expr::Unary { expr, .. }
            | Expr::Cast { expr, .. }
            | Expr::IsNull { expr, .. }
            | Expr::BoolTest { expr, .. }
            | Expr::FieldAccess { base: expr, .. } => {
                self.pull_up_walk(expr, in_agg, outer_aliases, joins_out);
            }
            Expr::Like { expr, pattern, .. } => {
                self.pull_up_walk(expr, in_agg, outer_aliases, joins_out);
                self.pull_up_walk(pattern, in_agg, outer_aliases, joins_out);
            }
            Expr::InList { expr, list, .. } => {
                self.pull_up_walk(expr, in_agg, outer_aliases, joins_out);
                for it in list.iter_mut() {
                    self.pull_up_walk(it, in_agg, outer_aliases, joins_out);
                }
            }
            Expr::Case {
                operand,
                branches,
                else_branch,
            } => {
                if let Some(o) = operand {
                    self.pull_up_walk(o, in_agg, outer_aliases, joins_out);
                }
                for (w, t) in branches.iter_mut() {
                    self.pull_up_walk(w, in_agg, outer_aliases, joins_out);
                    self.pull_up_walk(t, in_agg, outer_aliases, joins_out);
                }
                if let Some(eb) = else_branch {
                    self.pull_up_walk(eb, in_agg, outer_aliases, joins_out);
                }
            }
            // Window functions, EXISTS / IN subqueries, and other variants
            // are intentionally not descended for this rewrite — the
            // common aggregate-arg shapes above cover the reported load and
            // anything missed simply keeps its existing evaluation.
            _ => {}
        }
    }

    /// Decide whether a correlated scalar subquery qualifies for the
    /// unique-key LEFT JOIN pull-up. Returns the join to append and the
    /// column that replaces the subquery node, or None to leave it alone.
    fn try_pull_up_join(
        &self,
        inner: &SelectStatement,
        outer_aliases: &alloc::collections::BTreeSet<String>,
        alias_n: usize,
    ) -> Option<(FromJoin, ColumnName)> {
        // Inner must be a single plain-table scan with one projected
        // column and none of the shape-breaking clauses.
        if !inner.ctes.is_empty()
            || !inner.unions.is_empty()
            || inner.group_by.is_some()
            || inner.having.is_some()
            || inner.distinct
            || !inner.order_by.is_empty()
            || inner.limit.is_some()
            || inner.offset.is_some()
            || inner.items.len() != 1
        {
            return None;
        }
        let from = inner.from.as_ref()?;
        if !from.joins.is_empty()
            || from.primary.lateral_subquery.is_some()
            || from.primary.unnest_expr.is_some()
            || from.primary.generate_series_args.is_some()
            || from.primary.as_of_segment.is_some()
        {
            return None;
        }
        let inner_table = from.primary.name.clone();
        let inner_alias = from
            .primary
            .alias
            .clone()
            .unwrap_or_else(|| inner_table.clone());
        let is_inner = |c: &ColumnName| -> bool {
            c.qualifier
                .as_deref()
                .is_some_and(|q| q.eq_ignore_ascii_case(&inner_alias))
        };
        let is_outer = |c: &ColumnName| -> bool {
            c.qualifier
                .as_deref()
                .is_some_and(|q| outer_aliases.contains(&q.to_ascii_lowercase()))
        };
        // Projected column: a single inner-qualified column.
        let SelectItem::Expr { expr: out_expr, .. } = &inner.items[0] else {
            return None;
        };
        let Expr::Column(out_col) = out_expr else {
            return None;
        };
        if !is_inner(out_col) {
            return None;
        }
        // WHERE: exactly one `inner.key = outer.col`, rest all-inner.
        let w = inner.where_.as_ref()?;
        let mut corr: Option<(String, ColumnName)> = None;
        let mut rest: Vec<Expr> = Vec::new();
        for c in reorder::split_and_conjunctions(w) {
            if let Expr::Binary {
                lhs,
                op: BinOp::Eq,
                rhs,
            } = c
                && let (Expr::Column(a), Expr::Column(b)) = (lhs.as_ref(), rhs.as_ref())
            {
                let pair = if is_inner(a) && is_outer(b) {
                    Some((a.name.clone(), b.clone()))
                } else if is_inner(b) && is_outer(a) {
                    Some((b.name.clone(), a.clone()))
                } else {
                    None
                };
                if let Some(p) = pair {
                    if corr.is_some() {
                        return None; // more than one correlation
                    }
                    corr = Some(p);
                    continue;
                }
            }
            if !expr_is_all_inner(c, &inner_alias) {
                return None;
            }
            rest.push(c.clone());
        }
        let (inner_key, outer_col) = corr?;
        // Safety gate: the correlation key must be UNIQUE / PRIMARY KEY on
        // the inner table so the join can't multiply outer rows.
        if !self.column_is_single_unique(&inner_table, &inner_key) {
            return None;
        }
        // Build the LEFT JOIN against a fresh alias.
        let fresh = alloc::format!("__plj_{alias_n}");
        let key_eq = Expr::Binary {
            lhs: alloc::boxed::Box::new(Expr::Column(ColumnName {
                qualifier: Some(fresh.clone()),
                name: inner_key,
            })),
            op: BinOp::Eq,
            rhs: alloc::boxed::Box::new(Expr::Column(outer_col)),
        };
        let on = rest
            .into_iter()
            .map(|mut e| {
                rename_qualifier(&mut e, &inner_alias, &fresh);
                e
            })
            .fold(key_eq, |acc, pred| Expr::Binary {
                lhs: alloc::boxed::Box::new(acc),
                op: BinOp::And,
                rhs: alloc::boxed::Box::new(pred),
            });
        let join = FromJoin {
            kind: JoinKind::Left,
            table: TableRef {
                name: inner_table,
                alias: Some(fresh.clone()),
                as_of_segment: None,
                unnest_expr: None,
                unnest_column_aliases: Vec::new(),
                with_ordinality: false,
                generate_series_args: None,
                lateral_subquery: None,
                jsonb_each_text_arg: None,
                table_fn_call: None,
                rows_from: None,
                json_table: None,
                scalar_fn_item: false,
            },
            on: Some(on),
            using_cols: None,
            natural: false,
        };
        let repl = ColumnName {
            qualifier: Some(fresh),
            name: out_col.name.clone(),
        };
        Some((join, repl))
    }

    /// v7.34.2 (mailrs prod NOT EXISTS hot-path) — plan-time EXISTS /
    /// NOT EXISTS sublink pull-up to semi/anti-join. PostgreSQL's
    /// `convert_EXISTS_sublink_to_join`-flavoured rewrite: a correlated
    /// `[NOT] EXISTS (SELECT … FROM t WHERE t.k = outer.col [AND inner])`
    /// in the WHERE-AND spine collapses to a real JOIN against `t`. The
    /// per-row dispatch (clone host expr × 25 k + splice + eval) goes
    /// away entirely — the executor streams one tight join loop the
    /// same way it would for a hand-written JOIN.
    ///
    /// Shape rules:
    ///   * NOT EXISTS  → LEFT JOIN t AS __exsj_N ON t.k = outer.col [AND …]
    ///                   AND a survivor `__exsj_N.k IS NULL` conjunct
    ///                   stays in WHERE. Safe regardless of uniqueness:
    ///                   IS-NULL only fires on the LEFT-JOIN pad row,
    ///                   so duplicate inner matches collapse cleanly
    ///                   (any match drops the outer row; only no-match
    ///                   outer rows survive).
    ///   * EXISTS      → INNER JOIN. Safe only when inner.k is single-
    ///                   column UNIQUE / PRIMARY KEY (otherwise INNER
    ///                   would multiply outer rows). Gated by
    ///                   `column_is_single_unique`. No survivor needed
    ///                   in WHERE — the join itself encodes EXISTS=true.
    ///
    /// Eligible inner: single plain-table FROM, no nested JOIN / CTE /
    /// UNION / GROUP / HAVING / DISTINCT / ORDER / LIMIT / OFFSET, and
    /// WHERE = exactly one `inner.k = outer.col` correlation plus
    /// optional all-inner predicates that ride into the ON clause.
    /// Anything else is left for the per-row resolver.
    ///
    /// Returns true when at least one conjunct was pulled up.
    pub(crate) fn pull_up_exists_sublinks(&self, stmt: &mut SelectStatement) -> bool {
        if stmt.from.is_none() {
            return false;
        }
        let Some(where_expr) = stmt.where_.take() else {
            return false;
        };
        // v7.37.4 A'' — pre-disambiguate outer unqualified column refs
        // whose name would collide with a future pulled-up inner
        // table's columns. mailrs `/api/conversations` uses bare
        // `thread_id != ''` in outer WHERE; once we add
        // `__exsj_0 LEFT JOIN snoozed_conversations` (also with a
        // `thread_id` column), the resolver raises "ambiguous column".
        // Conservative: scan EXISTS / NOT EXISTS subqueries in the
        // WHERE we just took out, look up each inner plain-table's
        // column set, and for every collision column that exists in
        // exactly one outer table, pre-qualify it to that owning alias.
        let mut collision_names: alloc::collections::BTreeSet<String> =
            alloc::collections::BTreeSet::new();
        for c in reorder::split_and_conjunctions(&where_expr) {
            let inner_subq: Option<&SelectStatement> = match c {
                Expr::Exists { subquery, .. } => Some(subquery.as_ref()),
                Expr::Unary {
                    op: UnOp::Not,
                    expr,
                } => match expr.as_ref() {
                    Expr::Exists { subquery, .. } => Some(subquery.as_ref()),
                    _ => None,
                },
                _ => None,
            };
            let Some(inner) = inner_subq else { continue };
            let Some(from) = &inner.from else { continue };
            if !from.joins.is_empty() {
                continue;
            }
            let Some(t) = self.active_catalog().get(&from.primary.name) else {
                continue;
            };
            for col in &t.schema().columns {
                collision_names.insert(col.name.to_ascii_lowercase());
            }
        }
        let mut where_expr = where_expr;
        if !collision_names.is_empty() {
            let from = stmt.from.as_ref().expect("from present");
            let outer_tables: Vec<(String, String)> = {
                let mut v = Vec::new();
                let collect = |v: &mut Vec<(String, String)>, t: &TableRef| {
                    let alias = t.alias.clone().unwrap_or_else(|| t.name.clone());
                    v.push((alias, t.name.clone()));
                };
                collect(&mut v, &from.primary);
                for j in &from.joins {
                    collect(&mut v, &j.table);
                }
                v
            };
            let mut owner: alloc::collections::BTreeMap<String, String> =
                alloc::collections::BTreeMap::new();
            for col_lc in &collision_names {
                let mut matches: Vec<String> = Vec::new();
                for (alias, tname) in &outer_tables {
                    let Some(t) = self.active_catalog().get(tname) else {
                        continue;
                    };
                    if t.schema()
                        .columns
                        .iter()
                        .any(|c| c.name.eq_ignore_ascii_case(col_lc))
                    {
                        matches.push(alias.clone());
                    }
                }
                if matches.len() == 1 {
                    owner.insert(col_lc.clone(), matches.remove(0));
                }
            }
            if !owner.is_empty() {
                disambiguate_stmt_unqualified_columns(stmt, &owner);
                disambiguate_expr_unqualified_columns(&mut where_expr, &owner);
            }
        }
        let outer_aliases: alloc::collections::BTreeSet<String> = {
            let from = stmt.from.as_ref().expect("from present");
            let mut s = alloc::collections::BTreeSet::new();
            let push = |s: &mut alloc::collections::BTreeSet<String>, t: &TableRef| {
                s.insert(
                    t.alias
                        .clone()
                        .unwrap_or_else(|| t.name.clone())
                        .to_ascii_lowercase(),
                );
            };
            push(&mut s, &from.primary);
            for j in &from.joins {
                push(&mut s, &j.table);
            }
            s
        };
        let conjuncts = reorder::split_and_conjunctions(&where_expr);
        let mut survivors: Vec<Expr> = Vec::new();
        let mut new_joins: Vec<FromJoin> = Vec::new();
        let mut rewrote_any = false;
        for c in conjuncts {
            // v7.34.3 — the parser emits `NOT EXISTS(...)` as
            // `Expr::Unary{Not, Exists{negated:false, …}}`, NOT as
            // `Exists{negated:true}`. Match both shapes so the
            // pull-up handles both `EXISTS` and `NOT EXISTS`.
            let parsed: Option<(&SelectStatement, bool)> = match c {
                Expr::Exists { subquery, negated } => Some((subquery.as_ref(), *negated)),
                Expr::Unary {
                    op: UnOp::Not,
                    expr,
                } => match expr.as_ref() {
                    Expr::Exists { subquery, negated } => Some((subquery.as_ref(), !*negated)),
                    _ => None,
                },
                _ => None,
            };
            if let Some((subquery, neg)) = parsed {
                // v7.34.2 first chose `[NOT] IN (SELECT k FROM t)` first
                // because the `mailrs_prod_not_exists` 250 k probe
                // dropped 178 ms (LEFT JOIN + IS NULL form) → 74 ms
                // (NOT IN form). But that win was from the OUTER ORDER
                // BY id DESC LIMIT N walker fast path
                // (`try_pk_walk_top_n`), which only the InList shape
                // exposes (early-stop on first N survivors). For
                // shapes WITHOUT an outer LIMIT (e.g. `SELECT
                // COUNT(*) FROM messages WHERE NOT EXISTS …`) the IN
                // form has to materialise the entire 12.5 k inner
                // value set as `Vec<Expr::Literal>` before HashSet
                // build — pure overhead that the LEFT ANTI JOIN
                // executor skips by hashing the inner table directly.
                // v7.37.x (docker-fair NOTEX) — branch on outer
                // LIMIT presence: with LIMIT, prefer InList (walker
                // benefit); without LIMIT, prefer LEFT ANTI JOIN
                // (streaming build, no Expr::Literal Vec roundtrip).
                let outer_has_limit = stmt.limit.is_some();
                let try_in_first = outer_has_limit;
                let mut consumed = false;
                if try_in_first
                    && let Some(rewritten) =
                        self.try_pull_up_exists_as_in(subquery, neg, &outer_aliases)
                {
                    survivors.push(rewritten);
                    consumed = true;
                }
                if !consumed
                    && let Some((join, residual)) = self.try_pull_up_exists_sublink(
                        subquery,
                        neg,
                        &outer_aliases,
                        new_joins.len(),
                    )
                {
                    new_joins.push(join);
                    if let Some(r) = residual {
                        survivors.push(r);
                    }
                    consumed = true;
                }
                if !consumed
                    && !try_in_first
                    && let Some(rewritten) =
                        self.try_pull_up_exists_as_in(subquery, neg, &outer_aliases)
                {
                    // Fallback when LEFT ANTI JOIN refused (e.g. inner
                    // shape too complex) — IN form is the next best.
                    survivors.push(rewritten);
                    consumed = true;
                }
                if consumed {
                    rewrote_any = true;
                    continue;
                }
            }
            survivors.push(c.clone());
        }
        if !rewrote_any {
            stmt.where_ = Some(where_expr);
            return false;
        }
        EXISTS_PULLUP_FIRE_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if !new_joins.is_empty() {
            stmt.from
                .as_mut()
                .expect("from present")
                .joins
                .extend(new_joins);
        }
        stmt.where_ = survivors.into_iter().reduce(|a, b| Expr::Binary {
            lhs: alloc::boxed::Box::new(a),
            op: BinOp::And,
            rhs: alloc::boxed::Box::new(b),
        });
        true
    }

    /// v7.34.3 — emit the EXISTS conjunct as `outer.col IN (SELECT
    /// inner.k FROM inner.table)` (or its negated form). Eligibility
    /// mirrors `try_pull_up_exists_sublink` — single plain-table FROM,
    /// no shape-breaking clauses, exactly one `inner.k = outer.col`
    /// correlation plus optional all-inner predicates — except no
    /// uniqueness check is needed (IN handles duplicate inner.k
    /// fine). For the NEGATED case we ALSO require inner.k to be
    /// declared NOT NULL: `outer.col NOT IN (set with NULL)` returns
    /// UNKNOWN for every outer row in SQL three-valued logic, which
    /// differs from NOT EXISTS semantics. None on ineligible →
    /// caller falls back to the LEFT JOIN + IS NULL injection or
    /// the legacy per-row resolver.
    fn try_pull_up_exists_as_in(
        &self,
        inner: &SelectStatement,
        negated: bool,
        outer_aliases: &alloc::collections::BTreeSet<String>,
    ) -> Option<Expr> {
        if !inner.ctes.is_empty()
            || !inner.unions.is_empty()
            || inner.group_by.is_some()
            || inner.having.is_some()
            || inner.distinct
            || !inner.order_by.is_empty()
            || inner.limit.is_some()
            || inner.offset.is_some()
        {
            return None;
        }
        let from = inner.from.as_ref()?;
        if !from.joins.is_empty()
            || from.primary.lateral_subquery.is_some()
            || from.primary.unnest_expr.is_some()
            || from.primary.generate_series_args.is_some()
            || from.primary.as_of_segment.is_some()
        {
            return None;
        }
        let inner_table = from.primary.name.clone();
        let inner_alias = from
            .primary
            .alias
            .clone()
            .unwrap_or_else(|| inner_table.clone());
        let is_inner = |c: &ColumnName| -> bool {
            c.qualifier
                .as_deref()
                .is_some_and(|q| q.eq_ignore_ascii_case(&inner_alias))
        };
        let is_outer = |c: &ColumnName| -> bool {
            c.qualifier
                .as_deref()
                .is_some_and(|q| outer_aliases.contains(&q.to_ascii_lowercase()))
        };
        let w = inner.where_.as_ref()?;
        let mut corr: Option<(String, ColumnName)> = None;
        let mut rest: Vec<Expr> = Vec::new();
        for c in reorder::split_and_conjunctions(w) {
            if let Expr::Binary {
                lhs,
                op: BinOp::Eq,
                rhs,
            } = c
                && let (Expr::Column(a), Expr::Column(b)) = (lhs.as_ref(), rhs.as_ref())
            {
                let pair = if is_inner(a) && is_outer(b) {
                    Some((a.name.clone(), b.clone()))
                } else if is_inner(b) && is_outer(a) {
                    Some((b.name.clone(), a.clone()))
                } else {
                    None
                };
                if let Some(p) = pair {
                    if corr.is_some() {
                        return None;
                    }
                    corr = Some(p);
                    continue;
                }
            }
            if !expr_is_all_inner(c, &inner_alias) {
                return None;
            }
            rest.push(c.clone());
        }
        let (inner_key, outer_col) = corr?;
        if negated && !self.column_is_not_null(&inner_table, &inner_key) {
            return None;
        }
        // Build the rewritten inner SELECT: `SELECT inner.k FROM
        // inner.table [WHERE rest]`. The correlation conjunct is
        // dropped — IN-subquery handles equality membership. All-inner
        // residual predicates ride into the new WHERE.
        let mut rewritten = inner.clone();
        rewritten.limit = None;
        rewritten.offset = None;
        rewritten.order_by = Vec::new();
        rewritten.distinct = false;
        rewritten.where_ = rest.into_iter().reduce(|a, b| Expr::Binary {
            lhs: alloc::boxed::Box::new(a),
            op: BinOp::And,
            rhs: alloc::boxed::Box::new(b),
        });
        rewritten.items = alloc::vec![SelectItem::Expr {
            expr: Expr::Column(ColumnName {
                qualifier: Some(inner_alias),
                name: inner_key,
            }),
            alias: None,
        }];
        Some(Expr::InSubquery {
            expr: alloc::boxed::Box::new(Expr::Column(outer_col)),
            subquery: alloc::boxed::Box::new(rewritten),
            negated,
        })
    }

    fn try_pull_up_exists_sublink(
        &self,
        inner: &SelectStatement,
        negated: bool,
        outer_aliases: &alloc::collections::BTreeSet<String>,
        alias_n: usize,
    ) -> Option<(FromJoin, Option<Expr>)> {
        EXISTS_PULLUP_CANDIDATE_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if !inner.ctes.is_empty()
            || !inner.unions.is_empty()
            || inner.group_by.is_some()
            || inner.having.is_some()
            || inner.distinct
            || !inner.order_by.is_empty()
            || inner.limit.is_some()
            || inner.offset.is_some()
        {
            EXISTS_PULLUP_BAIL_INNER_SHAPE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            return None;
        }
        let from = inner.from.as_ref()?;
        if !from.joins.is_empty()
            || from.primary.lateral_subquery.is_some()
            || from.primary.unnest_expr.is_some()
            || from.primary.generate_series_args.is_some()
            || from.primary.as_of_segment.is_some()
        {
            EXISTS_PULLUP_BAIL_INNER_FROM.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            return None;
        }
        let inner_table = from.primary.name.clone();
        let inner_alias = from
            .primary
            .alias
            .clone()
            .unwrap_or_else(|| inner_table.clone());
        let is_inner = |c: &ColumnName| -> bool {
            c.qualifier
                .as_deref()
                .is_some_and(|q| q.eq_ignore_ascii_case(&inner_alias))
        };
        let is_outer = |c: &ColumnName| -> bool {
            c.qualifier
                .as_deref()
                .is_some_and(|q| outer_aliases.contains(&q.to_ascii_lowercase()))
        };
        let Some(w) = inner.where_.as_ref() else {
            EXISTS_PULLUP_BAIL_NO_WHERE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            return None;
        };
        // v7.37.4 A'' (mailrs prod /api/conversations 2-col anti-join) —
        // accept multi-column correlation. Today's single-pair restriction
        // forced mailrs's
        //   NOT EXISTS (SELECT 1 FROM sc WHERE sc.thread_id = m.thread_id
        //                                  AND sc.account_address = mb.user_address
        //                                  AND sc.snoozed_until > 0)
        // to fall back to the batch `try_batch_correlated_exists` path,
        // which builds the inner set fine but then pays a per-row host-
        // expression clone + AST walk + eval to splice each EXISTS node
        // into a Bool literal (line 194-211 above). 100k join survivors ×
        // ~1.5 µs per splice = ~150 ms on the mini cold bench. Pulling
        // multi-col is the same shape SPG / PG / MySQL / MariaDB plan a
        // multi-key anti-join: LEFT JOIN sc ON (sc.thread_id = m.thread_id
        //   AND sc.account_address = mb.user_address [AND inner preds])
        // + WHERE sc.<first key> IS NULL. NULL semantics: a NULL on any
        // join key means no match, identical to NOT EXISTS three-valued
        // logic (the IS NULL probe matches the pad row).
        let mut corr_pairs: Vec<(String, ColumnName)> = Vec::new();
        let mut rest: Vec<Expr> = Vec::new();
        for c in reorder::split_and_conjunctions(w) {
            if let Expr::Binary {
                lhs,
                op: BinOp::Eq,
                rhs,
            } = c
                && let (Expr::Column(a), Expr::Column(b)) = (lhs.as_ref(), rhs.as_ref())
            {
                let pair = if is_inner(a) && is_outer(b) {
                    Some((a.name.clone(), b.clone()))
                } else if is_inner(b) && is_outer(a) {
                    Some((b.name.clone(), a.clone()))
                } else {
                    None
                };
                if let Some(p) = pair {
                    corr_pairs.push(p);
                    continue;
                }
            }
            if !expr_is_all_inner(c, &inner_alias) {
                EXISTS_PULLUP_BAIL_RESIDUAL_NOT_INNER
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                return None;
            }
            rest.push(c.clone());
        }
        if corr_pairs.is_empty() {
            EXISTS_PULLUP_BAIL_NO_CORR.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            return None;
        }
        // Differential knob — refuse the multi-col case under test so
        // the baseline path (batch resolver) runs and its result can
        // be compared against the pullup-on path. Single-col stays on.
        if corr_pairs.len() > 1
            && EXISTS_PULLUP_MULTICOL_DISABLE.load(core::sync::atomic::Ordering::Relaxed)
        {
            EXISTS_PULLUP_BAIL_MULTICOL_DISABLED
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            return None;
        }
        // EXISTS (semi-join) requires uniqueness on EVERY inner key so
        // the INNER JOIN can't multiply outer rows when more than one
        // inner row matches the tuple. NOT EXISTS (anti-join) uses
        // LEFT + IS NULL and is safe regardless of inner key uniqueness:
        // duplicate inner matches collapse into "matched" for the
        // anti-join probe.
        if !negated {
            // For multi-col EXISTS today we conservatively require each
            // inner column to carry a single-column UNIQUE / PRIMARY KEY
            // — the join cardinality guarantee is per-column. A truer
            // composite-unique gate could relax this; the prod hot
            // path (mailrs) is negated so deferring is safe.
            for (k, _) in &corr_pairs {
                if !self.column_is_single_unique(&inner_table, k) {
                    EXISTS_PULLUP_BAIL_UNIQUE_KEY_MISSING
                        .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    return None;
                }
            }
        }
        let fresh = alloc::format!("__exsj_{alias_n}");
        // Build the ON conjunction: every (inner_key = outer_col) pair
        // joined by AND, then folded with the all-inner residual.
        let mut on_iter = corr_pairs.iter().map(|(ik, oc)| Expr::Binary {
            lhs: alloc::boxed::Box::new(Expr::Column(ColumnName {
                qualifier: Some(fresh.clone()),
                name: ik.clone(),
            })),
            op: BinOp::Eq,
            rhs: alloc::boxed::Box::new(Expr::Column(oc.clone())),
        });
        let first_key_eq = on_iter
            .next()
            .expect("corr_pairs non-empty post `is_empty()` gate");
        let on = rest
            .into_iter()
            .map(|mut e| {
                rename_qualifier(&mut e, &inner_alias, &fresh);
                e
            })
            .chain(on_iter)
            .fold(first_key_eq, |acc, pred| Expr::Binary {
                lhs: alloc::boxed::Box::new(acc),
                op: BinOp::And,
                rhs: alloc::boxed::Box::new(pred),
            });
        let join = FromJoin {
            kind: if negated {
                JoinKind::Left
            } else {
                JoinKind::Inner
            },
            table: TableRef {
                name: inner_table,
                alias: Some(fresh.clone()),
                as_of_segment: None,
                unnest_expr: None,
                unnest_column_aliases: Vec::new(),
                with_ordinality: false,
                generate_series_args: None,
                lateral_subquery: None,
                jsonb_each_text_arg: None,
                table_fn_call: None,
                rows_from: None,
                json_table: None,
                scalar_fn_item: false,
            },
            on: Some(on),
            using_cols: None,
            natural: false,
        };
        let residual = if negated {
            // anti-join: pick the FIRST inner key as the IS NULL probe.
            // Any IS NULL on a joined-side column is sufficient — the
            // LEFT-JOIN pad row sets ALL inner columns to NULL atomically,
            // so a single column witnesses "no match".
            let probe_key = corr_pairs[0].0.clone();
            Some(Expr::IsNull {
                expr: alloc::boxed::Box::new(Expr::Column(ColumnName {
                    qualifier: Some(fresh),
                    name: probe_key,
                })),
                negated: false,
            })
        } else {
            None
        };
        Some((join, residual))
    }

    /// v7.34.3 — true when `col` on `table` is declared NOT NULL (the
    /// `ColumnSchema.nullable` flag is `false`). Used to gate the
    /// `NOT EXISTS → NOT IN` rewrite, since SQL three-valued logic
    /// turns `outer.col NOT IN (set with NULL)` into UNKNOWN for every
    /// outer row, which would differ from the NOT EXISTS semantics.
    fn column_is_not_null(&self, table: &str, col: &str) -> bool {
        let Some(t) = self.active_catalog().get(table) else {
            return false;
        };
        let sch = t.schema();
        // Direct flag — cheap path. Covers explicit NOT NULL columns
        // and table-level PK constraints (ddl.rs line 1252).
        if sch
            .columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(col))
            .is_some_and(|c| !c.nullable)
        {
            return true;
        }
        // v7.34.3 — inline `PRIMARY KEY` on a column definition
        // (e.g. `id BIGSERIAL PRIMARY KEY`) does NOT currently flip
        // `ColumnSchema.nullable` to false in ddl.rs (only the
        // table-level `CONSTRAINT … PRIMARY KEY (col)` shape does).
        // PK semantically implies NOT NULL, so cross-check the
        // installed uniqueness constraints' `is_primary_key` flag too.
        let Some(pos) = sch
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(col))
        else {
            return false;
        };
        sch.uniqueness_constraints
            .iter()
            .any(|u| u.is_primary_key && u.columns.as_slice() == [pos])
    }

    /// True when `col` on `table` is covered by a single-column UNIQUE or
    /// PRIMARY KEY constraint (declared and engine-enforced), or a unique
    /// index — i.e. an equality on it matches at most one row.
    fn column_is_single_unique(&self, table: &str, col: &str) -> bool {
        let Some(t) = self.active_catalog().get(table) else {
            return false;
        };
        let sch = t.schema();
        let Some(pos) = sch
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(col))
        else {
            return false;
        };
        if sch
            .uniqueness_constraints
            .iter()
            .any(|u| u.columns.as_slice() == [pos])
        {
            return true;
        }
        t.index_on(pos).is_some_and(|idx| idx.is_unique)
    }
}

// ---- subquery free-fn helpers (lib.rs split 6) ----

/// v7.33 — true when every column in `e` is qualified to `inner_alias`
/// and `e` contains no nested subquery. Used by the sublink pull-up to
/// confirm a non-correlation conjunct is purely inner (safe to carry into
/// the join ON after a qualifier rename).
/// v7.37.4 — refuse projection expressions that would dangle after
/// the LIMIT 1 pullup: aggregates / window calls / EXISTS / scalar
/// subqueries / outer-qualified columns (the pulled-up CTE body is
/// uncorrelated, so an outer reference inside the projection has no
/// scope to bind against). All-inner column references are fine.
fn proj_has_disqualifying_shape(
    e: &Expr,
    inner_alias: &str,
    outer_aliases: &alloc::collections::BTreeSet<String>,
) -> bool {
    match e {
        Expr::AggregateOrdered { .. }
        | Expr::WindowFunction { .. }
        | Expr::ScalarSubquery(_)
        | Expr::Exists { .. } => true,
        Expr::FunctionCall { name, args } => {
            if aggregate::is_aggregate_name(name) {
                return true;
            }
            args.iter()
                .any(|a| proj_has_disqualifying_shape(a, inner_alias, outer_aliases))
        }
        Expr::Column(c) => {
            // Reject outer-qualified columns inside the projection
            // (they'd dangle in the uncorrelated CTE body). Unqualified
            // columns are ambiguous in a multi-table inner — for the
            // phase-2 single-table gate they resolve to `inner_alias`
            // anyway, accept them. Qualified inner refs are OK.
            if let Some(q) = c.qualifier.as_deref() {
                outer_aliases.contains(&q.to_ascii_lowercase())
                    && !q.eq_ignore_ascii_case(inner_alias)
            } else {
                false
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            proj_has_disqualifying_shape(lhs, inner_alias, outer_aliases)
                || proj_has_disqualifying_shape(rhs, inner_alias, outer_aliases)
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => {
            proj_has_disqualifying_shape(expr, inner_alias, outer_aliases)
        }
        Expr::Like { expr, pattern, .. } => {
            proj_has_disqualifying_shape(expr, inner_alias, outer_aliases)
                || proj_has_disqualifying_shape(pattern, inner_alias, outer_aliases)
        }
        Expr::InList { expr, list, .. } => {
            proj_has_disqualifying_shape(expr, inner_alias, outer_aliases)
                || list
                    .iter()
                    .any(|it| proj_has_disqualifying_shape(it, inner_alias, outer_aliases))
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            operand
                .as_ref()
                .is_some_and(|o| proj_has_disqualifying_shape(o, inner_alias, outer_aliases))
                || branches.iter().any(|(w, t)| {
                    proj_has_disqualifying_shape(w, inner_alias, outer_aliases)
                        || proj_has_disqualifying_shape(t, inner_alias, outer_aliases)
                })
                || else_branch
                    .as_ref()
                    .is_some_and(|b| proj_has_disqualifying_shape(b, inner_alias, outer_aliases))
        }
        Expr::ArraySubscript { target, index } => {
            proj_has_disqualifying_shape(target, inner_alias, outer_aliases)
                || proj_has_disqualifying_shape(index, inner_alias, outer_aliases)
        }
        _ => false,
    }
}

/// v7.37.4 A'' — walk every Expr field of a SelectStatement and
/// qualify any unqualified column whose name is in `owner`. Skips
/// nested subqueries' bodies (they own their own scope) but covers
/// SELECT items, WHERE, GROUP BY, HAVING, ORDER BY, and the
/// outer FROM clause's join ON predicates. Pulled-up join names
/// (`__exsj_*` / `__cl1_*` / `__plj_*`) are NOT in `owner`, so this
/// pass is idempotent under re-runs.
fn disambiguate_stmt_unqualified_columns(
    stmt: &mut SelectStatement,
    owner: &alloc::collections::BTreeMap<String, String>,
) {
    for item in &mut stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            disambiguate_expr_unqualified_columns(expr, owner);
        }
    }
    if let Some(from) = &mut stmt.from {
        for j in &mut from.joins {
            if let Some(on) = &mut j.on {
                disambiguate_expr_unqualified_columns(on, owner);
            }
        }
    }
    if let Some(g) = &mut stmt.group_by {
        for e in g.iter_mut() {
            disambiguate_expr_unqualified_columns(e, owner);
        }
    }
    if let Some(h) = &mut stmt.having {
        disambiguate_expr_unqualified_columns(h, owner);
    }
    for ob in &mut stmt.order_by {
        disambiguate_expr_unqualified_columns(&mut ob.expr, owner);
    }
}

fn disambiguate_expr_unqualified_columns(
    e: &mut Expr,
    owner: &alloc::collections::BTreeMap<String, String>,
) {
    match e {
        Expr::Column(c) => {
            if c.qualifier.is_none()
                && let Some(alias) = owner.get(&c.name.to_ascii_lowercase())
            {
                c.qualifier = Some(alias.clone());
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            disambiguate_expr_unqualified_columns(lhs, owner);
            disambiguate_expr_unqualified_columns(rhs, owner);
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => {
            disambiguate_expr_unqualified_columns(expr, owner);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args.iter_mut() {
                disambiguate_expr_unqualified_columns(a, owner);
            }
        }
        Expr::AggregateOrdered {
            call,
            order_by,
            filter,
            ..
        } => {
            disambiguate_expr_unqualified_columns(call, owner);
            for ob in order_by.iter_mut() {
                disambiguate_expr_unqualified_columns(&mut ob.expr, owner);
            }
            if let Some(f) = filter {
                disambiguate_expr_unqualified_columns(f, owner);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            disambiguate_expr_unqualified_columns(expr, owner);
            disambiguate_expr_unqualified_columns(pattern, owner);
        }
        Expr::InList { expr, list, .. } => {
            disambiguate_expr_unqualified_columns(expr, owner);
            for it in list.iter_mut() {
                disambiguate_expr_unqualified_columns(it, owner);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                disambiguate_expr_unqualified_columns(o, owner);
            }
            for (w, t) in branches.iter_mut() {
                disambiguate_expr_unqualified_columns(w, owner);
                disambiguate_expr_unqualified_columns(t, owner);
            }
            if let Some(eb) = else_branch {
                disambiguate_expr_unqualified_columns(eb, owner);
            }
        }
        Expr::ArraySubscript { target, index } => {
            disambiguate_expr_unqualified_columns(target, owner);
            disambiguate_expr_unqualified_columns(index, owner);
        }
        // Subquery bodies own their own scope — leave untouched.
        _ => {}
    }
}

fn expr_is_all_inner(e: &Expr, inner_alias: &str) -> bool {
    let mut cols: Vec<ColumnName> = Vec::new();
    let mut subs: Vec<&SelectStatement> = Vec::new();
    visit_expr_columns_and_subqueries(e, &mut |c| cols.push(c.clone()), &mut |s| subs.push(s));
    subs.is_empty()
        && cols.iter().all(|c| {
            c.qualifier
                .as_deref()
                .is_some_and(|q| q.eq_ignore_ascii_case(inner_alias))
        })
}

/// v7.33 — rename every column qualifier equal to `from` into `to` in
/// place. Used to retarget an inner subquery's predicates from its
/// original table alias onto the fresh LEFT JOIN alias.
fn rename_qualifier(e: &mut Expr, from: &str, to: &str) {
    match e {
        Expr::Column(c) => {
            if c.qualifier
                .as_deref()
                .is_some_and(|q| q.eq_ignore_ascii_case(from))
            {
                c.qualifier = Some(to.into());
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            rename_qualifier(lhs, from, to);
            rename_qualifier(rhs, from, to);
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => {
            rename_qualifier(expr, from, to);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args.iter_mut() {
                rename_qualifier(a, from, to);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            rename_qualifier(expr, from, to);
            rename_qualifier(pattern, from, to);
        }
        Expr::InList { expr, list, .. } => {
            rename_qualifier(expr, from, to);
            for it in list.iter_mut() {
                rename_qualifier(it, from, to);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                rename_qualifier(o, from, to);
            }
            for (w, t) in branches.iter_mut() {
                rename_qualifier(w, from, to);
                rename_qualifier(t, from, to);
            }
            if let Some(eb) = else_branch {
                rename_qualifier(eb, from, to);
            }
        }
        _ => {}
    }
}

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
/// v7.37.x (docker-fair SCALARSQ attack) — pre-analysed plan for the
/// `(SELECT COUNT(*) FROM T WHERE T.pk = outer.col)` correlated
/// scalar subquery shape. Computing the table + index + position
/// lookups once per query (instead of once per outer row) drops the
/// per-row work to a single column read + index probe.
#[derive(Debug, Clone)]
pub struct ScalarPkProbeFastPath {
    /// Position in the OUTER scan schema for the column that drives
    /// the equality. Per row we read `row.values[outer_pos]` directly.
    pub outer_pos: usize,
    /// Catalog-qualified name of the inner table (looked up per probe).
    pub inner_table_name: String,
    /// Column position of the inner-side PK on which we probe.
    pub inner_pos: usize,
    /// v7.37.42 (docker-fair SCALARSQ attack 1) — cached insertion-order
    /// index of `inner_table_name` in the active catalog at PREPARE time.
    /// The executor and prepare share a single engine `RwLock` read guard
    /// per query (see `pgwire.rs` simple-query path), so the catalog
    /// can't mutate mid-query — the cached index stays in sync with the
    /// string name. The per-row probe therefore skips the
    /// `BTreeMap<String, usize>` descent that `Catalog::get(&str)` would
    /// otherwise perform, saving ~300 ns × N outer rows.
    pub table_idx: usize,
}

impl ScalarPkProbeFastPath {
    /// Per-row probe. Reads `row.values[self.outer_pos]`, looks up the
    /// inner table and PK index, and returns `Int(1)` on a hit or
    /// `Int(0)` on a miss / NULL outer key.
    pub fn probe(&self, row: &Row<'static>) -> Value<'static> {
        // The engine handle is needed to access the live catalog. The
        // probe is called from the run-loop with the engine in scope,
        // so we look up the catalog via a thread_local-cached
        // borrow. Simpler: defer to the engine helper that takes the
        // pre-analysed plan + the row. Kept here as a vtable-style
        // entry point so the run-loop's hot path is small.
        let outer_int = match row.values.get(self.outer_pos) {
            Some(Value::BigInt(n)) => *n,
            Some(Value::Int(n)) => i64::from(*n),
            Some(Value::SmallInt(n)) => i64::from(*n),
            Some(Value::Null) | None => return Value::BigInt(0),
            _ => return Value::BigInt(0),
        };
        SCALARSQ_PK_PROBE_PLAN_OUTER_INT.store(outer_int, core::sync::atomic::Ordering::Relaxed);
        SCALARSQ_PK_PROBE_PLAN_FIRED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        // The actual seek lives in `Engine::probe_with_pk_fast_path` —
        // we can't carry an engine borrow here without a lifetime
        // round-trip. Returning BigInt(0) as a placeholder would break
        // semantics; instead the run-loop calls
        // `engine.probe_with_pk_fast_path(&self, row)` directly so
        // the plan's `probe()` method is used only in tests where
        // the table data isn't load-bearing.
        Value::BigInt(0)
    }
}

/// v7.37.x — per-row hit counter for the plan-cached fast path.
pub static SCALARSQ_PK_PROBE_PLAN_FIRED: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static SCALARSQ_PK_PROBE_PLAN_OUTER_INT: core::sync::atomic::AtomicI64 =
    core::sync::atomic::AtomicI64::new(0);

/// v7.37.x (docker-fair SCALARSQ attack) — direct PK probe for the
/// `(SELECT COUNT(*) FROM T WHERE T.pk = outer.col)` correlated
/// scalar subquery shape. Returns `Some(BigInt(0))` if the probe misses
/// or `Some(BigInt(1))` if it hits; `None` when the shape doesn't match
/// (caller falls back to per-row exec). Bypasses parse / resolve /
/// plan / aggregate; the SCALARSQ docker-fair bench drops from
/// per-row ~3 µs to per-row ~100 ns.
impl Engine {
    /// Run a pre-analysed PK probe against the live catalog. Used by
    /// the per-row projection fast path to avoid going through
    /// `eval_expr_with_correlated`.
    pub(crate) fn probe_with_pk_fast_path(
        &self,
        plan: &ScalarPkProbeFastPath,
        row: &Row<'static>,
    ) -> Value<'static> {
        let outer_int = match row.values.get(plan.outer_pos) {
            Some(Value::BigInt(n)) => *n,
            Some(Value::Int(n)) => i64::from(*n),
            Some(Value::SmallInt(n)) => i64::from(*n),
            Some(Value::Null) | None => return Value::BigInt(0),
            _ => return Value::BigInt(0),
        };
        // v7.37.42 attack 1 — bypass per-row `BTreeMap<String,usize>::get`
        // by going through the cached positional index. The prepare-time
        // analyser stores the index against the same catalog snapshot
        // the executor sees (same engine read guard), so the cached
        // index remains valid for the query's duration.
        let Some(inner_table) = self.active_catalog().tables_at(plan.table_idx) else {
            return Value::BigInt(0);
        };
        let Some(idx) = inner_table.index_on(plan.inner_pos) else {
            return Value::BigInt(0);
        };
        let Some(key) = spg_storage::IndexKey::from_value(&Value::BigInt(outer_int)) else {
            return Value::BigInt(0);
        };
        let hit = !idx.lookup_eq(&key).is_empty();
        SCALARSQ_PK_PROBE_FIRED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        Value::BigInt(i64::from(hit))
    }

    /// Analyse a scalar subquery against the OUTER scan schema; return
    /// a `ScalarPkProbeFastPath` plan when the canonical shape is
    /// recognised, otherwise `None`. The outer alias and column-name
    /// resolution use the scan schema so the run-loop can read the
    /// outer value by position.
    pub(crate) fn analyse_scalar_count_pk_eq_probe(
        &self,
        inner: &SelectStatement,
        outer_schema: &[spg_storage::ColumnSchema],
        outer_alias: &str,
    ) -> Option<ScalarPkProbeFastPath> {
        use spg_sql::ast::{BinOp, ColumnName, SelectItem};
        if !inner.ctes.is_empty()
            || !inner.unions.is_empty()
            || inner.group_by.is_some()
            || inner.having.is_some()
            || inner.distinct
            || !inner.order_by.is_empty()
            || inner.limit.is_some()
            || inner.offset.is_some()
            || inner.items.len() != 1
        {
            return None;
        }
        let SelectItem::Expr { expr, .. } = &inner.items[0] else {
            return None;
        };
        let is_count_shape = match expr {
            Expr::FunctionCall { name, args } => {
                (name.eq_ignore_ascii_case("count_star") && args.is_empty())
                    || name.eq_ignore_ascii_case("count")
            }
            _ => false,
        };
        if !is_count_shape {
            return None;
        }
        let from = inner.from.as_ref()?;
        if !from.joins.is_empty()
            || from.primary.lateral_subquery.is_some()
            || from.primary.unnest_expr.is_some()
            || from.primary.generate_series_args.is_some()
            || from.primary.as_of_segment.is_some()
        {
            return None;
        }
        let inner_table_name = from.primary.name.clone();
        let inner_alias = from
            .primary
            .alias
            .as_deref()
            .unwrap_or(inner_table_name.as_str());
        let where_expr = inner.where_.as_ref()?;
        let Expr::Binary {
            lhs,
            op: BinOp::Eq,
            rhs,
        } = where_expr
        else {
            return None;
        };
        let (Expr::Column(a), Expr::Column(b)) = (lhs.as_ref(), rhs.as_ref()) else {
            return None;
        };
        let pick = |x: &ColumnName, y: &ColumnName| -> Option<(String, ColumnName)> {
            if x.qualifier
                .as_deref()
                .is_some_and(|q| q.eq_ignore_ascii_case(inner_alias))
            {
                Some((x.name.clone(), y.clone()))
            } else {
                None
            }
        };
        let (inner_col_name, outer_col) = pick(a, b).or_else(|| pick(b, a))?;
        // Outer column must be in the scan schema and qualified to
        // outer_alias (or unqualified).
        if let Some(q) = outer_col.qualifier.as_deref()
            && !q.eq_ignore_ascii_case(outer_alias)
        {
            return None;
        }
        let outer_pos = outer_schema
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&outer_col.name))?;
        // Inner column must be a single-column PK on an integer family.
        // v7.37.42 attack 1 — resolve the inner table's positional index
        // alongside the table fetch so the per-row probe can skip the
        // `BTreeMap<String,usize>::get(&str)` descent.
        let catalog = self.active_catalog();
        let table_idx = catalog.tables_position_of(inner_table_name.as_str())?;
        let inner_table = catalog.tables_at(table_idx)?;
        let inner_schema_ref = inner_table.schema();
        let inner_pos = inner_schema_ref
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&inner_col_name))?;
        if !matches!(
            inner_schema_ref.columns[inner_pos].ty,
            spg_storage::DataType::BigInt
                | spg_storage::DataType::Int
                | spg_storage::DataType::SmallInt
        ) {
            return None;
        }
        if !inner_schema_ref
            .uniqueness_constraints
            .iter()
            .any(|u| u.is_primary_key && u.columns.as_slice() == [inner_pos])
        {
            return None;
        }
        Some(ScalarPkProbeFastPath {
            outer_pos,
            inner_table_name,
            inner_pos,
            table_idx,
        })
    }

    pub(crate) fn try_scalar_count_pk_eq_probe(
        &self,
        inner: &SelectStatement,
        row: &Row<'static>,
        ctx: &EvalContext<'_>,
    ) -> Result<Option<Value<'static>>, EngineError> {
        use spg_sql::ast::{BinOp, ColumnName, SelectItem};
        if !inner.ctes.is_empty()
            || !inner.unions.is_empty()
            || inner.group_by.is_some()
            || inner.having.is_some()
            || inner.distinct
            || !inner.order_by.is_empty()
            || inner.limit.is_some()
            || inner.offset.is_some()
            || inner.items.len() != 1
        {
            return Ok(None);
        }
        let SelectItem::Expr { expr, .. } = &inner.items[0] else {
            return Ok(None);
        };
        let is_count_shape = match expr {
            Expr::FunctionCall { name, args } => {
                (name.eq_ignore_ascii_case("count_star") && args.is_empty())
                    || name.eq_ignore_ascii_case("count")
            }
            _ => false,
        };
        if !is_count_shape {
            return Ok(None);
        }
        let Some(from) = &inner.from else {
            return Ok(None);
        };
        if !from.joins.is_empty()
            || from.primary.lateral_subquery.is_some()
            || from.primary.unnest_expr.is_some()
            || from.primary.generate_series_args.is_some()
            || from.primary.as_of_segment.is_some()
        {
            return Ok(None);
        }
        let inner_table_name = from.primary.name.as_str();
        let inner_alias = from.primary.alias.as_deref().unwrap_or(inner_table_name);
        let Some(where_expr) = &inner.where_ else {
            return Ok(None);
        };
        let Expr::Binary {
            lhs,
            op: BinOp::Eq,
            rhs,
        } = where_expr
        else {
            return Ok(None);
        };
        let (Expr::Column(a), Expr::Column(b)) = (lhs.as_ref(), rhs.as_ref()) else {
            return Ok(None);
        };
        let pick = |x: &ColumnName, y: &ColumnName| -> Option<(String, ColumnName)> {
            if x.qualifier
                .as_deref()
                .is_some_and(|q| q.eq_ignore_ascii_case(inner_alias))
            {
                Some((x.name.clone(), y.clone()))
            } else {
                None
            }
        };
        let Some((inner_col_name, outer_col)) = pick(a, b).or_else(|| pick(b, a)) else {
            return Ok(None);
        };
        let catalog = self.active_catalog();
        let Some(inner_table) = catalog.get(inner_table_name) else {
            return Ok(None);
        };
        let inner_schema = inner_table.schema();
        let Some(inner_pos) = inner_schema
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&inner_col_name))
        else {
            return Ok(None);
        };
        if !matches!(
            inner_schema.columns[inner_pos].ty,
            spg_storage::DataType::BigInt
                | spg_storage::DataType::Int
                | spg_storage::DataType::SmallInt
        ) {
            return Ok(None);
        }
        if !inner_schema
            .uniqueness_constraints
            .iter()
            .any(|u| u.is_primary_key && u.columns.as_slice() == [inner_pos])
        {
            return Ok(None);
        }
        let outer_val = match eval::eval_expr(&Expr::Column(outer_col), row, ctx) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        let outer_int = match outer_val {
            Value::BigInt(n) => n,
            Value::Int(n) => i64::from(n),
            Value::SmallInt(n) => i64::from(n),
            Value::Null => return Ok(Some(Value::BigInt(0))),
            _ => return Ok(None),
        };
        let Some(idx) = inner_table.index_on(inner_pos) else {
            return Ok(None);
        };
        let Some(key) = spg_storage::IndexKey::from_value(&Value::BigInt(outer_int)) else {
            return Ok(None);
        };
        SCALARSQ_PK_PROBE_FIRED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let hit = !idx.lookup_eq(&key).is_empty();
        Ok(Some(Value::BigInt(i64::from(hit))))
    }
}

pub static SCALARSQ_PK_PROBE_FIRED: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// v7.37.x (docker-fair SCALARSQ attack) — return the SQL empty-set
/// default for a scalar subquery's output expression. PG semantics
/// distinguish `COUNT(*)` (0 over an empty set) from other aggregates
/// (NULL). Called by the batched ScalarSubquery resolver when a
/// per-outer-row probe finds no matching inner partition.
fn scalar_subquery_empty_default(inner: &SelectStatement) -> Value<'static> {
    use spg_sql::ast::SelectItem;
    if inner.items.len() != 1 {
        return Value::Null;
    }
    let SelectItem::Expr { expr, .. } = &inner.items[0] else {
        return Value::Null;
    };
    fn is_count(e: &Expr) -> bool {
        match e {
            // COUNT(*) parses as `count_star`; COUNT(col) as `count`.
            // Both have BIGINT-shaped empty-set default of 0.
            Expr::FunctionCall { name, .. } => {
                name.eq_ignore_ascii_case("count") || name.eq_ignore_ascii_case("count_star")
            }
            Expr::AggregateOrdered { call, .. } => is_count(call),
            _ => false,
        }
    }
    if is_count(expr) {
        // v7.39 (round 189) — count is BIGINT; the Int(0) default
        // leaked an integer-typed zero on the empty-set path.
        Value::BigInt(0)
    } else {
        Value::Null
    }
}

pub(crate) fn select_is_correlated(s: &SelectStatement) -> bool {
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
    // v7.39 (round 530) — a derived-table FROM entry used to answer "not
    // correlated" for the WHOLE subquery, on the grounds that its scope
    // was beyond this cheap check. The direction was backwards. An
    // uncorrelated subquery is evaluated ONCE and its answer reused for
    // every outer row, so a wrong "no" is silently wrong:
    //
    //   EXISTS(SELECT 1 FROM (SELECT 1 AS id) x WHERE t.id = x.id)
    //   PG18  true only for the matching row     SPG  true for EVERY row
    //
    // A wrong "yes" only costs a re-evaluation. So the derived entry's
    // alias joins the inner scope like any other name, and the ordinary
    // scan decides — plus the check below, since a derived body that is
    // itself correlated reaches outside its own scope.
    let mut inner: Vec<&str> = Vec::new();
    if let Some(a) = &from.primary.alias {
        inner.push(a.as_str());
    }
    if !from.primary.name.is_empty() {
        inner.push(from.primary.name.as_str());
    }
    for j in &from.joins {
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
    // A LATERAL body reads the row beside it, and its references never
    // appear in the expressions above — the visitor drops subquery
    // bodies. A body correlated against its OWN scope is reaching
    // further out, which is this statement's scope or beyond; either
    // way this statement has to be evaluated per row.
    if !correlated {
        for t in core::iter::once(&from.primary).chain(from.joins.iter().map(|j| &j.table)) {
            if let Some(body) = &t.lateral_subquery
                && select_is_correlated(body)
            {
                correlated = true;
                break;
            }
        }
    }
    correlated
}

/// v7.29 (3c) — pre-order collection of SCALAR subquery nodes in a
/// host expression (no descent into subquery bodies). The splice
/// walk below uses the same order; the pair must stay in lockstep.
pub(crate) fn collect_scalar_subqueries<'a>(e: &'a Expr, out: &mut Vec<&'a SelectStatement>) {
    match e {
        Expr::ScalarSubquery(s) => out.push(s),
        Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::RowInSubquery { .. }
        | Expr::RowCmpSubquery { .. } => {}
        Expr::Binary { lhs, rhs, .. } => {
            collect_scalar_subqueries(lhs, out);
            collect_scalar_subqueries(rhs, out);
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => {
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
        Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::RowInSubquery { .. }
        | Expr::RowCmpSubquery { .. } => {}
        Expr::Binary { lhs, rhs, .. } => {
            hollow_scalar_subqueries(lhs);
            hollow_scalar_subqueries(rhs);
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => {
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
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<bool, EngineError> {
    match e {
        Expr::ScalarSubquery(_) => {
            let Some(Some(gm)) = plan.get(*idx) else {
                return Ok(false);
            };
            *idx += 1;
            // v7.37.x (docker-fair SCALARSQ attack) — empty_default is
            // carried on the GroupMap (PG empty-set semantics: COUNT = 0,
            // others = NULL). The inner here may be HOLLOWED by the
            // template-rewrite step, so re-introspecting it for the
            // aggregate kind doesn't work — the construction-time
            // value on the GroupMap is the source of truth.
            let (outer_col, map, empty_default) = gm.as_ref();
            let key_v = eval::eval_expr(&Expr::Column(outer_col.clone()), row, ctx)
                .map_err(EngineError::Eval)?;
            let v = if matches!(key_v, Value::Null) {
                Value::Null
            } else {
                map.get(&aggregate::encode_key(core::slice::from_ref(&key_v)))
                    .cloned()
                    .unwrap_or_else(|| empty_default.clone())
            };
            *e = value_to_literal_expr(v)?;
            Ok(true)
        }
        Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::RowInSubquery { .. }
        | Expr::RowCmpSubquery { .. } => Ok(true),
        Expr::Binary { lhs, rhs, .. } => Ok(splice_planned_subqueries(lhs, plan, idx, row, ctx)?
            && splice_planned_subqueries(rhs, plan, idx, row, ctx)?),
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => {
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

/// v7.34.2 (EXISTS-FILTER baseline) — pre-order collect for EXISTS
/// subqueries. Mirrors `collect_scalar_subqueries` so the per-row
/// splice walker can re-traverse in the same order and pick the
/// matching planned set by ordinal index — no string repr, no
/// BTreeMap probe per row. ScalarSubquery / InSubquery nodes are
/// skipped here (they ride their own planners).
pub(crate) fn collect_exists_subqueries<'a>(e: &'a Expr, out: &mut Vec<&'a SelectStatement>) {
    match e {
        Expr::Exists { subquery, .. } => out.push(subquery.as_ref()),
        Expr::ScalarSubquery(_)
        | Expr::InSubquery { .. }
        | Expr::RowInSubquery { .. }
        | Expr::RowCmpSubquery { .. } => {}
        Expr::Binary { lhs, rhs, .. } => {
            collect_exists_subqueries(lhs, out);
            collect_exists_subqueries(rhs, out);
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => {
            collect_exists_subqueries(expr, out);
        }
        Expr::Like { expr, pattern, .. } => {
            collect_exists_subqueries(expr, out);
            collect_exists_subqueries(pattern, out);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_exists_subqueries(a, out);
            }
        }
        Expr::AggregateOrdered { call, order_by, .. } => {
            collect_exists_subqueries(call, out);
            for o in order_by {
                collect_exists_subqueries(&o.expr, out);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(op) = operand {
                collect_exists_subqueries(op, out);
            }
            for (w, t) in branches {
                collect_exists_subqueries(w, out);
                collect_exists_subqueries(t, out);
            }
            if let Some(eb) = else_branch {
                collect_exists_subqueries(eb, out);
            }
        }
        Expr::ArraySubscript { target, index } => {
            collect_exists_subqueries(target, out);
            collect_exists_subqueries(index, out);
        }
        Expr::InList { expr, list, .. } => {
            collect_exists_subqueries(expr, out);
            for item in list {
                collect_exists_subqueries(item, out);
            }
        }
        _ => {}
    }
}

/// v7.34.2 — per-row splice for the planned EXISTS sets. Walks the
/// (cloned) host expression in the SAME pre-order as
/// `collect_exists_subqueries`, increments `idx` past each EXISTS
/// node, and replaces it in place with `Bool(true/false)` derived
/// from the planned key-set + outer-row column values. Returns
/// `Ok(false)` when any encountered EXISTS lacks a planned set; the
/// caller falls back to the legacy per-row resolver path.
fn splice_planned_exists(
    e: &mut Expr,
    plan: &[Option<alloc::rc::Rc<memoize::ExistsSet>>],
    idx: &mut usize,
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<bool, EngineError> {
    match e {
        Expr::Exists { negated, .. } => {
            let Some(Some(es)) = plan.get(*idx) else {
                return Ok(false);
            };
            *idx += 1;
            let (outer_cols, set) = es.as_ref();
            let mut key_vals: Vec<Value<'static>> = Vec::with_capacity(outer_cols.len());
            let mut any_null = false;
            for oc in outer_cols {
                let v = eval::eval_expr(&Expr::Column(oc.clone()), row, ctx)
                    .map_err(EngineError::Eval)?;
                if matches!(v, Value::Null) {
                    any_null = true;
                }
                key_vals.push(v);
            }
            let present = !any_null && set.contains(&aggregate::encode_key(&key_vals));
            let bit = if *negated { !present } else { present };
            *e = Expr::Literal(Literal::Bool(bit));
            Ok(true)
        }
        Expr::ScalarSubquery(_)
        | Expr::InSubquery { .. }
        | Expr::RowInSubquery { .. }
        | Expr::RowCmpSubquery { .. } => Ok(true),
        Expr::Binary { lhs, rhs, .. } => Ok(splice_planned_exists(lhs, plan, idx, row, ctx)?
            && splice_planned_exists(rhs, plan, idx, row, ctx)?),
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => splice_planned_exists(expr, plan, idx, row, ctx),
        Expr::Like { expr, pattern, .. } => Ok(splice_planned_exists(expr, plan, idx, row, ctx)?
            && splice_planned_exists(pattern, plan, idx, row, ctx)?),
        Expr::FunctionCall { args, .. } => {
            for a in args.iter_mut() {
                if !splice_planned_exists(a, plan, idx, row, ctx)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Expr::AggregateOrdered { call, order_by, .. } => {
            if !splice_planned_exists(call, plan, idx, row, ctx)? {
                return Ok(false);
            }
            for o in order_by.iter_mut() {
                if !splice_planned_exists(&mut o.expr, plan, idx, row, ctx)? {
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
                if !splice_planned_exists(op, plan, idx, row, ctx)? {
                    return Ok(false);
                }
            }
            for (w, t) in branches.iter_mut() {
                if !splice_planned_exists(w, plan, idx, row, ctx)?
                    || !splice_planned_exists(t, plan, idx, row, ctx)?
                {
                    return Ok(false);
                }
            }
            if let Some(eb) = else_branch {
                if !splice_planned_exists(eb, plan, idx, row, ctx)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Expr::ArraySubscript { target, index } => {
            Ok(splice_planned_exists(target, plan, idx, row, ctx)?
                && splice_planned_exists(index, plan, idx, row, ctx)?)
        }
        Expr::InList { expr, list, .. } => {
            if !splice_planned_exists(expr, plan, idx, row, ctx)? {
                return Ok(false);
            }
            for item in list.iter_mut() {
                if !splice_planned_exists(item, plan, idx, row, ctx)? {
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


/// v7.39 (round 275) — is this cast target one of the integer widths
/// whose values all live in the same `InListSet::Int`?
fn cast_target_is_integer(target: &spg_sql::ast::CastTarget) -> bool {
    use spg_sql::ast::CastTarget;
    match target {
        CastTarget::BigInt | CastTarget::Int => true,
        CastTarget::Named(n) => {
            matches!(
                n.to_ascii_lowercase().as_str(),
                "int2" | "int4" | "int8" | "smallint" | "integer" | "int" | "bigint"
            )
        }
        _ => false,
    }
}

/// Analyse an `IN` list for set eligibility: every element a literal,
/// all of one family (integer or string, NULLs tracked separately).
pub(crate) fn build_in_list_set(list: &[Expr]) -> Option<memoize::InListSetEntry> {
    let mut has_null = false;
    let mut ints: hashbrown::HashSet<i64> = hashbrown::HashSet::with_capacity(list.len());
    let mut texts: hashbrown::HashSet<String> = hashbrown::HashSet::with_capacity(list.len());
    for item in list {
        // v7.39 (round 275) — see through the integer cast round 189
        // wraps a materialised BIGINT / SMALLINT subquery result in.
        // Before that round every element was a bare literal; after it
        // the elements of a pulled-up NOT EXISTS list are
        // `Expr::Cast { Literal::Integer, ::int8 }`, and requiring a
        // bare literal here silently dropped the whole set — the
        // membership probe fell back to an O(N x M) linear scan and the
        // mailrs content_worker shape went from 9 ms to 321 s.
        //
        // The set is keyed by VALUE, not by width: the probe side
        // already matches SmallInt / Int / BigInt against
        // `InListSet::Int`, so the cast carries nothing the set needs.
        let lit = match item {
            Expr::Literal(lit) => lit,
            Expr::Cast { expr, target } if cast_target_is_integer(target) => {
                match expr.as_ref() {
                    Expr::Literal(inner) => inner,
                    _ => return None,
                }
            }
            _ => return None,
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
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
    m: &mut memoize::MemoizeCache,
) -> Result<Value<'static>, EngineError> {
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
                (Value::Text(t), memoize::InListSet::Text(s)) => s.contains(t.as_ref()),
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

fn substitute_outer_columns(stmt: &mut SelectStatement, row: &Row<'static>, ctx: &EvalContext<'_>) {
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
    row: &Row<'static>,
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

fn substitute_in_expr(e: &mut Expr, row: &Row<'static>, ctx: &EvalContext<'_>, outer_alias: &str) {
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
        Expr::NamedArg { expr, .. } => substitute_in_expr(expr, row, ctx, outer_alias),
        Expr::Variadic(expr) => substitute_in_expr(expr, row, ctx, outer_alias),
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
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => {
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
        Expr::RowInSubquery {
            row: row_exprs,
            subquery,
            ..
        } => {
            for el in row_exprs.iter_mut() {
                substitute_in_expr(el, row, ctx, outer_alias);
            }
            substitute_in_select(subquery, row, ctx, outer_alias);
        }
        Expr::RowCmpSubquery {
            row: row_exprs,
            subquery,
            ..
        } => {
            for el in row_exprs.iter_mut() {
                substitute_in_expr(el, row, ctx, outer_alias);
            }
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
        Expr::ArraySlice { target, lo, hi } => {
            substitute_in_expr(target, row, ctx, outer_alias);
            if let Some(l) = lo {
                substitute_in_expr(l, row, ctx, outer_alias);
            }
            if let Some(h) = hi {
                substitute_in_expr(h, row, ctx, outer_alias);
            }
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
pub fn expr_tree_has_subquery(stmt: &SelectStatement) -> bool {
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
        Expr::NamedArg { expr, .. } => expr_has_subquery(expr),
        Expr::Variadic(expr) => expr_has_subquery(expr),
        Expr::ScalarSubquery(_)
        | Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::RowInSubquery { .. }
        | Expr::RowCmpSubquery { .. } => true,
        Expr::AggregateOrdered { call, order_by, .. } => {
            expr_has_subquery(call) || order_by.iter().any(|o| expr_has_subquery(&o.expr))
        }
        Expr::Binary { lhs, rhs, .. } => expr_has_subquery(lhs) || expr_has_subquery(rhs),
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => expr_has_subquery(expr),
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
        Expr::ArraySlice { target, lo, hi } => {
            expr_has_subquery(target)
                || lo.as_deref().is_some_and(expr_has_subquery)
                || hi.as_deref().is_some_and(expr_has_subquery)
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
