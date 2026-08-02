//! Row / value ordering split out of `lib.rs` (lib.rs split 7): the
//! ORDER BY comparator stack (`order_by_value_cmp` with NULLS placement,
//! the `build_order_keys` / `sort_by_keys` / `partial_sort_tagged` /
//! `cmp_multi_key` tagged-sort pipeline, `apply_offset_and_limit[_tagged]`
//! windowing, and the `expand_group_by_all` / `resolve_order_by_position`
//! pre-passes), the generic value-comparison primitives (`value_cmp` /
//! `value_to_f64`) those comparators build on, and the histogram-bound
//! rendering helpers (`sort_values_for_histogram` / `render_histogram_bounds`
//! / `canonical_value_repr`) shared with the statistics path. Free
//! functions; the SELECT / aggregate / window / statistics paths drive them.

use alloc::string::ToString;
use alloc::vec::Vec;

use spg_sql::ast::{ColumnName, Expr, Literal, OrderBy, SelectItem, SelectStatement};
use spg_storage::{ColumnSchema, Row, Value};

use crate::conversions::{
    format_bigint_2d_text, format_hstore_str, format_int_2d_text, format_range_str,
    format_text_2d_text,
};
use crate::eval::{self, EvalContext};
use crate::{EngineError, aggregate, value_to_order_key};

#[allow(clippy::match_same_arms)] // explicit arms per type document the supported pairs
/// v7.24 (round-16 A) — per-key ORDER BY comparator honouring DESC
/// and the effective NULLS placement (explicit NULLS FIRST/LAST,
/// else the PG default: NULLS LAST for ASC, NULLS FIRST for DESC).
/// NULL placement is absolute — it does not flip with DESC.
pub(crate) fn order_by_value_cmp(
    desc: bool,
    nulls_first: Option<bool>,
    a: &Value,
    b: &Value,
) -> core::cmp::Ordering {
    order_by_value_cmp_in(desc, nulls_first, a, b, false)
}

/// v7.39 (round 364, M4 P2) — ORDER BY comparator with the session
/// dialect. On a MySQL session, text sorts by its FOLDED form (accent-
/// and case-insensitive), so `bar`/`Bär` sort adjacent and before
/// `foo`/`Foo`. Values that fold equal keep an implementation-defined
/// order between them (MariaDB does too — GROUP_CONCAT's tie order among
/// collation-equal values is not specified).
pub(crate) fn order_by_value_cmp_in(
    desc: bool,
    nulls_first: Option<bool>,
    a: &Value,
    b: &Value,
    mysql: bool,
) -> core::cmp::Ordering {
    order_by_value_cmp_coll(desc, nulls_first, a, b, mysql, None)
}

/// v7.39 (round 686) — the value-comparing family, with a collation.
pub(crate) fn order_by_value_cmp_coll(
    desc: bool,
    nulls_first: Option<bool>,
    a: &Value,
    b: &Value,
    mysql: bool,
    collation: Option<&str>,
) -> core::cmp::Ordering {
    if let (Value::Text(x), Value::Text(y), Some(c)) = (a, b, collation)
        && let Some(ord) = crate::collate::compare(c, x, y)
    {
        return if desc { ord.reverse() } else { ord };
    }
    order_by_value_cmp_raw(desc, nulls_first, a, b, mysql)
}

fn order_by_value_cmp_raw(
    desc: bool,
    nulls_first: Option<bool>,
    a: &Value,
    b: &Value,
    mysql: bool,
) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    // v7.39 (round 403) — MySQL treats NULL as the SMALLEST value, so its
    // default is NULLS FIRST for ASC / NULLS LAST for DESC; PG's default is
    // the reverse (NULLS LAST for ASC / FIRST for DESC). An explicit
    // NULLS FIRST/LAST still wins.
    let nf = nulls_first.unwrap_or(if mysql { !desc } else { desc });

    match (matches!(a, Value::Null), matches!(b, Value::Null)) {
        (true, true) => Ordering::Equal,
        (true, false) => {
            if nf {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (false, true) => {
            if nf {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (false, false) => {
            let c = if mysql {
                if let (Value::Text(x), Value::Text(y)) = (a, b) {
                    spg_storage::mysql_compare_fold(x).cmp(&spg_storage::mysql_compare_fold(y))
                } else {
                    value_cmp(a, b)
                }
            } else {
                value_cmp(a, b)
            };
            if desc { c.reverse() } else { c }
        }
    }
}

/// v7.38 (read01, T3.C3) — order a pair when a NUMERIC beyond i128 is involved,
/// via exact BigNumeric comparison; `None` when neither side is big (the finite
/// comparators handle those). Shared by the ORDER BY / aggregate / min-max
/// comparators.
pub(crate) fn numeric_bignum_cmp(a: &Value, b: &Value) -> Option<core::cmp::Ordering> {
    use spg_storage::bignum::BigNumeric;
    if !matches!(a, Value::NumericBig(_)) && !matches!(b, Value::NumericBig(_)) {
        return None;
    }
    let to_big = |v: &Value| -> Option<BigNumeric> {
        Some(match v {
            Value::SmallInt(n) => BigNumeric::from_i128(i128::from(*n), 0),
            Value::Int(n) => BigNumeric::from_i128(i128::from(*n), 0),
            Value::BigInt(n) => BigNumeric::from_i128(i128::from(*n), 0),
            Value::Numeric {
                scaled,
                scale,
                kind,
            } if *kind == spg_storage::NumericKind::Finite => {
                BigNumeric::from_i128(*scaled, *scale)
            }
            Value::NumericBig(b) => (**b).clone(),
            _ => return None,
        })
    };
    match (to_big(a), to_big(b)) {
        (Some(x), Some(y)) => Some(x.cmp(&y)),
        _ => None,
    }
}

/// v7.39 (round 643, F32) — there are FOUR value comparators in the
/// engine: this one, `eval::values::value_cmp` (GREATEST / LEAST),
/// `aggregate::value_cmp` (min / max) and the ordering match inside
/// `eval::binop::compare` (the operators). They cover different variant
/// sets and that looks like drift, but converging them is not a
/// mechanical merge: **each has a different fallback**, and the
/// fallback is what covers the variants the arm list omits.
///
/// This one falls back to comparing `format!("{:?}")` of the two
/// values. `eval::values::value_cmp` delegates to the operator
/// comparison instead. `aggregate::value_cmp` has its own.
///
/// Round 643 probed the difference — UUID / bytea / macaddr / time /
/// money / inet under both ORDER BY and min/max, plus a TIME pair
/// constructed specifically to break a Debug-string ordering
/// (3_600_000_000 against 32_400_000_000, where string order and
/// numeric order disagree) — and SPG matched PG18 on every one. ORDER
/// BY extracts a numeric key before reaching here, which is why.
///
/// So the drift is latent, and a union would CHANGE behaviour on the
/// paths where a fallback currently answers. If this is converged, the
/// shape is a shared `Option<Ordering>` core with each caller keeping
/// its own fallback — not one function with the union of the arms.
pub(crate) fn value_cmp(a: &Value, b: &Value) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    // v7.39 (round 485) — a same-variant scalar answers here, ahead of the
    // two gates below, which is where DISTINCT and ORDER BY spend a call
    // per row. Neither gate can fire on these pairs: `numeric_bignum_cmp`
    // returns None unless a side is `NumericBig`, and the NumericKind rank
    // block only returns when a side is non-finite, which an integer, a
    // string, and a boolean never are. Each pair lands on exactly the arm
    // it lands on today — the same comparison, without first walking past
    // gates that cannot apply. (Same argument as round 483 made for
    // `binop::compare`, in the ordering comparator this time.)
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => return x.cmp(y),
        (Value::BigInt(x), Value::BigInt(y)) => return x.cmp(y),
        (Value::SmallInt(x), Value::SmallInt(y)) => return x.cmp(y),
        (Value::Text(x), Value::Text(y)) => return x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => return x.cmp(y),
        _ => {}
    }
    // v7.38 (read01, T3.C3) — a NUMERIC beyond i128 orders via exact bignum.
    if let Some(ord) = numeric_bignum_cmp(a, b) {
        return ord;
    }
    // v7.38 (read01, T6.P3) — a NUMERIC special orders by the total order
    // -Inf < finite < +Inf < NaN (NaN == NaN), ahead of the finite arms which
    // would read a special's canonical 0 as the number 0.
    //
    // v7.37.16 — FLOAT specials ride the same rank. The float arms below used
    // `partial_cmp().unwrap_or(Equal)`, so a float NaN compared Equal to EVERY
    // number: ORDER BY interleaved NaNs mid-stream and DISTINCT swallowed
    // every float that followed a NaN (a non-transitive "equality"). PG's
    // float8 total order is -Inf < finite < +Inf < NaN with NaN = NaN (and
    // -0 = 0, which partial_cmp already gives on the finite path). Routing
    // float NaN/±Inf through the NumericKind rank fixes both, and also closes
    // the `Float(0.0) == Numeric-NaN` hole (the special's canonical scaled=0
    // used to reach the Numeric↔Float f64 arm). Every float arm below is now
    // reached only with both sides finite, where partial_cmp is total.
    {
        use spg_storage::NumericKind as NK;
        let kind = |v: &Value| -> Option<NK> {
            match v {
                Value::Numeric { kind, .. } => Some(*kind),
                Value::Int(_) | Value::BigInt(_) | Value::SmallInt(_) => Some(NK::Finite),
                Value::Float(x) => Some(if x.is_nan() {
                    NK::NaN
                } else if *x == f64::INFINITY {
                    NK::PosInf
                } else if *x == f64::NEG_INFINITY {
                    NK::NegInf
                } else {
                    NK::Finite
                }),
                // v7.37.16 — REAL (f32) joins the same rank: it had NO
                // value_cmp arm at all and fell to the debug-string
                // fallback, which ordered "Infinity" above "NaN" (wrong
                // vs PG) and only equated bit-identical payloads.
                Value::Real(x) => Some(if x.is_nan() {
                    NK::NaN
                } else if *x == f32::INFINITY {
                    NK::PosInf
                } else if *x == f32::NEG_INFINITY {
                    NK::NegInf
                } else {
                    NK::Finite
                }),
                _ => None,
            }
        };
        if let (Some(lk), Some(rk)) = (kind(a), kind(b)) {
            if lk != NK::Finite || rk != NK::Finite {
                let rank = |k: NK| match k {
                    NK::NegInf => -2,
                    NK::Finite => 0,
                    NK::PosInf => 1,
                    NK::NaN => 2,
                };
                return rank(lk).cmp(&rank(rk));
            }
        }
    }
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::BigInt(x), Value::BigInt(y)) => x.cmp(y),
        (Value::SmallInt(x), Value::SmallInt(y)) => x.cmp(y),
        // Cross integer widths — an ORDER BY key can mix variants
        // (literal-seeded rows, integer casts). Widen and compare by
        // value, not by the debug string (which would sort "1000"
        // before "9").
        (Value::SmallInt(x), Value::Int(y)) => i32::from(*x).cmp(y),
        (Value::Int(x), Value::SmallInt(y)) => x.cmp(&i32::from(*y)),
        (Value::SmallInt(x), Value::BigInt(y)) => i64::from(*x).cmp(y),
        (Value::BigInt(x), Value::SmallInt(y)) => x.cmp(&i64::from(*y)),
        (Value::Int(x), Value::BigInt(y)) => i64::from(*x).cmp(y),
        (Value::BigInt(x), Value::Int(y)) => x.cmp(&i64::from(*y)),
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        // v7.38 (read01, T11) — bpchar orders blank-insensitively (either side).
        (Value::BpChar(x), Value::BpChar(y)) => {
            x.trim_end_matches(' ').cmp(y.trim_end_matches(' '))
        }
        (Value::BpChar(x), Value::Text(y)) => x.trim_end_matches(' ').cmp(y.trim_end_matches(' ')),
        (Value::Text(x), Value::BpChar(y)) => x.trim_end_matches(' ').cmp(y.trim_end_matches(' ')),
        // v7.38 (read01 P6.24) — jsonb sorts by PG's type-aware total order
        // (Null<String<Number<Boolean<Array<Object, recursive), not by its
        // text spelling. Fall back to text only if either side fails to parse.
        (Value::Json(x), Value::Json(y)) => match (crate::json::parse(x), crate::json::parse(y)) {
            (Ok(jx), Ok(jy)) => crate::json::jsonb_compare(&jx, &jy),
            _ => x.cmp(y),
        },
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        // v7.37.16 — REAL finite arms (specials were ranked above).
        // Cross-family compares widen to f64, PG's float4↔float8/int
        // promotion (`1.5::float4 = 1.5::float8` and `2 = 2.0::float4`
        // are both true).
        (Value::Real(x), Value::Real(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Value::Real(x), Value::Float(y)) => {
            f64::from(*x).partial_cmp(y).unwrap_or(Ordering::Equal)
        }
        (Value::Float(x), Value::Real(y)) => {
            x.partial_cmp(&f64::from(*y)).unwrap_or(Ordering::Equal)
        }
        (Value::SmallInt(n), Value::Real(x)) => f64::from(*n)
            .partial_cmp(&f64::from(*x))
            .unwrap_or(Ordering::Equal),
        (Value::Real(x), Value::SmallInt(n)) => f64::from(*x)
            .partial_cmp(&f64::from(*n))
            .unwrap_or(Ordering::Equal),
        (Value::Int(n), Value::Real(x)) => f64::from(*n)
            .partial_cmp(&f64::from(*x))
            .unwrap_or(Ordering::Equal),
        (Value::Real(x), Value::Int(n)) => f64::from(*x)
            .partial_cmp(&f64::from(*n))
            .unwrap_or(Ordering::Equal),
        #[allow(clippy::cast_precision_loss)]
        (Value::BigInt(n), Value::Real(x)) => (*n as f64)
            .partial_cmp(&f64::from(*x))
            .unwrap_or(Ordering::Equal),
        #[allow(clippy::cast_precision_loss)]
        (Value::Real(x), Value::BigInt(n)) => f64::from(*x)
            .partial_cmp(&(*n as f64))
            .unwrap_or(Ordering::Equal),
        (
            Value::Numeric {
                scaled: xs,
                scale: xsc,
                ..
            },
            Value::Real(y),
        ) => numeric_to_f64(*xs, *xsc)
            .partial_cmp(&f64::from(*y))
            .unwrap_or(Ordering::Equal),
        (
            Value::Real(x),
            Value::Numeric {
                scaled: ys,
                scale: ysc,
                ..
            },
        ) => f64::from(*x)
            .partial_cmp(&numeric_to_f64(*ys, *ysc))
            .unwrap_or(Ordering::Equal),
        // Mixed integer / float — widen the integer to f64.
        (Value::SmallInt(n), Value::Float(x)) => {
            f64::from(*n).partial_cmp(x).unwrap_or(Ordering::Equal)
        }
        (Value::Float(x), Value::SmallInt(n)) => {
            x.partial_cmp(&f64::from(*n)).unwrap_or(Ordering::Equal)
        }
        (Value::Int(n), Value::Float(x)) => f64::from(*n).partial_cmp(x).unwrap_or(Ordering::Equal),
        (Value::Float(x), Value::Int(n)) => {
            x.partial_cmp(&f64::from(*n)).unwrap_or(Ordering::Equal)
        }
        #[allow(clippy::cast_precision_loss)]
        (Value::BigInt(n), Value::Float(x)) => {
            (*n as f64).partial_cmp(x).unwrap_or(Ordering::Equal)
        }
        #[allow(clippy::cast_precision_loss)]
        (Value::Float(x), Value::BigInt(n)) => {
            x.partial_cmp(&(*n as f64)).unwrap_or(Ordering::Equal)
        }
        // Exact decimal — align scales, then compare the integral
        // forms. The old `_ => debug-string` fallback sorted
        // `12.50 < 5.25 < 99.99` (lexicographic on `Numeric { scaled:
        // .. }`), corrupting `ORDER BY numeric_col` and every
        // aggregate-internal `ORDER BY` over a NUMERIC key.
        (
            Value::Numeric {
                scaled: xs,
                scale: xsc,
                ..
            },
            Value::Numeric {
                scaled: ys,
                scale: ysc,
                ..
            },
        ) => cmp_numeric(*xs, *xsc, *ys, *ysc),
        // Mixed exact-decimal ↔ integer — promote the integer to a
        // NUMERIC at scale 0 and compare exactly. Mirrors the int→numeric
        // promotion `apply_binary_numeric` (binop.rs `numeric_or_widen`)
        // uses for arithmetic + WHERE comparison, so ORDER BY / min / max
        // over a mixed NUMERIC/int key orders consistently with them.
        (
            Value::Numeric {
                scaled: xs,
                scale: xsc,
                ..
            },
            Value::SmallInt(y),
        ) => cmp_numeric(*xs, *xsc, i128::from(*y), 0),
        (
            Value::SmallInt(x),
            Value::Numeric {
                scaled: ys,
                scale: ysc,
                ..
            },
        ) => cmp_numeric(i128::from(*x), 0, *ys, *ysc),
        (
            Value::Numeric {
                scaled: xs,
                scale: xsc,
                ..
            },
            Value::Int(y),
        ) => cmp_numeric(*xs, *xsc, i128::from(*y), 0),
        (
            Value::Int(x),
            Value::Numeric {
                scaled: ys,
                scale: ysc,
                ..
            },
        ) => cmp_numeric(i128::from(*x), 0, *ys, *ysc),
        (
            Value::Numeric {
                scaled: xs,
                scale: xsc,
                ..
            },
            Value::BigInt(y),
        ) => cmp_numeric(*xs, *xsc, i128::from(*y), 0),
        (
            Value::BigInt(x),
            Value::Numeric {
                scaled: ys,
                scale: ysc,
                ..
            },
        ) => cmp_numeric(i128::from(*x), 0, *ys, *ysc),
        // Mixed exact-decimal ↔ float — PG demotes NUMERIC to float8 for
        // `numeric op double precision` (the float_path in
        // `apply_binary_numeric`), so compare as f64 with the same
        // NaN-as-Equal fallback the Float arms above use.
        (
            Value::Numeric {
                scaled: xs,
                scale: xsc,
                ..
            },
            Value::Float(y),
        ) => numeric_to_f64(*xs, *xsc)
            .partial_cmp(y)
            .unwrap_or(Ordering::Equal),
        (
            Value::Float(x),
            Value::Numeric {
                scaled: ys,
                scale: ysc,
                ..
            },
        ) => x
            .partial_cmp(&numeric_to_f64(*ys, *ysc))
            .unwrap_or(Ordering::Equal),
        (Value::Date(x), Value::Date(y)) => x.cmp(y),
        (Value::Timestamp(x), Value::Timestamp(y)) => x.cmp(y),
        // v7.39 (round 673) — the rest of what `aggregate`'s `value_cmp`
        // carries and this one did not. Every one of these fell to the
        // canonical-text fallback, which sorts by how a value PRINTS:
        //
        //   ORDER BY inet   10.0.0.10, 10.0.0.100, 10.0.0.9   (PG: .9, .10, .100)
        //   ORDER BY money  $10.00, $100.00, $9.00            (PG: 9, 10, 100)
        //   ORDER BY bytea  64, 0a, 09                        (PG: 09, 0a, 64)
        //   ORDER BY uuid   0000000a, 00000064, 00000009      (PG: 09, 0a, 64)
        //
        // Round 672 measured these as "fine" and was wrong: its probe used
        // 2/5/9, single digits that sort identically by text and by value.
        // 9/10/100 is what tells them apart.
        //
        // The comment forty lines below already recorded this exact failure
        // for NUMERIC — "the old `_ => debug-string` fallback sorted
        // 12.50 < 5.25 < 99.99, corrupting ORDER BY numeric_col". That fix
        // named one type; the fallback kept the rest.
        (Value::Money(x), Value::Money(y)) => x.cmp(y),
        (Value::Bytes(x), Value::Bytes(y)) => x.as_ref().cmp(y.as_ref()),
        (Value::Uuid(x), Value::Uuid(y)) => x.cmp(y),
        (Value::Macaddr(x), Value::Macaddr(y)) => x.cmp(y),
        (Value::Macaddr8(x), Value::Macaddr8(y)) => x.cmp(y),
        (
            Value::Inet {
                family: xf,
                bits: xb,
                addr: xa,
            },
            Value::Inet {
                family: yf,
                bits: yb,
                addr: ya,
            },
        )
        | (
            Value::Cidr {
                family: xf,
                bits: xb,
                addr: xa,
            },
            Value::Cidr {
                family: yf,
                bits: yb,
                addr: ya,
            },
        ) => (xf, xa, xb).cmp(&(yf, ya, yb)),
        // v7.39 (round 672) — TIME was missing HERE while `aggregate`'s own
        // `value_cmp` had it, so `ORDER BY time_col` fell to the catch-all
        // and did not sort at all. Measured: three rows inserted 09/02/05
        // came back 05,09,02 where PG gives 02,05,09.
        //
        // Two independently written comparison matrices, each missing types
        // the other carries — the same shape F32 recorded for the four
        // sum/avg accumulators. `docs/COLLATION_RFC.md` §3 records
        // converging them as the structural fix; this is the measured half.
        (Value::Time(x), Value::Time(y)) => x.cmp(y),
        // v7.39 (read01 pg_lsn.c) — LSN ordering is plain u64.
        (Value::PgLsn(x), Value::PgLsn(y)) => x.cmp(y),
        // v7.39 (read01 char.c) — "char" orders by byte value.
        (Value::Char1(x), Value::Char1(y)) => x.cmp(y),
        // v7.39 (read01 ruleutils.c) — regclass orders by oid.
        // v7.39 (round 511) — a tid orders by block then offset, which is
        // what makes `min(ctid)` pick `(0,2)` out of `(0,2) (0,9) (0,10)`.
        (Value::Tid(b1, o1), Value::Tid(b2, o2)) => b1.cmp(b2).then(o1.cmp(o2)),
        (Value::Xid(x), Value::Xid(y)) => x.cmp(y),
        (Value::Cid(x), Value::Cid(y)) => x.cmp(y),
        (Value::RegClass(x, _), Value::RegClass(y, _)) => x.cmp(y),
        (Value::RegClass(x, _), Value::BigInt(y)) => x.cmp(y),
        (Value::BigInt(x), Value::RegClass(y, _)) => x.cmp(y),
        // v7.39 (round 342, V65) — regproc compares by oid, same as
        // regclass, so `ORDER BY 'f'::regproc` and a join against
        // `pg_proc.oid` both behave.
        // v7.39 (round 648) — one arm per shape, all three reg types in
        // it; see the note in `eval::binop`'s comparator for what a
        // per-type arm cost.
        (
            Value::RegProc(x, _) | Value::RegType(x, _),
            Value::RegProc(y, _) | Value::RegType(y, _),
        ) => x.cmp(y),
        (Value::RegProc(x, _) | Value::RegType(x, _), Value::BigInt(y)) => x.cmp(y),
        (Value::BigInt(x), Value::RegProc(y, _) | Value::RegType(y, _)) => x.cmp(y),
        // v7.39 (read01 orderedsetaggs.c, found via interval percentile) —
        // INTERVAL had no arm and fell to the debug-string fallback, which
        // ordered by the decimal rendering of `micros` (so 4h < 1h < 2h) —
        // ORDER BY on an interval column was wrong everywhere. PG's
        // interval_cmp compares the normalized span: a month is 30 days,
        // a day 24 hours.
        (
            Value::Interval {
                months: xm,
                days: xd,
                micros: xu,
            },
            Value::Interval {
                months: ym,
                days: yd,
                micros: yu,
            },
        ) => {
            let span = |m: i32, d: i32, u: i64| -> i128 {
                (i128::from(m) * 30 + i128::from(d)) * 86_400_000_000 + i128::from(u)
            };
            span(*xm, *xd, *xu).cmp(&span(*ym, *yd, *yu))
        }
        // Cross-type compare: fall back to the debug rendering —
        // same-partition is the goal, exact order is irrelevant.
        _ => alloc::format!("{a:?}").cmp(&alloc::format!("{b:?}")),
    }
}

/// Order two exact-decimal values by aligning their scales to the
/// larger of the two, then comparing the integral representations.
/// `i128` headroom covers every in-range `NUMERIC`; on the rare
/// overflow of the alignment multiply we fall back to an `f64`
/// comparison (imprecise but never panics).
pub(crate) fn cmp_numeric(xs: i128, xsc: u16, ys: i128, ysc: u16) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    let max_scale = xsc.max(ysc);
    let widen = |v: i128, sc: u16| -> Option<i128> {
        10i128
            .checked_pow(u32::from(max_scale - sc))
            .and_then(|f| v.checked_mul(f))
    };
    match (widen(xs, xsc), widen(ys, ysc)) {
        (Some(a), Some(b)) => a.cmp(&b),
        _ => {
            let af = xs as f64 / 10f64.powi(i32::from(xsc));
            let bf = ys as f64 / 10f64.powi(i32::from(ysc));
            af.partial_cmp(&bf).unwrap_or(Ordering::Equal)
        }
    }
}

/// Demote an exact-decimal `(scaled, scale)` pair to `f64`, matching PG's
/// `numeric op float8` demotion (and the f64 fallback inside
/// `cmp_numeric`). Used by the mixed NUMERIC↔Float comparison arms.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn numeric_to_f64(scaled: i128, scale: u16) -> f64 {
    scaled as f64 / 10f64.powi(i32::from(scale))
}

pub(crate) fn value_to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::SmallInt(n) => Some(f64::from(*n)),
        Value::Int(n) => Some(f64::from(*n)),
        #[allow(clippy::cast_precision_loss)]
        Value::BigInt(n) => Some(*n as f64),
        Value::Float(x) => Some(*x),
        Value::Real(x) => Some(f64::from(*x)),
        _ => None,
    }
}

/// v6.2.0 — total ordering on `Value`s used by ANALYZE to sort a
/// column's non-NULL sample before histogram building. Cross-type
/// pairs (Int vs Float, Date vs Timestamp, …) compare via the
/// same widening the eval-side `compare` operator uses; everything
/// else (the genuinely-incompatible pairs) falls back to ordering
/// by canonical string form so the sort is still total + stable.
/// Vector / SQ8 / Half / Json / Numeric / Interval values reach
/// here only via the string-fallback path because vector columns
/// are filtered out upstream.
pub(crate) fn sort_values_for_histogram(a: &Value, b: &Value) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    match (a, b) {
        (Value::SmallInt(a), Value::SmallInt(b)) => a.cmp(b),
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        (Value::BigInt(a), Value::BigInt(b)) => a.cmp(b),
        (Value::SmallInt(a), Value::Int(b)) => i32::from(*a).cmp(b),
        (Value::Int(a), Value::SmallInt(b)) => a.cmp(&i32::from(*b)),
        (Value::Int(a), Value::BigInt(b)) => i64::from(*a).cmp(b),
        (Value::BigInt(a), Value::Int(b)) => a.cmp(&i64::from(*b)),
        (Value::SmallInt(a), Value::BigInt(b)) => i64::from(*a).cmp(b),
        (Value::BigInt(a), Value::SmallInt(b)) => a.cmp(&i64::from(*b)),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
        (Value::Text(a), Value::Text(b)) => a.cmp(b),
        // v7.38 (read01 P6.24) — jsonb sorts by PG's type-aware total order
        // (Null<String<Number<Boolean<Array<Object, recursive), not by its
        // text spelling. Fall back to text order only if either side fails to
        // parse (should not happen for stored jsonb).
        (Value::Json(a), Value::Json(b)) => match (crate::json::parse(a), crate::json::parse(b)) {
            (Ok(ja), Ok(jb)) => crate::json::jsonb_compare(&ja, &jb),
            _ => a.cmp(b),
        },
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Value::Date(a), Value::Date(b)) => a.cmp(b),
        (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
        // Mixed numeric/float — widen to f64 and compare.
        (Value::SmallInt(n), Value::Float(x)) => {
            (f64::from(*n)).partial_cmp(x).unwrap_or(Ordering::Equal)
        }
        (Value::Float(x), Value::SmallInt(n)) => {
            x.partial_cmp(&f64::from(*n)).unwrap_or(Ordering::Equal)
        }
        (Value::Int(n), Value::Float(x)) => {
            (f64::from(*n)).partial_cmp(x).unwrap_or(Ordering::Equal)
        }
        (Value::Float(x), Value::Int(n)) => {
            x.partial_cmp(&f64::from(*n)).unwrap_or(Ordering::Equal)
        }
        (Value::BigInt(n), Value::Float(x)) => {
            #[allow(clippy::cast_precision_loss)]
            let nf = *n as f64;
            nf.partial_cmp(x).unwrap_or(Ordering::Equal)
        }
        (Value::Float(x), Value::BigInt(n)) => {
            #[allow(clippy::cast_precision_loss)]
            let nf = *n as f64;
            x.partial_cmp(&nf).unwrap_or(Ordering::Equal)
        }
        // Cross-type fallback: lexicographic on canonical form.
        // Total + stable so the sort is well-defined.
        _ => canonical_value_repr(a).cmp(&canonical_value_repr(b)),
    }
}

/// v6.2.0 — render the histogram bounds list as a `[v0, v1, ...]`
/// string for the `spg_statistic.histogram_bounds` column. Values
/// containing `,` or `[` / `]` are JSON-style escaped so the
/// rendering round-trips through a future parser; v6.2.0 only
/// uses the rendered form for human consumption, so the escaping
/// is conservative.
pub(crate) fn render_histogram_bounds(bounds: &[alloc::string::String]) -> alloc::string::String {
    let mut out = alloc::string::String::with_capacity(bounds.len() * 8 + 2);
    out.push('[');
    for (i, b) in bounds.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let needs_quote = b.contains([',', '[', ']', '"']) || b.is_empty();
        if needs_quote {
            out.push('"');
            for ch in b.chars() {
                if ch == '"' || ch == '\\' {
                    out.push('\\');
                }
                out.push(ch);
            }
            out.push('"');
        } else {
            out.push_str(b);
        }
    }
    out.push(']');
    out
}

/// v6.2.0 — canonical textual form of a `Value` for histogram
/// bound storage. Strings used by ANALYZE for sort + bound output.
/// INT / BIGINT → decimal; FLOAT → shortest-round-trip via
/// `{:?}`; TEXT pass-through; BOOL → `t` / `f`; DATE / TIMESTAMP →
/// the same form `format_date` / `format_timestamp` produce for
/// SQL Display. Vector / SQ8 / Half / Json / Numeric / Interval
/// reach this only via a non-Vector column (vector columns are
/// skipped upstream); they fall back to a Debug-derived form so
/// stats still serialise without crashing.
pub(crate) fn canonical_value_repr(v: &Value) -> alloc::string::String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::SmallInt(n) => alloc::format!("{n}"),
        Value::Int(n) => alloc::format!("{n}"),
        Value::BigInt(n) => alloc::format!("{n}"),
        Value::Float(x) => alloc::format!("{x:?}"),
        Value::Text(s) | Value::Json(s) => s.to_string(),
        // v7.38 (read01, T11) — bpchar dedups/orders blank-insensitively.
        Value::BpChar(s) => s.trim_end_matches(' ').to_string(),
        Value::Bool(b) => if *b { "t" } else { "f" }.to_string(),
        Value::Date(d) => eval::format_date(*d),
        Value::Timestamp(t) => eval::format_timestamp(*t),
        // v7.17.0 Phase 3.P0-32 — PG TIME canonical text form.
        Value::Time(us) => eval::format_time(*us),
        // v7.17.0 Phase 3.P0-33 — MySQL YEAR 4-digit zero-padded.
        Value::Year(y) => alloc::format!("{y:04}"),
        // v7.17.0 Phase 3.P0-34 — PG TIMETZ canonical text form.
        Value::TimeTz { us, offset_secs } => eval::format_timetz(*us, *offset_secs),
        // v7.17.0 Phase 3.P0-35 — PG MONEY canonical en_US text form.
        Value::Money(c) => eval::format_money(*c),
        // v7.17.0 Phase 3.P0-38 — PG range canonical text form.
        v @ Value::Range { .. } => format_range_str(v),
        // v7.17.0 Phase 3.P0-39 — PG hstore canonical text form.
        Value::Hstore(pairs) => format_hstore_str(pairs),
        // v7.17.0 Phase 3.P0-40 — 2D array canonical text form.
        Value::IntArray2D(rows) => format_int_2d_text(rows),
        Value::BigIntArray2D(rows) => format_bigint_2d_text(rows),
        Value::TextArray2D(rows) => format_text_2d_text(rows),
        Value::Interval {
            months,
            days,
            micros,
        } => eval::format_interval(*months, *days, *micros),
        Value::Numeric {
            scaled,
            scale,
            kind,
        } => eval::format_numeric_kind(*kind, *scaled, *scale),
        Value::Vector(_) | Value::Sq8Vector(_) | Value::HalfVector(_) => {
            // Unreachable in practice (vector columns are filtered
            // out before this). Defensive fallback so a future
            // vector-stats path doesn't crash.
            alloc::format!("{v:?}")
        }
        // v7.5.0 — Value is #[non_exhaustive] for downstream
        // forward-compat. Future variants fall through to Debug
        // form here (same shape as the vector fallback above).
        _ => alloc::format!("{v:?}"),
    }
}

/// `ORDER BY <integer>` references the N-th SELECT item (1-based).
/// Swap the integer literal for the matching item's expression so the
/// executor doesn't need a special-case branch. Recurses into UNION
/// peers because each peer keeps its own SELECT list.
/// v6.4.1 — expand `GROUP BY ALL` to every non-aggregate SELECT-list
/// item. Mirrors DuckDB / PG 19 semantics. Wildcards (`SELECT * …`)
/// are NOT expanded by GROUP BY ALL (PG 19 leaves the wildcard intact
/// and groups by whatever explicit non-aggregates remain — none in
/// the wildcard-only case, which still works for non-aggregate
/// queries).
pub(crate) fn expand_group_by_all(s: &mut SelectStatement) {
    if !s.group_by_all {
        for (_, peer) in &mut s.unions {
            expand_group_by_all(peer);
        }
        return;
    }
    let mut groups: Vec<Expr> = Vec::new();
    for item in &s.items {
        if let SelectItem::Expr { expr, .. } = item
            && !aggregate::contains_aggregate(expr)
        {
            groups.push(expr.clone());
        }
    }
    s.group_by = Some(groups);
    s.group_by_all = false;
    for (_, peer) in &mut s.unions {
        expand_group_by_all(peer);
    }
}

/// v7.39 (round 185) — the implicit output label PG gives a
/// projection with no AS: a cast keeps its inner expression's label,
/// a function call is named after the function, a bare column keeps
/// its name. Anything else (PG's `?column?` family) returns None so
/// it never accidentally matches an ORDER BY identifier.
fn implicit_output_label(e: &Expr) -> Option<&str> {
    match e {
        Expr::Cast { expr, .. } => implicit_output_label(expr),
        Expr::FunctionCall { name, .. } => Some(name.as_str()),
        Expr::Column(c) => Some(c.name.as_str()),
        _ => None,
    }
}

/// v7.39 (round 529) — does an ORDER BY key name a SELECT-list alias
/// that has not been substituted yet?
///
/// `resolve_order_by_position` runs once per STATEMENT, so a SELECT
/// nested in a FROM clause, a CTE or a scalar subquery never got it:
/// `SELECT * FROM (SELECT v AS w FROM t ORDER BY w) z` answered
/// `column "w" does not exist` while the same SELECT run on its own
/// worked. Ordinals and real columns were fine either way — only the
/// alias needed the pass.
///
/// The test is deliberately narrow so the nested path pays nothing for
/// the ordinary query: a statement that has already been resolved no
/// longer has a bare alias in its ORDER BY, so it answers false.
#[must_use]
pub(crate) fn order_by_names_an_alias(s: &SelectStatement) -> bool {
    s.order_by.iter().any(|o| match &o.expr {
        Expr::Column(c) if c.qualifier.is_none() => s.items.iter().any(|it| {
            matches!(
                it,
                SelectItem::Expr { expr, alias: Some(a) }
                    // A SET-returning item is named, not substituted —
                    // round 80's rule, and re-substituting the expression
                    // here would undo it and make the sort a no-op again.
                    if a.eq_ignore_ascii_case(&c.name)
                        && !crate::select::expr_contains_builtin_srf(expr)
            )
        }),
        _ => false,
    })
}

pub(crate) fn resolve_order_by_position(s: &mut SelectStatement) {
    // v6.4.0 — iterate every ORDER BY key. Position references
    // (`ORDER BY 2`) bind to the 1-based projection index;
    // identifier references that match a SELECT-list alias bind to
    // the projected expression (Step 4 of L3a).
    // v7.37.17 (17.6 siblings) — with UNION peers the combined
    // result is already projected, so substituting the HEAD's item
    // expression would sort every combined row by the head's value
    // (a constant SELECT head made `… UNION ALL … ORDER BY x` a
    // silent no-sort). Resolve to the projected column NAME instead;
    // the union ORDER BY path evaluates names against the output
    // schema.
    let has_unions = !s.unions.is_empty();
    for order in &mut s.order_by {
        match &order.expr {
            Expr::Literal(Literal::Integer(n)) if *n >= 1 => {
                if let Ok(idx_one_based) = usize::try_from(*n) {
                    let idx = idx_one_based - 1;
                    if idx < s.items.len()
                        && let SelectItem::Expr { expr, alias } = &s.items[idx]
                    {
                        // v7.39 (read01 round 80) — a SET-returning item cannot be
                        // copied into ORDER BY. Substituting the expression makes
                        // the sort key "the whole set", evaluated once against the
                        // INPUT row — the same value for every row the set expands
                        // to, so the sort silently became a no-op:
                        // `SELECT unnest(ARRAY['B','a','A','b']) ORDER BY 1` came
                        // back in input order. The positional key means the Nth
                        // OUTPUT column, which after expansion is a real column;
                        // name it instead. (Same reasoning the UNION branch below
                        // already used, for the same reason.)
                        if crate::select::expr_contains_builtin_srf(expr) {
                            if let Some(name) = alias.clone() {
                                order.expr = Expr::Column(ColumnName {
                                    qualifier: None,
                                    name,
                                });
                            }
                            continue;
                        }
                        order.expr = match (has_unions, alias) {
                            (true, Some(a)) => Expr::Column(ColumnName {
                                qualifier: None,
                                name: a.clone(),
                            }),
                            // Unions + no alias: leave the positional
                            // literal for the union sort path to map
                            // onto the Nth projected column —
                            // substituting the HEAD's expr would sort
                            // every combined row by the head's value.
                            (true, None) => continue,
                            _ => expr.clone(),
                        };
                    }
                }
            }
            Expr::Column(c) if c.qualifier.is_none() => {
                // Alias-in-ORDER-BY lookup. Under unions the alias
                // already names the projected column — leave it.
                if has_unions {
                    continue;
                }
                // Local copy: the arms below assign `order.expr`,
                // which invalidates the `c` borrow.
                let target = c.name.clone();
                let mut bound = false;
                for item in &s.items {
                    if let SelectItem::Expr {
                        expr,
                        alias: Some(a),
                    } = item
                        && a == &target
                    {
                        order.expr = expr.clone();
                        bound = true;
                        break;
                    }
                }
                // v7.39 (round 185) — PG binds `ORDER BY <name>` to the
                // OUTPUT column even when its label is implicit: a cast
                // keeps the inner column's name (`SELECT x::text …
                // ORDER BY x` sorts the TEXT), a function call is named
                // after the function. Pre-r185 only explicit aliases
                // matched, so the sort silently used the SOURCE column
                // (int order instead of text order — live-PG18
                // differential 2026-07-18). A bare column projection is
                // skipped (substitution would be identity), and SRF
                // expressions keep the name path (round-80 reasoning:
                // copying a set into the key breaks the sort).
                if !bound {
                    for item in &s.items {
                        if let SelectItem::Expr { expr, alias: None } = item
                            && !matches!(expr, Expr::Column(_))
                            && implicit_output_label(expr) == Some(target.as_str())
                            && !crate::select::expr_contains_builtin_srf(expr)
                        {
                            order.expr = expr.clone();
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    for (_, peer) in &mut s.unions {
        resolve_order_by_position(peer);
    }
}

/// Sort `tagged` by `f64` key, reversing the comparator under DESC.
/// Used by the UNION ORDER BY path; per-block paths inline the same
/// comparator because they already hold `&OrderBy` directly.
/// v3.1.1: partial-sort helper. When `keep` (= offset + limit) is
/// strictly less than `tagged.len()`, run `select_nth_unstable_by` to
/// partition the prefix in O(n), then sort just that prefix in O(k
/// log k). Total O(n + k log k), vs O(n log n) for a full sort. The
/// caller decides what `keep` is; passing `None` (no LIMIT) keeps the
/// full-sort behaviour.
///
/// `tagged` holds `(Option<f64>, Row)` (the SELECT path) — `None` keys
/// sort last in ascending order, mirroring NULL-sorts-last in SQL.
/// v7.37.16 — one ORDER BY sort-key component. Numeric-shaped values
/// (int / float / date / numeric / time / money / …) keep the
/// lossless-enough `f64` fast path; TEXT carries the FULL string so
/// values sharing a long common prefix (`product_001` vs
/// `product_002`, ISO timestamps stored as text, prefixed IDs / SKUs)
/// order by their exact bytes rather than the old ~6-byte f64 coarse
/// key (which packed only the first 8 bytes into an f64 mantissa and
/// tied past ~6 bytes). Text comparison is byte-lexicographic, which
/// matches PG's default C / binary collation (SPG's collations are
/// byte-order). NULLs continue to ride the `Num(±INFINITY)` sentinel
/// the builders emit, so NULLS FIRST/LAST placement is unchanged.
#[derive(Clone, PartialEq)]
pub(crate) enum OrderKey {
    /// v7.37.16 — NULL sentinels. Historically NULL packed as `Num(±INF)`,
    /// which (a) made a real float `+Inf` VALUE indistinguishable from NULL
    /// and (b) left float NaN nowhere to go — PG orders
    /// `finite < +Inf < NaN < NULL` (ASC, NULLS LAST). `NullBig` sorts above
    /// everything including NaN; `NullSmall` below everything (the explicit
    /// NULLS FIRST/LAST flip picks the side, exactly as ±INF did).
    NullSmall,
    NullBig,
    Num(f64),
    /// v7.38 (read01 U31) — EXACT integer key for the integer-valued types
    /// (SmallInt/Int/BigInt/Date/Year/Timestamp/Time/TimeTz/Money). An f64
    /// projection silently collapses adjacent BigInt/Timestamp/Time/Money
    /// values past 2^53 (`9007199254740993` == `9007199254740992` as f64),
    /// giving a wrong ORDER BY. i128 holds every such value exactly.
    Int(i128),
    Text(alloc::string::String),
    /// v7.37 — byte-orderable key for types PG sorts byte-wise but that
    /// have no meaningful f64 projection: bytea, uuid, macaddr(8), inet/cidr
    /// (encoded `[family, addr.., bits]`). Sorts after finite Num / Text in
    /// the rare heterogeneous case; same-type is the load-bearing path.
    Bytes(alloc::vec::Vec<u8>),
    /// v7.38 (read01 P6.24) — jsonb sorted by PG's type-aware total order
    /// (`crate::json::jsonb_compare`), not its text spelling.
    Json(crate::json::JsonValue),
    /// v7.38 (read01, U16) — array key sorted element-wise, then shorter-first,
    /// like PG (`{1} < {1,2} < {2} < {10}`). Elements carry their own OrderKey so
    /// integer arrays sort numerically, not by text.
    Array(alloc::vec::Vec<OrderKey>),
    /// v7.38 (read01, T3.C3) — a NUMERIC beyond i128, sorted by exact bignum
    /// value. A big value's magnitude always exceeds i128, so vs a finite Num/Int
    /// key its sign alone orders it.
    BigNum(spg_storage::bignum::BigNumeric),
}

/// Compare two sort-key components (before any per-key DESC reverse).
/// Same-type pairs use the exact comparator; the only cross-type pairs
/// in practice are a NULL sentinel (`Num(±INF)`) meeting a `Text` value
/// (a NULL in a text column) — `+INF` sorts last, `-INF` first. A
/// finite `Num` vs `Text` (heterogeneous ORDER BY expression) is given
/// a deterministic total order (Num before Text) so the sort stays
/// total + stable.
/// v7.39 (round 683) — the same comparison under a column's collation.
///
/// Only a Text/Text pair can move: PG18's `en_US.utf8` is a deterministic
/// collation, so equality is unchanged there too (`'a' = 'A'` is false) and
/// only ORDER differs. A collation this build cannot perform keeps byte
/// order, which is what round 679's CREATE TABLE warning tells the user.
fn order_key_elem_cmp_in(
    a: &OrderKey,
    b: &OrderKey,
    collation: Option<&str>,
) -> core::cmp::Ordering {
    if let (OrderKey::Text(x), OrderKey::Text(y), Some(c)) = (a, b, collation)
        && let Some(ord) = crate::collate::compare(c, x, y)
    {
        return ord;
    }
    order_key_elem_cmp(a, b)
}

fn order_key_elem_cmp(a: &OrderKey, b: &OrderKey) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    match (a, b) {
        // v7.37.16 — NULL sentinels bracket every value (incl. NaN).
        (OrderKey::NullBig, OrderKey::NullBig) | (OrderKey::NullSmall, OrderKey::NullSmall) => {
            Ordering::Equal
        }
        (OrderKey::NullBig, _) => Ordering::Greater,
        (_, OrderKey::NullBig) => Ordering::Less,
        (OrderKey::NullSmall, _) => Ordering::Less,
        (_, OrderKey::NullSmall) => Ordering::Greater,
        // v7.37.16 — float keys use PG's float8 total order: NaN is the
        // greatest value and equals itself (partial_cmp is total once
        // both sides are non-NaN; -0 = 0 preserved).
        (OrderKey::Num(x), OrderKey::Num(y)) => match (x.is_nan(), y.is_nan()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        },
        // Exact same-type integer comparison — the load-bearing path that
        // U31 fixes (no f64 rounding).
        (OrderKey::Int(x), OrderKey::Int(y)) => x.cmp(y),
        // v7.38 (read01, U16) — array key: element-wise, then shorter-first.
        (OrderKey::Array(x), OrderKey::Array(y)) => {
            for (ex, ey) in x.iter().zip(y.iter()) {
                let c = order_key_elem_cmp(ex, ey);
                if c != Ordering::Equal {
                    return c;
                }
            }
            x.len().cmp(&y.len())
        }
        (OrderKey::Text(x), OrderKey::Text(y)) => x.cmp(y),
        (OrderKey::Bytes(x), OrderKey::Bytes(y)) => x.cmp(y),
        // v7.38 (read01 P6.24) — jsonb total order. Same-type is the
        // load-bearing path; a Json key only meets a foreign key via a NULL
        // sentinel (`Num(±INF)`) or a degenerate heterogeneous ORDER BY, where
        // Json is placed after every finite scalar key deterministically.
        (OrderKey::Json(x), OrderKey::Json(y)) => crate::json::jsonb_compare(x, y),
        // (NULL sentinels are handled above, so a Num here is a real float
        // value; the degenerate heterogeneous order puts Num before Json.)
        (OrderKey::Json(_), OrderKey::Num(_)) => Ordering::Greater,
        (OrderKey::Num(_), OrderKey::Json(_)) => Ordering::Less,
        (OrderKey::Json(_), OrderKey::Int(_) | OrderKey::Text(_) | OrderKey::Bytes(_)) => {
            Ordering::Greater
        }
        (OrderKey::Int(_) | OrderKey::Text(_) | OrderKey::Bytes(_), OrderKey::Json(_)) => {
            Ordering::Less
        }
        // Int vs Num: a mixed integer/float ORDER BY expression — compared
        // via f64 widening (lossy only in that rare cross-type case, never
        // same-type). NaN is greatest, per the float8 total order.
        #[allow(clippy::cast_precision_loss)]
        (OrderKey::Int(x), OrderKey::Num(y)) => {
            if y.is_nan() {
                Ordering::Less
            } else {
                (*x as f64).partial_cmp(y).unwrap_or(Ordering::Equal)
            }
        }
        #[allow(clippy::cast_precision_loss)]
        (OrderKey::Num(x), OrderKey::Int(y)) => {
            if x.is_nan() {
                Ordering::Greater
            } else {
                x.partial_cmp(&(*y as f64)).unwrap_or(Ordering::Equal)
            }
        }
        // A real Num value sorts before Text/Bytes in the degenerate
        // heterogeneous case (NULLs are sentinel-handled above).
        (OrderKey::Num(_), OrderKey::Text(_) | OrderKey::Bytes(_)) => Ordering::Less,
        (OrderKey::Text(_) | OrderKey::Bytes(_), OrderKey::Num(_)) => Ordering::Greater,
        // A finite Int sorts before Text/Bytes, same as a finite Num.
        (OrderKey::Int(_), OrderKey::Text(_) | OrderKey::Bytes(_)) => Ordering::Less,
        (OrderKey::Text(_) | OrderKey::Bytes(_), OrderKey::Int(_)) => Ordering::Greater,
        // Text sorts before Bytes in the rare heterogeneous case.
        (OrderKey::Text(_), OrderKey::Bytes(_)) => Ordering::Less,
        (OrderKey::Bytes(_), OrderKey::Text(_)) => Ordering::Greater,
        // v7.38 (read01, U16) — an Array key meets a foreign key only via the
        // ±INF NULL sentinel (NULLs ride to the ends) or a degenerate
        // heterogeneous ORDER BY (Array placed after every scalar key).
        // (NULLs sentinel-handled above; a real value keeps Array last
        // among the scalar keys in the degenerate heterogeneous case.)
        (OrderKey::Array(_), OrderKey::Num(_)) => Ordering::Greater,
        (OrderKey::Num(_), OrderKey::Array(_)) => Ordering::Less,
        (OrderKey::Array(_), _) => Ordering::Greater,
        (_, OrderKey::Array(_)) => Ordering::Less,
        // v7.38 (read01, T3.C3) — bignum keys compare exactly; vs a finite
        // scalar key a big value's sign orders it (its magnitude always
        // exceeds i128). A real float ±Inf/NaN still outranks any bignum.
        (OrderKey::BigNum(x), OrderKey::BigNum(y)) => x.cmp(y),
        (OrderKey::BigNum(x), OrderKey::Num(y)) => {
            if y.is_nan() || *y == f64::INFINITY {
                Ordering::Less
            } else if *y == f64::NEG_INFINITY {
                Ordering::Greater
            } else if x.parts().0 {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (OrderKey::Num(x), OrderKey::BigNum(y)) => {
            if x.is_nan() || *x == f64::INFINITY {
                Ordering::Greater
            } else if *x == f64::NEG_INFINITY {
                Ordering::Less
            } else if y.parts().0 {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (OrderKey::BigNum(x), _) => {
            if x.parts().0 {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (_, OrderKey::BigNum(y)) => {
            if y.parts().0 {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
    }
}

pub(crate) fn partial_sort_tagged(
    tagged: &mut Vec<(Vec<OrderKey>, Row)>,
    keep: Option<usize>,
    descs: &[bool],
) {
    partial_sort_tagged_in(tagged, keep, descs, &[]);
}

/// v7.39 (round 683) — `partial_sort_tagged` honouring one collation per key
/// position. This is the full-sort half of the single-table scan; the top-N
/// half compares through `cmp_multi_key_in` directly. Both had to land in
/// the same round: a collation honoured by one and not the other would make
/// ORDER BY depend on whether a LIMIT was small enough to take the top-N
/// path.
pub(crate) fn partial_sort_tagged_in(
    tagged: &mut Vec<(Vec<OrderKey>, Row)>,
    keep: Option<usize>,
    descs: &[bool],
    collations: &[Option<alloc::string::String>],
) {
    let cmp = |a: &(Vec<OrderKey>, Row), b: &(Vec<OrderKey>, Row)| {
        cmp_multi_key_in(&a.0, &b.0, descs, collations)
    };
    match keep {
        Some(k) if k < tagged.len() && k > 0 => {
            let pivot = k - 1;
            tagged.select_nth_unstable_by(pivot, cmp);
            tagged[..k].sort_by(cmp);
            tagged.truncate(k);
        }
        _ => {
            tagged.sort_by(cmp);
        }
    }
}

pub(crate) fn sort_by_keys(tagged: &mut [(Vec<OrderKey>, Row)], descs: &[bool]) {
    sort_by_keys_in(tagged, descs, &[]);
}

/// v7.39 (round 683) — `sort_by_keys` honouring one collation per position.
pub(crate) fn sort_by_keys_in(
    tagged: &mut [(Vec<OrderKey>, Row)],
    descs: &[bool],
    collations: &[Option<alloc::string::String>],
) {
    tagged.sort_by(|a, b| cmp_multi_key_in(&a.0, &b.0, descs, collations));
}

/// v7.39 (round 683) — the collation for each ORDER BY position.
///
/// Only a bare column reference has one: `ORDER BY upper(name)` sorts a new
/// value, and SPG does not track collation through expressions. Resolved
/// once per sort, beside `descs`, never per row.
pub(crate) fn order_by_collations(
    order_by: &[spg_sql::ast::OrderBy],
    ctx: &EvalContext,
) -> alloc::vec::Vec<Option<alloc::string::String>> {
    order_by
        .iter()
        .map(|o| {
            let Expr::Column(c) = &o.expr else {
                return None;
            };
            let pos = eval::find_column_pos(c, ctx)?;
            let name = ctx.columns.get(pos)?.collation_name.clone()?;
            crate::collate::is_supported(&name).then_some(name)
        })
        .collect()
}

/// Streaming top-N trim for `ORDER BY … LIMIT k`. Called inside the row-
/// production loop after each push: once the accumulator reaches `2·keep`
/// rows, `select_nth_unstable_by` partitions the `keep` smallest into the
/// prefix in O(len) and the tail is truncated away. Over the whole scan
/// this bounds live memory to O(keep) instead of materialising all N
/// projected rows (the OOM shape from large `… ORDER BY col LIMIT 10`
/// scans) while staying O(N) total time — the same top-N-heap complexity
/// PG's tuplesort uses, without a per-row heap sift.
///
/// The retained set after every trim is exactly the running top-`keep`,
/// so a final [`partial_sort_tagged`] over the ≤ `2·keep` survivors
/// yields the identical rows a full sort would. Rows sharing an ORDER BY
/// key may be retained in a different order than a full sort would pick —
/// but `select_nth_unstable_by` is already unstable, so no tie order was
/// ever guaranteed. A no-op when `keep == 0` (LIMIT 0 is handled by the
/// caller's truncation).
pub(crate) fn topk_trim(tagged: &mut Vec<(Vec<OrderKey>, Row)>, keep: usize, descs: &[bool]) {
    topk_trim_recycling(tagged, keep, descs, &mut Vec::new(), &mut Vec::new(), &mut None);
}

/// v7.39 (round 571) — the same trim, handing the dropped rows' value
/// buffers back to the caller instead of freeing them.
///
/// A streaming top-N scan allocates a projection `Vec` per input row
/// and keeps `keep` of them: on 500k rows with `LIMIT 10`, 499,990 are
/// built and dropped almost at once, and a profile of that query put a
/// quarter of the connection thread in the allocator. The accumulator
/// is bounded at `2 * keep`, so every trim frees `keep` buffers with
/// their capacity already grown — exactly what the next rows need.
pub(crate) fn topk_trim_recycling<'a>(
    tagged: &mut Vec<(Vec<OrderKey>, Row<'a>)>,
    keep: usize,
    descs: &[bool],
    pool: &mut Vec<Vec<Value<'a>>>,
    key_pool: &mut Vec<Vec<OrderKey>>,
    boundary: &mut Option<Vec<OrderKey>>,
) {
    // v7.39 (round 580) — trim on a floor, not on `2k` alone.
    //
    // `2 * keep` means a `LIMIT 10` over 500k rows trims fifty thousand
    // times: each one a `select_nth_unstable_by` over twenty elements
    // plus a drain and a return to the pool. A profile of that query put
    // 8.6% of it in the comparator alone, more than any other symbol.
    // Selecting the same k out of a larger batch costs the same O(n)
    // per element but pays the call and drain overhead a hundredth as
    // often; the accumulator stays bounded, just by a bigger constant.
    const TRIM_FLOOR: usize = 1024;
    let trigger = keep.saturating_mul(2).max(TRIM_FLOOR);
    if keep > 0 && tagged.len() >= trigger {
        let cmp = |a: &(Vec<OrderKey>, Row<'a>), b: &(Vec<OrderKey>, Row<'a>)| {
            cmp_multi_key(&a.0, &b.0, descs)
        };
        tagged.select_nth_unstable_by(keep - 1, cmp);
        for (mut keys, row) in tagged.drain(keep..) {
            let mut v = row.values;
            v.clear();
            pool.push(v);
            keys.clear();
            key_pool.push(keys);
        }
        // v7.39 (round 581) — the worst row still kept is a floor on the
        // answer: the final k-th best can only be better than it, so a
        // row that loses to it can never enter the top k and does not
        // need to be built at all. `select_nth_unstable_by` has just put
        // that row at `keep - 1`.
        if let Some((keys, _)) = tagged.get(keep - 1) {
            match boundary {
                Some(b) => {
                    b.clear();
                    b.extend_from_slice(keys);
                }
                None => *boundary = Some(keys.clone()),
            }
        }
    }
}

/// v6.4.0 — multi-key ORDER BY comparator. Each key's per-key DESC
/// flag is honored independently. NULL is encoded as `f64::INFINITY`
/// so it sorts last in ASC and first in DESC (matches PG default).
/// v7.37.16 — keys are `OrderKey` (numeric fast path OR full text);
/// see `order_key_elem_cmp`.
pub(crate) fn cmp_multi_key(a: &[OrderKey], b: &[OrderKey], descs: &[bool]) -> core::cmp::Ordering {
    cmp_multi_key_in(a, b, descs, &[])
}

/// v7.39 (round 683) — `cmp_multi_key` with one collation per key position.
///
/// Per POSITION because that is how the other per-key metadata already
/// travels: every sort site in the engine carries `descs: &[bool]`, one
/// flag per ORDER BY item, derived from the same `order_by`. A collation is
/// the same shape and rides beside it — including through the `Ord` impl in
/// `join.rs`, which stores its `descs` in the struct and can store these
/// the same way. Round 682 looked for a seam and typed at three wrong ones;
/// the seam was the metadata that was already being carried.
pub(crate) fn cmp_multi_key_in(
    a: &[OrderKey],
    b: &[OrderKey],
    descs: &[bool],
    collations: &[Option<alloc::string::String>],
) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    for (i, (ka, kb)) in a.iter().zip(b.iter()).enumerate() {
        let ord = order_key_elem_cmp_in(ka, kb, collations.get(i).and_then(|c| c.as_deref()));
        let ord = if descs.get(i).copied().unwrap_or(false) {
            ord.reverse()
        } else {
            ord
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// v6.4.0 — eval every ORDER BY expression for a row and pack the
/// resulting keys into a `Vec<f64>`. NULL → `f64::INFINITY`.
/// When the ORDER BY item is a bare reference to an enum column and the
/// value is one of the type's labels, return its 0-based ordinal in the
/// enum's declaration order — the key PG sorts by (enumsortorder). Else
/// `None`, so the caller falls back to the normal value key.
pub(crate) fn enum_order_ordinal(expr: &Expr, v: &Value, ctx: &EvalContext) -> Option<f64> {
    let Expr::Column(c) = expr else { return None };
    let Value::Text(label) = v else { return None };
    let pos = eval::find_column_pos(c, ctx)?;
    let col = ctx.columns.get(pos)?;
    // v7.39 (round 401) — an inline MySQL `ENUM('a','b',…)` column sorts by
    // its variant declaration order too (`ORDER BY e` is low, mid, high, not
    // the alphabetical high, low, mid), like a PG CREATE TYPE enum.
    let ord = if let Some(enum_name) = col.user_enum_type.as_deref() {
        ctx.catalog?
            .enum_types()
            .get(enum_name)?
            .labels
            .iter()
            .position(|l| l.as_str() == label.as_ref())?
    } else {
        col.inline_enum_variants
            .as_deref()?
            .iter()
            .position(|l| l.as_str() == label.as_ref())?
    };
    #[allow(clippy::cast_precision_loss)]
    Some(ord as f64)
}

pub(crate) fn build_order_keys(
    order_by: &[OrderBy],
    row: &Row<'static>,
    ctx: &EvalContext,
) -> Result<Vec<OrderKey>, EngineError> {
    let mut keys = Vec::with_capacity(order_by.len());
    build_order_keys_into(order_by, row, ctx, &mut keys)?;
    Ok(keys)
}

/// v7.39 (round 571) — the same keys, into a buffer the caller owns.
///
/// The streaming top-N scan builds one of these per input row and keeps
/// `keep` of them, so on 500k rows with `LIMIT 10` it allocated half a
/// million vectors of one element each. The trim hands the dropped ones
/// back and they come through here.
/// v7.39 (round 582) — which ORDER BY keys are a bare column, and where
/// that column sits.
///
/// `build_order_keys_into` evaluated every key INTERPRETIVELY for every
/// row, and a profile of `ORDER BY g DESC, id DESC LIMIT 10` over 500k
/// rows put 8% of the query in `resolve_column` alone — resolving the
/// same two names, by string, half a million times each. The position
/// does not change between rows.
///
/// `None` for a key that is not a bare column of this relation; those
/// keep the interpretive path.
pub(crate) fn order_by_bound_positions(
    order_by: &[OrderBy],
    schema_cols: &[ColumnSchema],
    alias: Option<&str>,
) -> Vec<Option<usize>> {
    order_by
        .iter()
        .map(|o| bound_column_position(&o.expr, schema_cols, alias))
        .collect()
}

/// v7.39 (round 593) — the cell an expression reads, when it reads exactly
/// one and the position is the same for every row. `None` keeps the caller on
/// the resolver, which is what anything else needs.
///
/// Round 582 established this for the top-level ORDER BY; the window pipeline
/// resolves its partition and order keys by NAME once per row, which a
/// per-library profile puts at 5.8% of a `lag()` query in `resolve_column`
/// alone, with `rehydrate_cell` and the `eval_expr` dispatch behind it.
pub(crate) fn bound_column_position(
    expr: &Expr,
    schema_cols: &[ColumnSchema],
    alias: Option<&str>,
) -> Option<usize> {
    let Expr::Column(c) = expr else {
        return None;
    };
    if let Some(q) = c.qualifier.as_deref()
        && !alias.is_some_and(|a| q.eq_ignore_ascii_case(a))
    {
        return None;
    }
    // An ambiguous bare name must keep the resolver's own rules.
    let mut hit = None;
    for (i, s) in schema_cols.iter().enumerate() {
        if s.name.eq_ignore_ascii_case(&c.name) {
            if hit.is_some() {
                return None;
            }
            hit = Some(i);
        }
    }
    // A composite column is stored as JSON and rebuilt by `rehydrate_cell`
    // on the way out, so reading its cell raw would compare the wrong thing.
    let i = hit?;
    if schema_cols[i].user_composite_type.is_some() {
        return None;
    }
    Some(i)
}

pub(crate) fn build_order_keys_into(
    order_by: &[OrderBy],
    row: &Row<'static>,
    ctx: &EvalContext,
    keys: &mut Vec<OrderKey>,
) -> Result<(), EngineError> {
    build_order_keys_bound(order_by, &[], row, ctx, keys)
}

/// The same, reading a bound key's cell instead of evaluating it.
/// `bound` may be shorter than `order_by`; a missing or `None` entry
/// falls back to the interpretive path, so the keys are the ones
/// `build_order_keys_into` would have produced.
pub(crate) fn build_order_keys_bound(
    order_by: &[OrderBy],
    bound: &[Option<usize>],
    row: &Row<'static>,
    ctx: &EvalContext,
    keys: &mut Vec<OrderKey>,
) -> Result<(), EngineError> {
    keys.clear();
    keys.reserve(order_by.len());
    for (i, o) in order_by.iter().enumerate() {
        let borrowed: Option<&Value<'static>> =
            bound.get(i).copied().flatten().and_then(|p| row.values.get(p));
        let owned: Value<'static>;
        // Borrowed when the key is a bound column, owned when it had to
        // be evaluated — either way the body below only reads it, so
        // neither path pays a clone.
        let v: &Value<'static> = match borrowed {
            Some(v) => v,
            None => {
                owned = eval::eval_expr(&o.expr, row, ctx)?;
                &owned
            }
        };
        // v7.24 (round-16 A) — explicit NULLS FIRST/LAST. The f64
        // packing sorts ascending THEN applies the per-key DESC
        // reverse, so a NULL must land at +INF exactly when the
        // effective placement agrees with the reverse direction:
        // nf == desc → +INF (ASC default last / DESC default
        // first), nf != desc → -INF (the explicit flips).
        if matches!(v, Value::Null) {
            // v7.39 (round 403) — MySQL treats NULL as the SMALLEST value
            // (NULLS FIRST for ASC / LAST for DESC); PG's default is the
            // reverse. An explicit NULLS FIRST/LAST still wins.
            let nf = o
                .nulls_first
                .unwrap_or(if ctx.mysql_dialect { !o.desc } else { o.desc });
            keys.push(if nf == o.desc {
                OrderKey::NullBig
            } else {
                OrderKey::NullSmall
            });
        } else if let Some(ord) = enum_order_ordinal(&o.expr, v, ctx) {
            // Enum columns sort by declaration order (enumsortorder), not
            // the label text: `ORDER BY mood` puts 'sad' < 'ok' < 'happy'
            // when that is the CREATE TYPE order, not alphabetical.
            keys.push(OrderKey::Num(ord));
        } else if ctx.mysql_dialect && matches!(v, Value::Text(_) | Value::BpChar(_)) {
            // v7.39 (round 411) — under the MySQL collation (case- and
            // accent-insensitive, PAD SPACE) ORDER BY sorts by the FOLDED
            // text, so `apple`/`Apple` sort adjacent and `Zebra` after
            // `Mango`. The precomputed key must fold to match
            // `order_by_value_cmp_in`; a byte key would sort by ASCII case.
            let s = match v {
                Value::Text(s) | Value::BpChar(s) => s.as_ref(),
                _ => unreachable!("guarded by matches! above"),
            };
            keys.push(OrderKey::Text(spg_storage::mysql_compare_fold(s)));
        } else {
            keys.push(value_to_order_key(v)?);
        }
    }
    Ok(())
}

/// Drop the first `offset` rows then truncate to `limit`. PG / `MySQL`
/// agree: OFFSET applies *after* ORDER BY but *before* LIMIT (so
/// `LIMIT 10 OFFSET 5` keeps rows 6..=15).
pub(crate) fn apply_offset_and_limit(
    rows: &mut Vec<Row<'static>>,
    offset: Option<u32>,
    limit: Option<u32>,
) {
    if let Some(off) = offset {
        let off = off as usize;
        if off >= rows.len() {
            rows.clear();
        } else {
            rows.drain(..off);
        }
    }
    if let Some(n) = limit {
        rows.truncate(n as usize);
    }
}

/// v7.17.0 Phase 3.P0-49 — offset + limit applied to a tagged
/// `(order_keys, row)` sequence, with optional SQL:2008 `WITH
/// TIES` extension. When `with_ties` is set, the truncated tail
/// is extended through every subsequent row whose order keys
/// equal the last-kept row's keys (so a "top 3 by score" with
/// WITH TIES emits row 4 too when row 4 ties row 3 on `score`).
///
/// The order-key vector is the per-row sort key the caller already
/// computed via `build_order_keys`; equal-key detection therefore
/// matches the sort comparator exactly.
pub(crate) fn apply_offset_and_limit_tagged(
    tagged: &mut Vec<(Vec<OrderKey>, Row)>,
    offset: Option<u32>,
    limit: Option<u32>,
    with_ties: bool,
) {
    if let Some(off) = offset {
        let off = off as usize;
        if off >= tagged.len() {
            tagged.clear();
        } else {
            tagged.drain(..off);
        }
    }
    if let Some(n) = limit {
        let n = n as usize;
        if with_ties && n > 0 && n < tagged.len() {
            let cutoff_key = tagged[n - 1].0.clone();
            let mut end = n;
            while end < tagged.len() && tagged[end].0 == cutoff_key {
                end += 1;
            }
            tagged.truncate(end);
        } else {
            tagged.truncate(n);
        }
    }
}

/// v7.39 (round 232) — the ORDER BY legality rules PG enforces before it
/// sorts anything. SPG accepted all three of these and silently answered
/// something; PG rejects each by name (42P10 INVALID_COLUMN_REFERENCE),
/// which is what a client branches on.
///
/// * a positional key must name an existing output column;
/// * under SELECT DISTINCT the sort keys must be output columns, because
///   the de-duplication happens before the sort and a key that isn't in
///   the result has no defined value to sort by;
/// * under DISTINCT ON the leading sort keys must come from the
///   DISTINCT ON list, since which row survives per group is decided by
///   that ordering.
///
/// Wording and rules measured off live PG18.4 (r232 probe).
pub(crate) fn check_order_by_legality(
    stmt: &spg_sql::ast::SelectStatement,
) -> Result<(), crate::EngineError> {
    use spg_sql::ast::{Expr, Literal, SelectItem};
    let err = |m: alloc::string::String| Err(crate::EngineError::Unsupported(m));
    if stmt.order_by.is_empty() {
        return Ok(());
    }
    // A wildcard makes the output width unknown here (it expands against
    // the scanned schema), so the positional bound is only checked when
    // every item is explicit.
    check_order_by_positions(stmt)?;
    // Does this sort key name one of the output columns?
    let is_output_column = |e: &Expr| -> bool {
        if matches!(e, Expr::Literal(Literal::Integer(_))) {
            return true; // positional; bounds already checked
        }
        stmt.items.iter().any(|item| match item {
            SelectItem::Expr { expr, alias } => {
                expr == e
                    || match (alias, e) {
                        (Some(a), Expr::Column(c)) => {
                            c.qualifier.is_none() && c.name.eq_ignore_ascii_case(a)
                        }
                        _ => false,
                    }
            }
            // A wildcard puts every scanned column in the output, so any
            // plain column reference is satisfied by it.
            _ => matches!(e, Expr::Column(_)),
        })
    };
    if !stmt.distinct_on.is_empty() {
        // Probed rule (PG18.4): scan the sort keys left to right; until
        // every DISTINCT ON expression has been matched, each key must be
        // one of them. Once they are all accounted for the remaining keys
        // are free — `DISTINCT ON (a) … ORDER BY a, b` is fine while
        // `ORDER BY b, a` is not, and running out of sort keys early
        // (`DISTINCT ON (a, b) … ORDER BY a`) is fine too.
        let mut matched = alloc::vec![false; stmt.distinct_on.len()];
        for ob in &stmt.order_by {
            if matched.iter().all(|m| *m) {
                break;
            }
            match stmt.distinct_on.iter().position(|d| d == &ob.expr) {
                Some(i) => matched[i] = true,
                None => {
                    return err(
                        "SELECT DISTINCT ON expressions must match initial ORDER BY expressions"
                            .into(),
                    );
                }
            }
        }
        return Ok(());
    }
    if stmt.distinct {
        for ob in &stmt.order_by {
            if !is_output_column(&ob.expr) {
                return err(
                    "for SELECT DISTINCT, ORDER BY expressions must appear in select list".into(),
                );
            }
        }
    }
    Ok(())
}

/// v7.39 (round 232) — the positional half of [`check_order_by_legality`],
/// split out because a set-operation wrapper carries its own ORDER BY over
/// the head's output columns but not the head's DISTINCT, so only this part
/// applies there.
pub(crate) fn check_order_by_positions(
    stmt: &spg_sql::ast::SelectStatement,
) -> Result<(), crate::EngineError> {
    use spg_sql::ast::{Expr, Literal, SelectItem};
    // A wildcard makes the output width unknown here (it expands against
    // the scanned schema), so the bound is only checked when every item is
    // explicit.
    let Some(width) = stmt
        .items
        .iter()
        .all(|i| matches!(i, SelectItem::Expr { .. }))
        .then(|| stmt.items.len())
    else {
        return Ok(());
    };
    for ob in &stmt.order_by {
        if let Expr::Literal(Literal::Integer(n)) = &ob.expr
            && (*n < 1 || *n as usize > width as i64 as usize)
        {
            return Err(crate::EngineError::Unsupported(alloc::format!(
                "ORDER BY position {n} is not in select list"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod value_cmp_mixed_numeric_tests {
    //! v7.37.16 Slice A — direct coverage of the mixed NUMERIC↔int/float
    //! `value_cmp` arms. These pairs previously fell through the `_`
    //! debug-string arm (which sorted by the `Value` debug rendering,
    //! e.g. `Numeric { scaled: 1000, .. }` before `SmallInt(9)`), so an
    //! ORDER BY / min / max / window / mode over a key that mixes a
    //! NUMERIC with an integer or float was silently mis-ordered. The
    //! arms now promote int→NUMERIC (exact, mirroring binop.rs
    //! `numeric_or_widen`) and demote NUMERIC→f64 against a float (PG
    //! `numeric op float8`), matching arithmetic + WHERE comparison.
    use super::value_cmp;
    use core::cmp::Ordering;
    use spg_storage::Value;

    fn num(scaled: i128, scale: u16) -> Value<'static> {
        Value::Numeric {
            scaled,
            scale,
            kind: spg_storage::NumericKind::Finite,
        }
    }

    #[test]
    fn numeric_vs_integer_exact() {
        // 2.50 < 5 ; symmetric
        assert_eq!(value_cmp(&num(250, 2), &Value::Int(5)), Ordering::Less);
        assert_eq!(value_cmp(&Value::Int(5), &num(250, 2)), Ordering::Greater);
        // The debug-string-fallback bug: 1000 (numeric, scale 0) vs 9.
        // Lexical debug order put "Numeric{scaled:1000}" before
        // "SmallInt(9)" -> Less (WRONG). Value order is Greater.
        assert_eq!(
            value_cmp(&num(1000, 0), &Value::SmallInt(9)),
            Ordering::Greater
        );
        assert_eq!(
            value_cmp(&Value::SmallInt(9), &num(1000, 0)),
            Ordering::Less
        );
        // BigInt both directions.
        assert_eq!(
            value_cmp(&num(350, 2), &Value::BigInt(3)),
            Ordering::Greater
        );
        assert_eq!(
            value_cmp(&Value::BigInt(4), &num(350, 2)),
            Ordering::Greater
        );
    }

    #[test]
    fn numeric_equals_integer_when_value_equal() {
        // 2.0 (scaled 20, scale 1) == 2 (int) -> exact promotion, Equal.
        assert_eq!(value_cmp(&num(20, 1), &Value::Int(2)), Ordering::Equal);
        assert_eq!(value_cmp(&Value::Int(2), &num(20, 1)), Ordering::Equal);
        assert_eq!(value_cmp(&num(2, 0), &Value::SmallInt(2)), Ordering::Equal);
    }

    #[test]
    fn numeric_vs_float_demote() {
        // 3.5 numeric vs 3.5 float -> Equal ; 3.5 vs 3.0 -> Greater.
        assert_eq!(value_cmp(&num(35, 1), &Value::Float(3.5)), Ordering::Equal);
        assert_eq!(
            value_cmp(&num(35, 1), &Value::Float(3.0)),
            Ordering::Greater
        );
        assert_eq!(value_cmp(&Value::Float(1.0), &num(25, 1)), Ordering::Less);
        assert_eq!(
            value_cmp(&Value::Float(9.9), &num(25, 1)),
            Ordering::Greater
        );
    }
}
