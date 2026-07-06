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
    false
}

fn expr_has_window(e: &Expr) -> bool {
    match e {
        Expr::WindowFunction { .. } => true,
        Expr::AggregateOrdered { call, order_by, .. } => {
            expr_has_window(call) || order_by.iter().any(|o| expr_has_window(&o.expr))
        }
        Expr::Binary { lhs, rhs, .. } => expr_has_window(lhs) || expr_has_window(rhs),
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            expr_has_window(expr)
        }
        Expr::FunctionCall { args, .. } => args.iter().any(expr_has_window),
        Expr::Like { expr, pattern, .. } => expr_has_window(expr) || expr_has_window(pattern),
        Expr::Extract { source, .. } => expr_has_window(source),
        Expr::ScalarSubquery(_)
        | Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::RowInSubquery { .. }
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
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
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
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
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
            #[allow(clippy::needless_range_loop)]
            for i in 0..slice.len() {
                let (lo, hi) = frame_bounds_for_row(&eff, i, slice)?;
                let mut sum: f64 = 0.0;
                // v7.37.16 — exact NUMERIC accumulator for sum/avg over a
                // Numeric frame (was deferred → NULL). Only written when a
                // Numeric cell appears; the f64 path stays untouched.
                let mut num_scaled: i128 = 0;
                let mut num_scale: u8 = 0;
                let mut use_numeric = false;
                let mut count: i64 = 0;
                // min/max compare the *actual* Value via value_cmp so
                // they work on TEXT / DATE / NUMERIC (not just f64) and
                // preserve the column type. The old f64-only path
                // returned NULL for `max(text_col)` / `min(numeric_col)`
                // (value_to_f64 → None) — a silent divergence from PG,
                // which orders text lexically and numerics exactly.
                let mut min_val: Option<Value<'static>> = None;
                let mut max_val: Option<Value<'static>> = None;
                let mut row_count: i64 = 0;
                if lo <= hi {
                    for j in lo..=hi {
                        // EXCLUDE CURRENT ROW drops the current row
                        // from the aggregate frame.
                        if exclude == FrameExclusion::CurrentRow && j == i {
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
                                if let Value::Numeric { scaled, scale } = v {
                                    let (s, sc) = crate::numeric::numeric_add(
                                        num_scaled, num_scale, *scaled, *scale,
                                    );
                                    num_scaled = s;
                                    num_scale = sc;
                                    use_numeric = true;
                                    count += 1;
                                } else if let Some(x) = value_to_f64(v) {
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
                            Value::Numeric { scaled: num_scaled, scale: num_scale }
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
                            Value::Numeric { scaled, scale }
                        } else {
                            Value::Float(sum / count as f64)
                        }
                    }
                    "min" => min_val.unwrap_or(Value::Null),
                    "max" => max_val.unwrap_or(Value::Null),
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
            for i in 0..slice.len() {
                let (lo, hi) = frame_bounds_for_row(&eff, i, slice)?;
                let (_, _, idx) = &slice[i];
                let v = if lo > hi {
                    Value::Null
                } else if ignore_nulls && matches!(lower.as_str(), "first_value" | "last_value") {
                    // v6.4.2 — IGNORE NULLS: skip NULL cells when
                    // selecting the boundary value within the frame.
                    if lower == "first_value" {
                        (lo..=hi)
                            .find_map(|j| {
                                let v = &values[j];
                                (!v.is_null()).then(|| v.clone())
                            })
                            .unwrap_or(Value::Null)
                    } else {
                        (lo..=hi)
                            .rev()
                            .find_map(|j| {
                                let v = &values[j];
                                (!v.is_null()).then(|| v.clone())
                            })
                            .unwrap_or(Value::Null)
                    }
                } else {
                    match lower.as_str() {
                        "first_value" => values[lo].clone(),
                        "last_value" => values[hi].clone(),
                        "nth_value" => {
                            let pos = lo + nth - 1;
                            if pos > hi {
                                Value::Null
                            } else {
                                values[pos].clone()
                            }
                        }
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
        other => Err(EngineError::Unsupported(alloc::format!(
            "window function {other:?} not supported (v4.21: row_number/rank/dense_rank/sum/avg/count/min/max/lag/lead/first_value/last_value/nth_value/ntile/percent_rank/cume_dist)"
        ))),
    }
}

/// v4.20: resolve the user-provided frame down to a normalised
/// `(kind, start, end)`. `None` means default — derive from
/// `ordered`: ordered ⇒ RANGE UNBOUNDED PRECEDING AND CURRENT ROW,
/// unordered ⇒ ROWS UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING.
/// Single-bound shorthand (e.g. `ROWS 5 PRECEDING`) normalises
/// end → CURRENT ROW per the PG spec.
/// The frame's EXCLUDE mode. GROUP / TIES need peer-group awareness
/// that the aggregate loop doesn't have yet, so they error honestly;
/// CURRENT ROW and the default NO OTHERS are supported.
fn frame_exclusion(frame: Option<&WindowFrame>) -> Result<FrameExclusion, EngineError> {
    let ex = frame.map_or(FrameExclusion::NoOthers, |f| f.exclude);
    if matches!(ex, FrameExclusion::Group | FrameExclusion::Ties) {
        return Err(EngineError::Unsupported(
            "EXCLUDE GROUP / TIES is not supported yet (only CURRENT ROW / NO OTHERS)".into(),
        ));
    }
    Ok(ex)
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
            // Reject start > end (a few impossible combinations).
            if matches!(fr.start, FrameBound::UnboundedFollowing)
                || matches!(end, FrameBound::UnboundedPreceding)
            {
                return Err(EngineError::Unsupported(alloc::format!(
                    "invalid frame: start={:?} end={:?}",
                    fr.start,
                    end
                )));
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
    let is_int_offset =
        |b: &FrameBound| matches!(b, FrameBound::OffsetPreceding(_) | FrameBound::OffsetFollowing(_));
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
        Value::Numeric { scaled, scale } => {
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
    let unsupported = || {
        EngineError::Unsupported(
            "RANGE offset frame requires a single numeric ORDER BY column".into(),
        )
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
        Value::Date(d) => i64::from(*d) * 86_400_000_000,
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
