//! Value-utility free functions split out of `eval.rs` (cut 36): pure
//! functions over `Value`(s) that support the evaluator and aggregates —
//! `value_cmp_for_min_max` (MIN/MAX ordering), `value_to_f64` (numeric
//! coercion), `values_equal_for_nullif` (NULLIF equality),
//! `gen_random_uuid_bytes` (UUID v4), and the central `value_to_text`
//! renderer. These reach the canonical formatters (re-exported from
//! `eval::format` / `eval::textsearch`), `crate::conversions`, the PRNG
//! (`eval::math`), and `civil_from_days` through `use super::*`.

use super::*;

/// Compare two values for min/max selection. Returns Equal when
/// values are equal (including cross-numeric-width), Less when
/// a < b, Greater when a > b. NULL handling is upstream.
pub(super) fn value_cmp_for_min_max(
    a: &Value,
    b: &Value,
    mysql: bool,
) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    // v7.39 (round 412) — GREATEST / LEAST over text under the MySQL default
    // collation compares by the folded form (case- and accent-insensitive,
    // PAD SPACE), matching ORDER BY / MIN / MAX.
    if mysql {
        if let (Value::Text(x), Value::Text(y)) | (Value::BpChar(x), Value::BpChar(y)) = (a, b) {
            return spg_storage::mysql_compare_fold(x)
                .cmp(&spg_storage::mysql_compare_fold(y));
        }
    }
    // v7.38 (read01, T3.C3) — a NUMERIC beyond i128 orders via exact bignum.
    if let Some(ord) = crate::orderby::numeric_bignum_cmp(a, b) {
        return ord;
    }
    // v7.38 (read01, T6.P3) — min()/max() over NUMERIC honor the special total
    // order -Inf < finite < +Inf < NaN, ahead of the f64 widen (which reads a
    // special's canonical 0 as the number 0).
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
    // Integer-widen first (covers SmallInt vs Int vs BigInt).
    let a_int = match a {
        Value::SmallInt(x) => Some(i64::from(*x)),
        Value::Int(x) => Some(i64::from(*x)),
        Value::BigInt(x) => Some(*x),
        _ => None,
    };
    let b_int = match b {
        Value::SmallInt(x) => Some(i64::from(*x)),
        Value::Int(x) => Some(i64::from(*x)),
        Value::BigInt(x) => Some(*x),
        _ => None,
    };
    if let (Some(av), Some(bv)) = (a_int, b_int) {
        return av.cmp(&bv);
    }
    // Float-widen.
    let a_f = value_to_f64(a);
    let b_f = value_to_f64(b);
    if let (Some(av), Some(bv)) = (a_f, b_f) {
        return av.partial_cmp(&bv).unwrap_or(Ordering::Equal);
    }
    // Text/Text and the remaining ordered types. The fallthrough
    // used to swallow dates/timestamps/intervals as Equal, making
    // greatest()/least() silently keep the first argument.
    match (a, b) {
        (Value::Text(av), Value::Text(bv)) => av.cmp(bv),
        (Value::Bytes(av), Value::Bytes(bv)) => av.cmp(bv),
        (Value::Date(av), Value::Date(bv)) => av.cmp(bv),
        (Value::Timestamp(av), Value::Timestamp(bv)) => av.cmp(bv),
        // Date vs timestamp: lift the date to midnight micros.
        (Value::Date(av), Value::Timestamp(bv)) => {
            (i64::from(*av).saturating_mul(86_400_000_000)).cmp(bv)
        }
        (Value::Timestamp(av), Value::Date(bv)) => {
            av.cmp(&i64::from(*bv).saturating_mul(86_400_000_000))
        }
        (Value::Time(av), Value::Time(bv)) => av.cmp(bv),
        (Value::Bool(av), Value::Bool(bv)) => av.cmp(bv),
        // Intervals order by their justified total microseconds
        // (months at 30 days, PG's comparison convention).
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
            let total = |m: i32, d: i32, u: i64| -> i128 {
                i128::from(m) * 30 * 86_400_000_000 + i128::from(d) * 86_400_000_000 + i128::from(u)
            };
            total(*am, *ad, *au).cmp(&total(*bm, *bd, *bu))
        }
        // v7.39 (round 511) — a tid orders by block then offset. GREATEST /
        // LEAST share this comparator with min/max's, and both used to reach
        // the `_ => Equal` below.
        (Value::Tid(b1, o1), Value::Tid(b2, o2)) => b1.cmp(b2).then(o1.cmp(o2)),
        (Value::Xid(a), Value::Xid(b)) => a.cmp(b),
        (Value::Cid(a), Value::Cid(b)) => a.cmp(b),
        // v7.39 (round 516) — anything with no arm here asks the comparison
        // the OPERATORS use instead of answering Equal.
        //
        // `_ => Equal` is not a neutral default in a min/max comparator: it
        // silently keeps whichever value arrived first. That is how round
        // 511's `max(ctid)` answered `(0,1)`, and how `network_larger`
        // answered the SMALLER address here — inet has a `network_cmp` in
        // the operator path and had no arm in this one. Delegating means a
        // type only has to teach the engine its order once.
        _ => crate::eval::binop::compare(spg_sql::ast::BinOp::Lt, a, b)
            .ok()
            .and_then(|v| match v {
                Value::Bool(true) => Some(Ordering::Less),
                Value::Bool(false) => {
                    match crate::eval::binop::compare(spg_sql::ast::BinOp::Gt, a, b) {
                        Ok(Value::Bool(true)) => Some(Ordering::Greater),
                        Ok(Value::Bool(false)) => Some(Ordering::Equal),
                        _ => None,
                    }
                }
                _ => None,
            })
            .unwrap_or(Ordering::Equal),
    }
}

pub(super) fn value_to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Float(x) => Some(*x),
        Value::Real(x) => Some(f64::from(*x)),
        Value::SmallInt(x) => Some(f64::from(*x)),
        Value::Int(x) => Some(f64::from(*x)),
        Value::BigInt(x) => Some(*x as f64),
        Value::Numeric { scaled, scale, .. } => {
            Some((*scaled as f64) / f64_powi(10.0, i32::from(*scale)))
        }
        _ => None,
    }
}

/// PG-style equality for nullif. Handles cross-numeric-width
/// comparison (Int vs BigInt vs SmallInt vs Float vs Numeric);
/// text matches text exactly; everything else uses derived
/// PartialEq.
pub(super) fn values_equal_for_nullif(a: &Value, b: &Value) -> bool {
    // Same-type fast path.
    if a == b {
        return true;
    }
    // Cross-int widening: SmallInt / Int / BigInt all comparable.
    let a_int = match a {
        Value::SmallInt(x) => Some(i64::from(*x)),
        Value::Int(x) => Some(i64::from(*x)),
        Value::BigInt(x) => Some(*x),
        _ => None,
    };
    let b_int = match b {
        Value::SmallInt(x) => Some(i64::from(*x)),
        Value::Int(x) => Some(i64::from(*x)),
        Value::BigInt(x) => Some(*x),
        _ => None,
    };
    if let (Some(a), Some(b)) = (a_int, b_int) {
        return a == b;
    }
    // Float / Numeric: widen to f64.
    let a_f = match a {
        Value::Float(x) => Some(*x),
        Value::SmallInt(x) => Some(f64::from(*x)),
        Value::Int(x) => Some(f64::from(*x)),
        Value::BigInt(x) => Some(*x as f64),
        Value::Numeric { scaled, scale, .. } => {
            Some((*scaled as f64) / f64_powi(10.0, i32::from(*scale)))
        }
        _ => None,
    };
    let b_f = match b {
        Value::Float(x) => Some(*x),
        Value::SmallInt(x) => Some(f64::from(*x)),
        Value::Int(x) => Some(f64::from(*x)),
        Value::BigInt(x) => Some(*x as f64),
        Value::Numeric { scaled, scale, .. } => {
            Some((*scaled as f64) / f64_powi(10.0, i32::from(*scale)))
        }
        _ => None,
    };
    if let (Some(a), Some(b)) = (a_f, b_f) {
        return a == b;
    }
    false
}

/// v7.17.0 — generate a RFC 4122 v4 (random) UUID. Layout: 16
/// random bytes with the version nibble (high nibble of byte 6)
/// pinned to `0100` (= 4) and the variant top bits (high two bits
/// of byte 8) pinned to `10` — exactly what PG's
/// `gen_random_uuid()` and the historical uuid-ossp
/// `uuid_generate_v4()` produce.
pub fn gen_random_uuid_bytes() -> [u8; 16] {
    let mut out = [0u8; 16];
    let hi = prng_next_u64().to_be_bytes();
    let lo = prng_next_u64().to_be_bytes();
    out[..8].copy_from_slice(&hi);
    out[8..].copy_from_slice(&lo);
    // Version 4: top nibble of byte 6 must be 0100.
    out[6] = (out[6] & 0x0f) | 0x40;
    // Variant 1 (RFC 4122): top two bits of byte 8 must be 10.
    out[8] = (out[8] & 0x3f) | 0x80;
    out
}

/// v7.39 (round 425) — render a value the way a MySQL client expects a
/// column with a DECLARED fractional-seconds precision to look: EXACTLY
/// `fsp` fractional digits, zero-padded (`DATETIME(3)` shows `.250`, and
/// `.000` for a whole second), or none at all at precision 0. PG's renderer
/// trims trailing zeros, which is right for PG and wrong for MySQL — the
/// stored instant is identical either way.
///
/// `fsp` is `ColumnSchema.mysql_fsp`, which is `None` for every PG column
/// and for any expression that reads no MySQL temporal column; those fall
/// straight through to [`value_to_text`].
#[must_use]
pub fn value_to_text_with_fsp(v: &Value, fsp: Option<u8>) -> String {
    let Some(fsp) = fsp else {
        return value_to_text(v);
    };
    let (whole, micros) = match v {
        Value::Timestamp(us) => (
            crate::eval::format_timestamp(us.div_euclid(1_000_000) * 1_000_000),
            us.rem_euclid(1_000_000),
        ),
        Value::Time(us) => (
            crate::eval::format_time(us.div_euclid(1_000_000) * 1_000_000),
            us.rem_euclid(1_000_000),
        ),
        other => return value_to_text(other),
    };
    if fsp == 0 {
        return whole;
    }
    let digits = usize::from(fsp.min(6));
    // `micros` is already truncated to the column's precision on write, so
    // this only ever pads — it never drops a digit the caller could see.
    let frac = format!("{micros:06}");
    format!("{whole}.{}", &frac[..digits])
}

pub fn value_to_text(v: &Value) -> String {
    value_to_text_styled(v, &crate::eval::RenderStyle::default())
}

/// v7.39 (GUC knife 3) — the canonical renderer under a session
/// `RenderStyle` (DateStyle / IntervalStyle / extra_float_digits).
/// `value_to_text` is the default-style shorthand.
pub fn value_to_text_styled(v: &Value, style: &crate::eval::RenderStyle) -> String {
    match v {
        // v7.5.0 — Value is #[non_exhaustive]; any future variant
        // without explicit text rendering hits the Debug fallback
        // at the end.
        Value::SmallInt(n) => format!("{n}"),
        Value::Int(n) => format!("{n}"),
        Value::BigInt(n) => format!("{n}"),
        // PG `float8out`: shortest round-trip, scientific notation past
        // the ±exponent thresholds, `Infinity` / `-Infinity` / `NaN` for
        // the non-finite values.
        Value::Float(x) => crate::eval::format_float_styled(*x, style),
        // v7.38 (read01, T-float4) — PG float4out (f32 shortest round-trip).
        Value::Real(x) => crate::eval::format_real_styled(*x, style),
        // v7.38 (read01, T11) — bpchar renders blank-padded (the stored form,
        // as PG's wire display). The ::text CAST strips (handled in cast.rs).
        Value::BpChar(s) => s.to_string(),
        // v4.9: JSON renders identically to Text — both are raw UTF-8.
        Value::Text(s) | Value::Json(s) => s.to_string(),
        Value::Bool(b) => (if *b { "true" } else { "false" }).into(),
        // v7.38 (read01, T3.C3) — arbitrary-precision NUMERIC renders its exact
        // decimal string.
        Value::NumericBig(b) => b.to_decimal_str(),
        // v7.38 (read01, T9) — PG record_out: `(f1,f2,...)`, NULL fields empty,
        // fields with special characters double-quoted (`\` and `"` escaped).
        Value::Composite(fields) => {
            let mut out = String::from("(");
            for (i, (_, fv)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if matches!(fv, Value::Null) {
                    continue;
                }
                let field = super::strings::value_to_format_text(fv);
                let needs_quote = field.is_empty()
                    || field
                        .chars()
                        .any(|c| matches!(c, ',' | '(' | ')' | '"' | '\\') || c.is_whitespace());
                if needs_quote {
                    out.push('"');
                    for c in field.chars() {
                        match c {
                            '"' => out.push_str("\"\""),
                            '\\' => out.push_str("\\\\"),
                            other => out.push(other),
                        }
                    }
                    out.push('"');
                } else {
                    out.push_str(&field);
                }
            }
            out.push(')');
            out
        }
        Value::Vector(v) => {
            let cells: Vec<String> = v.iter().map(|x| format!("{x}")).collect();
            format!("[{}]", cells.join(","))
        }
        // v6.0.1: render SQ8 cells dequantised, so SELECT output
        // matches the pgvector wire shape clients expect. The
        // recall envelope already absorbs the ≤ (max-min)/255/2
        // dequantisation error.
        Value::Sq8Vector(q) => {
            let cells: Vec<String> = spg_storage::quantize::dequantize(q)
                .iter()
                .map(|x| format!("{x}"))
                .collect();
            format!("[{}]", cells.join(","))
        }
        // v6.0.3: HalfVector cells dequantise bit-exactly to f32
        // for SELECT output.
        Value::HalfVector(h) => {
            let cells: Vec<String> = h.to_f32_vec().iter().map(|x| format!("{x}")).collect();
            format!("[{}]", cells.join(","))
        }
        Value::Numeric {
            scaled,
            scale,
            kind,
        } => format_numeric_kind(*kind, *scaled, *scale),
        Value::Date(d) => crate::eval::format_date_styled(*d, style),
        Value::Timestamp(t) => crate::eval::format_timestamp_styled(*t, style),
        Value::Interval {
            months,
            days,
            micros,
        } => crate::eval::format_interval_styled(*months, *days, *micros, style),
        Value::Null => "NULL".into(),
        // v7.10.4 — BYTEA renders as PG hex form.
        // v7.39 (round 524) — unless the session asked for `escape`.
        Value::Bytes(b) => {
            if style.bytea_escape {
                crate::eval::format::format_bytea_escape(b)
            } else {
                format_bytea_hex(b)
            }
        }
        // v7.10.9 — TEXT[] / INT[] / BIGINT[] render PG external form.
        Value::TextArray(items) => format_text_array(items),
        Value::IntArray(items) => format_int_array(items),
        Value::BigIntArray(items) => format_bigint_array(items),
        // v7.12.0 — tsvector / tsquery render PG external form.
        Value::TsVector(lexs) => format_tsvector(lexs),
        Value::TsQuery(ast) => format_tsquery(ast),
        // v7.17.0 — UUID renders canonical lowercase 8-4-4-4-12
        // hyphenated form (PG `uuid_out`).
        Value::Uuid(b) => spg_storage::format_uuid(b),
        // v7.17.0 Phase 3.P0-32 — TIME canonical text.
        Value::Time(us) => format_time(*us),
        // v7.17.0 Phase 3.P0-34 — TIMETZ canonical text.
        Value::TimeTz { us, offset_secs } => format_timetz(*us, *offset_secs),
        // v7.17.0 Phase 3.P0-33 — YEAR 4-digit zero-padded.
        Value::Year(y) => format!("{y:04}"),
        // v7.17.0 Phase 3.P0-35 — MONEY en_US locale.
        Value::Money(c) => format_money(*c),
        // v7.17.0 Phase 3.P0-38 — Range canonical form. Routes
        // through the engine's format_range_text to share the
        // single renderer with pgwire / sqllogictest.
        Value::Range { .. } => crate::conversions::format_range_text(v),
        // v7.17.0 Phase 3.P0-39 — Hstore canonical PG text form.
        Value::Hstore(pairs) => crate::conversions::format_hstore_text(pairs),
        // v7.17.0 Phase 3.P0-40 — 2D array canonical PG text form.
        Value::IntArray2D(rows) => crate::conversions::format_int_2d_text_pub(rows),
        Value::BigIntArray2D(rows) => crate::conversions::format_bigint_2d_text_pub(rows),
        Value::TextArray2D(rows) => crate::conversions::format_text_2d_text_pub(rows),
        Value::BoolArray2D(rows) => crate::conversions::format_bool_2d_text_pub(rows),
        // v7.37.5 γ — complete array-family rendering for the
        // ζ-A/γ/δ/ε first-class types.
        Value::BoolArray(items) => crate::eval::format_bool_array(items),
        Value::SmallIntArray(items) => crate::eval::format_smallint_array(items),
        Value::FloatArray(items) => crate::eval::format_float_array_styled(items, style),
        Value::NumericArray(items) => crate::eval::format_numeric_array(items),
        Value::DateArray(items) => crate::eval::format_date_array_styled(items, style),
        Value::TimestampArray(items) => {
            crate::eval::format_timestamp_array_styled(items, false, style)
        }
        Value::TimestamptzArray(items) => {
            crate::eval::format_timestamp_array_styled(items, true, style)
        }
        Value::UuidArray(items) => crate::eval::format_uuid_array(items),
        Value::JsonArray(items) | Value::JsonbArray(items) => crate::eval::format_text_array(items),
        Value::BytesArray(items) => crate::eval::format_bytea_array(items),
        Value::IntervalArray(items) => crate::eval::format_interval_array_styled(items, style),
        Value::MoneyArray(items) => crate::conversions::format_money_array(items),
        // v7.37.5 ε — geometry canonical PG text.
        Value::Point(p) => crate::conversions::format_point(*p),
        Value::Lseg(a, b) => crate::conversions::format_lseg(*a, *b),
        Value::Path { points, closed } => crate::conversions::format_path(points, *closed),
        Value::PgBox(ur, ll) => crate::conversions::format_pg_box(*ur, *ll),
        Value::Polygon(points) => crate::conversions::format_polygon(points),
        Value::Line { a, b, c } => crate::conversions::format_line(*a, *b, *c),
        Value::Circle { center, radius } => crate::conversions::format_circle(*center, *radius),
        // v7.37.5 δ — multirange canonical PG text.
        Value::Multirange { ranges, .. } => crate::conversions::format_multirange(ranges),
        // v7.37.5 ζ-A — network/MAC/bit/XML/char1.
        Value::Inet { family, bits, addr } => {
            crate::conversions::format_inet(*family, *bits, addr)
        }
        // v7.39 (round 262) — a CIDR ALWAYS shows its mask length, where
        // an inet omits a full-width one: `'192.168.1.5'::inet::cidr` is
        // `192.168.1.5/32` and `'::1'::inet::cidr` is `::1/128` (probed).
        // Both variants shared the inet renderer, so a full-width cidr
        // printed without its `/32`.
        Value::Cidr { family, bits, addr } => {
            crate::conversions::format_inet_full(*family, *bits, addr)
        }
        Value::Macaddr(b) => crate::conversions::format_macaddr(b),
        Value::Macaddr8(b) => crate::conversions::format_macaddr8(b),
        Value::PgLsn(l) => crate::conversions::format_pg_lsn(*l),
        Value::RegClass(_, name) | Value::RegProc(_, name) => name.to_string(),
        Value::RegType(_, name) => name.to_string(),
        // v7.39 (round 511) — PG renders a tid `(block,offset)`.
        Value::Tid(b, o) => alloc::format!("({b},{o})"),
        // A transaction / command id renders as its number.
        Value::Xid(x) => alloc::format!("{x}"),
        Value::Cid(c) => alloc::format!("{c}"),
        Value::BitString { nbits, bytes } => crate::conversions::format_bit_string(*nbits, bytes),
        Value::Xml(s) => s.to_string(),
        Value::Char1(b) => format!("{}", *b as char),
        // v7.5.0 — #[non_exhaustive] fallback for future Value variants.
        _ => format!("{v:?}"),
    }
}

/// Element count of a 1-D array value, or `None` when `v` is not a 1-D
/// array. Element-type-agnostic — every PG array element type is covered
/// so count-only callers (array_length / array_upper / array_lower /
/// array_ndims / array_dims / cardinality) stay uniform.
pub(crate) fn array_len(v: &Value) -> Option<usize> {
    match v {
        Value::TextArray(items)
        | Value::VarcharArray(items)
        | Value::CharArray(items)
        | Value::JsonArray(items)
        | Value::JsonbArray(items) => Some(items.len()),
        Value::IntArray(items) => Some(items.len()),
        Value::BigIntArray(items) => Some(items.len()),
        Value::SmallIntArray(items) => Some(items.len()),
        Value::BoolArray(items) => Some(items.len()),
        Value::FloatArray(items) => Some(items.len()),
        Value::NumericArray(items) => Some(items.len()),
        Value::DateArray(items) => Some(items.len()),
        Value::TimestampArray(items) | Value::TimestamptzArray(items) => Some(items.len()),
        Value::MoneyArray(items) => Some(items.len()),
        Value::IntervalArray(items) => Some(items.len()),
        Value::UuidArray(items) => Some(items.len()),
        Value::BytesArray(items) => Some(items.len()),
        _ => None,
    }
}

/// v7.39 (read01 round 76) — every element of an array as an owned `Value`,
/// or `None` when `v` is not an array at all. A 2-D matrix yields one 1-D
/// array `Value` per row, so a caller that recurses (JSON encoding) gets the
/// nesting for free.
///
/// This is the *iteration* half of the element menu whose *indexing* half is
/// `array_element_at`: without it, every consumer that WALKS an array was
/// written variant by variant, and the variants nobody had needed yet fell
/// into a `_ =>` that quietly did the wrong thing — `to_jsonb(ARRAY[[1,2]])`
/// rendered the *text* `"{{1,2}}"` as a JSON string instead of `[[1, 2]]`.
pub(crate) fn array_elements(v: &Value) -> Option<alloc::vec::Vec<Value<'static>>> {
    if let Some(n) = array_len(v) {
        let mut out = alloc::vec::Vec::with_capacity(n);
        for i in 0..n {
            out.push(array_element_at(v, i)?);
        }
        return Some(out);
    }
    // 2-D: one 1-D array per row, same element type.
    macro_rules! rows {
        ($m:expr, $variant:ident) => {
            Some($m.iter().map(|r| Value::$variant(r.clone())).collect())
        };
    }
    match v {
        Value::IntArray2D(m) => rows!(m, IntArray),
        Value::BigIntArray2D(m) => rows!(m, BigIntArray),
        Value::TextArray2D(m) => rows!(m, TextArray),
        Value::BoolArray2D(m) => rows!(m, BoolArray),
        _ => None,
    }
}

/// v7.38 (read01, T10) — the (rows, cols) dimensions of a 2-D array, or None
/// for anything that is not a 2-D matrix.
pub(super) fn array_2d_dims(v: &Value) -> Option<(usize, usize)> {
    match v {
        Value::IntArray2D(m) => Some((m.len(), m.first().map_or(0, alloc::vec::Vec::len))),
        Value::BigIntArray2D(m) => Some((m.len(), m.first().map_or(0, alloc::vec::Vec::len))),
        Value::TextArray2D(m) => Some((m.len(), m.first().map_or(0, alloc::vec::Vec::len))),
        Value::BoolArray2D(m) => Some((m.len(), m.first().map_or(0, alloc::vec::Vec::len))),
        _ => None,
    }
}

/// The `pos`-th (0-based) element of a 1-D array as an owned scalar
/// `Value` (a NULL hole becomes `Value::Null`), or `None` when `pos` is
/// out of range or `v` is not a 1-D array. O(1) per element. This is the
/// single per-type element menu shared by array subscript and
/// array_position / array_positions, which previously only matched
/// Text/Int/BigInt arrays and errored on every other element type.
pub(crate) fn array_element_at(v: &Value, pos: usize) -> Option<Value<'static>> {
    use alloc::borrow::Cow;
    macro_rules! nth {
        ($items:expr, $map:expr) => {
            $items
                .get(pos)
                .map(|e| e.as_ref().map_or(Value::Null, $map))
        };
    }
    match v {
        Value::TextArray(items) | Value::VarcharArray(items) | Value::CharArray(items) => {
            nth!(items, |s| Value::Text(Cow::Owned(s.clone())))
        }
        Value::JsonArray(items) | Value::JsonbArray(items) => {
            nth!(items, |s| Value::Json(Cow::Owned(s.clone())))
        }
        Value::IntArray(items) => nth!(items, |n| Value::Int(*n)),
        Value::BigIntArray(items) => nth!(items, |n| Value::BigInt(*n)),
        Value::SmallIntArray(items) => nth!(items, |n| Value::SmallInt(*n)),
        Value::BoolArray(items) => nth!(items, |b| Value::Bool(*b)),
        Value::FloatArray(items) => nth!(items, |f| Value::Float(*f)),
        Value::NumericArray(items) => {
            nth!(items, |t: &(i128, u16)| Value::Numeric {
                scaled: t.0,
                scale: t.1,
                kind: spg_storage::NumericKind::Finite
            })
        }
        Value::DateArray(items) => nth!(items, |d| Value::Date(*d)),
        Value::TimestampArray(items) | Value::TimestamptzArray(items) => {
            nth!(items, |t| Value::Timestamp(*t))
        }
        Value::MoneyArray(items) => nth!(items, |m| Value::Money(*m)),
        Value::IntervalArray(items) => nth!(items, |s| Value::Interval {
            months: s.months,
            days: s.days,
            micros: s.micros,
        }),
        Value::UuidArray(items) => nth!(items, |u| Value::Uuid(*u)),
        Value::BytesArray(items) => nth!(items, |b| Value::Bytes(Cow::Owned(b.clone()))),
        _ => None,
    }
}

/// v7.39 (read01 round 72) — rebuild an array of the SAME variant as `model`
/// from a list of element values. The mirror of `array_element_at`, and the
/// piece that was missing: without it, every array function that BUILDS a result
/// was written variant by variant, and the variants nobody had needed yet were
/// simply absent (`array_remove` had int arms only — see round 71).
///
/// A value that does not fit the model's element type is a caller error, and the
/// caller phrases it; here it becomes `None`.
pub(super) fn array_rebuild(model: &Value<'_>, elems: &[Value<'static>]) -> Option<Value<'static>> {
    macro_rules! build {
        ($variant:ident, $conv:expr) => {{
            let mut out = alloc::vec::Vec::with_capacity(elems.len());
            for e in elems {
                if matches!(e, Value::Null) {
                    out.push(None);
                    continue;
                }
                out.push(Some(($conv)(e)?));
            }
            Some(Value::$variant(out))
        }};
    }
    let as_i64 = |v: &Value<'_>| -> Option<i64> {
        match v {
            Value::SmallInt(n) => Some(i64::from(*n)),
            Value::Int(n) => Some(i64::from(*n)),
            Value::BigInt(n) => Some(*n),
            _ => None,
        }
    };
    match model {
        Value::TextArray(_) => build!(TextArray, |e: &Value<'_>| match e {
            Value::Text(s) => Some(s.as_ref().to_string()),
            _ => None,
        }),
        Value::VarcharArray(_) => build!(VarcharArray, |e: &Value<'_>| match e {
            Value::Text(s) => Some(s.as_ref().to_string()),
            _ => None,
        }),
        Value::JsonArray(_) => build!(JsonArray, |e: &Value<'_>| match e {
            Value::Json(s) | Value::Text(s) => Some(s.as_ref().to_string()),
            _ => None,
        }),
        Value::JsonbArray(_) => build!(JsonbArray, |e: &Value<'_>| match e {
            Value::Json(s) | Value::Text(s) => Some(s.as_ref().to_string()),
            _ => None,
        }),
        Value::IntArray(_) => build!(IntArray, |e: &Value<'_>| as_i64(e)
            .and_then(|n| i32::try_from(n).ok())),
        Value::BigIntArray(_) => build!(BigIntArray, as_i64),
        Value::SmallIntArray(_) => build!(SmallIntArray, |e: &Value<'_>| as_i64(e)
            .and_then(|n| i16::try_from(n).ok())),
        Value::BoolArray(_) => build!(BoolArray, |e: &Value<'_>| match e {
            Value::Bool(b) => Some(*b),
            _ => None,
        }),
        Value::FloatArray(_) => build!(FloatArray, |e: &Value<'_>| match e {
            Value::Float(f) => Some(*f),
            Value::Real(f) => Some(f64::from(*f)),
            other => as_i64(other).map(|n| n as f64),
        }),
        Value::NumericArray(_) => build!(NumericArray, |e: &Value<'_>| match e {
            Value::Numeric { scaled, scale, .. } => Some((*scaled, *scale)),
            other => as_i64(other).map(|n| (i128::from(n), 0u16)),
        }),
        Value::DateArray(_) => build!(DateArray, |e: &Value<'_>| match e {
            Value::Date(d) => Some(*d),
            _ => None,
        }),
        Value::TimestampArray(_) => build!(TimestampArray, |e: &Value<'_>| match e {
            Value::Timestamp(t) => Some(*t),
            _ => None,
        }),
        Value::TimestamptzArray(_) => build!(TimestamptzArray, |e: &Value<'_>| match e {
            Value::Timestamp(t) => Some(*t),
            _ => None,
        }),
        Value::MoneyArray(_) => build!(MoneyArray, |e: &Value<'_>| match e {
            Value::Money(m) => Some(*m),
            _ => None,
        }),
        Value::UuidArray(_) => build!(UuidArray, |e: &Value<'_>| match e {
            Value::Uuid(u) => Some(*u),
            _ => None,
        }),
        Value::BytesArray(_) => build!(BytesArray, |e: &Value<'_>| match e {
            Value::Bytes(b) => Some(b.as_ref().to_vec()),
            _ => None,
        }),
        Value::IntervalArray(_) => build!(IntervalArray, |e: &Value<'_>| match e {
            Value::Interval {
                months,
                days,
                micros,
            } => Some(spg_storage::IntervalSpan {
                months: *months,
                days: *days,
                micros: *micros,
            }),
            _ => None,
        }),
        _ => None,
    }
}

/// v7.39 (read01 round 73) — build an array Value from a list of element values,
/// choosing the element type PG would choose. ONE place, used by the `ARRAY[…]`
/// literal, by `array_agg`, and by the ordered-set aggregates.
///
/// This is the fifth site that had been written variant by variant with a text
/// fallback for everything it did not know (rounds 71/72 killed the first four).
/// The rule: a homogeneous non-numeric, non-text list keeps its type; the numeric
/// ladder unifies (float > numeric > bigint > int); anything mixed or unknown is
/// `text[]`, which is a DECISION. v7.39 (round 779 audit, I1) — PG does NOT
/// make the same one for the EMPTY case: bare `ARRAY[]` refuses there
/// (`cannot determine type of empty array`) while SPG answers `text[]`.
/// A deliberate superset, §9-ledgered: `ARRAY[]::t[]`, `'{}'::t[]` and every
/// non-empty list agree with PG exactly.
pub(crate) fn build_array_from_values(vals: &[Value<'static>]) -> Value<'static> {
    if let Some(v) = homogeneous_typed_array(vals) {
        return v;
    }
    let mut has_text = false;
    let mut has_float = false;
    let mut has_numeric = false;
    let mut has_bigint = false;
    let mut has_int = false;
    for v in vals {
        match v {
            Value::Null => {}
            Value::Int(_) | Value::SmallInt(_) => has_int = true,
            Value::BigInt(_) => has_bigint = true,
            Value::Numeric { .. } | Value::NumericBig(_) => has_numeric = true,
            Value::Float(_) | Value::Real(_) => has_float = true,
            _ => has_text = true,
        }
    }
    let as_i64 = |v: &Value<'_>| -> Option<i64> {
        match v {
            Value::SmallInt(n) => Some(i64::from(*n)),
            Value::Int(n) => Some(i64::from(*n)),
            Value::BigInt(n) => Some(*n),
            _ => None,
        }
    };
    if !has_text {
        if has_float {
            return Value::FloatArray(
                vals.iter()
                    .map(|v| match v {
                        Value::Null => None,
                        Value::Float(f) => Some(*f),
                        Value::Real(f) => Some(f64::from(*f)),
                        #[allow(clippy::cast_precision_loss)]
                        Value::Numeric { scaled, scale, .. } => {
                            Some(*scaled as f64 / libm::pow(10.0, f64::from(*scale)))
                        }
                        other => as_i64(other).map(|n| n as f64),
                    })
                    .collect(),
            );
        }
        if has_numeric {
            // A NumericBig / non-finite value cannot live in a (i128, scale)
            // cell, so it falls through to text[] rather than losing precision.
            if vals.iter().all(|v| {
                matches!(
                    v,
                    Value::Null
                        | Value::SmallInt(_)
                        | Value::Int(_)
                        | Value::BigInt(_)
                        | Value::Numeric {
                            kind: spg_storage::NumericKind::Finite,
                            ..
                        }
                )
            }) {
                return Value::NumericArray(
                    vals.iter()
                        .map(|v| match v {
                            Value::Null => None,
                            Value::Numeric { scaled, scale, .. } => Some((*scaled, *scale)),
                            other => as_i64(other).map(|n| (i128::from(n), 0u16)),
                        })
                        .collect(),
                );
            }
        } else if has_bigint {
            return Value::BigIntArray(vals.iter().map(as_i64).collect());
        } else if has_int {
            return Value::IntArray(
                vals.iter()
                    .map(|v| as_i64(v).and_then(|n| i32::try_from(n).ok()))
                    .collect(),
            );
        }
    }
    Value::TextArray(
        vals.iter()
            .map(|v| match v {
                Value::Null => None,
                Value::Text(s) | Value::Json(s) => Some(s.as_ref().to_string()),
                other => Some(crate::eval::value_to_text(other)),
            })
            .collect(),
    )
}

/// An `ARRAY[…]` / `array_agg` of ONE non-numeric, non-text type keeps that
/// type. `None` for an empty list or a mix.
pub(crate) fn homogeneous_typed_array(vals: &[Value<'static>]) -> Option<Value<'static>> {
    let first = vals.iter().find(|v| !matches!(v, Value::Null))?;
    macro_rules! collect {
        ($variant:ident, $pat:pat => $val:expr) => {{
            let mut out = alloc::vec::Vec::with_capacity(vals.len());
            for v in vals {
                match v {
                    Value::Null => out.push(None),
                    $pat => out.push(Some($val)),
                    _ => return None,
                }
            }
            Some(Value::$variant(out))
        }};
    }
    match first {
        Value::Bool(_) => collect!(BoolArray, Value::Bool(b) => *b),
        Value::Date(_) => collect!(DateArray, Value::Date(d) => *d),
        Value::Timestamp(_) => collect!(TimestampArray, Value::Timestamp(t) => *t),
        Value::Uuid(_) => collect!(UuidArray, Value::Uuid(u) => *u),
        Value::Money(_) => collect!(MoneyArray, Value::Money(m) => *m),
        Value::Bytes(_) => collect!(BytesArray, Value::Bytes(b) => b.as_ref().to_vec()),
        Value::Interval { .. } => {
            let mut out = alloc::vec::Vec::with_capacity(vals.len());
            for v in vals {
                match v {
                    Value::Null => out.push(None),
                    Value::Interval {
                        months,
                        days,
                        micros,
                    } => out.push(Some(spg_storage::IntervalSpan {
                        months: *months,
                        days: *days,
                        micros: *micros,
                    })),
                    _ => return None,
                }
            }
            Some(Value::IntervalArray(out))
        }
        _ => None,
    }
}

/// v7.39 (read01 round 75) — build a 2-D array from rows that are themselves
/// arrays. `None` when the list is not all-arrays (the caller then treats it as
/// a 1-D list).
///
/// SEVENTH site of the per-variant pattern this campaign has been unpicking: the
/// INSERT literal path had its OWN array builder, and it did not know 2-D at all
/// — an `ARRAY[ARRAY[…]]` in a VALUES list collapsed to text[]. One builder now.
/// v7.39 (read01 round 92) — a 2-D array literal like `{{1,2},{3,4}}`. If the
/// brace-stripped inner opens with `{`, split it into the top-level `{…}` rows
/// (depth-aware, respecting nesting and double-quoted strings), else return None
/// so the caller runs its 1-D path. The `ARRAY[[…]]` constructor already made
/// 2-D values (round 75); the text-literal cast — the form pg_dump emits —
/// never learned nested braces and split the whole thing on the first comma.
pub(crate) fn split_2d_rows(s: &str) -> Option<Vec<alloc::string::String>> {
    let trimmed = s.trim();
    let inner = trimmed
        .strip_prefix('{')
        .and_then(|x| x.strip_suffix('}'))?
        .trim();
    if !inner.starts_with('{') {
        return None;
    }
    let mut rows = alloc::vec::Vec::new();
    let bytes = inner.as_bytes();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut in_quote = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_quote {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == b'"' {
                in_quote = false;
            }
        } else {
            match c {
                b'"' => in_quote = true,
                b'{' => depth += 1,
                b'}' => depth -= 1,
                b',' if depth == 0 => {
                    rows.push(inner[start..i].trim().to_string());
                    start = i + 1;
                }
                _ => {}
            }
        }
        i += 1;
    }
    rows.push(inner[start..].trim().to_string());
    Some(rows)
}

pub(crate) fn build_2d_from_rows(rows: &[Value<'static>]) -> Option<Value<'static>> {
    if rows.is_empty() || !rows.iter().all(|v| array_len(v).is_some()) {
        return None;
    }
    let width = array_len(&rows[0])?;
    if !rows.iter().all(|v| array_len(v) == Some(width)) {
        return None;
    }
    if rows.iter().all(|v| matches!(v, Value::BoolArray(_))) {
        return Some(Value::BoolArray2D(
            rows.iter()
                .map(|v| match v {
                    Value::BoolArray(r) => r.clone(),
                    _ => unreachable!("checked"),
                })
                .collect(),
        ));
    }
    if rows.iter().all(|v| matches!(v, Value::IntArray(_))) {
        return Some(Value::IntArray2D(
            rows.iter()
                .map(|v| match v {
                    Value::IntArray(r) => r.clone(),
                    _ => unreachable!("checked"),
                })
                .collect(),
        ));
    }
    if rows
        .iter()
        .all(|v| matches!(v, Value::IntArray(_) | Value::BigIntArray(_)))
    {
        return Some(Value::BigIntArray2D(
            rows.iter()
                .map(|v| match v {
                    Value::BigIntArray(r) => r.clone(),
                    Value::IntArray(r) => r.iter().map(|c| c.map(i64::from)).collect(),
                    _ => unreachable!("checked"),
                })
                .collect(),
        ));
    }
    // Everything else renders into the text 2-D form, element by element, with
    // the SCALAR rendering (a cell pulled out with `[i][j]::text` must read like
    // a scalar).
    Some(Value::TextArray2D(
        rows.iter()
            .map(|v| {
                let n = array_len(v).unwrap_or(0);
                (0..n)
                    .map(|i| match array_element_at(v, i) {
                        None | Some(Value::Null) => None,
                        Some(x) => Some(crate::eval::value_to_text(&x)),
                    })
                    .collect()
            })
            .collect(),
    ))
}

/// v7.39 (round 236) — PG's array functions treat a multidimensional array
/// as its elements in row-major order: `array_to_string(ARRAY[[1,2],[3,4]],
/// ',')` is `1,2,3,4` and `unnest` of it yields four rows. SPG stores 2-D
/// arrays as their own variants, and the generic `array_len` /
/// element-access helpers only knew the 1-D ones, so both functions
/// rejected the value outright. Flattening here keeps every caller generic.
pub(crate) fn flatten_2d(v: &Value<'_>) -> Option<Value<'static>> {
    Some(match v {
        Value::IntArray2D(rows) => Value::IntArray(rows.iter().flatten().copied().collect()),
        Value::BigIntArray2D(rows) => Value::BigIntArray(rows.iter().flatten().copied().collect()),
        Value::BoolArray2D(rows) => Value::BoolArray(rows.iter().flatten().copied().collect()),
        Value::TextArray2D(rows) => Value::TextArray(rows.iter().flatten().cloned().collect()),
        _ => return None,
    })
}
