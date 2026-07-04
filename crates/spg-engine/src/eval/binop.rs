//! Operator evaluation split out of `eval.rs` (cut 33): the pure
//! value-level unary / binary operator machinery that `eval_expr` (and
//! the compiled-expression stepper) dispatch into. Covers `apply_unary`,
//! `apply_binary` and its arithmetic / comparison / 3VL-logic / vector /
//! interval-calendar sub-evaluators (apply_binary_numeric,
//! apply_binary_calendar, apply_binary_interval, compare, and_3vl /
//! or_3vl, the pgvector distance ops, bit / shift arith, numeric rescale).
//! These are business-agnostic value math — no `EvalContext` or `Row`
//! entanglement. The calendar primitives they lean on (`civil_from_days`,
//! `add_months_to_civil`, `days_from_civil`) and the central
//! `value_to_text` renderer stay in `eval.rs` and are reached via
//! `use super::`.

use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;

use spg_sql::ast::{BinOp, UnOp};
use spg_storage::{DataType, Value};

use super::{
    EvalError, add_months_to_civil, civil_from_days, days_from_civil, inet_op_bool_result,
    parse_date_literal, parse_timestamp_literal, ts_match, tsvector_concat, value_to_text,
};

pub(super) fn apply_unary(op: UnOp, v: Value<'static>) -> Result<Value<'static>, EvalError> {
    match (op, v) {
        (_, Value::Null) => Ok(Value::Null),
        (UnOp::Neg, Value::Int(n)) => {
            n.checked_neg()
                .map(Value::Int)
                .ok_or(EvalError::TypeMismatch {
                    detail: "integer overflow on unary -".into(),
                })
        }
        (UnOp::Neg, Value::BigInt(n)) => {
            n.checked_neg()
                .map(Value::BigInt)
                .ok_or(EvalError::TypeMismatch {
                    detail: "bigint overflow on unary -".into(),
                })
        }
        (UnOp::Neg, Value::Float(x)) => Ok(Value::Float(-x)),
        (
            UnOp::Neg,
            Value::Interval {
                months,
                days,
                micros,
            },
        ) => {
            let overflow = || EvalError::TypeMismatch {
                detail: "INTERVAL overflows on unary -".into(),
            };
            Ok(Value::Interval {
                months: months.checked_neg().ok_or_else(overflow)?,
                days: days.checked_neg().ok_or_else(overflow)?,
                micros: micros.checked_neg().ok_or_else(overflow)?,
            })
        }
        (UnOp::Neg, other) => Err(EvalError::TypeMismatch {
            detail: format!("unary - applied to {:?}", other.data_type()),
        }),
        (UnOp::BitNot, Value::SmallInt(n)) => Ok(Value::Int(!i32::from(n))),
        (UnOp::BitNot, Value::Int(n)) => Ok(Value::Int(!n)),
        (UnOp::BitNot, Value::BigInt(n)) => Ok(Value::BigInt(!n)),
        (UnOp::BitNot, other) => Err(EvalError::TypeMismatch {
            detail: format!("cannot apply ~ to {other:?}"),
        }),
        (UnOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
        (UnOp::Not, other) => Err(EvalError::TypeMismatch {
            detail: format!("NOT applied to {:?}", other.data_type()),
        }),
    }
}

/// v7.9.27b — true when two values are "not distinct" per PG:
/// both NULL counts as equal; otherwise reduces to regular Eq.
fn values_not_distinct(l: &Value<'_>, r: &Value<'_>) -> bool {
    match (l, r) {
        (Value::Null, Value::Null) => true,
        (Value::Null, _) | (_, Value::Null) => false,
        _ => l == r,
    }
}

/// v7.37.9 T3 S4 — by-reference comparison/3VL fast path.
///
/// Lifts the operand contract from owned `Value<'static>` to borrowed
/// `&Value<'_>` for the **read-only** binary ops where the result is a
/// fresh `Value::Bool` / `Value::Null` and the operand bytes are never
/// stored in the result. Eliminates the per-pop `.into_owned()` that
/// the owning `apply_binary` requires (which String::clones every
/// Text/Bytes/Json/Vector value off the Step VM stack).
///
/// Returns `None` for ops that build a new owned result (Add / Sub /
/// Concat / Json* / arithmetic etc.) — the caller falls through to the
/// owning `apply_binary` path. This keeps the by-ref change a pure
/// optimization with no behaviour change for non-comparison ops.
pub(crate) fn apply_binary_by_ref(
    op: BinOp,
    l: &Value<'_>,
    r: &Value<'_>,
) -> Result<Option<Value<'static>>, EvalError> {
    // 3VL And/Or/IS [NOT] DISTINCT FROM — NULL handling without
    // moving operands. These mirror the owning path's pre-checks.
    if let BinOp::IsNotDistinctFrom = op {
        return Ok(Some(Value::Bool(values_not_distinct(l, r))));
    }
    if let BinOp::IsDistinctFrom = op {
        return Ok(Some(Value::Bool(!values_not_distinct(l, r))));
    }
    if let BinOp::And = op {
        return Ok(Some(and_3vl_by_ref(l, r)));
    }
    if let BinOp::Or = op {
        return Ok(Some(or_3vl_by_ref(l, r)));
    }
    // Any NULL operand → NULL for the remaining ops.
    if l.is_null() || r.is_null() {
        match op {
            BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => {
                return Ok(Some(Value::Null));
            }
            _ => return Ok(None),
        }
    }
    match op {
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => {
            Ok(Some(compare(op, l, r)?))
        }
        // Everything else needs owned semantics; caller falls back.
        _ => Ok(None),
    }
}

/// Read-only 3VL AND/OR mirror of `and_3vl` / `or_3vl` for the by-ref
/// path. Reuses the existing helpers via cheap `.clone()` only when
/// non-NULL branches need to be promoted to the owning code path; for
/// the common case (one side NULL) we resolve directly.
fn and_3vl_by_ref(l: &Value<'_>, r: &Value<'_>) -> Value<'static> {
    // 3VL truth table:
    //   FALSE AND _     = FALSE
    //   _ AND FALSE     = FALSE
    //   NULL AND TRUE/NULL = NULL
    //   TRUE AND TRUE   = TRUE
    match (l, r) {
        (Value::Bool(false), _) | (_, Value::Bool(false)) => Value::Bool(false),
        (Value::Bool(true), Value::Bool(true)) => Value::Bool(true),
        _ => Value::Null,
    }
}

fn or_3vl_by_ref(l: &Value<'_>, r: &Value<'_>) -> Value<'static> {
    // TRUE OR _   = TRUE
    // _ OR TRUE   = TRUE
    // NULL OR FALSE/NULL = NULL
    // FALSE OR FALSE = FALSE
    match (l, r) {
        (Value::Bool(true), _) | (_, Value::Bool(true)) => Value::Bool(true),
        (Value::Bool(false), Value::Bool(false)) => Value::Bool(false),
        _ => Value::Null,
    }
}

pub(super) fn apply_binary(
    op: BinOp,
    l: Value<'static>,
    r: Value<'static>,
) -> Result<Value<'static>, EvalError> {
    // SQL three-valued logic for AND / OR with NULL is special — handle before
    // the general NULL-propagation rule.
    if let BinOp::And = op {
        return and_3vl(l, r);
    }
    if let BinOp::Or = op {
        return or_3vl(l, r);
    }
    // v7.9.27b — IS [NOT] DISTINCT FROM. NULL-safe equality:
    // `NULL IS NOT DISTINCT FROM NULL` → true. mailrs pg_dump.
    if let BinOp::IsNotDistinctFrom = op {
        return Ok(Value::Bool(values_not_distinct(&l, &r)));
    }
    if let BinOp::IsDistinctFrom = op {
        return Ok(Value::Bool(!values_not_distinct(&l, &r)));
    }
    // Everything else: any NULL operand → NULL.
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    // NUMERIC arithmetic and comparisons run in fixed-point; promote
    // integers to a common NUMERIC scale and stay in i128 throughout.
    // A NUMERIC paired with an INTERVAL is interval scaling, not
    // numeric math — let the calendar path below take it.
    if (matches!(l, Value::Numeric { .. }) || matches!(r, Value::Numeric { .. }))
        && !matches!(l, Value::Interval { .. })
        && !matches!(r, Value::Interval { .. })
    {
        return apply_binary_numeric(op, l, r);
    }
    // Date / Timestamp arithmetic. PG semantics:
    //   * date + int      → date  (int is days)
    //   * int + date      → date
    //   * date - int      → date
    //   * date - date     → int   (days, signed)
    //   * timestamp - timestamp → bigint (microseconds, signed)
    // Other date/time math (`timestamp + int`, INTERVAL) lands later.
    if let Some(result) = apply_binary_calendar(op, &l, &r)? {
        return Ok(result);
    }
    match op {
        BinOp::Add => arith(l, r, i64::checked_add, |a, b| a + b, "+"),
        // PG `jsonb - text` / `jsonb - int` / `jsonb - text[]` deletes an
        // object key, an array element, or a set of object keys. Routes
        // to the same code the `jsonb_delete` function uses.
        BinOp::Sub if matches!(l, Value::Json(_)) => crate::json::delete_key(&l, &r),
        BinOp::Sub => arith(l, r, i64::checked_sub, |a, b| a - b, "-"),
        BinOp::Mul => arith(l, r, i64::checked_mul, |a, b| a * b, "*"),
        BinOp::Div => div_op(l, r),
        BinOp::Mod => mod_op(l, r),
        BinOp::L2Distance => l2_distance(l, r),
        BinOp::InnerProduct => inner_product(l, r),
        BinOp::CosineDistance => cosine_distance(l, r),
        // PG `jsonb || jsonb` merges objects (right wins on dup keys) /
        // appends arrays. Text `||` stays text concatenation.
        BinOp::Concat if matches!(l, Value::Json(_)) || matches!(r, Value::Json(_)) => {
            crate::json::concat(&l, &r)
        }
        BinOp::Concat => Ok(text_concat(&l, &r)),
        // PG `jsonb #- text[]` deletes the value at a nested path.
        BinOp::JsonDeletePath => crate::json::delete_path(&[l, r]),
        BinOp::BitOr => bitop(l, r, |a, b| a | b, "|"),
        BinOp::BitAnd => bitop(l, r, |a, b| a & b, "&"),
        BinOp::JsonGet => crate::json::path_get(&l, &r, false),
        BinOp::JsonGetText => crate::json::path_get(&l, &r, true),
        BinOp::JsonGetPath => crate::json::path_walk(&l, &r, false),
        BinOp::JsonGetPathText => crate::json::path_walk(&l, &r, true),
        // Array operands claim && / @> / <@ before the inet / JSON
        // interpretations: ARRAY[1,2] && ARRAY[2,3] is the overlap
        // test, ARRAY[1,2,3] @> ARRAY[2] the containment test.
        BinOp::JsonContains
            if array_scalar_elems(&l).is_some() && array_scalar_elems(&r).is_some() =>
        {
            Ok(Value::Bool(array_contains_all(&l, &r)))
        }
        BinOp::JsonContainedBy
            if array_scalar_elems(&l).is_some() && array_scalar_elems(&r).is_some() =>
        {
            Ok(Value::Bool(array_contains_all(&r, &l)))
        }
        BinOp::InetOverlap
            if array_scalar_elems(&l).is_some() && array_scalar_elems(&r).is_some() =>
        {
            let (a, _) = array_scalar_elems(&l).expect("guard checked");
            let (b, _) = array_scalar_elems(&r).expect("guard checked");
            Ok(Value::Bool(
                a.iter().any(|x| b.iter().any(|y| x == y)),
            ))
        }
        BinOp::JsonContains => crate::json::contains(&l, &r),
        // v7.37.6-A `<@` reuses `@>` with swapped args.
        BinOp::JsonContainedBy => crate::json::contains(&r, &l),
        BinOp::JsonKeyExists => crate::json::key_exists(&l, &r),
        BinOp::JsonKeysAny => crate::json::keys_any(&l, &r),
        BinOp::JsonKeysAll => crate::json::keys_all(&l, &r),
        // v7.12.2 — `@@` match. NULL on either side → NULL; PG
        // accepts both orderings so we normalise.
        BinOp::TsMatch => ts_match(l, r),
        // Integer operands claim << / >> as bit shifts before the
        // inet containment interpretation (PG int4/int8 shift ops).
        BinOp::InetContainedBy | BinOp::InetContains
            if int_operand(&l).is_some() && int_operand(&r).is_some() =>
        {
            let a = int_operand(&l).expect("guard checked");
            let n = int_operand(&r).expect("guard checked");
            let shifted = if matches!(op, BinOp::InetContainedBy) {
                // PG masks the shift count to the operand width.
                a.wrapping_shl((n & 63) as u32)
            } else {
                a.wrapping_shr((n & 63) as u32)
            };
            if matches!(l, Value::BigInt(_)) {
                Ok(Value::BigInt(shifted))
            } else {
                #[allow(clippy::cast_possible_truncation)]
                Ok(Value::Int(shifted as i32))
            }
        }
        // v7.17.0 Phase 3.P0-47 — PG INET / CIDR containment + overlap.
        BinOp::InetContainedBy
        | BinOp::InetContainedByEq
        | BinOp::InetContains
        | BinOp::InetContainsEq
        | BinOp::InetOverlap => inet_op_bool_result(op, &l, &r),
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => {
            compare(op, &l, &r)
        }
        BinOp::And | BinOp::Or | BinOp::IsDistinctFrom | BinOp::IsNotDistinctFrom => {
            unreachable!("handled above")
        }
    }
}

/// Calendar arithmetic. Returns `Some(value)` when the operand pair
/// is a date/time combo this function understands, `None` to let the
/// caller fall through to the regular numeric / text paths.
fn apply_binary_calendar(
    op: BinOp,
    l: &Value<'static>,
    r: &Value<'static>,
) -> Result<Option<Value<'static>>, EvalError> {
    let int_value = |v: &Value| -> Option<i64> {
        match v {
            Value::SmallInt(n) => Some(i64::from(*n)),
            Value::Int(n) => Some(i64::from(*n)),
            Value::BigInt(n) => Some(*n),
            _ => None,
        }
    };
    // Most-specific cases first — DATE-DATE / TS-TS subtraction before
    // DATE-integer subtraction, otherwise the latter swallows the
    // former with an `int_value(Date) = None` no-op fall-through.
    match (l, r) {
        (Value::Date(a), Value::Date(b)) if op == BinOp::Sub => {
            return Ok(Some(Value::BigInt(i64::from(*a) - i64::from(*b))));
        }
        (Value::Timestamp(a), Value::Timestamp(b)) if op == BinOp::Sub => {
            let delta = a.checked_sub(*b).ok_or(EvalError::TypeMismatch {
                detail: "TIMESTAMP - TIMESTAMP overflows i64 microseconds".into(),
            })?;
            return Ok(Some(Value::BigInt(delta)));
        }
        _ => {}
    }
    // INTERVAL arithmetic. PG: timestamp ± interval → timestamp,
    // date ± interval → date (if interval is pure days/months with no
    // sub-day component) else timestamp, interval ± interval → interval.
    if let Some(out) = apply_binary_interval(op, l, r)? {
        return Ok(Some(out));
    }
    match (l, r) {
        (Value::Date(d), other) if op == BinOp::Add => {
            if let Some(n) = int_value(other) {
                let days = i64::from(*d).saturating_add(n);
                let days32 = i32::try_from(days).map_err(|_| EvalError::TypeMismatch {
                    detail: "DATE + integer overflows DATE range".into(),
                })?;
                return Ok(Some(Value::Date(days32)));
            }
        }
        (other, Value::Date(d)) if op == BinOp::Add => {
            if let Some(n) = int_value(other) {
                let days = i64::from(*d).saturating_add(n);
                let days32 = i32::try_from(days).map_err(|_| EvalError::TypeMismatch {
                    detail: "integer + DATE overflows DATE range".into(),
                })?;
                return Ok(Some(Value::Date(days32)));
            }
        }
        (Value::Date(d), other) if op == BinOp::Sub => {
            if let Some(n) = int_value(other) {
                let days = i64::from(*d).saturating_sub(n);
                let days32 = i32::try_from(days).map_err(|_| EvalError::TypeMismatch {
                    detail: "DATE - integer overflows DATE range".into(),
                })?;
                return Ok(Some(Value::Date(days32)));
            }
        }
        _ => {}
    }
    Ok(None)
}

/// INTERVAL-aware binary ops. Recognises:
///   timestamp ± interval → timestamp
///   date ± interval      → date (if interval is integral days/months only)
///                       → timestamp (if interval has sub-day micros)
///   interval ± interval  → interval
/// Commutative for `+`. Returns `None` for unrecognised operand pairs so
/// the caller can fall through.
pub(crate) fn apply_binary_interval(
    op: BinOp,
    l: &Value<'static>,
    r: &Value<'static>,
) -> Result<Option<Value<'static>>, EvalError> {
    // interval * numeric (either order) and interval / numeric scale
    // the interval per PG's interval_mul: months and days truncate
    // toward zero and their fractional remainders spill downward
    // (fractional month → 30 days, fractional day → microseconds).
    match (l, r, op) {
        (Value::Interval { .. }, other, BinOp::Mul)
        | (other, Value::Interval { .. }, BinOp::Mul)
            if as_f64(other).is_ok() =>
        {
            let iv = if matches!(l, Value::Interval { .. }) { l } else { r };
            return scale_interval(iv, as_f64(other)?).map(Some);
        }
        (Value::Interval { .. }, other, BinOp::Div) if as_f64(other).is_ok() => {
            let divisor = as_f64(other)?;
            if divisor == 0.0 {
                return Err(EvalError::DivisionByZero);
            }
            return scale_interval(l, 1.0 / divisor).map(Some);
        }
        _ => {}
    }
    // Normalise so the interval (if any) is always on the right for Add;
    // Sub stays left-handed because it isn't commutative.
    let (lhs, rhs, sign): (&Value<'static>, &Value<'static>, i64) = match (l, r, op) {
        (Value::Interval { .. }, _, BinOp::Add) => (r, l, 1),
        (_, Value::Interval { .. }, BinOp::Add) => (l, r, 1),
        (_, Value::Interval { .. }, BinOp::Sub) => (l, r, -1),
        _ => return Ok(None),
    };
    let Value::Interval {
        months: rhs_months,
        days: rhs_days,
        micros: rhs_us,
    } = rhs
    else {
        unreachable!("rhs guaranteed to be Interval by the match above");
    };
    let signed_months = i64::from(*rhs_months) * sign;
    let signed_days = i64::from(*rhs_days) * sign;
    let signed_micros = rhs_us.checked_mul(sign).ok_or(EvalError::TypeMismatch {
        detail: "INTERVAL micros overflows on negation".into(),
    })?;
    match lhs {
        // TIME ± INTERVAL wraps within the day (PG semantics): only
        // the sub-day microseconds apply, and the result is taken
        // modulo 24 hours so it stays a valid time of day.
        Value::Time(t) => {
            const DAY_US: i64 = 86_400_000_000;
            let shifted = t
                .checked_add(signed_micros)
                .ok_or(EvalError::TypeMismatch {
                    detail: "TIME ± INTERVAL overflows i64 microseconds".into(),
                })?;
            Ok(Some(Value::Time(shifted.rem_euclid(DAY_US))))
        }
        Value::Timestamp(t) => Ok(Some(Value::Timestamp(add_interval_to_micros(
            *t,
            signed_months,
            signed_days,
            signed_micros,
        )?))),
        Value::Date(d) => {
            // Date + interval stays a date when the interval has zero
            // sub-day microseconds; otherwise promote to TIMESTAMP at
            // midnight of the (months-shifted, days-shifted) date.
            if signed_micros == 0 {
                let shifted = shift_date_by_months(*d, signed_months)?;
                let new_days =
                    i64::from(shifted)
                        .checked_add(signed_days)
                        .ok_or(EvalError::TypeMismatch {
                            detail: "DATE ± INTERVAL overflows DATE range".into(),
                        })?;
                let days32 = i32::try_from(new_days).map_err(|_| EvalError::TypeMismatch {
                    detail: "DATE ± INTERVAL overflows DATE range".into(),
                })?;
                Ok(Some(Value::Date(days32)))
            } else {
                let base =
                    i64::from(*d)
                        .checked_mul(86_400_000_000)
                        .ok_or(EvalError::TypeMismatch {
                            detail: "DATE → TIMESTAMP lift overflows for INTERVAL math".into(),
                        })?;
                Ok(Some(Value::Timestamp(add_interval_to_micros(
                    base,
                    signed_months,
                    signed_days,
                    signed_micros,
                )?)))
            }
        }
        Value::Interval {
            months: lhs_months,
            days: lhs_days,
            micros: lhs_us,
        } => {
            let new_months = i64::from(*lhs_months)
                .checked_add(signed_months)
                .and_then(|n| i32::try_from(n).ok())
                .ok_or(EvalError::TypeMismatch {
                    detail: "INTERVAL ± INTERVAL months overflows i32".into(),
                })?;
            let raw_days =
                i64::from(*lhs_days)
                    .checked_add(signed_days)
                    .ok_or(EvalError::TypeMismatch {
                        detail: "INTERVAL ± INTERVAL days overflows i64".into(),
                    })?;
            let raw_micros = lhs_us
                .checked_add(signed_micros)
                .ok_or(EvalError::TypeMismatch {
                    detail: "INTERVAL ± INTERVAL micros overflows i64".into(),
                })?;
            // v7.38 P1 (轴 1 pg_regress closure) — PG normalises
            // INTERVAL arithmetic at the day / sub-day boundary so
            // mixed-sign components (e.g. `1 day - 12 hours`) collapse
            // to a single sign before display. Rule:
            //   total_us = days·86_400e6 + micros
            //   new_days  = total_us / 86_400e6   (truncate toward 0)
            //   new_micros = total_us % 86_400e6  (keeps total_us sign)
            // No-op when signs already align (both positive or both
            // negative); kills the `1 day -12:00:00` artefact.
            // Months stay separate — their length is ambiguous (28-31
            // days), and PG never merges them into the day count
            // implicitly.
            const US_PER_DAY: i64 = 86_400_000_000;
            let total_us = raw_days
                .checked_mul(US_PER_DAY)
                .and_then(|x| x.checked_add(raw_micros))
                .ok_or(EvalError::TypeMismatch {
                    detail: "INTERVAL ± INTERVAL day-micros normalise overflows i64".into(),
                })?;
            let norm_days_i64 = total_us / US_PER_DAY;
            let norm_micros = total_us % US_PER_DAY;
            let new_days = i32::try_from(norm_days_i64).map_err(|_| EvalError::TypeMismatch {
                detail: "INTERVAL ± INTERVAL day count exceeds i32".into(),
            })?;
            Ok(Some(Value::Interval {
                months: new_months,
                days: new_days,
                micros: norm_micros,
            }))
        }
        _ => Err(EvalError::TypeMismatch {
            detail: format!(
                "operator {op:?} not defined for {:?} and INTERVAL",
                lhs.data_type()
            ),
        }),
    }
}

/// Scale an interval by a float factor per PG's `interval_mul`
/// (timestamp.c): months and days truncate toward zero; the
/// fractional month remainder spills into days at 30 days/month and
/// the fractional day remainder spills into microseconds.
fn scale_interval(iv: &Value<'static>, factor: f64) -> Result<Value<'static>, EvalError> {
    let Value::Interval {
        months,
        days,
        micros,
    } = iv
    else {
        unreachable!("caller guarantees Interval");
    };
    if !factor.is_finite() {
        return Err(EvalError::TypeMismatch {
            detail: "INTERVAL scale factor must be finite".into(),
        });
    }
    let m = f64::from(*months) * factor;
    let new_months = m.trunc();
    let d = f64::from(*days) * factor + (m - new_months) * 30.0;
    let new_days = d.trunc();
    let new_micros = (*micros as f64) * factor + (d - new_days) * 86_400_000_000.0;
    let months_ok = new_months >= f64::from(i32::MIN) && new_months <= f64::from(i32::MAX);
    let days_ok = new_days >= f64::from(i32::MIN) && new_days <= f64::from(i32::MAX);
    let micros_ok = new_micros >= -9.3e18 && new_micros <= 9.3e18;
    if !(months_ok && days_ok && micros_ok) {
        return Err(EvalError::TypeMismatch {
            detail: "INTERVAL out of range after scaling".into(),
        });
    }
    #[allow(clippy::cast_possible_truncation)]
    Ok(Value::Interval {
        months: new_months as i32,
        days: new_days as i32,
        micros: libm::rint(new_micros) as i64,
    })
}

/// Shift a `Date` by a signed number of months using the PG clamp rule.
fn shift_date_by_months(d: i32, months: i64) -> Result<i32, EvalError> {
    let (y, m, day) = civil_from_days(d);
    let months_i32 = i32::try_from(months).map_err(|_| EvalError::TypeMismatch {
        detail: "INTERVAL months delta out of i32 range".into(),
    })?;
    let (ny, nm, nd) = add_months_to_civil(y, m, day, months_i32);
    Ok(days_from_civil(ny, nm, nd))
}

/// Add (months, days, micros) to a `Timestamp` (microseconds since
/// epoch). Months part is applied through civil calendar with
/// clamp-to-last-day; days part is a plain 86_400_000_000 micros each
/// (DST-naïve at the TIMESTAMP level, matching PG); micros part is
/// plain i64 addition with overflow guard. v7.37.5 β added the
/// `days` parameter.
fn add_interval_to_micros(t: i64, months: i64, days: i64, micros: i64) -> Result<i64, EvalError> {
    const MICROS_PER_DAY: i64 = 86_400_000_000;
    let mut out = t;
    if months != 0 {
        let day_count = out.div_euclid(MICROS_PER_DAY);
        let day_micros = out.rem_euclid(MICROS_PER_DAY);
        let day_i32 = i32::try_from(day_count).map_err(|_| EvalError::TypeMismatch {
            detail: "TIMESTAMP day component out of i32 range for INTERVAL months math".into(),
        })?;
        let shifted_days = shift_date_by_months(day_i32, months)?;
        out = i64::from(shifted_days)
            .checked_mul(MICROS_PER_DAY)
            .and_then(|n| n.checked_add(day_micros))
            .ok_or(EvalError::TypeMismatch {
                detail: "TIMESTAMP ± INTERVAL months overflows i64 microseconds".into(),
            })?;
    }
    if days != 0 {
        let day_micros = days
            .checked_mul(MICROS_PER_DAY)
            .ok_or(EvalError::TypeMismatch {
                detail: "INTERVAL days overflows i64 microseconds".into(),
            })?;
        out = out.checked_add(day_micros).ok_or(EvalError::TypeMismatch {
            detail: "TIMESTAMP ± INTERVAL days overflows i64".into(),
        })?;
    }
    out.checked_add(micros).ok_or(EvalError::TypeMismatch {
        detail: "TIMESTAMP ± INTERVAL micros overflows i64".into(),
    })
}

/// Dispatch for any binary op when at least one operand is NUMERIC.
/// Other-side integers / floats are promoted to a NUMERIC at a common
/// scale; all add / sub / mul / div / compare paths stay in i128.
#[allow(clippy::needless_pass_by_value)] // mirrors `apply_binary`'s by-value calling convention
fn apply_binary_numeric(
    op: BinOp,
    l: Value<'static>,
    r: Value<'static>,
) -> Result<Value<'static>, EvalError> {
    // Float still wins — Numeric + Float coerces both to f64 and runs
    // through the float path. PG demotes Numeric to float in this mix
    // too (the documented behaviour for `numeric + double precision`).
    let float_path = matches!(l, Value::Float(_)) || matches!(r, Value::Float(_));
    if float_path {
        let af = as_f64(&l)?;
        let bf = as_f64(&r)?;
        return match op {
            BinOp::Add => Ok(Value::Float(af + bf)),
            BinOp::Sub => Ok(Value::Float(af - bf)),
            BinOp::Mul => Ok(Value::Float(af * bf)),
            BinOp::Div => {
                if bf == 0.0 {
                    Err(EvalError::DivisionByZero)
                } else {
                    Ok(Value::Float(af / bf))
                }
            }
            BinOp::Mod => {
                if bf == 0.0 {
                    Err(EvalError::DivisionByZero)
                } else {
                    Ok(Value::Float(af % bf))
                }
            }
            BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => {
                let ord = af.partial_cmp(&bf).ok_or(EvalError::TypeMismatch {
                    detail: "NaN in NUMERIC/Float comparison".into(),
                })?;
                Ok(Value::Bool(cmp_to_bool(op, ord)))
            }
            BinOp::Concat => Ok(text_concat(&l, &r)),
            other => Err(EvalError::TypeMismatch {
                detail: format!("operator {other:?} not defined for NUMERIC and Float"),
            }),
        };
    }
    // Promote integer ↔ numeric to a shared scale (max of both sides).
    let (a, sa) = numeric_or_widen(&l).ok_or_else(|| EvalError::TypeMismatch {
        detail: format!("NUMERIC op against non-numeric {:?}", l.data_type()),
    })?;
    let (b, sb) = numeric_or_widen(&r).ok_or_else(|| EvalError::TypeMismatch {
        detail: format!("NUMERIC op against non-numeric {:?}", r.data_type()),
    })?;
    match op {
        BinOp::Add | BinOp::Sub => {
            let target_scale = sa.max(sb);
            let lhs = rescale(a, sa, target_scale).ok_or(EvalError::TypeMismatch {
                detail: "NUMERIC overflow on rescale".into(),
            })?;
            let rhs = rescale(b, sb, target_scale).ok_or(EvalError::TypeMismatch {
                detail: "NUMERIC overflow on rescale".into(),
            })?;
            let r = match op {
                BinOp::Add => lhs.checked_add(rhs),
                BinOp::Sub => lhs.checked_sub(rhs),
                _ => unreachable!(),
            }
            .ok_or(EvalError::TypeMismatch {
                detail: "NUMERIC overflow on +/-".into(),
            })?;
            Ok(Value::Numeric {
                scaled: r,
                scale: target_scale,
            })
        }
        BinOp::Mul => {
            let scaled = a.checked_mul(b).ok_or(EvalError::TypeMismatch {
                detail: "NUMERIC overflow on *".into(),
            })?;
            Ok(Value::Numeric {
                scaled,
                scale: sa.saturating_add(sb),
            })
        }
        BinOp::Div => {
            if b == 0 {
                return Err(EvalError::DivisionByZero);
            }
            // Result scale: keep the wider operand's scale. Pre-scale
            // the numerator so the integer division retains that many
            // fractional digits. Round half-away-from-zero.
            let target_scale = sa.max(sb);
            // Numerator effective scale becomes sa + target_scale; we
            // bring it up to (target_scale + sb) so the divisor's scale
            // cancels cleanly.
            let bump = pow10_i128(target_scale.saturating_add(sb).saturating_sub(sa));
            let num = a.checked_mul(bump).ok_or(EvalError::TypeMismatch {
                detail: "NUMERIC overflow on / scaling".into(),
            })?;
            let half = if b >= 0 { b / 2 } else { -(b / 2) };
            let adj = if (num >= 0) == (b >= 0) {
                num + half
            } else {
                num - half
            };
            Ok(Value::Numeric {
                scaled: adj / b,
                scale: target_scale,
            })
        }
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => {
            let target_scale = sa.max(sb);
            let lhs = rescale(a, sa, target_scale).ok_or(EvalError::TypeMismatch {
                detail: "NUMERIC overflow on rescale".into(),
            })?;
            let rhs = rescale(b, sb, target_scale).ok_or(EvalError::TypeMismatch {
                detail: "NUMERIC overflow on rescale".into(),
            })?;
            Ok(Value::Bool(cmp_to_bool(op, lhs.cmp(&rhs))))
        }
        BinOp::Concat => Ok(text_concat(&l, &r)),
        other => Err(EvalError::TypeMismatch {
            detail: format!("operator {other:?} not defined for NUMERIC"),
        }),
    }
}

/// Express `v` as a `(scaled_i128, scale)` pair. Plain integers come
/// back with `scale=0`; NUMERIC keeps its own scale. Anything else
/// returns `None` and the caller raises a type error.
fn numeric_or_widen(v: &Value<'static>) -> Option<(i128, u8)> {
    match v {
        Value::Numeric { scaled, scale } => Some((*scaled, *scale)),
        Value::Int(n) => Some((i128::from(*n), 0)),
        Value::SmallInt(n) => Some((i128::from(*n), 0)),
        Value::BigInt(n) => Some((i128::from(*n), 0)),
        _ => None,
    }
}

fn rescale(scaled: i128, src: u8, dst: u8) -> Option<i128> {
    if src == dst {
        return Some(scaled);
    }
    if dst > src {
        scaled.checked_mul(pow10_i128(dst - src))
    } else {
        let drop = pow10_i128(src - dst);
        let half = drop / 2;
        let r = if scaled >= 0 {
            scaled + half
        } else {
            scaled - half
        };
        Some(r / drop)
    }
}

pub(super) const fn pow10_i128(p: u8) -> i128 {
    let mut acc: i128 = 1;
    let mut i = 0;
    while i < p {
        acc *= 10;
        i += 1;
    }
    acc
}

const fn cmp_to_bool(op: BinOp, ord: core::cmp::Ordering) -> bool {
    use core::cmp::Ordering::{Equal, Greater, Less};
    match op {
        BinOp::Eq => matches!(ord, Equal),
        BinOp::NotEq => !matches!(ord, Equal),
        BinOp::Lt => matches!(ord, Less),
        BinOp::LtEq => matches!(ord, Less | Equal),
        BinOp::Gt => matches!(ord, Greater),
        BinOp::GtEq => matches!(ord, Greater | Equal),
        _ => false,
    }
}

/// SQL `||` string concatenation. Operands are coerced to text via the same
/// rule as `::text` cast. NULL propagates (handled above; this function only
/// runs with non-NULL operands).
/// v7.24 (round-16 C) — `tsvector || tsvector`. PG semantics: the
/// right side's positions shift by the left side's max position;
/// lexemes present on both sides merge (positions concatenated,
/// the higher weight wins — SPG models weight per lexeme, PG per
/// position, so the stronger label is the faithful collapse).
fn text_concat(l: &Value<'static>, r: &Value<'static>) -> Value<'static> {
    if let (Value::TsVector(a), Value::TsVector(b)) = (l, r) {
        return tsvector_concat(a, b);
    }
    // v7.11.8 — PG `||` overloads: TEXT[] || TEXT[] = concatenated array;
    // TEXT[] || TEXT (or TEXT || TEXT[]) prepends/appends the single
    // element. NULL || anything = NULL (PG semantics for arrays;
    // text concat treats NULL the same way after value_to_text).
    match (l, r) {
        (Value::Null, _) | (_, Value::Null) => {
            // PG text concat: NULL || x = NULL. Array concat: NULL || x = NULL.
            // Keep the legacy text path (value_to_text handles Null as ""),
            // but for arrays we surface real NULL to match PG.
            if matches!(
                l,
                Value::TextArray(_) | Value::IntArray(_) | Value::BigIntArray(_) | Value::Bytes(_)
            ) || matches!(
                r,
                Value::TextArray(_) | Value::IntArray(_) | Value::BigIntArray(_) | Value::Bytes(_)
            ) {
                return Value::Null;
            }
        }
        (Value::TextArray(a), Value::TextArray(b)) => {
            let mut out = a.clone();
            out.extend(b.iter().cloned());
            return Value::TextArray(out);
        }
        (Value::TextArray(a), Value::Text(s)) => {
            let mut out = a.clone();
            out.push(Some(s.to_string()));
            return Value::TextArray(out);
        }
        (Value::Text(s), Value::TextArray(b)) => {
            let mut out: alloc::vec::Vec<Option<alloc::string::String>> =
                alloc::vec::Vec::with_capacity(1 + b.len());
            out.push(Some(s.to_string()));
            out.extend(b.iter().cloned());
            return Value::TextArray(out);
        }
        // v7.11.13 — IntArray / BigIntArray `||` overloads. Same
        // PG semantics as TEXT[]: array||array concatenates, and
        // array||scalar appends/prepends. Mixed Int/BigInt widens
        // to BigIntArray.
        (Value::IntArray(a), Value::IntArray(b)) => {
            let mut out = a.clone();
            out.extend(b.iter().copied());
            return Value::IntArray(out);
        }
        (Value::IntArray(a), Value::Int(n)) => {
            let mut out = a.clone();
            out.push(Some(*n));
            return Value::IntArray(out);
        }
        (Value::IntArray(a), Value::SmallInt(n)) => {
            let mut out = a.clone();
            out.push(Some(i32::from(*n)));
            return Value::IntArray(out);
        }
        (Value::Int(n), Value::IntArray(b)) => {
            let mut out: alloc::vec::Vec<Option<i32>> = alloc::vec::Vec::with_capacity(1 + b.len());
            out.push(Some(*n));
            out.extend(b.iter().copied());
            return Value::IntArray(out);
        }
        (Value::SmallInt(n), Value::IntArray(b)) => {
            let mut out: alloc::vec::Vec<Option<i32>> = alloc::vec::Vec::with_capacity(1 + b.len());
            out.push(Some(i32::from(*n)));
            out.extend(b.iter().copied());
            return Value::IntArray(out);
        }
        (Value::BigIntArray(a), Value::BigIntArray(b)) => {
            let mut out = a.clone();
            out.extend(b.iter().copied());
            return Value::BigIntArray(out);
        }
        (Value::BigIntArray(a), Value::IntArray(b)) => {
            let mut out = a.clone();
            out.extend(b.iter().map(|o| o.map(i64::from)));
            return Value::BigIntArray(out);
        }
        (Value::IntArray(a), Value::BigIntArray(b)) => {
            let mut out: alloc::vec::Vec<Option<i64>> =
                a.iter().map(|o| o.map(i64::from)).collect();
            out.extend(b.iter().copied());
            return Value::BigIntArray(out);
        }
        (Value::BigIntArray(a), Value::BigInt(n)) => {
            let mut out = a.clone();
            out.push(Some(*n));
            return Value::BigIntArray(out);
        }
        (Value::BigIntArray(a), Value::Int(n)) => {
            let mut out = a.clone();
            out.push(Some(i64::from(*n)));
            return Value::BigIntArray(out);
        }
        (Value::BigIntArray(a), Value::SmallInt(n)) => {
            let mut out = a.clone();
            out.push(Some(i64::from(*n)));
            return Value::BigIntArray(out);
        }
        (Value::BigInt(n), Value::BigIntArray(b)) => {
            let mut out: alloc::vec::Vec<Option<i64>> = alloc::vec::Vec::with_capacity(1 + b.len());
            out.push(Some(*n));
            out.extend(b.iter().copied());
            return Value::BigIntArray(out);
        }
        (Value::Int(n), Value::BigIntArray(b)) => {
            let mut out: alloc::vec::Vec<Option<i64>> = alloc::vec::Vec::with_capacity(1 + b.len());
            out.push(Some(i64::from(*n)));
            out.extend(b.iter().copied());
            return Value::BigIntArray(out);
        }
        (Value::SmallInt(n), Value::BigIntArray(b)) => {
            let mut out: alloc::vec::Vec<Option<i64>> = alloc::vec::Vec::with_capacity(1 + b.len());
            out.push(Some(i64::from(*n)));
            out.extend(b.iter().copied());
            return Value::BigIntArray(out);
        }
        // v7.11.15 — BYTEA `||` is byte concatenation.
        (Value::Bytes(a), Value::Bytes(b)) => {
            let mut out = a.clone().into_owned();
            out.extend_from_slice(b);
            return Value::bytes(out);
        }
        _ => {}
    }
    let a = value_to_text(l);
    let b = value_to_text(r);
    Value::text(a + &b)
}

/// pgvector inner-product `<#>`. Returns the *negative* dot product so
/// smaller still means more similar — same convention as pgvector.
fn inner_product(l: Value<'static>, r: Value<'static>) -> Result<Value<'static>, EvalError> {
    let (a, b) = unwrap_vec_pair(l, r, "<#>")?;
    let mut dot: f64 = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += f64::from(*x) * f64::from(*y);
    }
    Ok(Value::Float(-dot))
}

/// pgvector cosine distance `<=>` — `1 - (a·b) / (‖a‖ ‖b‖)`. A zero-norm
/// operand produces NaN (matches pgvector).
fn cosine_distance(l: Value<'static>, r: Value<'static>) -> Result<Value<'static>, EvalError> {
    let (a, b) = unwrap_vec_pair(l, r, "<=>")?;
    let mut dot: f64 = 0.0;
    let mut na: f64 = 0.0;
    let mut nb: f64 = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        let xf = f64::from(*x);
        let yf = f64::from(*y);
        dot += xf * yf;
        na += xf * xf;
        nb += yf * yf;
    }
    let denom = sqrt_newton(na) * sqrt_newton(nb);
    if denom == 0.0 {
        return Ok(Value::Float(f64::NAN));
    }
    Ok(Value::Float(1.0 - dot / denom))
}

fn unwrap_vec_pair(
    l: Value<'static>,
    r: Value<'static>,
    op: &str,
) -> Result<(Vec<f32>, Vec<f32>), EvalError> {
    // v6.0.1: SQ8 cells coming through the SQL evaluator are
    // dequantised to f32 here so the existing scalar distance
    // arithmetic stays intact. HNSW kNN search continues to use
    // the asymmetric ADC variant inside `cell_to_query_metric_
    // distance` — this path only runs when a vector expression
    // lands in the evaluator (full-scan ORDER BY, SELECT
    // projection of `v <-> $1`, etc.).
    let to_f32 = |v: Value| -> Option<Vec<f32>> {
        match v {
            Value::Vector(a) => Some(a.into_owned()),
            Value::Sq8Vector(q) => Some(spg_storage::quantize::dequantize(&q)),
            // v6.0.3: bit-exact dequant for halfvec cells.
            Value::HalfVector(h) => Some(h.to_f32_vec()),
            _ => None,
        }
    };
    let l_ty = l.data_type();
    let r_ty = r.data_type();
    match (to_f32(l), to_f32(r)) {
        (Some(a), Some(b)) => {
            if a.len() != b.len() {
                return Err(EvalError::TypeMismatch {
                    detail: format!("vector dim mismatch in {op}: {} vs {}", a.len(), b.len()),
                });
            }
            Ok((a, b))
        }
        _ => Err(EvalError::TypeMismatch {
            detail: format!("{op} requires two vectors, got {l_ty:?} and {r_ty:?}"),
        }),
    }
}

/// Numeric arithmetic with widening.
/// - both `Int` → `Int` (with overflow check)
/// - `Int` op `BigInt` (either side) → `BigInt`
/// - any `Float` involved → `Float`
/// Bitwise integer op (`|` / `&`). PG defines these for integer
/// types only — SmallInt widens to Int, Int x BigInt widens to
/// BigInt, anything else is a type error (mailrs embed round-12).
fn bitop(
    l: Value<'static>,
    r: Value<'static>,
    f: impl Fn(i64, i64) -> i64,
    op_name: &str,
) -> Result<Value<'static>, EvalError> {
    let widen = |v: Value<'static>| -> Value<'static> {
        match v {
            Value::SmallInt(n) => Value::Int(i32::from(n)),
            other => other,
        }
    };
    match (widen(l), widen(r)) {
        (Value::Int(a), Value::Int(b)) => {
            let result = f(i64::from(a), i64::from(b));
            // Two i32 inputs can't overflow i32 under | / &.
            Ok(Value::Int(result as i32))
        }
        (Value::Int(a), Value::BigInt(b)) | (Value::BigInt(b), Value::Int(a)) => {
            Ok(Value::BigInt(f(i64::from(a), b)))
        }
        (Value::BigInt(a), Value::BigInt(b)) => Ok(Value::BigInt(f(a, b))),
        (a, b) => Err(EvalError::TypeMismatch {
            detail: format!("cannot apply {op_name} to {a:?} and {b:?}"),
        }),
    }
}

/// The i64 value of an integer-family operand (SmallInt/Int/BigInt),
/// or `None` for anything else — used to claim bit-shift ops for
/// integers before the inet interpretation.
fn int_operand(v: &Value<'_>) -> Option<i64> {
    match v {
        Value::SmallInt(n) => Some(i64::from(*n)),
        Value::Int(n) => Some(i64::from(*n)),
        Value::BigInt(n) => Some(*n),
        _ => None,
    }
}

/// Elements of a scalar array Value normalised for equality
/// (integers widen to BigInt so int4[] and int8[] compare), plus a
/// flag for NULL elements. `None` for non-array operands.
fn array_scalar_elems(v: &Value<'_>) -> Option<(Vec<Value<'static>>, bool)> {
    fn collect<T: Clone>(
        items: &[Option<T>],
        f: impl Fn(T) -> Value<'static>,
    ) -> (Vec<Value<'static>>, bool) {
        let mut has_null = false;
        let mut out = Vec::with_capacity(items.len());
        for o in items {
            match o {
                Some(x) => out.push(f(x.clone())),
                None => has_null = true,
            }
        }
        (out, has_null)
    }
    Some(match v {
        Value::TextArray(i) => collect(i, Value::text),
        Value::IntArray(i) => collect(i, |x| Value::BigInt(i64::from(x))),
        Value::SmallIntArray(i) => collect(i, |x| Value::BigInt(i64::from(x))),
        Value::BigIntArray(i) => collect(i, Value::BigInt),
        Value::FloatArray(i) => collect(i, Value::Float),
        Value::BoolArray(i) => collect(i, Value::Bool),
        Value::DateArray(i) => collect(i, Value::Date),
        Value::TimestampArray(i) => collect(i, Value::Timestamp),
        _ => return None,
    })
}

/// `l @> r` for scalar arrays: every non-NULL element of `r` appears
/// in `l`; a NULL element on the right can never be contained (PG
/// semantics), and the empty array is contained in everything.
fn array_contains_all(l: &Value<'_>, r: &Value<'_>) -> bool {
    let (left, _) = array_scalar_elems(l).expect("caller guards array");
    let (right, right_nulls) = array_scalar_elems(r).expect("caller guards array");
    if right_nulls {
        return false;
    }
    right.iter().all(|b| left.contains(b))
}

fn arith(
    l: Value<'static>,
    r: Value<'static>,
    int_op: impl Fn(i64, i64) -> Option<i64>,
    float_op: impl Fn(f64, f64) -> f64,
    op_name: &str,
) -> Result<Value<'static>, EvalError> {
    // Widen SmallInt to Int up front so the rest of the arithmetic
    // table only deals with Int / BigInt / Float pairs.
    let widen = |v: Value<'static>| -> Value<'static> {
        match v {
            Value::SmallInt(n) => Value::Int(i32::from(n)),
            other => other,
        }
    };
    let l = widen(l);
    let r = widen(r);
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => {
            let result = int_op(i64::from(a), i64::from(b)).ok_or(EvalError::TypeMismatch {
                detail: format!("integer overflow on {op_name}"),
            })?;
            if let Ok(small) = i32::try_from(result) {
                Ok(Value::Int(small))
            } else {
                Ok(Value::BigInt(result))
            }
        }
        (Value::Int(a), Value::BigInt(b)) | (Value::BigInt(b), Value::Int(a)) => {
            let result = int_op(i64::from(a), b).ok_or(EvalError::TypeMismatch {
                detail: format!("bigint overflow on {op_name}"),
            })?;
            Ok(Value::BigInt(result))
        }
        (Value::BigInt(a), Value::BigInt(b)) => {
            let result = int_op(a, b).ok_or(EvalError::TypeMismatch {
                detail: format!("bigint overflow on {op_name}"),
            })?;
            Ok(Value::BigInt(result))
        }
        (a, b)
            if a.data_type() == Some(DataType::Float) || b.data_type() == Some(DataType::Float) =>
        {
            let af = as_f64(&a)?;
            let bf = as_f64(&b)?;
            Ok(Value::Float(float_op(af, bf)))
        }
        (a, b) => Err(EvalError::TypeMismatch {
            detail: format!(
                "{op_name} applied to non-numeric: {:?} vs {:?}",
                a.data_type(),
                b.data_type()
            ),
        }),
    }
}

/// L2 (Euclidean) distance between two vectors of equal dimension.
/// Returned as `Value::Float(d)` so it composes with the existing
/// comparison / sort plumbing. Mismatched dims or non-vector operands
/// raise `TypeMismatch`.
#[allow(clippy::many_single_char_names)] // l, r, a, b, d are the natural names
fn l2_distance(l: Value<'static>, r: Value<'static>) -> Result<Value<'static>, EvalError> {
    // v6.0.1: route both operands through `unwrap_vec_pair` so SQ8
    // cells dequantise on the way in. Sub-f64 precision loss is
    // negligible vs the dequantisation noise the SQ8 path already
    // ships with.
    let (a, b) = unwrap_vec_pair(l, r, "<->")?;
    let mut sum: f64 = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        let d = f64::from(*x) - f64::from(*y);
        sum += d * d;
    }
    Ok(Value::Float(sqrt_newton(sum)))
}

/// Self-built `sqrt` for `f64` — `std::f64::sqrt` lives in `std`, which the
/// engine's `no_std` constraint disallows. Newton-Raphson with a few rounds
/// reaches IEEE-754 precision for the inputs we'll see (sum of squares of
/// f32-derived distances, always non-negative, never NaN).
fn sqrt_newton(x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    let mut g = x;
    // 10 iterations is conservative; 6 already converges to ulp for typical
    // distances.
    for _ in 0..10 {
        g = 0.5 * (g + x / g);
    }
    g
}

/// v7.37.7 C.1.7 — PG integer modulo. Float operands fall back to
/// `f64::rem_euclid` (PG `%` on floats is C `fmod` semantics; the
/// `rem_euclid` choice keeps the sign of the divisor for the
/// uncommon negative-divisor case — matches PG 14+ behaviour).
/// Division by zero surfaces as `DivisionByZero` so callers see the
/// same error variant for `/` and `%`.
fn mod_op(l: Value<'static>, r: Value<'static>) -> Result<Value<'static>, EvalError> {
    let any_float = matches!(l.data_type(), Some(DataType::Float))
        || matches!(r.data_type(), Some(DataType::Float));
    if any_float {
        let a = as_f64(&l)?;
        let b = as_f64(&r)?;
        if b == 0.0 {
            return Err(EvalError::DivisionByZero);
        }
        return Ok(Value::Float(a % b));
    }
    // `arith()` is integer-only when the float fast path above didn't
    // match; both closures take `i64`. `rem_euclid` is well-defined
    // for i64 division (panics only on divide-by-zero, which the gate
    // above prevents). We DO NOT need an `rem_euclid` for f64 — that
    // path returned earlier with `%` (C-style fmod), matching PG.
    arith(
        l,
        r,
        |a, b| {
            if b == 0 { None } else { Some(a.rem_euclid(b)) }
        },
        // f64 fallback (when widening to Float happens inside arith);
        // f64's `%` is C `fmod` semantics, matching PG `%` on floats.
        |a, b| if b == 0.0 { 0.0 } else { a % b },
        "%",
    )
    .map_err(|e| match e {
        EvalError::TypeMismatch { detail } if detail.contains('%') => EvalError::DivisionByZero,
        other => other,
    })
}

fn div_op(l: Value<'static>, r: Value<'static>) -> Result<Value<'static>, EvalError> {
    let any_float = matches!(l.data_type(), Some(DataType::Float))
        || matches!(r.data_type(), Some(DataType::Float));
    if any_float {
        let a = as_f64(&l)?;
        let b = as_f64(&r)?;
        if b == 0.0 {
            return Err(EvalError::DivisionByZero);
        }
        return Ok(Value::Float(a / b));
    }
    arith(
        l,
        r,
        |a, b| {
            if b == 0 { None } else { Some(a / b) }
        },
        |a, b| a / b,
        "/",
    )
    .map_err(|e| match e {
        // The closure returns None on b == 0; translate that into the dedicated
        // DivisionByZero variant instead of "integer overflow on /".
        EvalError::TypeMismatch { detail } if detail.contains('/') => EvalError::DivisionByZero,
        other => other,
    })
}

fn as_f64(v: &Value<'_>) -> Result<f64, EvalError> {
    match v {
        Value::SmallInt(n) => Ok(f64::from(*n)),
        Value::Int(n) => Ok(f64::from(*n)),
        #[allow(clippy::cast_precision_loss)]
        Value::BigInt(n) => Ok(*n as f64),
        Value::Float(x) => Ok(*x),
        #[allow(clippy::cast_precision_loss)]
        Value::Numeric { scaled, scale } => {
            let mut div = 1.0_f64;
            for _ in 0..*scale {
                div *= 10.0;
            }
            Ok((*scaled as f64) / div)
        }
        other => Err(EvalError::TypeMismatch {
            detail: format!("cannot convert {:?} to FLOAT", other.data_type()),
        }),
    }
}

pub(super) fn compare(
    op: BinOp,
    l: &Value<'_>,
    r: &Value<'_>,
) -> Result<Value<'static>, EvalError> {
    let ord = match (l, r) {
        (Value::Int(a), Value::Int(b)) => i64::from(*a).cmp(&i64::from(*b)),
        (Value::Int(a), Value::BigInt(b)) => i64::from(*a).cmp(b),
        (Value::BigInt(a), Value::Int(b)) => a.cmp(&i64::from(*b)),
        (Value::BigInt(a), Value::BigInt(b)) => a.cmp(b),
        (a, b)
            if matches!(a.data_type(), Some(DataType::Float))
                || matches!(b.data_type(), Some(DataType::Float)) =>
        {
            let af = as_f64(a)?;
            let bf = as_f64(b)?;
            af.partial_cmp(&bf).ok_or(EvalError::TypeMismatch {
                detail: "NaN in comparison".into(),
            })?
        }
        (Value::Text(a), Value::Text(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        // Date / Timestamp compare on their integer storage repr.
        // Cross-domain (Date vs Timestamp) lifts the Date to the
        // matching midnight TIMESTAMP first.
        (Value::Date(a), Value::Date(b)) => a.cmp(b),
        (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
        (Value::Date(a), Value::Timestamp(b)) => (i64::from(*a) * 86_400_000_000).cmp(b),
        (Value::Timestamp(a), Value::Date(b)) => a.cmp(&(i64::from(*b) * 86_400_000_000)),
        // INTERVAL compares by PG's canonical microsecond span:
        // months count as 30 days and days as 24 hours, so
        // `INTERVAL '1 month' = INTERVAL '30 days'` and
        // `INTERVAL '1 day' = INTERVAL '24 hours'` (i128 keeps the
        // months*30*86_400e6 product from overflowing i64).
        (
            Value::Interval {
                months: am,
                days: ad,
                micros: au,
            },
            Value::Interval {
                months: bm,
                days: bd,
                micros: bu,
            },
        ) => {
            let span = |m: i32, d: i32, u: i64| -> i128 {
                (i128::from(m) * 30 + i128::from(d)) * 86_400_000_000 + i128::from(u)
            };
            span(*am, *ad, *au).cmp(&span(*bm, *bd, *bu))
        }
        // PG-style implicit coercion: comparing a DATE / TIMESTAMP
        // column against a text literal lifts the literal into the
        // matching domain (e.g. `day >= '2024-01-01'`).
        (Value::Date(a), Value::Text(b)) => {
            let bd = parse_date_literal(b).ok_or_else(|| EvalError::TypeMismatch {
                detail: format!("cannot parse {b:?} as DATE for comparison"),
            })?;
            a.cmp(&bd)
        }
        (Value::Text(a), Value::Date(b)) => {
            let ad = parse_date_literal(a).ok_or_else(|| EvalError::TypeMismatch {
                detail: format!("cannot parse {a:?} as DATE for comparison"),
            })?;
            ad.cmp(b)
        }
        (Value::Timestamp(a), Value::Text(b)) => {
            let bt = parse_timestamp_literal(b).ok_or_else(|| EvalError::TypeMismatch {
                detail: format!("cannot parse {b:?} as TIMESTAMP for comparison"),
            })?;
            a.cmp(&bt)
        }
        (Value::Text(a), Value::Timestamp(b)) => {
            let at = parse_timestamp_literal(a).ok_or_else(|| EvalError::TypeMismatch {
                detail: format!("cannot parse {a:?} as TIMESTAMP for comparison"),
            })?;
            at.cmp(b)
        }
        // v7.17.0 — UUID byte-wise comparison; both sides UUID.
        (Value::Uuid(a), Value::Uuid(b)) => a.cmp(b),
        // v7.17.0 — PG promotes a `text` literal compared against a
        // `uuid` column into uuid (unknown-type literal inference).
        // Without this, `WHERE id = '550e...'` falls through to the
        // generic TypeMismatch — the application's literal becomes
        // an error rather than a comparison.
        (Value::Uuid(a), Value::Text(b)) => {
            let bu = spg_storage::parse_uuid_str(b).ok_or_else(|| EvalError::TypeMismatch {
                detail: format!("invalid input syntax for type uuid: {b:?}"),
            })?;
            a.cmp(&bu)
        }
        (Value::Text(a), Value::Uuid(b)) => {
            let au = spg_storage::parse_uuid_str(a).ok_or_else(|| EvalError::TypeMismatch {
                detail: format!("invalid input syntax for type uuid: {a:?}"),
            })?;
            au.cmp(b)
        }
        (a, b) => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "comparison between {:?} and {:?}",
                    a.data_type(),
                    b.data_type()
                ),
            });
        }
    };
    let result = match op {
        BinOp::Eq => ord.is_eq(),
        BinOp::NotEq => !ord.is_eq(),
        BinOp::Lt => ord.is_lt(),
        BinOp::LtEq => ord.is_le(),
        BinOp::Gt => ord.is_gt(),
        BinOp::GtEq => ord.is_ge(),
        BinOp::And
        | BinOp::Or
        | BinOp::BitOr
        | BinOp::BitAnd
        | BinOp::Add
        | BinOp::Sub
        | BinOp::Mul
        | BinOp::Div
        | BinOp::Mod
        | BinOp::L2Distance
        | BinOp::InnerProduct
        | BinOp::CosineDistance
        | BinOp::Concat
        | BinOp::JsonGet
        | BinOp::JsonGetText
        | BinOp::JsonGetPath
        | BinOp::JsonGetPathText
        | BinOp::JsonContains
        | BinOp::JsonContainedBy
        | BinOp::JsonKeyExists
        | BinOp::JsonKeysAny
        | BinOp::JsonKeysAll
        | BinOp::JsonDeletePath
        | BinOp::TsMatch
        | BinOp::IsDistinctFrom
        | BinOp::IsNotDistinctFrom
        | BinOp::InetContainedBy
        | BinOp::InetContainedByEq
        | BinOp::InetContains
        | BinOp::InetContainsEq
        | BinOp::InetOverlap => {
            unreachable!("compare() only called with comparison ops")
        }
    };
    Ok(Value::Bool(result))
}

// SQL three-valued AND / OR.
pub(crate) fn and_3vl(l: Value<'static>, r: Value<'static>) -> Result<Value<'static>, EvalError> {
    match (l, r) {
        (Value::Bool(false), _) | (_, Value::Bool(false)) => Ok(Value::Bool(false)),
        (Value::Bool(true), Value::Bool(true)) => Ok(Value::Bool(true)),
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (a, b) => Err(EvalError::TypeMismatch {
            detail: format!(
                "AND on non-boolean: {:?} and {:?}",
                a.data_type(),
                b.data_type()
            ),
        }),
    }
}

fn or_3vl(l: Value<'static>, r: Value<'static>) -> Result<Value<'static>, EvalError> {
    match (l, r) {
        (Value::Bool(true), _) | (_, Value::Bool(true)) => Ok(Value::Bool(true)),
        (Value::Bool(false), Value::Bool(false)) => Ok(Value::Bool(false)),
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (a, b) => Err(EvalError::TypeMismatch {
            detail: format!(
                "OR on non-boolean: {:?} and {:?}",
                a.data_type(),
                b.data_type()
            ),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_add_to_timestamp_micros_part() {
        // 2024-01-01 00:00:00 + INTERVAL '1 hour' = 2024-01-01 01:00:00
        let ts = i64::from(days_from_civil(2024, 1, 1)) * 86_400_000_000;
        let r = add_interval_to_micros(ts, 0, 0, 3_600_000_000).unwrap();
        let expected = ts + 3_600_000_000;
        assert_eq!(r, expected);
    }

    /// v7.37.5 β — days dimension threads cleanly through
    /// `add_interval_to_micros`. INTERVAL '1 day' adds exactly one
    /// 86_400_000_000-µs chunk, independent of the months path.
    #[test]
    fn interval_add_to_timestamp_days_part() {
        let ts = i64::from(days_from_civil(2024, 1, 1)) * 86_400_000_000;
        let r = add_interval_to_micros(ts, 0, 1, 0).unwrap();
        let expected = i64::from(days_from_civil(2024, 1, 2)) * 86_400_000_000;
        assert_eq!(r, expected);
    }

    #[test]
    fn interval_clamp_month_end() {
        // 2024-01-31 + 1 month = 2024-02-29 (leap year).
        let d = days_from_civil(2024, 1, 31);
        let shifted = shift_date_by_months(d, 1).unwrap();
        let (y, m, day) = civil_from_days(shifted);
        assert_eq!((y, m, day), (2024, 2, 29));
        // 2023-01-31 + 1 month = 2023-02-28 (non-leap).
        let d = days_from_civil(2023, 1, 31);
        let shifted = shift_date_by_months(d, 1).unwrap();
        let (y, m, day) = civil_from_days(shifted);
        assert_eq!((y, m, day), (2023, 2, 28));
        // 2024-03-31 - 1 month = 2024-02-29.
        let d = days_from_civil(2024, 3, 31);
        let shifted = shift_date_by_months(d, -1).unwrap();
        let (y, m, day) = civil_from_days(shifted);
        assert_eq!((y, m, day), (2024, 2, 29));
    }

    #[test]
    fn interval_date_plus_pure_days_stays_date() {
        // DATE + INTERVAL '7 days' must stay DATE.
        let d = days_from_civil(2024, 6, 1);
        let lhs = Value::Date(d);
        let rhs = Value::Interval {
            months: 0,
            days: 7,
            micros: 0,
        };
        let v = apply_binary_interval(BinOp::Add, &lhs, &rhs)
            .unwrap()
            .unwrap();
        let expected = days_from_civil(2024, 6, 8);
        assert_eq!(v, Value::Date(expected));
    }

    #[test]
    fn interval_date_plus_sub_day_lifts_to_timestamp() {
        // DATE + INTERVAL '1 hour' must lift to TIMESTAMP.
        let d = days_from_civil(2024, 6, 1);
        let lhs = Value::Date(d);
        let rhs = Value::Interval {
            months: 0,
            days: 0,
            micros: 3_600_000_000,
        };
        let v = apply_binary_interval(BinOp::Add, &lhs, &rhs)
            .unwrap()
            .unwrap();
        let expected = i64::from(d) * 86_400_000_000 + 3_600_000_000;
        assert_eq!(v, Value::Timestamp(expected));
    }
}
