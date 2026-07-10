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
use spg_storage::{Row, Value};

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
    use core::cmp::Ordering;
    let nf = nulls_first.unwrap_or(desc);
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
            let c = value_cmp(a, b);
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

pub(crate) fn value_cmp(a: &Value, b: &Value) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    // v7.38 (read01, T3.C3) — a NUMERIC beyond i128 orders via exact bignum.
    if let Some(ord) = numeric_bignum_cmp(a, b) {
        return ord;
    }
    // v7.38 (read01, T6.P3) — a NUMERIC special orders by the total order
    // -Inf < finite < +Inf < NaN (NaN == NaN), ahead of the finite arms which
    // would read a special's canonical 0 as the number 0.
    {
        use spg_storage::NumericKind as NK;
        let kind = |v: &Value| -> Option<NK> {
            match v {
                Value::Numeric { kind, .. } => Some(*kind),
                Value::Int(_) | Value::BigInt(_) | Value::SmallInt(_) => Some(NK::Finite),
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
pub(crate) fn cmp_numeric(xs: i128, xsc: u8, ys: i128, ysc: u8) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    let max_scale = xsc.max(ysc);
    let widen = |v: i128, sc: u8| -> Option<i128> {
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
pub(crate) fn numeric_to_f64(scaled: i128, scale: u8) -> f64 {
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
                for item in &s.items {
                    if let SelectItem::Expr {
                        expr,
                        alias: Some(a),
                    } = item
                        && a == &c.name
                    {
                        order.expr = expr.clone();
                        break;
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
fn order_key_elem_cmp(a: &OrderKey, b: &OrderKey) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    match (a, b) {
        (OrderKey::Num(x), OrderKey::Num(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
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
        (OrderKey::Json(_), OrderKey::Num(y)) => {
            if *y == f64::INFINITY {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (OrderKey::Num(x), OrderKey::Json(_)) => {
            if *x == f64::INFINITY {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (OrderKey::Json(_), OrderKey::Int(_) | OrderKey::Text(_) | OrderKey::Bytes(_)) => {
            Ordering::Greater
        }
        (OrderKey::Int(_) | OrderKey::Text(_) | OrderKey::Bytes(_), OrderKey::Json(_)) => {
            Ordering::Less
        }
        // Int vs Num: the only Num a value-typed Int meets is the ±INF NULL
        // sentinel (rides to the far ends) or, in a genuinely heterogeneous
        // ORDER BY expression, a finite float — compared via f64 widening
        // (lossy only in that rare cross-type case, never same-type).
        #[allow(clippy::cast_precision_loss)]
        (OrderKey::Int(x), OrderKey::Num(y)) => {
            if *y == f64::INFINITY {
                Ordering::Less
            } else if *y == f64::NEG_INFINITY {
                Ordering::Greater
            } else {
                (*x as f64).partial_cmp(y).unwrap_or(Ordering::Equal)
            }
        }
        #[allow(clippy::cast_precision_loss)]
        (OrderKey::Num(x), OrderKey::Int(y)) => {
            if *x == f64::INFINITY {
                Ordering::Greater
            } else if *x == f64::NEG_INFINITY {
                Ordering::Less
            } else {
                x.partial_cmp(&(*y as f64)).unwrap_or(Ordering::Equal)
            }
        }
        (OrderKey::Num(x), OrderKey::Text(_) | OrderKey::Bytes(_)) => {
            // Finite Num sorts before Text/Bytes; the ±INF NULL sentinels ride
            // to the far ends (`+INF` last, `-INF` first).
            if *x == f64::INFINITY {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (OrderKey::Text(_) | OrderKey::Bytes(_), OrderKey::Num(y)) => {
            if *y == f64::INFINITY {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        // A finite Int sorts before Text/Bytes, same as a finite Num.
        (OrderKey::Int(_), OrderKey::Text(_) | OrderKey::Bytes(_)) => Ordering::Less,
        (OrderKey::Text(_) | OrderKey::Bytes(_), OrderKey::Int(_)) => Ordering::Greater,
        // Text sorts before Bytes in the rare heterogeneous case.
        (OrderKey::Text(_), OrderKey::Bytes(_)) => Ordering::Less,
        (OrderKey::Bytes(_), OrderKey::Text(_)) => Ordering::Greater,
        // v7.38 (read01, U16) — an Array key meets a foreign key only via the
        // ±INF NULL sentinel (NULLs ride to the ends) or a degenerate
        // heterogeneous ORDER BY (Array placed after every scalar key).
        (OrderKey::Array(_), OrderKey::Num(y)) => {
            if *y == f64::NEG_INFINITY {
                Ordering::Greater
            } else if *y == f64::INFINITY {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (OrderKey::Num(x), OrderKey::Array(_)) => {
            if *x == f64::NEG_INFINITY {
                Ordering::Less
            } else if *x == f64::INFINITY {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (OrderKey::Array(_), _) => Ordering::Greater,
        (_, OrderKey::Array(_)) => Ordering::Less,
        // v7.38 (read01, T3.C3) — bignum keys compare exactly; vs a finite
        // scalar key a big value's sign orders it (its magnitude always exceeds
        // i128). The ±INF NULL sentinels still ride to the far ends.
        (OrderKey::BigNum(x), OrderKey::BigNum(y)) => x.cmp(y),
        (OrderKey::BigNum(x), OrderKey::Num(y)) => {
            if *y == f64::INFINITY {
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
            if *x == f64::INFINITY {
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
    let cmp = |a: &(Vec<OrderKey>, Row), b: &(Vec<OrderKey>, Row)| cmp_multi_key(&a.0, &b.0, descs);
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
    tagged.sort_by(|a, b| cmp_multi_key(&a.0, &b.0, descs));
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
    if keep > 0 && tagged.len() >= keep.saturating_mul(2) {
        let cmp =
            |a: &(Vec<OrderKey>, Row), b: &(Vec<OrderKey>, Row)| cmp_multi_key(&a.0, &b.0, descs);
        tagged.select_nth_unstable_by(keep - 1, cmp);
        tagged.truncate(keep);
    }
}

/// v6.4.0 — multi-key ORDER BY comparator. Each key's per-key DESC
/// flag is honored independently. NULL is encoded as `f64::INFINITY`
/// so it sorts last in ASC and first in DESC (matches PG default).
/// v7.37.16 — keys are `OrderKey` (numeric fast path OR full text);
/// see `order_key_elem_cmp`.
pub(crate) fn cmp_multi_key(a: &[OrderKey], b: &[OrderKey], descs: &[bool]) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    for (i, (ka, kb)) in a.iter().zip(b.iter()).enumerate() {
        let ord = order_key_elem_cmp(ka, kb);
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
fn enum_order_ordinal(expr: &Expr, v: &Value, ctx: &EvalContext) -> Option<f64> {
    let Expr::Column(c) = expr else { return None };
    let Value::Text(label) = v else { return None };
    let pos = eval::find_column_pos(c, ctx)?;
    let enum_name = ctx.columns.get(pos)?.user_enum_type.as_deref()?;
    let def = ctx.catalog?.enum_types().get(enum_name)?;
    let ord = def
        .labels
        .iter()
        .position(|l| l.as_str() == label.as_ref())?;
    #[allow(clippy::cast_precision_loss)]
    Some(ord as f64)
}

pub(crate) fn build_order_keys(
    order_by: &[OrderBy],
    row: &Row<'static>,
    ctx: &EvalContext,
) -> Result<Vec<OrderKey>, EngineError> {
    let mut keys = Vec::with_capacity(order_by.len());
    for o in order_by {
        let v = eval::eval_expr(&o.expr, row, ctx)?;
        // v7.24 (round-16 A) — explicit NULLS FIRST/LAST. The f64
        // packing sorts ascending THEN applies the per-key DESC
        // reverse, so a NULL must land at +INF exactly when the
        // effective placement agrees with the reverse direction:
        // nf == desc → +INF (ASC default last / DESC default
        // first), nf != desc → -INF (the explicit flips).
        if matches!(v, Value::Null) {
            let nf = o.nulls_first.unwrap_or(o.desc);
            keys.push(OrderKey::Num(if nf == o.desc {
                f64::INFINITY
            } else {
                f64::NEG_INFINITY
            }));
        } else if let Some(ord) = enum_order_ordinal(&o.expr, &v, ctx) {
            // Enum columns sort by declaration order (enumsortorder), not
            // the label text: `ORDER BY mood` puts 'sad' < 'ok' < 'happy'
            // when that is the CREATE TYPE order, not alphabetical.
            keys.push(OrderKey::Num(ord));
        } else {
            keys.push(value_to_order_key(&v)?);
        }
    }
    Ok(keys)
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

    fn num(scaled: i128, scale: u8) -> Value<'static> {
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
