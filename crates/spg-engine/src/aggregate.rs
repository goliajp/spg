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
use alloc::collections::BTreeMap;
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
    }
}

pub fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count" | "count_star" | "sum" | "min" | "max" | "avg"
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
}

#[derive(Debug, Clone)]
struct AggSpec {
    name: String, // lowercased
    /// Argument for sum/min/max/avg/count. `None` for `count(*)`.
    arg: Option<Expr>,
}

/// Output of running the aggregate path. Schema describes one row per
/// group; rows are not yet ORDER BY-sorted (caller does it).
#[derive(Debug)]
pub struct AggResult {
    pub columns: Vec<ColumnSchema>,
    pub rows: Vec<Row>,
}

/// Execute aggregate logic against an already-WHERE-filtered iterator of
/// rows. `table_alias` is the alias accepted by column resolution.
#[allow(clippy::too_many_lines)]
pub fn run(
    stmt: &SelectStatement,
    rows: &[&Row],
    schema_cols: &[ColumnSchema],
    table_alias: Option<&str>,
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

    // Map group key (vec of values, encoded as canonical string) -> group state.
    // Order of insertion is preserved via a parallel Vec of keys.
    let mut groups: BTreeMap<String, (Vec<Value>, Vec<AggState>)> = BTreeMap::new();
    let mut key_order: Vec<String> = Vec::new();
    // When there are no GROUP BY exprs *and* there is at least one aggregate,
    // every row collapses into a single anonymous group keyed by "".
    if rows.is_empty() && group_exprs.is_empty() {
        // Single empty-aggregate group: count=0, sum=0, max=NULL, etc.
        let init: Vec<AggState> = (0..agg_specs.len()).map(|_| AggState::default()).collect();
        groups.insert(String::new(), (Vec::new(), init));
        key_order.push(String::new());
    }

    for row in rows {
        let group_vals: Vec<Value> = group_exprs
            .iter()
            .map(|g| eval::eval_expr(g, row, &ctx))
            .collect::<Result<_, _>>()?;
        let key = encode_key(&group_vals);
        let entry = groups.entry(key.clone()).or_insert_with(|| {
            key_order.push(key.clone());
            let init: Vec<AggState> = (0..agg_specs.len()).map(|_| AggState::default()).collect();
            (group_vals.clone(), init)
        });
        for (i, spec) in agg_specs.iter().enumerate() {
            let arg_val = match &spec.arg {
                None => Value::Bool(true), // count_star: sentinel non-null
                Some(e) => eval::eval_expr(e, row, &ctx)?,
            };
            update_state(&mut entry.1[i], &spec.name, &arg_val)?;
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
    let agg_types: Vec<DataType> = agg_specs.iter().map(infer_agg_type).collect();
    let mut synth_schema: Vec<ColumnSchema> = Vec::new();
    for (i, ty) in group_types.iter().enumerate() {
        synth_schema.push(ColumnSchema::new(format!("__grp_{i}"), *ty, true));
    }
    for (i, ty) in agg_types.iter().enumerate() {
        synth_schema.push(ColumnSchema::new(format!("__agg_{i}"), *ty, true));
    }

    // Materialise synthetic rows.
    let mut synth_rows: Vec<Row> = Vec::new();
    for k in &key_order {
        let (gvals, states) = &groups[k];
        let mut values: Vec<Value> = Vec::with_capacity(synth_schema.len());
        values.extend(gvals.iter().cloned());
        for (i, st) in states.iter().enumerate() {
            values.push(finalize(&agg_specs[i].name, st));
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
    let mut kept_synth: Vec<Row> = Vec::new();
    let mut out_rows: Vec<Row> = Vec::new();
    for srow in synth_rows {
        if let Some(h) = &having_rewritten {
            let cond = eval::eval_expr(h, &srow, &synth_ctx)?;
            if !matches!(cond, Value::Bool(true)) {
                continue;
            }
        }
        let mut values: Vec<Value> = Vec::with_capacity(columns.len());
        for item in &stmt.items {
            if let SelectItem::Expr { expr, .. } = item {
                let rewritten = rewrite_expr(expr, &group_exprs, &agg_specs);
                values.push(eval::eval_expr(&rewritten, &srow, &synth_ctx)?);
            }
        }
        kept_synth.push(srow);
        out_rows.push(Row::new(values));
    }

    // ORDER BY: evaluate the rewritten order_by against each synth row,
    // sort, then drop the keys. Limit is applied by the caller.
    if !stmt.order_by.is_empty() {
        // v6.4.0 — multi-key ORDER BY on aggregate output. Each key
        // gets its own rewrite + per-key DESC flag.
        let rewritten: Vec<Expr> = stmt
            .order_by
            .iter()
            .map(|o| rewrite_expr(&o.expr, &group_exprs, &agg_specs))
            .collect();
        let descs: Vec<bool> = stmt.order_by.iter().map(|o| o.desc).collect();
        let mut tagged: Vec<(Vec<Value>, Row)> = kept_synth
            .into_iter()
            .zip(out_rows)
            .map(|(s, o)| {
                let mut keys = Vec::with_capacity(rewritten.len());
                for e in &rewritten {
                    keys.push(eval::eval_expr(e, &s, &synth_ctx)?);
                }
                Ok::<_, EvalError>((keys, o))
            })
            .collect::<Result<_, _>>()?;
        tagged.sort_by(|a, b| {
            use core::cmp::Ordering;
            for (i, (ka, kb)) in a.0.iter().zip(b.0.iter()).enumerate() {
                let cmp = value_cmp(ka, kb);
                let cmp = if descs[i] { cmp.reverse() } else { cmp };
                if cmp != Ordering::Equal {
                    return cmp;
                }
            }
            Ordering::Equal
        });
        out_rows = tagged.into_iter().map(|(_, o)| o).collect();
    }

    Ok(AggResult {
        columns,
        rows: out_rows,
    })
}

fn collect_aggregates(e: &Expr, out: &mut Vec<AggSpec>) {
    match e {
        Expr::FunctionCall { name, args } => {
            let lower = name.to_ascii_lowercase();
            if is_aggregate_name(&lower) {
                let arg = if lower == "count_star" {
                    None
                } else {
                    args.first().cloned()
                };
                let spec = AggSpec {
                    name: lower,
                    arg: arg.clone(),
                };
                if !out.iter().any(|s| s.name == spec.name && s.arg == spec.arg) {
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
    }
}

fn update_state(st: &mut AggState, name: &str, v: &Value) -> Result<(), EvalError> {
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
        _ => unreachable!(),
    }
}

fn infer_agg_type(spec: &AggSpec) -> DataType {
    match spec.name.as_str() {
        // count/count_star are exact integer counts; sum widens to BigInt
        // and reports as such even for Float input (the value column is
        // nullable so the wire layer surfaces the Float at runtime).
        "count" | "count_star" | "sum" => DataType::BigInt,
        "avg" => DataType::Float,
        // min/max: we don't know the input type without probing — default
        // to Text and let downstream rendering coerce.
        _ => DataType::Text,
    }
}

fn agg_or_group_type(e: &Expr, synth: &[ColumnSchema]) -> DataType {
    if let Expr::Column(c) = e
        && let Some(s) = synth.iter().find(|s| s.name == c.name)
    {
        return s.ty;
    }
    // Compound expression — fall back to Text (matches build_projection
    // behaviour for non-column expressions in the non-aggregate path).
    DataType::Text
}

fn rewrite_expr(e: &Expr, group_exprs: &[Expr], aggs: &[AggSpec]) -> Expr {
    // Match aggregate FunctionCalls first — they sit outside group_by.
    if let Expr::FunctionCall { name, args } = e {
        let lower = name.to_ascii_lowercase();
        if is_aggregate_name(&lower) {
            let arg = if lower == "count_star" {
                None
            } else {
                args.first().cloned()
            };
            for (i, spec) in aggs.iter().enumerate() {
                if spec.name == lower && spec.arg == arg {
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
        } => Expr::Like {
            expr: Box::new(rewrite_expr(expr, group_exprs, aggs)),
            pattern: Box::new(rewrite_expr(pattern, group_exprs, aggs)),
            negated: *negated,
        },
        Expr::Extract { field, source } => Expr::Extract {
            field: *field,
            source: Box::new(rewrite_expr(source, group_exprs, aggs)),
        },
        // v4.10 subquery + v4.12 window / Literal / Column —
        // clone-pass (these don't participate in aggregate rewrite).
        Expr::ScalarSubquery(_)
        | Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::WindowFunction { .. }
        | Expr::Literal(_)
        | Expr::Placeholder(_)
        | Expr::Column(_) => e.clone(),
    }
}

/// Canonical string key for a tuple of group values. Used as map key.
fn encode_key(vals: &[Value]) -> String {
    let mut out = String::new();
    for v in vals {
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
        }
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
