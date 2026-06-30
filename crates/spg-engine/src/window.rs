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

use spg_sql::ast::{Expr, FrameBound, FrameKind, SelectItem, SelectStatement, WindowFrame};
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
        | Expr::Literal(_)
        | Expr::Placeholder(_)
        | Expr::Column(_) => false,
        Expr::Array(items) => items.iter().any(expr_has_window),
        Expr::ArraySubscript { target, index } => expr_has_window(target) || expr_has_window(index),
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
    slice: &[(Vec<Value<'static>>, Vec<(Value, bool, Option<bool>)>, usize)],
    filtered_rows: &[&Row<'static>],
    ctx: &EvalContext<'_>,
    out_vals: &mut [Value],
) -> Result<(), EngineError> {
    let ignore_nulls = matches!(null_treatment, spg_sql::ast::NullTreatment::Ignore);
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
            #[allow(clippy::needless_range_loop)]
            for i in 0..slice.len() {
                let (lo, hi) = frame_bounds_for_row(&eff, i, slice);
                let mut sum: f64 = 0.0;
                let mut count: i64 = 0;
                let mut min_v: Option<f64> = None;
                let mut max_v: Option<f64> = None;
                let mut row_count: i64 = 0;
                if lo <= hi {
                    for j in lo..=hi {
                        let v = &arg_values[j];
                        match lower.as_str() {
                            "count_star" => row_count += 1,
                            "count" => {
                                if !v.is_null() {
                                    count += 1;
                                }
                            }
                            _ => {
                                if let Some(x) = value_to_f64(v) {
                                    sum += x;
                                    count += 1;
                                    min_v = Some(min_v.map_or(x, |m| m.min(x)));
                                    max_v = Some(max_v.map_or(x, |m| m.max(x)));
                                }
                            }
                        }
                    }
                }
                let value = match lower.as_str() {
                    "count_star" => Value::BigInt(row_count),
                    "count" => Value::BigInt(count),
                    "sum" => Value::Float(sum),
                    "avg" => {
                        if count == 0 {
                            Value::Null
                        } else {
                            Value::Float(sum / count as f64)
                        }
                    }
                    "min" => min_v.map_or(Value::Null, Value::Float),
                    "max" => max_v.map_or(Value::Null, Value::Float),
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
                let (lo, hi) = frame_bounds_for_row(&eff, i, slice);
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
            // RANGE OFFSET PRECEDING / FOLLOWING needs value-typed
            // arithmetic on the ORDER BY key (e.g. `RANGE BETWEEN
            // INTERVAL '1 day' PRECEDING AND CURRENT ROW`). Not
            // implemented in v4.20.
            if matches!(fr.kind, FrameKind::Range | FrameKind::Groups)
                && (matches!(
                    fr.start,
                    FrameBound::OffsetPreceding(_) | FrameBound::OffsetFollowing(_)
                ) || matches!(
                    end,
                    FrameBound::OffsetPreceding(_) | FrameBound::OffsetFollowing(_)
                ))
            {
                return Err(EngineError::Unsupported(
                    "RANGE / GROUPS with explicit offset bounds is not supported (v7.37.19: only UNBOUNDED / CURRENT ROW for peer-aware frames)".into(),
                ));
            }
            Ok((fr.kind, fr.start.clone(), end))
        }
    }
}

/// Compute `(lo, hi)` row-index bounds inside the partition slice
/// for the row at position `i`. Inclusive, clamped to
/// `[0, slice.len()-1]`. Empty result if `lo > hi`.
#[allow(clippy::type_complexity)]
fn frame_bounds_for_row(
    eff: &(FrameKind, FrameBound, FrameBound),
    i: usize,
    slice: &[(Vec<Value<'static>>, Vec<(Value, bool, Option<bool>)>, usize)],
) -> (usize, usize) {
    let (kind, start, end) = eff;
    let n = slice.len();
    let last = n.saturating_sub(1);
    let (mut lo, mut hi) = match kind {
        FrameKind::Rows => {
            let lo = match start {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::OffsetPreceding(k) => {
                    let k = usize::try_from(*k).unwrap_or(usize::MAX);
                    i.saturating_sub(k)
                }
                FrameBound::CurrentRow => i,
                FrameBound::OffsetFollowing(k) => {
                    let k = usize::try_from(*k).unwrap_or(usize::MAX);
                    i.saturating_add(k).min(last)
                }
                FrameBound::UnboundedFollowing => last,
            };
            let hi = match end {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::OffsetPreceding(k) => {
                    let k = usize::try_from(*k).unwrap_or(usize::MAX);
                    i.saturating_sub(k)
                }
                FrameBound::CurrentRow => i,
                FrameBound::OffsetFollowing(k) => {
                    let k = usize::try_from(*k).unwrap_or(usize::MAX);
                    i.saturating_add(k).min(last)
                }
                FrameBound::UnboundedFollowing => last,
            };
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
    (lo, hi)
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
