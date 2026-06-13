//! Aggregate executor.
//!
//! Handles `SELECT … <aggs> … [GROUP BY …]` queries. The planning strategy
//! is straightforward:
//!
//! 1. Walk the SELECT (and ORDER BY) expressions to find every aggregate
//!    function call. Dedupe by AST equality and assign each `__agg_<i>`.
//! 2. Same for every `GROUP BY` expression: assign `__grp_<j>`.
//! 3. Stream the WHERE-filtered rows, group by the tuple of GROUP BY
//!    values, and update per-group aggregate state.
//! 4. Materialise a synthetic per-group row containing
//!    `[__grp_0..__grp_K, __agg_0..__agg_N]` and rewrite the user's
//!    SELECT / ORDER BY expressions to reference those synthetic columns
//!    instead of the originals.
//! 5. Evaluate the rewritten expressions against the synthetic schema and
//!    emit results.
//!
//! v1.8 implements `count(*)`, `count(expr)`, `sum`, `min`, `max`, `avg`.
//! NULL semantics follow PG: aggregates skip NULL inputs (except
//! `count(*)`, which counts rows). `sum(int)` widens to `BigInt`;
//! `avg(int|bigint)` returns `Float`.

use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_sql::ast::{Expr, SelectItem, SelectStatement};
use spg_storage::{ColumnSchema, DataType, Row, Value};

use crate::eval::{self, EvalContext, EvalError};

/// True if this statement should go through the aggregate path.
pub fn uses_aggregate(stmt: &SelectStatement) -> bool {
    if stmt.group_by.is_some() || stmt.having.is_some() {
        return true;
    }
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item
            && contains_aggregate(expr)
        {
            return true;
        }
    }
    for o in &stmt.order_by {
        if contains_aggregate(&o.expr) {
            return true;
        }
    }
    if let Some(h) = &stmt.having
        && contains_aggregate(h)
    {
        return true;
    }
    false
}

pub fn contains_aggregate(e: &Expr) -> bool {
    match e {
        Expr::FunctionCall { name, args } => {
            is_aggregate_name(name) || args.iter().any(contains_aggregate)
        }
        Expr::AggregateOrdered { .. } => true,
        Expr::Binary { lhs, rhs, .. } => contains_aggregate(lhs) || contains_aggregate(rhs),
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            contains_aggregate(expr)
        }
        Expr::Like { expr, pattern, .. } => contains_aggregate(expr) || contains_aggregate(pattern),
        Expr::Extract { source, .. } => contains_aggregate(source),
        // v4.10 subqueries + v4.12 window functions / Literal /
        // Column — all non-aggregate leaves from the regular
        // aggregate planner's POV. Window-bearing projections are
        // routed to exec_select_with_window before this runs.
        Expr::ScalarSubquery(_)
        | Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::WindowFunction { .. }
        | Expr::Literal(_)
        | Expr::Placeholder(_)
        | Expr::Column(_) => false,
        // v7.10.10 — recurse into array constructor / subscript /
        // ANY/ALL children. Aggregates inside `ARRAY[SUM(x)]` are
        // valid PG and must be detected here.
        Expr::Array(items) => items.iter().any(contains_aggregate),
        Expr::ArraySubscript { target, index } => {
            contains_aggregate(target) || contains_aggregate(index)
        }
        Expr::AnyAll { expr, array, .. } => contains_aggregate(expr) || contains_aggregate(array),
        Expr::InList { expr, list, .. } => {
            contains_aggregate(expr) || list.iter().any(contains_aggregate)
        }
        // v7.13.0 — CASE WHEN … END. Recurse into operand,
        // every (WHEN, THEN) pair, and the ELSE branch.
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            operand.as_deref().is_some_and(contains_aggregate)
                || branches
                    .iter()
                    .any(|(w, t)| contains_aggregate(w) || contains_aggregate(t))
                || else_branch.as_deref().is_some_and(contains_aggregate)
        }
    }
}

pub fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count"
            | "count_star"
            | "sum"
            | "min"
            | "max"
            | "avg"
            // v7.17.0 — variadic / collection aggregates. ORM
            // reports (Hibernate / Rails / Django) emit these in
            // GROUP BY rollups; pre-7.17 SPG hit "unknown
            // aggregate".
            | "string_agg"
            | "array_agg"
            // v7.17.0 — boolean aggregates. `every` is SQL-standard
            // alias for `bool_and`.
            | "bool_and"
            | "bool_or"
            | "every"
            // v7.32 (round-29) — statistical aggregates (every BI /
            // dashboard emits these in rollups).
            | "stddev" | "stddev_samp" | "stddev_pop"
            | "variance" | "var_samp" | "var_pop"
            // v7.32 (round-29) — bitwise aggregates.
            | "bit_and" | "bit_or" | "bit_xor"
            // v7.32 (round-29) — ordered-set aggregates (used with
            // `WITHIN GROUP (ORDER BY …)`).
            | "percentile_cont" | "percentile_disc" | "mode"
    )
}

/// v7.32 (round-29) — ordered-set aggregates: the value to aggregate
/// comes from the `WITHIN GROUP (ORDER BY …)` sort spec, and any
/// in-parens arguments are *direct* arguments (the percentile fraction).
/// `mode()` takes no direct argument.
pub fn is_ordered_set_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "percentile_cont" | "percentile_disc" | "mode"
    )
}

/// Per-aggregate running state.
#[derive(Debug, Default, Clone)]
struct AggState {
    count: i64,
    sum_int: i64,
    sum_float: f64,
    extreme: Option<Value>,
    use_float: bool,
    /// v7.17.0 — running collection for string_agg / array_agg.
    /// Each entry is one row's contribution (NULL preserved as
    /// `Value::Null`; string_agg's finalize step drops them, but
    /// array_agg keeps them). Pushing in insertion order matches
    /// PG behaviour when no `ORDER BY` is given inside the
    /// aggregate call.
    items: Vec<Value>,
    /// v7.25 (round-17) — per-group dedupe set for DISTINCT
    /// aggregates (encoded values; NULLs never reach it because
    /// the caller's skip runs after the per-aggregate NULL rules).
    seen: BTreeSet<String>,
    /// v7.24 (round-16 A) — per-item ORDER BY key tuples, parallel
    /// to `items` (pushed under the same skip/keep conditions).
    /// Empty when the aggregate carries no internal ordering.
    item_keys: Vec<Vec<Value>>,
    /// v7.17.0 — captured separator for string_agg. PG accepts a
    /// non-constant separator expression but in practice every
    /// caller passes a literal; the engine snapshots the last
    /// non-NULL text it sees, which matches PG's "use the latest
    /// row's value" behaviour.
    separator: Option<String>,
    /// v7.17.0 — running boolean accumulator for bool_and /
    /// bool_or / every. `None` until the first non-NULL input;
    /// at finalize None → SQL NULL.
    bool_acc: Option<bool>,
    /// v7.32 (round-29) — sum of squares for the variance / stddev
    /// family (`sum_float` carries the running sum; `count` the n).
    sum_sq: f64,
    /// v7.32 (round-29) — running accumulator for bit_and / bit_or /
    /// bit_xor. `None` until the first non-NULL input → SQL NULL.
    bit_acc: Option<i64>,
}

#[derive(Debug, Clone)]
struct AggSpec {
    name: String, // lowercased
    /// First argument (value expression) for every aggregate
    /// except `count(*)`. `None` for `count_star`.
    arg: Option<Expr>,
    /// v7.17.0 — second argument. Only `string_agg(value, sep)`
    /// uses it today. `None` for every other aggregate (or for
    /// `array_agg`, which is single-arg). Carried in the spec so
    /// per-row evaluation can re-use the same separator
    /// expression across calls.
    arg2: Option<Expr>,
    /// v7.25 (round-17) — `COUNT(DISTINCT x)` & friends: dedupe
    /// the input stream per group before accumulation.
    distinct: bool,
    /// v7.24 (round-16 A) — aggregate-internal ORDER BY keys
    /// (`array_agg(x ORDER BY y DESC NULLS LAST)`). Empty for the
    /// plain form. Only the collection aggregates honour it;
    /// other aggregates are order-insensitive and ignore it (PG
    /// accepts the syntax everywhere too).
    order_by: Vec<spg_sql::ast::OrderBy>,
    /// v7.32 (round-29) — `FILTER (WHERE cond)`: a per-row predicate
    /// evaluated against the source row before accumulation. A row
    /// whose `cond` is not TRUE (false or NULL) is excluded from this
    /// aggregate only. `None` for the unfiltered form.
    filter: Option<Expr>,
    /// v7.32 (round-29) — ordered-set aggregates only: the *direct*
    /// argument (the percentile fraction for `percentile_cont/disc`).
    /// PG requires it constant, so it is evaluated once. `None` for
    /// `mode()` and for every non-ordered-set aggregate.
    direct_arg: Option<Expr>,
}

/// Output of running the aggregate path. Schema describes one row per
/// group; rows are not yet ORDER BY-sorted (caller does it).
#[derive(Debug)]
pub struct AggResult {
    pub columns: Vec<ColumnSchema>,
    pub rows: Vec<Row>,
    /// v7.31 (perf — PG lesson #1, post-LIMIT subquery projection):
    /// select-list items whose rewritten expr carries a subquery and
    /// is referenced by neither ORDER BY nor HAVING. Their output
    /// cells hold NULL placeholders; the caller truncates to
    /// LIMIT+OFFSET first and only then evaluates these for the
    /// surviving rows (PG runs the same shape with SubPlan loops=50
    /// instead of loops=24000). `(output_col, rewritten_expr)`.
    pub deferred: Vec<(usize, Expr)>,
    /// Synthetic group rows aligned 1:1 with `rows`; populated only
    /// when `deferred` is non-empty.
    pub synth_rows: Vec<Row>,
    /// Schema the deferred exprs evaluate against.
    pub synth_schema: Vec<ColumnSchema>,
}

/// Execute aggregate logic against an already-WHERE-filtered iterator of
/// rows. `table_alias` is the alias accepted by column resolution.
#[allow(clippy::too_many_lines)]
/// v7.25.2 (round-19 A) — caller-injected evaluator for synth-row
/// expressions that still carry subquery nodes after the rewrite
/// (correlated subqueries in the select list / HAVING / aggregate
/// ORDER BY of a GROUP BY query). The engine passes its
/// correlated-aware evaluator; pure-library callers pass None and
/// surviving subqueries keep erroring loudly.
pub type CorrelatedEval<'a> = &'a dyn Fn(&Expr, &Row, &EvalContext<'_>) -> Result<Value, EvalError>;

pub fn run(
    stmt: &SelectStatement,
    rows: &[&Row],
    schema_cols: &[ColumnSchema],
    table_alias: Option<&str>,
    correlated_eval: Option<CorrelatedEval<'_>>,
) -> Result<AggResult, EvalError> {
    let ctx = EvalContext::new(schema_cols, table_alias);
    let group_exprs: Vec<Expr> = stmt.group_by.clone().unwrap_or_default();

    // Collect aggregate sub-expressions across items + order_by.
    let mut agg_specs: Vec<AggSpec> = Vec::new();
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            collect_aggregates(expr, &mut agg_specs);
        }
    }
    for o in &stmt.order_by {
        collect_aggregates(&o.expr, &mut agg_specs);
    }
    if let Some(h) = &stmt.having {
        collect_aggregates(h, &mut agg_specs);
    }
    // v7.17.0 — arity validation. The collector tolerates an
    // arbitrary positional-arg count; here we enforce the
    // per-aggregate contract so a malformed call (e.g.
    // `array_agg()` or `string_agg(x)`) surfaces as a SQL error
    // rather than silently coercing to a degenerate aggregate.
    validate_agg_arities(stmt, &agg_specs)?;
    // v7.32 (round-29) — ordered-set aggregates require WITHIN GROUP
    // (PG raises a hard error otherwise rather than silently degrading).
    for spec in &agg_specs {
        if is_ordered_set_name(&spec.name) {
            if spec.order_by.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "{}() is an ordered-set aggregate and requires WITHIN GROUP (ORDER BY …)",
                        spec.name
                    ),
                });
            }
            if spec.name != "mode" && spec.direct_arg.is_none() {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{}() requires a single fraction argument", spec.name),
                });
            }
        }
    }

    // Map group key (vec of values, encoded as canonical string) -> group state.
    // v7.32 (architecture v2, P2b) — insertion-ordered group state in
    // a Vec; the hash map only maps key → index. Removes the parallel
    // `key_order: Vec<String>` (a second per-group key clone) and the
    // per-group re-probe `groups[k]` at finalize (24k hash lookups for
    // the inbox shape). The map owns its key once on vacant insert.
    let mut order: Vec<(Vec<Value>, Vec<AggState>)> = Vec::new();
    let mut groups: hashbrown::HashMap<String, usize> = hashbrown::HashMap::new();
    // When there are no GROUP BY exprs *and* there is at least one aggregate,
    // every row collapses into a single anonymous group keyed by "".
    if rows.is_empty() && group_exprs.is_empty() {
        // Single empty-aggregate group: count=0, sum=0, max=NULL, etc.
        // No rows follow, so the map is never probed — seed `order` only.
        let init: Vec<AggState> = (0..agg_specs.len()).map(|_| AggState::default()).collect();
        order.push((Vec::new(), init));
    }

    // v7.30 (perf campaign) - hoist the per-row work that doesn't
    // depend on the row: which group exprs need collation folding
    // (none, for most queries - the old code cloned the whole
    // group_vals vec per row just in case).
    // v7.30 (perf campaign) - the no-tax row loop. When a group
    // expr or an aggregate argument is a bare column reference
    // (the overwhelmingly common shape), bind its position ONCE
    // and read row cells by offset in the loop - no per-row tree
    // walk, no owned-Value clone out of resolve_column. Anything
    // more complex keeps the eval path.
    let col_pos = |e: &Expr| -> Option<usize> {
        // Qualified references only: the bare-name resolver carries
        // alias/ambiguity logic the bind-once path must not fork.
        if let Expr::Column(c) = e
            && c.qualifier.is_some()
        {
            eval::find_column_pos(c, &ctx)
        } else {
            None
        }
    };
    let group_pos: Vec<Option<usize>> = group_exprs.iter().map(col_pos).collect();
    let all_groups_bound = group_pos.iter().all(Option::is_some);
    let arg_pos: Vec<Option<usize>> = agg_specs
        .iter()
        .map(|spec| spec.arg.as_ref().and_then(|e| col_pos(e)))
        .collect();
    let ci_positions: Vec<usize> = group_exprs
        .iter()
        .enumerate()
        .filter(|(_, g)| {
            matches!(
                eval::column_collation(g, &ctx),
                Some(spg_storage::Collation::CaseInsensitive)
            )
        })
        .map(|(i, _)| i)
        .collect();
    // v7.31 (perf 3e) — per-row scratch buffers. The fast path used
    // to allocate a key String (and a refs Vec) for EVERY row just
    // to probe the group map; hits — the overwhelming case — now
    // touch the allocator zero times.
    let mut keybuf_s = String::new();
    let mut dkeybuf = String::new();
    let mut refs: Vec<&Value> = Vec::with_capacity(group_pos.len());
    for row in rows {
        // Fast key: bound positions + no ci folding -> encode
        // straight from borrowed cells; group_vals materialise
        // only when the group is NEW.
        if all_groups_bound && ci_positions.is_empty() && !group_exprs.is_empty() {
            refs.clear();
            refs.extend(
                group_pos
                    .iter()
                    .map(|p| row.values.get(p.unwrap()).unwrap_or(&Value::Null)),
            );
            encode_key_refs_into(&refs, &mut keybuf_s);
            let idx = match groups.get(keybuf_s.as_str()) {
                Some(&i) => i,
                None => {
                    let i = order.len();
                    let init: Vec<AggState> =
                        (0..agg_specs.len()).map(|_| AggState::default()).collect();
                    let owned: Vec<Value> = refs.iter().map(|v| (*v).clone()).collect();
                    order.push((owned, init));
                    groups.insert(keybuf_s.clone(), i);
                    i
                }
            };
            let entry = &mut order[idx];
            for (i, spec) in agg_specs.iter().enumerate() {
                // v7.32 (round-29) — FILTER (WHERE cond): exclude rows
                // where cond is not TRUE before they reach this
                // aggregate's accumulator (and before DISTINCT dedup).
                if let Some(f) = &spec.filter
                    && !matches!(eval::eval_expr(f, row, &ctx)?, Value::Bool(true))
                {
                    continue;
                }
                let arg_owned: Value;
                let arg_ref: &Value = match (&arg_pos[i], &spec.arg) {
                    (Some(p), _) => row.values.get(*p).unwrap_or(&Value::Null),
                    (None, None) => {
                        arg_owned = Value::Bool(true);
                        &arg_owned
                    }
                    (None, Some(e)) => {
                        arg_owned = eval::eval_expr(e, row, &ctx)?;
                        &arg_owned
                    }
                };
                let arg2_val = match &spec.arg2 {
                    None => None,
                    Some(e) => Some(eval::eval_expr(e, row, &ctx)?),
                };
                let order_keys = if spec.order_by.is_empty() {
                    None
                } else {
                    let mut keys = Vec::with_capacity(spec.order_by.len());
                    for o in &spec.order_by {
                        keys.push(eval::eval_expr(&o.expr, row, &ctx)?);
                    }
                    Some(keys)
                };
                if spec.distinct {
                    encode_key_refs_into(core::slice::from_ref(&arg_ref), &mut dkeybuf);
                    if entry.1[i].seen.contains(dkeybuf.as_str()) {
                        continue;
                    }
                    entry.1[i].seen.insert(dkeybuf.clone());
                }
                update_state(
                    &mut entry.1[i],
                    &spec.name,
                    arg_ref,
                    arg2_val.as_ref(),
                    order_keys,
                )?;
            }
            continue;
        }
        let group_vals: Vec<Value> = group_exprs
            .iter()
            .map(|g| eval::eval_expr(g, row, &ctx))
            .collect::<Result<_, _>>()?;
        // v7.17.0 Phase 2.5b — case-insensitive group keying: fold
        // only the ci columns, and only when any exist. Display
        // value (`group_vals`) stays original — only the key folds.
        let key = if ci_positions.is_empty() {
            encode_key(&group_vals)
        } else {
            let mut key_vals = group_vals.clone();
            for &i in &ci_positions {
                if let Value::Text(s) = &key_vals[i] {
                    key_vals[i] = Value::Text(s.to_ascii_lowercase());
                }
            }
            encode_key(&key_vals)
        };
        // Probe by index; the map owns the key once on vacant insert.
        let idx = match groups.get(key.as_str()) {
            Some(&i) => i,
            None => {
                let i = order.len();
                let init: Vec<AggState> =
                    (0..agg_specs.len()).map(|_| AggState::default()).collect();
                order.push((group_vals.clone(), init));
                groups.insert(key, i);
                i
            }
        };
        let entry = &mut order[idx];
        for (i, spec) in agg_specs.iter().enumerate() {
            // v7.32 (round-29) — FILTER (WHERE cond): exclude rows where
            // cond is not TRUE before accumulation (and before DISTINCT).
            if let Some(f) = &spec.filter
                && !matches!(eval::eval_expr(f, row, &ctx)?, Value::Bool(true))
            {
                continue;
            }
            let arg_val = match &spec.arg {
                None => Value::Bool(true), // count_star: sentinel non-null
                Some(e) => eval::eval_expr(e, row, &ctx)?,
            };
            // v7.17.0 — `string_agg(value, separator)` evaluates the
            // separator per row but PG treats it as constant; we
            // pass the per-row value into update_state so a future
            // varying-separator caller still sees correct output,
            // even though SPG (like PG) only uses the most recent.
            let arg2_val = match &spec.arg2 {
                None => None,
                Some(e) => Some(eval::eval_expr(e, row, &ctx)?),
            };
            // v7.24 (round-16 A) — aggregate-internal ORDER BY:
            // evaluate the key tuple against the source row.
            let order_keys = if spec.order_by.is_empty() {
                None
            } else {
                let mut keys = Vec::with_capacity(spec.order_by.len());
                for o in &spec.order_by {
                    keys.push(eval::eval_expr(&o.expr, row, &ctx)?);
                }
                Some(keys)
            };
            // v7.25 (round-17) — DISTINCT: drop repeated inputs
            // before they reach the accumulator. NULLs flow through
            // (each aggregate's own NULL rule applies; PG also
            // treats NULL as a single distinct value for array_agg).
            if spec.distinct {
                let key = encode_key(core::slice::from_ref(&arg_val));
                if !entry.1[i].seen.insert(key) {
                    continue;
                }
            }
            update_state(
                &mut entry.1[i],
                &spec.name,
                &arg_val,
                arg2_val.as_ref(),
                order_keys,
            )?;
        }
    }

    // Build synthetic schema: __grp_0..K then __agg_0..N.
    let group_types: Vec<DataType> = if rows.is_empty() {
        // Use Text as a safe stand-in — empty result means schema isn't
        // observable. Avoids needing to evaluate group exprs on no row.
        group_exprs.iter().map(|_| DataType::Text).collect()
    } else {
        let probe = rows[0];
        group_exprs
            .iter()
            .map(|g| {
                eval::eval_expr(g, probe, &ctx).map(|v| v.data_type().unwrap_or(DataType::Text))
            })
            .collect::<Result<_, _>>()?
    };
    let agg_types: Vec<DataType> = agg_specs
        .iter()
        .map(|spec| infer_agg_type(spec, schema_cols))
        .collect();
    let mut synth_schema: Vec<ColumnSchema> = Vec::new();
    for (i, ty) in group_types.iter().enumerate() {
        synth_schema.push(ColumnSchema::new(format!("__grp_{i}"), *ty, true));
    }
    for (i, ty) in agg_types.iter().enumerate() {
        synth_schema.push(ColumnSchema::new(format!("__agg_{i}"), *ty, true));
    }

    // v7.32 (round-29) — ordered-set direct arguments (the percentile
    // fraction) are constant per PG, so evaluate each once up front.
    let direct_arg_vals: Vec<Option<Value>> = agg_specs
        .iter()
        .map(|spec| match (&spec.direct_arg, rows.first()) {
            (Some(e), Some(r)) => eval::eval_expr(e, r, &ctx).map(Some),
            _ => Ok(None),
        })
        .collect::<Result<_, _>>()?;

    // Materialise synthetic rows (insertion order = `order`).
    let mut synth_rows: Vec<Row> = Vec::new();
    for (gvals, states) in &order {
        let mut values: Vec<Value> = Vec::with_capacity(synth_schema.len());
        values.extend(gvals.iter().cloned());
        for (i, st) in states.iter().enumerate() {
            // v7.24 (round-16 A) — order the collected items per the
            // aggregate-internal ORDER BY before finalize consumes
            // them.
            let st_sorted;
            let st_final: &AggState =
                if !agg_specs[i].order_by.is_empty() && st.item_keys.len() == st.items.len() {
                    let mut idx: Vec<usize> = (0..st.items.len()).collect();
                    let ob = &agg_specs[i].order_by;
                    idx.sort_by(|&x, &y| {
                        for (k, o) in ob.iter().enumerate() {
                            let cmp = crate::order_by_value_cmp(
                                o.desc,
                                o.nulls_first,
                                &st.item_keys[x][k],
                                &st.item_keys[y][k],
                            );
                            if cmp != core::cmp::Ordering::Equal {
                                return cmp;
                            }
                        }
                        core::cmp::Ordering::Equal
                    });
                    let mut sorted = st.clone();
                    sorted.items = idx.iter().map(|&j| st.items[j].clone()).collect();
                    st_sorted = sorted;
                    &st_sorted
                } else {
                    st
                };
            // Ordered-set aggregates compute from the sorted items + the
            // direct fraction; everything else uses the running state.
            let v = if is_ordered_set_name(&agg_specs[i].name) {
                finalize_ordered_set(&agg_specs[i].name, st_final, direct_arg_vals[i].as_ref())
            } else {
                finalize(&agg_specs[i].name, st_final)
            };
            values.push(v);
        }
        synth_rows.push(Row::new(values));
    }

    // Rewrite the user's SELECT items + ORDER BY to reference synthetic
    // columns. After rewriting, every remaining `Expr::Column` must
    // resolve against the synthetic schema (i.e. must have been a GROUP
    // BY expression).
    let columns: Vec<ColumnSchema> = stmt
        .items
        .iter()
        .map(|item| match item {
            SelectItem::Wildcard => Err(EvalError::TypeMismatch {
                detail: "SELECT * with aggregates is not supported".into(),
            }),
            SelectItem::Expr { expr, alias } => {
                let rewritten = rewrite_expr(expr, &group_exprs, &agg_specs);
                let name = alias.clone().unwrap_or_else(|| expr.to_string());
                Ok(ColumnSchema::new(
                    name,
                    agg_or_group_type(&rewritten, &synth_schema),
                    true,
                ))
            }
        })
        .collect::<Result<_, _>>()?;

    // Project per synthetic row. HAVING filters out groups *before*
    // we keep the projected row — same semantics as PG: HAVING runs
    // against the aggregated row (so `HAVING count(*) > 1` works) and
    // sees only group-by'd columns plus aggregate values.
    let synth_ctx = EvalContext::new(&synth_schema, None);
    let having_rewritten = stmt
        .having
        .as_ref()
        .map(|h| rewrite_expr(h, &group_exprs, &agg_specs));
    // v7.30 (phase 3e-1) - rewrite SELECT items ONCE. This ran per
    // GROUP (23.5k x 9 items of AST cloning = ~48% of the inbox
    // query in sampled stacks); the rewrite is group-independent.
    // Stable addresses also let the per-expression subquery plans
    // (v7.29 3c) hit across groups instead of rebuilding.
    let items_rewritten: alloc::vec::Vec<Option<Expr>> = stmt
        .items
        .iter()
        .map(|item| match item {
            SelectItem::Expr { expr, .. } => Some(rewrite_expr(expr, &group_exprs, &agg_specs)),
            SelectItem::Wildcard => None,
        })
        .collect();
    // v7.31 (perf — PG lesson #1): subquery-bearing select items
    // deferred to post-LIMIT, when no sort/filter key can observe
    // them. ORDER BY rewrites are hoisted here so the safety check
    // and the sort below share one rewrite pass.
    let order_rewritten: Vec<Expr> = stmt
        .order_by
        .iter()
        .map(|o| rewrite_expr(&o.expr, &group_exprs, &agg_specs))
        .collect();
    let defer_enabled = correlated_eval.is_some()
        && !stmt.distinct
        && !having_rewritten
            .as_ref()
            .is_some_and(crate::expr_has_subquery)
        && !order_rewritten.iter().any(crate::expr_has_subquery);
    let deferred: Vec<(usize, Expr)> = if defer_enabled {
        items_rewritten
            .iter()
            .enumerate()
            .filter_map(|(i, r)| {
                r.as_ref()
                    .filter(|e| crate::expr_has_subquery(e))
                    .map(|e| (i, e.clone()))
            })
            .collect()
    } else {
        Vec::new()
    };
    // v7.32 (architecture v2, P2) — compile the per-group synth-row
    // expressions ONCE. The projection / HAVING here run per GROUP
    // (24k for the inbox shape) × per item; the rewritten exprs are
    // mostly `Column(__agg_N)` / `Column(__grp_K)` against the synth
    // schema — flat step programs, no tree walk per group.
    let having_compiled = having_rewritten
        .as_ref()
        .filter(|h| eval::fully_compilable(h))
        .map(|h| eval::compile_expr(h, &synth_ctx));
    let items_compiled: Vec<Option<eval::CompiledExpr>> = items_rewritten
        .iter()
        .enumerate()
        .map(|(i, r)| {
            r.as_ref()
                .filter(|e| !deferred.iter().any(|(c, _)| *c == i) && eval::fully_compilable(e))
                .map(|e| eval::compile_expr(e, &synth_ctx))
        })
        .collect();
    let mut kept_synth: Vec<Row> = Vec::new();
    let mut out_rows: Vec<Row> = Vec::new();
    let mut stack: Vec<Value> = Vec::new();
    for srow in synth_rows {
        if let Some(hc) = &having_compiled {
            let cond = eval::eval_compiled(hc, &srow, &synth_ctx, &mut stack)?;
            if !matches!(cond, Value::Bool(true)) {
                continue;
            }
        } else if let Some(h) = &having_rewritten {
            let cond = match correlated_eval {
                Some(f) if crate::expr_has_subquery(h) => f(h, &srow, &synth_ctx)?,
                _ => eval::eval_expr(h, &srow, &synth_ctx)?,
            };
            if !matches!(cond, Value::Bool(true)) {
                continue;
            }
        }
        let mut values: Vec<Value> = Vec::with_capacity(columns.len());
        for (i, rewritten) in items_rewritten.iter().enumerate() {
            let Some(rewritten) = rewritten else { continue };
            if deferred.iter().any(|(c, _)| *c == i) {
                values.push(Value::Null);
                continue;
            }
            values.push(if let Some(cc) = &items_compiled[i] {
                eval::eval_compiled(cc, &srow, &synth_ctx, &mut stack)?
            } else {
                match correlated_eval {
                    Some(f) if crate::expr_has_subquery(rewritten) => {
                        f(rewritten, &srow, &synth_ctx)?
                    }
                    _ => eval::eval_expr(rewritten, &srow, &synth_ctx)?,
                }
            });
        }
        kept_synth.push(srow);
        out_rows.push(Row::new(values));
    }

    // ORDER BY: evaluate the rewritten order_by against each synth row,
    // sort, then drop the keys. Limit is applied by the caller.
    if !stmt.order_by.is_empty() {
        // v6.4.0 — multi-key ORDER BY on aggregate output. Each key
        // gets its own rewrite + per-key DESC flag. (Rewrites hoisted
        // above as `order_rewritten` — shared with the deferral
        // safety check.)
        let keys_meta: Vec<(bool, Option<bool>)> = stmt
            .order_by
            .iter()
            .map(|o| (o.desc, o.nulls_first))
            .collect();
        // P2: compile order-by keys once (per-group sort keys are
        // the same `__agg_N` / `__grp_K` shape as the projection).
        let order_compiled: Vec<Option<eval::CompiledExpr>> = order_rewritten
            .iter()
            .map(|e| {
                Some(e)
                    .filter(|e| eval::fully_compilable(e))
                    .map(|e| eval::compile_expr(e, &synth_ctx))
            })
            .collect();
        // The synth row rides through the sort so deferred exprs can
        // evaluate against the surviving groups after the caller's
        // LIMIT truncation.
        let mut keystack: Vec<Value> = Vec::new();
        let mut tagged: Vec<(Vec<Value>, Row, Row)> = Vec::with_capacity(kept_synth.len());
        for (s, o) in kept_synth.into_iter().zip(out_rows) {
            let mut keys = Vec::with_capacity(order_rewritten.len());
            for (e, oc) in order_rewritten.iter().zip(&order_compiled) {
                keys.push(if let Some(oc) = oc {
                    eval::eval_compiled(oc, &s, &synth_ctx, &mut keystack)?
                } else {
                    match correlated_eval {
                        Some(f) if crate::expr_has_subquery(e) => f(e, &s, &synth_ctx)?,
                        _ => eval::eval_expr(e, &s, &synth_ctx)?,
                    }
                });
            }
            tagged.push((keys, s, o));
        }
        tagged.sort_by(|a, b| {
            use core::cmp::Ordering;
            for (i, (ka, kb)) in a.0.iter().zip(b.0.iter()).enumerate() {
                let (desc, nf) = keys_meta[i];
                let cmp = crate::order_by_value_cmp(desc, nf, ka, kb);
                if cmp != Ordering::Equal {
                    return cmp;
                }
            }
            Ordering::Equal
        });
        kept_synth = Vec::with_capacity(tagged.len());
        out_rows = Vec::with_capacity(tagged.len());
        for (_, s, o) in tagged {
            kept_synth.push(s);
            out_rows.push(o);
        }
    }

    let (synth_rows_out, synth_schema_out) = if deferred.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        (kept_synth, synth_schema.clone())
    };
    Ok(AggResult {
        columns,
        rows: out_rows,
        deferred,
        synth_rows: synth_rows_out,
        synth_schema: synth_schema_out,
    })
}

/// v7.17.0 — walk the statement again to validate the positional
/// arity of every aggregate call site. Done after AST collection
/// rather than inside `collect_aggregates` so the collector stays
/// infallible; callers in `run()` can do a single early-error
/// exit before any per-row work.
fn validate_agg_arities(stmt: &SelectStatement, _specs: &[AggSpec]) -> Result<(), EvalError> {
    fn walk(e: &Expr) -> Result<(), EvalError> {
        if let Expr::FunctionCall { name, args } = e {
            let lower = name.to_ascii_lowercase();
            let expected: Option<usize> = match lower.as_str() {
                "count_star" => Some(0),
                "count" | "sum" | "avg" | "min" | "max" | "array_agg"
                // v7.17.0 — boolean aggregates also take exactly
                // one arg. `every` is an alias normalised inside
                // collect_aggregates / rewrite_expr.
                | "bool_and" | "bool_or" | "every"
                // v7.32 (round-29) — statistical + bitwise aggregates
                // are single-argument.
                | "stddev" | "stddev_samp" | "stddev_pop"
                | "variance" | "var_samp" | "var_pop"
                | "bit_and" | "bit_or" | "bit_xor" => Some(1),
                "string_agg" => Some(2),
                _ => None,
            };
            if let Some(want) = expected
                && args.len() != want
            {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("{lower}() takes {want} arg(s), got {}", args.len()),
                });
            }
            for a in args {
                walk(a)?;
            }
        } else if let Expr::Binary { lhs, rhs, .. } = e {
            walk(lhs)?;
            walk(rhs)?;
        } else if let Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. } = e
        {
            walk(expr)?;
        }
        Ok(())
    }
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            walk(expr)?;
        }
    }
    for o in &stmt.order_by {
        walk(&o.expr)?;
    }
    if let Some(h) = &stmt.having {
        walk(h)?;
    }
    Ok(())
}

fn collect_aggregates(e: &Expr, out: &mut Vec<AggSpec>) {
    match e {
        // v7.24 (round-16 A) — ordered aggregate: register the inner
        // call's spec with the ordering attached.
        Expr::AggregateOrdered {
            call,
            order_by,
            distinct,
            filter,
        } => {
            if let Expr::FunctionCall { name, args } = call.as_ref() {
                let lower = name.to_ascii_lowercase();
                if is_aggregate_name(&lower) {
                    let canonical = if lower == "every" {
                        "bool_and".to_string()
                    } else {
                        lower
                    };
                    // Ordered-set aggregates (`percentile_cont(f)
                    // WITHIN GROUP (ORDER BY x)`) take the value to
                    // aggregate from the sort spec and the in-parens
                    // arg as the direct (fraction) argument.
                    let ordered_set = is_ordered_set_name(&canonical);
                    let (arg, direct_arg) = if ordered_set {
                        (order_by.first().map(|o| o.expr.clone()), args.first().cloned())
                    } else {
                        (args.first().cloned(), None)
                    };
                    let spec = AggSpec {
                        name: canonical,
                        arg,
                        arg2: if name.eq_ignore_ascii_case("string_agg") {
                            args.get(1).cloned()
                        } else {
                            None
                        },
                        distinct: *distinct,
                        order_by: order_by.clone(),
                        filter: filter.as_deref().cloned(),
                        direct_arg,
                    };
                    if !out.iter().any(|s| {
                        s.name == spec.name
                            && s.arg == spec.arg
                            && s.arg2 == spec.arg2
                            && s.distinct == spec.distinct
                            && s.order_by == spec.order_by
                            && s.filter == spec.filter
                            && s.direct_arg == spec.direct_arg
                    }) {
                        out.push(spec);
                    }
                    return;
                }
            }
            collect_aggregates(call, out);
            for o in order_by {
                collect_aggregates(&o.expr, out);
            }
        }
        Expr::FunctionCall { name, args } => {
            let lower = name.to_ascii_lowercase();
            if is_aggregate_name(&lower) {
                let arg = if lower == "count_star" {
                    None
                } else {
                    args.first().cloned()
                };
                // v7.17.0 — second positional arg for
                // `string_agg(value, separator)`. Everything else
                // ignores it.
                let arg2 = if lower == "string_agg" {
                    args.get(1).cloned()
                } else {
                    None
                };
                // v7.17.0 — `every` is the SQL-standard alias for
                // `bool_and`; collapse at collection time so
                // update_state / finalize need only one arm.
                let canonical = if lower == "every" {
                    "bool_and".to_string()
                } else {
                    lower
                };
                let spec = AggSpec {
                    name: canonical,
                    arg: arg.clone(),
                    arg2: arg2.clone(),
                    distinct: false,
                    order_by: Vec::new(),
                    filter: None,
                    direct_arg: None,
                };
                if !out.iter().any(|s| {
                    s.name == spec.name
                        && s.arg == spec.arg
                        && s.arg2 == spec.arg2
                        && !s.distinct
                        && s.order_by == spec.order_by
                        && s.filter.is_none()
                }) {
                    out.push(spec);
                }
                // Don't recurse into the arg — nested aggregates are
                // illegal in standard SQL.
            } else {
                for a in args {
                    collect_aggregates(a, out);
                }
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            collect_aggregates(lhs, out);
            collect_aggregates(rhs, out);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            collect_aggregates(expr, out);
        }
        Expr::Like { expr, pattern, .. } => {
            collect_aggregates(expr, out);
            collect_aggregates(pattern, out);
        }
        Expr::InList { expr, list, .. } => {
            collect_aggregates(expr, out);
            for item in list {
                collect_aggregates(item, out);
            }
        }
        Expr::Extract { source, .. } => collect_aggregates(source, out),
        // v4.10 subquery + v4.12 window / Literal / Column —
        // non-recursing leaves for the aggregate collector.
        Expr::ScalarSubquery(_)
        | Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::WindowFunction { .. }
        | Expr::Literal(_)
        | Expr::Placeholder(_)
        | Expr::Column(_) => {}
        // v7.10.10 — recurse into array constructor children +
        // subscript / ANY/ALL operands.
        Expr::Array(items) => {
            for elem in items {
                collect_aggregates(elem, out);
            }
        }
        Expr::ArraySubscript { target, index } => {
            collect_aggregates(target, out);
            collect_aggregates(index, out);
        }
        Expr::AnyAll { expr, array, .. } => {
            collect_aggregates(expr, out);
            collect_aggregates(array, out);
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                collect_aggregates(o, out);
            }
            for (w, t) in branches {
                collect_aggregates(w, out);
                collect_aggregates(t, out);
            }
            if let Some(e) = else_branch {
                collect_aggregates(e, out);
            }
        }
    }
}

fn update_state(
    st: &mut AggState,
    name: &str,
    v: &Value,
    arg2: Option<&Value>,
    order_keys: Option<Vec<Value>>,
) -> Result<(), EvalError> {
    let is_null = matches!(v, Value::Null);
    match name {
        "count_star" => st.count += 1,
        "count" => {
            if !is_null {
                st.count += 1;
            }
        }
        "sum" | "avg" => {
            if is_null {
                return Ok(());
            }
            st.count += 1;
            match v {
                Value::Int(n) => st.sum_int += i64::from(*n),
                Value::BigInt(n) => st.sum_int += *n,
                Value::Float(x) => {
                    st.use_float = true;
                    st.sum_float += *x;
                }
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!("sum/avg need numeric, got {:?}", other.data_type()),
                    });
                }
            }
        }
        "min" => {
            if is_null {
                return Ok(());
            }
            match &st.extreme {
                None => st.extreme = Some(v.clone()),
                Some(cur) => {
                    if value_cmp(v, cur) == core::cmp::Ordering::Less {
                        st.extreme = Some(v.clone());
                    }
                }
            }
        }
        "max" => {
            if is_null {
                return Ok(());
            }
            match &st.extreme {
                None => st.extreme = Some(v.clone()),
                Some(cur) => {
                    if value_cmp(v, cur) == core::cmp::Ordering::Greater {
                        st.extreme = Some(v.clone());
                    }
                }
            }
        }
        // v7.17.0 — string_agg(value, separator). NULL value is
        // skipped (PG aggregate-skip-null). Separator captured
        // from the latest row that flows through; matches PG's
        // semantics of evaluating the separator per row but using
        // the last value at finalize time (in practice it's
        // constant). count is bumped so we can distinguish "empty
        // group → NULL" from "all-NULL group → NULL".
        "string_agg" => {
            if let Some(sep) = arg2
                && let Value::Text(s) = sep
            {
                st.separator = Some(s.clone());
            }
            if is_null {
                return Ok(());
            }
            if let Value::Text(s) = v {
                st.items.push(Value::Text(s.clone()));
                if let Some(k) = order_keys {
                    st.item_keys.push(k);
                }
                st.count += 1;
            } else {
                return Err(EvalError::TypeMismatch {
                    detail: format!("string_agg requires text value, got {:?}", v.data_type()),
                });
            }
        }
        // v7.17.0 — array_agg(value). Unlike string_agg, NULL
        // elements are KEPT in the array (PG behaviour); the
        // result is NULL only when ZERO rows fed in. Element type
        // is locked from the first row's value type; subsequent
        // rows must match (PG also rejects mixed-type array_agg).
        "array_agg" => {
            st.items.push(v.clone());
            if let Some(k) = order_keys {
                st.item_keys.push(k);
            }
            st.count += 1;
        }
        // v7.17.0 — bool_and(p): TRUE iff every non-NULL input is
        // TRUE. NULL skipped; running accumulator stays at TRUE
        // until the first non-NULL FALSE.
        "bool_and" => {
            if is_null {
                return Ok(());
            }
            let b = match v {
                Value::Bool(b) => *b,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!("bool_and requires bool, got {:?}", other.data_type()),
                    });
                }
            };
            st.bool_acc = Some(st.bool_acc.map_or(b, |acc| acc && b));
        }
        // v7.17.0 — bool_or(p): TRUE iff any non-NULL input is
        // TRUE. NULL skipped.
        "bool_or" => {
            if is_null {
                return Ok(());
            }
            let b = match v {
                Value::Bool(b) => *b,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!("bool_or requires bool, got {:?}", other.data_type()),
                    });
                }
            };
            st.bool_acc = Some(st.bool_acc.map_or(b, |acc| acc || b));
        }
        // v7.32 (round-29) — variance / stddev family. Accumulate the
        // running sum (sum_float) and sum of squares (sum_sq) over the
        // non-NULL numeric inputs; finalize divides by n or n-1.
        "stddev" | "stddev_samp" | "stddev_pop" | "variance" | "var_samp" | "var_pop" => {
            if is_null {
                return Ok(());
            }
            let x = match v {
                Value::Int(n) => f64::from(*n),
                Value::SmallInt(n) => f64::from(*n),
                Value::BigInt(n) => *n as f64,
                Value::Float(x) => *x,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!("{name} needs numeric, got {:?}", other.data_type()),
                    });
                }
            };
            st.count += 1;
            st.sum_float += x;
            st.sum_sq += x * x;
        }
        // v7.32 (round-29) — bitwise aggregates over integer inputs.
        "bit_and" | "bit_or" | "bit_xor" => {
            if is_null {
                return Ok(());
            }
            let n = match v {
                Value::Int(n) => i64::from(*n),
                Value::SmallInt(n) => i64::from(*n),
                Value::BigInt(n) => *n,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!("{name} needs integer, got {:?}", other.data_type()),
                    });
                }
            };
            st.bit_acc = Some(match (st.bit_acc, name) {
                (None, _) => n,
                (Some(acc), "bit_and") => acc & n,
                (Some(acc), "bit_or") => acc | n,
                (Some(acc), _) => acc ^ n, // bit_xor
            });
        }
        // v7.32 (round-29) — ordered-set aggregates collect the
        // WITHIN GROUP value (NULLs ignored, per PG) into `items`,
        // sorted at finalize by the parallel `item_keys`.
        "percentile_cont" | "percentile_disc" | "mode" => {
            if is_null {
                return Ok(());
            }
            st.items.push(v.clone());
            if let Some(k) = order_keys {
                st.item_keys.push(k);
            }
            st.count += 1;
        }
        _ => unreachable!("non-aggregate {name} in update_state"),
    }
    Ok(())
}

#[allow(clippy::cast_precision_loss)]
fn finalize(name: &str, st: &AggState) -> Value {
    match name {
        "count" | "count_star" => Value::BigInt(st.count),
        "sum" => {
            if st.count == 0 {
                Value::Null
            } else if st.use_float {
                Value::Float(st.sum_float + (st.sum_int as f64))
            } else {
                Value::BigInt(st.sum_int)
            }
        }
        "avg" => {
            if st.count == 0 {
                Value::Null
            } else {
                let total = if st.use_float {
                    st.sum_float + (st.sum_int as f64)
                } else {
                    st.sum_int as f64
                };
                Value::Float(total / (st.count as f64))
            }
        }
        "min" | "max" => st.extreme.clone().unwrap_or(Value::Null),
        // v7.17.0 — string_agg: join all collected text items with
        // the captured separator. Empty / all-NULL group → NULL
        // (PG semantics).
        "string_agg" => {
            if st.items.is_empty() {
                return Value::Null;
            }
            let sep = st.separator.clone().unwrap_or_default();
            let mut out = String::new();
            for (i, item) in st.items.iter().enumerate() {
                if i > 0 {
                    out.push_str(&sep);
                }
                if let Value::Text(s) = item {
                    out.push_str(s);
                }
            }
            Value::Text(out)
        }
        // v7.17.0 — array_agg: collect into a typed array. NULL
        // elements are preserved per PG. Result type is decided
        // by the first non-NULL element seen (or Text fallback
        // when the whole group is NULL — PG would surface the
        // declared input type, but SPG hasn't yet wired the
        // aggregate's static input-type from `describe`).
        "array_agg" => {
            if st.items.is_empty() {
                return Value::Null;
            }
            let probe = st.items.iter().find(|v| !v.is_null());
            match probe.and_then(spg_storage::Value::data_type) {
                Some(DataType::Int) | Some(DataType::SmallInt) => {
                    let items: Vec<Option<i32>> = st
                        .items
                        .iter()
                        .map(|v| match v {
                            Value::Int(n) => Some(*n),
                            Value::SmallInt(n) => Some(i32::from(*n)),
                            _ => None,
                        })
                        .collect();
                    Value::IntArray(items)
                }
                Some(DataType::BigInt) => {
                    let items: Vec<Option<i64>> = st
                        .items
                        .iter()
                        .map(|v| match v {
                            Value::BigInt(n) => Some(*n),
                            _ => None,
                        })
                        .collect();
                    Value::BigIntArray(items)
                }
                _ => {
                    let items: Vec<Option<String>> = st
                        .items
                        .iter()
                        .map(|v| match v {
                            Value::Text(s) => Some(s.clone()),
                            Value::Null => None,
                            other => Some(format!("{other:?}")),
                        })
                        .collect();
                    Value::TextArray(items)
                }
            }
        }
        // v7.17.0 — bool_and / bool_or finalize: lazy-init pattern
        // means `None` is exactly "empty group or all-NULL", which
        // PG surfaces as SQL NULL.
        "bool_and" | "bool_or" => st.bool_acc.map_or(Value::Null, Value::Bool),
        // v7.32 (round-29) — variance / stddev. PG: `variance` ==
        // `var_samp`, `stddev` == `stddev_samp`. samp needs n >= 2
        // (n < 2 → NULL); pop needs n >= 1 (n == 1 → 0).
        "variance" | "var_samp" | "var_pop" | "stddev" | "stddev_samp" | "stddev_pop" => {
            let n = st.count;
            if n == 0 {
                return Value::Null;
            }
            let nf = n as f64;
            // Sum of squared deviations from the mean.
            let ss = st.sum_sq - (st.sum_float * st.sum_float) / nf;
            let pop = name.ends_with("_pop");
            let denom = if pop { nf } else { nf - 1.0 };
            if denom <= 0.0 {
                // var_samp / stddev (samp) with n == 1 → NULL.
                return Value::Null;
            }
            let var = (ss / denom).max(0.0); // clamp fp noise below 0
            if name.starts_with("stddev") {
                Value::Float(crate::eval::f64_sqrt(var))
            } else {
                Value::Float(var)
            }
        }
        // v7.32 (round-29) — bitwise aggregates: None (empty / all-NULL)
        // → SQL NULL.
        "bit_and" | "bit_or" | "bit_xor" => st.bit_acc.map_or(Value::Null, Value::BigInt),
        // Ordered-set aggregates are finalized in `run` (they need the
        // sorted items + the direct fraction argument), never here.
        _ => unreachable!(),
    }
}

/// v7.32 (round-29) — numeric coercion for the percentile interpolation.
fn agg_value_to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(n) => Some(f64::from(*n)),
        Value::SmallInt(n) => Some(f64::from(*n)),
        Value::BigInt(n) => Some(*n as f64),
        Value::Float(x) => Some(*x),
        _ => None,
    }
}

/// v7.32 (round-29) — finalize an ordered-set aggregate. `st.items` is
/// already sorted by the `WITHIN GROUP (ORDER BY …)` spec. `fraction`
/// is the evaluated direct argument for `percentile_*` (ignored by
/// `mode`).
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn finalize_ordered_set(name: &str, st: &AggState, fraction: Option<&Value>) -> Value {
    let items = &st.items;
    if items.is_empty() {
        return Value::Null;
    }
    let n = items.len();
    match name {
        // Most frequent value; equal values are adjacent in the sorted
        // run, and a frequency tie resolves to the earliest run (the
        // smallest value under an ascending sort), matching PG.
        "mode" => {
            let (mut best_i, mut best_cnt) = (0usize, 1usize);
            let (mut run_i, mut run_cnt) = (0usize, 1usize);
            for i in 1..n {
                if value_cmp(&items[i], &items[run_i]) == core::cmp::Ordering::Equal {
                    run_cnt += 1;
                } else {
                    run_i = i;
                    run_cnt = 1;
                }
                if run_cnt > best_cnt {
                    best_cnt = run_cnt;
                    best_i = run_i;
                }
            }
            items[best_i].clone()
        }
        // The first value whose cumulative fraction reaches `f`.
        "percentile_disc" => {
            let f = fraction.and_then(agg_value_to_f64).unwrap_or(0.0).clamp(0.0, 1.0);
            let idx = if f <= 0.0 {
                0
            } else {
                (crate::eval::f64_ceil(f * n as f64) as usize)
                    .saturating_sub(1)
                    .min(n - 1)
            };
            items[idx].clone()
        }
        // Linear interpolation between the two bracketing values.
        "percentile_cont" => {
            let f = fraction.and_then(agg_value_to_f64).unwrap_or(0.0).clamp(0.0, 1.0);
            let Some(nums) = items.iter().map(agg_value_to_f64).collect::<Option<Vec<f64>>>() else {
                return Value::Null; // non-numeric ordered set
            };
            if n == 1 {
                return Value::Float(nums[0]);
            }
            let rank = f * (n as f64 - 1.0);
            let lo = crate::eval::f64_floor(rank) as usize;
            let hi = crate::eval::f64_ceil(rank) as usize;
            let frac = rank - lo as f64;
            Value::Float(nums[lo] + (nums[hi] - nums[lo]) * frac)
        }
        _ => unreachable!(),
    }
}

fn infer_agg_type(spec: &AggSpec, schema_cols: &[ColumnSchema]) -> DataType {
    // v7.26 (round-20 C) — the argument's statically-derived shape
    // types MIN/MAX/SUM/array_agg properly; RowDescription used to
    // report TEXT for these, breaking every sqlx typed decode.
    let arg_ty = spec
        .arg
        .as_ref()
        .and_then(|a| crate::describe::describe_expr(a, schema_cols))
        .map(|shape| shape.ty);
    match spec.name.as_str() {
        "count" | "count_star" => DataType::BigInt,
        "sum" => match arg_ty {
            Some(DataType::Float) => DataType::Float,
            _ => DataType::BigInt,
        },
        "avg" => DataType::Float,
        // v7.17.0 — string_agg always returns TEXT.
        "string_agg" => DataType::Text,
        "array_agg" => match arg_ty {
            Some(DataType::Int | DataType::SmallInt) => DataType::IntArray,
            Some(DataType::BigInt) => DataType::BigIntArray,
            _ => DataType::TextArray,
        },
        // v7.17.0 — boolean aggregates always return BOOL (nullable
        // — empty / all-NULL group → NULL).
        "bool_and" | "bool_or" => DataType::Bool,
        // v7.32 (round-29) — variance / stddev are floating point;
        // percentile_cont interpolates to float.
        "stddev" | "stddev_samp" | "stddev_pop" | "variance" | "var_samp" | "var_pop"
        | "percentile_cont" => DataType::Float,
        // v7.32 (round-29) — bitwise aggregates return an integer.
        "bit_and" | "bit_or" | "bit_xor" => DataType::BigInt,
        // min/max, percentile_disc, mode, and anything pass-through:
        // the argument's shape (for ordered-set aggs `spec.arg` is the
        // WITHIN GROUP value expression).
        _ => arg_ty.unwrap_or(DataType::Text),
    }
}

fn agg_or_group_type(e: &Expr, synth: &[ColumnSchema]) -> DataType {
    if let Expr::Column(c) = e
        && let Some(s) = synth.iter().find(|s| s.name == c.name)
    {
        return s.ty;
    }
    // v7.26 (round-20 C) — compound expressions over aggregates
    // (COALESCE(BOOL_OR(…), false), (array_agg(…))[1], CASE …)
    // derive their shape statically against the synth schema; the
    // old Text fallback broke sqlx typed decodes of exactly these
    // columns.
    crate::describe::describe_expr(e, synth)
        .map(|shape| shape.ty)
        .unwrap_or(DataType::Text)
}

fn rewrite_expr(e: &Expr, group_exprs: &[Expr], aggs: &[AggSpec]) -> Expr {
    // v7.24 (round-16 A) — ordered aggregate: match on the inner
    // call PLUS the ordering keys.
    if let Expr::AggregateOrdered {
        call,
        order_by,
        distinct,
        filter,
    } = e
        && let Expr::FunctionCall { name, args } = call.as_ref()
    {
        let lower = name.to_ascii_lowercase();
        if is_aggregate_name(&lower) {
            let canonical: &str = if lower == "every" { "bool_and" } else { &lower };
            // Mirror collect_aggregates: ordered-set aggregates take the
            // value from the sort spec and the in-parens arg as direct.
            let (arg, direct_arg) = if is_ordered_set_name(canonical) {
                (order_by.first().map(|o| o.expr.clone()), args.first().cloned())
            } else {
                (args.first().cloned(), None)
            };
            let arg2 = if lower == "string_agg" {
                args.get(1).cloned()
            } else {
                None
            };
            let filter_owned = filter.as_deref().cloned();
            for (i, spec) in aggs.iter().enumerate() {
                if spec.name == canonical
                    && spec.arg == arg
                    && spec.arg2 == arg2
                    && spec.distinct == *distinct
                    && spec.order_by == *order_by
                    && spec.filter == filter_owned
                    && spec.direct_arg == direct_arg
                {
                    return Expr::Column(spg_sql::ast::ColumnName {
                        qualifier: None,
                        name: format!("__agg_{i}"),
                    });
                }
            }
        }
    }
    // Match aggregate FunctionCalls first — they sit outside group_by.
    if let Expr::FunctionCall { name, args } = e {
        let lower = name.to_ascii_lowercase();
        if is_aggregate_name(&lower) {
            let arg = if lower == "count_star" {
                None
            } else {
                args.first().cloned()
            };
            // v7.17.0 — match the spec we registered for
            // string_agg(value, separator) on the full pair.
            let arg2 = if lower == "string_agg" {
                args.get(1).cloned()
            } else {
                None
            };
            // v7.17.0 — `every` collapses into `bool_and` at
            // collection; mirror that here so the rewrite finds
            // the matching synth column.
            let canonical: &str = if lower == "every" {
                "bool_and"
            } else {
                lower.as_str()
            };
            for (i, spec) in aggs.iter().enumerate() {
                if spec.name == canonical
                    && spec.arg == arg
                    && spec.arg2 == arg2
                    && !spec.distinct
                    && spec.order_by.is_empty()
                {
                    return Expr::Column(spg_sql::ast::ColumnName {
                        qualifier: None,
                        name: format!("__agg_{i}"),
                    });
                }
            }
        }
    }
    // Match a group_by expression by AST equality.
    for (i, g) in group_exprs.iter().enumerate() {
        if g == e {
            return Expr::Column(spg_sql::ast::ColumnName {
                qualifier: None,
                name: format!("__grp_{i}"),
            });
        }
    }
    // Recurse into children.
    match e {
        Expr::AggregateOrdered {
            call,
            order_by,
            distinct,
            filter,
        } => Expr::AggregateOrdered {
            call: Box::new(rewrite_expr(call, group_exprs, aggs)),
            distinct: *distinct,
            order_by: order_by
                .iter()
                .map(|o| spg_sql::ast::OrderBy {
                    expr: rewrite_expr(&o.expr, group_exprs, aggs),
                    desc: o.desc,
                    nulls_first: o.nulls_first,
                })
                .collect(),
            // The filter is evaluated against SOURCE rows during
            // accumulation, never against synth rows — keep it as-is.
            filter: filter.clone(),
        },
        Expr::Binary { lhs, op, rhs } => Expr::Binary {
            lhs: Box::new(rewrite_expr(lhs, group_exprs, aggs)),
            op: *op,
            rhs: Box::new(rewrite_expr(rhs, group_exprs, aggs)),
        },
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: Box::new(rewrite_expr(expr, group_exprs, aggs)),
        },
        Expr::Cast { expr, target } => Expr::Cast {
            expr: Box::new(rewrite_expr(expr, group_exprs, aggs)),
            target: *target,
        },
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(rewrite_expr(expr, group_exprs, aggs)),
            negated: *negated,
        },
        Expr::FunctionCall { name, args } => Expr::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| rewrite_expr(a, group_exprs, aggs))
                .collect(),
        },
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => Expr::Like {
            expr: Box::new(rewrite_expr(expr, group_exprs, aggs)),
            pattern: Box::new(rewrite_expr(pattern, group_exprs, aggs)),
            negated: *negated,
            case_insensitive: *case_insensitive,
        },
        Expr::Extract { field, source } => Expr::Extract {
            field: *field,
            source: Box::new(rewrite_expr(source, group_exprs, aggs)),
        },
        // v7.25.2 (round-19 A) — subquery nodes: rewrite group-key
        // references INSIDE the body to `__grp_N` so the correlated
        // resolver can substitute them against the synthesised group
        // row (aggs are NOT matched inside the body — a COUNT in the
        // subquery is the subquery's own aggregate).
        Expr::ScalarSubquery(s) => {
            Expr::ScalarSubquery(Box::new(rewrite_group_keys_in_select(s, group_exprs)))
        }
        Expr::Exists { subquery, negated } => Expr::Exists {
            subquery: Box::new(rewrite_group_keys_in_select(subquery, group_exprs)),
            negated: *negated,
        },
        Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => Expr::InSubquery {
            expr: Box::new(rewrite_expr(expr, group_exprs, aggs)),
            subquery: Box::new(rewrite_group_keys_in_select(subquery, group_exprs)),
            negated: *negated,
        },
        // v4.12 window / Literal / Column — clone-pass (these don't
        // participate in aggregate rewrite).
        Expr::WindowFunction { .. } | Expr::Literal(_) | Expr::Placeholder(_) | Expr::Column(_) => {
            e.clone()
        }
        // v7.10.10 — recurse children for array nodes.
        Expr::Array(items) => Expr::Array(
            items
                .iter()
                .map(|elem| rewrite_expr(elem, group_exprs, aggs))
                .collect(),
        ),
        Expr::ArraySubscript { target, index } => Expr::ArraySubscript {
            target: Box::new(rewrite_expr(target, group_exprs, aggs)),
            index: Box::new(rewrite_expr(index, group_exprs, aggs)),
        },
        Expr::AnyAll {
            expr,
            op,
            array,
            is_any,
        } => Expr::AnyAll {
            expr: Box::new(rewrite_expr(expr, group_exprs, aggs)),
            op: *op,
            array: Box::new(rewrite_expr(array, group_exprs, aggs)),
            is_any: *is_any,
        },
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(rewrite_expr(expr, group_exprs, aggs)),
            list: list
                .iter()
                .map(|item| rewrite_expr(item, group_exprs, aggs))
                .collect(),
            negated: *negated,
        },
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => Expr::Case {
            operand: operand
                .as_deref()
                .map(|o| Box::new(rewrite_expr(o, group_exprs, aggs))),
            branches: branches
                .iter()
                .map(|(w, t)| {
                    (
                        rewrite_expr(w, group_exprs, aggs),
                        rewrite_expr(t, group_exprs, aggs),
                    )
                })
                .collect(),
            else_branch: else_branch
                .as_deref()
                .map(|e| Box::new(rewrite_expr(e, group_exprs, aggs))),
        },
    }
}

/// v7.25.2 (round-19 A) — rewrite group-key references inside a
/// subquery body to `__grp_N` synthetic columns (aggregates are
/// not touched: empty spec list). Runs through the canonical
/// Select walker so every expression slot is covered.
fn rewrite_group_keys_in_select(
    s: &spg_sql::ast::SelectStatement,
    group_exprs: &[Expr],
) -> spg_sql::ast::SelectStatement {
    let mut out = s.clone();
    let _ = crate::walk_select_exprs_mut(&mut out, &mut |e| {
        *e = rewrite_expr(e, group_exprs, &[]);
        Ok(())
    });
    out
}

/// Canonical string key for a tuple of group values. Used as map key.
/// Per-value group-key encoding (shared by owned and borrowed paths).
fn encode_one(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("N|"),
        Value::SmallInt(n) => {
            out.push('s');
            out.push_str(&n.to_string());
            out.push('|');
        }
        Value::Int(n) => {
            out.push('I');
            out.push_str(&n.to_string());
            out.push('|');
        }
        Value::BigInt(n) => {
            out.push('B');
            out.push_str(&n.to_string());
            out.push('|');
        }
        Value::Float(x) => {
            out.push('F');
            out.push_str(&x.to_string());
            out.push('|');
        }
        Value::Bool(b) => {
            out.push(if *b { 'T' } else { 'f' });
            out.push('|');
        }
        Value::Text(s) => {
            out.push('S');
            out.push_str(s);
            out.push('|');
        }
        Value::Vector(v) => {
            out.push('V');
            for x in v {
                out.push_str(&x.to_string());
                out.push(',');
            }
            out.push('|');
        }
        // v6.0.1: GROUP BY on a `VECTOR(N) USING SQ8` column.
        // Two cells with byte-identical `(min, max, bytes)`
        // share the same group; equivalence is byte-equality
        // (same as f32 grouping today — neither path tries to
        // normalise nan/-0).
        Value::Sq8Vector(q) => {
            out.push('Q');
            out.push_str(&q.min.to_string());
            out.push('@');
            out.push_str(&q.max.to_string());
            out.push(':');
            for b in &q.bytes {
                out.push_str(&b.to_string());
                out.push(',');
            }
            out.push('|');
        }
        // v6.0.3: GROUP BY on a `VECTOR(N) USING HALF` column.
        // Byte-equality over the raw u16 bits; matches the SQ8
        // path's byte-key model.
        Value::HalfVector(h) => {
            out.push('H');
            for b in &h.bytes {
                out.push_str(&b.to_string());
                out.push(',');
            }
            out.push('|');
        }
        Value::Numeric { scaled, scale } => {
            out.push('D');
            out.push_str(&scaled.to_string());
            out.push('@');
            out.push_str(&scale.to_string());
            out.push('|');
        }
        Value::Date(d) => {
            out.push('d');
            out.push_str(&d.to_string());
            out.push('|');
        }
        Value::Timestamp(t) => {
            out.push('t');
            out.push_str(&t.to_string());
            out.push('|');
        }
        Value::Interval { months, micros } => {
            out.push('i');
            out.push_str(&months.to_string());
            out.push('m');
            out.push_str(&micros.to_string());
            out.push('|');
        }
        Value::Json(s) => {
            out.push('j');
            out.push_str(s);
            out.push('|');
        }
        // v7.5.0 — Value is #[non_exhaustive] for downstream
        // forward-compat. Any future variant lacking explicit
        // handling here will share a debug-derived group key,
        // which is observably wrong but won't crash.
        _ => {
            out.push('?');
            out.push_str(&format!("{v:?}"));
            out.push('|');
        }
    }
}

/// v7.30 (perf campaign) - encode from borrowed cells without
/// materialising an owned Vec<Value> first.
pub(crate) fn encode_key_refs(vals: &[&Value]) -> String {
    let mut out = String::new();
    for v in vals {
        encode_one(&mut out, v);
    }
    out
}

/// v7.31 (perf 3e) — encode into a caller-owned scratch buffer.
/// The per-row key paths (group hash, DISTINCT set, join build/
/// probe) ran 24k+ String allocations per query through the
/// allocator just to LOOK UP a map; the scratch form allocates
/// only when a map actually has to take ownership (vacant insert).
pub(crate) fn encode_key_refs_into(vals: &[&Value], out: &mut String) {
    out.clear();
    for v in vals {
        encode_one(out, v);
    }
}

pub(crate) fn encode_key(vals: &[Value]) -> String {
    let mut out = String::new();
    for v in vals {
        encode_one(&mut out, v);
    }
    out
}

#[allow(clippy::cast_precision_loss)]
fn value_cmp(a: &Value, b: &Value) -> core::cmp::Ordering {
    use core::cmp::Ordering::Equal;
    match (a, b) {
        (Value::Null, Value::Null) => Equal,
        (Value::Null, _) => core::cmp::Ordering::Greater, // NULLs last
        (_, Value::Null) => core::cmp::Ordering::Less,
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::BigInt(x), Value::BigInt(y)) => x.cmp(y),
        (Value::Int(x), Value::BigInt(y)) => i64::from(*x).cmp(y),
        (Value::BigInt(x), Value::Int(y)) => x.cmp(&i64::from(*y)),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or(Equal),
        (Value::Int(x), Value::Float(y)) => f64::from(*x).partial_cmp(y).unwrap_or(Equal),
        (Value::Float(x), Value::Int(y)) => x.partial_cmp(&f64::from(*y)).unwrap_or(Equal),
        (Value::BigInt(x), Value::Float(y)) => (*x as f64).partial_cmp(y).unwrap_or(Equal),
        (Value::Float(x), Value::BigInt(y)) => x.partial_cmp(&(*y as f64)).unwrap_or(Equal),
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => Equal,
    }
}
