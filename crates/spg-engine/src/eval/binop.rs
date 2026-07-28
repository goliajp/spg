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

/// v7.39 (round 508) — `?#`: do the two shapes meet?
///
/// PG defines it over box/box, line/box, line/line, lseg/box, lseg/line,
/// lseg/lseg and path/path. Every one reduces to the same two questions —
/// do two segments cross, and do two axis-aligned extents overlap — so the
/// shapes are reduced to segments (a box to its four sides, a path to its
/// edges) and tested pairwise.
fn geom_intersects(l: &Value<'_>, r: &Value<'_>) -> Result<Value<'static>, EvalError> {
    use spg_storage::Point2D;
    const EPS: f64 = 1.0e-6;

    fn cross(o: Point2D, a: Point2D, b: Point2D) -> f64 {
        (a.x - o.x) * (b.y - o.y) - (a.y - o.y) * (b.x - o.x)
    }
    fn on_span(p: Point2D, a: Point2D, b: Point2D) -> bool {
        p.x >= a.x.min(b.x) - EPS
            && p.x <= a.x.max(b.x) + EPS
            && p.y >= a.y.min(b.y) - EPS
            && p.y <= a.y.max(b.y) + EPS
    }
    // Two closed segments meet when their endpoints straddle each other, or
    // when they are collinear and their spans overlap.
    fn segs_meet(p1: Point2D, p2: Point2D, q1: Point2D, q2: Point2D) -> bool {
        let (d1, d2) = (cross(q1, q2, p1), cross(q1, q2, p2));
        let (d3, d4) = (cross(p1, p2, q1), cross(p1, p2, q2));
        if ((d1 > EPS && d2 < -EPS) || (d1 < -EPS && d2 > EPS))
            && ((d3 > EPS && d4 < -EPS) || (d3 < -EPS && d4 > EPS))
        {
            return true;
        }
        (d1.abs() <= EPS && on_span(p1, q1, q2))
            || (d2.abs() <= EPS && on_span(p2, q1, q2))
            || (d3.abs() <= EPS && on_span(q1, p1, p2))
            || (d4.abs() <= EPS && on_span(q2, p1, p2))
    }

    // Every shape as the segments that bound it. A LINE is infinite, so it
    // is represented by a very long segment through its two known points —
    // enough to reach past any finite operand it is compared with.
    fn segments(v: &Value<'_>) -> Option<alloc::vec::Vec<(Point2D, Point2D)>> {
        match v {
            Value::Lseg(a, b) => Some(alloc::vec![(*a, *b)]),
            Value::PgBox(a, b) => {
                let (lo_x, hi_x) = (a.x.min(b.x), a.x.max(b.x));
                let (lo_y, hi_y) = (a.y.min(b.y), a.y.max(b.y));
                let c = |x, y| Point2D { x, y };
                Some(alloc::vec![
                    (c(lo_x, lo_y), c(hi_x, lo_y)),
                    (c(hi_x, lo_y), c(hi_x, hi_y)),
                    (c(hi_x, hi_y), c(lo_x, hi_y)),
                    (c(lo_x, hi_y), c(lo_x, lo_y)),
                ])
            }
            Value::Path { points, closed } => {
                if points.len() < 2 {
                    return Some(alloc::vec![]);
                }
                let mut out: alloc::vec::Vec<(Point2D, Point2D)> = points
                    .windows(2)
                    .map(|w| (w[0], w[1]))
                    .collect();
                if *closed {
                    out.push((points[points.len() - 1], points[0]));
                }
                Some(out)
            }
            // `Ax + By + C = 0`, sampled far enough out to behave as the
            // infinite line it is against any finite operand.
            Value::Line { a, b, c } => {
                const FAR: f64 = 1.0e9;
                if b.abs() > EPS {
                    let y = |x: f64| (-c - a * x) / b;
                    Some(alloc::vec![(
                        Point2D { x: -FAR, y: y(-FAR) },
                        Point2D { x: FAR, y: y(FAR) },
                    )])
                } else if a.abs() > EPS {
                    let x = -c / a;
                    Some(alloc::vec![(
                        Point2D { x, y: -FAR },
                        Point2D { x, y: FAR },
                    )])
                } else {
                    Some(alloc::vec![])
                }
            }
            _ => None,
        }
    }

    match (segments(l), segments(r)) {
        (Some(ls), Some(rs)) => Ok(Value::Bool(ls.iter().any(|(p1, p2)| {
            rs.iter().any(|(q1, q2)| segs_meet(*p1, *p2, *q1, *q2))
        }))),
        _ => Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "operator ?# not supported for {:?} and {:?}",
                l.data_type(),
                r.data_type()
            ),
        }),
    }
}

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
                    detail: "bigint out of range".into(),
                })
        }
        (UnOp::Neg, Value::Float(x)) => Ok(Value::Float(-x)),
        // v7.39 (read01 round 107) — unary minus on REAL (f32) was missing, so
        // `-3.5::real` errored "unary - applied to Real".
        (UnOp::Neg, Value::Real(x)) => Ok(Value::Real(-x)),
        (UnOp::Neg, Value::SmallInt(n)) => {
            n.checked_neg()
                .map(Value::SmallInt)
                .ok_or(EvalError::TypeMismatch {
                    detail: "smallint overflow on unary -".into(),
                })
        }
        (
            UnOp::Neg,
            Value::Numeric {
                scaled,
                scale,
                kind,
            },
        ) => {
            use spg_storage::NumericKind as NK;
            match kind {
                NK::Finite => scaled
                    .checked_neg()
                    .map(|s| Value::Numeric {
                        scaled: s,
                        scale,
                        kind: NK::Finite,
                    })
                    .ok_or(EvalError::TypeMismatch {
                        detail: "numeric overflow on unary -".into(),
                    }),
                // v7.38 (read01, T6.P3) — -(NaN)=NaN, -(±Inf)=∓Inf.
                NK::NaN => Ok(Value::numeric_special(NK::NaN)),
                NK::PosInf => Ok(Value::numeric_special(NK::NegInf)),
                NK::NegInf => Ok(Value::numeric_special(NK::PosInf)),
            }
        }
        // NOTE: PG has no `- money` operator (unary minus on money is an
        // error there), so money is intentionally NOT handled here.
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
        // v7.39 (round 238) — PG's wording: "operator does not exist: - text".
        (UnOp::Neg, other) => Err(EvalError::TypeMismatch {
            detail: format!(
                "operator does not exist: - {}",
                super::strings::pg_typeof_name(&other)
            ),
        }),
        // v7.39 (round 507) — unary `+` is the identity on a number and
        // KEEPS its type: measured on PG18, `+1` is integer, `+1.5` is
        // numeric, `+'2'::bigint` is bigint. It is not a no-op the parser
        // could have dropped, because PG refuses every other operand —
        // "operator does not exist: + boolean", and the same for text and
        // interval, which is why interval is absent here even though unary
        // MINUS accepts one.
        (
            UnOp::Plus,
            v @ (Value::SmallInt(_)
            | Value::Int(_)
            | Value::BigInt(_)
            | Value::Real(_)
            | Value::Float(_)
            | Value::Numeric { .. }),
        ) => Ok(v),
        (UnOp::Plus, other) => Err(EvalError::TypeMismatch {
            detail: format!(
                "operator does not exist: + {}",
                super::strings::pg_typeof_name(&other)
            ),
        }),
        // v7.38 (read01) — PG `~ int2` is int2, not int4.
        (UnOp::BitNot, Value::SmallInt(n)) => Ok(Value::SmallInt(!n)),
        (UnOp::BitNot, Value::Int(n)) => Ok(Value::Int(!n)),
        (UnOp::BitNot, Value::BigInt(n)) => Ok(Value::BigInt(!n)),
        // v7.38 (read01) — PG `~ inet` flips every host-address bit, keeping the
        // operand's netmask (`~ 255.0.0.0/8` → `0.255.255.255/8`).
        (UnOp::BitNot, Value::Inet { family, bits, addr }) => {
            let mask = if family == 4 {
                u128::from(u32::MAX)
            } else {
                u128::MAX
            };
            let flipped = !inet_addr_u128(family, &addr) & mask;
            Ok(inet_from_u128(family, bits, flipped))
        }
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
        // PG `~ macaddr`: complement all six octets.
        (UnOp::BitNot, Value::Macaddr(a)) => {
            let mut out = [0u8; 6];
            for i in 0..6 {
                out[i] = !a[i];
            }
            Ok(Value::Macaddr(out))
        }
        // PG `~ macaddr8`: complement all eight EUI-64 octets.
        (UnOp::BitNot, Value::Macaddr8(a)) => {
            let mut out = [0u8; 8];
            for i in 0..8 {
                out[i] = !a[i];
            }
            Ok(Value::Macaddr8(out))
        }
        (UnOp::BitNot, other) => Err(EvalError::TypeMismatch {
            detail: format!("cannot apply ~ to {other:?}"),
        }),
        (UnOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
        // v7.39 (round 346, M1) — PG names the type it wanted, in its own
        // words; SPG printed `NOT applied to Some(Int)`, which is a Rust
        // Debug of an internal shape. MariaDB negates any truth value
        // (`NOT 5` is 0), and that reading is applied by the caller, which
        // is where the dialect is known.
        (UnOp::Not, other) => Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "argument of NOT must be type boolean, not type {}",
                super::strings::pg_typeof_name(&other)
            ),
        }),
    }
}

/// v7.9.27b — true when two values are "not distinct" per PG:
/// both NULL counts as equal; otherwise reduces to regular Eq.
fn values_not_distinct(l: &Value<'_>, r: &Value<'_>) -> bool {
    match (l, r) {
        (Value::Null, Value::Null) => true,
        (Value::Null, _) | (_, Value::Null) => false,
        // v7.38 (read01) — "not distinct" is the type's `=` semantics, not a
        // representation-exact match, so `1 IS NOT DISTINCT FROM 1.0` is true
        // (int and numeric compare equal) like PG. Fall back to structural
        // equality only for types `compare` cannot order.
        _ => match compare(BinOp::Eq, l, r) {
            Ok(Value::Bool(b)) => b,
            _ => l == r,
        },
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
        require_comparable(BinOp::Eq, l, r)?;
        return Ok(Some(Value::Bool(values_not_distinct(l, r))));
    }
    if let BinOp::IsDistinctFrom = op {
        require_comparable(BinOp::Eq, l, r)?;
        return Ok(Some(Value::Bool(!values_not_distinct(l, r))));
    }
    if let BinOp::And = op {
        require_boolean_argument("AND", l, r)?;
        return Ok(Some(and_3vl_by_ref(l, r)));
    }
    if let BinOp::Or = op {
        require_boolean_argument("OR", l, r)?;
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

pub(crate) fn apply_binary(
    op: BinOp,
    l: Value<'static>,
    r: Value<'static>,
) -> Result<Value<'static>, EvalError> {
    // v7.39 (read01 round 70) — `tags @> '{b}'`. PG reads a bare string literal
    // beside an ARRAY operand as an array of that type (an "unknown" literal
    // takes the other side's type). SPG saw a TEXT and routed the operator to
    // its JSON reading — `@>` / `<@` errored, and `&&` went to the INET one. The
    // element-typed forms (`tags @> ARRAY['b']`) worked all along, so this was a
    // pure literal-coercion hole, not a missing operator.
    let (l, r) = coerce_array_literal_operands(op, l, r);
    // v7.39 (round 256) — PG treats a plain range as the one-element
    // multirange containing it, so the range↔multirange forms of the
    // CONTAINMENT / OVERLAP / POSITIONAL operators resolve. SPG had only
    // the multirange↔multirange arms wired: a mixed `range && multirange`
    // fell through to the INET reading and errored, and `range @>
    // multirange` / `multirange <@ range` answered a silent FALSE.
    // Promoting once here is the whole fix — the existing multirange arms
    // then handle every combination.
    //
    // The list is deliberately NOT every operator: PG declares no mixed
    // overload for the set algebra (`+` `-` `*`) or the comparisons, and
    // a blanket promotion made `multirange + range` silently answer where
    // PG raises `operator does not exist` (caught by probing the widened
    // surface, not by the cases this round set out to fix).
    let mixed_promotable = matches!(
        op,
        BinOp::JsonContains
            | BinOp::JsonContainedBy
            | BinOp::InetOverlap
            | BinOp::InetContainedBy
            | BinOp::InetContains
            | BinOp::OverLeft
            | BinOp::OverRight
    );
    let (l, r) = match (
        mixed_promotable && matches!(l, Value::Multirange { .. }),
        mixed_promotable && matches!(r, Value::Multirange { .. }),
    ) {
        (true, false) => match range_as_multirange(&r) {
            Some(m) => (l, m),
            None => (l, r),
        },
        (false, true) => match range_as_multirange(&l) {
            Some(m) => (m, r),
            None => (l, r),
        },
        _ => (l, r),
    };
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
        require_comparable(BinOp::Eq, &l, &r)?;
        return Ok(Value::Bool(values_not_distinct(&l, &r)));
    }
    if let BinOp::IsDistinctFrom = op {
        require_comparable(BinOp::Eq, &l, &r)?;
        return Ok(Value::Bool(!values_not_distinct(&l, &r)));
    }
    // v7.38 (read01) — PG's array `||` treats a NULL array operand as an empty
    // array (array_cat semantics), so `arr || NULL` and `NULL || arr` yield the
    // array itself, not NULL. (A scalar `text || NULL` still propagates NULL;
    // this only fires when the non-NULL side is an array.)
    if let BinOp::Concat = op {
        if l.is_null() && super::values::array_len(&r).is_some() {
            return Ok(r);
        }
        if r.is_null() && super::values::array_len(&l).is_some() {
            return Ok(l);
        }
    }
    // Everything else: any NULL operand → NULL.
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    // v7.38 (read01 sweep) — arithmetic against an unknown-type string literal
    // coerces the literal to the numeric operand's type (`5 + '3'`, `'10' - 2`,
    // `1.5 + '2'`), mirroring PG's implicit unknown → typed cast. Only the
    // arithmetic operators: `||` keeps its number→text rule and comparisons
    // coerce inside compare(). A literal that won't parse falls through to the
    // normal type-mismatch error.
    if matches!(
        op,
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod
    ) {
        let is_num = |v: &Value<'_>| {
            matches!(
                v.data_type(),
                Some(
                    DataType::Int
                        | DataType::BigInt
                        | DataType::SmallInt
                        | DataType::Float
                        | DataType::Numeric { .. }
                )
            )
        };
        if let Value::Text(s) = &l {
            if let Some(dt) = is_num(&r).then(|| r.data_type()).flatten() {
                if let Ok(c) = crate::conversions::coerce_value(Value::text(s.as_ref()), dt, "", 0)
                {
                    return apply_binary(op, c, r);
                }
            }
        }
        if let Value::Text(s) = &r {
            if let Some(dt) = is_num(&l).then(|| l.data_type()).flatten() {
                if let Ok(c) = crate::conversions::coerce_value(Value::text(s.as_ref()), dt, "", 0)
                {
                    return apply_binary(op, l, c);
                }
            }
        }
    }
    // NUMERIC arithmetic and comparisons run in fixed-point; promote
    // integers to a common NUMERIC scale and stay in i128 throughout.
    // A NUMERIC paired with an INTERVAL is interval scaling, not
    // numeric math — let the calendar path below take it.
    // `inet - inet` -> bigint: the count of addresses between them. Same
    // family required, matching PG.
    if op == BinOp::Sub {
        if let (
            Value::Inet {
                family: fa,
                addr: aa,
                ..
            },
            Value::Inet {
                family: fb,
                addr: ab,
                ..
            },
        ) = (&l, &r)
        {
            if fa != fb {
                return Err(EvalError::TypeMismatch {
                    detail: "cannot subtract addresses of different families".into(),
                });
            }
            let diff = inet_addr_u128(*fa, aa) as i128 - inet_addr_u128(*fb, ab) as i128;
            return i64::try_from(diff)
                .map(Value::BigInt)
                .map_err(|_| EvalError::TypeMismatch {
                    detail: "result out of range".into(),
                });
        }
    }
    // `inet ± bigint` -> inet: shift the address by N, keeping family + bits.
    // `bigint + inet` is commutative. PG18-verified: 192.168.1.5 + 10 =
    // 192.168.1.15/32.
    if matches!(op, BinOp::Add | BinOp::Sub) {
        let as_int = |v: &Value| -> Option<i128> {
            match v {
                Value::SmallInt(n) => Some(i128::from(*n)),
                Value::Int(n) => Some(i128::from(*n)),
                Value::BigInt(n) => Some(i128::from(*n)),
                _ => None,
            }
        };
        // A cidr operand behaves as PG's implicit cidr->inet cast: the result
        // is an inet shifted the same way.
        let arith = match (&l, &r) {
            (Value::Inet { family, bits, addr }, other)
            | (Value::Cidr { family, bits, addr }, other) => as_int(other)
                .map(|n| (*family, *bits, *addr, if op == BinOp::Sub { -n } else { n })),
            (other, Value::Inet { family, bits, addr })
            | (other, Value::Cidr { family, bits, addr })
                if op == BinOp::Add =>
            {
                as_int(other).map(|n| (*family, *bits, *addr, n))
            }
            _ => None,
        };
        if let Some((family, bits, addr, delta)) = arith {
            let cur = inet_addr_u128(family, &addr);
            let next = if delta >= 0 {
                cur.checked_add(delta as u128)
            } else {
                cur.checked_sub(delta.unsigned_abs())
            };
            let max = if family == 4 {
                u128::from(u32::MAX)
            } else {
                u128::MAX
            };
            let next = match next {
                Some(v) if v <= max => v,
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: "result is out of range for the inet type".into(),
                    });
                }
            };
            let mut new_addr = [0u8; 16];
            if family == 4 {
                #[allow(clippy::cast_possible_truncation)]
                new_addr[0..4].copy_from_slice(&(next as u32).to_be_bytes());
            } else {
                new_addr.copy_from_slice(&next.to_be_bytes());
            }
            return Ok(Value::Inet {
                family,
                bits,
                addr: new_addr,
            });
        }
    }
    // v7.39 (read01 pg_lsn.c) — `pg_lsn ± numeric` shifts the WAL location
    // by N bytes (commutative for +); `pg_lsn - pg_lsn` is the byte
    // difference as numeric. Out-of-range shifts are PG's dedicated error.
    if matches!(op, BinOp::Add | BinOp::Sub) {
        let as_bytes = |v: &Value| -> Option<i128> {
            match v {
                Value::SmallInt(n) => Some(i128::from(*n)),
                Value::Int(n) => Some(i128::from(*n)),
                Value::BigInt(n) => Some(i128::from(*n)),
                Value::Numeric {
                    scaled,
                    scale: 0,
                    kind: spg_storage::NumericKind::Finite,
                } => Some(*scaled),
                _ => None,
            }
        };
        if let (Value::PgLsn(a), Value::PgLsn(b)) = (&l, &r) {
            if op == BinOp::Sub {
                return Ok(Value::Numeric {
                    scaled: i128::from(*a) - i128::from(*b),
                    scale: 0,
                    kind: spg_storage::NumericKind::Finite,
                });
            }
        }
        let shift = match (&l, &r) {
            (Value::PgLsn(a), other) => {
                as_bytes(other).map(|n| (*a, if op == BinOp::Sub { -n } else { n }))
            }
            (other, Value::PgLsn(a)) if op == BinOp::Add => as_bytes(other).map(|n| (*a, n)),
            _ => None,
        };
        if let Some((base, delta)) = shift {
            let next = i128::from(base) + delta;
            let Ok(next) = u64::try_from(next) else {
                return Err(EvalError::TypeMismatch {
                    detail: "pg_lsn out of range".into(),
                });
            };
            return Ok(Value::PgLsn(next));
        }
    }
    // PG `path + path` concatenates the two point lists (both taken open);
    // the result is an open path. Verified vs PG18.4.
    if op == BinOp::Add {
        if let (Value::Path { points: pa, .. }, Value::Path { points: pb, .. }) = (&l, &r) {
            let mut points = pa.clone();
            points.extend_from_slice(pb);
            return Ok(Value::Path {
                points,
                closed: false,
            });
        }
    }
    // PG `point ± point` translates a point by another's coordinates;
    // `point * point` / `point / point` are complex-number multiply/divide
    // (PG treats a point as the complex number x + yi).
    if let (Value::Point(a), Value::Point(b)) = (&l, &r) {
        match op {
            BinOp::Add => {
                return Ok(Value::Point(spg_storage::Point2D {
                    x: a.x + b.x,
                    y: a.y + b.y,
                }));
            }
            BinOp::Sub => {
                return Ok(Value::Point(spg_storage::Point2D {
                    x: a.x - b.x,
                    y: a.y - b.y,
                }));
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
    // v7.39 (read01 geo_ops.c part 2) — box / circle / path scale by a
    // point with complex-number multiply/divide, like `point * point`
    // (PG box_mul / circle_mul_pt / path_mul_pt and the _div twins).
    if matches!(op, BinOp::Mul | BinOp::Div)
        && matches!(&r, Value::Point(_))
        && matches!(
            &l,
            Value::PgBox(..) | Value::Circle { .. } | Value::Path { .. }
        )
    {
        let Value::Point(f) = &r else { unreachable!() };
        let cx = |p: &spg_storage::Point2D| -> Result<spg_storage::Point2D, EvalError> {
            Ok(if matches!(op, BinOp::Mul) {
                spg_storage::Point2D {
                    x: p.x * f.x - p.y * f.y,
                    y: p.x * f.y + p.y * f.x,
                }
            } else {
                let denom = f.x * f.x + f.y * f.y;
                if denom == 0.0 {
                    return Err(EvalError::DivisionByZero);
                }
                spg_storage::Point2D {
                    x: (p.x * f.x + p.y * f.y) / denom,
                    y: (p.y * f.x - p.x * f.y) / denom,
                }
            })
        };
        let mag = sqrt_newton(f.x * f.x + f.y * f.y);
        return match &l {
            Value::PgBox(a, b) => {
                let (na, nb) = (cx(a)?, cx(b)?);
                // Re-canonicalize the corners (high, low).
                let hi = spg_storage::Point2D {
                    x: na.x.max(nb.x),
                    y: na.y.max(nb.y),
                };
                let lo = spg_storage::Point2D {
                    x: na.x.min(nb.x),
                    y: na.y.min(nb.y),
                };
                Ok(Value::PgBox(hi, lo))
            }
            Value::Circle { center, radius } => Ok(Value::Circle {
                center: cx(center)?,
                radius: if matches!(op, BinOp::Mul) {
                    radius * mag
                } else {
                    if mag == 0.0 {
                        return Err(EvalError::DivisionByZero);
                    }
                    radius / mag
                },
            }),
            Value::Path { points, closed } => {
                let mut out = alloc::vec::Vec::with_capacity(points.len());
                for p in points {
                    out.push(cx(p)?);
                }
                Ok(Value::Path {
                    points: out,
                    closed: *closed,
                })
            }
            _ => unreachable!(),
        };
    }
    // v7.39 (read01 multirangetypes.c) — multirange set algebra:
    // + union, - difference, * intersection.
    if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul)
        && matches!(l, Value::Multirange { .. })
        && matches!(r, Value::Multirange { .. })
    {
        let (Value::Multirange { kind, ranges: a }, Value::Multirange { ranges: b, .. }) = (&l, &r)
        else {
            unreachable!()
        };
        let ranges = match op {
            BinOp::Add => multirange_union(*kind, a, b),
            BinOp::Sub => multirange_difference(*kind, a, b),
            _ => multirange_intersection(*kind, a, b),
        };
        return Ok(Value::Multirange {
            kind: *kind,
            ranges,
        });
    }
    // MONEY arithmetic (integer cents) before the generic numeric path.
    if let Some(result) = money_arith(op, &l, &r) {
        return result;
    }
    // Range `*` intersection / `+` union / `-` difference claim Mul / Add / Sub
    // before numeric/date arithmetic.
    if op == BinOp::Mul || op == BinOp::Add || op == BinOp::Sub {
        if let (
            Value::Range {
                kind: ak,
                lower: al,
                upper: au,
                lower_inc: ali,
                upper_inc: aui,
                empty: ae,
            },
            Value::Range {
                kind: bk,
                lower: bl,
                upper: bu,
                lower_inc: bli,
                upper_inc: bui,
                empty: be,
            },
        ) = (&l, &r)
        {
            return match op {
                BinOp::Mul => Ok(range_intersect(
                    RangeParts::new(*ak, al, au, *ali, *aui, *ae),
                    RangeParts::new(*bk, bl, bu, *bli, *bui, *be),
                )),
                BinOp::Add => range_union(
                    RangeParts::new(*ak, al, au, *ali, *aui, *ae),
                    RangeParts::new(*bk, bl, bu, *bli, *bui, *be),
                ),
                _ => range_difference(
                    RangeParts::new(*ak, al, au, *ali, *aui, *ae),
                    RangeParts::new(*bk, bl, bu, *bli, *bui, *be),
                ),
            };
        }
    }
    if (matches!(l, Value::Numeric { .. } | Value::NumericBig(_))
        || matches!(r, Value::Numeric { .. } | Value::NumericBig(_))
        || matches!(l, Value::Real(_))
        || matches!(r, Value::Real(_)))
        && !matches!(l, Value::Interval { .. })
        && !matches!(r, Value::Interval { .. })
        // A range containment/overlap op (`range @> numeric`, `<@`, `&&`) keeps
        // a Numeric element — don't let the numeric fast-path claim it; the
        // range arms below handle it.
        && !matches!(l, Value::Range { .. })
        && !matches!(r, Value::Range { .. })
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
        // v7.39 (round 353, M9) — MySQL's `DIV`: integer division that
        // truncates TOWARD ZERO (`-7 DIV 2` is -3, `7 DIV -2` is -3) and
        // answers NULL on a zero divisor rather than raising. Measured on
        // MariaDB 11; a fractional or string operand is read as a number
        // first (`7.5 DIV 2` is 3, `'9' DIV 2` is 4).
        BinOp::IntDiv => int_div_op(&l, &r),
        BinOp::Mod => mod_op(l, r),
        // v7.39 (round 245) — `tsquery <-> tsquery` is PG's phrase
        // concatenation (tsquery_phrase, distance 1), a different operator
        // from the vector distance that shares the spelling.
        BinOp::L2Distance if matches!(l, Value::TsQuery(_)) && matches!(r, Value::TsQuery(_)) => {
            let (Value::TsQuery(a), Value::TsQuery(b)) = (&l, &r) else {
                unreachable!()
            };
            Ok(Value::TsQuery(spg_storage::TsQueryAst::Phrase {
                left: alloc::boxed::Box::new(a.clone()),
                right: alloc::boxed::Box::new(b.clone()),
                distance: 1,
            }))
        }
        BinOp::L2Distance => l2_distance(l, r),
        BinOp::InnerProduct => inner_product(l, r),
        BinOp::CosineDistance => cosine_distance(l, r),
        // PG `jsonb || jsonb` merges objects (right wins on dup keys) /
        // appends arrays. Text `||` stays text concatenation.
        // tsquery `||` is boolean OR (claim it before text concatenation).
        BinOp::Concat if matches!(l, Value::TsQuery(_)) && matches!(r, Value::TsQuery(_)) => {
            let (Value::TsQuery(a), Value::TsQuery(b)) = (&l, &r) else {
                unreachable!()
            };
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
        BinOp::BitOr | BinOp::BitAnd
            if matches!(l, Value::Inet { .. }) && matches!(r, Value::Inet { .. }) =>
        {
            inet_bitwise(op, &l, &r)
        }
        BinOp::BitOr => bitop(l, r, |a, b| a | b, "|"),
        BinOp::BitAnd => bitop(l, r, |a, b| a & b, "&"),
        // v7.38 (read01) — PG `lseg # lseg`: the point where two segments cross,
        // or NULL when they don't. Distinct from the bit/integer `#` (XOR).
        BinOp::BitXor if matches!(l, Value::Lseg(..)) && matches!(r, Value::Lseg(..)) => {
            let (Value::Lseg(a1, a2), Value::Lseg(b1, b2)) = (&l, &r) else {
                unreachable!()
            };
            match lseg_intersection(*a1, *a2, *b1, *b2) {
                Some(p) => Ok(Value::Point(p)),
                None => Ok(Value::Null),
            }
        }
        // v7.39 (read01 geo_ops.c part 2) — `box # box` intersection box
        // (NULL when they don't overlap) and `line # line` intersection
        // point (NULL for parallel lines, PG line_interpt_line).
        BinOp::BitXor if matches!(l, Value::PgBox(..)) && matches!(r, Value::PgBox(..)) => {
            let (Value::PgBox(aur, all), Value::PgBox(bur, bll)) = (&l, &r) else {
                unreachable!()
            };
            if !(all.x <= bur.x && bll.x <= aur.x && all.y <= bur.y && bll.y <= aur.y) {
                return Ok(Value::Null);
            }
            Ok(Value::PgBox(
                spg_storage::Point2D {
                    x: aur.x.min(bur.x),
                    y: aur.y.min(bur.y),
                },
                spg_storage::Point2D {
                    x: all.x.max(bll.x),
                    y: all.y.max(bll.y),
                },
            ))
        }
        BinOp::BitXor if matches!(l, Value::Line { .. }) && matches!(r, Value::Line { .. }) => {
            let (
                Value::Line {
                    a: a1,
                    b: b1,
                    c: c1,
                },
                Value::Line {
                    a: a2,
                    b: b2,
                    c: c2,
                },
            ) = (&l, &r)
            else {
                unreachable!()
            };
            match line_line_interpt(*a1, *b1, *c1, *a2, *b2, *c2) {
                Some(p) => Ok(Value::Point(p)),
                None => Ok(Value::Null),
            }
        }
        BinOp::BitXor => bitop(l, r, |a, b| a ^ b, "#"),
        // v7.39 (read01 geo_ops.c part 2) — `##` closest point on the
        // right-hand object to the left-hand point (PG close_ps /
        // close_pb; the point is ITS OWN closest point when inside the
        // box).
        BinOp::ClosestPoint => match (&l, &r) {
            (Value::Point(pt), Value::Lseg(a, b)) => {
                Ok(Value::Point(lseg_closest_to_point(a, b, pt)))
            }
            (Value::Point(pt), Value::PgBox(ur, ll)) => {
                let (hx, hy) = (ll.x.max(ur.x), ll.y.max(ur.y));
                let (lx, ly) = (ll.x.min(ur.x), ll.y.min(ur.y));
                if pt.x >= lx && pt.x <= hx && pt.y >= ly && pt.y <= hy {
                    return Ok(Value::Point(*pt));
                }
                let c = [
                    spg_storage::Point2D { x: lx, y: ly },
                    spg_storage::Point2D { x: lx, y: hy },
                    spg_storage::Point2D { x: hx, y: hy },
                    spg_storage::Point2D { x: hx, y: ly },
                ];
                let mut best = spg_storage::Point2D { x: lx, y: ly };
                let mut bd = f64::INFINITY;
                for i in 0..4 {
                    let j = (i + 1) % 4;
                    let n = lseg_closest_to_point(&c[i], &c[j], pt);
                    let d = pt_dist(&n, pt);
                    if d < bd {
                        bd = d;
                        best = n;
                    }
                }
                Ok(Value::Point(best))
            }
            _ => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "operator ## not supported for {:?} and {:?}",
                    l.data_type(),
                    r.data_type()
                ),
            }),
        },
        // v7.39 (read01 geo_ops.c part 2) — `point ?- point`: horizontally
        // aligned (equal y under the geometric epsilon).
        BinOp::GeomHoriz => match (&l, &r) {
            (Value::Point(a), Value::Point(b)) => Ok(Value::Bool((a.y - b.y).abs() <= 1.0e-6)),
            _ => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "operator ?- not supported for {:?} and {:?}",
                    l.data_type(),
                    r.data_type()
                ),
            }),
        },
        // v7.39 (round 508) — `<^` / `>^`: strictly below / strictly above.
        // On points that is the y coordinate; on boxes PG compares the two
        // boxes' vertical extents, so a box is "below" another when its
        // HIGH y is under the other's LOW y. Measured: `box '((0,0),(1,1))'
        // <^ box '((0,2),(1,3))'` is true.
        BinOp::IsBelow | BinOp::IsAbove => {
            let below = matches!(op, BinOp::IsBelow);
            let extent = |v: &Value<'_>| -> Option<(f64, f64)> {
                match v {
                    Value::Point(p) => Some((p.y, p.y)),
                    Value::PgBox(a, b) => Some((a.y.min(b.y), a.y.max(b.y))),
                    _ => None,
                }
            };
            match (extent(&l), extent(&r)) {
                (Some((llo, lhi)), Some((rlo, rhi))) => Ok(Value::Bool(if below {
                    lhi < rlo
                } else {
                    llo > rhi
                })),
                _ => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "operator {} not supported for {:?} and {:?}",
                        if below { "<^" } else { ">^" },
                        l.data_type(),
                        r.data_type()
                    ),
                }),
            }
        }
        // v7.39 (round 508) — `?#` "do these intersect". PG defines it for
        // box/box, line/box, line/line, lseg/box, lseg/line, lseg/lseg and
        // path/path; the shared reading is "do the two point sets meet".
        BinOp::Intersects => geom_intersects(&l, &r),
        // v7.39 (round 508) — the `text_pattern_ops` comparisons. They
        // compare BYTES and ignore collation, which is exactly what makes
        // them usable for a LIKE-prefix index: measured on PG18,
        // `'A' ~<~ 'a'` is true while `'A' < 'a'` is false.
        BinOp::PatternLt | BinOp::PatternLtEq | BinOp::PatternGt | BinOp::PatternGtEq => {
            match (&l, &r) {
                (Value::Text(a), Value::Text(b)) => {
                    let ord = a.as_bytes().cmp(b.as_bytes());
                    Ok(Value::Bool(match op {
                        BinOp::PatternLt => ord.is_lt(),
                        BinOp::PatternLtEq => ord.is_le(),
                        BinOp::PatternGt => ord.is_gt(),
                        _ => ord.is_ge(),
                    }))
                }
                _ => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "operator {op} not supported for {:?} and {:?}",
                        l.data_type(),
                        r.data_type()
                    ),
                }),
            }
        }
        // v7.39 (read01 geo_ops.c) — geometric predicates. Slopes compare
        // with PG's geometric EPSILON (1e-6): parallel = equal slopes,
        // perpendicular = vertical×horizontal or m1·m2 = -1.
        BinOp::GeomParallel | BinOp::GeomPerp => {
            const EPS: f64 = 1.0e-6;
            let slope_of = |v: &Value<'_>| -> Option<f64> {
                match v {
                    Value::Lseg(a, b) => Some(if (a.x - b.x).abs() <= EPS {
                        f64::INFINITY
                    } else if (a.y - b.y).abs() <= EPS {
                        0.0
                    } else {
                        (a.y - b.y) / (a.x - b.x)
                    }),
                    Value::Line { a, b, .. } => Some(if b.abs() <= EPS {
                        f64::INFINITY
                    } else {
                        -a / b
                    }),
                    _ => None,
                }
            };
            match (slope_of(&l), slope_of(&r)) {
                (Some(m1), Some(m2)) => {
                    let res = if matches!(op, BinOp::GeomParallel) {
                        (m1.is_infinite() && m2.is_infinite()) || (m1 - m2).abs() <= EPS
                    } else if m1.is_infinite() {
                        m2.abs() <= EPS
                    } else if m2.is_infinite() {
                        m1.abs() <= EPS
                    } else {
                        (m1 * m2 + 1.0).abs() <= EPS
                    };
                    Ok(Value::Bool(res))
                }
                _ => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "operator {} needs lseg or line operands, got {:?} and {:?}",
                        if matches!(op, BinOp::GeomParallel) {
                            "?||"
                        } else {
                            "?-|"
                        },
                        l.data_type(),
                        r.data_type()
                    ),
                }),
            }
        }
        // v7.39 (read01 geo_ops.c) — `~=` "same as": component equality
        // under the geometric EPSILON, for point / box / circle / polygon.
        BinOp::GeomSameAs => {
            const EPS: f64 = 1.0e-6;
            let feq = |a: f64, b: f64| (a - b).abs() <= EPS;
            let res = match (&l, &r) {
                (Value::Point(a), Value::Point(b)) => Some(feq(a.x, b.x) && feq(a.y, b.y)),
                (Value::PgBox(a1, a2), Value::PgBox(b1, b2)) => {
                    // Canonical corners (high, low) compare pairwise.
                    let hx = |p: &spg_storage::Point2D, q: &spg_storage::Point2D| {
                        (p.x.max(q.x), p.y.max(q.y), p.x.min(q.x), p.y.min(q.y))
                    };
                    let a = hx(a1, a2);
                    let b = hx(b1, b2);
                    Some(feq(a.0, b.0) && feq(a.1, b.1) && feq(a.2, b.2) && feq(a.3, b.3))
                }
                (
                    Value::Circle {
                        center: c1,
                        radius: r1,
                    },
                    Value::Circle {
                        center: c2,
                        radius: r2,
                    },
                ) => Some(feq(c1.x, c2.x) && feq(c1.y, c2.y) && feq(*r1, *r2)),
                (Value::Polygon(a), Value::Polygon(b)) => Some(
                    a.len() == b.len()
                        && a.iter()
                            .zip(b.iter())
                            .all(|(p, q)| feq(p.x, q.x) && feq(p.y, q.y)),
                ),
                _ => None,
            };
            match res {
                Some(v) => Ok(Value::Bool(v)),
                None => Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "operator ~= needs matching geometric operands, got {:?} and {:?}",
                        l.data_type(),
                        r.data_type()
                    ),
                }),
            }
        }
        BinOp::JsonGet => crate::json::path_get(&l, &r, false),
        BinOp::JsonGetText => crate::json::path_get(&l, &r, true),
        BinOp::JsonGetPath => crate::json::path_walk(&l, &r, false),
        BinOp::JsonGetPathText => crate::json::path_walk(&l, &r, true),
        // v7.37 — RANGE operands claim @> / <@ / && ahead of the array /
        // JSON / inet interpretations. `range @> elem|range`, `<@` swapped,
        // `&&` overlap. Verified vs PG18.
        // v7.39 (read01 multirangetypes.c) — multirange containment.
        BinOp::JsonContains if matches!(l, Value::Multirange { .. }) => {
            let Value::Multirange { kind, ranges } = &l else {
                unreachable!()
            };
            match multirange_contains(*kind, ranges, &r) {
                Some(b) => Ok(Value::Bool(b)),
                None => Ok(Value::Null),
            }
        }
        BinOp::JsonContainedBy if matches!(r, Value::Multirange { .. }) => {
            let Value::Multirange { kind, ranges } = &r else {
                unreachable!()
            };
            match multirange_contains(*kind, ranges, &l) {
                Some(b) => Ok(Value::Bool(b)),
                None => Ok(Value::Null),
            }
        }
        // v7.39 (round 256) — the POSITIONAL operators read a multirange
        // as its outer hull (probed: `{[1,3),[9,11)} -|- {[3,5)}` is
        // FALSE, so it is not an any-element rule). An empty multirange
        // has no hull and answers false. Rewriting to the hull lets the
        // existing range arms below do the work.
        BinOp::InetContainedBy | BinOp::InetContains | BinOp::OverLeft | BinOp::OverRight
            if matches!(l, Value::Multirange { .. })
                && matches!(r, Value::Multirange { .. }) =>
        {
            let (Value::Multirange { kind, ranges: a }, Value::Multirange { ranges: b, .. }) =
                (&l, &r)
            else {
                unreachable!()
            };
            if a.is_empty() || b.is_empty() {
                return Ok(Value::Bool(false));
            }
            let (hl, hr) = (multirange_hull(*kind, a), multirange_hull(*kind, b));
            apply_binary(op, hl, hr)
        }
        // Multirange overlap: non-empty intersection.
        BinOp::InetOverlap
            if matches!(l, Value::Multirange { .. }) && matches!(r, Value::Multirange { .. }) =>
        {
            let (Value::Multirange { kind, ranges: a }, Value::Multirange { ranges: b, .. }) =
                (&l, &r)
            else {
                unreachable!()
            };
            Ok(Value::Bool(
                !multirange_intersection(*kind, a, b).is_empty(),
            ))
        }
        BinOp::JsonContains if matches!(l, Value::Range { .. }) => {
            let Value::Range {
                kind: ak,
                lower: al,
                upper: au,
                lower_inc: ali,
                upper_inc: aui,
                empty: ae,
            } = &l
            else {
                unreachable!()
            };
            Ok(Value::Bool(match &r {
                Value::Range {
                    kind: bk,
                    lower: bl,
                    upper: bu,
                    lower_inc: bli,
                    upper_inc: bui,
                    empty: be,
                } => range_contains_range(
                    RangeParts::new(*ak, al, au, *ali, *aui, *ae),
                    RangeParts::new(*bk, bl, bu, *bli, *bui, *be),
                ),
                // v7.39 (round 256) — an ARRAY is not an element of any
                // range type: PG reports `operator does not exist:
                // int4range @> integer[]`, where SPG answered a silent
                // false through the element path.
                elem if super::values::array_len(elem).is_some() => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "operator does not exist: {} @> {}",
                            super::strings::pg_typeof_name(&l),
                            super::strings::pg_typeof_name(elem)
                        ),
                    });
                }
                elem => range_contains_elem(*ak, al, au, *ali, *aui, *ae, elem),
            }))
        }
        BinOp::JsonContainedBy if matches!(r, Value::Range { .. }) => {
            let Value::Range {
                kind: bk,
                lower: bl,
                upper: bu,
                lower_inc: bli,
                upper_inc: bui,
                empty: be,
            } = &r
            else {
                unreachable!()
            };
            Ok(Value::Bool(match &l {
                Value::Range {
                    kind: ak,
                    lower: al,
                    upper: au,
                    lower_inc: ali,
                    upper_inc: aui,
                    empty: ae,
                } => range_contains_range(
                    RangeParts::new(*bk, bl, bu, *bli, *bui, *be),
                    RangeParts::new(*ak, al, au, *ali, *aui, *ae),
                ),
                elem => range_contains_elem(*bk, bl, bu, *bli, *bui, *be, elem),
            }))
        }
        // tsquery `&&` is boolean AND (claim it before array / inet overlap).
        BinOp::InetOverlap if matches!(l, Value::TsQuery(_)) && matches!(r, Value::TsQuery(_)) => {
            let (Value::TsQuery(a), Value::TsQuery(b)) = (&l, &r) else {
                unreachable!()
            };
            Ok(Value::TsQuery(spg_storage::TsQueryAst::And(
                alloc::boxed::Box::new(a.clone()),
                alloc::boxed::Box::new(b.clone()),
            )))
        }
        // PG `tsquery @> tsquery` / `<@` — containment by lexeme set: `a @> b`
        // iff every lexeme in b appears in a (combining operators ignored).
        BinOp::JsonContains | BinOp::JsonContainedBy
            if matches!(l, Value::TsQuery(_)) && matches!(r, Value::TsQuery(_)) =>
        {
            let (Value::TsQuery(a), Value::TsQuery(b)) = (&l, &r) else {
                unreachable!()
            };
            let (container, contained) = if matches!(op, BinOp::JsonContains) {
                (a, b)
            } else {
                (b, a)
            };
            let mut cont_lex = alloc::collections::BTreeSet::new();
            tsquery_lexemes(container, &mut cont_lex);
            let mut sub_lex = alloc::collections::BTreeSet::new();
            tsquery_lexemes(contained, &mut sub_lex);
            Ok(Value::Bool(sub_lex.is_subset(&cont_lex)))
        }
        BinOp::InetOverlap
            if matches!(l, Value::Range { .. }) && matches!(r, Value::Range { .. }) =>
        {
            let (
                Value::Range {
                    kind: ak,
                    lower: al,
                    upper: au,
                    lower_inc: ali,
                    upper_inc: aui,
                    empty: ae,
                },
                Value::Range {
                    kind: bk,
                    lower: bl,
                    upper: bu,
                    lower_inc: bli,
                    upper_inc: bui,
                    empty: be,
                },
            ) = (&l, &r)
            else {
                unreachable!()
            };
            Ok(Value::Bool(range_overlaps(
                RangeParts::new(*ak, al, au, *ali, *aui, *ae),
                RangeParts::new(*bk, bl, bu, *bli, *bui, *be),
            )))
        }
        // Geometric `container @> point` / `point <@ container` for
        // polygon / box / circle, ahead of the array & JSON interpretations.
        BinOp::JsonContains if geo_contains_point(&l, &r).is_some() => Ok(Value::Bool(
            geo_contains_point(&l, &r).expect("guard checked"),
        )),
        BinOp::JsonContainedBy if geo_contains_point(&r, &l).is_some() => Ok(Value::Bool(
            geo_contains_point(&r, &l).expect("guard checked"),
        )),
        // v7.39 (read01 geo_ops.c part 2) — polygon⊃polygon and
        // circle⊃circle containment (both directions).
        BinOp::JsonContains if geo_contains_geo(&l, &r).is_some() => Ok(Value::Bool(
            geo_contains_geo(&l, &r).expect("guard checked"),
        )),
        BinOp::JsonContainedBy if geo_contains_geo(&r, &l).is_some() => Ok(Value::Bool(
            geo_contains_geo(&r, &l).expect("guard checked"),
        )),
        // v7.39 (read01 geo_ops.c part 2) — `point <@ lseg` / `point <@
        // path` (the `@>` spellings do not exist in PG, so only the
        // contained-by direction hooks here).
        BinOp::JsonContainedBy if geo_pt_on_object(&r, &l).is_some() => Ok(Value::Bool(
            geo_pt_on_object(&r, &l).expect("guard checked"),
        )),
        BinOp::JsonContains if geo_contains_box(&l, &r).is_some() => Ok(Value::Bool(
            geo_contains_box(&l, &r).expect("guard checked"),
        )),
        BinOp::JsonContainedBy if geo_contains_box(&r, &l).is_some() => Ok(Value::Bool(
            geo_contains_box(&r, &l).expect("guard checked"),
        )),
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
        // Geometric `box && box` / `circle && circle` overlap, ahead of the
        // array-overlap interpretation.
        BinOp::InetOverlap if geo_overlaps(&l, &r).is_some() => {
            Ok(Value::Bool(geo_overlaps(&l, &r).expect("guard checked")))
        }
        BinOp::InetOverlap
            if array_scalar_elems(&l).is_some() && array_scalar_elems(&r).is_some() =>
        {
            let (a, _) = array_scalar_elems(&l).expect("guard checked");
            let (b, _) = array_scalar_elems(&r).expect("guard checked");
            Ok(Value::Bool(a.iter().any(|x| b.iter().any(|y| x == y))))
        }
        BinOp::JsonContains => crate::json::contains(&l, &r),
        // v7.37 — `jsonb @? jsonpath` = jsonb_path_exists: does the path
        // return any item? Reuses the jsonpath query engine.
        BinOp::JsonPathExists => {
            if matches!(l, Value::Null) || matches!(r, Value::Null) {
                Ok(Value::Null)
            } else {
                // v7.39 (round 235) — `@?` SUPPRESSES a strict-mode
                // refusal and answers NULL, where jsonb_path_exists()
                // raises it. Probed against PG18.4.
                match crate::json::path_query(&l, &r) {
                    Err(_) => Ok(Value::Null),
                    Ok(Value::TextArray(items)) => Ok(Value::Bool(!items.is_empty())),
                    Ok(Value::Null) => Ok(Value::Null),
                    Ok(_) => Ok(Value::Bool(true)),
                }
            }
        }
        // v7.37.6-A `<@` reuses `@>` with swapped args.
        BinOp::JsonContainedBy => crate::json::contains(&r, &l),
        BinOp::JsonKeyExists => crate::json::key_exists(&l, &r),
        // v7.39 (read01 geo_ops.c part 2) — `point ?| point`: vertically
        // aligned (equal x under the geometric epsilon), ahead of the
        // JSONB keys-any interpretation.
        BinOp::JsonKeysAny if matches!(l, Value::Point(_)) && matches!(r, Value::Point(_)) => {
            let (Value::Point(a), Value::Point(b)) = (&l, &r) else {
                unreachable!()
            };
            Ok(Value::Bool((a.x - b.x).abs() <= 1.0e-6))
        }
        BinOp::JsonKeysAny => crate::json::keys_any(&l, &r),
        BinOp::JsonKeysAll => crate::json::keys_all(&l, &r),
        // v7.38 (read01, T8) — `jsonb @@ jsonpath` evaluates a boolean
        // predicate (`'{"a":5}' @@ '$.a > 3'`), distinct from tsvector `@@`.
        BinOp::TsMatch if matches!(l, Value::Json(_)) => {
            if matches!(r, Value::Null) {
                return Ok(Value::Null);
            }
            // v7.39 (round 235) — `@@` suppresses a strict-mode refusal
            // too (same PG rule as `@?`).
            match match crate::json::path_predicate(&l, &r) {
                Err(_) => return Ok(Value::Null),
                Ok(v) => v,
            } {
                Some(b) => Ok(Value::Bool(b)),
                None => match crate::json::path_query(&l, &r) {
                    Err(_) => Ok(Value::Null),
                    Ok(Value::TextArray(items)) => Ok(Value::Bool(match items.first() {
                        Some(Some(s)) if s == "false" => false,
                        Some(_) => true,
                        None => false,
                    })),
                    Ok(Value::Null) => Ok(Value::Null),
                    Ok(_) => Ok(Value::Bool(true)),
                },
            }
        }
        // v7.12.2 — `@@` match. NULL on either side → NULL; PG
        // accepts both orderings so we normalise.
        BinOp::TsMatch => ts_match(l, r),
        // v7.39 (read01 rangetypes.c) — range `&<` / `&>`.
        BinOp::OverLeft | BinOp::OverRight
            if matches!(l, Value::Range { .. }) && matches!(r, Value::Range { .. }) =>
        {
            let (Some(a), Some(b)) = (RangeParts::from_value(&l), RangeParts::from_value(&r))
            else {
                unreachable!("guard checked both are ranges")
            };
            Ok(Value::Bool(if matches!(op, BinOp::OverLeft) {
                range_overleft(a, b)
            } else {
                range_overright(a, b)
            }))
        }
        BinOp::OverLeft | BinOp::OverRight => Err(EvalError::TypeMismatch {
            detail: format!(
                "operator {} requires range operands",
                if matches!(op, BinOp::OverLeft) {
                    "&<"
                } else {
                    "&>"
                }
            ),
        }),
        // range `<<` / `>>` — strictly left / right of. Claims the operator
        // ahead of the bitstring / integer-shift / inet interpretations when
        // both operands are ranges.
        BinOp::InetContainedBy | BinOp::InetContains
            if matches!(l, Value::Range { .. }) && matches!(r, Value::Range { .. }) =>
        {
            let (
                Value::Range {
                    kind: ak,
                    lower: al,
                    upper: au,
                    lower_inc: ali,
                    upper_inc: aui,
                    empty: ae,
                },
                Value::Range {
                    kind: bk,
                    lower: bl,
                    upper: bu,
                    lower_inc: bli,
                    upper_inc: bui,
                    empty: be,
                },
            ) = (&l, &r)
            else {
                unreachable!()
            };
            // `a >> b` is `b << a`.
            let strictly_left = if matches!(op, BinOp::InetContainedBy) {
                range_strictly_left(
                    RangeParts::new(*ak, al, au, *ali, *aui, *ae),
                    RangeParts::new(*bk, bl, bu, *bli, *bui, *be),
                )
            } else {
                range_strictly_left(
                    RangeParts::new(*bk, bl, bu, *bli, *bui, *be),
                    RangeParts::new(*ak, al, au, *ali, *aui, *ae),
                )
            };
            Ok(Value::Bool(strictly_left))
        }
        // bit(n) << / >> k: shift within the fixed-width bit string. Claims
        // the operator ahead of the integer-shift and inet interpretations.
        BinOp::InetContainedBy | BinOp::InetContains
            if matches!(l, Value::BitString { .. }) && int_operand(&r).is_some() =>
        {
            let Value::BitString { nbits, bytes } = &l else {
                unreachable!()
            };
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
            } else if matches!(l, Value::SmallInt(_)) {
                // v7.38 (read01) — PG `int2 << / >> k` stays int2.
                small_int_result(shifted)
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
        BinOp::And | BinOp::Or | BinOp::LogicalXor
        | BinOp::IsDistinctFrom | BinOp::IsNotDistinctFrom => {
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
    // PG resolves `<temporal> - <unknown-type string literal>` to the
    // same-type difference (date-date→int days, ts-ts/time-time/interval-
    // interval→interval; `date - '5 days'` is a date-date parse error in PG,
    // not date-interval). Coerce the Text operand to the temporal operand's
    // type before the arms below. Only `-` is unambiguous this way — `+`
    // stays a "not unique" error, matching PG.
    if op == BinOp::Sub {
        let temporal_dt = |v: &Value| match v.data_type() {
            Some(
                dt @ (DataType::Date | DataType::Timestamp | DataType::Time | DataType::Interval),
            ) => Some(dt),
            _ => None,
        };
        if let (Value::Text(s), Some(dt)) = (l, temporal_dt(r)) {
            if let Ok(c) = crate::conversions::coerce_value(Value::text(s.as_ref()), dt, "", 0) {
                return apply_binary_calendar(op, &c, r);
            }
        }
        if let (Some(dt), Value::Text(s)) = (temporal_dt(l), r) {
            if let Ok(c) = crate::conversions::coerce_value(Value::text(s.as_ref()), dt, "", 0) {
                return apply_binary_calendar(op, l, &c);
            }
        }
    }
    // PG resolves `<time|interval|timestamp> + <unknown-type string literal>`
    // to `… + interval` (the only `+` those types have), so the literal is
    // coerced to INTERVAL: `'10:00'::time + '30 minutes'` → 10:30:00. `date +`
    // stays a "not unique" error (date+int and date+interval both exist),
    // matching PG. Symmetric in operand order.
    if op == BinOp::Add {
        let plus_interval = |v: &Value| {
            matches!(
                v.data_type(),
                Some(DataType::Time | DataType::Interval | DataType::Timestamp)
            )
        };
        if let (Value::Text(s), true) = (l, plus_interval(r)) {
            if let Ok(c) =
                crate::conversions::coerce_value(Value::text(s.as_ref()), DataType::Interval, "", 0)
            {
                return apply_binary_calendar(op, &c, r);
            }
        }
        if let (true, Value::Text(s)) = (plus_interval(l), r) {
            if let Ok(c) =
                crate::conversions::coerce_value(Value::text(s.as_ref()), DataType::Interval, "", 0)
            {
                return apply_binary_calendar(op, l, &c);
            }
        }
    }
    // Most-specific cases first — DATE-DATE / TS-TS subtraction before
    // DATE-integer subtraction, otherwise the latter swallows the
    // former with an `int_value(Date) = None` no-op fall-through.
    match (l, r) {
        (Value::Date(a), Value::Date(b)) if op == BinOp::Sub => {
            // PG: date - date → integer (int4) day count, not bigint.
            let days = i64::from(*a) - i64::from(*b);
            return i32::try_from(days).map(Value::Int).map(Some).map_err(|_| {
                EvalError::TypeMismatch {
                    detail: "DATE - DATE day count out of integer range".into(),
                }
            });
        }
        (Value::Timestamp(a), Value::Timestamp(b)) if op == BinOp::Sub => {
            // v7.39 (read01 timestamp.c) — like-signed infinities cannot
            // subtract (PG "interval out of range"); mixed-sign needs an
            // interval infinity SPG's Interval can't represent yet
            // (recorded delta) so it errors the same way.
            let a_inf = *a == i64::MAX || *a == i64::MIN;
            let b_inf = *b == i64::MAX || *b == i64::MIN;
            if a_inf || b_inf {
                return Err(EvalError::TypeMismatch {
                    detail: "interval out of range".into(),
                });
            }
            // PG: timestamp - timestamp -> interval, justified to hours (every
            // 24h of the microsecond delta becomes one day). 30h -> `1 day
            // 06:00:00`, not a raw microsecond count.
            let delta = a.checked_sub(*b).ok_or(EvalError::TypeMismatch {
                detail: "interval out of range".into(),
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
        // v7.37 D.35 — `date + time` (and `time + date`) → timestamp at that
        // date's midnight plus the time-of-day. PG: date '2024-06-15' + time
        // '10:30' → 2024-06-15 10:30:00.
        (Value::Date(d), Value::Time(t)) | (Value::Time(t), Value::Date(d)) if op == BinOp::Add => {
            let micros = i64::from(*d)
                .checked_mul(86_400_000_000)
                .and_then(|day_us| day_us.checked_add(*t))
                .ok_or(EvalError::TypeMismatch {
                    detail: "DATE + TIME overflows TIMESTAMP range".into(),
                })?;
            return Ok(Some(Value::Timestamp(micros)));
        }
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
            let iv = if matches!(l, Value::Interval { .. }) {
                l
            } else {
                r
            };
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
            // v7.38 (read01) — PG interval arithmetic is PURELY component-wise:
            // it does NOT justify days ↔ micros, so `1 day - 2 hours` stays
            // `1 day -02:00:00` (mixed sign) and `1 day - 26 hours` stays
            // `1 day -26:00:00` (micros beyond a day). Months, days and micros
            // each subtract independently; only explicit justify_* reshuffles
            // them. (An earlier fix normalised at the day boundary, mistaking
            // PG's `1 day -12:00:00` for an artefact — it is PG's real output.)
            let new_days = i32::try_from(raw_days).map_err(|_| EvalError::TypeMismatch {
                detail: "INTERVAL ± INTERVAL day count exceeds i32".into(),
            })?;
            Ok(Some(Value::Interval {
                months: new_months,
                days: new_days,
                micros: raw_micros,
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
    let micros_ok = (-9.3e18..=9.3e18).contains(&new_micros);
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
pub(crate) fn add_interval_to_micros(
    t: i64,
    months: i64,
    days: i64,
    micros: i64,
) -> Result<i64, EvalError> {
    const MICROS_PER_DAY: i64 = 86_400_000_000;
    // v7.39 (read01 timestamp.c) — an infinite timestamp absorbs any
    // finite interval (PG: infinity + '1 day' = infinity).
    if t == i64::MAX || t == i64::MIN {
        return Ok(t);
    }
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
    let out = out.checked_add(micros).ok_or(EvalError::TypeMismatch {
        detail: "timestamp out of range".into(),
    })?;
    // v7.39 (read01 timestamp.c) — PG's lower bound (4714-11-24 BC,
    // Unix-epoch microseconds); arithmetic below it errors like PG.
    // The upper bound is i64 itself (checked adds above) — SPG's
    // 1970-based clock ends ~30 years before PG's 2000-based ceiling,
    // a recorded delta at the extreme fringe.
    const TS_MIN: i64 = -210_866_803_200_000_000;
    if out < TS_MIN {
        return Err(EvalError::TypeMismatch {
            detail: "timestamp out of range".into(),
        });
    }
    Ok(out)
}

/// Dispatch for any binary op when at least one operand is NUMERIC.
/// Other-side integers / floats are promoted to a NUMERIC at a common
/// scale; all add / sub / mul / div / compare paths stay in i128.
#[allow(clippy::needless_pass_by_value)] // mirrors `apply_binary`'s by-value calling convention
/// v7.38 (read01) — intersection point of two closed line segments, or `None`
/// when they are parallel or do not overlap within both segments' extents.
fn lseg_intersection(
    a1: spg_storage::Point2D,
    a2: spg_storage::Point2D,
    b1: spg_storage::Point2D,
    b2: spg_storage::Point2D,
) -> Option<spg_storage::Point2D> {
    let d1x = a2.x - a1.x;
    let d1y = a2.y - a1.y;
    let d2x = b2.x - b1.x;
    let d2y = b2.y - b1.y;
    let denom = d1x * d2y - d1y * d2x;
    if denom == 0.0 {
        return None; // parallel or degenerate
    }
    let t = ((b1.x - a1.x) * d2y - (b1.y - a1.y) * d2x) / denom;
    let s = ((b1.x - a1.x) * d1y - (b1.y - a1.y) * d1x) / denom;
    if (0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&s) {
        Some(spg_storage::Point2D {
            x: a1.x + t * d1x,
            y: a1.y + t * d1y,
        })
    } else {
        None
    }
}

/// v7.38 (read01, T6.P3) — the NUMERIC kind of a numeric-family operand, or None
/// for a non-numeric value (which falls through to the ordinary type path).
fn numeric_kind_of(v: &Value) -> Option<spg_storage::NumericKind> {
    use spg_storage::NumericKind::Finite;
    match v {
        Value::Numeric { kind, .. } => Some(*kind),
        Value::Int(_) | Value::BigInt(_) | Value::SmallInt(_) => Some(Finite),
        _ => None,
    }
}

fn finite_sign_of(v: &Value) -> i32 {
    match v {
        Value::Numeric { scaled, .. } => (*scaled).signum() as i32,
        Value::Int(n) => n.signum(),
        Value::BigInt(n) => (*n).signum() as i32,
        Value::SmallInt(n) => i32::from(*n).signum(),
        _ => 0,
    }
}

/// v7.38 (read01, T6.P3) — PG's NUMERIC special-value arithmetic. Returns
/// `Some` when a NaN/±Infinity operand determines the result (verified vs PG:
/// NaN wins over everything incl. div-by-zero; `Inf/0` still errors; `Inf-Inf` /
/// `Inf*0` / `Inf/Inf` → NaN; `finite/Inf` → 0; `finite%Inf` → the dividend;
/// `Inf%x` → NaN; sign multiplies). `None` falls through to the finite path.
fn numeric_special_result(
    op: BinOp,
    l: &Value<'static>,
    r: &Value<'static>,
) -> Option<Result<Value<'static>, EvalError>> {
    use spg_storage::NumericKind as NK;
    let (lk, rk) = (numeric_kind_of(l)?, numeric_kind_of(r)?);
    if lk == NK::Finite && rk == NK::Finite {
        return None;
    }
    if !matches!(
        op,
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod
    ) {
        return None;
    }
    let nan = || Ok(Value::numeric_special(NK::NaN));
    // NaN wins over everything, including division by zero.
    if lk == NK::NaN || rk == NK::NaN {
        return Some(nan());
    }
    // After NaN: a zero divisor still errors (PG: `Infinity / 0` errors too).
    let r_is_zero = rk == NK::Finite && finite_sign_of(r) == 0;
    if matches!(op, BinOp::Div | BinOp::Mod) && r_is_zero {
        return Some(Err(EvalError::DivisionByZero));
    }
    let sign = |k: NK, v: &Value<'static>| -> i32 {
        match k {
            NK::PosInf => 1,
            NK::NegInf => -1,
            _ => finite_sign_of(v),
        }
    };
    let (ls, rs) = (sign(lk, l), sign(rk, r));
    let (l_inf, r_inf) = (
        matches!(lk, NK::PosInf | NK::NegInf),
        matches!(rk, NK::PosInf | NK::NegInf),
    );
    let inf = |s: i32| Value::numeric_special(if s < 0 { NK::NegInf } else { NK::PosInf });
    let l_zero = lk == NK::Finite && ls == 0;
    let res = match op {
        BinOp::Add => {
            if l_inf && r_inf {
                if ls == rs {
                    inf(ls)
                } else {
                    return Some(nan());
                }
            } else if l_inf {
                inf(ls)
            } else {
                inf(rs)
            }
        }
        BinOp::Sub => {
            let rs2 = -rs;
            if l_inf && r_inf {
                if ls == rs2 {
                    inf(ls)
                } else {
                    return Some(nan());
                }
            } else if l_inf {
                inf(ls)
            } else {
                inf(rs2)
            }
        }
        BinOp::Mul => {
            if (l_inf && r_is_zero) || (r_inf && l_zero) {
                return Some(nan());
            }
            inf(ls * rs)
        }
        BinOp::Div => {
            if l_inf && r_inf {
                return Some(nan());
            } else if l_inf {
                inf(ls * rs)
            } else {
                Value::numeric(0, 0) // finite / Inf → 0
            }
        }
        BinOp::Mod => {
            if l_inf {
                return Some(nan()); // Inf % x → NaN
            }
            l.clone().into_owned() // finite % Inf → the dividend
        }
        _ => unreachable!(),
    };
    Some(Ok(res))
}

/// v7.38 (read01, T3.C3) — convert a numeric-family value to `BigNumeric` for
/// the arbitrary-precision fallback. `None` for a non-numeric.
pub(crate) fn value_to_bignum(v: &Value) -> Option<spg_storage::bignum::BigNumeric> {
    use spg_storage::bignum::BigNumeric;
    Some(match v {
        Value::SmallInt(n) => BigNumeric::from_i128(i128::from(*n), 0),
        Value::Int(n) => BigNumeric::from_i128(i128::from(*n), 0),
        Value::BigInt(n) => BigNumeric::from_i128(i128::from(*n), 0),
        Value::Numeric {
            scaled,
            scale,
            kind,
        } if *kind == spg_storage::NumericKind::Finite => BigNumeric::from_i128(*scaled, *scale),
        Value::NumericBig(b) => (**b).clone(),
        _ => return None,
    })
}

/// Collapse a `BigNumeric` back to `Value::Numeric` when its mantissa fits
/// `i128`, else keep the big form.
pub(crate) fn bignum_to_value(b: spg_storage::bignum::BigNumeric) -> Value<'static> {
    match b.to_i128() {
        Some(scaled) => Value::Numeric {
            scaled,
            scale: b.scale(),
            kind: spg_storage::NumericKind::Finite,
        },
        None => Value::NumericBig(alloc::boxed::Box::new(b)),
    }
}

/// v7.38 (read01, T3.C3) — arbitrary-precision fallback for a numeric op whose
/// i128 fast path overflowed, or where an operand is already `NumericBig`.
fn numeric_big_op(op: BinOp, l: &Value, r: &Value) -> Option<Result<Value<'static>, EvalError>> {
    let (a, b) = (value_to_bignum(l)?, value_to_bignum(r)?);
    // Comparison honors the exact big values (sign + scale-aligned magnitude).
    if matches!(
        op,
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq
    ) {
        return Some(Ok(Value::Bool(cmp_to_bool(op, a.cmp(&b)))));
    }
    // Division carries PG's display scale (~16 significant digits, floored by
    // the dividend's scale); a zero divisor errors like the i128 path.
    if matches!(op, BinOp::Div) {
        if b.is_zero() {
            return Some(Err(EvalError::DivisionByZero));
        }
        let rscale = crate::numeric::division_display_scale_big(&a, &b);
        return Some(Ok({
            let q = a.div(&b, rscale)?;
            bignum_to_value(q)
        }));
    }
    // Modulo of two big integers (scale 0) via truncating integer div_rem;
    // a scaled big remainder is rare enough to leave to the type error.
    if matches!(op, BinOp::Mod) {
        if b.is_zero() {
            return Some(Err(EvalError::DivisionByZero));
        }
        if a.scale() == 0 && b.scale() == 0 {
            let (_, rem) = a.div_rem_int(&b);
            return Some(Ok(bignum_to_value(rem)));
        }
        return None;
    }
    let res = match op {
        BinOp::Add => a.add(&b),
        BinOp::Sub => a.sub(&b),
        BinOp::Mul => a.mul(&b),
        _ => return None,
    };
    Some(Ok(bignum_to_value(res)))
}

fn apply_binary_numeric(
    op: BinOp,
    l: Value<'static>,
    r: Value<'static>,
) -> Result<Value<'static>, EvalError> {
    // v7.39 (round 353, M9) — `DIV` reads a NUMERIC operand as a number
    // like every other one (`7.5 DIV 2` is 3, measured); it has no
    // fixed-point form of its own to compute here.
    if matches!(op, BinOp::IntDiv) {
        return int_div_op(&l, &r);
    }
    // v7.38 (read01, T3.C3) — an already-big operand skips the i128 fast path.
    if matches!(l, Value::NumericBig(_)) || matches!(r, Value::NumericBig(_)) {
        if let Some(res) = numeric_big_op(op, &l, &r) {
            return res;
        }
    }
    // v7.38 (read01) — an unknown-type string operand against NUMERIC coerces to
    // numeric (PG): `3.5::numeric = '3.5'`, `n <op> '2'`. Do it up front so both
    // the comparison and arithmetic paths see two numerics; a string that isn't
    // a valid numeric falls through to the type error (matching PG `n = 'abc'`).
    // Concat is the exception — `numeric || text` is text concatenation.
    if !matches!(op, BinOp::Concat) {
        let to_num = |v: &Value<'static>| -> Option<Value<'static>> {
            match v {
                Value::Text(s) => crate::conversions::coerce_value(
                    Value::text(s.as_ref()),
                    spg_storage::DataType::Numeric {
                        precision: 0,
                        scale: 0,
                    },
                    "",
                    0,
                )
                .ok(),
                _ => None,
            }
        };
        if let Some(nl) = to_num(&l) {
            return apply_binary_numeric(op, nl, r);
        }
        if let Some(nr) = to_num(&r) {
            return apply_binary_numeric(op, l, nr);
        }
    }
    // v7.38 (read01, T6.P3) — a NaN / ±Infinity operand determines the result
    // before the finite fixed-point path (arithmetic ops only; comparison of
    // specials is handled in the compare arm / cmp_numeric).
    if let Some(res) = numeric_special_result(op, &l, &r) {
        return res;
    }
    // Float still wins — Numeric + Float coerces both to f64 and runs
    // through the float path. PG demotes Numeric to float in this mix
    // too (the documented behaviour for `numeric + double precision`).
    // v7.38 (read01, T-float4) — `real op {real,int}` stays real (compute in
    // f64, narrow to f32); a float8 / numeric operand widens to the float path.
    let has_real = matches!(l, Value::Real(_)) || matches!(r, Value::Real(_));
    let has_float = matches!(l, Value::Float(_)) || matches!(r, Value::Float(_));
    let has_numeric = matches!(l, Value::Numeric { .. } | Value::NumericBig(_))
        || matches!(r, Value::Numeric { .. } | Value::NumericBig(_));
    let both_real = matches!(l, Value::Real(_)) && matches!(r, Value::Real(_));
    if both_real
        && matches!(
            op,
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod
        )
    {
        let af = as_f64(&l)?;
        let bf = as_f64(&r)?;
        return Ok(Value::Real(match op {
            BinOp::Add => (af + bf) as f32,
            BinOp::Sub => (af - bf) as f32,
            BinOp::Mul => (af * bf) as f32,
            BinOp::Div => {
                if bf == 0.0 {
                    return Err(EvalError::DivisionByZero);
                }
                (af / bf) as f32
            }
            BinOp::Mod => {
                if bf == 0.0 {
                    return Err(EvalError::DivisionByZero);
                }
                (af % bf) as f32
            }
            _ => unreachable!(),
        }));
    }
    let float_path = has_float || has_real;
    if float_path {
        let af = as_f64(&l)?;
        let bf = as_f64(&r)?;
        return match op {
            BinOp::Add => Ok(Value::Float(check_float8_range(af + bf, af, bf, "+")?)),
            BinOp::Sub => Ok(Value::Float(check_float8_range(af - bf, af, bf, "-")?)),
            BinOp::Mul => Ok(Value::Float(check_float8_range(af * bf, af, bf, "*")?)),
            BinOp::Div => {
                if bf == 0.0 {
                    Err(EvalError::DivisionByZero)
                } else {
                    Ok(Value::Float(check_float8_range(af / bf, af, bf, "/")?))
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
    // v7.37 D.34 — `numeric || anything` / `anything || numeric` is text
    // concatenation (PG has no `||` operator on numeric); handle it before
    // numeric_or_widen, which rejects a non-numeric operand. Previously
    // `'x' || 1.5::numeric` errored "NUMERIC op against non-numeric Text"
    // because the numeric fast-path claimed the expression but couldn't widen
    // the text side. (The float path already special-cases Concat this way.)
    if matches!(op, BinOp::Concat) {
        return Ok(text_concat(&l, &r));
    }
    // v7.38 (read01, T6.P3) — comparison involving a NUMERIC special uses the
    // total order -Inf < finite < +Inf < NaN (NaN == NaN). Handled before the
    // finite widen, which would read a special's canonical 0 as the number 0.
    if matches!(
        op,
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq
    ) {
        use spg_storage::NumericKind as NK;
        if let (Some(lk), Some(rk)) = (numeric_kind_of(&l), numeric_kind_of(&r)) {
            if lk != NK::Finite || rk != NK::Finite {
                let rank = |k: NK| match k {
                    NK::NegInf => -2,
                    NK::Finite => 0,
                    NK::PosInf => 1,
                    NK::NaN => 2,
                };
                // At least one side is special, so the ranks alone order the pair
                // (two finites never reach here).
                let ord = rank(lk).cmp(&rank(rk));
                return Ok(Value::Bool(cmp_to_bool(op, ord)));
            }
        }
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
            // v7.38 (read01, T3.C3) — try the exact i128 path; any overflow
            // (rescale or the add/sub) promotes to arbitrary precision.
            let fast = rescale(a, sa, target_scale).and_then(|lhs| {
                rescale(b, sb, target_scale).and_then(|rhs| match op {
                    BinOp::Add => lhs.checked_add(rhs),
                    BinOp::Sub => lhs.checked_sub(rhs),
                    _ => unreachable!(),
                })
            });
            match fast {
                Some(r) => Ok(Value::Numeric {
                    scaled: r,
                    scale: target_scale,
                    kind: spg_storage::NumericKind::Finite,
                }),
                None => numeric_big_op(op, &l, &r).expect("numeric operands"),
            }
        }
        BinOp::Mod => {
            // PG `numeric % numeric`: rescale both to the shared scale, then
            // take the truncated remainder (sign of the dividend). Result keeps
            // that scale. 12.5 % 5 -> 125 % 50 = 25 @scale1 = 2.5.
            let target_scale = sa.max(sb);
            let lhs = rescale(a, sa, target_scale).ok_or(EvalError::TypeMismatch {
                detail: "NUMERIC overflow on rescale".into(),
            })?;
            let rhs = rescale(b, sb, target_scale).ok_or(EvalError::TypeMismatch {
                detail: "NUMERIC overflow on rescale".into(),
            })?;
            if rhs == 0 {
                return Err(EvalError::DivisionByZero);
            }
            Ok(Value::Numeric {
                scaled: lhs.wrapping_rem(rhs),
                scale: target_scale,
                kind: spg_storage::NumericKind::Finite,
            })
        }
        BinOp::Mul => {
            // v7.38 (read01, T3.C3) — i128 product overflow promotes to bignum.
            match a.checked_mul(b) {
                Some(scaled) => Ok(Value::Numeric {
                    scaled,
                    scale: sa.saturating_add(sb),
                    kind: spg_storage::NumericKind::Finite,
                }),
                None => numeric_big_op(op, &l, &r).expect("numeric operands"),
            }
        }
        BinOp::Div => {
            if b == 0 {
                return Err(EvalError::DivisionByZero);
            }
            // `numeric / numeric` picks the result scale from
            // `division_display_scale`, which keeps ~16 significant digits
            // (matching PG's observable division scale), so `10::numeric / 3`
            // yields 16 fractional digits (3.3333333333333333) rather than
            // truncating to the operands' scale-0 (which silently gave `3`).
            // v7.38 (read01, T3.C3) — when the scaled division overflows i128
            // (large dividend × 10^rscale), promote to the arbitrary-precision
            // path rather than erroring.
            match crate::numeric::numeric_div(a, sa, b, sb) {
                Some((scaled, scale)) => Ok(Value::Numeric {
                    scaled,
                    scale,
                    kind: spg_storage::NumericKind::Finite,
                }),
                None => numeric_big_op(op, &l, &r).expect("numeric operands"),
            }
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
fn numeric_or_widen(v: &Value<'static>) -> Option<(i128, u16)> {
    match v {
        Value::Numeric { scaled, scale, .. } => Some((*scaled, *scale)),
        Value::Int(n) => Some((i128::from(*n), 0)),
        Value::SmallInt(n) => Some((i128::from(*n), 0)),
        Value::BigInt(n) => Some((i128::from(*n), 0)),
        _ => None,
    }
}

fn rescale(scaled: i128, src: u16, dst: u16) -> Option<i128> {
    if src == dst {
        return Some(scaled);
    }
    // v7.39 (round 271) — with `scale` widened to u16 the power can
    // exceed i128. Returning None here drops the caller onto the
    // BigNumeric path it already has; the old unchecked `pow10_i128`
    // panicked and aborted the query.
    if dst > src {
        scaled.checked_mul(pow10_i128_checked(dst - src)?)
    } else {
        let drop = pow10_i128_checked(src - dst)?;
        let half = drop / 2;
        let r = if scaled >= 0 {
            scaled + half
        } else {
            scaled - half
        };
        Some(r / drop)
    }
}

pub(super) const fn pow10_i128(p: u16) -> i128 {
    match pow10_i128_checked(p) {
        Some(v) => v,
        None => i128::MAX,
    }
}

/// `10^p`, or None once it leaves the i128 range.
const fn pow10_i128_checked(p: u16) -> Option<i128> {
    let mut acc: i128 = 1;
    let mut i = 0;
    while i < p {
        match acc.checked_mul(10) {
            Some(v) => acc = v,
            None => return None,
        }
        i += 1;
    }
    Some(acc)
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
/// Concatenate two MSB-packed bit strings (`bit || bit`): copy `a`'s
/// `an` bits, then append `b`'s `bn` bits starting at bit offset `an`.
/// Returns `(total_bits, packed_bytes)`.
fn bit_concat(an: u32, ab: &[u8], bn: u32, bb: &[u8]) -> (u32, alloc::vec::Vec<u8>) {
    let total = an + bn;
    let mut out = alloc::vec![0u8; ((total + 7) / 8) as usize];
    let get = |src: &[u8], i: u32| -> bool {
        src.get((i / 8) as usize)
            .is_some_and(|&byte| byte & (0x80 >> (i % 8)) != 0)
    };
    for i in 0..an {
        if get(ab, i) {
            out[(i / 8) as usize] |= 0x80 >> (i % 8);
        }
    }
    for i in 0..bn {
        if get(bb, i) {
            let p = an + i;
            out[(p / 8) as usize] |= 0x80 >> (p % 8);
        }
    }
    (total, out)
}

fn text_concat<'a>(l: &'a Value<'static>, r: &'a Value<'static>) -> Value<'static> {
    // v7.38 (read01, T11) — bpchar concatenates with its trailing blanks
    // removed (`'ab'::char(4) || 'x'` = `abx`).
    if matches!(l, Value::BpChar(_)) || matches!(r, Value::BpChar(_)) {
        let norm = |v: &Value<'static>| -> Value<'static> {
            match v {
                Value::BpChar(s) => Value::text(s.trim_end_matches(' ').to_string()),
                other => other.clone(),
            }
        };
        return text_concat(&norm(l), &norm(r));
    }
    if let (Value::TsVector(a), Value::TsVector(b)) = (l, r) {
        return tsvector_concat(a, b);
    }
    // PG `bit || bit` = a `bit varying` of length nbits(a)+nbits(b). Both
    // operands are MSB-packed and zero-padded, so the second string's bits
    // must be shifted to start at bit offset nbits(a), not byte-appended.
    if let (
        Value::BitString {
            nbits: an,
            bytes: ab,
        },
        Value::BitString {
            nbits: bn,
            bytes: bb,
        },
    ) = (l, r)
    {
        let (total, out) = bit_concat(*an, ab, *bn, bb);
        return Value::BitString {
            nbits: total,
            bytes: alloc::borrow::Cow::Owned(out),
        };
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
        // v7.39 (read01 round 108) — 2-D array `||` appends the second matrix's
        // rows to the first (`ARRAY[[1,2]] || ARRAY[[3,4]]` → `{{1,2},{3,4}}`).
        // Without these arms two matrices fell through to text_concat, giving
        // the malformed `{{1,2}}{{3,4}}`.
        (Value::IntArray2D(a), Value::IntArray2D(b)) => {
            let mut out = a.clone();
            out.extend(b.iter().cloned());
            return Value::IntArray2D(out);
        }
        (Value::BigIntArray2D(a), Value::BigIntArray2D(b)) => {
            let mut out = a.clone();
            out.extend(b.iter().cloned());
            return Value::BigIntArray2D(out);
        }
        (Value::TextArray2D(a), Value::TextArray2D(b)) => {
            let mut out = a.clone();
            out.extend(b.iter().cloned());
            return Value::TextArray2D(out);
        }
        (Value::BoolArray2D(a), Value::BoolArray2D(b)) => {
            let mut out = a.clone();
            out.extend(b.iter().cloned());
            return Value::BoolArray2D(out);
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
    // v7.39 (round 608) — `s || 'x'` allocated five times a row over 200k
    // rows where `upper(s)`, which also builds a new string, allocated one.
    // Two of those were `value_to_text` copying operands that already WERE
    // their text, and a third was the concatenation growing into them.
    // Borrow what is already text, and size the result once.
    let borrow = |v: &'a Value<'static>| -> Option<&'a str> {
        match v {
            Value::Text(s) | Value::Json(s) => Some(s.as_ref()),
            _ => None,
        }
    };
    match (borrow(l), borrow(r)) {
        (Some(a), Some(b)) => {
            let mut out = alloc::string::String::with_capacity(a.len() + b.len());
            out.push_str(a);
            out.push_str(b);
            Value::text(out)
        }
        (Some(a), None) => {
            let b = value_to_text(r);
            let mut out = alloc::string::String::with_capacity(a.len() + b.len());
            out.push_str(a);
            out.push_str(&b);
            Value::text(out)
        }
        (None, Some(b)) => {
            let a = value_to_text(l);
            let mut out = alloc::string::String::with_capacity(a.len() + b.len());
            out.push_str(&a);
            out.push_str(b);
            Value::text(out)
        }
        (None, None) => {
            let a = value_to_text(l);
            let b = value_to_text(r);
            let mut out = alloc::string::String::with_capacity(a.len() + b.len());
            out.push_str(&a);
            out.push_str(&b);
            Value::text(out)
        }
    }
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
        let src = if left {
            i.checked_add(k)
        } else {
            i.checked_sub(k)
        };
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
    if let (
        Value::BitString {
            nbits: an,
            bytes: ab,
        },
        Value::BitString {
            nbits: bn,
            bytes: bb,
        },
    ) = (&l, &r)
    {
        if an != bn {
            // v7.39 (read01 varbit.c) — PG spells the operator out.
            let word = match op_name {
                "&" => "AND",
                "|" => "OR",
                "#" => "XOR",
                other => other,
            };
            return Err(EvalError::TypeMismatch {
                detail: format!("cannot {word} bit strings of different sizes"),
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
    // PG `macaddr & / | macaddr`: byte-wise over the six address octets.
    if let (Value::Macaddr(a), Value::Macaddr(b)) = (&l, &r) {
        let mut out = [0u8; 6];
        for i in 0..6 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                out[i] = f(i64::from(a[i]), i64::from(b[i])) as u8;
            }
        }
        return Ok(Value::Macaddr(out));
    }
    // PG `macaddr8 & / | macaddr8`: byte-wise over the eight EUI-64 octets.
    if let (Value::Macaddr8(a), Value::Macaddr8(b)) = (&l, &r) {
        let mut out = [0u8; 8];
        for i in 0..8 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                out[i] = f(i64::from(a[i]), i64::from(b[i])) as u8;
            }
        }
        return Ok(Value::Macaddr8(out));
    }
    // v7.38 (read01) — PG `int2 & | # int2` stays int2 (a bitwise result of
    // two int2 always fits int2). Mixed widths widen as before.
    if let (Value::SmallInt(a), Value::SmallInt(b)) = (&l, &r) {
        return small_int_result(f(i64::from(*a), i64::from(*b)));
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

/// PG's float8 range guard (utils/adt/float.c `check_float8_val`): a
/// finite ⊕ finite operation that overflows to ±Infinity is an error,
/// not a silent Inf; a multiply/divide whose non-zero operands underflow
/// to exactly 0 is an error too. Inf/NaN operands (whose Inf result is
/// legitimate) and additive cancellation to 0 pass through. Learned from
/// read01 float.c study — a silent Inf is exactly the kind of quiet
/// wrong answer SPG refuses to emit.
fn check_float8_range(result: f64, a: f64, b: f64, op_name: &str) -> Result<f64, EvalError> {
    if result.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        return Err(EvalError::TypeMismatch {
            detail: "value out of range: overflow".into(),
        });
    }
    if (op_name == "*" || op_name == "/") && result == 0.0 && a != 0.0 && b != 0.0 {
        return Err(EvalError::TypeMismatch {
            detail: "value out of range: underflow".into(),
        });
    }
    Ok(result)
}

/// v7.38 (read01) — narrow an i64 result of two int2 operands back to int2,
/// erroring "smallint out of range" on overflow. PG keeps int2 arithmetic and
/// bitwise ops in int2; this is the shared narrowing all those sites use.
fn small_int_result(res: i64) -> Result<Value<'static>, EvalError> {
    i16::try_from(res)
        .map(Value::SmallInt)
        .map_err(|_| EvalError::TypeMismatch {
            detail: "smallint out of range".into(),
        })
}

fn arith(
    l: Value<'static>,
    r: Value<'static>,
    int_op: impl Fn(i64, i64) -> Option<i64>,
    float_op: impl Fn(f64, f64) -> f64,
    op_name: &str,
) -> Result<Value<'static>, EvalError> {
    // v7.38 (read01) — PG keeps int2 <op> int2 in int2 (widening only when
    // mixed with int4/int8), and a result outside int2 is "smallint out of
    // range", not a silent widen. Handle the both-small case before widening.
    if let (Value::SmallInt(a), Value::SmallInt(b)) = (&l, &r) {
        let res = int_op(i64::from(*a), i64::from(*b)).ok_or(EvalError::TypeMismatch {
            detail: format!("smallint overflow on {op_name}"),
        })?;
        return small_int_result(res);
    }
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
                detail: alloc::string::String::from("bigint out of range"),
            })?;
            Ok(Value::BigInt(result))
        }
        (Value::BigInt(a), Value::Int(b)) => {
            let result = int_op(a, i64::from(b)).ok_or(EvalError::TypeMismatch {
                detail: alloc::string::String::from("bigint out of range"),
            })?;
            Ok(Value::BigInt(result))
        }
        (Value::BigInt(a), Value::BigInt(b)) => {
            let result = int_op(a, b).ok_or(EvalError::TypeMismatch {
                detail: alloc::string::String::from("bigint out of range"),
            })?;
            Ok(Value::BigInt(result))
        }
        (a, b)
            if a.data_type() == Some(DataType::Float) || b.data_type() == Some(DataType::Float) =>
        {
            let af = as_f64(&a)?;
            let bf = as_f64(&b)?;
            Ok(Value::Float(check_float8_range(
                float_op(af, bf),
                af,
                bf,
                op_name,
            )?))
        }
        // v7.39 (round 238) — PG's wording: "operator does not exist:
        // integer + text". SPG printed `+ applied to non-numeric:
        // Some(Int) vs Some(Text)` — a Rust Debug dump of an internal enum,
        // and nothing a driver can match on.
        (a, b) => Err(EvalError::TypeMismatch {
            detail: format!(
                "operator does not exist: {} {op_name} {}",
                super::strings::pg_typeof_name(&a),
                super::strings::pg_typeof_name(&b)
            ),
        }),
    }
}

/// v7.39 (round 351, M11) — `apply_binary` with the session dialect.
///
/// It lives HERE, at the leaf, and not as an arm of `eval_expr`: that
/// function is the recursive frame the 768 KiB stack budget is tuned
/// against, and the guard test refused three separate shapes that added
/// locals to it (the round-305 frame cliff, twice in one round). A leaf
/// costs one frame, once.
pub(crate) fn apply_binary_in(
    op: BinOp,
    l: Value<'static>,
    r: Value<'static>,
    mysql: bool,
) -> Result<Value<'static>, EvalError> {
    if !mysql {
        return apply_binary(op, l, r);
    }
    // Date ± date-granular interval keeps a DATE — checked on the ORIGINAL
    // operands, before the operand-reading pair lifts a bare date string
    // to a midnight timestamp.
    if let Some(v) = mysql_date_plus_interval(op, &l, &r) {
        return Ok(v);
    }
    // v7.39 (round 393) — a STRING operand makes `/` a DOUBLE (`'10'/'4'`
    // is 2.5, not the DECIMAL 2.5000 that `10/4` is), checked on the
    // ORIGINAL operands before the reading pair lifts a string to a number.
    let div_text_operand = matches!(op, BinOp::Div)
        && (matches!(l, Value::Text(_)) || matches!(r, Value::Text(_)));
    let (l, r) = super::mysql_operand_reading_pair(op, l, r);
    if let Some(v) = super::mysql_true_division(op, &l, &r, div_text_operand) {
        return Ok(v);
    }
    // v7.39 (round 383) — `& | ^ << >>` are UNSIGNED 64-bit here, and
    // `^` reaches this path as BitXor (the parser maps `^` to XOR under
    // the MySQL dialect, not PG's `power`).
    if let Some(v) = super::mysql_bitwise(op, &l, &r) {
        return Ok(v);
    }
    apply_binary(op, l, r)
}

/// The DATE (days since the epoch) a `date ± INTERVAL` operand carries: a
/// `Value::Date`, or a bare date string with no time part.
fn date_operand_days(v: &Value<'_>) -> Option<i32> {
    match v {
        Value::Date(d) => Some(*d),
        Value::Text(s) if !s.contains(':') => crate::eval::parse_date_literal(s),
        // v7.39 (round 414) — only reached from `mysql_date_plus_interval`
        // (an out-of-line, MySQL-only step), so an integer here is MySQL's
        // YYYYMMDD / YYMMDD form. PG never reaches this arm.
        Value::SmallInt(n) => mysql_int_as_date_days(i64::from(*n)),
        Value::Int(n) => mysql_int_as_date_days(i64::from(*n)),
        Value::BigInt(n) => mysql_int_as_date_days(*n),
        _ => None,
    }
}

/// v7.39 (round 414) — read a MySQL YYYYMMDD (8) / YYMMDD (6) integer as
/// day offset. Bigger int shapes carry a time component and lift the result
/// to DATETIME, which is out of scope here — `mysql_date_plus_interval`
/// falls through and the operator errors, matching MariaDB's own refusal.
fn mysql_int_as_date_days(n: i64) -> Option<i32> {
    if n < 0 {
        return None;
    }
    let (y, mo, d) = match n {
        10_000_000..=99_999_999 => (
            (n / 10_000) as i32,
            ((n / 100) % 100) as u32,
            (n % 100) as u32,
        ),
        100_000..=999_999 => {
            let yy = (n / 10_000) as i32;
            let y = if yy < 70 { 2000 + yy } else { 1900 + yy };
            (y, ((n / 100) % 100) as u32, (n % 100) as u32)
        }
        _ => return None,
    };
    let text = alloc::format!("{y:04}-{mo:02}-{d:02}");
    crate::eval::parse_date_literal(&text)
}

/// v7.39 (round 373) — `date ± INTERVAL` keeps a DATE result under the
/// MySQL dialect when the interval has no time component (YEAR / MONTH /
/// DAY / WEEK): `DATE_ADD('2020-01-31', INTERVAL 1 MONTH)` is DATE
/// 2020-02-29 on MariaDB, not the midnight DATETIME PG produces. An
/// interval carrying a sub-day part (HOUR / MINUTE / SECOND) still lifts
/// to DATETIME, so this returns None for it and the PG path runs.
fn mysql_date_plus_interval(op: BinOp, l: &Value<'_>, r: &Value<'_>) -> Option<Value<'static>> {
    let iv = |v: &Value<'_>| match v {
        Value::Interval {
            months,
            days,
            micros,
        } => Some((*months, *days, *micros)),
        _ => None,
    };
    let (date, months, days, micros, sign) = match op {
        BinOp::Add => {
            if let (Some(d), Some((m, dd, us))) = (date_operand_days(l), iv(r)) {
                (d, m, dd, us, 1i64)
            } else if let (Some((m, dd, us)), Some(d)) = (iv(l), date_operand_days(r)) {
                (d, m, dd, us, 1i64)
            } else {
                return None;
            }
        }
        BinOp::Sub => {
            if let (Some(d), Some((m, dd, us))) = (date_operand_days(l), iv(r)) {
                (d, m, dd, us, -1i64)
            } else {
                return None;
            }
        }
        _ => return None,
    };
    if micros != 0 {
        return None; // a time component lifts the result to DATETIME
    }
    let base = i64::from(date).checked_mul(86_400_000_000)?;
    let res = add_interval_to_micros(base, i64::from(months) * sign, i64::from(days) * sign, 0).ok()?;
    let day = res.div_euclid(86_400_000_000);
    Some(Value::Date(i32::try_from(day).ok()?))
}

/// v7.39 (round 353, M9) — `a DIV b`.
fn int_div_op(l: &Value<'_>, r: &Value<'_>) -> Result<Value<'static>, EvalError> {
    let num = |v: &Value<'_>| -> Option<f64> {
        Some(match v {
            Value::SmallInt(n) => f64::from(*n),
            Value::Int(n) => f64::from(*n),
            #[allow(clippy::cast_precision_loss)]
            Value::BigInt(n) => *n as f64,
            Value::Float(f) => *f,
            Value::Real(f) => f64::from(*f),
            #[allow(clippy::cast_precision_loss)]
            Value::Numeric { scaled, scale, .. } => {
                *scaled as f64 / 10_f64.powi(i32::from(*scale))
            }
            Value::Text(t) | Value::BpChar(t) => crate::eval::mysql_leading_number(t),
            Value::Null => return None,
            _ => return None,
        })
    };
    let (Some(a), Some(b)) = (num(l), num(r)) else {
        return Ok(Value::Null);
    };
    if b == 0.0 {
        return Ok(Value::Null);
    }
    #[allow(clippy::cast_possible_truncation)]
    Ok(Value::BigInt((a / b).trunc() as i64))
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
    // PG `point <-> {lseg|box|circle}` (either order) — distance from the
    // point to the nearest point of the geometry.
    if let Some(d) = point_geo_distance(&l, &r).or_else(|| point_geo_distance(&r, &l)) {
        return Ok(Value::Float(d));
    }
    // PG `box <-> box` is the distance between the two box centres (verified
    // vs PG18.4: overlapping boxes still return the centre distance, not 0).
    if let (Value::PgBox(aur, all), Value::PgBox(bur, bll)) = (&l, &r) {
        let acx = (aur.x + all.x) / 2.0;
        let acy = (aur.y + all.y) / 2.0;
        let bcx = (bur.x + bll.x) / 2.0;
        let bcy = (bur.y + bll.y) / 2.0;
        let (dx, dy) = (acx - bcx, acy - bcy);
        return Ok(Value::Float(sqrt_newton(dx * dx + dy * dy)));
    }
    // PG `circle <-> circle` is the gap between the boundaries: the centre
    // distance minus both radii, clamped to 0 when they overlap.
    if let (
        Value::Circle {
            center: ac,
            radius: ar,
        },
        Value::Circle {
            center: bc,
            radius: br,
        },
    ) = (&l, &r)
    {
        let (dx, dy) = (ac.x - bc.x, ac.y - bc.y);
        let gap = sqrt_newton(dx * dx + dy * dy) - ar - br;
        return Ok(Value::Float(if gap < 0.0 { 0.0 } else { gap }));
    }
    // v7.39 (read01 geo_ops.c part 2) — the remaining geometric distance
    // pairs (lseg↔lseg, box↔lseg, point↔path, point↔polygon).
    if let Some(d) = geo_pair_distance(&l, &r) {
        return Ok(Value::Float(d));
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
    // libm's correctly-rounded sqrt matches PG's libc sqrt to the last ULP;
    // the former hand-rolled Newton iteration was one ULP low on irrational
    // results (e.g. box <-> box centre distance sqrt(2)).
    libm::sqrt(x)
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
            if b == 0 {
                None
            } else {
                Some(a.wrapping_rem(b))
            }
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
        return Ok(Value::Float(check_float8_range(a / b, a, b, "/")?));
    }
    // v7.38 (read01 P4.18) — a zero divisor is DivisionByZero; INT_MIN / -1
    // has no representable quotient and is a separate overflow (PG: "out of
    // range"), NOT a zero-divide. Check the divisor up front so the two stay
    // distinct instead of both collapsing to DivisionByZero.
    if matches!(r, Value::SmallInt(0) | Value::Int(0) | Value::BigInt(0)) {
        return Err(EvalError::DivisionByZero);
    }
    arith(
        l,
        r,
        // Divisor is non-zero here, so `checked_div` returns None only on the
        // INT_MIN / -1 overflow, which `arith` surfaces as an overflow error.
        |a, b| a.checked_div(b),
        |a, b| a / b,
        "/",
    )
}

fn as_f64(v: &Value<'_>) -> Result<f64, EvalError> {
    match v {
        Value::SmallInt(n) => Ok(f64::from(*n)),
        Value::Int(n) => Ok(f64::from(*n)),
        #[allow(clippy::cast_precision_loss)]
        Value::BigInt(n) => Ok(*n as f64),
        Value::Float(x) => Ok(*x),
        Value::Real(x) => Ok(f64::from(*x)),
        #[allow(clippy::cast_precision_loss)]
            // v7.39 (round 271) — parse the decimal text instead of
            // dividing by a power built with repeated multiplication,
            // which accumulated rounding error and ran to infinity once
            // `scale` could exceed 308.
        Value::Numeric { scaled, scale, .. } => {
            Ok(crate::eval::format_numeric(*scaled, *scale)
                .parse()
                .unwrap_or(f64::NAN))
        }
        // v7.39 (read01 numeric.c) — a big NUMERIC in a mixed float op
        // approximates through its decimal text (same promotion as the
        // i128-mantissa arm above).
        Value::NumericBig(b) => b
            .to_decimal_str()
            .parse()
            .map_err(|_| EvalError::TypeMismatch {
                detail: "value out of range: overflow".into(),
            }),
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
fn numeric_pair_cmp(a: (i128, u16), b: (i128, u16)) -> core::cmp::Ordering {
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

/// v7.38 (read01, T12.4) — PG's tsvector B-tree total order: lexeme count
/// first, then per-lexeme by word (length, then bytes), then positions, then
/// weight. (The position tiebreak direction for identical words is a niche
/// detail; the count + word ordering matches PG.)
fn tsvector_total_cmp(
    a: &[spg_storage::TsLexeme],
    b: &[spg_storage::TsLexeme],
) -> core::cmp::Ordering {
    use core::cmp::Ordering::Equal;
    if a.len() != b.len() {
        return a.len().cmp(&b.len());
    }
    for (la, lb) in a.iter().zip(b.iter()) {
        let c = la
            .word
            .len()
            .cmp(&lb.word.len())
            .then_with(|| la.word.cmp(&lb.word))
            .then_with(|| la.positions.cmp(&lb.positions))
            .then_with(|| la.weight.cmp(&lb.weight));
        if c != Equal {
            return c;
        }
    }
    Equal
}

/// v7.38 (read01, R2) — the "traditional" CRC-32 (reflected poly 0xEDB88320,
/// init/final 0xFFFFFFFF) PG's `silly_cmp_tsquery` uses to order tsquery
/// lexemes — so ordering is CRC-based, NOT alphabetical (`'b' > 'c'`).
fn crc32_traditional(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    crc ^ 0xFFFF_FFFF
}

/// v7.38 (read01, R2) — one item of a tsquery flattened to prefix order.
enum QItem {
    Val {
        crc: u32,
        bytes: alloc::vec::Vec<u8>,
    },
    Phrase(u16),
    Or,
    And,
    Not,
}

fn tsquery_numnode(ast: &spg_storage::TsQueryAst) -> usize {
    use spg_storage::TsQueryAst as Q;
    match ast {
        Q::Term { .. } => 1,
        Q::Not(x) => 1 + tsquery_numnode(x),
        Q::And(l, r) | Q::Or(l, r) => 1 + tsquery_numnode(l) + tsquery_numnode(r),
        Q::Phrase { left, right, .. } => 1 + tsquery_numnode(left) + tsquery_numnode(right),
    }
}

fn tsquery_flatten(ast: &spg_storage::TsQueryAst, out: &mut alloc::vec::Vec<QItem>) {
    use spg_storage::TsQueryAst as Q;
    match ast {
        Q::Term { word, .. } => out.push(QItem::Val {
            crc: crc32_traditional(word.as_bytes()),
            bytes: word.as_bytes().to_vec(),
        }),
        Q::Not(x) => {
            out.push(QItem::Not);
            tsquery_flatten(x, out);
        }
        Q::And(l, r) => {
            out.push(QItem::And);
            tsquery_flatten(l, out);
            tsquery_flatten(r, out);
        }
        Q::Or(l, r) => {
            out.push(QItem::Or);
            tsquery_flatten(l, out);
            tsquery_flatten(r, out);
        }
        Q::Phrase {
            left,
            right,
            distance,
        } => {
            out.push(QItem::Phrase(*distance));
            tsquery_flatten(left, out);
            tsquery_flatten(right, out);
        }
    }
}

/// v7.38 (read01, R2) — PG's non-recursive tsquery total order: node count
/// first, then a prefix-item compare where an operand (VAL) sorts before any
/// operator, VALs compare by CRC-32 then length then bytes, operators sort
/// `PHRASE < OR < AND < NOT`, and two PHRASE nodes sort larger-distance-first.
fn tsquery_total_cmp(
    a: &spg_storage::TsQueryAst,
    b: &spg_storage::TsQueryAst,
) -> core::cmp::Ordering {
    use core::cmp::Ordering::{Equal, Greater, Less};
    let (na, nb) = (tsquery_numnode(a), tsquery_numnode(b));
    if na != nb {
        return na.cmp(&nb);
    }
    let opr_rank = |q: &QItem| -> u8 {
        match q {
            QItem::Phrase(_) => 0,
            QItem::Or => 1,
            QItem::And => 2,
            QItem::Not => 3,
            QItem::Val { .. } => unreachable!(),
        }
    };
    let item_cmp = |x: &QItem, y: &QItem| -> core::cmp::Ordering {
        match (x, y) {
            (QItem::Val { crc: cx, bytes: bx }, QItem::Val { crc: cy, bytes: by }) => {
                (*cx as i32) // PG's valcrc is int32 — the compare is SIGNED
                    .cmp(&(*cy as i32))
                    .then_with(|| bx.len().cmp(&by.len()))
                    .then_with(|| bx.cmp(by))
            }
            (QItem::Val { .. }, _) => Less,
            (_, QItem::Val { .. }) => Greater,
            (QItem::Phrase(dx), QItem::Phrase(dy)) => dy.cmp(dx), // larger distance first
            _ => opr_rank(x).cmp(&opr_rank(y)),
        }
    };
    let (mut ia, mut ib) = (alloc::vec::Vec::new(), alloc::vec::Vec::new());
    tsquery_flatten(a, &mut ia);
    tsquery_flatten(b, &mut ib);
    for (x, y) in ia.iter().zip(ib.iter()) {
        let c = item_cmp(x, y);
        if c != Equal {
            return c;
        }
    }
    Equal
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
fn upper_cmp(
    au: &Option<Value<'static>>,
    aui: bool,
    bu: &Option<Value<'static>>,
    bui: bool,
) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    match (au, bu) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater, // +∞ greatest
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => bound_cmp(x, y).then(aui.cmp(&bui)), // inclusive(true) greater
    }
}
/// v7.38 — the six components of one range operand (kind, bounds, bound
/// inclusivity, emptiness), bundled so the `range_*` family below takes two
/// parameters instead of twelve.
#[derive(Clone, Copy)]
struct RangeParts<'v> {
    kind: spg_storage::RangeKind,
    lower: &'v Option<alloc::boxed::Box<Value<'static>>>,
    upper: &'v Option<alloc::boxed::Box<Value<'static>>>,
    lower_inc: bool,
    upper_inc: bool,
    empty: bool,
}

impl<'v> RangeParts<'v> {
    /// v7.39 (read01 rangetypes.c) — borrow the parts straight off a
    /// `Value::Range`; `None` for any other variant.
    fn from_value(v: &'v Value<'_>) -> Option<Self> {
        match v {
            Value::Range {
                kind,
                lower,
                upper,
                lower_inc,
                upper_inc,
                empty,
            } => Some(Self::new(
                *kind, lower, upper, *lower_inc, *upper_inc, *empty,
            )),
            _ => None,
        }
    }

    fn new(
        kind: spg_storage::RangeKind,
        lower: &'v Option<alloc::boxed::Box<Value<'static>>>,
        upper: &'v Option<alloc::boxed::Box<Value<'static>>>,
        lower_inc: bool,
        upper_inc: bool,
        empty: bool,
    ) -> Self {
        Self {
            kind,
            lower,
            upper,
            lower_inc,
            upper_inc,
            empty,
        }
    }
}

/// PG `range_cmp` total order: an empty range sorts first (two empties are
/// equal), otherwise compare canonical lower bounds, then upper bounds. The
/// `Equal` result subsumes range equality (canonical `[)` forms must match).
fn range_cmp(a: RangeParts<'_>, b: RangeParts<'_>) -> core::cmp::Ordering {
    let RangeParts {
        kind: ak,
        lower: al,
        upper: au,
        lower_inc: ali,
        upper_inc: aui,
        empty: ae,
    } = a;
    let RangeParts {
        kind: bk,
        lower: bl,
        upper: bu,
        lower_inc: bli,
        upper_inc: bui,
        empty: be,
    } = b;
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
        (
            Value::Numeric {
                scaled: xs,
                scale: xc,
                ..
            },
            Value::Numeric {
                scaled: ys,
                scale: yc,
                ..
            },
        ) => numeric_pair_cmp((*xs, *xc), (*ys, *yc)),
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
        Value::Numeric { scaled, scale, .. } => {
            Some(*scaled as f64 / 10i128.pow(u32::from(*scale)) as f64)
        }
        _ => None,
    }
}

/// Sort key for range lowers: `None` (−∞) first, then by value, then an
/// inclusive lower before an exclusive one at the same value.
fn lower_cmp(
    al: &Option<Value<'static>>,
    ali: bool,
    bl: &Option<Value<'static>>,
    bli: bool,
) -> core::cmp::Ordering {
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
fn upper_reaches_lower(
    au: &Option<Value<'static>>,
    aui: bool,
    bl: &Option<Value<'static>>,
    bli: bool,
) -> bool {
    use core::cmp::Ordering;
    match (au, bl) {
        (None, _) => true, // +∞ upper reaches anything
        (_, None) => true, // following span starts at −∞
        (Some(u), Some(l)) => match bound_cmp(u, l) {
            Ordering::Greater => true,     // overlap
            Ordering::Equal => aui || bli, // adjacent iff the touching point is covered
            Ordering::Less => false,       // gap
        },
    }
}

/// Is upper `(au, aui)` strictly greater than upper `(bu, bui)`?
fn upper_greater(
    au: &Option<Value<'static>>,
    aui: bool,
    bu: &Option<Value<'static>>,
    bui: bool,
) -> bool {
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
fn normalize_multirange(
    kind: spg_storage::RangeKind,
    spans: &[spg_storage::RangeSpan],
) -> alloc::vec::Vec<CanonSpan> {
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

/// Normalize a multirange's member spans into PG-canonical form: drop
/// empty, canonicalize each (discrete `(a` → `[a+1`, `b]` → `b+1)`),
/// sort by lower bound, and merge overlapping/adjacent spans. This is
/// what `multirange` constructors must store — `int4multirange(int4range
/// (1,5), int4range(4,8))` is `{[1,8)}`, not two spans.
/// v7.39 (read01 multirangetypes.c) — span -> RangeParts view.
fn span_parts(kind: spg_storage::RangeKind, s: &spg_storage::RangeSpan) -> RangeParts<'_> {
    RangeParts::new(kind, &s.lower, &s.upper, s.lower_inc, s.upper_inc, s.empty)
}

/// Multirange contains: an element / a range is contained when SOME span
/// contains it; a multirange is contained when EVERY span of it is
/// contained in some span (canonical spans are disjoint + sorted, so the
/// per-span check is exact).
pub(crate) fn multirange_contains(
    kind: spg_storage::RangeKind,
    spans: &[spg_storage::RangeSpan],
    rhs: &Value<'_>,
) -> Option<bool> {
    match rhs {
        Value::Range {
            kind: rk,
            lower,
            upper,
            lower_inc,
            upper_inc,
            empty,
        } => {
            if *rk != kind {
                return None;
            }
            if *empty {
                return Some(true);
            }
            let b = RangeParts::new(*rk, lower, upper, *lower_inc, *upper_inc, *empty);
            Some(
                spans
                    .iter()
                    .any(|s| range_contains_range(span_parts(kind, s), b)),
            )
        }
        Value::Multirange { kind: rk, ranges } => {
            if *rk != kind {
                return None;
            }
            Some(ranges.iter().all(|r| {
                spans
                    .iter()
                    .any(|s| range_contains_range(span_parts(kind, s), span_parts(kind, r)))
            }))
        }
        Value::Null => None,
        elem => Some(spans.iter().any(|s| {
            range_contains_elem(
                kind,
                &s.lower,
                &s.upper,
                s.lower_inc,
                s.upper_inc,
                s.empty,
                elem,
            )
        })),
    }
}

/// Multirange set algebra: union / difference / intersection, all
/// returning canonical span lists.
/// v7.39 (round 256) — a multirange's outer hull as a plain range (the
/// same value `range_merge(multirange)` returns). PG's POSITIONAL
/// operators (`<<` `>>` `&<` `&>` `-|-`) treat a multirange as its hull:
/// probed live, `{[1,3),[9,11)} -|- {[3,5)}` is FALSE even though the
/// first element is adjacent to the operand — the hull `[1,11)`
/// overlaps it. An empty multirange has no hull, so those operators
/// answer false for it.
pub(crate) fn multirange_hull(
    kind: spg_storage::RangeKind,
    ranges: &[spg_storage::RangeSpan],
) -> Value<'static> {
    if ranges.is_empty() {
        return Value::Range {
            kind,
            lower: None,
            upper: None,
            lower_inc: false,
            upper_inc: false,
            empty: true,
        };
    }
    let first = &ranges[0];
    let last = &ranges[ranges.len() - 1];
    Value::Range {
        kind,
        lower: first.lower.clone(),
        upper: last.upper.clone(),
        lower_inc: first.lower_inc,
        upper_inc: last.upper_inc,
        empty: false,
    }
}

/// v7.39 (round 256) — promote a plain range to the one-element
/// multirange PG treats it as when the other operand is a multirange
/// (`range && multirange`, `range @> multirange`, `multirange <@ range`
/// …). Returns `None` for anything else so the caller leaves it alone.
pub(crate) fn range_as_multirange(v: &Value<'_>) -> Option<Value<'static>> {
    let Value::Range {
        kind,
        lower,
        upper,
        lower_inc,
        upper_inc,
        empty,
    } = v
    else {
        return None;
    };
    let ranges = if *empty {
        alloc::vec::Vec::new()
    } else {
        alloc::vec![spg_storage::RangeSpan {
            lower: lower.clone(),
            upper: upper.clone(),
            lower_inc: *lower_inc,
            upper_inc: *upper_inc,
            empty: false,
        }]
    };
    Some(Value::Multirange { kind: *kind, ranges })
}

pub(crate) fn multirange_union(
    kind: spg_storage::RangeKind,
    a: &[spg_storage::RangeSpan],
    b: &[spg_storage::RangeSpan],
) -> alloc::vec::Vec<spg_storage::RangeSpan> {
    let mut all: alloc::vec::Vec<spg_storage::RangeSpan> = a.to_vec();
    all.extend_from_slice(b);
    normalize_multirange_spans(kind, &all)
}

pub(crate) fn multirange_intersection(
    kind: spg_storage::RangeKind,
    a: &[spg_storage::RangeSpan],
    b: &[spg_storage::RangeSpan],
) -> alloc::vec::Vec<spg_storage::RangeSpan> {
    let mut out: alloc::vec::Vec<spg_storage::RangeSpan> = alloc::vec::Vec::new();
    for x in a {
        for y in b {
            if let Value::Range {
                lower,
                upper,
                lower_inc,
                upper_inc,
                empty,
                ..
            } = range_intersect(span_parts(kind, x), span_parts(kind, y))
            {
                if !empty {
                    out.push(spg_storage::RangeSpan {
                        lower,
                        upper,
                        lower_inc,
                        upper_inc,
                        empty: false,
                    });
                }
            }
        }
    }
    normalize_multirange_spans(kind, &out)
}

pub(crate) fn multirange_difference(
    kind: spg_storage::RangeKind,
    a: &[spg_storage::RangeSpan],
    b: &[spg_storage::RangeSpan],
) -> alloc::vec::Vec<spg_storage::RangeSpan> {
    // Subtract every b-span from every surviving a-fragment. A cut can
    // split a fragment in two: [frag.lower, cut.lower) + (cut.upper,
    // frag.upper] — expressed through the bound flips.
    let mut frags: alloc::vec::Vec<spg_storage::RangeSpan> = a.to_vec();
    for cut in b {
        let mut next: alloc::vec::Vec<spg_storage::RangeSpan> = alloc::vec::Vec::new();
        for f in frags {
            // No overlap -> keep whole.
            let inter = range_intersect(span_parts(kind, &f), span_parts(kind, cut));
            let overlap = matches!(&inter, Value::Range { empty: false, .. });
            if !overlap {
                next.push(f);
                continue;
            }
            let Value::Range {
                lower: il,
                upper: iu,
                lower_inc: ili,
                upper_inc: iui,
                ..
            } = inter
            else {
                next.push(f);
                continue;
            };
            // Left remainder: [f.lower, inter.lower)
            let left = spg_storage::RangeSpan {
                lower: f.lower.clone(),
                upper: il.clone(),
                lower_inc: f.lower_inc,
                upper_inc: !ili,
                empty: false,
            };
            if span_nonempty(kind, &left) {
                next.push(left);
            }
            // Right remainder: (inter.upper, f.upper]
            let right = spg_storage::RangeSpan {
                lower: iu.clone(),
                upper: f.upper.clone(),
                lower_inc: !iui,
                upper_inc: f.upper_inc,
                empty: false,
            };
            if span_nonempty(kind, &right) {
                next.push(right);
            }
        }
        frags = next;
    }
    normalize_multirange_spans(kind, &frags)
}

/// A span is non-empty when lower < upper, or lower == upper with both
/// bounds inclusive; open bounds (None) are infinite.
fn span_nonempty(kind: spg_storage::RangeKind, s: &spg_storage::RangeSpan) -> bool {
    let _ = kind;
    match (&s.lower, &s.upper) {
        (Some(l), Some(u)) => match crate::orderby::value_cmp(l, u) {
            core::cmp::Ordering::Less => true,
            core::cmp::Ordering::Equal => s.lower_inc && s.upper_inc,
            core::cmp::Ordering::Greater => false,
        },
        _ => true,
    }
}

pub(crate) fn normalize_multirange_spans(
    kind: spg_storage::RangeKind,
    spans: &[spg_storage::RangeSpan],
) -> alloc::vec::Vec<spg_storage::RangeSpan> {
    normalize_multirange(kind, spans)
        .into_iter()
        .map(|(lo, li, up, ui)| spg_storage::RangeSpan {
            lower: lo.map(alloc::boxed::Box::new),
            upper: up.map(alloc::boxed::Box::new),
            lower_inc: li,
            upper_inc: ui,
            empty: false,
        })
        .collect()
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
fn range_point_le(
    l: &Option<Value<'static>>,
    l_inc: bool,
    u: &Option<Value<'static>>,
    u_inc: bool,
) -> bool {
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
fn range_overlaps(a: RangeParts<'_>, b: RangeParts<'_>) -> bool {
    let RangeParts {
        kind: ak,
        lower: al,
        upper: au,
        lower_inc: ali,
        upper_inc: aui,
        empty: ae,
    } = a;
    let RangeParts {
        kind: bk,
        lower: bl,
        upper: bu,
        lower_inc: bli,
        upper_inc: bui,
        empty: be,
    } = b;
    if ae || be {
        return false;
    }
    let (al2, ali2, au2, aui2) = range_canonical(ak, al, au, ali, aui);
    let (bl2, bli2, bu2, bui2) = range_canonical(bk, bl, bu, bli, bui);
    range_point_le(&al2, ali2, &bu2, bui2) && range_point_le(&bl2, bli2, &au2, aui2)
}

/// Range `@>` range: `a` contains `b`. Empty is contained in everything;
/// only empty contains empty.
fn range_contains_range(a: RangeParts<'_>, b: RangeParts<'_>) -> bool {
    let RangeParts {
        kind: ak,
        lower: al,
        upper: au,
        lower_inc: ali,
        upper_inc: aui,
        empty: ae,
    } = a;
    let RangeParts {
        kind: bk,
        lower: bl,
        upper: bu,
        lower_inc: bli,
        upper_inc: bui,
        empty: be,
    } = b;
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
    ak: spg_storage::RangeKind,
    al: &Option<alloc::boxed::Box<Value<'static>>>,
    au: &Option<alloc::boxed::Box<Value<'static>>>,
    ali: bool,
    aui: bool,
    ae: bool,
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
fn range_intersect(a: RangeParts<'_>, b: RangeParts<'_>) -> Value<'static> {
    let RangeParts {
        kind: ak,
        lower: al,
        upper: au,
        lower_inc: ali,
        upper_inc: aui,
        empty: ae,
    } = a;
    let RangeParts {
        kind: bk,
        lower: bl,
        upper: bu,
        lower_inc: bli,
        upper_inc: bui,
        empty: be,
    } = b;
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
fn bounds_touch(
    up: &Option<Value<'static>>,
    up_inc: bool,
    low: &Option<Value<'static>>,
    low_inc: bool,
) -> bool {
    match (up, low) {
        (Some(u), Some(l)) => bound_cmp(u, l) == core::cmp::Ordering::Equal && (up_inc != low_inc),
        _ => false,
    }
}

/// Range `+` union: the two ranges must overlap or be adjacent, else PG
/// errors "result of range union would not be contiguous".
fn range_union(a: RangeParts<'_>, b: RangeParts<'_>) -> Result<Value<'static>, EvalError> {
    let RangeParts {
        kind: ak,
        lower: al,
        upper: au,
        lower_inc: ali,
        upper_inc: aui,
        empty: ae,
    } = a;
    let RangeParts {
        kind: bk,
        lower: bl,
        upper: bu,
        lower_inc: bli,
        upper_inc: bui,
        empty: be,
    } = b;
    let mk = |k,
              lo: &Option<alloc::boxed::Box<Value<'static>>>,
              up: &Option<alloc::boxed::Box<Value<'static>>>,
              li,
              ui,
              e| Value::Range {
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
    let overlap = range_overlaps(
        RangeParts::new(ak, al, au, ali, aui, ae),
        RangeParts::new(bk, bl, bu, bli, bui, be),
    );
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
fn range_difference(a: RangeParts<'_>, b: RangeParts<'_>) -> Result<Value<'static>, EvalError> {
    let RangeParts {
        kind: ak,
        lower: al,
        upper: au,
        lower_inc: ali,
        upper_inc: aui,
        empty: ae,
    } = a;
    let RangeParts {
        kind: bk,
        lower: bl,
        upper: bu,
        lower_inc: bli,
        upper_inc: bui,
        empty: be,
    } = b;
    let empty = Value::Range {
        kind: ak,
        lower: None,
        upper: None,
        lower_inc: false,
        upper_inc: false,
        empty: true,
    };
    let mk = |lo: Option<alloc::boxed::Box<Value<'static>>>,
              up: Option<alloc::boxed::Box<Value<'static>>>,
              li,
              ui| Value::Range {
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
    if be
        || !range_overlaps(
            RangeParts::new(ak, al, au, ali, aui, ae),
            RangeParts::new(bk, bl, bu, bli, bui, be),
        )
    {
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
        Ok(mk(
            al2.map(alloc::boxed::Box::new),
            bl2.map(alloc::boxed::Box::new),
            ali2,
            !bli2,
        ))
    } else {
        // Keep `a`'s right part: [b.upper, a.upper).
        Ok(mk(
            bu2.map(alloc::boxed::Box::new),
            au2.map(alloc::boxed::Box::new),
            !bui2,
            aui2,
        ))
    }
}

/// MONEY arithmetic on integer cents (PG semantics): `money ± money → money`,
/// `money × number → money`, `money ÷ number → money`, `money ÷ money → float8`
/// ratio. Returns `None` when the operator/operand shape is not money math (let
/// the caller fall through). Rounding is half-away-from-zero, no_std-friendly.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn money_arith(
    op: BinOp,
    l: &Value<'_>,
    r: &Value<'_>,
) -> Option<Result<Value<'static>, EvalError>> {
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
            Value::Numeric { scaled, scale, .. } => {
                Some(*scaled as f64 / 10i128.pow(u32::from(*scale)) as f64)
            }
            _ => None,
        }
    };
    let round_cents = |v: f64| -> i64 {
        if v >= 0.0 {
            (v + 0.5) as i64
        } else {
            (v - 0.5) as i64
        }
    };
    match (op, l, r) {
        (BinOp::Add, Value::Money(a), Value::Money(b)) => {
            Some(Ok(Value::Money(a.saturating_add(*b))))
        }
        (BinOp::Sub, Value::Money(a), Value::Money(b)) => {
            Some(Ok(Value::Money(a.saturating_sub(*b))))
        }
        (BinOp::Div, Value::Money(a), Value::Money(b)) => Some(if *b == 0 {
            Err(EvalError::DivisionByZero)
        } else {
            Ok(Value::Float(*a as f64 / *b as f64))
        }),
        (BinOp::Mul, Value::Money(a), other) | (BinOp::Mul, other, Value::Money(a)) => {
            factor(other).map(|f| Ok(Value::Money(round_cents(*a as f64 * f))))
        }
        (BinOp::Div, Value::Money(a), other) => factor(other).map(|f| {
            if f == 0.0 {
                Err(EvalError::DivisionByZero)
            } else {
                Ok(Value::Money(round_cents(*a as f64 / f)))
            }
        }),
        _ => None,
    }
}

/// Range `<<` "strictly left of": every point of `a` is less than every point
/// of `b`. True when `a`'s upper bound value is below `b`'s lower bound value;
/// at an equal boundary they must not both be inclusive (else the shared point
/// overlaps). Unbounded sides on the touching edges make it false.
fn range_strictly_left(a: RangeParts<'_>, b: RangeParts<'_>) -> bool {
    let RangeParts {
        kind: ak,
        lower: al,
        upper: au,
        lower_inc: ali,
        upper_inc: aui,
        empty: ae,
    } = a;
    let RangeParts {
        kind: bk,
        lower: bl,
        upper: bu,
        lower_inc: bli,
        upper_inc: bui,
        empty: be,
    } = b;
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

/// v7.39 (read01 rangetypes.c) — range `&<` "does not extend to the right
/// of": a's upper bound position <= b's (empty on either side → false; at
/// equal values an inclusive upper sits right of an exclusive one).
fn range_overleft(a: RangeParts<'_>, b: RangeParts<'_>) -> bool {
    use core::cmp::Ordering;
    if a.empty || b.empty {
        return false;
    }
    let (_, _, au, aui) = range_canonical(a.kind, a.lower, a.upper, a.lower_inc, a.upper_inc);
    let (_, _, bu, bui) = range_canonical(b.kind, b.lower, b.upper, b.lower_inc, b.upper_inc);
    match (&au, &bu) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(_), None) => true,
        (Some(x), Some(y)) => match bound_cmp(x, y) {
            Ordering::Less => true,
            Ordering::Greater => false,
            Ordering::Equal => !aui || bui,
        },
    }
}

/// v7.39 (read01 rangetypes.c) — range `&>` "does not extend to the left
/// of": a's lower bound position >= b's (at equal values an exclusive lower
/// sits right of an inclusive one).
fn range_overright(a: RangeParts<'_>, b: RangeParts<'_>) -> bool {
    use core::cmp::Ordering;
    if a.empty || b.empty {
        return false;
    }
    let (al, ali, _, _) = range_canonical(a.kind, a.lower, a.upper, a.lower_inc, a.upper_inc);
    let (bl, bli, _, _) = range_canonical(b.kind, b.lower, b.upper, b.lower_inc, b.upper_inc);
    match (&al, &bl) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(_), None) => true,
        (Some(x), Some(y)) => match bound_cmp(x, y) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => !ali || bli,
        },
    }
}

/// Distance from a point to a segment, box, or circle (`point <-> geo`).
/// `None` unless the first operand is a point and the second is one of those
/// geometries.
/// v7.39 (read01 geo_ops.c part 2) — nearest point on segment (a,b) to pt
/// (projection clamped to the segment).
fn lseg_closest_to_point(
    a: &spg_storage::Point2D,
    b: &spg_storage::Point2D,
    pt: &spg_storage::Point2D,
) -> spg_storage::Point2D {
    let (vx, vy) = (b.x - a.x, b.y - a.y);
    let len2 = vx * vx + vy * vy;
    let t = if len2 == 0.0 {
        0.0
    } else {
        (((pt.x - a.x) * vx + (pt.y - a.y) * vy) / len2).clamp(0.0, 1.0)
    };
    spg_storage::Point2D {
        x: a.x + t * vx,
        y: a.y + t * vy,
    }
}

/// v7.39 (read01 geo_ops.c part 2) — intersection of Ax+By+C=0 lines
/// (PG line_interpt_line): parallel (epsilon slope test) → None; -0 is
/// normalized to 0.
fn line_line_interpt(
    a1: f64,
    b1: f64,
    c1: f64,
    a2: f64,
    b2: f64,
    c2: f64,
) -> Option<spg_storage::Point2D> {
    const EPS: f64 = 1.0e-6;
    let (x, y);
    if b1.abs() > EPS {
        if (a2 - a1 * (b2 / b1)).abs() <= EPS {
            return None;
        }
        x = (b1 * c2 - b2 * c1) / (a1 * b2 - a2 * b1);
        y = -(a1 * x + c1) / b1;
    } else if b2.abs() > EPS {
        if (a1 - a2 * (b1 / b2)).abs() <= EPS {
            return None;
        }
        x = (b2 * c1 - b1 * c2) / (a2 * b1 - a1 * b2);
        y = -(a2 * x + c2) / b2;
    } else {
        return None;
    }
    Some(spg_storage::Point2D {
        x: if x == 0.0 { 0.0 } else { x },
        y: if y == 0.0 { 0.0 } else { y },
    })
}

fn pt_dist(a: &spg_storage::Point2D, b: &spg_storage::Point2D) -> f64 {
    let (dx, dy) = (a.x - b.x, a.y - b.y);
    sqrt_newton(dx * dx + dy * dy)
}

/// Segment-to-segment distance: 0 when they intersect, else the minimum
/// endpoint-to-segment distance over the four combinations (PG's
/// lseg_closept_lseg shape).
fn lseg_lseg_distance(
    a1: &spg_storage::Point2D,
    a2: &spg_storage::Point2D,
    b1: &spg_storage::Point2D,
    b2: &spg_storage::Point2D,
) -> f64 {
    if lseg_intersection(*a1, *a2, *b1, *b2).is_some() {
        return 0.0;
    }
    let d1 = pt_dist(&lseg_closest_to_point(a1, a2, b1), b1);
    let d2 = pt_dist(&lseg_closest_to_point(a1, a2, b2), b2);
    let d3 = pt_dist(&lseg_closest_to_point(b1, b2, a1), a1);
    let d4 = pt_dist(&lseg_closest_to_point(b1, b2, a2), a2);
    d1.min(d2).min(d3).min(d4)
}

/// Even-odd ray-casting point-in-polygon (shared by containment and the
/// point-polygon distance).
fn point_in_polygon(pt: &spg_storage::Point2D, pts: &[spg_storage::Point2D]) -> bool {
    if pts.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = pts.len() - 1;
    for i in 0..pts.len() {
        let (xi, yi) = (pts[i].x, pts[i].y);
        let (xj, yj) = (pts[j].x, pts[j].y);
        if ((yi > pt.y) != (yj > pt.y)) && (pt.x < (xj - xi) * (pt.y - yi) / (yj - yi) + xi) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// The polygon's edges including the closure segment.
fn poly_edges(
    pts: &[spg_storage::Point2D],
) -> impl Iterator<Item = (&spg_storage::Point2D, &spg_storage::Point2D)> {
    (0..pts.len()).map(move |i| {
        let j = if i == 0 { pts.len() - 1 } else { i - 1 };
        (&pts[j], &pts[i])
    })
}

/// v7.39 (read01 geo_ops.c part 2) — distances the generic point arm
/// doesn't cover: lseg↔lseg, box↔lseg, point↔path, point↔polygon
/// (either operand order for the mixed pairs).
fn geo_pair_distance(l: &Value<'_>, r: &Value<'_>) -> Option<f64> {
    let one = |a: &Value<'_>, b: &Value<'_>| -> Option<f64> {
        match (a, b) {
            (Value::Lseg(a1, a2), Value::Lseg(b1, b2)) => Some(lseg_lseg_distance(a1, a2, b1, b2)),
            (Value::PgBox(ur, ll), Value::Lseg(s1, s2)) => {
                // Inside or crossing the box is distance 0; otherwise the
                // minimum over the four box edges (PG box_closept_lseg).
                let inside = |p: &spg_storage::Point2D| {
                    p.x >= ll.x.min(ur.x)
                        && p.x <= ll.x.max(ur.x)
                        && p.y >= ll.y.min(ur.y)
                        && p.y <= ll.y.max(ur.y)
                };
                if inside(s1) || inside(s2) {
                    return Some(0.0);
                }
                let (hx, hy) = (ll.x.max(ur.x), ll.y.max(ur.y));
                let (lx, ly) = (ll.x.min(ur.x), ll.y.min(ur.y));
                let c = [
                    spg_storage::Point2D { x: lx, y: ly },
                    spg_storage::Point2D { x: lx, y: hy },
                    spg_storage::Point2D { x: hx, y: hy },
                    spg_storage::Point2D { x: hx, y: ly },
                ];
                let mut best = f64::INFINITY;
                for i in 0..4 {
                    let j = (i + 1) % 4;
                    best = best.min(lseg_lseg_distance(&c[i], &c[j], s1, s2));
                }
                Some(best)
            }
            (Value::Point(pt), Value::Path { points, closed }) => {
                if points.is_empty() {
                    return None;
                }
                if points.len() == 1 {
                    return Some(pt_dist(pt, &points[0]));
                }
                let mut best = f64::INFINITY;
                for i in 0..points.len() {
                    if i == 0 && !closed {
                        continue;
                    }
                    let j = if i == 0 { points.len() - 1 } else { i - 1 };
                    let n = lseg_closest_to_point(&points[j], &points[i], pt);
                    best = best.min(pt_dist(&n, pt));
                }
                Some(best)
            }
            (Value::Point(pt), Value::Polygon(pts)) => {
                if point_in_polygon(pt, pts) {
                    return Some(0.0);
                }
                let mut best = f64::INFINITY;
                for (a, b) in poly_edges(pts) {
                    let n = lseg_closest_to_point(a, b, pt);
                    best = best.min(pt_dist(&n, pt));
                }
                Some(best)
            }
            _ => None,
        }
    };
    one(l, r).or_else(|| one(r, l))
}

/// v7.39 (read01 geo_ops.c part 2) — direction-sensitive geometric
/// containments beyond point-in-shape: polygon⊃polygon (bbox gate +
/// every vertex and edge midpoint of the contained polygon inside the
/// container — a clean-room simplification of PG's lseg_inside_poly
/// recursion; concave edge-crossing corner cases are a recorded
/// residual), and circle⊃circle.
fn geo_contains_geo(container: &Value<'_>, contained: &Value<'_>) -> Option<bool> {
    const EPS: f64 = 1.0e-6;
    match (container, contained) {
        (Value::Polygon(a), Value::Polygon(b)) => {
            if a.len() < 3 || b.is_empty() {
                return Some(false);
            }
            let on_edge = |p: &spg_storage::Point2D| {
                poly_edges(a).any(|(e1, e2)| {
                    (pt_dist(p, e1) + pt_dist(p, e2) - pt_dist(e1, e2)).abs() <= EPS
                })
            };
            let inside = |p: &spg_storage::Point2D| point_in_polygon(p, a) || on_edge(p);
            if !b.iter().all(|p| inside(p)) {
                return Some(false);
            }
            let mids = (0..b.len()).all(|i| {
                let j = if i == 0 { b.len() - 1 } else { i - 1 };
                let m = spg_storage::Point2D {
                    x: (b[i].x + b[j].x) / 2.0,
                    y: (b[i].y + b[j].y) / 2.0,
                };
                inside(&m)
            });
            Some(mids)
        }
        (
            Value::Circle {
                center: c1,
                radius: r1,
            },
            Value::Circle {
                center: c2,
                radius: r2,
            },
        ) => Some(pt_dist(c1, c2) + r2 <= r1 + EPS),
        _ => None,
    }
}

/// v7.39 (read01 geo_ops.c part 2) — containers valid only on the `<@`
/// side (PG has point <@ lseg / point <@ path but no lseg @> point).
fn geo_pt_on_object(container: &Value<'_>, p: &Value<'_>) -> Option<bool> {
    const EPS: f64 = 1.0e-6;
    let Value::Point(pt) = p else { return None };
    match container {
        Value::Lseg(a, b) => Some((pt_dist(pt, a) + pt_dist(pt, b) - pt_dist(a, b)).abs() <= EPS),
        Value::Path { points, closed } => {
            if points.is_empty() {
                return Some(false);
            }
            if points.len() == 1 {
                return Some(pt_dist(pt, &points[0]) <= EPS);
            }
            let on_any = (0..points.len()).any(|i| {
                if i == 0 && !closed {
                    return false;
                }
                let j = if i == 0 { points.len() - 1 } else { i - 1 };
                (pt_dist(pt, &points[j]) + pt_dist(pt, &points[i])
                    - pt_dist(&points[j], &points[i]))
                .abs()
                    <= EPS
            });
            Some(on_any)
        }
        _ => None,
    }
}

fn point_geo_distance(p: &Value<'_>, geo: &Value<'_>) -> Option<f64> {
    let Value::Point(pt) = p else { return None };
    match geo {
        Value::Lseg(a, b) => {
            // Nearest point on the segment: project pt, clamp t to [0,1].
            let (vx, vy) = (b.x - a.x, b.y - a.y);
            let len2 = vx * vx + vy * vy;
            let t = if len2 == 0.0 {
                0.0
            } else {
                (((pt.x - a.x) * vx + (pt.y - a.y) * vy) / len2).clamp(0.0, 1.0)
            };
            let (nx, ny) = (a.x + t * vx, a.y + t * vy);
            let (dx, dy) = (pt.x - nx, pt.y - ny);
            Some(sqrt_newton(dx * dx + dy * dy))
        }
        Value::PgBox(ur, ll) => {
            // Clamp the point into the box; distance to the clamped point.
            let nx = pt.x.clamp(ll.x, ur.x);
            let ny = pt.y.clamp(ll.y, ur.y);
            let (dx, dy) = (pt.x - nx, pt.y - ny);
            Some(sqrt_newton(dx * dx + dy * dy))
        }
        Value::Circle { center, radius } => {
            let (dx, dy) = (pt.x - center.x, pt.y - center.y);
            let d = sqrt_newton(dx * dx + dy * dy) - radius;
            Some(if d < 0.0 { 0.0 } else { d })
        }
        // Point-to-line distance: PG's line_closept_point drops a
        // perpendicular through the point and measures to the
        // intersection — same math as |Ax+By+C|/√(A²+B²) but the
        // operation order matters to the last ULP, so mirror it.
        Value::Line { a, b, c } => {
            const EPS: f64 = 1.0e-6;
            // Perpendicular through pt: slope of the line is -a/b
            // (vertical when |b|≈0), the perpendicular inverts it.
            let (pa, pb, pc) = if b.abs() <= EPS {
                // Line is vertical → perpendicular is horizontal y = pt.y.
                (0.0, -1.0, pt.y)
            } else {
                let m = -a / b;
                let im = if m.abs() <= EPS {
                    // Horizontal line → vertical perpendicular x = pt.x.
                    return line_line_interpt(-1.0, 0.0, pt.x, *a, *b, *c)
                        .map(|ip| pt_dist(&ip, pt));
                } else {
                    -1.0 / m
                };
                (im, -1.0, pt.y - im * pt.x)
            };
            line_line_interpt(pa, pb, pc, *a, *b, *c).map(|ip| pt_dist(&ip, pt))
        }
        _ => None,
    }
}

/// Collect every lexeme (Term word) appearing anywhere in a tsquery tree,
/// ignoring the combining operators — the set used by `tsquery @> tsquery`.
fn tsquery_lexemes(
    q: &spg_storage::TsQueryAst,
    out: &mut alloc::collections::BTreeSet<alloc::string::String>,
) {
    use spg_storage::TsQueryAst;
    match q {
        TsQueryAst::Term { word, .. } => {
            out.insert(word.clone());
        }
        TsQueryAst::And(a, b) | TsQueryAst::Or(a, b) => {
            tsquery_lexemes(a, out);
            tsquery_lexemes(b, out);
        }
        TsQueryAst::Not(a) => tsquery_lexemes(a, out),
        TsQueryAst::Phrase { left, right, .. } => {
            tsquery_lexemes(left, out);
            tsquery_lexemes(right, out);
        }
    }
}

/// Geometric containment `container @> box` for a box container: the argument
/// box lies inside (or on the boundary of) the container box. `None` unless
/// both operands are boxes.
fn geo_contains_box(container: &Value<'_>, inner: &Value<'_>) -> Option<bool> {
    let (Value::PgBox(cur, cll), Value::PgBox(iur, ill)) = (container, inner) else {
        return None;
    };
    Some(ill.x >= cll.x && iur.x <= cur.x && ill.y >= cll.y && iur.y <= cur.y)
}

/// Geometric overlap `a && b` for same-type box or circle: boxes overlap when
/// their x- and y-projections both overlap; circles overlap when the distance
/// between centres is ≤ the sum of the radii. `None` for other operand pairs.
fn geo_overlaps(a: &Value<'_>, b: &Value<'_>) -> Option<bool> {
    match (a, b) {
        // v7.39 (read01 geo_ops.c part 2) — polygon overlap: any vertex of
        // one inside the other, or any pair of edges crossing.
        (Value::Polygon(pa), Value::Polygon(pb)) => {
            if pa.len() < 3 || pb.len() < 3 {
                return Some(false);
            }
            if pa.iter().any(|p| point_in_polygon(p, pb))
                || pb.iter().any(|p| point_in_polygon(p, pa))
            {
                return Some(true);
            }
            for (a1, a2) in poly_edges(pa) {
                for (b1, b2) in poly_edges(pb) {
                    if lseg_intersection(*a1, *a2, *b1, *b2).is_some() {
                        return Some(true);
                    }
                }
            }
            Some(false)
        }
        (Value::PgBox(aur, all), Value::PgBox(bur, bll)) => {
            Some(all.x <= bur.x && bll.x <= aur.x && all.y <= bur.y && bll.y <= aur.y)
        }
        (
            Value::Circle {
                center: c1,
                radius: r1,
            },
            Value::Circle {
                center: c2,
                radius: r2,
            },
        ) => {
            let (dx, dy) = (c1.x - c2.x, c1.y - c2.y);
            let rsum = r1 + r2;
            Some(dx * dx + dy * dy <= rsum * rsum)
        }
        _ => None,
    }
}

/// Geometric containment `container @> point`: whether the point lies inside
/// (or on the boundary of) a polygon (even-odd ray cast), box (bounding-box
/// test), or circle (distance ≤ radius). `None` if the operands are not a
/// geometry/point pair.
fn geo_contains_point(container: &Value<'_>, p: &Value<'_>) -> Option<bool> {
    let Value::Point(pt) = p else { return None };
    match container {
        Value::PgBox(ur, ll) => Some(pt.x >= ll.x && pt.x <= ur.x && pt.y >= ll.y && pt.y <= ur.y),
        Value::Circle { center, radius } => {
            let (dx, dy) = (pt.x - center.x, pt.y - center.y);
            Some(dx * dx + dy * dy <= radius * radius)
        }
        Value::Polygon(pts) => {
            if pts.len() < 3 {
                return Some(false);
            }
            // Even-odd ray-casting: count crossings of a ray going +x.
            let mut inside = false;
            let mut j = pts.len() - 1;
            for i in 0..pts.len() {
                let (xi, yi) = (pts[i].x, pts[i].y);
                let (xj, yj) = (pts[j].x, pts[j].y);
                if ((yi > pt.y) != (yj > pt.y)) && (pt.x < (xj - xi) * (pt.y - yi) / (yj - yi) + xi)
                {
                    inside = !inside;
                }
                j = i;
            }
            Some(inside)
        }
        _ => None,
    }
}

/// Range `-|-` "is adjacent to": `a` and `b` touch at exactly one bound with
/// no gap and no overlap — one range's upper bound value equals the other's
/// lower bound value, and exactly one of the two touching bounds is inclusive.
pub(crate) fn range_adjacent_pair(a: &Value<'_>, b: &Value<'_>) -> Option<bool> {
    use core::cmp::Ordering;
    // v7.39 (round 256) — `-|-` reads a multirange as its outer hull
    // (probed: `{[1,3),[9,11)} -|- {[3,5)}` is FALSE, so it is not an
    // any-element rule), and an empty multirange is never adjacent.
    if matches!(a, Value::Multirange { .. }) || matches!(b, Value::Multirange { .. }) {
        let hull = |v: &Value<'_>| -> Option<Value<'static>> {
            match v {
                Value::Multirange { kind, ranges } => {
                    if ranges.is_empty() {
                        return None;
                    }
                    Some(multirange_hull(*kind, ranges))
                }
                Value::Range { .. } => Some(v.clone().into_owned()),
                _ => None,
            }
        };
        let (Some(ha), Some(hb)) = (hull(a), hull(b)) else {
            return Some(false);
        };
        return range_adjacent_pair(&ha, &hb);
    }
    let (ak, al, au, ali, aui, ae) = match a {
        Value::Range {
            kind,
            lower,
            upper,
            lower_inc,
            upper_inc,
            empty,
        } => (
            *kind,
            lower.clone(),
            upper.clone(),
            *lower_inc,
            *upper_inc,
            *empty,
        ),
        _ => return None,
    };
    let (bk, bl, bu, bli, bui, be) = match b {
        Value::Range {
            kind,
            lower,
            upper,
            lower_inc,
            upper_inc,
            empty,
        } => (
            *kind,
            lower.clone(),
            upper.clone(),
            *lower_inc,
            *upper_inc,
            *empty,
        ),
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
        Value::Range {
            kind,
            lower,
            upper,
            lower_inc,
            upper_inc,
            empty,
        } => (
            *kind,
            lower.clone(),
            upper.clone(),
            *lower_inc,
            *upper_inc,
            *empty,
        ),
        _ => return None,
    };
    let (bk, bl, bu, bli, bui, be) = match b {
        Value::Range {
            kind,
            lower,
            upper,
            lower_inc,
            upper_inc,
            empty,
        } => (
            *kind,
            lower.clone(),
            upper.clone(),
            *lower_inc,
            *upper_inc,
            *empty,
        ),
        _ => return None,
    };
    let mk = |k,
              lo: Option<alloc::boxed::Box<Value<'static>>>,
              up: Option<alloc::boxed::Box<Value<'static>>>,
              li,
              ui,
              e| {
        Value::Range {
            kind: k,
            lower: lo,
            upper: up,
            lower_inc: li,
            upper_inc: ui,
            empty: e,
        }
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
    Some(mk(
        ak,
        lo.map(alloc::boxed::Box::new),
        up.map(alloc::boxed::Box::new),
        lo_inc,
        up_inc,
        false,
    ))
}

/// The numeric value of an inet/cidr address (IPv4 in the low 4 bytes,
/// IPv6 across all 16), MSB-first.
fn inet_addr_u128(family: u8, addr: &[u8; 16]) -> u128 {
    let slice: &[u8] = if family == 4 { &addr[0..4] } else { &addr[..] };
    slice
        .iter()
        .fold(0u128, |acc, &b| (acc << 8) | u128::from(b))
}

/// Rebuild a `Value::Inet` from a numeric address (inverse of
/// `inet_addr_u128`), MSB-packing the low 4 (IPv4) or 16 (IPv6) bytes.
fn inet_from_u128(family: u8, bits: u8, val: u128) -> Value<'static> {
    let mut addr = [0u8; 16];
    if family == 4 {
        #[allow(clippy::cast_possible_truncation)]
        addr[0..4].copy_from_slice(&(val as u32).to_be_bytes());
    } else {
        addr.copy_from_slice(&val.to_be_bytes());
    }
    Value::Inet { family, bits, addr }
}

/// v7.38 (read01) — PG `inet & inet` / `inet | inet`: both must be the same
/// family, the addresses combine bitwise, and the result netmask is the wider
/// of the two (`10.0.0.0/8 & 255.0.0.0` → `10.0.0.0/32`).
fn inet_bitwise(op: BinOp, l: &Value<'_>, r: &Value<'_>) -> Result<Value<'static>, EvalError> {
    let (
        Value::Inet {
            family: fa,
            bits: ba,
            addr: aa,
        },
        Value::Inet {
            family: fb,
            bits: bb,
            addr: ab,
        },
    ) = (l, r)
    else {
        return Err(EvalError::TypeMismatch {
            detail: "inet bitwise operator needs two inet values".into(),
        });
    };
    if fa != fb {
        return Err(EvalError::TypeMismatch {
            detail: "cannot AND/OR/XOR inet values of different sizes".into(),
        });
    }
    let x = inet_addr_u128(*fa, aa);
    let y = inet_addr_u128(*fb, ab);
    let val = match op {
        BinOp::BitAnd => x & y,
        BinOp::BitOr => x | y,
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: "unsupported inet bitwise operator".into(),
            });
        }
    };
    Ok(inet_from_u128(*fa, (*ba).max(*bb), val))
}

/// v7.39 (read01 round 79) — PG's message for "no operator takes these two
/// types": `operator does not exist: text >= integer`. SPG used to print
/// `comparison between Some(Text) and Some(Int)` — a Rust `Option`/`Debug`
/// dump in a user-facing error, and nothing a driver could match on. The
/// SQLSTATE (42883) already keyed off PG's phrasing, so the wire class was
/// right while the text was not.
/// v7.39 (round 238) — the operator-resolution gate `=` already applies,
/// reusable by the constructs that compare WITHOUT going through `compare`:
/// `IS DISTINCT FROM`, `NULLIF` and `IN`. All three answered happily across
/// types PG has no operator for — `1 IS DISTINCT FROM 'a'::text` was `true`,
/// `nullif(1, 'a'::text)` was `1`, `1 IN (1, 'a'::text)` was `true` — so a
/// predicate that PG rejects outright silently decided a row's fate.
pub(super) fn require_comparable(
    op: BinOp,
    a: &Value<'_>,
    b: &Value<'_>,
) -> Result<(), EvalError> {
    if a.is_null() || b.is_null() {
        return Ok(());
    }
    // `compare` is the authority: if it can answer, the pair is comparable.
    match compare(op, a, b) {
        Ok(_) => Ok(()),
        Err(e) => Err(e),
    }
}

fn no_such_operator(op: BinOp, a: &Value<'_>, b: &Value<'_>) -> EvalError {
    // `pg_typeof_name` is the FULL value→type-name table (the one pg_typeof
    // itself answers from); `pg_typeof_name_for_datatype` covers only the
    // numeric/temporal subset and returns None for text, which is exactly the
    // type this error is most often about.
    EvalError::TypeMismatch {
        detail: alloc::format!(
            "operator does not exist: {} {op} {}",
            super::strings::pg_typeof_name(a),
            super::strings::pg_typeof_name(b)
        ),
    }
}

pub(super) fn compare(
    op: BinOp,
    l: &Value<'_>,
    r: &Value<'_>,
) -> Result<Value<'static>, EvalError> {
    // v7.39 (round 483) — same-variant scalars compare straight away.
    //
    // Round 482 removed the per-row `Value` churn and `compare` became the
    // dominant cost: 35.6 % of self time on `g = 5`, with `Value::data_type`
    // another 4.5 % — both spent on the pre-checks below before reaching
    // the ordering match at the bottom.
    //
    // None of those pre-checks can fire for these pairs. Each is gated on
    // one side being Text against a TYPED other (Text-vs-Text is explicitly
    // left alone), on `NumericBig`, or on `BpChar` — all different variants
    // from the ones matched here. So this is the same answer by the same
    // arms, reached without walking past four guards that cannot apply.
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => {
            return cmp_result(op, a.cmp(b));
        }
        (Value::BigInt(a), Value::BigInt(b)) => {
            return cmp_result(op, a.cmp(b));
        }
        (Value::SmallInt(a), Value::SmallInt(b)) => {
            return cmp_result(op, a.cmp(b));
        }
        (Value::Int(a), Value::BigInt(b)) => {
            return cmp_result(op, i64::from(*a).cmp(b));
        }
        (Value::BigInt(a), Value::Int(b)) => {
            return cmp_result(op, a.cmp(&i64::from(*b)));
        }
        (Value::Text(a), Value::Text(b)) => {
            return cmp_result(op, a.cmp(b));
        }
        (Value::Bool(a), Value::Bool(b)) => {
            return cmp_result(op, a.cmp(b));
        }
        _ => {}
    }
    // PG implicitly casts an unknown-type string literal to the other
    // operand's type (`i = '5'`, `1 = '1'`, `2 < '10'`). When one side is Text
    // and the other is a typed scalar, coerce the Text to that type first; a
    // Text that won't parse (`1 = 'abc'`) falls through to the type-mismatch
    // error, matching PG. Text-vs-Text stays a string comparison, and Bool has
    // its own arm, so both are left alone.
    let needs_text_coerce = |other: &Value<'_>| -> Option<DataType> {
        match other.data_type() {
            // Text-vs-Text is a string comparison; an untyped operand can't
            // drive a coercion. Everything else (numerics, bool, date/time,
            // inet, uuid, …) coerces the Text side to it — e.g. `true = 't'`.
            Some(DataType::Text) | None => None,
            dt => dt,
        }
    };
    if let (Value::Text(s), Some(dt)) = (l, needs_text_coerce(r)) {
        if let Ok(c) = crate::conversions::coerce_value(Value::text(s.as_ref()), dt, "", 0) {
            return compare(op, &c, r);
        }
    }
    if let (Some(dt), Value::Text(s)) = (needs_text_coerce(l), r) {
        if let Ok(c) = crate::conversions::coerce_value(Value::text(s.as_ref()), dt, "", 0) {
            return compare(op, l, &c);
        }
    }
    // v7.38 (read01, T3.C3) — a NUMERIC beyond i128 compares via bignum (the
    // finite path below would mis-read its canonical form).
    if matches!(l, Value::NumericBig(_)) || matches!(r, Value::NumericBig(_)) {
        if let (Some(a), Some(b)) = (value_to_bignum(l), value_to_bignum(r)) {
            return Ok(Value::Bool(cmp_to_bool(op, a.cmp(&b))));
        }
    }
    // v7.38 (read01, T11) — bpchar compares blank-insensitively: when either
    // side is bpchar, both text-like operands are compared with trailing blanks
    // trimmed (`'ab'::char(4) = 'ab'` is true).
    if matches!(l, Value::BpChar(_)) || matches!(r, Value::BpChar(_)) {
        let trimmed = |v: &Value<'_>| -> Option<alloc::string::String> {
            match v {
                Value::BpChar(s) | Value::Text(s) => Some(s.trim_end_matches(' ').to_string()),
                _ => None,
            }
        };
        if let (Some(a), Some(b)) = (trimmed(l), trimmed(r)) {
            return cmp_result(op, a.cmp(&b));
        }
    }
    let ord = match (l, r) {
        (Value::Int(a), Value::Int(b)) => i64::from(*a).cmp(&i64::from(*b)),
        (Value::Int(a), Value::BigInt(b)) => i64::from(*a).cmp(b),
        (Value::BigInt(a), Value::Int(b)) => a.cmp(&i64::from(*b)),
        (Value::BigInt(a), Value::BigInt(b)) => a.cmp(b),
        // v7.38 (read01) — NUMERIC vs NUMERIC / integer compares exactly on a
        // shared scale (bare decimal literals are numeric now, so `n = 9.99`
        // lands here). A numeric-vs-float mix still falls to the float arm.
        (a, b)
            if (matches!(a, Value::Numeric { .. } | Value::NumericBig(_))
                || matches!(b, Value::Numeric { .. } | Value::NumericBig(_)))
                && !matches!(a.data_type(), Some(DataType::Float))
                && !matches!(b.data_type(), Some(DataType::Float)) =>
        {
            // Inline widen (numeric_or_widen wants `&Value<'static>`; these
            // operands are shorter-lived but only their Copy fields are read).
            let widen = |v: &Value<'_>| -> Option<(i128, u16)> {
                match v {
                    Value::Numeric { scaled, scale, .. } => Some((*scaled, *scale)),
                    Value::Int(n) => Some((i128::from(*n), 0)),
                    Value::SmallInt(n) => Some((i128::from(*n), 0)),
                    Value::BigInt(n) => Some((i128::from(*n), 0)),
                    _ => None,
                }
            };
            // v7.39 (read01 round 79) — PG's wording, and without leaking Rust's
            // `Some(Text)` debug form into a user-facing error:
            // "operator does not exist: text >= integer".
            let mismatch = || no_such_operator(op, a, b);
            let (na, sa) = widen(a).ok_or_else(mismatch)?;
            let (nb, sb) = widen(b).ok_or_else(mismatch)?;
            let target = sa.max(sb);
            let lhs = rescale(na, sa, target).ok_or_else(|| EvalError::TypeMismatch {
                detail: "NUMERIC overflow on rescale".into(),
            })?;
            let rhs = rescale(nb, sb, target).ok_or_else(|| EvalError::TypeMismatch {
                detail: "NUMERIC overflow on rescale".into(),
            })?;
            lhs.cmp(&rhs)
        }
        (a, b)
            if matches!(a.data_type(), Some(DataType::Float | DataType::Real))
                || matches!(b.data_type(), Some(DataType::Float | DataType::Real)) =>
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
        // array elements order-sensitive, numbers by value). Equality
        // returns early because it is not expressible as an `Ordering`
        // (two unequal jsonb values can still compare `Equal` under no
        // total order at all).
        //
        // v7.39 (read01 round 76) — the ordering operators (< <= > >=)
        // now route to the same total order ORDER BY already sorts by
        // (`json::jsonb_compare`: the PG type-class ladder Object >
        // Array > Boolean > Number > String > Null).
        (Value::Json(a), Value::Json(b)) => {
            if matches!(op, BinOp::Eq | BinOp::NotEq) {
                let eq = crate::json::equals(l, r)?;
                return Ok(Value::Bool(if matches!(op, BinOp::Eq) { eq } else { !eq }));
            }
            let pa = crate::json::parse(a).map_err(|e| EvalError::TypeMismatch {
                detail: alloc::format!("invalid jsonb operand: {e:?}"),
            })?;
            let pb = crate::json::parse(b).map_err(|e| EvalError::TypeMismatch {
                detail: alloc::format!("invalid jsonb operand: {e:?}"),
            })?;
            crate::json::jsonb_compare(&pa, &pb)
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
        // v7.39 (read01 pg_lsn.c) — LSN ordering is plain u64.
        (Value::PgLsn(a), Value::PgLsn(b)) => a.cmp(b),
        // v7.39 (read01 char.c) — "char" compares by byte value.
        (Value::Char1(a), Value::Char1(b)) => a.cmp(b),
        // v7.39 (read01 ruleutils.c) — regclass compares by oid, including
        // against the synth catalogs' plain integer oid columns.
        (Value::Tid(b1, o1), Value::Tid(b2, o2)) => b1.cmp(b2).then(o1.cmp(o2)),
        (Value::Xid(x), Value::Xid(y)) => x.cmp(y),
        (Value::Cid(x), Value::Cid(y)) => x.cmp(y),
        (Value::RegClass(a, _), Value::RegClass(b, _)) => a.cmp(b),
        (Value::RegClass(a, _), Value::BigInt(b)) => a.cmp(b),
        (Value::BigInt(a), Value::RegClass(b, _)) => a.cmp(b),
        (Value::RegClass(a, _), Value::Int(b)) => a.cmp(&i64::from(*b)),
        (Value::Int(a), Value::RegClass(b, _)) => i64::from(*a).cmp(b),
        // v7.39 (round 342, V65) — regproc joins on oid the same way:
        // `pg_proc.oid = 'f'::regproc` is the point of carrying it.
        (Value::RegProc(a, _), Value::RegProc(b, _)) => a.cmp(b),
        (Value::RegProc(a, _), Value::BigInt(b)) => a.cmp(b),
        (Value::BigInt(a), Value::RegProc(b, _)) => a.cmp(b),
        (Value::RegProc(a, _), Value::Int(b)) => a.cmp(&i64::from(*b)),
        (Value::Int(a), Value::RegProc(b, _)) => i64::from(*a).cmp(b),
        // Text form compares by name (the pre-dual-shape contract).
        (Value::RegClass(_, a), Value::Text(b)) => a.as_ref().cmp(b.as_ref()),
        (Value::Text(a), Value::RegClass(_, b)) => a.as_ref().cmp(b.as_ref()),
        (Value::RegProc(_, a), Value::Text(b)) => a.as_ref().cmp(b.as_ref()),
        (Value::Text(a), Value::RegProc(_, b)) => a.as_ref().cmp(b.as_ref()),
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
            Value::Inet {
                family: af,
                bits: ab,
                addr: aa,
            },
            Value::Inet {
                family: bf,
                bits: bb,
                addr: ba,
            },
        )
        | (
            Value::Cidr {
                family: af,
                bits: ab,
                addr: aa,
            },
            Value::Cidr {
                family: bf,
                bits: bb,
                addr: ba,
            },
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
            Value::BitString {
                nbits: an,
                bytes: ab,
            },
            Value::BitString {
                nbits: bn,
                bytes: bb,
            },
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
            Value::Range {
                kind: ak,
                lower: al,
                upper: au,
                lower_inc: ali,
                upper_inc: aui,
                empty: ae,
            },
            Value::Range {
                kind: bk,
                lower: bl,
                upper: bu,
                lower_inc: bli,
                upper_inc: bui,
                empty: be,
            },
        ) => {
            let ord = range_cmp(
                RangeParts::new(*ak, al, au, *ali, *aui, *ae),
                RangeParts::new(*bk, bl, bu, *bli, *bui, *be),
            );
            return cmp_result(op, ord);
        }
        // v7.37 — MULTIRANGE comparison (=/<> and ordering). Normalise both
        // to PG's canonical form (canonicalise each span, drop empties, sort,
        // merge overlapping/adjacent) and compare the span lists lexically.
        (
            Value::Multirange {
                kind: ak,
                ranges: ar,
            },
            Value::Multirange {
                kind: bk,
                ranges: br,
            },
        ) => {
            return cmp_result(op, multirange_cmp(*ak, ar, *bk, br));
        }
        // v7.37 — TSVECTOR equality (=/<>). SPG stores lexemes sorted by
        // word + deduped with their (ascending) positions and weight, so a
        // structural compare matches PG (`'bar foo' = 'foo bar'`, position-
        // sensitive, deduped). Ordering (< etc.) is deferred.
        // v7.38 (read01, T12.4) — TSVECTOR total order. Equality stays
        // structural (position/weight sensitive); ordering follows PG: lexeme
        // count first, then per-lexeme by word (length, then bytes), then
        // positions, then weight.
        (Value::TsVector(a), Value::TsVector(b)) => {
            return match op {
                BinOp::Eq => Ok(Value::Bool(a == b)),
                BinOp::NotEq => Ok(Value::Bool(a != b)),
                _ => cmp_result(op, tsvector_total_cmp(a, b)),
            };
        }
        // v7.38 (read01, T12.4/R2) — TSQUERY total order. Equality stays
        // structural (PG does not normalise operand order: `'a & b' <> 'b & a'`);
        // ordering follows PG's non-recursive silly_cmp_tsquery.
        (Value::TsQuery(a), Value::TsQuery(b)) => {
            return match op {
                BinOp::Eq => Ok(Value::Bool(a == b)),
                BinOp::NotEq => Ok(Value::Bool(a != b)),
                _ => cmp_result(op, tsquery_total_cmp(a, b)),
            };
        }
        // v7.38 (read01, T21) — geometric comparison is by AREA: PG's box and
        // circle operators (=, <>, <, <=, >, >=) all compare areas, so
        // `<(0,0),5> = <(100,100),5>` is true (same radius → same area) and
        // `(0,0),(1,1) = (5,5),(6,6)` is true (same area, different position).
        (Value::Circle { radius: r1, .. }, Value::Circle { radius: r2, .. }) => {
            let area = |r: f64| core::f64::consts::PI * r * r;
            return cmp_result(op, float_pg_cmp(area(*r1), area(*r2)));
        }
        (Value::PgBox(aur, all), Value::PgBox(bur, bll)) => {
            let area = |ur: &spg_storage::Point2D, ll: &spg_storage::Point2D| {
                (ur.x - ll.x).abs() * (ur.y - ll.y).abs()
            };
            return cmp_result(op, float_pg_cmp(area(aur, all), area(bur, bll)));
        }
        // v7.39 (read01 rowtypes.c) — runtime composite comparison
        // (PG record_cmp: pairwise field order). The ROW-literal syntax
        // desugars earlier; this arm serves cast-produced composites.
        (Value::Composite(x), Value::Composite(y)) => {
            if x.len() != y.len() {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "cannot compare record types with different numbers of columns: {} vs {}",
                        x.len(),
                        y.len()
                    ),
                });
            }
            let mut ord = core::cmp::Ordering::Equal;
            for ((_, a), (_, b)) in x.iter().zip(y.iter()) {
                let c = crate::orderby::value_cmp(a, b);
                if c != core::cmp::Ordering::Equal {
                    ord = c;
                    break;
                }
            }
            ord
        }
        (a, b) => {
            return Err(no_such_operator(op, a, b));
        }
    };
    let result = match op {
        BinOp::Eq => ord.is_eq(),
        BinOp::NotEq => !ord.is_eq(),
        BinOp::Lt => ord.is_lt(),
        BinOp::LtEq => ord.is_le(),
        BinOp::Gt => ord.is_gt(),
        BinOp::GtEq => ord.is_ge(),
        BinOp::IntDiv
        | BinOp::And
        | BinOp::Or
        | BinOp::LogicalXor
        | BinOp::BitOr
        | BinOp::BitAnd
        | BinOp::BitXor
        | BinOp::GeomParallel
        | BinOp::OverLeft
        | BinOp::OverRight
        | BinOp::GeomPerp
        | BinOp::GeomSameAs
        | BinOp::ClosestPoint
        | BinOp::GeomHoriz
        | BinOp::Intersects
        | BinOp::IsBelow
        | BinOp::IsAbove
        | BinOp::PatternLt
        | BinOp::PatternLtEq
        | BinOp::PatternGt
        | BinOp::PatternGtEq
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
        | BinOp::JsonPathExists
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

/// v7.39 (round 238) — both arguments of a boolean connective must be
/// boolean, checked BEFORE the short-circuit. PG type-checks its arguments
/// up front, so `1 OR true` and `false AND 1` are refused; SPG decided the
/// answer from the other side first and let a non-boolean operand through.
fn require_boolean_argument(kw: &str, l: &Value<'_>, r: &Value<'_>) -> Result<(), EvalError> {
    for v in [l, r] {
        if !matches!(v, Value::Bool(_) | Value::Null) {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "argument of {kw} must be type boolean, not type {}",
                    super::strings::pg_typeof_name(v)
                ),
            });
        }
    }
    Ok(())
}

// SQL three-valued AND / OR.
pub(crate) fn and_3vl(l: Value<'static>, r: Value<'static>) -> Result<Value<'static>, EvalError> {
    require_boolean_argument("AND", &l, &r)?;
    match (l, r) {
        (Value::Bool(false), _) | (_, Value::Bool(false)) => Ok(Value::Bool(false)),
        (Value::Bool(true), Value::Bool(true)) => Ok(Value::Bool(true)),
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        // v7.39 (round 238) — PG names the offending side's type:
        // "argument of AND must be type boolean, not type integer".
        (a, b) => Err(EvalError::TypeMismatch {
            detail: format!(
                "argument of AND must be type boolean, not type {}",
                super::strings::pg_typeof_name(if matches!(a, Value::Bool(_)) { &b } else { &a })
            ),
        }),
    }
}

fn or_3vl(l: Value<'static>, r: Value<'static>) -> Result<Value<'static>, EvalError> {
    require_boolean_argument("OR", &l, &r)?;
    match (l, r) {
        (Value::Bool(true), _) | (_, Value::Bool(true)) => Ok(Value::Bool(true)),
        (Value::Bool(false), Value::Bool(false)) => Ok(Value::Bool(false)),
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (a, b) => Err(EvalError::TypeMismatch {
            detail: format!(
                "argument of OR must be type boolean, not type {}",
                super::strings::pg_typeof_name(if matches!(a, Value::Bool(_)) { &b } else { &a })
            ),
        }),
    }
}

/// v7.39 (read01 round 70) — for the three ARRAY operators (`@>`, `<@`, `&&`),
/// read a bare TEXT literal on either side as an array of the OTHER side's
/// type. PG's unknown-literal resolution, in the one place SPG needed it.
///
/// A text that does not parse as an array literal is left alone — it may well be
/// the JSON or inet reading of the same operator, which the dispatch below still
/// has to reach.
fn coerce_array_literal_operands(
    op: BinOp,
    l: Value<'static>,
    r: Value<'static>,
) -> (Value<'static>, Value<'static>) {
    // v7.39 (read01 round 71) — a RANGE beside a bare literal takes the same
    // rule: `r && '[4,11)'` is a range, not an inet operand. Same family as the
    // array case, found by the same sweep.
    if matches!(op, BinOp::InetOverlap) {
        if let (Value::Range { kind, .. }, Value::Text(_)) = (&l, &r) {
            let k = *kind;
            if let Ok(coerced) = crate::eval::cast::cast_value(
                r.clone(),
                spg_sql::ast::CastTarget::Named(range_type_name(k).into()),
            ) {
                return (l, coerced);
            }
        }
        if let (Value::Text(_), Value::Range { kind, .. }) = (&l, &r) {
            let k = *kind;
            if let Ok(coerced) = crate::eval::cast::cast_value(
                l.clone(),
                spg_sql::ast::CastTarget::Named(range_type_name(k).into()),
            ) {
                return (coerced, r);
            }
        }
    }
    if !matches!(
        op,
        BinOp::JsonContains | BinOp::JsonContainedBy | BinOp::InetOverlap
    ) {
        return (l, r);
    }
    let as_array_like = |v: &Value<'_>| array_scalar_elems(v).is_some();
    let cast_like = |text: Value<'static>, model: &Value<'_>| -> Option<Value<'static>> {
        let target = match model {
            Value::TextArray(_) => spg_sql::ast::CastTarget::TextArray,
            Value::IntArray(_) => spg_sql::ast::CastTarget::IntArray,
            Value::BigIntArray(_) => spg_sql::ast::CastTarget::BigIntArray,
            _ => return None,
        };
        crate::eval::cast::cast_value(text, target).ok()
    };
    match (&l, &r) {
        (_, Value::Text(_)) if as_array_like(&l) => match cast_like(r.clone(), &l) {
            Some(coerced) => (l, coerced),
            None => (l, r),
        },
        (Value::Text(_), _) if as_array_like(&r) => match cast_like(l.clone(), &r) {
            Some(coerced) => (coerced, r),
            None => (l, r),
        },
        _ => (l, r),
    }
}

/// v7.39 (read01 round 71) — the SQL name of a range kind, for coercing a bare
/// literal beside a range operand.
fn range_type_name(kind: spg_storage::RangeKind) -> &'static str {
    use spg_storage::RangeKind as K;
    match kind {
        K::Int4 => "int4range",
        K::Int8 => "int8range",
        K::Num => "numrange",
        K::Ts => "tsrange",
        K::TsTz => "tstzrange",
        K::Date => "daterange",
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
