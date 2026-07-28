//! Window-function evaluation split out of `lib.rs` (lib.rs split 2):
//! the OVER(...) rewrite phase (select_has_window / expr_has_window /
//! collect_window_nodes / rewrite_window_to_columns) and the per-partition
//! evaluator (compute_window_partition plus its frame / peer-group
//! helpers effective_frame / frame_bounds_for_row / peer_group_start /
//! peer_group_end), with the window partition / order key comparators
//! (partition_key_cmp / order_key_cmp). Driven by the windowed-SELECT
//! path in `select.rs`. Pure free functions; shared comparators
//! (value_cmp / order_by_value_cmp) and the evaluator stay in the crate
//! root and are reached via `use crate::`.

use alloc::vec::Vec;

use spg_sql::ast::{
    Expr, FrameBound, FrameExclusion, FrameKind, SelectItem, SelectStatement, WindowFrame,
};
use spg_storage::{Row, Value};

use crate::eval::{self, EvalContext};
use crate::{EngineError, order_by_value_cmp, value_cmp, value_to_f64};

pub(crate) fn select_has_window(stmt: &SelectStatement) -> bool {
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item
            && expr_has_window(expr)
        {
            return true;
        }
    }
    // v7.39 (round 592) — a window function can appear in ORDER BY without
    // being selected: `SELECT id FROM t ORDER BY row_number() OVER (…)` is a
    // query PG answers. Only the select list was consulted, so the statement
    // took the ordinary path and the call reached row eval, where it produced
    // the internal "engine rewrite bug" message — the same leak round 229
    // closed for WHERE and HAVING, from the other direction.
    stmt.order_by.iter().any(|o| expr_has_window(&o.expr))
}

/// v7.39 (round 229) — PG forbids window functions in WHERE and HAVING:
/// both are evaluated before the window pass, so a window call there has
/// no defined value. SPG used to let one through to row eval, where it hit
/// an internal "engine rewrite bug" message; this reports PG's wording
/// (42P20) at the front of execution instead.
pub(crate) fn reject_window_in_row_clauses(stmt: &SelectStatement) -> Result<(), EngineError> {
    if stmt.where_.as_ref().is_some_and(expr_has_window) {
        return Err(EngineError::Unsupported(
            "window functions are not allowed in WHERE".into(),
        ));
    }
    if stmt.having.as_ref().is_some_and(expr_has_window) {
        return Err(EngineError::Unsupported(
            "window functions are not allowed in HAVING".into(),
        ));
    }
    Ok(())
}

fn expr_has_window(e: &Expr) -> bool {
    match e {
        Expr::NamedArg { expr, .. } => expr_has_window(expr),
        Expr::Variadic(expr) => expr_has_window(expr),
        Expr::WindowFunction { .. } => true,
        Expr::AggregateOrdered { call, order_by, .. } => {
            expr_has_window(call) || order_by.iter().any(|o| expr_has_window(&o.expr))
        }
        Expr::Binary { lhs, rhs, .. } => expr_has_window(lhs) || expr_has_window(rhs),
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => expr_has_window(expr),
        Expr::FunctionCall { args, .. } => args.iter().any(expr_has_window),
        Expr::Like { expr, pattern, .. } => expr_has_window(expr) || expr_has_window(pattern),
        Expr::Extract { source, .. } => expr_has_window(source),
        Expr::ScalarSubquery(_)
        | Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::RowInSubquery { .. }
        | Expr::RowCmpSubquery { .. }
        | Expr::Literal(_)
        | Expr::Placeholder(_)
        | Expr::Column(_) => false,
        Expr::Array(items) => items.iter().any(expr_has_window),
        Expr::ArraySubscript { target, index } => expr_has_window(target) || expr_has_window(index),
        Expr::ArraySlice { target, lo, hi } => {
            expr_has_window(target)
                || lo.as_deref().is_some_and(expr_has_window)
                || hi.as_deref().is_some_and(expr_has_window)
        }
        Expr::AnyAll { expr, array, .. } => expr_has_window(expr) || expr_has_window(array),
        Expr::InList { expr, list, .. } => {
            expr_has_window(expr) || list.iter().any(expr_has_window)
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            operand.as_deref().is_some_and(expr_has_window)
                || branches
                    .iter()
                    .any(|(w, t)| expr_has_window(w) || expr_has_window(t))
                || else_branch.as_deref().is_some_and(expr_has_window)
        }
    }
}

pub(crate) fn collect_window_nodes(e: &Expr, out: &mut Vec<Expr>) {
    if let Expr::WindowFunction { .. } = e {
        // Deduplicate by structural equality on the expression
        // (cheap because window args + partition + order are
        // small). Without dedup we'd recompute identical windows
        // once per occurrence in the projection.
        if !out.iter().any(|x| x == e) {
            out.push(e.clone());
        }
        return;
    }
    match e {
        // Already handled by the early-return at the top.
        Expr::WindowFunction { .. } => unreachable!(),
        Expr::Binary { lhs, rhs, .. } => {
            collect_window_nodes(lhs, out);
            collect_window_nodes(rhs, out);
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => {
            collect_window_nodes(expr, out);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_window_nodes(a, out);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            collect_window_nodes(expr, out);
            collect_window_nodes(pattern, out);
        }
        Expr::Extract { source, .. } => collect_window_nodes(source, out),
        _ => {}
    }
}

pub(crate) fn rewrite_window_to_columns(e: &mut Expr, window_nodes: &[Expr]) {
    if let Expr::WindowFunction { .. } = e
        && let Some(idx) = window_nodes.iter().position(|w| w == e)
    {
        *e = Expr::Column(spg_sql::ast::ColumnName {
            qualifier: None,
            name: alloc::format!("__win_{idx}"),
        });
        return;
    }
    match e {
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_window_to_columns(lhs, window_nodes);
            rewrite_window_to_columns(rhs, window_nodes);
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => {
            rewrite_window_to_columns(expr, window_nodes);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                rewrite_window_to_columns(a, window_nodes);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            rewrite_window_to_columns(expr, window_nodes);
            rewrite_window_to_columns(pattern, window_nodes);
        }
        Expr::Extract { source, .. } => rewrite_window_to_columns(source, window_nodes),
        _ => {}
    }
}

/// Total order over partition-key tuples. NULL sorts as the
/// lowest value (matches the `<` partial order's NULL-last
/// behaviour with `INFINITY` flipped).
pub(crate) fn partition_key_cmp(a: &[Value<'static>], b: &[Value<'static>]) -> core::cmp::Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let c = value_cmp(x, y);
        if c != core::cmp::Ordering::Equal {
            return c;
        }
    }
    a.len().cmp(&b.len())
}

pub(crate) fn order_key_cmp(
    a: &[(Value, bool, Option<bool>)],
    b: &[(Value, bool, Option<bool>)],
) -> core::cmp::Ordering {
    // v7.24.1 — per-key DESC + effective NULLS placement (shared
    // contract with order_by_value_cmp).
    for ((va, desc, nf), (vb, _, _)) in a.iter().zip(b.iter()) {
        let c = order_by_value_cmp(*desc, *nf, va, vb);
        if c != core::cmp::Ordering::Equal {
            return c;
        }
    }
    a.len().cmp(&b.len())
}

/// v7.17.0 Phase 3.10 — true when the Value is one of the
/// integer-shaped variants `generate_series` accepts as a start
/// / stop / step component. Float / NUMERIC are rejected — PG's
/// `generate_series(numeric, numeric)` overload is out of v7.17
/// scope.
pub(crate) const fn value_is_integer(v: &Value) -> bool {
    matches!(v, Value::SmallInt(_) | Value::Int(_) | Value::BigInt(_))
}

/// True for a 64-bit integer value (the int8/bigint domain). Used by
/// generate_series to type the series int8 when any bound is bigint.
pub(crate) const fn value_is_bigint(v: &Value) -> bool {
    matches!(v, Value::BigInt(_))
}

/// v7.17.0 Phase 3.10 — widen any integer-shaped Value to i64 for
/// the generate_series iteration loop. Non-integer inputs panic;
/// caller guards via `value_is_integer`.
pub(crate) const fn value_to_i64(v: &Value) -> i64 {
    match v {
        Value::SmallInt(n) => *n as i64,
        Value::Int(n) => *n as i64,
        Value::BigInt(n) => *n,
        _ => panic!("value_to_i64 called on non-integer Value"),
    }
}

/// Compute the window function's per-row output for one partition.
/// `slice` has (partition key, order key, original-row-index)
/// tuples already sorted by order key. `filtered_rows` is the
/// full row list indexed by original-row-index. `out_vals` is
/// the destination, also indexed by original-row-index.
#[allow(
    clippy::too_many_arguments,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::type_complexity,
    clippy::match_same_arms
)]
pub(crate) fn compute_window_partition(
    name: &str,
    args: &[Expr],
    ordered: bool,
    frame: Option<&WindowFrame>,
    null_treatment: spg_sql::ast::NullTreatment,
    filter: Option<&Expr>,
    slice: &[(Vec<Value<'static>>, Vec<(Value, bool, Option<bool>)>, usize)],
    filtered_rows: &[&Row<'static>],
    ctx: &EvalContext<'_>,
    out_vals: &mut [Value],
) -> Result<(), EngineError> {
    let ignore_nulls = matches!(null_treatment, spg_sql::ast::NullTreatment::Ignore);
    // v7.37 D.40 — `agg(...) FILTER (WHERE cond) OVER (...)`: pre-evaluate the
    // predicate per peer row (in slice order). A false/NULL predicate drops that
    // row from the aggregate — the frame bounds are unchanged, only which rows
    // inside the frame contribute. `None` = no FILTER (all pass).
    let filter_pass: Vec<bool> = match filter {
        None => alloc::vec![true; slice.len()],
        Some(pred) => slice
            .iter()
            .map(|(_, _, idx)| {
                Ok(matches!(
                    eval::eval_expr(pred, filtered_rows[*idx], ctx).map_err(EngineError::Eval)?,
                    Value::Bool(true)
                ))
            })
            .collect::<Result<_, EngineError>>()?,
    };
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "row_number" => {
            for (rank, (_, _, idx)) in slice.iter().enumerate() {
                out_vals[*idx] = Value::BigInt((rank + 1) as i64);
            }
            Ok(())
        }
        "rank" => {
            let mut prev_key: Option<&[(Value, bool, Option<bool>)]> = None;
            let mut current_rank: i64 = 1;
            for (i, (_, okey, idx)) in slice.iter().enumerate() {
                if let Some(p) = prev_key
                    && order_key_cmp(p, okey) != core::cmp::Ordering::Equal
                {
                    current_rank = (i + 1) as i64;
                }
                if prev_key.is_none() {
                    current_rank = 1;
                }
                out_vals[*idx] = Value::BigInt(current_rank);
                prev_key = Some(okey.as_slice());
            }
            Ok(())
        }
        "dense_rank" => {
            let mut prev_key: Option<&[(Value, bool, Option<bool>)]> = None;
            let mut current_rank: i64 = 0;
            for (_, okey, idx) in slice {
                if prev_key.is_none_or(|p| order_key_cmp(p, okey) != core::cmp::Ordering::Equal) {
                    current_rank += 1;
                }
                out_vals[*idx] = Value::BigInt(current_rank);
                prev_key = Some(okey.as_slice());
            }
            Ok(())
        }
        "sum" | "avg" | "min" | "max" | "count" | "count_star" => {
            // Pre-evaluate the function arg per row in the slice
            // (count_star has no arg).
            let arg_values: Vec<Value<'static>> = if lower == "count_star" || args.is_empty() {
                slice.iter().map(|_| Value::Null).collect()
            } else {
                slice
                    .iter()
                    .map(|(_, _, idx)| eval::eval_expr(&args[0], filtered_rows[*idx], ctx))
                    .collect::<Result<_, _>>()
                    .map_err(EngineError::Eval)?
            };
            // v4.20: pick the effective frame. Explicit frame
            // overrides the implicit default (running for ordered,
            // whole-partition for unordered).
            let eff = effective_frame(frame, ordered)?;
            let exclude = frame_exclusion(frame)?;
            // v7.39 (round 589) — the two commonest frames start at the
            // partition's first row and end at the current row or its last,
            // so the frame only ever GROWS as `i` advances. Recomputing the
            // aggregate from the start for every row made both O(partition²):
            // `sum(x) OVER (PARTITION BY g)` over 500k rows in 50 partitions
            // took 36.6 SECONDS where PG takes 7.65 ms, and the classic
            // running total `sum(x) OVER (ORDER BY t)` took 1.46 s over
            // 20k rows against PG's 1.10 ms. Carry the accumulators across
            // rows and extend them only over the rows that just entered.
            // Any other frame shape, or any EXCLUDE (which can drop a row
            // that is already in the accumulator), keeps the recompute path.
            let incremental = matches!(eff.1, FrameBound::UnboundedPreceding)
                && matches!(
                    eff.2,
                    FrameBound::CurrentRow | FrameBound::UnboundedFollowing
                )
                && matches!(exclude, FrameExclusion::NoOthers);
            let mut sum: f64 = 0.0;
            let mut num_scaled: i128 = 0;
            let mut num_scale: u16 = 0;
            let mut use_numeric = false;
            let mut int_sum: i128 = 0;
            let mut all_int = true;
            let mut count: i64 = 0;
            let mut min_val: Option<Value<'static>> = None;
            let mut max_val: Option<Value<'static>> = None;
            let mut row_count: i64 = 0;
            // The first row of the frame not yet folded into the above.
            let mut next_j: usize = 0;
            #[allow(clippy::needless_range_loop)]
            for i in 0..slice.len() {
                let (lo, hi) = frame_bounds_for_row(&eff, i, slice)?;
                // v7.39 (read01 round 109) — the current row's peer group, for
                // EXCLUDE GROUP / TIES (only computed when one is in effect).
                let (peer_start, peer_end) =
                    if matches!(exclude, FrameExclusion::Group | FrameExclusion::Ties) {
                        (peer_group_start(slice, i), peer_group_end(slice, i))
                    } else {
                        (0, 0)
                    };
                // Rows already folded in stay folded in for a growing
                // frame; every other shape starts this row from nothing.
                if !incremental {
                    sum = 0.0;
                    num_scaled = 0;
                    num_scale = 0;
                    use_numeric = false;
                    int_sum = 0;
                    all_int = true;
                    count = 0;
                    min_val = None;
                    max_val = None;
                    row_count = 0;
                    next_j = lo;
                }
                if lo <= hi {
                    for j in next_j..=hi {
                        // EXCLUDE {CURRENT ROW | GROUP | TIES} drops the current
                        // row and/or its peer group from the aggregate frame.
                        if frame_row_excluded(exclude, j, i, peer_start, peer_end) {
                            continue;
                        }
                        // v7.37 D.40 — a FILTER (WHERE …) predicate that this peer
                        // row fails drops it from the aggregate (frame unchanged).
                        if !filter_pass[j] {
                            continue;
                        }
                        let v = &arg_values[j];
                        match lower.as_str() {
                            "count_star" => row_count += 1,
                            "count" => {
                                if !v.is_null() {
                                    count += 1;
                                }
                            }
                            _ => {
                                // sum/avg/min/max all skip NULLs.
                                if v.is_null() {
                                    continue;
                                }
                                // sum/avg: exact NUMERIC accumulation for
                                // Numeric cells (aligns scales, no f64);
                                // int/float continue through the f64 path.
                                if let Value::Numeric { scaled, scale, .. } = v {
                                    let (s, sc) = crate::numeric::numeric_add(
                                        num_scaled, num_scale, *scaled, *scale,
                                    );
                                    num_scaled = s;
                                    num_scale = sc;
                                    use_numeric = true;
                                    all_int = false;
                                    count += 1;
                                } else if let Some(x) = value_to_f64(v) {
                                    match v {
                                        Value::Int(n) => int_sum += i128::from(*n),
                                        Value::SmallInt(n) => int_sum += i128::from(*n),
                                        Value::BigInt(n) => int_sum += i128::from(*n),
                                        _ => all_int = false,
                                    }
                                    sum += x;
                                    count += 1;
                                }
                                if min_val
                                    .as_ref()
                                    .is_none_or(|m| value_cmp(v, m) == core::cmp::Ordering::Less)
                                {
                                    min_val = Some(v.clone());
                                }
                                if max_val
                                    .as_ref()
                                    .is_none_or(|m| value_cmp(m, v) == core::cmp::Ordering::Less)
                                {
                                    max_val = Some(v.clone());
                                }
                            }
                        }
                    }
                    next_j = next_j.max(hi + 1);
                }
                let value = match lower.as_str() {
                    "count_star" => Value::BigInt(row_count),
                    "count" => Value::BigInt(count),
                    // sum over a frame with no non-NULL numeric value is
                    // NULL in PG (empty / all-NULL frame), not 0. The old
                    // `Value::Float(sum)` returned 0 for `sum(x)` over an
                    // all-NULL partition (see PARTITION BY nullable col).
                    "sum" => {
                        if count == 0 {
                            Value::Null
                        } else if use_numeric {
                            Value::Numeric {
                                scaled: num_scaled,
                                scale: num_scale,
                                kind: spg_storage::NumericKind::Finite,
                            }
                        } else if all_int && i64::try_from(int_sum).is_ok() {
                            // Integer inputs → BIGINT, matching PG and the
                            // GROUP BY sum() path.
                            Value::BigInt(int_sum as i64)
                        } else {
                            Value::Float(sum)
                        }
                    }
                    "avg" => {
                        if count == 0 {
                            Value::Null
                        } else if use_numeric {
                            let (scaled, scale) = crate::numeric::numeric_avg(
                                num_scaled,
                                num_scale,
                                i128::from(count),
                            );
                            Value::Numeric {
                                scaled,
                                scale,
                                kind: spg_storage::NumericKind::Finite,
                            }
                        } else if all_int {
                            // v7.38 (read01) — avg over integer inputs is NUMERIC
                            // in PG (as it is for the GROUP BY avg() path), not
                            // double. Divide the exact integer sum at scale 0.
                            let (scaled, scale) =
                                crate::numeric::numeric_avg(int_sum, 0, i128::from(count));
                            Value::Numeric {
                                scaled,
                                scale,
                                kind: spg_storage::NumericKind::Finite,
                            }
                        } else {
                            Value::Float(sum / count as f64)
                        }
                    }
                    "min" => min_val.clone().unwrap_or(Value::Null),
                    "max" => max_val.clone().unwrap_or(Value::Null),
                    _ => unreachable!(),
                };
                let (_, _, idx) = &slice[i];
                out_vals[*idx] = value;
            }
            Ok(())
        }
        "lag" | "lead" => {
            // lag(expr [, offset [, default]])
            // lead(expr [, offset [, default]])
            if args.is_empty() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "{lower}() requires at least one argument"
                )));
            }
            let offset: i64 = if args.len() >= 2 {
                let v = eval::eval_expr(&args[1], filtered_rows[slice[0].2], ctx)
                    .map_err(EngineError::Eval)?;
                match v {
                    Value::SmallInt(n) => i64::from(n),
                    Value::Int(n) => i64::from(n),
                    Value::BigInt(n) => n,
                    _ => {
                        return Err(EngineError::Unsupported(alloc::format!(
                            "{lower}() offset must be integer"
                        )));
                    }
                }
            } else {
                1
            };
            let default: Value = if args.len() >= 3 {
                eval::eval_expr(&args[2], filtered_rows[slice[0].2], ctx)
                    .map_err(EngineError::Eval)?
            } else {
                Value::Null
            };
            let values: Vec<Value<'static>> = slice
                .iter()
                .map(|(_, _, idx)| eval::eval_expr(&args[0], filtered_rows[*idx], ctx))
                .collect::<Result<_, _>>()
                .map_err(EngineError::Eval)?;
            let n = slice.len();
            for (i, (_, _, idx)) in slice.iter().enumerate() {
                let signed_offset = if lower == "lag" { -offset } else { offset };
                let v = if ignore_nulls {
                    // v6.4.2 — IGNORE NULLS: walk in the offset direction
                    // skipping NULL values; the `offset`-th non-NULL
                    // encountered is the result.
                    let step: i64 = if signed_offset >= 0 { 1 } else { -1 };
                    let needed: i64 = signed_offset.abs();
                    if needed == 0 {
                        values[i].clone()
                    } else {
                        let mut j: i64 = i as i64;
                        let mut hits: i64 = 0;
                        let mut found: Option<Value> = None;
                        loop {
                            j += step;
                            if j < 0 || j >= n as i64 {
                                break;
                            }
                            #[allow(clippy::cast_sign_loss)]
                            let v = &values[j as usize];
                            if !v.is_null() {
                                hits += 1;
                                if hits == needed {
                                    found = Some(v.clone());
                                    break;
                                }
                            }
                        }
                        found.unwrap_or_else(|| default.clone())
                    }
                } else {
                    let target_signed = i64::try_from(i).unwrap_or(i64::MAX) + signed_offset;
                    if target_signed < 0 || target_signed >= i64::try_from(n).unwrap_or(i64::MAX) {
                        default.clone()
                    } else {
                        #[allow(clippy::cast_sign_loss)]
                        {
                            values[target_signed as usize].clone()
                        }
                    }
                };
                out_vals[*idx] = v;
            }
            Ok(())
        }
        "first_value" | "last_value" | "nth_value" => {
            if args.is_empty() {
                return Err(EngineError::Unsupported(alloc::format!(
                    "{lower}() requires at least one argument"
                )));
            }
            let values: Vec<Value<'static>> = slice
                .iter()
                .map(|(_, _, idx)| eval::eval_expr(&args[0], filtered_rows[*idx], ctx))
                .collect::<Result<_, _>>()
                .map_err(EngineError::Eval)?;
            let nth: usize = if lower == "nth_value" {
                if args.len() < 2 {
                    return Err(EngineError::Unsupported(
                        "nth_value() requires (expr, n)".into(),
                    ));
                }
                let v = eval::eval_expr(&args[1], filtered_rows[slice[0].2], ctx)
                    .map_err(EngineError::Eval)?;
                let raw = match v {
                    Value::SmallInt(n) => i64::from(n),
                    Value::Int(n) => i64::from(n),
                    Value::BigInt(n) => n,
                    _ => {
                        return Err(EngineError::Unsupported(
                            "nth_value() n must be integer".into(),
                        ));
                    }
                };
                if raw < 1 {
                    return Err(EngineError::Unsupported(
                        "nth_value() n must be >= 1".into(),
                    ));
                }
                #[allow(clippy::cast_sign_loss)]
                {
                    raw as usize
                }
            } else {
                0
            };
            let eff = effective_frame(frame, ordered)?;
            // v7.39 (read01 round 109) — value functions honour EXCLUDE too. For
            // the default NO OTHERS the fast path (values[lo] / values[hi] / …)
            // is unchanged; any exclusion filters the frame indices first.
            let exclude = frame_exclusion(frame)?;
            for i in 0..slice.len() {
                let (lo, hi) = frame_bounds_for_row(&eff, i, slice)?;
                let (_, _, idx) = &slice[i];
                let (peer_start, peer_end) =
                    if matches!(exclude, FrameExclusion::Group | FrameExclusion::Ties) {
                        (peer_group_start(slice, i), peer_group_end(slice, i))
                    } else {
                        (0, 0)
                    };
                // The frame index list this row sees, with EXCLUDE applied.
                let frame_idxs: Vec<usize> = if lo > hi {
                    Vec::new()
                } else if exclude == FrameExclusion::NoOthers {
                    (lo..=hi).collect()
                } else {
                    (lo..=hi)
                        .filter(|&j| !frame_row_excluded(exclude, j, i, peer_start, peer_end))
                        .collect()
                };
                let pick = |j: usize| values[j].clone();
                let v = if frame_idxs.is_empty() {
                    Value::Null
                } else if ignore_nulls && matches!(lower.as_str(), "first_value" | "last_value") {
                    // v6.4.2 — IGNORE NULLS: skip NULL cells when selecting the
                    // boundary value within the (post-exclude) frame.
                    let found = if lower == "first_value" {
                        frame_idxs.iter().copied().find(|&j| !values[j].is_null())
                    } else {
                        frame_idxs
                            .iter()
                            .rev()
                            .copied()
                            .find(|&j| !values[j].is_null())
                    };
                    found.map(pick).unwrap_or(Value::Null)
                } else {
                    match lower.as_str() {
                        "first_value" => pick(frame_idxs[0]),
                        "last_value" => pick(*frame_idxs.last().unwrap()),
                        "nth_value" => frame_idxs.get(nth - 1).copied().map_or(Value::Null, pick),
                        _ => unreachable!(),
                    }
                };
                out_vals[*idx] = v;
            }
            Ok(())
        }
        "ntile" => {
            if args.is_empty() {
                return Err(EngineError::Unsupported(
                    "ntile(n) requires an integer argument".into(),
                ));
            }
            let v = eval::eval_expr(&args[0], filtered_rows[slice[0].2], ctx)
                .map_err(EngineError::Eval)?;
            let bucket_count: i64 = match v {
                Value::SmallInt(n) => i64::from(n),
                Value::Int(n) => i64::from(n),
                Value::BigInt(n) => n,
                _ => {
                    return Err(EngineError::Unsupported(
                        "ntile() argument must be integer".into(),
                    ));
                }
            };
            if bucket_count < 1 {
                return Err(EngineError::Unsupported(
                    "ntile() argument must be >= 1".into(),
                ));
            }
            #[allow(clippy::cast_sign_loss)]
            let buckets = bucket_count as usize;
            let n = slice.len();
            // Each bucket gets `base` rows; the first `extras` buckets
            // get one extra. PG semantics.
            let base = n / buckets;
            let extras = n % buckets;
            let mut bucket: usize = 1;
            let mut remaining_in_bucket = if extras > 0 { base + 1 } else { base };
            let mut buckets_with_extra_remaining = extras;
            for (_, _, idx) in slice {
                if remaining_in_bucket == 0 {
                    bucket += 1;
                    buckets_with_extra_remaining = buckets_with_extra_remaining.saturating_sub(1);
                    remaining_in_bucket = if buckets_with_extra_remaining > 0 {
                        base + 1
                    } else {
                        base
                    };
                    // Edge: if base==0 and extras==0, all rows fit;
                    // shouldn't reach here, but guard anyway.
                    if remaining_in_bucket == 0 {
                        remaining_in_bucket = 1;
                    }
                }
                out_vals[*idx] = Value::BigInt(i64::try_from(bucket).unwrap_or(i64::MAX));
                remaining_in_bucket -= 1;
            }
            Ok(())
        }
        "percent_rank" => {
            // (rank - 1) / (n - 1) where rank is the standard RANK().
            // Single-row partitions get 0.
            let n = slice.len();
            let mut prev_key: Option<&[(Value, bool, Option<bool>)]> = None;
            let mut current_rank: i64 = 1;
            for (i, (_, okey, idx)) in slice.iter().enumerate() {
                if let Some(p) = prev_key
                    && order_key_cmp(p, okey) != core::cmp::Ordering::Equal
                {
                    current_rank = i64::try_from(i + 1).unwrap_or(i64::MAX);
                }
                if prev_key.is_none() {
                    current_rank = 1;
                }
                #[allow(clippy::cast_precision_loss)]
                let pr = if n <= 1 {
                    0.0
                } else {
                    (current_rank - 1) as f64 / (n - 1) as f64
                };
                out_vals[*idx] = Value::Float(pr);
                prev_key = Some(okey.as_slice());
            }
            Ok(())
        }
        "cume_dist" => {
            // # rows up to and including this row's peer group / n.
            let n = slice.len();
            // First pass: find peer-group-end rank for each row.
            for i in 0..slice.len() {
                let peer_end = peer_group_end(slice, i);
                #[allow(clippy::cast_precision_loss)]
                let cd = (peer_end + 1) as f64 / n as f64;
                let (_, _, idx) = &slice[i];
                out_vals[*idx] = Value::Float(cd);
            }
            Ok(())
        }
        // v7.39 (round 230) — every *aggregate* is usable as a window
        // function in PG, not just the handful with a bespoke arm above.
        // Rather than grow that list one accumulator at a time, drive the
        // aggregate module's own state machine over each row's frame:
        // string_agg / array_agg / bool_and / bool_or / stddev / variance /
        // bit_and / json_agg / range_agg and the rest all arrive for free,
        // with the same NULL handling and result typing they have in a
        // GROUP BY.
        other if crate::aggregate::is_aggregate_name(other) => {
            generic_aggregate_window(other, args, ordered, frame, &filter_pass, slice, filtered_rows, ctx, out_vals)
        }
        // Neither a window function nor an aggregate: PG resolves the call
        // like any other and reports the missing function (42883).
        other => Err(EngineError::Unsupported(alloc::format!(
            "function {other}() does not exist"
        ))),
    }
}

/// v7.39 (round 230) — an arbitrary aggregate evaluated over each row's
/// window frame. Mirrors the bespoke sum/avg/min/max path's frame walk
/// (bounds, EXCLUDE, FILTER) but accumulates through `aggregate`'s own
/// `AggState`, so the value and its type are whatever the same aggregate
/// would produce over that row set in a GROUP BY.
#[allow(clippy::too_many_arguments)]
fn generic_aggregate_window(
    name: &str,
    args: &[Expr],
    ordered: bool,
    frame: Option<&WindowFrame>,
    filter_pass: &[bool],
    slice: &[(Vec<Value<'static>>, Vec<(Value, bool, Option<bool>)>, usize)],
    filtered_rows: &[&Row<'static>],
    ctx: &EvalContext<'_>,
    out_vals: &mut [Value],
) -> Result<(), EngineError> {
    // Pre-evaluate the argument(s) once per row: the frame walk below
    // revisits the same rows for every output row.
    let eval_arg = |n: usize| -> Result<Vec<Value<'static>>, EngineError> {
        slice
            .iter()
            .map(|(_, _, idx)| eval::eval_expr(&args[n], filtered_rows[*idx], ctx))
            .collect::<Result<_, _>>()
            .map_err(EngineError::Eval)
    };
    let arg1 = if args.is_empty() { Vec::new() } else { eval_arg(0)? };
    let arg2 = if args.len() > 1 { Some(eval_arg(1)?) } else { None };
    let name = crate::aggregate::canonical_agg_name(name);
    let kind = crate::aggregate::classify_agg_name(name);
    let eff = effective_frame(frame, ordered)?;
    let exclude = frame_exclusion(frame)?;
    for i in 0..slice.len() {
        let (lo, hi) = frame_bounds_for_row(&eff, i, slice)?;
        let (peer_start, peer_end) =
            if matches!(exclude, FrameExclusion::Group | FrameExclusion::Ties) {
                (peer_group_start(slice, i), peer_group_end(slice, i))
            } else {
                (0, 0)
            };
        let mut st = crate::aggregate::AggState::default();
        if lo <= hi {
            for j in lo..=hi {
                if frame_row_excluded(exclude, j, i, peer_start, peer_end) || !filter_pass[j] {
                    continue;
                }
                let v = arg1.get(j).unwrap_or(&Value::Null);
                crate::aggregate::update_state(
                    &mut st,
                    kind,
                    name,
                    v,
                    arg2.as_ref().and_then(|a| a.get(j)),
                    None,
                    None,
                    ctx.mysql_dialect,
                )
                .map_err(EngineError::Eval)?;
            }
        }
        let (_, _, idx) = &slice[i];
        let v = crate::aggregate::finalize(name, &st, ctx.mysql_dialect);
        // v7.39 (round 327, V44) — keep the zone identity here too. A
        // timestamptz rides as `Value::Timestamp` at runtime, so the array
        // `array_agg(x) OVER (…)` builds is a TimestampArray; the
        // ARGUMENT's static type is what says otherwise.
        let v = match (v, args.first().and_then(|a| crate::describe::describe_expr(a, ctx.columns))) {
            (Value::TimestampArray(items), Some(shape))
                if shape.ty == spg_storage::DataType::Timestamptz =>
            {
                Value::TimestamptzArray(items)
            }
            (v, _) => v,
        };
        out_vals[*idx] = v;
    }
    Ok(())
}

/// v4.20: resolve the user-provided frame down to a normalised
/// `(kind, start, end)`. `None` means default — derive from
/// `ordered`: ordered ⇒ RANGE UNBOUNDED PRECEDING AND CURRENT ROW,
/// unordered ⇒ ROWS UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING.
/// Single-bound shorthand (e.g. `ROWS 5 PRECEDING`) normalises
/// end → CURRENT ROW per the PG spec.
/// The frame's EXCLUDE mode.
fn frame_exclusion(frame: Option<&WindowFrame>) -> Result<FrameExclusion, EngineError> {
    Ok(frame.map_or(FrameExclusion::NoOthers, |f| f.exclude))
}

/// v7.39 (read01 round 109) — is frame row `j` excluded, given the current row
/// `i` and its peer group `[peer_start, peer_end]` (rows with the same ORDER BY
/// key)? CURRENT ROW drops `i`; GROUP drops `i` and all its peers; TIES drops
/// the peers but keeps `i`.
fn frame_row_excluded(
    exclude: FrameExclusion,
    j: usize,
    i: usize,
    peer_start: usize,
    peer_end: usize,
) -> bool {
    match exclude {
        FrameExclusion::NoOthers => false,
        FrameExclusion::CurrentRow => j == i,
        FrameExclusion::Group => peer_start <= j && j <= peer_end,
        FrameExclusion::Ties => peer_start <= j && j <= peer_end && j != i,
    }
}

fn effective_frame(
    frame: Option<&WindowFrame>,
    ordered: bool,
) -> Result<(FrameKind, FrameBound, FrameBound), EngineError> {
    match frame {
        None => {
            if ordered {
                Ok((
                    FrameKind::Range,
                    FrameBound::UnboundedPreceding,
                    FrameBound::CurrentRow,
                ))
            } else {
                Ok((
                    FrameKind::Rows,
                    FrameBound::UnboundedPreceding,
                    FrameBound::UnboundedFollowing,
                ))
            }
        }
        Some(fr) => {
            let end = fr.end.clone().unwrap_or(FrameBound::CurrentRow);
            // v7.39 (round 229) — reject the frames whose start is after
            // their end. PG names each case; before this round SPG rejected
            // only the two unbounded ones (with its own wording) and let the
            // rest through, silently producing an all-NULL column for a
            // frame that can never contain a row. Wording and the case split
            // are PG18.4's (r229 probe).
            let starts_preceding = matches!(
                end,
                FrameBound::OffsetPreceding(_) | FrameBound::IntervalPreceding { .. }
            );
            if matches!(fr.start, FrameBound::UnboundedFollowing) {
                return Err(EngineError::Unsupported(
                    "frame start cannot be UNBOUNDED FOLLOWING".into(),
                ));
            }
            if matches!(end, FrameBound::UnboundedPreceding) {
                return Err(EngineError::Unsupported(
                    "frame end cannot be UNBOUNDED PRECEDING".into(),
                ));
            }
            if matches!(fr.start, FrameBound::CurrentRow) && starts_preceding {
                return Err(EngineError::Unsupported(
                    "frame starting from current row cannot have preceding rows".into(),
                ));
            }
            if matches!(
                fr.start,
                FrameBound::OffsetFollowing(_) | FrameBound::IntervalFollowing { .. }
            ) && (starts_preceding || matches!(end, FrameBound::CurrentRow))
            {
                return Err(EngineError::Unsupported(
                    "frame starting from following row cannot have preceding rows".into(),
                ));
            }
            // RANGE and GROUPS offset bounds are both supported now (see
            // frame_bounds_for_row → range_offset_bounds / groups_offset_bounds).
            Ok((fr.kind, fr.start.clone(), end))
        }
    }
}

/// Compute `(lo, hi)` row-index bounds inside the partition slice
/// for the row at position `i`. Inclusive, clamped to
/// `[0, slice.len()-1]`. Empty result if `lo > hi`.
#[allow(
    clippy::type_complexity,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation
)]
fn frame_bounds_for_row(
    eff: &(FrameKind, FrameBound, FrameBound),
    i: usize,
    slice: &[(Vec<Value<'static>>, Vec<(Value, bool, Option<bool>)>, usize)],
) -> Result<(usize, usize), EngineError> {
    let (kind, start, end) = eff;
    let n = slice.len();
    let last = n.saturating_sub(1);
    // RANGE with an explicit numeric offset (`RANGE BETWEEN 1 PRECEDING AND 1
    // FOLLOWING`): the frame is value-based — row j is in it iff its single
    // ORDER BY key sits within the offset window around row i's key. Handled
    // before the peer-aware Range arm below (which covers UNBOUNDED / CURRENT).
    let is_int_offset = |b: &FrameBound| {
        matches!(
            b,
            FrameBound::OffsetPreceding(_) | FrameBound::OffsetFollowing(_)
        )
    };
    let is_interval_offset = |b: &FrameBound| {
        matches!(
            b,
            FrameBound::IntervalPreceding { .. } | FrameBound::IntervalFollowing { .. }
        )
    };
    // An INTERVAL offset is only meaningful in a RANGE frame over a
    // temporal ORDER BY column — PG rejects it for ROWS / GROUPS.
    if (is_interval_offset(start) || is_interval_offset(end)) && !matches!(kind, FrameKind::Range) {
        return Err(EngineError::Unsupported(
            "INTERVAL frame offset is only valid in a RANGE frame".into(),
        ));
    }
    let has_offset = is_int_offset(start)
        || is_int_offset(end)
        || is_interval_offset(start)
        || is_interval_offset(end);
    if has_offset && matches!(kind, FrameKind::Range) {
        return range_offset_bounds(start, end, i, slice);
    }
    // GROUPS offset (`GROUPS N PRECEDING`, PG 11+) counts N peer groups in the
    // row order (direction already baked into the sort, so no value flip).
    if has_offset && matches!(kind, FrameKind::Groups) {
        return groups_offset_bounds(start, end, i, slice);
    }
    let (mut lo, mut hi) = match kind {
        FrameKind::Rows => {
            // Compute the raw (signed) row indices first so a frame
            // that lies ENTIRELY before the partition start (e.g.
            // `ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING` on row 0) or
            // entirely past the end (`1 FOLLOWING AND 2 FOLLOWING` on
            // the last row) is recognised as EMPTY. The old
            // saturating_sub / .min(last) collapsed such bounds onto
            // index 0 / `last`, wrongly pulling the boundary row into
            // the frame — PG returns NULL for a fully out-of-range
            // ROWS frame, not the boundary value.
            let i_s = i as i64;
            let last_s = last as i64;
            let bound = |b: &FrameBound| -> i64 {
                match b {
                    FrameBound::UnboundedPreceding => 0,
                    FrameBound::OffsetPreceding(k) => i_s - (*k as i64),
                    FrameBound::CurrentRow => i_s,
                    FrameBound::OffsetFollowing(k) => i_s + (*k as i64),
                    FrameBound::UnboundedFollowing => last_s,
                    // INTERVAL offsets are rejected for ROWS above.
                    FrameBound::IntervalPreceding { .. } | FrameBound::IntervalFollowing { .. } => {
                        unreachable!("INTERVAL offset rejected for ROWS frames")
                    }
                }
            };
            let lo_s = bound(start);
            let hi_s = bound(end);
            // Empty frame: end before partition start, start past the
            // partition end, or (degenerate) start after end.
            if hi_s < 0 || lo_s > last_s || lo_s > hi_s {
                return Ok((1, 0)); // lo > hi ⇒ caller treats as empty
            }
            let lo = lo_s.max(0) as usize;
            let hi = hi_s.min(last_s) as usize;
            (lo, hi)
        }
        FrameKind::Range | FrameKind::Groups => {
            // RANGE bounds are peer-aware. With only UNBOUNDED and
            // CURRENT ROW supported (rejected at effective_frame for
            // explicit offsets), the start/end map to the
            // partition's full extent at the same-order-key peer
            // group boundary.
            //
            // v7.37.19 (19.11) — GROUPS with UNBOUNDED / CURRENT ROW
            // bounds behaves identically to RANGE. Integer-offset
            // GROUPS (PG 11+ `GROUPS N PRECEDING`) is rejected at
            // effective_frame the same way RANGE offsets are.
            let lo = match start {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::CurrentRow => peer_group_start(slice, i),
                FrameBound::UnboundedFollowing => last,
                _ => unreachable!("offset bounds rejected for RANGE/GROUPS"),
            };
            let hi = match end {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::CurrentRow => peer_group_end(slice, i),
                FrameBound::UnboundedFollowing => last,
                _ => unreachable!("offset bounds rejected for RANGE/GROUPS"),
            };
            (lo, hi)
        }
    };
    if hi >= n {
        hi = last;
    }
    if lo >= n {
        lo = last;
    }
    Ok((lo, hi))
}

/// A single ORDER BY key rendered to f64 for RANGE-offset value arithmetic.
/// `None` unless there is exactly one order column and it is a real number.
#[allow(clippy::cast_precision_loss)]
fn range_order_key_f64(key: &[(Value, bool, Option<bool>)]) -> Option<(f64, bool)> {
    if key.len() != 1 {
        return None;
    }
    // The middle field is the per-key DESC flag (see order_key_cmp); asc = !desc.
    let (v, desc, _) = &key[0];
    let f = match v {
        Value::SmallInt(n) => f64::from(*n),
        Value::Int(n) => f64::from(*n),
        Value::BigInt(n) => *n as f64,
        Value::Float(x) => *x,
        Value::Numeric { scaled, scale, .. } => {
            (*scaled as f64) / (10i128.pow(u32::from(*scale)) as f64)
        }
        _ => return None,
    };
    Some((f, !*desc))
}

/// `RANGE BETWEEN <offset> PRECEDING/FOLLOWING` — value-based frame bounds. Row
/// `j` is in the frame iff its ORDER BY value lies within the offset window
/// around row `i`'s value. Requires a single numeric ORDER BY column (PG's own
/// restriction for numeric RANGE offsets); errors honestly otherwise.
#[allow(clippy::type_complexity)]
fn range_offset_bounds(
    start: &FrameBound,
    end: &FrameBound,
    i: usize,
    slice: &[(Vec<Value<'static>>, Vec<(Value, bool, Option<bool>)>, usize)],
) -> Result<(usize, usize), EngineError> {
    // An INTERVAL offset drives the temporal path (DATE / TIMESTAMP
    // ORDER BY key); the numeric-f64 path below handles integer /
    // numeric offsets.
    if matches!(
        start,
        FrameBound::IntervalPreceding { .. } | FrameBound::IntervalFollowing { .. }
    ) || matches!(
        end,
        FrameBound::IntervalPreceding { .. } | FrameBound::IntervalFollowing { .. }
    ) {
        return range_offset_bounds_interval(start, end, i, slice);
    }
    // v7.39 (round 229) — PG splits this into two messages: a key-count
    // complaint and a per-type one. Match both (probed against 18.4).
    let unsupported = || {
        let key = &slice[i].1;
        if key.len() != 1 {
            return EngineError::Unsupported(
                "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY column".into(),
            );
        }
        let ty = match key[0].0.data_type() {
            Some(t) => crate::system_catalog::pg_data_type_text(t),
            None => alloc::string::String::from("unknown"),
        };
        EngineError::Unsupported(alloc::format!(
            "RANGE with offset PRECEDING/FOLLOWING is not supported for column type {ty}"
        ))
    };
    let (v, asc) = range_order_key_f64(&slice[i].1).ok_or_else(unsupported)?;
    // The value a bound resolves to, given the ordering direction. PRECEDING is
    // "earlier in the order": smaller values under ASC, larger under DESC.
    // FOLLOWING is the mirror. UNBOUNDED means no limit on that side.
    let bound_value = |b: &FrameBound| -> Option<f64> {
        match b {
            FrameBound::OffsetPreceding(k) => Some(if asc { v - *k as f64 } else { v + *k as f64 }),
            FrameBound::OffsetFollowing(k) => Some(if asc { v + *k as f64 } else { v - *k as f64 }),
            FrameBound::CurrentRow => Some(v),
            FrameBound::UnboundedPreceding | FrameBound::UnboundedFollowing => None,
            FrameBound::IntervalPreceding { .. } | FrameBound::IntervalFollowing { .. } => {
                unreachable!("interval offsets routed to range_offset_bounds_interval")
            }
        }
    };
    // Window in value space [lo_val, hi_val] (inclusive). Under ASC the start
    // bound sets the low value and the end bound the high value; under DESC the
    // roles flip because larger values come first in the ordering.
    let (lo_val, hi_val) = if asc {
        (bound_value(start), bound_value(end))
    } else {
        (bound_value(end), bound_value(start))
    };
    let mut lo = usize::MAX;
    let mut hi = 0usize;
    let mut found = false;
    for (j, row) in slice.iter().enumerate() {
        let (x, _) = range_order_key_f64(&row.1).ok_or_else(unsupported)?;
        let ge_lo = lo_val.is_none_or(|lv| x >= lv - 1e-9);
        let le_hi = hi_val.is_none_or(|hv| x <= hv + 1e-9);
        if ge_lo && le_hi {
            if !found {
                lo = j;
                found = true;
            }
            hi = j;
        }
    }
    if !found {
        return Ok((1, 0)); // empty frame
    }
    Ok((lo, hi))
}

/// A single DATE / TIMESTAMP ORDER BY key as microseconds-since-epoch,
/// for INTERVAL RANGE-offset arithmetic. `None` unless there is exactly
/// one order column and it is temporal.
fn range_order_key_micros(key: &[(Value, bool, Option<bool>)]) -> Option<(i64, bool)> {
    if key.len() != 1 {
        return None;
    }
    let (v, desc, _) = &key[0];
    let micros = match v {
        Value::Date(d) => crate::conversions::date_days_to_micros(*d),
        Value::Timestamp(t) => *t,
        _ => return None,
    };
    Some((micros, !*desc))
}

/// `RANGE BETWEEN <interval> PRECEDING/FOLLOWING` over a DATE / TIMESTAMP
/// ORDER BY column — PG time-series windows. Row `j` is in the frame iff
/// its temporal key lies within the calendar-aware interval window around
/// row `i`'s key. The boundary is computed by applying the interval to
/// row `i`'s own instant (so month offsets get PG's clamp-to-last-day),
/// then compared against every other row's instant.
#[allow(clippy::type_complexity)]
fn range_offset_bounds_interval(
    start: &FrameBound,
    end: &FrameBound,
    i: usize,
    slice: &[(Vec<Value<'static>>, Vec<(Value, bool, Option<bool>)>, usize)],
) -> Result<(usize, usize), EngineError> {
    let unsupported = || {
        EngineError::Unsupported(
            "RANGE INTERVAL offset frame requires a single DATE/TIMESTAMP ORDER BY column".into(),
        )
    };
    let (v, asc) = range_order_key_micros(&slice[i].1).ok_or_else(unsupported)?;
    // The instant a bound resolves to. PRECEDING is "earlier in the
    // order" — subtract the interval under ASC, add it under DESC;
    // FOLLOWING mirrors. Interval subtraction negates all components,
    // matching PG's `timestamp - interval`.
    let bound_value = |b: &FrameBound| -> Result<Option<i64>, EngineError> {
        let (mo, da, mi, subtract) = match b {
            FrameBound::IntervalPreceding {
                months,
                days,
                micros,
            } => (*months, *days, *micros, asc),
            FrameBound::IntervalFollowing {
                months,
                days,
                micros,
            } => (*months, *days, *micros, !asc),
            FrameBound::CurrentRow => return Ok(Some(v)),
            FrameBound::UnboundedPreceding | FrameBound::UnboundedFollowing => return Ok(None),
            FrameBound::OffsetPreceding(_) | FrameBound::OffsetFollowing(_) => {
                return Err(unsupported());
            }
        };
        let sign = if subtract { -1 } else { 1 };
        let boundary =
            eval::add_interval_to_micros(v, sign * i64::from(mo), sign * i64::from(da), sign * mi)
                .map_err(EngineError::Eval)?;
        Ok(Some(boundary))
    };
    let (lo_val, hi_val) = if asc {
        (bound_value(start)?, bound_value(end)?)
    } else {
        (bound_value(end)?, bound_value(start)?)
    };
    let mut lo = usize::MAX;
    let mut hi = 0usize;
    let mut found = false;
    for (j, row) in slice.iter().enumerate() {
        let (x, _) = range_order_key_micros(&row.1).ok_or_else(unsupported)?;
        let ge_lo = lo_val.is_none_or(|lv| x >= lv);
        let le_hi = hi_val.is_none_or(|hv| x <= hv);
        if ge_lo && le_hi {
            if !found {
                lo = j;
                found = true;
            }
            hi = j;
        }
    }
    if !found {
        return Ok((1, 0)); // empty frame
    }
    Ok((lo, hi))
}

/// `GROUPS BETWEEN <n> PRECEDING AND <n> FOLLOWING` — peer-group-counted frame
/// bounds. The slice is already sorted per the ORDER BY (direction baked in), so
/// "preceding" is simply earlier peer groups by row index. A start bound landing
/// past the partition end, or an end bound before its start, yields an empty
/// frame.
#[allow(clippy::type_complexity)]
fn groups_offset_bounds(
    start: &FrameBound,
    end: &FrameBound,
    i: usize,
    slice: &[(Vec<Value<'static>>, Vec<(Value, bool, Option<bool>)>, usize)],
) -> Result<(usize, usize), EngineError> {
    let last = slice.len().saturating_sub(1);
    // Start row of the peer group `k` groups before i's (clamped to 0).
    let back_start = |k: u64| -> usize {
        let mut g = peer_group_start(slice, i);
        for _ in 0..k {
            if g == 0 {
                break;
            }
            g = peer_group_start(slice, g - 1);
        }
        g
    };
    // End row of the peer group `k` groups after i's (clamped to last).
    let fwd_end = |k: u64| -> usize {
        let mut g = peer_group_end(slice, i);
        for _ in 0..k {
            if g >= last {
                break;
            }
            g = peer_group_end(slice, g + 1);
        }
        g
    };
    // Start row of the peer group `k` groups after i's — `None` if beyond the end.
    let fwd_start = |k: u64| -> Option<usize> {
        let mut s = peer_group_start(slice, i);
        for _ in 0..k {
            let e = peer_group_end(slice, s);
            if e >= last {
                return None;
            }
            s = e + 1;
        }
        Some(s)
    };
    // End row of the peer group `k` groups before i's — `None` if before the start.
    let back_end = |k: u64| -> Option<usize> {
        let mut e = peer_group_end(slice, i);
        for _ in 0..k {
            let s = peer_group_start(slice, e);
            if s == 0 {
                return None;
            }
            e = s - 1;
        }
        Some(e)
    };
    // INTERVAL offsets are rejected for GROUPS frames upstream.
    let interval_unreachable = || unreachable!("INTERVAL offset rejected for GROUPS frames");
    let lo = match start {
        FrameBound::UnboundedPreceding => 0,
        FrameBound::CurrentRow => peer_group_start(slice, i),
        FrameBound::OffsetPreceding(k) => back_start(*k),
        FrameBound::OffsetFollowing(k) => match fwd_start(*k) {
            Some(s) => s,
            None => return Ok((1, 0)),
        },
        FrameBound::UnboundedFollowing => last,
        FrameBound::IntervalPreceding { .. } | FrameBound::IntervalFollowing { .. } => {
            interval_unreachable()
        }
    };
    let hi = match end {
        FrameBound::UnboundedFollowing => last,
        FrameBound::CurrentRow => peer_group_end(slice, i),
        FrameBound::OffsetFollowing(k) => fwd_end(*k),
        FrameBound::OffsetPreceding(k) => match back_end(*k) {
            Some(e) => e,
            None => return Ok((1, 0)),
        },
        FrameBound::UnboundedPreceding => 0,
        FrameBound::IntervalPreceding { .. } | FrameBound::IntervalFollowing { .. } => {
            interval_unreachable()
        }
    };
    if lo > hi {
        return Ok((1, 0));
    }
    Ok((lo, hi))
}

/// Find the inclusive index of the first row with the same ORDER
/// BY key as `slice[i]`. Slice is already sorted by partition then
/// order, so peers are contiguous.
#[allow(clippy::type_complexity)]
fn peer_group_start(
    slice: &[(Vec<Value<'static>>, Vec<(Value, bool, Option<bool>)>, usize)],
    i: usize,
) -> usize {
    let key = &slice[i].1;
    let mut j = i;
    while j > 0 && order_key_cmp(&slice[j - 1].1, key) == core::cmp::Ordering::Equal {
        j -= 1;
    }
    j
}

/// Find the inclusive index of the last row with the same ORDER
/// BY key as `slice[i]`.
#[allow(clippy::type_complexity)]
fn peer_group_end(
    slice: &[(Vec<Value<'static>>, Vec<(Value, bool, Option<bool>)>, usize)],
    i: usize,
) -> usize {
    let key = &slice[i].1;
    let mut j = i;
    while j + 1 < slice.len() && order_key_cmp(&slice[j + 1].1, key) == core::cmp::Ordering::Equal {
        j += 1;
    }
    j
}
