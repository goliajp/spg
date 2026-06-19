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
pub(super) fn value_cmp_for_min_max(a: &Value, b: &Value) -> core::cmp::Ordering {
    use core::cmp::Ordering;
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
    // Text/Text.
    match (a, b) {
        (Value::Text(av), Value::Text(bv)) => av.cmp(bv),
        (Value::Bytes(av), Value::Bytes(bv)) => av.cmp(bv),
        _ => Ordering::Equal,
    }
}

pub(super) fn value_to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Float(x) => Some(*x),
        Value::SmallInt(x) => Some(f64::from(*x)),
        Value::Int(x) => Some(f64::from(*x)),
        Value::BigInt(x) => Some(*x as f64),
        Value::Numeric { scaled, scale } => {
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
        Value::Numeric { scaled, scale } => {
            Some((*scaled as f64) / f64_powi(10.0, i32::from(*scale)))
        }
        _ => None,
    };
    let b_f = match b {
        Value::Float(x) => Some(*x),
        Value::SmallInt(x) => Some(f64::from(*x)),
        Value::Int(x) => Some(f64::from(*x)),
        Value::BigInt(x) => Some(*x as f64),
        Value::Numeric { scaled, scale } => {
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

pub(super) fn value_to_text(v: &Value) -> String {
    match v {
        // v7.5.0 — Value is #[non_exhaustive]; any future variant
        // without explicit text rendering hits the Debug fallback
        // at the end.
        Value::SmallInt(n) => format!("{n}"),
        Value::Int(n) => format!("{n}"),
        Value::BigInt(n) => format!("{n}"),
        Value::Float(x) => format!("{x}"),
        // v4.9: JSON renders identically to Text — both are raw UTF-8.
        Value::Text(s) | Value::Json(s) => s.clone(),
        Value::Bool(b) => (if *b { "true" } else { "false" }).into(),
        Value::Vector(v) => {
            let cells: Vec<String> = v.iter().map(|x| format!("{x}")).collect();
            format!("[{}]", cells.join(", "))
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
            format!("[{}]", cells.join(", "))
        }
        // v6.0.3: HalfVector cells dequantise bit-exactly to f32
        // for SELECT output.
        Value::HalfVector(h) => {
            let cells: Vec<String> = h.to_f32_vec().iter().map(|x| format!("{x}")).collect();
            format!("[{}]", cells.join(", "))
        }
        Value::Numeric { scaled, scale } => format_numeric(*scaled, *scale),
        Value::Date(d) => format_date(*d),
        Value::Timestamp(t) => format_timestamp(*t),
        Value::Interval {
            months,
            days,
            micros,
        } => format_interval(*months, *days, *micros),
        Value::Null => "NULL".into(),
        // v7.10.4 — BYTEA renders as PG hex form.
        Value::Bytes(b) => format_bytea_hex(b),
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
        // v7.37.5 γ — complete array-family rendering for the
        // ζ-A/γ/δ/ε first-class types.
        Value::BoolArray(items) => crate::eval::format_bool_array(items),
        Value::SmallIntArray(items) => crate::eval::format_smallint_array(items),
        Value::FloatArray(items) => crate::eval::format_float_array(items),
        Value::NumericArray(items) => crate::eval::format_numeric_array(items),
        Value::DateArray(items) => crate::eval::format_date_array(items),
        Value::TimestampArray(items) => crate::eval::format_timestamp_array(items, false),
        Value::TimestamptzArray(items) => crate::eval::format_timestamp_array(items, true),
        Value::UuidArray(items) => crate::eval::format_uuid_array(items),
        Value::JsonArray(items) | Value::JsonbArray(items) => crate::eval::format_text_array(items),
        Value::BytesArray(items) => crate::eval::format_bytea_array(items),
        Value::IntervalArray(items) => crate::eval::format_interval_array(items),
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
        Value::Inet { family, bits, addr } | Value::Cidr { family, bits, addr } => {
            crate::conversions::format_inet(*family, *bits, addr)
        }
        Value::Macaddr(b) => crate::conversions::format_macaddr(b),
        Value::Macaddr8(b) => crate::conversions::format_macaddr8(b),
        Value::BitString { nbits, bytes } => crate::conversions::format_bit_string(*nbits, bytes),
        Value::Xml(s) => s.clone(),
        Value::Char1(b) => format!("{}", *b as char),
        // v7.5.0 — #[non_exhaustive] fallback for future Value variants.
        _ => format!("{v:?}"),
    }
}
