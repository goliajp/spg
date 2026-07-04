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
        (UnOp::BitNot, Value::BitString { nbits, bytes }) => {
            // PG `~ bit(n)`: flip every bit, then re-zero the padding bits
            // past `nbits` so the value stays canonical (MSB-packed).
            let mut out: Vec<u8> = bytes.iter().map(|b| !b).collect();
            let used = nbits as usize;
            let full = used / 8;
            let rem = used % 8;
            if rem != 0 {
                if let Some(byte) = out.get_mut(full) {
                    *byte &= 0xffu8 << (8 - rem);
                }
            }
            Ok(Value::BitString {
                nbits,
                bytes: alloc::borrow::Cow::Owned(out),
            })
        }
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
    // `inet - inet` -> bigint: the count of addresses between them. Same
    // family required, matching PG.
    if op == BinOp::Sub {
        if let (
            Value::Inet { family: fa, addr: aa, .. },
            Value::Inet { family: fb, addr: ab, .. },
        ) = (&l, &r)
        {
            if fa != fb {
                return Err(EvalError::TypeMismatch {
                    detail: "cannot subtract addresses of different families".into(),
                });
            }
            let diff = inet_addr_u128(*fa, aa) as i128 - inet_addr_u128(*fb, ab) as i128;
            return i64::try_from(diff).map(Value::BigInt).map_err(|_| EvalError::TypeMismatch {
                detail: "result out of range".into(),
            });
        }
    }
    // PG `point ± point` translates a point by another's coordinates;
    // `point * point` / `point / point` are complex-number multiply/divide
    // (PG treats a point as the complex number x + yi).
    if let (Value::Point(a), Value::Point(b)) = (&l, &r) {
        match op {
            BinOp::Add => {
                return Ok(Value::Point(spg_storage::Point2D { x: a.x + b.x, y: a.y + b.y }));
            }
            BinOp::Sub => {
                return Ok(Value::Point(spg_storage::Point2D { x: a.x - b.x, y: a.y - b.y }));
            }
            BinOp::Mul => {
                // (a.x + a.y i)(b.x + b.y i)
                return Ok(Value::Point(spg_storage::Point2D {
                    x: a.x * b.x - a.y * b.y,
                    y: a.x * b.y + a.y * b.x,
                }));
            }
            BinOp::Div => {
                let denom = b.x * b.x + b.y * b.y;
                return Ok(Value::Point(spg_storage::Point2D {
                    x: (a.x * b.x + a.y * b.y) / denom,
                    y: (a.y * b.x - a.x * b.y) / denom,
                }));
            }
            _ => {}
        }
    }
    // MONEY arithmetic (integer cents) before the generic numeric path.
    if let Some(result) = money_arith(op, &l, &r) {
        return result;
    }
    // Range `*` intersection / `+` union / `-` difference claim Mul / Add / Sub
    // before numeric/date arithmetic.
    if op == BinOp::Mul || op == BinOp::Add || op == BinOp::Sub {
        if let (
            Value::Range { kind: ak, lower: al, upper: au, lower_inc: ali, upper_inc: aui, empty: ae },
            Value::Range { kind: bk, lower: bl, upper: bu, lower_inc: bli, upper_inc: bui, empty: be },
        ) = (&l, &r)
        {
            return match op {
                BinOp::Mul => Ok(range_intersect(
                    *ak, al, au, *ali, *aui, *ae, *bk, bl, bu, *bli, *bui, *be,
                )),
                BinOp::Add => range_union(
                    *ak, al, au, *ali, *aui, *ae, *bk, bl, bu, *bli, *bui, *be,
                ),
                _ => range_difference(
                    *ak, al, au, *ali, *aui, *ae, *bk, bl, bu, *bli, *bui, *be,
                ),
            };
        }
    }
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
        // tsquery `||` is boolean OR (claim it before text concatenation).
        BinOp::Concat if matches!(l, Value::TsQuery(_)) && matches!(r, Value::TsQuery(_)) => {
            let (Value::TsQuery(a), Value::TsQuery(b)) = (&l, &r) else { unreachable!() };
            Ok(Value::TsQuery(spg_storage::TsQueryAst::Or(
                alloc::boxed::Box::new(a.clone()),
                alloc::boxed::Box::new(b.clone()),
            )))
        }
        BinOp::Concat if matches!(l, Value::Json(_)) || matches!(r, Value::Json(_)) => {
            crate::json::concat(&l, &r)
        }
        BinOp::Concat => Ok(text_concat(&l, &r)),
        // PG `jsonb #- text[]` deletes the value at a nested path.
        BinOp::JsonDeletePath => crate::json::delete_path(&[l, r]),
        BinOp::BitOr => bitop(l, r, |a, b| a | b, "|"),
        BinOp::BitAnd => bitop(l, r, |a, b| a & b, "&"),
        BinOp::BitXor => bitop(l, r, |a, b| a ^ b, "#"),
        BinOp::JsonGet => crate::json::path_get(&l, &r, false),
        BinOp::JsonGetText => crate::json::path_get(&l, &r, true),
        BinOp::JsonGetPath => crate::json::path_walk(&l, &r, false),
        BinOp::JsonGetPathText => crate::json::path_walk(&l, &r, true),
        // v7.37 — RANGE operands claim @> / <@ / && ahead of the array /
        // JSON / inet interpretations. `range @> elem|range`, `<@` swapped,
        // `&&` overlap. Verified vs PG18.
        BinOp::JsonContains if matches!(l, Value::Range { .. }) => {
            let Value::Range { kind: ak, lower: al, upper: au, lower_inc: ali, upper_inc: aui, empty: ae } = &l
            else { unreachable!() };
            Ok(Value::Bool(match &r {
                Value::Range { kind: bk, lower: bl, upper: bu, lower_inc: bli, upper_inc: bui, empty: be } =>
                    range_contains_range(*ak, al, au, *ali, *aui, *ae, *bk, bl, bu, *bli, *bui, *be),
                elem => range_contains_elem(*ak, al, au, *ali, *aui, *ae, elem),
            }))
        }
        BinOp::JsonContainedBy if matches!(r, Value::Range { .. }) => {
            let Value::Range { kind: bk, lower: bl, upper: bu, lower_inc: bli, upper_inc: bui, empty: be } = &r
            else { unreachable!() };
            Ok(Value::Bool(match &l {
                Value::Range { kind: ak, lower: al, upper: au, lower_inc: ali, upper_inc: aui, empty: ae } =>
                    range_contains_range(*bk, bl, bu, *bli, *bui, *be, *ak, al, au, *ali, *aui, *ae),
                elem => range_contains_elem(*bk, bl, bu, *bli, *bui, *be, elem),
            }))
        }
        // tsquery `&&` is boolean AND (claim it before array / inet overlap).
        BinOp::InetOverlap if matches!(l, Value::TsQuery(_)) && matches!(r, Value::TsQuery(_)) => {
            let (Value::TsQuery(a), Value::TsQuery(b)) = (&l, &r) else { unreachable!() };
            Ok(Value::TsQuery(spg_storage::TsQueryAst::And(
                alloc::boxed::Box::new(a.clone()),
                alloc::boxed::Box::new(b.clone()),
            )))
        }
        BinOp::InetOverlap
            if matches!(l, Value::Range { .. }) && matches!(r, Value::Range { .. }) =>
        {
            let (
                Value::Range { kind: ak, lower: al, upper: au, lower_inc: ali, upper_inc: aui, empty: ae },
                Value::Range { kind: bk, lower: bl, upper: bu, lower_inc: bli, upper_inc: bui, empty: be },
            ) = (&l, &r) else { unreachable!() };
            Ok(Value::Bool(range_overlaps(
                *ak, al, au, *ali, *aui, *ae, *bk, bl, bu, *bli, *bui, *be,
            )))
        }
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
        // range `<<` / `>>` — strictly left / right of. Claims the operator
        // ahead of the bitstring / integer-shift / inet interpretations when
        // both operands are ranges.
        BinOp::InetContainedBy | BinOp::InetContains
            if matches!(l, Value::Range { .. }) && matches!(r, Value::Range { .. }) =>
        {
            let (
                Value::Range { kind: ak, lower: al, upper: au, lower_inc: ali, upper_inc: aui, empty: ae },
                Value::Range { kind: bk, lower: bl, upper: bu, lower_inc: bli, upper_inc: bui, empty: be },
            ) = (&l, &r) else { unreachable!() };
            // `a >> b` is `b << a`.
            let strictly_left = if matches!(op, BinOp::InetContainedBy) {
                range_strictly_left(*ak, al, au, *ali, *aui, *ae, *bk, bl, bu, *bli, *bui, *be)
            } else {
                range_strictly_left(*bk, bl, bu, *bli, *bui, *be, *ak, al, au, *ali, *aui, *ae)
            };
            Ok(Value::Bool(strictly_left))
        }
        // bit(n) << / >> k: shift within the fixed-width bit string. Claims
        // the operator ahead of the integer-shift and inet interpretations.
        BinOp::InetContainedBy | BinOp::InetContains
            if matches!(l, Value::BitString { .. }) && int_operand(&r).is_some() =>
        {
            let Value::BitString { nbits, bytes } = &l else { unreachable!() };
            let k = int_operand(&r).expect("guard checked");
            let out = bitstring_shift(*nbits, bytes, k, matches!(op, BinOp::InetContainedBy));
            Ok(Value::BitString {
                nbits: *nbits,
                bytes: alloc::borrow::Cow::Owned(out),
            })
        }
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
            // PG: timestamp - timestamp -> interval, justified to hours (every
            // 24h of the microsecond delta becomes one day). 30h -> `1 day
            // 06:00:00`, not a raw microsecond count.
            let delta = a.checked_sub(*b).ok_or(EvalError::TypeMismatch {
                detail: "TIMESTAMP - TIMESTAMP overflows i64 microseconds".into(),
            })?;
            const DAY_US: i64 = 86_400_000_000;
            let days = i32::try_from(delta / DAY_US).map_err(|_| EvalError::TypeMismatch {
                detail: "TIMESTAMP - TIMESTAMP day count overflows".into(),
            })?;
            return Ok(Some(Value::Interval {
                months: 0,
                days,
                micros: delta % DAY_US,
            }));
        }
        (Value::Time(a), Value::Time(b)) if op == BinOp::Sub => {
            // PG: time - time -> interval, the signed microsecond difference
            // (always within a day, so no day component). 10:30 - 08:15 ->
            // 02:15:00.
            return Ok(Some(Value::Interval {
                months: 0,
                days: 0,
                micros: a - b,
            }));
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
            // PG: `date ± interval` ALWAYS yields TIMESTAMP (rendered at
            // midnight when the interval has no sub-day part), because the
            // interval may carry a time component. `date ± integer` stays a
            // date, but that is a different operator handled elsewhere.
            let base = i64::from(*d)
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
                // PG's total float order: NaN == NaN and NaN > any number,
                // so the comparison is total (never errors on NaN).
                let ord = float_pg_cmp(af, bf);
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
/// PG `bit(n) << k` / `>> k`: shift the MSB-packed bit sequence within its
/// fixed `nbits` window, zero-filling the vacated end (bits shifted past
/// either end are dropped). A negative count reverses direction.
fn bitstring_shift(nbits: u32, bytes: &[u8], k: i64, mut left: bool) -> alloc::vec::Vec<u8> {
    let n = nbits as usize;
    let k = if k < 0 {
        left = !left;
        k.unsigned_abs() as usize
    } else {
        k as usize
    };
    let get = |i: usize| -> bool { i < n && (bytes[i / 8] >> (7 - (i % 8))) & 1 == 1 };
    let mut out = alloc::vec![0u8; n.div_ceil(8)];
    for i in 0..n {
        // shift-left: out[i] = in[i+k]; shift-right: out[i] = in[i-k].
        let src = if left { i.checked_add(k) } else { i.checked_sub(k) };
        if src.is_some_and(get) {
            out[i / 8] |= 1 << (7 - (i % 8));
        }
    }
    out
}

fn bitop(
    l: Value<'static>,
    r: Value<'static>,
    f: impl Fn(i64, i64) -> i64,
    op_name: &str,
) -> Result<Value<'static>, EvalError> {
    // PG `bit(n) & / | bit(n)`: byte-wise over equal-length bit strings.
    // Operands are MSB-packed and zero-padded, so `&`/`|` keep the padding
    // zero. Differing lengths are an error, matching PG.
    if let (Value::BitString { nbits: an, bytes: ab }, Value::BitString { nbits: bn, bytes: bb }) =
        (&l, &r)
    {
        if an != bn {
            return Err(EvalError::TypeMismatch {
                detail: format!("cannot {op_name} bit strings of different sizes"),
            });
        }
        let out: alloc::vec::Vec<u8> = ab
            .iter()
            .zip(bb.iter())
            .map(|(&x, &y)| f(i64::from(x), i64::from(y)) as u8)
            .collect();
        return Ok(Value::BitString {
            nbits: *an,
            bytes: alloc::borrow::Cow::Owned(out),
        });
    }
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
            // PG: int4 <op> int4 -> int4; a result that doesn't fit int4 is
            // "integer out of range", NOT a silent widening to bigint. This
            // matches SPG's own `::int` cast, which already errors on
            // overflow — the arithmetic path must agree.
            let result = int_op(i64::from(a), i64::from(b)).ok_or(EvalError::TypeMismatch {
                detail: format!("integer overflow on {op_name}"),
            })?;
            let small = i32::try_from(result).map_err(|_| EvalError::TypeMismatch {
                detail: "integer out of range".into(),
            })?;
            Ok(Value::Int(small))
        }
        // Order-preserving: the `|`-merged form used to bind the Int as the
        // left operand regardless of position, so `bigint - int` computed
        // `int - bigint` (and `bigint / int` computed `int / bigint`). Split
        // so each side keeps its place for the non-commutative ops.
        (Value::Int(a), Value::BigInt(b)) => {
            let result = int_op(i64::from(a), b).ok_or(EvalError::TypeMismatch {
                detail: format!("bigint overflow on {op_name}"),
            })?;
            Ok(Value::BigInt(result))
        }
        (Value::BigInt(a), Value::Int(b)) => {
            let result = int_op(a, i64::from(b)).ok_or(EvalError::TypeMismatch {
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
    // PG `point <-> point` is the Euclidean distance between two points.
    if let (Value::Point(a), Value::Point(b)) = (&l, &r) {
        let dx = a.x - b.x;
        let dy = a.y - b.y;
        return Ok(Value::Float(sqrt_newton(dx * dx + dy * dy)));
    }
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

/// v7.37.7 C.1.7 — PG integer modulo. PG's `%` (like C and the SQL
/// `mod()` function) is truncated-division remainder: the result takes
/// the sign of the DIVIDEND, so `-5 % 3 = -2`, not the Euclidean `1`.
/// That is exactly Rust's native `%`; `wrapping_rem` is used so the
/// `i64::MIN % -1` corner returns 0 instead of panicking. Division by
/// zero surfaces as `DivisionByZero` so callers see the same error
/// variant for `/` and `%`.
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
    // match; both closures take `i64`. `wrapping_rem` is truncated
    // remainder (sign of the dividend, matching PG / C / `mod()`) and
    // is panic-free including the `i64::MIN % -1` overflow corner.
    arith(
        l,
        r,
        |a, b| {
            if b == 0 { None } else { Some(a.wrapping_rem(b)) }
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

/// Element-wise ordering of one array element pair. PG's `array_cmp`
/// (the btree support routine backing `=` / `<` / `>` on arrays)
/// treats a NULL element as *greater* than any non-NULL, and two
/// NULLs as equal — a total order, unlike scalar `NULL = NULL`
/// which is unknown. This is why `ARRAY[1,NULL] = ARRAY[1,NULL]`
/// is `t` and `ARRAY[1,2] < ARRAY[1,NULL]` is `t`.
fn cmp_array_elem<T: Ord>(a: &Option<T>, b: &Option<T>) -> core::cmp::Ordering {
    use core::cmp::Ordering::{Equal, Greater, Less};
    match (a, b) {
        (None, None) => Equal,
        (None, Some(_)) => Greater,
        (Some(_), None) => Less,
        (Some(x), Some(y)) => x.cmp(y),
    }
}

/// Lexicographic array comparison: first differing element decides;
/// when one array is a prefix of the other, the shorter is less.
fn cmp_array<T: Ord>(a: &[Option<T>], b: &[Option<T>]) -> core::cmp::Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let c = cmp_array_elem(x, y);
        if c != core::cmp::Ordering::Equal {
            return c;
        }
    }
    a.len().cmp(&b.len())
}

/// Element-wise array *value* equality (PG `array_eq`). Two arrays are
/// equal iff same length and every element pair is equal by `elem_eq`.
/// A NULL element is equal only to another NULL (PG `array_eq` treats
/// two NULLs as equal — `ARRAY[1,NULL] = ARRAY[1,NULL]` is `t`, not
/// NULL), and NULL vs non-NULL is unequal. This is used for the array
/// types whose element equality is *value-based* rather than the derived
/// `Ord` on the stored repr (NUMERIC[] scale-insensitive, FLOAT8[] NaN,
/// INTERVAL[] unit-equivalence), so `cmp_array`'s repr `Ord` is wrong.
/// Value comparison of two arrays under PG `array_cmp`: element by element
/// with the element comparator; the first non-equal pair decides. A NULL
/// element sorts GREATER than any non-NULL (two NULLs are equal). If every
/// common element is equal, the shorter array is less. Verified vs PG18:
/// `[1,NULL] > [1,2]`, `[1] < [1,2]`.
fn array_value_cmp<T>(
    a: &[Option<T>],
    b: &[Option<T>],
    elem_cmp: impl Fn(&T, &T) -> core::cmp::Ordering,
) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    for (x, y) in a.iter().zip(b.iter()) {
        let o = match (x, y) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(p), Some(q)) => elem_cmp(p, q),
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

/// Value comparison of two NUMERIC elements `(scaled, scale)`. Scale-
/// insensitive (`1.10` == `1.1`): rescale both up to the wider scale
/// (exact, no precision loss) and compare the i128. Mirrors the scalar
/// `numeric` compare (`apply_binary_numeric`). Rescale overflow
/// (astronomically different scales) falls back to the raw-scaled compare.
fn numeric_pair_cmp(a: (i128, u8), b: (i128, u8)) -> core::cmp::Ordering {
    let t = a.1.max(b.1);
    match (rescale(a.0, a.1, t), rescale(b.0, b.1, t)) {
        (Some(x), Some(y)) => x.cmp(&y),
        _ => a.0.cmp(&b.0),
    }
}

/// Value comparison of two FLOAT8 elements under PG's btree/array order:
/// `NaN` is the greatest value (`NaN == NaN`, `NaN >` any number) and
/// `+0.0 == -0.0`. `partial_cmp` gives the finite case (incl. signed-zero
/// equality); the NaN cases are special-cased.
fn float_pg_cmp(a: f64, b: f64) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
    }
}

/// Value comparison of two INTERVAL elements by PG's canonical microsecond
/// span (months = 30 days, days = 24 hours), so `1 mon` == `30 days` and
/// `1 day` == `24:00:00`. i128 keeps the product from overflowing i64.
fn interval_span_cmp(
    a: &spg_storage::IntervalSpan,
    b: &spg_storage::IntervalSpan,
) -> core::cmp::Ordering {
    let span = |s: &spg_storage::IntervalSpan| -> i128 {
        (i128::from(s.months) * 30 + i128::from(s.days)) * 86_400_000_000 + i128::from(s.micros)
    };
    span(a).cmp(&span(b))
}

/// Map a computed `Ordering` to the boolean result of a comparison op.
fn cmp_result(op: BinOp, ord: core::cmp::Ordering) -> Result<Value<'static>, EvalError> {
    match op {
        BinOp::Eq => Ok(Value::Bool(ord.is_eq())),
        BinOp::NotEq => Ok(Value::Bool(ord.is_ne())),
        BinOp::Lt => Ok(Value::Bool(ord.is_lt())),
        BinOp::LtEq => Ok(Value::Bool(ord.is_le())),
        BinOp::Gt => Ok(Value::Bool(ord.is_gt())),
        BinOp::GtEq => Ok(Value::Bool(ord.is_ge())),
        _ => Err(EvalError::TypeMismatch {
            detail: "array supports only comparison operators".into(),
        }),
    }
}

/// Compare the first `nbits` bits of two big-endian (MSB-first) byte
/// arrays, PG `bitncmp`. Full common bytes compared bytewise; the final
/// partial byte compared under a top-`rem`-bits mask.
fn bitncmp(a: &[u8; 16], b: &[u8; 16], nbits: u8) -> core::cmp::Ordering {
    let nbytes = (nbits / 8) as usize;
    let ord = a[..nbytes].cmp(&b[..nbytes]);
    if ord != core::cmp::Ordering::Equal {
        return ord;
    }
    let rem = nbits % 8;
    if rem == 0 {
        return core::cmp::Ordering::Equal;
    }
    let mask = 0xffu8 << (8 - rem);
    (a[nbytes] & mask).cmp(&(b[nbytes] & mask))
}

/// PG `network_cmp`: family first (IPv4 < IPv6), then the common netmask
/// prefix of the addresses, then the netmask length, then the full address.
fn network_cmp(
    af: u8,
    ab: u8,
    aa: &[u8; 16],
    bf: u8,
    bb: u8,
    ba: &[u8; 16],
) -> core::cmp::Ordering {
    if af != bf {
        return af.cmp(&bf);
    }
    let common = ab.min(bb);
    let ord = bitncmp(aa, ba, common);
    if ord != core::cmp::Ordering::Equal {
        return ord;
    }
    let ord = ab.cmp(&bb);
    if ord != core::cmp::Ordering::Equal {
        return ord;
    }
    let maxbits = if af == 4 { 32 } else { 128 };
    bitncmp(aa, ba, maxbits)
}

/// The successor of a discrete range bound (`+1` for int4/int8, `+1 day`
/// for date). `None` on overflow (PG errors; a comparison can just treat
/// the un-canonicalisable bound as-is). Continuous kinds return `None`.
fn range_step_up(kind: spg_storage::RangeKind, v: &Value<'_>) -> Option<Value<'static>> {
    use spg_storage::RangeKind as K;
    match (kind, v) {
        (K::Int4, Value::Int(n)) => n.checked_add(1).map(Value::Int),
        (K::Int8, Value::BigInt(n)) => n.checked_add(1).map(Value::BigInt),
        (K::Date, Value::Date(d)) => d.checked_add(1).map(Value::Date),
        _ => None,
    }
}

/// Canonicalise a range to PG's `[)` form for the DISCRETE kinds
/// (int4/int8/date): an exclusive lower becomes inclusive lower+1, an
/// inclusive upper becomes exclusive upper+1. Continuous kinds
/// (num/ts/tstz) are returned unchanged, so `[1,5)` != `[1,5]`.
fn range_canonical(
    kind: spg_storage::RangeKind,
    lower: &Option<alloc::boxed::Box<Value<'static>>>,
    upper: &Option<alloc::boxed::Box<Value<'static>>>,
    lower_inc: bool,
    upper_inc: bool,
) -> (Option<Value<'static>>, bool, Option<Value<'static>>, bool) {
    use spg_storage::RangeKind as K;
    let mut lo = lower.as_ref().map(|b| (**b).clone());
    let mut li = lower_inc;
    let mut up = upper.as_ref().map(|b| (**b).clone());
    let mut ui = upper_inc;
    if matches!(kind, K::Int4 | K::Int8 | K::Date) {
        if let (Some(l), false) = (lo.as_ref(), li) {
            if let Some(l2) = range_step_up(kind, l) {
                lo = Some(l2);
                li = true;
            }
        }
        if let (Some(u), true) = (up.as_ref(), ui) {
            if let Some(u2) = range_step_up(kind, u) {
                up = Some(u2);
                ui = false;
            }
        }
    }
    (lo, li, up, ui)
}

/// Sort key for range uppers: `None` (+∞) greatest, then by value, then an
/// inclusive upper after an exclusive one at the same value.
fn upper_cmp(au: &Option<Value<'static>>, aui: bool, bu: &Option<Value<'static>>, bui: bool) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    match (au, bu) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater, // +∞ greatest
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => bound_cmp(x, y).then(aui.cmp(&bui)), // inclusive(true) greater
    }
}

/// PG `range_cmp` total order: an empty range sorts first (two empties are
/// equal), otherwise compare canonical lower bounds, then upper bounds. The
/// `Equal` result subsumes range equality (canonical `[)` forms must match).
#[allow(clippy::too_many_arguments)]
fn range_cmp(
    ak: spg_storage::RangeKind,
    al: &Option<alloc::boxed::Box<Value<'static>>>,
    au: &Option<alloc::boxed::Box<Value<'static>>>,
    ali: bool,
    aui: bool,
    ae: bool,
    bk: spg_storage::RangeKind,
    bl: &Option<alloc::boxed::Box<Value<'static>>>,
    bu: &Option<alloc::boxed::Box<Value<'static>>>,
    bli: bool,
    bui: bool,
    be: bool,
) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    if ae || be {
        return match (ae, be) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Less,
            _ => Ordering::Greater,
        };
    }
    let (al2, ali2, au2, aui2) = range_canonical(ak, al, au, ali, aui);
    let (bl2, bli2, bu2, bui2) = range_canonical(bk, bl, bu, bli, bui);
    lower_cmp(&al2, ali2, &bl2, bli2).then_with(|| upper_cmp(&au2, aui2, &bu2, bui2))
}

/// A canonicalised range span: (lower, lower_inc, upper, upper_inc).
type CanonSpan = (Option<Value<'static>>, bool, Option<Value<'static>>, bool);

/// Compare two range-element bound values of the same range kind.
fn bound_cmp(a: &Value<'_>, b: &Value<'_>) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::BigInt(x), Value::BigInt(y)) => x.cmp(y),
        (Value::Date(x), Value::Date(y)) => x.cmp(y),
        (Value::Timestamp(x), Value::Timestamp(y)) => x.cmp(y),
        (Value::Numeric { scaled: xs, scale: xc }, Value::Numeric { scaled: ys, scale: yc }) => {
            numeric_pair_cmp((*xs, *xc), (*ys, *yc))
        }
        // Mixed real-number bounds/elements (e.g. `numrange(1.5,3.5) @> 2.5`
        // where the literal lexes as float but the range bounds are numeric):
        // compare as f64 so containment doesn't silently fall through to Equal.
        _ => match (num_as_f64(a), num_as_f64(b)) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
            _ => Ordering::Equal,
        },
    }
}

/// The f64 value of any real-number scalar, for cross-type bound comparison.
#[allow(clippy::cast_precision_loss)]
fn num_as_f64(v: &Value<'_>) -> Option<f64> {
    match v {
        Value::SmallInt(n) => Some(f64::from(*n)),
        Value::Int(n) => Some(f64::from(*n)),
        Value::BigInt(n) => Some(*n as f64),
        Value::Float(x) => Some(*x),
        Value::Numeric { scaled, scale } => {
            Some(*scaled as f64 / 10i128.pow(u32::from(*scale)) as f64)
        }
        _ => None,
    }
}

/// Sort key for range lowers: `None` (−∞) first, then by value, then an
/// inclusive lower before an exclusive one at the same value.
fn lower_cmp(al: &Option<Value<'static>>, ali: bool, bl: &Option<Value<'static>>, bli: bool) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    match (al, bl) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(x), Some(y)) => bound_cmp(x, y).then(bli.cmp(&ali)), // inclusive(true) first
    }
}

/// Does a span ending at `(au, aui)` reach a following span starting at
/// `(bl, bli)` — i.e. they overlap or are adjacent (no gap)?
fn upper_reaches_lower(au: &Option<Value<'static>>, aui: bool, bl: &Option<Value<'static>>, bli: bool) -> bool {
    use core::cmp::Ordering;
    match (au, bl) {
        (None, _) => true,   // +∞ upper reaches anything
        (_, None) => true,   // following span starts at −∞
        (Some(u), Some(l)) => match bound_cmp(u, l) {
            Ordering::Greater => true,       // overlap
            Ordering::Equal => aui || bli,   // adjacent iff the touching point is covered
            Ordering::Less => false,         // gap
        },
    }
}

/// Is upper `(au, aui)` strictly greater than upper `(bu, bui)`?
fn upper_greater(au: &Option<Value<'static>>, aui: bool, bu: &Option<Value<'static>>, bui: bool) -> bool {
    use core::cmp::Ordering;
    match (au, bu) {
        (None, None) => false,
        (None, Some(_)) => true, // +∞ greatest
        (Some(_), None) => false,
        (Some(x), Some(y)) => match bound_cmp(x, y) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => aui && !bui, // inclusive upper > exclusive upper
        },
    }
}

/// Normalise a multirange's spans to PG's canonical form: canonicalise each
/// (discrete `[)`), drop empties, sort by lower, then merge any overlapping
/// or adjacent spans. Two multiranges are equal iff their normal forms match.
fn normalize_multirange(kind: spg_storage::RangeKind, spans: &[spg_storage::RangeSpan]) -> alloc::vec::Vec<CanonSpan> {
    let mut cs: alloc::vec::Vec<CanonSpan> = spans
        .iter()
        .filter(|s| !s.empty)
        .map(|s| range_canonical(kind, &s.lower, &s.upper, s.lower_inc, s.upper_inc))
        .collect();
    cs.sort_by(|a, b| lower_cmp(&a.0, a.1, &b.0, b.1));
    let mut out: alloc::vec::Vec<CanonSpan> = alloc::vec::Vec::new();
    for s in cs {
        if let Some(last) = out.last_mut() {
            if upper_reaches_lower(&last.2, last.3, &s.0, s.1) {
                if upper_greater(&s.2, s.3, &last.2, last.3) {
                    last.2 = s.2;
                    last.3 = s.3;
                }
                continue;
            }
        }
        out.push(s);
    }
    out
}

/// Compare two already-canonicalised spans by lower then upper bound.
fn canon_span_cmp(a: &CanonSpan, b: &CanonSpan) -> core::cmp::Ordering {
    lower_cmp(&a.0, a.1, &b.0, b.1).then_with(|| upper_cmp(&a.2, a.3, &b.2, b.3))
}

/// PG multirange total order: compare the normalised span lists element by
/// element; a multirange that is a prefix of another sorts first (`{}` sorts
/// first). `Equal` subsumes multirange equality.
fn multirange_cmp(
    ak: spg_storage::RangeKind,
    aranges: &[spg_storage::RangeSpan],
    bk: spg_storage::RangeKind,
    branges: &[spg_storage::RangeSpan],
) -> core::cmp::Ordering {
    let _ = (ak, bk);
    let a = normalize_multirange(ak, aranges);
    let b = normalize_multirange(bk, branges);
    for (x, y) in a.iter().zip(b.iter()) {
        let o = canon_span_cmp(x, y);
        if o != core::cmp::Ordering::Equal {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

/// Is the interval bounded below by `(l, l_inc)` and above by `(u, u_inc)`
/// non-empty? (`−∞` lower / `+∞` upper are always fine; equal finite bounds
/// need both inclusive.)
fn range_point_le(l: &Option<Value<'static>>, l_inc: bool, u: &Option<Value<'static>>, u_inc: bool) -> bool {
    match (l, u) {
        (None, _) | (_, None) => true,
        (Some(lv), Some(uv)) => match bound_cmp(lv, uv) {
            core::cmp::Ordering::Less => true,
            core::cmp::Ordering::Greater => false,
            core::cmp::Ordering::Equal => l_inc && u_inc,
        },
    }
}

/// Range `&&` overlap: the two ranges share at least one point. Empty
/// overlaps nothing.
#[allow(clippy::too_many_arguments)]
fn range_overlaps(
    ak: spg_storage::RangeKind, al: &Option<alloc::boxed::Box<Value<'static>>>, au: &Option<alloc::boxed::Box<Value<'static>>>, ali: bool, aui: bool, ae: bool,
    bk: spg_storage::RangeKind, bl: &Option<alloc::boxed::Box<Value<'static>>>, bu: &Option<alloc::boxed::Box<Value<'static>>>, bli: bool, bui: bool, be: bool,
) -> bool {
    if ae || be {
        return false;
    }
    let (al2, ali2, au2, aui2) = range_canonical(ak, al, au, ali, aui);
    let (bl2, bli2, bu2, bui2) = range_canonical(bk, bl, bu, bli, bui);
    range_point_le(&al2, ali2, &bu2, bui2) && range_point_le(&bl2, bli2, &au2, aui2)
}

/// Range `@>` range: `a` contains `b`. Empty is contained in everything;
/// only empty contains empty.
#[allow(clippy::too_many_arguments)]
fn range_contains_range(
    ak: spg_storage::RangeKind, al: &Option<alloc::boxed::Box<Value<'static>>>, au: &Option<alloc::boxed::Box<Value<'static>>>, ali: bool, aui: bool, ae: bool,
    bk: spg_storage::RangeKind, bl: &Option<alloc::boxed::Box<Value<'static>>>, bu: &Option<alloc::boxed::Box<Value<'static>>>, bli: bool, bui: bool, be: bool,
) -> bool {
    if be {
        return true;
    }
    if ae {
        return false;
    }
    let (al2, ali2, au2, aui2) = range_canonical(ak, al, au, ali, aui);
    let (bl2, bli2, bu2, bui2) = range_canonical(bk, bl, bu, bli, bui);
    lower_cmp(&al2, ali2, &bl2, bli2) != core::cmp::Ordering::Greater
        && upper_cmp(&au2, aui2, &bu2, bui2) != core::cmp::Ordering::Less
}

/// Range `@>` element: `a` contains the scalar `e`.
fn range_contains_elem(
    ak: spg_storage::RangeKind, al: &Option<alloc::boxed::Box<Value<'static>>>, au: &Option<alloc::boxed::Box<Value<'static>>>, ali: bool, aui: bool, ae: bool,
    e: &Value<'_>,
) -> bool {
    use core::cmp::Ordering;
    if ae {
        return false;
    }
    let (al2, ali2, au2, aui2) = range_canonical(ak, al, au, ali, aui);
    let lower_ok = match &al2 {
        None => true,
        Some(lv) => match bound_cmp(e, lv) {
            Ordering::Greater => true,
            Ordering::Equal => ali2,
            Ordering::Less => false,
        },
    };
    let upper_ok = match &au2 {
        None => true,
        Some(uv) => match bound_cmp(e, uv) {
            Ordering::Less => true,
            Ordering::Equal => aui2,
            Ordering::Greater => false,
        },
    };
    lower_ok && upper_ok
}

/// Range `*` intersection: the overlapping sub-range (empty if disjoint).
#[allow(clippy::too_many_arguments)]
fn range_intersect(
    ak: spg_storage::RangeKind, al: &Option<alloc::boxed::Box<Value<'static>>>, au: &Option<alloc::boxed::Box<Value<'static>>>, ali: bool, aui: bool, ae: bool,
    bk: spg_storage::RangeKind, bl: &Option<alloc::boxed::Box<Value<'static>>>, bu: &Option<alloc::boxed::Box<Value<'static>>>, bli: bool, bui: bool, be: bool,
) -> Value<'static> {
    let _ = bk;
    let empty = Value::Range {
        kind: ak,
        lower: None,
        upper: None,
        lower_inc: false,
        upper_inc: false,
        empty: true,
    };
    if ae || be {
        return empty;
    }
    let (al2, ali2, au2, aui2) = range_canonical(ak, al, au, ali, aui);
    let (bl2, bli2, bu2, bui2) = range_canonical(bk, bl, bu, bli, bui);
    // intersection lower = the later start, upper = the earlier end.
    let (lo, lo_inc) = if lower_cmp(&al2, ali2, &bl2, bli2) == core::cmp::Ordering::Greater {
        (al2, ali2)
    } else {
        (bl2, bli2)
    };
    let (up, up_inc) = if upper_cmp(&au2, aui2, &bu2, bui2) == core::cmp::Ordering::Less {
        (au2, aui2)
    } else {
        (bu2, bui2)
    };
    if !range_point_le(&lo, lo_inc, &up, up_inc) {
        return empty;
    }
    Value::Range {
        kind: ak,
        lower: lo.map(alloc::boxed::Box::new),
        upper: up.map(alloc::boxed::Box::new),
        lower_inc: lo_inc,
        upper_inc: up_inc,
        empty: false,
    }
}

/// Do bound `(up, up_inc)` and `(low, low_inc)` touch with no gap and no
/// overlap? (equal values, exactly one side inclusive — the `[x) [x)` seam).
fn bounds_touch(up: &Option<Value<'static>>, up_inc: bool, low: &Option<Value<'static>>, low_inc: bool) -> bool {
    match (up, low) {
        (Some(u), Some(l)) => bound_cmp(u, l) == core::cmp::Ordering::Equal && (up_inc != low_inc),
        _ => false,
    }
}

/// Range `+` union: the two ranges must overlap or be adjacent, else PG
/// errors "result of range union would not be contiguous".
#[allow(clippy::too_many_arguments)]
fn range_union(
    ak: spg_storage::RangeKind, al: &Option<alloc::boxed::Box<Value<'static>>>, au: &Option<alloc::boxed::Box<Value<'static>>>, ali: bool, aui: bool, ae: bool,
    bk: spg_storage::RangeKind, bl: &Option<alloc::boxed::Box<Value<'static>>>, bu: &Option<alloc::boxed::Box<Value<'static>>>, bli: bool, bui: bool, be: bool,
) -> Result<Value<'static>, EvalError> {
    let mk = |k, lo: &Option<alloc::boxed::Box<Value<'static>>>, up: &Option<alloc::boxed::Box<Value<'static>>>, li, ui, e| Value::Range {
        kind: k,
        lower: lo.clone(),
        upper: up.clone(),
        lower_inc: li,
        upper_inc: ui,
        empty: e,
    };
    if ae {
        return Ok(mk(bk, bl, bu, bli, bui, be));
    }
    if be {
        return Ok(mk(ak, al, au, ali, aui, ae));
    }
    let overlap = range_overlaps(ak, al, au, ali, aui, ae, bk, bl, bu, bli, bui, be);
    let (al2, ali2, au2, aui2) = range_canonical(ak, al, au, ali, aui);
    let (bl2, bli2, bu2, bui2) = range_canonical(bk, bl, bu, bli, bui);
    let adjacent = bounds_touch(&au2, aui2, &bl2, bli2) || bounds_touch(&bu2, bui2, &al2, ali2);
    if !overlap && !adjacent {
        return Err(EvalError::TypeMismatch {
            detail: "result of range union would not be contiguous".into(),
        });
    }
    // union lower = the earlier start, upper = the later end.
    let (lo, lo_inc) = if lower_cmp(&al2, ali2, &bl2, bli2) == core::cmp::Ordering::Less {
        (al2, ali2)
    } else {
        (bl2, bli2)
    };
    let (up, up_inc) = if upper_cmp(&au2, aui2, &bu2, bui2) == core::cmp::Ordering::Greater {
        (au2, aui2)
    } else {
        (bu2, bui2)
    };
    Ok(Value::Range {
        kind: ak,
        lower: lo.map(alloc::boxed::Box::new),
        upper: up.map(alloc::boxed::Box::new),
        lower_inc: lo_inc,
        upper_inc: up_inc,
        empty: false,
    })
}

/// Range `-` difference: the part of `a` not covered by `b`. PG errors when
/// removing `b` would split `a` into two disjoint ranges.
#[allow(clippy::too_many_arguments)]
fn range_difference(
    ak: spg_storage::RangeKind, al: &Option<alloc::boxed::Box<Value<'static>>>, au: &Option<alloc::boxed::Box<Value<'static>>>, ali: bool, aui: bool, ae: bool,
    bk: spg_storage::RangeKind, bl: &Option<alloc::boxed::Box<Value<'static>>>, bu: &Option<alloc::boxed::Box<Value<'static>>>, bli: bool, bui: bool, be: bool,
) -> Result<Value<'static>, EvalError> {
    let empty = Value::Range {
        kind: ak,
        lower: None,
        upper: None,
        lower_inc: false,
        upper_inc: false,
        empty: true,
    };
    let mk = |lo: Option<alloc::boxed::Box<Value<'static>>>, up: Option<alloc::boxed::Box<Value<'static>>>, li, ui| Value::Range {
        kind: ak,
        lower: lo,
        upper: up,
        lower_inc: li,
        upper_inc: ui,
        empty: false,
    };
    if ae {
        return Ok(empty);
    }
    let a_unchanged = || Value::Range {
        kind: ak,
        lower: al.clone(),
        upper: au.clone(),
        lower_inc: ali,
        upper_inc: aui,
        empty: ae,
    };
    if be || !range_overlaps(ak, al, au, ali, aui, ae, bk, bl, bu, bli, bui, be) {
        return Ok(a_unchanged());
    }
    let (al2, ali2, au2, aui2) = range_canonical(ak, al, au, ali, aui);
    let (bl2, bli2, bu2, bui2) = range_canonical(bk, bl, bu, bli, bui);
    // `b` covers `a` entirely → empty.
    let b_starts_at_or_before_a = lower_cmp(&al2, ali2, &bl2, bli2) != core::cmp::Ordering::Less;
    let b_ends_at_or_after_a = upper_cmp(&au2, aui2, &bu2, bui2) != core::cmp::Ordering::Greater;
    if b_starts_at_or_before_a && b_ends_at_or_after_a {
        return Ok(empty);
    }
    let a_extends_left = lower_cmp(&al2, ali2, &bl2, bli2) == core::cmp::Ordering::Less;
    let a_extends_right = upper_cmp(&au2, aui2, &bu2, bui2) == core::cmp::Ordering::Greater;
    // `b` sits strictly inside `a` → the difference would be two ranges.
    if a_extends_left && a_extends_right {
        return Err(EvalError::TypeMismatch {
            detail: "result of range difference would not be contiguous".into(),
        });
    }
    if a_extends_left {
        // Keep `a`'s left part: [a.lower, b.lower).
        Ok(mk(al2.map(alloc::boxed::Box::new), bl2.map(alloc::boxed::Box::new), ali2, !bli2))
    } else {
        // Keep `a`'s right part: [b.upper, a.upper).
        Ok(mk(bu2.map(alloc::boxed::Box::new), au2.map(alloc::boxed::Box::new), !bui2, aui2))
    }
}

/// MONEY arithmetic on integer cents (PG semantics): `money ± money → money`,
/// `money × number → money`, `money ÷ number → money`, `money ÷ money → float8`
/// ratio. Returns `None` when the operator/operand shape is not money math (let
/// the caller fall through). Rounding is half-away-from-zero, no_std-friendly.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn money_arith(op: BinOp, l: &Value<'_>, r: &Value<'_>) -> Option<Result<Value<'static>, EvalError>> {
    if !matches!(l, Value::Money(_)) && !matches!(r, Value::Money(_)) {
        return None;
    }
    // A plain number that scales a money amount.
    let factor = |v: &Value<'_>| -> Option<f64> {
        match v {
            Value::SmallInt(n) => Some(f64::from(*n)),
            Value::Int(n) => Some(f64::from(*n)),
            Value::BigInt(n) => Some(*n as f64),
            Value::Float(x) => Some(*x),
            Value::Numeric { scaled, scale } => {
                Some(*scaled as f64 / 10i128.pow(u32::from(*scale)) as f64)
            }
            _ => None,
        }
    };
    let round_cents = |v: f64| -> i64 {
        if v >= 0.0 { (v + 0.5) as i64 } else { (v - 0.5) as i64 }
    };
    match (op, l, r) {
        (BinOp::Add, Value::Money(a), Value::Money(b)) => Some(Ok(Value::Money(a.saturating_add(*b)))),
        (BinOp::Sub, Value::Money(a), Value::Money(b)) => Some(Ok(Value::Money(a.saturating_sub(*b)))),
        (BinOp::Div, Value::Money(a), Value::Money(b)) => Some(if *b == 0 {
            Err(EvalError::DivisionByZero)
        } else {
            Ok(Value::Float(*a as f64 / *b as f64))
        }),
        (BinOp::Mul, Value::Money(a), other) | (BinOp::Mul, other, Value::Money(a)) => {
            factor(other).map(|f| Ok(Value::Money(round_cents(*a as f64 * f))))
        }
        (BinOp::Div, Value::Money(a), other) => factor(other).map(|f| {
            if f == 0.0 { Err(EvalError::DivisionByZero) } else { Ok(Value::Money(round_cents(*a as f64 / f))) }
        }),
        _ => None,
    }
}

/// Range `<<` "strictly left of": every point of `a` is less than every point
/// of `b`. True when `a`'s upper bound value is below `b`'s lower bound value;
/// at an equal boundary they must not both be inclusive (else the shared point
/// overlaps). Unbounded sides on the touching edges make it false.
#[allow(clippy::too_many_arguments)]
fn range_strictly_left(
    ak: spg_storage::RangeKind, al: &Option<alloc::boxed::Box<Value<'static>>>, au: &Option<alloc::boxed::Box<Value<'static>>>, ali: bool, aui: bool, ae: bool,
    bk: spg_storage::RangeKind, bl: &Option<alloc::boxed::Box<Value<'static>>>, bu: &Option<alloc::boxed::Box<Value<'static>>>, bli: bool, bui: bool, be: bool,
) -> bool {
    use core::cmp::Ordering;
    if ae || be {
        return false;
    }
    let (_, _, au2, aui2) = range_canonical(ak, al, au, ali, aui);
    let (bl2, bli2, _, _) = range_canonical(bk, bl, bu, bli, bui);
    match (&au2, &bl2) {
        (None, _) | (_, None) => false,
        (Some(u), Some(l)) => match bound_cmp(u, l) {
            Ordering::Less => true,
            Ordering::Equal => !(aui2 && bli2),
            Ordering::Greater => false,
        },
    }
}

/// Range `-|-` "is adjacent to": `a` and `b` touch at exactly one bound with
/// no gap and no overlap — one range's upper bound value equals the other's
/// lower bound value, and exactly one of the two touching bounds is inclusive.
pub(crate) fn range_adjacent_pair(a: &Value<'_>, b: &Value<'_>) -> Option<bool> {
    use core::cmp::Ordering;
    let (ak, al, au, ali, aui, ae) = match a {
        Value::Range { kind, lower, upper, lower_inc, upper_inc, empty } => {
            (*kind, lower.clone(), upper.clone(), *lower_inc, *upper_inc, *empty)
        }
        _ => return None,
    };
    let (bk, bl, bu, bli, bui, be) = match b {
        Value::Range { kind, lower, upper, lower_inc, upper_inc, empty } => {
            (*kind, lower.clone(), upper.clone(), *lower_inc, *upper_inc, *empty)
        }
        _ => return None,
    };
    if ae || be {
        return Some(false);
    }
    let (al2, ali2, au2, aui2) = range_canonical(ak, &al, &au, ali, aui);
    let (bl2, bli2, bu2, bui2) = range_canonical(bk, &bl, &bu, bli, bui);
    // a's upper touches b's lower.
    let a_left_of_b = match (&au2, &bl2) {
        (Some(u), Some(l)) => bound_cmp(u, l) == Ordering::Equal && (aui2 != bli2),
        _ => false,
    };
    // b's upper touches a's lower.
    let b_left_of_a = match (&bu2, &al2) {
        (Some(u), Some(l)) => bound_cmp(u, l) == Ordering::Equal && (bui2 != ali2),
        _ => false,
    };
    Some(a_left_of_b || b_left_of_a)
}

/// `range_merge(a, b)` — the smallest range that contains both `a` and `b`.
/// Unlike `+` union it never errors on a gap: the result spans it (earliest
/// start to latest end). An empty operand contributes nothing. Returns `None`
/// if either value is not a range (caller falls back).
pub(crate) fn range_merge_pair(a: &Value<'_>, b: &Value<'_>) -> Option<Value<'static>> {
    let (ak, al, au, ali, aui, ae) = match a {
        Value::Range { kind, lower, upper, lower_inc, upper_inc, empty } => {
            (*kind, lower.clone(), upper.clone(), *lower_inc, *upper_inc, *empty)
        }
        _ => return None,
    };
    let (bk, bl, bu, bli, bui, be) = match b {
        Value::Range { kind, lower, upper, lower_inc, upper_inc, empty } => {
            (*kind, lower.clone(), upper.clone(), *lower_inc, *upper_inc, *empty)
        }
        _ => return None,
    };
    let mk = |k, lo: Option<alloc::boxed::Box<Value<'static>>>, up: Option<alloc::boxed::Box<Value<'static>>>, li, ui, e| {
        Value::Range { kind: k, lower: lo, upper: up, lower_inc: li, upper_inc: ui, empty: e }
    };
    if ae {
        return Some(mk(bk, bl, bu, bli, bui, be));
    }
    if be {
        return Some(mk(ak, al, au, ali, aui, ae));
    }
    let (al2, ali2, au2, aui2) = range_canonical(ak, &al, &au, ali, aui);
    let (bl2, bli2, bu2, bui2) = range_canonical(bk, &bl, &bu, bli, bui);
    // merge lower = the earlier start, upper = the later end.
    let (lo, lo_inc) = if lower_cmp(&al2, ali2, &bl2, bli2) == core::cmp::Ordering::Less {
        (al2, ali2)
    } else {
        (bl2, bli2)
    };
    let (up, up_inc) = if upper_cmp(&au2, aui2, &bu2, bui2) == core::cmp::Ordering::Greater {
        (au2, aui2)
    } else {
        (bu2, bui2)
    };
    Some(mk(ak, lo.map(alloc::boxed::Box::new), up.map(alloc::boxed::Box::new), lo_inc, up_inc, false))
}

/// The numeric value of an inet/cidr address (IPv4 in the low 4 bytes,
/// IPv6 across all 16), MSB-first.
fn inet_addr_u128(family: u8, addr: &[u8; 16]) -> u128 {
    let slice: &[u8] = if family == 4 { &addr[0..4] } else { &addr[..] };
    slice.iter().fold(0u128, |acc, &b| (acc << 8) | u128::from(b))
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
            // PG total float order: NaN == NaN, NaN > any number.
            float_pg_cmp(af, bf)
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
        // v7.37.16 — element-wise array ordering for `=` / `<>` /
        // `<` / `<=` / `>` / `>=`. PG compares same-type arrays with
        // `array_cmp`; see `cmp_array` for the NULL / length rules.
        (Value::IntArray(a), Value::IntArray(b)) => cmp_array(a, b),
        (Value::BigIntArray(a), Value::BigIntArray(b)) => cmp_array(a, b),
        (Value::SmallIntArray(a), Value::SmallIntArray(b)) => cmp_array(a, b),
        (Value::TextArray(a), Value::TextArray(b)) => cmp_array(a, b),
        (Value::VarcharArray(a), Value::VarcharArray(b)) => cmp_array(a, b),
        (Value::BoolArray(a), Value::BoolArray(b)) => cmp_array(a, b),
        (Value::DateArray(a), Value::DateArray(b)) => cmp_array(a, b),
        (Value::TimestampArray(a), Value::TimestampArray(b)) => cmp_array(a, b),
        // Int / BigInt arrays cross-compare after widening the
        // narrower side (mirrors the `||` widening rules).
        (Value::IntArray(a), Value::BigIntArray(b)) => {
            let aw: alloc::vec::Vec<Option<i64>> = a.iter().map(|o| o.map(i64::from)).collect();
            cmp_array(&aw, b)
        }
        (Value::BigIntArray(a), Value::IntArray(b)) => {
            let bw: alloc::vec::Vec<Option<i64>> = b.iter().map(|o| o.map(i64::from)).collect();
            cmp_array(a, &bw)
        }
        // v7.37.16 — `jsonb = jsonb` / `jsonb <> jsonb` structural
        // equality (PG18-compatible: object keys compared order-free,
        // array elements order-sensitive, numbers by value). PG defines
        // a total order on jsonb, but the ordering operators (< <= > >=)
        // are DEFERRED here — only equality is wired. Returns early
        // because jsonb equality is not expressible as an `Ordering`.
        (Value::Json(_), Value::Json(_)) => {
            let eq = crate::json::equals(l, r)?;
            return match op {
                BinOp::Eq => Ok(Value::Bool(eq)),
                BinOp::NotEq => Ok(Value::Bool(!eq)),
                _ => Err(EvalError::TypeMismatch {
                    detail: "jsonb ordering (<, <=, >, >=) not supported; only = / <>".into(),
                }),
            };
        }
        // v7.37.17 — SMALLINT integer comparison. SmallInt↔Float goes
        // through the Float branch above; SmallInt↔Numeric through
        // `apply_binary_numeric`; only the pure-integer pairs land here.
        // Widen to the common width and compare by value.
        (Value::SmallInt(a), Value::SmallInt(b)) => i32::from(*a).cmp(&i32::from(*b)),
        (Value::SmallInt(a), Value::Int(b)) => i32::from(*a).cmp(b),
        (Value::Int(a), Value::SmallInt(b)) => a.cmp(&i32::from(*b)),
        (Value::SmallInt(a), Value::BigInt(b)) => i64::from(*a).cmp(b),
        (Value::BigInt(a), Value::SmallInt(b)) => a.cmp(&i64::from(*b)),
        // v7.37.17 — TIME / MONEY compare on their i64 storage repr
        // (microseconds since midnight / integer cents). PG defines a
        // full btree ordering for both.
        (Value::Time(a), Value::Time(b)) => a.cmp(b),
        (Value::Money(a), Value::Money(b)) => a.cmp(b),
        // v7.37.17 — BYTEA bytewise unsigned comparison (PG `byteacmp`).
        (Value::Bytes(a), Value::Bytes(b)) => a.as_ref().cmp(b.as_ref()),
        // v7.37.17 — MACADDR / MACADDR8 bytewise comparison
        // (PG `macaddr_cmp` / `macaddr8_cmp`).
        (Value::Macaddr(a), Value::Macaddr(b)) => a.cmp(b),
        (Value::Macaddr8(a), Value::Macaddr8(b)) => a.cmp(b),
        // v7.37.17 — same-type array `=` / `<>` / `<` / `<=` / `>` /
        // `>=` for the remaining Ord-element array variants. PG's
        // `array_cmp` total order = element-wise (`cmp_array`); uuid is
        // bytewise, bytea bytewise, money by cents — all match PG's
        // per-element btree ops.
        (Value::UuidArray(a), Value::UuidArray(b)) => cmp_array(a, b),
        (Value::BytesArray(a), Value::BytesArray(b)) => cmp_array(a, b),
        (Value::MoneyArray(a), Value::MoneyArray(b)) => cmp_array(a, b),
        // v7.37.18 — NUMERIC[] / FLOAT8[] / INTERVAL[] `=` / `<>` via
        // *value-based* element equality (PG `array_eq`), because the
        // stored repr's derived `Ord` is wrong for these element types:
        //   * NUMERIC (scaled, scale): `1.10` (110,2) vs `1.1` (11,1)
        //     differ in repr but are equal in value — must rescale.
        //   * FLOAT8: `NaN = NaN` is `t` and `+0.0 = -0.0` is `t` in PG.
        //   * INTERVAL: `1 day` = `24:00:00`, `1 mon` = `30 days` — equal
        //     by canonical microsecond span, not by (months, days, micros).
        // These use the SAME element comparators as the scalar `numeric =
        // numeric` / `interval = interval` paths so array and scalar
        // agree. Ordering (`<` etc.) is DEFERRED (`eq_only_result`).
        (Value::NumericArray(a), Value::NumericArray(b)) => {
            return cmp_result(op, array_value_cmp(a, b, |x, y| numeric_pair_cmp(*x, *y)));
        }
        (Value::FloatArray(a), Value::FloatArray(b)) => {
            return cmp_result(op, array_value_cmp(a, b, |x, y| float_pg_cmp(*x, *y)));
        }
        (Value::IntervalArray(a), Value::IntervalArray(b)) => {
            return cmp_result(op, array_value_cmp(a, b, interval_span_cmp));
        }
        // v7.37.17 — INET / CIDR comparison (=/<> and ordering). PG
        // `network_cmp`: family first (IPv4 < IPv6), then the common
        // netmask prefix of the addresses, then the netmask length, then
        // the full address. Live PG18.4-verified: 10.0.0.0/8 < 192.168/16,
        // 10.0.0.0/8 < 10.0.0.0/16, 1.2.3.4 < 1.2.3.5, IPv4 < IPv6.
        (
            Value::Inet { family: af, bits: ab, addr: aa },
            Value::Inet { family: bf, bits: bb, addr: ba },
        )
        | (
            Value::Cidr { family: af, bits: ab, addr: aa },
            Value::Cidr { family: bf, bits: bb, addr: ba },
        ) => {
            let ord = network_cmp(*af, *ab, aa, *bf, *bb, ba);
            return match op {
                BinOp::Eq => Ok(Value::Bool(ord.is_eq())),
                BinOp::NotEq => Ok(Value::Bool(ord.is_ne())),
                BinOp::Lt => Ok(Value::Bool(ord.is_lt())),
                BinOp::LtEq => Ok(Value::Bool(ord.is_le())),
                BinOp::Gt => Ok(Value::Bool(ord.is_gt())),
                BinOp::GtEq => Ok(Value::Bool(ord.is_ge())),
                _ => Err(EvalError::TypeMismatch {
                    detail: "inet/cidr supports only comparison operators".into(),
                }),
            };
        }
        // v7.37.17 — BIT / BIT VARYING comparison (=/<> and ordering).
        // Bytes are MSB-packed with the trailing sub-byte zero-padded at
        // construction, so a bytewise compare over the common byte prefix
        // reproduces PG `varbit_cmp`'s bit-lexicographic order; when the
        // common bytes tie, the string with MORE bits is greater (a shorter
        // bit string is a strict prefix of the longer, hence less). Verified
        // vs PG18: B'10'<B'11', B'1'<B'10', B'10'<B'100', B'101'<B'1010'.
        (
            Value::BitString { nbits: an, bytes: ab },
            Value::BitString { nbits: bn, bytes: bb },
        ) => {
            let av = ab.as_ref();
            let bv = bb.as_ref();
            let n = av.len().min(bv.len());
            let ord = av[..n].cmp(&bv[..n]).then_with(|| an.cmp(bn));
            return match op {
                BinOp::Eq => Ok(Value::Bool(ord.is_eq())),
                BinOp::NotEq => Ok(Value::Bool(ord.is_ne())),
                BinOp::Lt => Ok(Value::Bool(ord.is_lt())),
                BinOp::LtEq => Ok(Value::Bool(ord.is_le())),
                BinOp::Gt => Ok(Value::Bool(ord.is_gt())),
                BinOp::GtEq => Ok(Value::Bool(ord.is_ge())),
                _ => Err(EvalError::TypeMismatch {
                    detail: "bit/varbit supports only comparison operators".into(),
                }),
            };
        }
        // v7.37 — RANGE comparison (=/<> and ordering), PG `range_cmp`.
        // Discrete kinds (int4/int8/date) canonicalise to `[)` first, so
        // `int4range(1,5)` equals `'[1,4]'`; continuous kinds compare bounds
        // verbatim. An empty range sorts first. Verified vs PG18.
        (
            Value::Range { kind: ak, lower: al, upper: au, lower_inc: ali, upper_inc: aui, empty: ae },
            Value::Range { kind: bk, lower: bl, upper: bu, lower_inc: bli, upper_inc: bui, empty: be },
        ) => {
            let ord = range_cmp(*ak, al, au, *ali, *aui, *ae, *bk, bl, bu, *bli, *bui, *be);
            return cmp_result(op, ord);
        }
        // v7.37 — MULTIRANGE comparison (=/<> and ordering). Normalise both
        // to PG's canonical form (canonicalise each span, drop empties, sort,
        // merge overlapping/adjacent) and compare the span lists lexically.
        (Value::Multirange { kind: ak, ranges: ar }, Value::Multirange { kind: bk, ranges: br }) => {
            return cmp_result(op, multirange_cmp(*ak, ar, *bk, br));
        }
        // v7.37 — TSVECTOR equality (=/<>). SPG stores lexemes sorted by
        // word + deduped with their (ascending) positions and weight, so a
        // structural compare matches PG (`'bar foo' = 'foo bar'`, position-
        // sensitive, deduped). Ordering (< etc.) is deferred.
        (Value::TsVector(a), Value::TsVector(b)) => {
            let eq = a == b;
            return match op {
                BinOp::Eq => Ok(Value::Bool(eq)),
                BinOp::NotEq => Ok(Value::Bool(!eq)),
                _ => Err(EvalError::TypeMismatch {
                    detail: "tsvector ordering (<, <=, >, >=) not yet supported; only = / <>".into(),
                }),
            };
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
        | BinOp::BitXor
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
    fn interval_date_plus_pure_days_lifts_to_timestamp() {
        // PG: DATE + INTERVAL '7 days' yields TIMESTAMP at midnight, not DATE.
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
        let expected = i64::from(days_from_civil(2024, 6, 8)) * 86_400_000_000;
        assert_eq!(v, Value::Timestamp(expected));
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
