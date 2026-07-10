//! Type conversions — Value/literal <-> text/bytes/special-format. The
//! coercion entry point (`coerce_value`) plus every parser/formatter it
//! leans on: bytea, text/2-D arrays, hstore, ranges, money, time, year,
//! and literal->Value. Split out of `lib.rs` (v7.32 engine
//! modularisation); a self-contained cluster (its members call each
//! other), depending only on spg_storage/spg_sql, `eval`, and `numeric`.

use alloc::string::ToString;
use alloc::vec::Vec;

use spg_sql::ast::{ColumnTypeName, Expr, Literal, UnOp, VecEncoding as SqlVecEncoding};
use spg_storage::{ColumnSchema, DataType, StorageError, Value, VecEncoding};

use crate::EngineError;
use crate::eval::{self, EvalContext, EvalError};
use crate::numeric::{
    numeric_from_float, numeric_from_integer, numeric_rescale, numeric_round_to_integer,
    parse_numeric_text,
};

/// v7.10.4 — decode a BYTEA literal. Accepts:
///   * `\xDEADBEEF` (case-insensitive hex; whitespace stripped)
///   * `Hello\000world` (backslash escape form; `\\` for literal backslash)
///   * Anything else → raw UTF-8 bytes of the input (PG accepts this too).
pub(crate) fn decode_bytea_literal(s: &str) -> Result<alloc::vec::Vec<u8>, &'static str> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("\\x").or_else(|| s.strip_prefix("\\X")) {
        // Hex form. Each pair of hex digits → one byte.
        let cleaned: alloc::string::String = hex.chars().filter(|c| !c.is_whitespace()).collect();
        if cleaned.len() % 2 != 0 {
            return Err("odd-length hex literal");
        }
        let mut out = alloc::vec::Vec::with_capacity(cleaned.len() / 2);
        let cleaned_bytes = cleaned.as_bytes();
        for i in (0..cleaned_bytes.len()).step_by(2) {
            let hi = hex_nibble(cleaned_bytes[i])?;
            let lo = hex_nibble(cleaned_bytes[i + 1])?;
            out.push((hi << 4) | lo);
        }
        return Ok(out);
    }
    // Escape form or raw. Walk char-by-char; `\\` and `\NNN` octal
    // sequences decode; anything else is a literal byte.
    let bytes = s.as_bytes();
    let mut out = alloc::vec::Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && i + 1 < bytes.len() {
            let n = bytes[i + 1];
            if n == b'\\' {
                out.push(b'\\');
                i += 2;
                continue;
            }
            if n.is_ascii_digit()
                && i + 3 < bytes.len()
                && bytes[i + 2].is_ascii_digit()
                && bytes[i + 3].is_ascii_digit()
            {
                let oct = |x: u8| (x - b'0') as u32;
                let v = oct(n) * 64 + oct(bytes[i + 2]) * 8 + oct(bytes[i + 3]);
                if v <= 0xFF {
                    out.push(v as u8);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(b);
        i += 1;
    }
    Ok(out)
}

pub(crate) fn hex_nibble(b: u8) -> Result<u8, &'static str> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err("invalid hex digit"),
    }
}

/// v7.37.5 γ — uniform array-of-scalar shape detector. Returns
/// `Some(kind)` only when every non-NULL element fits the same
/// new-array element type; `None` falls back to the legacy
/// `array_literal_widen` Int/BigInt/Text path.
#[derive(Clone, Copy)]
enum UniformArrayKind {
    Bool,
    Float,
    Numeric,
    Date,
    Timestamp,
    Uuid,
    Bytes,
    Interval,
    Money,
}

impl UniformArrayKind {
    fn build(self, items: alloc::vec::Vec<Value<'static>>) -> Value<'static> {
        match self {
            Self::Bool => Value::BoolArray(
                items
                    .into_iter()
                    .map(|v| match v {
                        Value::Null => None,
                        Value::Bool(b) => Some(b),
                        _ => unreachable!("uniform Bool"),
                    })
                    .collect(),
            ),
            Self::Float => Value::FloatArray(
                items
                    .into_iter()
                    .map(|v| match v {
                        Value::Null => None,
                        Value::Float(x) => Some(x),
                        _ => unreachable!("uniform Float"),
                    })
                    .collect(),
            ),
            Self::Numeric => Value::NumericArray(
                items
                    .into_iter()
                    .map(|v| match v {
                        Value::Null => None,
                        Value::Numeric { scaled, scale, .. } => Some((scaled, scale)),
                        _ => unreachable!("uniform Numeric"),
                    })
                    .collect(),
            ),
            Self::Date => Value::DateArray(
                items
                    .into_iter()
                    .map(|v| match v {
                        Value::Null => None,
                        Value::Date(d) => Some(d),
                        _ => unreachable!("uniform Date"),
                    })
                    .collect(),
            ),
            Self::Timestamp => Value::TimestampArray(
                items
                    .into_iter()
                    .map(|v| match v {
                        Value::Null => None,
                        Value::Timestamp(t) => Some(t),
                        _ => unreachable!("uniform Timestamp"),
                    })
                    .collect(),
            ),
            Self::Uuid => Value::UuidArray(
                items
                    .into_iter()
                    .map(|v| match v {
                        Value::Null => None,
                        Value::Uuid(b) => Some(b),
                        _ => unreachable!("uniform Uuid"),
                    })
                    .collect(),
            ),
            Self::Bytes => Value::BytesArray(
                items
                    .into_iter()
                    .map(|v| match v {
                        Value::Null => None,
                        Value::Bytes(b) => Some(b.into_owned()),
                        _ => unreachable!("uniform Bytes"),
                    })
                    .collect(),
            ),
            Self::Interval => Value::IntervalArray(
                items
                    .into_iter()
                    .map(|v| match v {
                        Value::Null => None,
                        Value::Interval {
                            months,
                            days,
                            micros,
                        } => Some(spg_storage::IntervalSpan {
                            months,
                            days,
                            micros,
                        }),
                        _ => unreachable!("uniform Interval"),
                    })
                    .collect(),
            ),
            Self::Money => Value::MoneyArray(
                items
                    .into_iter()
                    .map(|v| match v {
                        Value::Null => None,
                        Value::Money(c) => Some(c),
                        _ => unreachable!("uniform Money"),
                    })
                    .collect(),
            ),
        }
    }
}

fn widen_uniform_typed(items: &[Value<'static>]) -> Option<UniformArrayKind> {
    let mut kind: Option<UniformArrayKind> = None;
    let mut saw_non_null = false;
    for v in items {
        let this = match v {
            Value::Null => continue,
            Value::Bool(_) => UniformArrayKind::Bool,
            Value::Float(_) => UniformArrayKind::Float,
            Value::Numeric { .. } => UniformArrayKind::Numeric,
            Value::Date(_) => UniformArrayKind::Date,
            Value::Timestamp(_) => UniformArrayKind::Timestamp,
            Value::Uuid(_) => UniformArrayKind::Uuid,
            Value::Bytes(_) => UniformArrayKind::Bytes,
            Value::Interval { .. } => UniformArrayKind::Interval,
            Value::Money(_) => UniformArrayKind::Money,
            // Int / BigInt / Text / Json — defer to the legacy
            // Int/Text widen below so the existing IntArray /
            // BigIntArray / TextArray behaviour is unchanged.
            _ => return None,
        };
        match kind {
            None => kind = Some(this),
            Some(prev) if discriminant_eq(prev, this) => {}
            Some(_) => return None,
        }
        saw_non_null = true;
    }
    if saw_non_null { kind } else { None }
}

fn discriminant_eq(a: UniformArrayKind, b: UniformArrayKind) -> bool {
    matches!(
        (a, b),
        (UniformArrayKind::Bool, UniformArrayKind::Bool)
            | (UniformArrayKind::Float, UniformArrayKind::Float)
            | (UniformArrayKind::Numeric, UniformArrayKind::Numeric)
            | (UniformArrayKind::Date, UniformArrayKind::Date)
            | (UniformArrayKind::Timestamp, UniformArrayKind::Timestamp)
            | (UniformArrayKind::Uuid, UniformArrayKind::Uuid)
            | (UniformArrayKind::Bytes, UniformArrayKind::Bytes)
            | (UniformArrayKind::Interval, UniformArrayKind::Interval)
            | (UniformArrayKind::Money, UniformArrayKind::Money)
    )
}

/// v7.10.11 — decode a PG TEXT[] external array form
/// (`{a,b,NULL}` with optional double-quoted elements). The
/// engine takes a leading/trailing `{`/`}` and splits at commas.
/// Quoted elements (`"hello, world"`) preserve embedded commas;
/// `\\` and `\"` decode to literal backslash / quote. Plain
/// unquoted `NULL` (case-insensitive) maps to `None`.
/// v7.11.13 — pick the array type for `ARRAY[lit, …]` from the
/// element values. Single-element-type rules:
///   - all NULL / all Text → TextArray
///   - all Int (or Int+NULL) → IntArray
///   - any BigInt without Text → BigIntArray (widening)
///   - any Text → TextArray (fallback; non-string elements
///     render as text)
pub(crate) fn array_literal_widen(items: alloc::vec::Vec<Value<'static>>) -> Value<'static> {
    // v7.37.5 γ — first, detect a uniform new-array-type. If every
    // non-NULL element shares one of the array-of-scalar element
    // shapes (Bool / Float / Numeric / Date / Timestamp / Uuid /
    // Bytes / Interval), build the matching typed array directly
    // so INSERT to a typed column doesn't have to go through the
    // TextArray fallback + coerce chain.
    if let Some(arr) = widen_uniform_typed(&items) {
        return arr.build(items);
    }
    let mut has_text = false;
    let mut has_bigint = false;
    let mut has_int = false;
    for v in &items {
        match v {
            Value::Null => {}
            Value::Text(_) | Value::Json(_) => has_text = true,
            Value::BigInt(_) => has_bigint = true,
            Value::Int(_) | Value::SmallInt(_) => has_int = true,
            _ => has_text = true,
        }
    }
    if has_text || (!has_bigint && !has_int) {
        let out: alloc::vec::Vec<Option<alloc::string::String>> = items
            .into_iter()
            .map(|v| match v {
                Value::Null => None,
                Value::Text(s) | Value::Json(s) => Some(s.into_owned()),
                other => Some(alloc::format!("{other:?}")),
            })
            .collect();
        return Value::TextArray(out);
    }
    if has_bigint {
        let out: alloc::vec::Vec<Option<i64>> = items
            .into_iter()
            .map(|v| match v {
                Value::Null => None,
                Value::Int(n) => Some(i64::from(n)),
                Value::SmallInt(n) => Some(i64::from(n)),
                Value::BigInt(n) => Some(n),
                _ => unreachable!("widen: unexpected non-integer in BigInt path"),
            })
            .collect();
        return Value::BigIntArray(out);
    }
    let out: alloc::vec::Vec<Option<i32>> = items
        .into_iter()
        .map(|v| match v {
            Value::Null => None,
            Value::Int(n) => Some(n),
            Value::SmallInt(n) => Some(i32::from(n)),
            _ => unreachable!("widen: unexpected non-i32-compatible in Int path"),
        })
        .collect();
    Value::IntArray(out)
}

pub(crate) fn decode_text_array_literal(
    s: &str,
) -> Result<alloc::vec::Vec<Option<alloc::string::String>>, &'static str> {
    let trimmed = s.trim();
    let inner = trimmed
        .strip_prefix('{')
        .and_then(|x| x.strip_suffix('}'))
        .ok_or("TEXT[] literal must be enclosed in '{...}'")?;
    let mut out: alloc::vec::Vec<Option<alloc::string::String>> = alloc::vec::Vec::new();
    if inner.trim().is_empty() {
        return Ok(out);
    }
    let bytes = inner.as_bytes();
    let mut i = 0;
    while i <= bytes.len() {
        // Skip leading whitespace.
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        // Quoted element.
        if i < bytes.len() && bytes[i] == b'"' {
            i += 1; // open quote
            let mut buf = alloc::string::String::new();
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    buf.push(bytes[i + 1] as char);
                    i += 2;
                } else {
                    buf.push(bytes[i] as char);
                    i += 1;
                }
            }
            if i >= bytes.len() {
                return Err("unterminated quoted element");
            }
            i += 1; // close quote
            out.push(Some(buf));
        } else {
            // Unquoted element — read until next comma or end.
            let start = i;
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            let raw = inner[start..i].trim();
            if raw.eq_ignore_ascii_case("NULL") {
                out.push(None);
            } else {
                out.push(Some(alloc::string::ToString::to_string(raw)));
            }
        }
        // Skip whitespace, expect comma or end.
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] != b',' {
            return Err("expected ',' between TEXT[] elements");
        }
        i += 1;
    }
    Ok(out)
}

/// v7.10.11 — encode a TEXT[] back into the PG external array
/// form. NULL elements become the literal `NULL`; elements
/// containing commas, quotes, backslashes, or braces are
/// double-quoted with `\\` / `\"` escapes.
pub(crate) fn encode_text_array(items: &[Option<alloc::string::String>]) -> alloc::string::String {
    let mut out = alloc::string::String::with_capacity(2 + items.len() * 8);
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            Some(s) => {
                let needs_quote = s.is_empty()
                    || s.eq_ignore_ascii_case("NULL")
                    || s.chars()
                        .any(|c| matches!(c, ',' | '{' | '}' | '"' | '\\' | ' ' | '\t'));
                if needs_quote {
                    out.push('"');
                    for c in s.chars() {
                        if c == '"' || c == '\\' {
                            out.push('\\');
                        }
                        out.push(c);
                    }
                    out.push('"');
                } else {
                    out.push_str(s);
                }
            }
        }
    }
    out.push('}');
    out
}

/// v7.10.4 — encode BYTEA bytes in PG hex output format
/// (`\x` prefix, lowercase hex pairs). Used by Text-side
/// round-trip + the wire layer's text-mode encoder.
pub(crate) fn encode_bytea_hex(b: &[u8]) -> alloc::string::String {
    let mut out = alloc::string::String::with_capacity(2 + 2 * b.len());
    out.push_str("\\x");
    for byte in b {
        let hi = byte >> 4;
        let lo = byte & 0x0F;
        out.push(hex_digit(hi));
        out.push(hex_digit(lo));
    }
    out
}

pub(crate) const fn hex_digit(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + n - 10) as char,
        _ => '?',
    }
}

/// v7.17.0 Phase 3.P0-39 — parse a PG `hstore` text literal into
/// a flat key→value map. Empty string → empty map. Duplicate
/// keys take last-write-wins (matches PG `hstore_in`).
///
/// Accepted shapes (minimal subset):
///   * `'a=>1, b=>2'`            — bareword keys/values
///   * `'"a"=>"1", "b"=>"2"'`    — quoted keys/values
///   * `'a=>NULL'`               — case-insensitive NULL token
///     surfaces as `None` (no quotes around NULL)
///
/// Returns None on parse failure → caller surfaces as hard error.
pub(crate) fn parse_hstore_str(
    s: &str,
) -> Option<Vec<(alloc::string::String, Option<alloc::string::String>)>> {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut out: Vec<(alloc::string::String, Option<alloc::string::String>)> = Vec::new();
    let skip_ws = |bytes: &[u8], i: &mut usize| {
        while *i < bytes.len() && matches!(bytes[*i], b' ' | b'\t' | b'\n' | b'\r') {
            *i += 1;
        }
    };
    let parse_token = |bytes: &[u8], i: &mut usize| -> Option<alloc::string::String> {
        if *i >= bytes.len() {
            return None;
        }
        if bytes[*i] == b'"' {
            *i += 1;
            let mut out = alloc::string::String::new();
            while *i < bytes.len() {
                match bytes[*i] {
                    b'"' => {
                        *i += 1;
                        return Some(out);
                    }
                    b'\\' if *i + 1 < bytes.len() => {
                        out.push(bytes[*i + 1] as char);
                        *i += 2;
                    }
                    c => {
                        out.push(c as char);
                        *i += 1;
                    }
                }
            }
            None
        } else {
            let start = *i;
            while *i < bytes.len()
                && !matches!(bytes[*i], b' ' | b'\t' | b'\n' | b'\r' | b',' | b'=')
            {
                *i += 1;
            }
            if *i == start {
                return None;
            }
            Some(alloc::str::from_utf8(&bytes[start..*i]).ok()?.to_string())
        }
    };
    skip_ws(bytes, &mut i);
    while i < bytes.len() {
        let key = parse_token(bytes, &mut i)?;
        skip_ws(bytes, &mut i);
        if i + 1 >= bytes.len() || bytes[i] != b'=' || bytes[i + 1] != b'>' {
            return None;
        }
        i += 2;
        skip_ws(bytes, &mut i);
        // Check for unquoted NULL token (case-insensitive).
        let val_token = if i + 4 <= bytes.len()
            && bytes[i..i + 4].eq_ignore_ascii_case(b"NULL")
            && (i + 4 == bytes.len() || matches!(bytes[i + 4], b' ' | b'\t' | b',' | b'\n' | b'\r'))
        {
            i += 4;
            None
        } else {
            Some(parse_token(bytes, &mut i)?)
        };
        // Replace any existing entry with the same key (last-wins).
        if let Some(pos) = out.iter().position(|(k, _)| k == &key) {
            out[pos] = (key, val_token);
        } else {
            out.push((key, val_token));
        }
        skip_ws(bytes, &mut i);
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b',' {
            i += 1;
            skip_ws(bytes, &mut i);
            continue;
        }
        return None;
    }
    Some(out)
}

/// v7.17.0 Phase 3.P0-39 — render a hstore as canonical PG text
/// form `"k"=>"v"` (keys and non-NULL values always quoted;
/// NULL token is bare).
pub(crate) fn format_hstore_str(
    pairs: &[(alloc::string::String, Option<alloc::string::String>)],
) -> alloc::string::String {
    let mut out = alloc::string::String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push('"');
        out.push_str(k);
        out.push_str("\"=>");
        match v {
            None => out.push_str("NULL"),
            Some(val) => {
                out.push('"');
                out.push_str(val);
                out.push('"');
            }
        }
    }
    out
}

/// v7.17.0 Phase 3.P0-39 — pub re-export so pgwire + sqllogictest
/// share the single hstore renderer.
pub fn format_hstore_text(
    pairs: &[(alloc::string::String, Option<alloc::string::String>)],
) -> alloc::string::String {
    format_hstore_str(pairs)
}

// ─── v7.17.0 Phase 3.P0-40 — 2D array parse + display ─────────

/// Split a PG external 2D-array literal `'{{a,b},{c,d}}'` into
/// per-row token lists. Returns Err on shape mismatch.
pub(crate) fn split_2d_literal(s: &str) -> Result<Vec<Vec<alloc::string::String>>, &'static str> {
    let s = s.trim();
    let outer = s
        .strip_prefix('{')
        .and_then(|x| x.strip_suffix('}'))
        .ok_or("missing outer '{...}' braces")?;
    let trimmed = outer.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut rows: Vec<Vec<alloc::string::String>> = Vec::new();
    let mut i = 0;
    let bytes = trimmed.as_bytes();
    while i < bytes.len() {
        while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r' | b',') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] != b'{' {
            return Err("expected '{' opening a row");
        }
        i += 1;
        let row_start = i;
        let mut depth = 1;
        while i < bytes.len() && depth > 0 {
            match bytes[i] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
            if depth > 0 {
                i += 1;
            }
        }
        if depth != 0 {
            return Err("unbalanced '{...}' in row");
        }
        let row_text = &trimmed[row_start..i];
        i += 1;
        let cells: Vec<alloc::string::String> = if row_text.trim().is_empty() {
            Vec::new()
        } else {
            row_text.split(',').map(|t| t.trim().to_string()).collect()
        };
        rows.push(cells);
    }
    if let Some(first) = rows.first() {
        let cols = first.len();
        for r in &rows {
            if r.len() != cols {
                return Err("ragged 2D array (rows have different column counts)");
            }
        }
    }
    Ok(rows)
}

pub(crate) fn parse_int_2d_literal(s: &str) -> Result<Vec<Vec<Option<i32>>>, &'static str> {
    let raw = split_2d_literal(s)?;
    raw.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| {
                    if cell.eq_ignore_ascii_case("NULL") {
                        Ok(None)
                    } else {
                        cell.parse::<i32>()
                            .map(Some)
                            .map_err(|_| "invalid int element")
                    }
                })
                .collect()
        })
        .collect()
}

pub(crate) fn parse_bigint_2d_literal(s: &str) -> Result<Vec<Vec<Option<i64>>>, &'static str> {
    let raw = split_2d_literal(s)?;
    raw.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| {
                    if cell.eq_ignore_ascii_case("NULL") {
                        Ok(None)
                    } else {
                        cell.parse::<i64>()
                            .map(Some)
                            .map_err(|_| "invalid bigint element")
                    }
                })
                .collect()
        })
        .collect()
}

pub(crate) fn parse_text_2d_literal(
    s: &str,
) -> Result<Vec<Vec<Option<alloc::string::String>>>, &'static str> {
    let raw = split_2d_literal(s)?;
    Ok(raw
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| {
                    if cell.eq_ignore_ascii_case("NULL") {
                        None
                    } else {
                        Some(cell.trim_matches('"').to_string())
                    }
                })
                .collect()
        })
        .collect())
}

pub(crate) fn format_int_2d_text(rows: &[Vec<Option<i32>>]) -> alloc::string::String {
    let mut out = alloc::string::String::from("{");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('{');
        for (j, cell) in row.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            match cell {
                None => out.push_str("NULL"),
                Some(n) => out.push_str(&alloc::format!("{n}")),
            }
        }
        out.push('}');
    }
    out.push('}');
    out
}

pub(crate) fn format_bigint_2d_text(rows: &[Vec<Option<i64>>]) -> alloc::string::String {
    let mut out = alloc::string::String::from("{");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('{');
        for (j, cell) in row.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            match cell {
                None => out.push_str("NULL"),
                Some(n) => out.push_str(&alloc::format!("{n}")),
            }
        }
        out.push('}');
    }
    out.push('}');
    out
}

pub(crate) fn format_text_2d_text(
    rows: &[Vec<Option<alloc::string::String>>],
) -> alloc::string::String {
    let mut out = alloc::string::String::from("{");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('{');
        for (j, cell) in row.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            match cell {
                None => out.push_str("NULL"),
                Some(s) => out.push_str(s),
            }
        }
        out.push('}');
    }
    out.push('}');
    out
}

/// v7.17.0 Phase 3.P0-40 — pub re-exports so pgwire + sqllogictest
/// share the single 2D-array renderer.
pub fn format_int_2d_text_pub(rows: &[Vec<Option<i32>>]) -> alloc::string::String {
    format_int_2d_text(rows)
}
pub fn format_bigint_2d_text_pub(rows: &[Vec<Option<i64>>]) -> alloc::string::String {
    format_bigint_2d_text(rows)
}
pub fn format_text_2d_text_pub(
    rows: &[Vec<Option<alloc::string::String>>],
) -> alloc::string::String {
    format_text_2d_text(rows)
}

/// v7.17.0 Phase 3.P0-38 — parse a PG range literal of the form
/// `'[lo,up)'` / `'(lo,up]'` / `'[lo,up]'` / `'(lo,up)'` /
/// `'empty'`. Lower / upper may be empty (unbounded). Returns
/// `None` on any parse failure; caller surfaces as hard error.
/// v7.38 (read01 U26) — PG range canonicalization, shared by the
/// `int4range(...)` constructors and the `'...'::int4range` text-input
/// path so both agree. PG forces an infinite (missing) bound to be
/// exclusive, then for DISCRETE element kinds (int4/int8/date) rewrites
/// to the `[)` form: an exclusive lower bumps to inclusive lower+1, an
/// inclusive upper bumps to exclusive upper+1 — so `[1,3]` becomes
/// `[1,4)`. Continuous kinds (num/ts/tstz) keep their bounds. Returns
/// the canonical `(lower, upper, lower_inc, upper_inc, empty)`, or
/// `None` if a discrete successor overflows the element type.
pub(crate) fn canonicalize_range_bounds(
    kind: spg_storage::RangeKind,
    lower: Option<Value<'static>>,
    upper: Option<Value<'static>>,
    lower_inc: bool,
    upper_inc: bool,
) -> Option<(Option<Value<'static>>, Option<Value<'static>>, bool, bool, bool)> {
    use spg_storage::RangeKind as K;
    // An infinite bound is always exclusive.
    let mut lower_inc = lower.is_some() && lower_inc;
    let mut upper_inc = upper.is_some() && upper_inc;
    let mut lower = lower;
    let mut upper = upper;
    if matches!(kind, K::Int4 | K::Int8 | K::Date) {
        fn succ(v: Value<'static>) -> Option<Value<'static>> {
            Some(match v {
                Value::Int(n) => Value::Int(n.checked_add(1)?),
                Value::BigInt(n) => Value::BigInt(n.checked_add(1)?),
                Value::Date(d) => Value::Date(d.checked_add(1)?),
                other => other,
            })
        }
        if let Some(l) = lower {
            lower = Some(if lower_inc { l } else { succ(l)? });
            lower_inc = true;
        }
        if let Some(u) = upper {
            upper = Some(if upper_inc { succ(u)? } else { u });
            upper_inc = false;
        }
    }
    // Equal bounds that don't include both ends collapse to 'empty'.
    let empty = match (&lower, &upper) {
        (Some(l), Some(u)) => l == u && !(lower_inc && upper_inc),
        _ => false,
    };
    Some((lower, upper, lower_inc, upper_inc, empty))
}

pub(crate) fn parse_range_str(s: &str, kind: spg_storage::RangeKind) -> Option<Value<'static>> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("empty") {
        return Some(Value::Range {
            kind,
            lower: None,
            upper: None,
            lower_inc: false,
            upper_inc: false,
            empty: true,
        });
    }
    let bytes = s.as_bytes();
    if bytes.len() < 3 {
        return None;
    }
    let lower_inc = match bytes[0] {
        b'[' => true,
        b'(' => false,
        _ => return None,
    };
    let upper_inc = match bytes[bytes.len() - 1] {
        b']' => true,
        b')' => false,
        _ => return None,
    };
    let inner = &s[1..s.len() - 1];
    let (lo_text, up_text) = inner.split_once(',')?;
    let lower = if lo_text.is_empty() {
        None
    } else {
        Some(parse_range_element(lo_text, kind)?)
    };
    let upper = if up_text.is_empty() {
        None
    } else {
        Some(parse_range_element(up_text, kind)?)
    };
    // Canonicalize (discrete `[)` fold + infinite→exclusive) so text
    // input agrees with the constructor functions.
    let (lower, upper, lower_inc, upper_inc, empty) =
        canonicalize_range_bounds(kind, lower, upper, lower_inc, upper_inc)?;
    Some(Value::Range {
        kind,
        lower: lower.map(alloc::boxed::Box::new),
        upper: upper.map(alloc::boxed::Box::new),
        lower_inc,
        upper_inc,
        empty,
    })
}

/// v7.37.5 δ — parse a PG multirange external form into a Vec of
/// `RangeSpan`. Grammar: `{}` empty, `{range1,range2,...}` with
/// each range in canonical `[/(/]/)` brackets. Empty subranges
/// (`empty`) are accepted but get dropped on round-trip per PG
/// semantics. The bounds parser reuses `parse_range_str` by
/// wrapping each subrange in the parent kind.
pub(crate) fn parse_multirange_str(
    s: &str,
    kind: spg_storage::RangeKind,
) -> Option<Vec<spg_storage::RangeSpan>> {
    let s = s.trim();
    let inner = s.strip_prefix('{').and_then(|x| x.strip_suffix('}'))?;
    let inner = inner.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    // Split the inner on commas that sit *between* ranges — not the
    // commas inside `[a,b)`. Walk depth: bump on `[` / `(`, drop on
    // `]` / `)`. Commas at depth 0 are range separators.
    let mut spans: Vec<spg_storage::RangeSpan> = Vec::new();
    let bytes = inner.as_bytes();
    let mut depth: i32 = 0;
    let mut start = 0usize;
    for i in 0..=bytes.len() {
        let cut = i == bytes.len() || (depth == 0 && bytes[i] == b',');
        if !cut {
            match bytes.get(i) {
                Some(b'[') | Some(b'(') => depth += 1,
                Some(b']') | Some(b')') => depth -= 1,
                _ => {}
            }
            continue;
        }
        let piece = inner[start..i].trim();
        if piece.is_empty() {
            return None;
        }
        let r = parse_range_str(piece, kind)?;
        let Value::Range {
            lower,
            upper,
            lower_inc,
            upper_inc,
            empty,
            ..
        } = r
        else {
            return None;
        };
        spans.push(spg_storage::RangeSpan {
            lower,
            upper,
            lower_inc,
            upper_inc,
            empty,
        });
        start = i + 1;
    }
    Some(spans)
}

/// v7.17.0 Phase 3.P0-38 — parse a single range bound text into
/// the matching element Value for the RangeKind.
pub(crate) fn parse_range_element(
    text: &str,
    kind: spg_storage::RangeKind,
) -> Option<Value<'static>> {
    let text = text.trim().trim_matches('"');
    use spg_storage::RangeKind as K;
    match kind {
        K::Int4 => text.parse::<i32>().ok().map(Value::Int),
        K::Int8 => text.parse::<i64>().ok().map(Value::BigInt),
        K::Num => {
            // Reuse the Numeric parse via the engine's text-coercion
            // path; bail to None on failure.
            let dot = text.find('.');
            let scale: u8 = dot.map_or(0, |p| (text.len() - p - 1) as u8);
            let digits: alloc::string::String = text
                .chars()
                .filter(|c| *c == '-' || c.is_ascii_digit())
                .collect();
            let scaled: i128 = digits.parse().ok()?;
            Some(Value::Numeric { scaled, scale, kind: spg_storage::NumericKind::Finite })
        }
        K::Ts | K::TsTz => {
            // Reuse the existing timestamp parse path. v7.17.0
            // expects `'YYYY-MM-DD HH:MM:SS[.ffffff]'` in range
            // bounds (TZ offset on TsTz is OOS for the initial
            // P0-38; ship plain Timestamp shape).
            crate::eval::parse_timestamp_literal(text).map(Value::Timestamp)
        }
        K::Date => crate::eval::parse_date_literal(text).map(Value::Date),
    }
}

/// v7.17.0 Phase 3.P0-38 — render a Range value as its canonical
/// PG text form. Re-exported via [`format_range_text`] for use
/// from spg-server's pgwire layer.
pub fn format_range_text(v: &Value) -> alloc::string::String {
    format_range_str(v)
}

pub(crate) fn format_range_str(v: &Value) -> alloc::string::String {
    let Value::Range {
        lower,
        upper,
        lower_inc,
        upper_inc,
        empty,
        ..
    } = v
    else {
        return alloc::string::String::new();
    };
    if *empty {
        return "empty".into();
    }
    let mut out = alloc::string::String::new();
    out.push(if *lower_inc { '[' } else { '(' });
    if let Some(l) = lower {
        out.push_str(&quote_range_bound(&format_range_element(l)));
    }
    out.push(',');
    if let Some(u) = upper {
        out.push_str(&quote_range_bound(&format_range_element(u)));
    }
    out.push(if *upper_inc { ']' } else { ')' });
    out
}

/// PG's `range_out` double-quotes a bound whose text is empty or
/// contains a range-syntax metacharacter (`"` `\` `(` `)` `[` `]` `,`)
/// or whitespace — so a timestamp bound `2020-01-01 10:00:00` prints
/// as `"2020-01-01 10:00:00"` inside the range. `"` and `\` are
/// backslash-escaped within the quotes. Numeric / date bounds (no
/// spaces) pass through unquoted, matching PG.
fn quote_range_bound(s: &str) -> alloc::string::String {
    let needs_quote = s.is_empty()
        || s.chars().any(|c| {
            matches!(c, '"' | '\\' | '(' | ')' | '[' | ']' | ',') || c.is_whitespace()
        });
    if !needs_quote {
        return s.into();
    }
    let mut out = alloc::string::String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// v7.37.5 ε — render a Point as PG canonical `(x,y)`.
pub fn format_point(p: spg_storage::Point2D) -> alloc::string::String {
    alloc::format!("({},{})", p.x, p.y)
}

/// v7.37.5 ε — render an Lseg as PG canonical `[(x1,y1),(x2,y2)]`.
pub fn format_lseg(p1: spg_storage::Point2D, p2: spg_storage::Point2D) -> alloc::string::String {
    alloc::format!("[({},{}),({},{})]", p1.x, p1.y, p2.x, p2.y)
}

/// v7.37.5 ε — render a Box as PG canonical `(ux,uy),(lx,ly)`.
/// PG normalises the corner order on input; we trust the engine's
/// constructor has already normalised so the field order here is
/// the canonical upper-right + lower-left.
pub fn format_pg_box(ur: spg_storage::Point2D, ll: spg_storage::Point2D) -> alloc::string::String {
    alloc::format!("({},{}),({},{})", ur.x, ur.y, ll.x, ll.y)
}

/// v7.37.5 ε — render a Line as PG canonical `{a,b,c}` (Ax+By+C=0).
pub fn format_line(a: f64, b: f64, c: f64) -> alloc::string::String {
    alloc::format!("{{{},{},{}}}", a, b, c)
}

/// v7.37.5 ε — render a Circle as PG canonical `<(x,y),r>`.
pub fn format_circle(center: spg_storage::Point2D, radius: f64) -> alloc::string::String {
    alloc::format!("<({},{}),{}>", center.x, center.y, radius)
}

/// v7.37.5 ε — render a Path as PG canonical `[(x,y),...]` open
/// or `((x,y),...)` closed.
pub fn format_path(points: &[spg_storage::Point2D], closed: bool) -> alloc::string::String {
    let (open, close) = if closed { ('(', ')') } else { ('[', ']') };
    let mut out = alloc::string::String::new();
    out.push(open);
    for (i, p) in points.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&alloc::format!("({},{})", p.x, p.y));
    }
    out.push(close);
    out
}

/// v7.37.5 ε — render a Polygon as PG canonical `((x,y),...)`.
pub fn format_polygon(points: &[spg_storage::Point2D]) -> alloc::string::String {
    let mut out = alloc::string::String::new();
    out.push('(');
    for (i, p) in points.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&alloc::format!("({},{})", p.x, p.y));
    }
    out.push(')');
    out
}

/// v7.37.5 ε — parse a single `(x,y)` or bare `x,y` Point text.
/// Surrounding whitespace OK. Returns `None` on malformed input.
fn parse_point(s: &str) -> Option<spg_storage::Point2D> {
    let s = s.trim();
    let inner = s
        .strip_prefix('(')
        .and_then(|x| x.strip_suffix(')'))
        .unwrap_or(s);
    let (xs, ys) = inner.split_once(',')?;
    let x: f64 = xs.trim().parse().ok()?;
    let y: f64 = ys.trim().parse().ok()?;
    Some(spg_storage::Point2D { x, y })
}

/// v7.37.5 ε — parse N points from a comma-separated PG point
/// list (`(x1,y1),(x2,y2),...`). Depth-aware split so the commas
/// inside each `(...)` aren't taken as separators. Returns `None`
/// on malformed input.
fn parse_point_list(s: &str) -> Option<Vec<spg_storage::Point2D>> {
    let bytes = s.as_bytes();
    let mut out: Vec<spg_storage::Point2D> = Vec::new();
    let mut depth: i32 = 0;
    let mut start = 0usize;
    for i in 0..=bytes.len() {
        let cut = i == bytes.len() || (depth == 0 && bytes[i] == b',');
        if !cut {
            match bytes.get(i) {
                Some(b'(') | Some(b'[') | Some(b'<') => depth += 1,
                Some(b')') | Some(b']') | Some(b'>') => depth -= 1,
                _ => {}
            }
            continue;
        }
        let piece = s[start..i].trim();
        if !piece.is_empty() {
            out.push(parse_point(piece)?);
        }
        start = i + 1;
    }
    Some(out)
}

/// v7.37.5 ε — parse Lseg text `[(x1,y1),(x2,y2)]`.
pub fn parse_lseg_text(s: &str) -> Option<(spg_storage::Point2D, spg_storage::Point2D)> {
    let s = s.trim();
    // PG accepts the bracketed `[(x1,y1),(x2,y2)]`, the fully-wrapped
    // `((x1,y1),(x2,y2))`, and the bare `(x1,y1),(x2,y2)` spellings.
    let inner = s
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .unwrap_or(s);
    let two_points =
        |v: Option<alloc::vec::Vec<spg_storage::Point2D>>| v.filter(|p| p.len() == 2);
    let pts = if let Some(p) = two_points(parse_point_list(inner)) {
        p
    } else if let Some(p) = inner
        .strip_prefix('(')
        .and_then(|x| x.strip_suffix(')'))
        .and_then(|w| two_points(parse_point_list(w)))
    {
        p
    } else {
        return None;
    };
    Some((pts[0], pts[1]))
}

/// v7.37.5 ε — parse Box text `(ux,uy),(lx,ly)`. PG normalises
/// any two-corner input into upper-right + lower-left; we do
/// the same.
pub fn parse_box_text(s: &str) -> Option<(spg_storage::Point2D, spg_storage::Point2D)> {
    // PG box input: `(x1,y1),(x2,y2)`, the fully-wrapped `((x1,y1),(x2,y2))`,
    // or the bare `x1,y1,x2,y2` (four raw numbers). Try the point-list form,
    // then the same list inside one stripped `(...)` layer, then four floats.
    let s = s.trim();
    let two_points = |v: Option<alloc::vec::Vec<spg_storage::Point2D>>| v.filter(|p| p.len() == 2);
    let pts = if let Some(p) = two_points(parse_point_list(s)) {
        p
    } else if let Some(p) = s
        .strip_prefix('(')
        .and_then(|x| x.strip_suffix(')'))
        .and_then(|inner| two_points(parse_point_list(inner)))
    {
        p
    } else {
        let nums: Option<alloc::vec::Vec<f64>> =
            s.split(',').map(|t| t.trim().parse::<f64>().ok()).collect();
        let nums = nums?;
        if nums.len() != 4 {
            return None;
        }
        alloc::vec![
            spg_storage::Point2D { x: nums[0], y: nums[1] },
            spg_storage::Point2D { x: nums[2], y: nums[3] },
        ]
    };
    if pts.len() != 2 {
        return None;
    }
    let (a, b) = (pts[0], pts[1]);
    // Normalise: upper-right has the larger x AND larger y.
    let ur = spg_storage::Point2D {
        x: a.x.max(b.x),
        y: a.y.max(b.y),
    };
    let ll = spg_storage::Point2D {
        x: a.x.min(b.x),
        y: a.y.min(b.y),
    };
    Some((ur, ll))
}

/// v7.37.5 ε — parse Line text `{a,b,c}`.
pub fn parse_line_text(s: &str) -> Option<(f64, f64, f64)> {
    let s = s.trim();
    let inner = s.strip_prefix('{').and_then(|x| x.strip_suffix('}'))?;
    let parts: Vec<&str> = inner.split(',').collect();
    if parts.len() != 3 {
        return None;
    }
    let a: f64 = parts[0].trim().parse().ok()?;
    let b: f64 = parts[1].trim().parse().ok()?;
    let c: f64 = parts[2].trim().parse().ok()?;
    Some((a, b, c))
}

/// v7.37.5 ε — parse Circle text `<(x,y),r>` or `((x,y),r)`.
pub fn parse_circle_text(s: &str) -> Option<(spg_storage::Point2D, f64)> {
    let s = s.trim();
    // PG circle input: `<(x,y),r>`, `((x,y),r)`, `(x,y),r`, or bare `x,y,r`.
    let inner = if let Some(i) = s.strip_prefix('<').and_then(|x| x.strip_suffix('>')) {
        i
    } else if let Some(i) = s.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
        i
    } else {
        s
    };
    // The last comma at depth 0 splits the center from the radius.
    let bytes = inner.as_bytes();
    let mut depth = 0i32;
    let mut split_at: Option<usize> = None;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' | b'[' | b'<' => depth += 1,
            b')' | b']' | b'>' => depth -= 1,
            b',' if depth == 0 => split_at = Some(i),
            _ => {}
        }
    }
    let i = split_at?;
    let center = parse_point(&inner[..i])?;
    let radius: f64 = inner[i + 1..].trim().parse().ok()?;
    Some((center, radius))
}

/// v7.37.5 ε — parse Path text `[(x,y),...]` (open) or
/// `((x,y),...)` (closed). The leading bracket pins openness.
pub fn parse_path_text(s: &str) -> Option<(Vec<spg_storage::Point2D>, bool)> {
    let s = s.trim();
    // `[...]` = open path, `(...)` = closed. A bare point list (no brackets)
    // is a closed path in PG. Strip a wrapping layer only when it yields a
    // valid point list; otherwise parse the bare list directly as closed
    // (stripping unconditionally would mangle `(0,0),(1,1)` into `0,0),(1,1`).
    if let Some(i) = s.strip_prefix('[').and_then(|x| x.strip_suffix(']')) {
        if let Some(pts) = parse_point_list(i) {
            return Some((pts, false));
        }
    }
    if let Some(i) = s.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
        if let Some(pts) = parse_point_list(i) {
            return Some((pts, true));
        }
    }
    parse_point_list(s).map(|pts| (pts, true))
}

/// v7.37.5 ε — parse Polygon text `((x,y),...)` (implicit closed).
pub fn parse_polygon_text(s: &str) -> Option<Vec<spg_storage::Point2D>> {
    let s = s.trim();
    // The outer parens are optional in PG — `((0,0),(1,1))` and `(0,0),(1,1)`
    // both parse. Try stripping one wrapping layer first (the `((...))` form);
    // if that doesn't yield a valid point list, parse the bare list directly.
    if let Some(inner) = s.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
        if let Some(pts) = parse_point_list(inner) {
            return Some(pts);
        }
    }
    parse_point_list(s)
}

/// v7.37.5 ζ-A — render an INET/CIDR address as canonical PG text:
/// IPv4: `a.b.c.d/bits`; IPv6: `xxxx:xxxx:.../bits`. The mask is
/// elided when it equals the family default (32 for IPv4, 128 for
/// IPv6), per PG convention.
/// v7.38 (read01) — inet text with the mask ALWAYS shown (`192.168.1.0/32`),
/// as PG's `inet::text` / `::varchar` cast renders it (the default display and
/// concat omit `/32` and `/128`; this is the cast-path form).
pub fn format_inet_full(family: u8, bits: u8, addr: &[u8; 16]) -> alloc::string::String {
    let max = if family == 4 { 32 } else { 128 };
    let base = format_inet(family, max, addr);
    alloc::format!("{base}/{bits}")
}

pub fn format_inet(family: u8, bits: u8, addr: &[u8; 16]) -> alloc::string::String {
    match family {
        4 => {
            let s = alloc::format!("{}.{}.{}.{}", addr[0], addr[1], addr[2], addr[3]);
            if bits == 32 {
                s
            } else {
                alloc::format!("{s}/{bits}")
            }
        }
        6 => {
            // v7.38 (read01) — RFC 5952 canonical form: compress the longest
            // run of consecutive all-zero groups (leftmost among ties) to `::`,
            // but only when that run is ≥ 2 groups. PG always renders this form.
            let mut groups = [0u16; 8];
            for (i, g) in groups.iter_mut().enumerate() {
                *g = (u16::from(addr[i * 2]) << 8) | u16::from(addr[i * 2 + 1]);
            }
            // v7.38 (read01, T19) — IPv4-mapped IPv6 (`::ffff:0:0/96` range:
            // first five groups zero, sixth 0xffff) renders with a dotted-quad
            // tail, matching PG (independent of the input spelling).
            if groups[..5].iter().all(|&g| g == 0) && groups[5] == 0xffff {
                let s =
                    alloc::format!("::ffff:{}.{}.{}.{}", addr[12], addr[13], addr[14], addr[15]);
                return if bits == 128 {
                    s
                } else {
                    alloc::format!("{s}/{bits}")
                };
            }
            let (mut best_start, mut best_len) = (usize::MAX, 0usize);
            let mut i = 0;
            while i < 8 {
                if groups[i] == 0 {
                    let start = i;
                    while i < 8 && groups[i] == 0 {
                        i += 1;
                    }
                    if i - start > best_len {
                        best_start = start;
                        best_len = i - start;
                    }
                } else {
                    i += 1;
                }
            }
            let mut out = alloc::string::String::new();
            if best_len >= 2 {
                for (idx, g) in groups.iter().enumerate().take(best_start) {
                    if idx > 0 {
                        out.push(':');
                    }
                    out.push_str(&alloc::format!("{g:x}"));
                }
                out.push_str("::");
                for (idx, g) in groups.iter().enumerate().skip(best_start + best_len) {
                    if idx > best_start + best_len {
                        out.push(':');
                    }
                    out.push_str(&alloc::format!("{g:x}"));
                }
            } else {
                for (idx, g) in groups.iter().enumerate() {
                    if idx > 0 {
                        out.push(':');
                    }
                    out.push_str(&alloc::format!("{g:x}"));
                }
            }
            if bits == 128 {
                out
            } else {
                alloc::format!("{out}/{bits}")
            }
        }
        _ => alloc::format!("?invalid-inet-family-{family}"),
    }
}

/// v7.37.5 ζ-A — render a MACADDR (6 bytes) as `aa:bb:cc:dd:ee:ff`.
pub fn format_macaddr(m: &[u8; 6]) -> alloc::string::String {
    alloc::format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0],
        m[1],
        m[2],
        m[3],
        m[4],
        m[5]
    )
}

/// v7.37.5 ζ-A — render a MACADDR8 (8 bytes) as `aa:bb:cc:dd:ee:ff:00:11`.
pub fn format_macaddr8(m: &[u8; 8]) -> alloc::string::String {
    alloc::format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0],
        m[1],
        m[2],
        m[3],
        m[4],
        m[5],
        m[6],
        m[7]
    )
}

/// v7.37.5 ζ-A — render a BIT / BIT VARYING as a binary string of
/// `'0'` and `'1'` chars (PG canonical text form). Bytes are packed
/// big-endian within each byte: the most-significant bit of byte 0
/// is bit 0 of the bit string.
pub fn format_bit_string(nbits: u32, bytes: &[u8]) -> alloc::string::String {
    let mut out = alloc::string::String::with_capacity(nbits as usize);
    for i in 0..nbits as usize {
        let byte = bytes[i / 8];
        let bit = (byte >> (7 - (i % 8))) & 1;
        out.push(if bit == 1 { '1' } else { '0' });
    }
    out
}

/// MSB-first integer value of a bit string (PG `bit`/`varbit` → integer cast).
pub fn bit_string_to_i64(nbits: u32, bytes: &[u8]) -> i64 {
    let mut val: i64 = 0;
    for i in 0..nbits as usize {
        let byte = bytes.get(i / 8).copied().unwrap_or(0);
        val = (val << 1) | i64::from((byte >> (7 - (i % 8))) & 1);
    }
    val
}

/// v7.37.5 ζ-A — render a MONEY[] in PG external form. Each element
/// is the canonical `format_money` output; the array wrapper is
/// `{...}` with NULL elements as the literal token `NULL`.
pub fn format_money_array(items: &[Option<i64>]) -> alloc::string::String {
    let mut out = alloc::string::String::new();
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            Some(c) => out.push_str(&crate::eval::format_money(*c)),
        }
    }
    out.push('}');
    out
}

/// v7.37.5 ζ-A — parse PG INET text. Accepts `a.b.c.d[/bits]`
/// (IPv4) or `xxxx:xxxx:.../[bits]` (IPv6 colon-separated). The
/// mask defaults to 32 (IPv4) / 128 (IPv6) when omitted. Returns
/// `(family, bits, addr16)`. `None` on malformed input.
pub fn parse_inet_text(s: &str) -> Option<(u8, u8, [u8; 16])> {
    let s = s.trim();
    let (addr_s, bits_s) = match s.split_once('/') {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    };
    if addr_s.contains(':') {
        // IPv6 — colon-separated up to 8 × u16 hex with optional
        // `::` zero-compression. v7.37.5 ship triage broadened the
        // pre-7.37.10 8-group-only form to accept canonical PG
        // IPv6 abbreviations like `2001:db8::/32`.
        let (head, tail) = match addr_s.find("::") {
            Some(idx) => (&addr_s[..idx], Some(&addr_s[idx + 2..])),
            None => (addr_s, None),
        };
        let mut head_groups: alloc::vec::Vec<&str> = if head.is_empty() {
            alloc::vec::Vec::new()
        } else {
            head.split(':').collect()
        };
        let mut tail_groups: alloc::vec::Vec<&str> = match tail {
            Some(t) if !t.is_empty() => t.split(':').collect(),
            _ => alloc::vec::Vec::new(),
        };
        // v7.38 (read01, T19) — a trailing dotted-quad (IPv4-in-IPv6, e.g.
        // `::ffff:192.168.1.1`, `64:ff9b::192.0.2.1`) fills the last two 16-bit
        // words. It is always the final group overall.
        let mut dotted_words: Option<[u16; 2]> = None;
        if let Some(g) = tail_groups.last().or_else(|| head_groups.last()) {
            if g.contains('.') {
                let oct: alloc::vec::Vec<&str> = g.split('.').collect();
                if oct.len() != 4 {
                    return None;
                }
                let mut b = [0u8; 4];
                for (i, o) in oct.iter().enumerate() {
                    b[i] = o.parse::<u8>().ok()?;
                }
                dotted_words = Some([
                    (u16::from(b[0]) << 8) | u16::from(b[1]),
                    (u16::from(b[2]) << 8) | u16::from(b[3]),
                ]);
                if !tail_groups.is_empty() {
                    tail_groups.pop();
                } else {
                    head_groups.pop();
                }
            }
        }
        let dq = if dotted_words.is_some() { 2 } else { 0 };
        let head_len = head_groups.len();
        let tail_len = tail_groups.len();
        if tail.is_none() {
            if head_len + dq != 8 {
                return None;
            }
        } else if head_len + tail_len + dq > 7 {
            return None;
        }
        let mut words = [0u16; 8];
        for (i, g) in head_groups.iter().enumerate() {
            words[i] = u16::from_str_radix(g, 16).ok()?;
        }
        // The dotted-quad (if any) occupies the final two words; hex tail groups
        // sit just before it.
        let trailing_start = 8 - dq - tail_len;
        for (i, g) in tail_groups.iter().enumerate() {
            words[trailing_start + i] = u16::from_str_radix(g, 16).ok()?;
        }
        if let Some(dw) = dotted_words {
            words[6] = dw[0];
            words[7] = dw[1];
        }
        let mut addr = [0u8; 16];
        for (i, w) in words.iter().enumerate() {
            addr[i * 2] = (w >> 8) as u8;
            addr[i * 2 + 1] = (w & 0xff) as u8;
        }
        let bits = match bits_s {
            Some(b) => b.parse::<u8>().ok().filter(|&n| n <= 128)?,
            None => 128,
        };
        Some((6, bits, addr))
    } else {
        // IPv4 — `a.b.c.d`.
        let parts: alloc::vec::Vec<&str> = addr_s.split('.').collect();
        if parts.len() != 4 {
            return None;
        }
        let mut addr = [0u8; 16];
        for (i, p) in parts.iter().enumerate() {
            addr[i] = p.parse::<u8>().ok()?;
        }
        let bits = match bits_s {
            Some(b) => b.parse::<u8>().ok().filter(|&n| n <= 32)?,
            None => 32,
        };
        Some((4, bits, addr))
    }
}

/// v7.37.5 ζ-A — parse PG MACADDR text `aa:bb:cc:dd:ee:ff` (also
/// accepts `aa-bb-cc-dd-ee-ff` and unseparated `aabbccddeeff`).
pub fn parse_macaddr_text(s: &str) -> Option<[u8; 6]> {
    let s = s.trim();
    let cleaned: alloc::string::String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if cleaned.len() != 12 {
        return None;
    }
    let mut out = [0u8; 6];
    for i in 0..6 {
        out[i] = u8::from_str_radix(&cleaned[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// v7.37.5 ζ-A — parse PG MACADDR8 text.
pub fn parse_macaddr8_text(s: &str) -> Option<[u8; 8]> {
    let s = s.trim();
    let cleaned: alloc::string::String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if cleaned.len() != 16 {
        return None;
    }
    let mut out = [0u8; 8];
    for i in 0..8 {
        out[i] = u8::from_str_radix(&cleaned[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// v7.37.5 ζ-A — parse PG bit string text (a sequence of `'0'` and
/// `'1'` chars). Returns `(nbits, packed_bytes)` — bytes are
/// big-endian within each byte (PG canonical).
pub fn parse_bit_string_text(s: &str) -> Option<(u32, alloc::vec::Vec<u8>)> {
    let s = s.trim();
    let nbits = u32::try_from(s.len()).ok()?;
    let nbytes = (s.len()).div_ceil(8);
    let mut bytes = alloc::vec![0u8; nbytes];
    for (i, c) in s.chars().enumerate() {
        let bit = match c {
            '0' => 0u8,
            '1' => 1u8,
            _ => return None,
        };
        if bit == 1 {
            bytes[i / 8] |= 1 << (7 - (i % 8));
        }
    }
    Some((nbits, bytes))
}

/// v7.37.5 δ — render a Multirange in PG external form
/// `{[a,b),[c,d)}`. Empty multirange renders as `{}`. Each range
/// element is formatted with the same `[/(/]/)` bracket grammar
/// as scalar `Value::Range`. RangeSpan carries no `kind` (it
/// lives on the parent Multirange), so this routes element
/// formatting through `format_range_element` as Value::Range does.
pub fn format_multirange(ranges: &[spg_storage::RangeSpan]) -> alloc::string::String {
    let mut out = alloc::string::String::new();
    out.push('{');
    for (i, r) in ranges.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        if r.empty {
            out.push_str("empty");
            continue;
        }
        out.push(if r.lower_inc { '[' } else { '(' });
        if let Some(l) = &r.lower {
            out.push_str(&quote_range_bound(&format_range_element(l)));
        }
        out.push(',');
        if let Some(u) = &r.upper {
            out.push_str(&quote_range_bound(&format_range_element(u)));
        }
        out.push(if r.upper_inc { ']' } else { ')' });
    }
    out.push('}');
    out
}

pub(crate) fn format_range_element(v: &Value) -> alloc::string::String {
    match v {
        Value::Int(n) => alloc::format!("{n}"),
        Value::BigInt(n) => alloc::format!("{n}"),
        Value::Date(d) => crate::eval::format_date(*d),
        Value::Timestamp(t) => crate::eval::format_timestamp(*t),
        Value::Numeric { scaled, scale, kind } => crate::eval::format_numeric_kind(*kind, *scaled, *scale),
        other => alloc::format!("{other:?}"),
    }
}

/// v7.17.0 Phase 3.P0-35 — parse a PG `money` literal into i64
/// cents. Accepts:
///   * Optional leading `-` (negative)
///   * Optional `$` prefix
///   * Integer portion with optional `,` thousands separators
///   * Optional `.` followed by 1-2 digits (cents); 1 digit
///     auto-pads to 2 (`.5` → 50 cents).
///
/// Returns None on any parse failure — caller surfaces as hard
/// SQL error.
pub(crate) fn parse_money_str(s: &str) -> Option<i64> {
    let s = s.trim();
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r.trim_start()),
        None => (false, s),
    };
    let rest = rest.strip_prefix('$').unwrap_or(rest).trim_start();
    let (int_part, frac_part) = match rest.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (rest, None),
    };
    if int_part.is_empty() {
        return None;
    }
    // Validate + strip commas from the integer portion.
    let mut int_digits = alloc::string::String::with_capacity(int_part.len());
    for b in int_part.bytes() {
        match b {
            b',' => {}
            b'0'..=b'9' => int_digits.push(b as char),
            _ => return None,
        }
    }
    if int_digits.is_empty() {
        return None;
    }
    let dollars: i64 = int_digits.parse().ok()?;
    let cents: i64 = match frac_part {
        None => 0,
        Some(f) => {
            if f.is_empty() || f.len() > 2 || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let padded = if f.len() == 1 {
                alloc::format!("{f}0")
            } else {
                f.to_string()
            };
            padded.parse().ok()?
        }
    };
    let total = dollars.checked_mul(100)?.checked_add(cents)?;
    Some(if neg { -total } else { total })
}

/// v7.17.0 Phase 3.P0-34 — parse a PG `timetz` literal
/// `HH:MM:SS[.fraction]±HH[:MM]` into (us, offset_secs).
///
/// The offset suffix is MANDATORY: SPG doesn't have a session TZ
/// wired into eval, so a bare `HH:MM:SS` literal would be
/// ambiguous. Returns None for any parse failure or out-of-range
/// component — caller surfaces as a hard SQL error.
///
/// Offset range: ±14 hours (±50400 seconds), matching PG's
/// internal limit.
pub(crate) fn parse_timetz_str(s: &str) -> Option<(i64, i32)> {
    let s = s.trim();
    // Find the offset sign — scan from right since the time part
    // never contains '+' / '-' (after the optional fractional dot
    // it's all digits and ':').
    let bytes = s.as_bytes();
    let sign_pos = bytes
        .iter()
        .enumerate()
        .rev()
        .find(|&(_, &b)| b == b'+' || b == b'-')
        .map(|(i, _)| i)?;
    if sign_pos == 0 {
        return None; // bare sign — no time component
    }
    let time_part = &s[..sign_pos];
    let offset_part = &s[sign_pos..];
    let us = parse_time_str(time_part)?;
    let sign: i32 = if offset_part.starts_with('+') { 1 } else { -1 };
    let offset_body = &offset_part[1..];
    let (hh_str, mm_str) = match offset_body.split_once(':') {
        Some((h, m)) => (h, m),
        None => (offset_body, "0"),
    };
    let hh: i32 = hh_str.parse().ok()?;
    let mm: i32 = mm_str.parse().ok()?;
    if !(0..=14).contains(&hh) || !(0..=59).contains(&mm) {
        return None;
    }
    let total = sign * (hh * 3600 + mm * 60);
    if total.abs() > 50_400 {
        return None;
    }
    Some((us, total))
}

/// v7.17.0 Phase 3.P0-33 — funnel an integer literal through MySQL
/// YEAR range validation: 0 sentinel or 1901..=2155. Out-of-range
/// surfaces as a hard SQL error (no silent truncation, mirrors PG
/// `time_in` / `uuid_in` discipline).
pub(crate) fn coerce_int_to_year(n: i64, col_name: &str) -> Result<Value<'static>, EngineError> {
    if n == 0 || (1901..=2155).contains(&n) {
        // u16::try_from cannot fail in this range; the cast also
        // covers the 0 sentinel.
        return Ok(Value::Year(n as u16));
    }
    Err(EngineError::Eval(EvalError::TypeMismatch {
        detail: alloc::format!(
            "year value out of range: {n} (column `{col_name}`; \
             MySQL accepts 0 or 1901..=2155)"
        ),
    }))
}

/// v7.17.0 Phase 3.P0-32 — parse a PG `time` literal
/// `HH:MM:SS[.fraction]` into microseconds since 00:00:00.
///
/// Accepts:
///   * `HH:MM:SS`            — exact-second precision
///   * `HH:MM:SS.f` .. `.ffffff` — 1-6 fractional digits, right-padded
///     with zeros to microseconds
///
/// Range: hour 0..=23, minute 0..=59, second 0..=59. Anything else
/// returns None — caller surfaces as a hard SQL error (no silent
/// truncation, matches PG's `time_in` behaviour).
pub(crate) fn parse_time_str(s: &str) -> Option<i64> {
    let s = s.trim();
    // PG special TIME value: `allballs` is midnight (all zeros).
    if s.eq_ignore_ascii_case("allballs") {
        return Some(0);
    }
    let (hms, frac) = match s.split_once('.') {
        Some((h, f)) => (h, Some(f)),
        None => (s, None),
    };
    let mut parts = hms.split(':');
    let hh: u32 = parts.next()?.parse().ok()?;
    let mm: u32 = parts.next()?.parse().ok()?;
    // PG accepts the seconds-optional `HH:MM` form for TIME
    // (`'10:30'::time` → `10:30:00`); missing seconds default to 0.
    let ss: u32 = match parts.next() {
        Some(x) => x.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    // PG accepts the end-of-day sentinel `24:00:00` (but nothing past it).
    if hh > 24 || mm > 59 || ss > 59 || (hh == 24 && (mm != 0 || ss != 0)) {
        return None;
    }
    let frac_us: i64 = match frac {
        None => 0,
        Some(f) => {
            if f.is_empty() || f.len() > 6 || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            // Right-pad with zeros so '.5' = 500000 µsec.
            let mut padded = alloc::string::String::with_capacity(6);
            padded.push_str(f);
            while padded.len() < 6 {
                padded.push('0');
            }
            padded.parse().ok()?
        }
    };
    if hh == 24 && frac_us != 0 {
        return None;
    }
    Some(
        i64::from(hh) * 3_600_000_000
            + i64::from(mm) * 60_000_000
            + i64::from(ss) * 1_000_000
            + frac_us,
    )
}

/// v7.37.5 ship triage — string-form PG type name → `DataType`
/// lookup driving `CastTarget::Named` (the generic typed-cast
/// escape). Covers the v7.37.5 γ/δ/ε/ζ-A type-completeness work
/// that landed without per-type CastTarget variants. Returns
/// `None` for genuinely-unknown idents so the caller can surface
/// the existing "unsupported cast target" error.
pub(crate) fn type_name_to_data_type(name: &str) -> Option<DataType> {
    let n = name.trim().to_ascii_lowercase();
    // v7.37.5 ship triage — `numeric(p,s)` precision/scale params:
    // peel them off and route to a precision-bearing DataType.
    if let Some((head, paren)) = n.split_once('(')
        && let Some(args) = paren.strip_suffix(')')
    {
        let nums: alloc::vec::Vec<u8> = args
            .split(',')
            .map(|s| s.trim().parse::<u8>().unwrap_or(0))
            .collect();
        match head {
            "numeric" | "decimal" => {
                let precision = nums.first().copied().unwrap_or(0);
                let scale = nums.get(1).copied().unwrap_or(0);
                return Some(DataType::Numeric { precision, scale });
            }
            // `varchar(n)` / `char(n)` carry length caps; SPG stores
            // these as DataType::Varchar / Char(n). v7.37.5 cast
            // recognises both but the cast itself drops the cap
            // (Text widening at value time honours the per-row
            // length contract already in coerce_value).
            "varchar" => {
                return Some(DataType::Varchar(nums.first().copied().unwrap_or(0).into()));
            }
            "char" | "character" => {
                return Some(DataType::Char(nums.first().copied().unwrap_or(0).into()));
            }
            // bit / varbit precision drops on cast — storage shape
            // is the BitString regardless.
            "bit" => return Some(DataType::Bit),
            "varbit" => return Some(DataType::BitVarying),
            _ => {}
        }
    }
    Some(match n.as_str() {
        "smallint" | "int2" => DataType::SmallInt,
        "numeric" | "decimal" => DataType::Numeric {
            precision: 0,
            scale: 0,
        },
        // Network/MAC/bit/XML/"char" — all first-class since
        // v7.37.5 ζ-A.
        "inet" => DataType::Inet,
        "cidr" => DataType::Cidr,
        "macaddr" => DataType::Macaddr,
        "macaddr8" => DataType::Macaddr8,
        "bit" => DataType::Bit,
        "varbit" | "bit varying" => DataType::BitVarying,
        "xml" => DataType::Xml,
        "money" => DataType::Money,
        "char1" => DataType::Char1,
        // Geometry (v7.37.5 ε).
        "point" => DataType::Point,
        "lseg" => DataType::Lseg,
        "path" => DataType::Path,
        "box" => DataType::PgBox,
        "polygon" => DataType::Polygon,
        "line" => DataType::Line,
        "circle" => DataType::Circle,
        // Multirange (v7.37.5 δ).
        "int4multirange" => DataType::Multirange(spg_storage::RangeKind::Int4),
        "int8multirange" => DataType::Multirange(spg_storage::RangeKind::Int8),
        "nummultirange" => DataType::Multirange(spg_storage::RangeKind::Num),
        "tsmultirange" => DataType::Multirange(spg_storage::RangeKind::Ts),
        "tstzmultirange" => DataType::Multirange(spg_storage::RangeKind::TsTz),
        "datemultirange" => DataType::Multirange(spg_storage::RangeKind::Date),
        // Range scalars(scaffolded in v7.17, casts join here).
        "int4range" => DataType::Range(spg_storage::RangeKind::Int4),
        "int8range" => DataType::Range(spg_storage::RangeKind::Int8),
        "numrange" => DataType::Range(spg_storage::RangeKind::Num),
        "tsrange" => DataType::Range(spg_storage::RangeKind::Ts),
        "tstzrange" => DataType::Range(spg_storage::RangeKind::TsTz),
        "daterange" => DataType::Range(spg_storage::RangeKind::Date),
        // Array forms — `::BOOL[]` etc. The parser canonicalises
        // postfix `[]` into the `_array` suffix; mirror PG's
        // builtin arrays so the cast lands on a typed array Value.
        "bool_array" | "boolean_array" => DataType::BoolArray,
        "smallint_array" | "int2_array" => DataType::SmallIntArray,
        "int_array" | "integer_array" | "int4_array" => DataType::IntArray,
        "bigint_array" | "int8_array" => DataType::BigIntArray,
        "float_array" | "double_array" | "real_array" | "float8_array" | "float4_array" => {
            DataType::FloatArray
        }
        // Width-suffixed float spellings and OID — SPG has one
        // float representation and OIDs are plain integers.
        "float4" | "real" => DataType::Real,
        "float8" | "double precision" | "float" => DataType::Float,
        "oid" => DataType::BigInt,
        // TIME [WITHOUT TIME ZONE] — first-class since the codec
        // carries Value::Time; the coerce path parses HH:MM:SS.
        "time" | "time without time zone" => DataType::Time,
        "timetz" | "time with time zone" => DataType::TimeTz,
        "numeric_array" | "decimal_array" => DataType::NumericArray,
        "varchar_array" | "character varying_array" | "char_array" | "bpchar_array" => {
            DataType::TextArray
        }
        "text_array" => DataType::TextArray,
        "date_array" => DataType::DateArray,
        "timestamp_array" => DataType::TimestampArray,
        "timestamptz_array" => DataType::TimestamptzArray,
        "uuid_array" => DataType::UuidArray,
        "json_array" => DataType::JsonArray,
        "jsonb_array" => DataType::JsonbArray,
        "bytea_array" => DataType::BytesArray,
        "interval_array" => DataType::IntervalArray,
        "money_array" => DataType::MoneyArray,
        // v7.38 (read01) — primitive scalar spellings. These reach here only
        // via CastTarget::Named (e.g. the function-style typecast `int4('5')` /
        // `text(42)` / `date('2024-01-15')`); the `expr::type` parser path maps
        // them to dedicated CastTarget variants and never touches this table.
        "int" | "int4" | "integer" => DataType::Int,
        "bigint" | "int8" => DataType::BigInt,
        "text" => DataType::Text,
        "varchar" | "character varying" => DataType::Varchar(0),
        "bpchar" | "character" => DataType::Char(0),
        "bool" | "boolean" => DataType::Bool,
        "date" => DataType::Date,
        "timestamp" | "timestamp without time zone" => DataType::Timestamp,
        "timestamptz" | "timestamp with time zone" => DataType::Timestamptz,
        "uuid" => DataType::Uuid,
        "json" => DataType::Json,
        "jsonb" => DataType::Jsonb,
        "bytea" => DataType::Bytes,
        "interval" => DataType::Interval,
        _ => return None,
    })
}

pub(crate) const fn column_type_to_data_type(t: ColumnTypeName) -> DataType {
    match t {
        ColumnTypeName::SmallInt => DataType::SmallInt,
        ColumnTypeName::Int => DataType::Int,
        ColumnTypeName::BigInt => DataType::BigInt,
        ColumnTypeName::Float => DataType::Float,
        ColumnTypeName::Text => DataType::Text,
        ColumnTypeName::Varchar(n) => DataType::Varchar(n),
        ColumnTypeName::Char(n) => DataType::Char(n),
        ColumnTypeName::Bool => DataType::Bool,
        ColumnTypeName::Vector { dim, encoding } => DataType::Vector {
            dim,
            encoding: match encoding {
                SqlVecEncoding::F32 => VecEncoding::F32,
                SqlVecEncoding::Sq8 => VecEncoding::Sq8,
                SqlVecEncoding::F16 => VecEncoding::F16,
            },
        },
        ColumnTypeName::Numeric(precision, scale) => DataType::Numeric { precision, scale },
        ColumnTypeName::Date => DataType::Date,
        ColumnTypeName::Timestamp => DataType::Timestamp,
        ColumnTypeName::Timestamptz => DataType::Timestamptz,
        ColumnTypeName::Json => DataType::Json,
        ColumnTypeName::Jsonb => DataType::Jsonb,
        ColumnTypeName::Bytes => DataType::Bytes,
        ColumnTypeName::TextArray => DataType::TextArray,
        ColumnTypeName::IntArray => DataType::IntArray,
        ColumnTypeName::BigIntArray => DataType::BigIntArray,
        ColumnTypeName::TsVector => DataType::TsVector,
        ColumnTypeName::TsQuery => DataType::TsQuery,
        ColumnTypeName::Uuid => DataType::Uuid,
        ColumnTypeName::Time => DataType::Time,
        ColumnTypeName::Year => DataType::Year,
        ColumnTypeName::TimeTz => DataType::TimeTz,
        ColumnTypeName::Money => DataType::Money,
        ColumnTypeName::Range(k) => DataType::Range(match k {
            spg_sql::ast::RangeKindAst::Int4 => spg_storage::RangeKind::Int4,
            spg_sql::ast::RangeKindAst::Int8 => spg_storage::RangeKind::Int8,
            spg_sql::ast::RangeKindAst::Num => spg_storage::RangeKind::Num,
            spg_sql::ast::RangeKindAst::Ts => spg_storage::RangeKind::Ts,
            spg_sql::ast::RangeKindAst::TsTz => spg_storage::RangeKind::TsTz,
            spg_sql::ast::RangeKindAst::Date => spg_storage::RangeKind::Date,
        }),
        ColumnTypeName::Hstore => DataType::Hstore,
        ColumnTypeName::IntArray2D => DataType::IntArray2D,
        ColumnTypeName::BigIntArray2D => DataType::BigIntArray2D,
        ColumnTypeName::TextArray2D => DataType::TextArray2D,
        ColumnTypeName::Interval => DataType::Interval,
        ColumnTypeName::IntervalArray => DataType::IntervalArray,
        ColumnTypeName::BoolArray => DataType::BoolArray,
        ColumnTypeName::SmallIntArray => DataType::SmallIntArray,
        ColumnTypeName::FloatArray => DataType::FloatArray,
        ColumnTypeName::NumericArray => DataType::NumericArray,
        ColumnTypeName::DateArray => DataType::DateArray,
        ColumnTypeName::TimestampArray => DataType::TimestampArray,
        ColumnTypeName::TimestamptzArray => DataType::TimestamptzArray,
        ColumnTypeName::UuidArray => DataType::UuidArray,
        ColumnTypeName::JsonArray => DataType::JsonArray,
        ColumnTypeName::JsonbArray => DataType::JsonbArray,
        ColumnTypeName::BytesArray => DataType::BytesArray,
        ColumnTypeName::VarcharArray => DataType::VarcharArray,
        ColumnTypeName::CharArray => DataType::CharArray,
        ColumnTypeName::Multirange(k) => DataType::Multirange(match k {
            spg_sql::ast::RangeKindAst::Int4 => spg_storage::RangeKind::Int4,
            spg_sql::ast::RangeKindAst::Int8 => spg_storage::RangeKind::Int8,
            spg_sql::ast::RangeKindAst::Num => spg_storage::RangeKind::Num,
            spg_sql::ast::RangeKindAst::Ts => spg_storage::RangeKind::Ts,
            spg_sql::ast::RangeKindAst::TsTz => spg_storage::RangeKind::TsTz,
            spg_sql::ast::RangeKindAst::Date => spg_storage::RangeKind::Date,
        }),
        ColumnTypeName::Point => DataType::Point,
        ColumnTypeName::Lseg => DataType::Lseg,
        ColumnTypeName::Path => DataType::Path,
        ColumnTypeName::PgBox => DataType::PgBox,
        ColumnTypeName::Polygon => DataType::Polygon,
        ColumnTypeName::Line => DataType::Line,
        ColumnTypeName::Circle => DataType::Circle,
        ColumnTypeName::Inet => DataType::Inet,
        ColumnTypeName::Cidr => DataType::Cidr,
        ColumnTypeName::Macaddr => DataType::Macaddr,
        ColumnTypeName::Macaddr8 => DataType::Macaddr8,
        ColumnTypeName::Bit => DataType::Bit,
        ColumnTypeName::BitVarying => DataType::BitVarying,
        ColumnTypeName::Xml => DataType::Xml,
        ColumnTypeName::Char1 => DataType::Char1,
        ColumnTypeName::MoneyArray => DataType::MoneyArray,
    }
}

/// Convert an INSERT VALUES expression to a storage Value. Supports literal
/// expressions, unary-minus over numeric literals, and pgvector-style
/// `'[..]'::vector` cast (v1.2). Anything more complex returns `Unsupported`.
pub(crate) fn literal_expr_to_value(expr: Expr) -> Result<Value<'static>, EngineError> {
    match expr {
        Expr::Literal(l) => Ok(literal_to_value(l)),
        Expr::Cast { expr, target } => {
            let inner_value = literal_expr_to_value(*expr)?;
            crate::eval::cast_value(inner_value, target).map_err(EngineError::Eval)
        }
        Expr::Unary {
            op: UnOp::Neg,
            expr,
        } => match *expr {
            Expr::Literal(Literal::Integer(n)) => {
                // Fold to i32 if it fits, else BigInt. Parser emits Integer(i64)
                // — overflow on negate of i64::MIN is the one edge case.
                let neg = n.checked_neg().ok_or_else(|| {
                    EngineError::Unsupported("integer literal overflow on negation".into())
                })?;
                Ok(int_value_for(neg))
            }
            Expr::Literal(Literal::Float(x)) => Ok(Value::Float(-x)),
            // v7.38 (read01) — a dotted literal is NUMERIC; negate the mantissa.
            Expr::Literal(Literal::Numeric { unscaled, scale }) => {
                Ok(Value::Numeric { scaled: -unscaled, scale , kind: spg_storage::NumericKind::Finite })
            }
            // v7.38 (read01, T3.C3) — a NUMERIC literal beyond i128; negate by
            // flipping the sign of the decimal string, then re-resolve.
            Expr::Literal(Literal::NumericBig(ref s)) => {
                let flipped = if let Some(rest) = s.strip_prefix('-') {
                    rest.to_string()
                } else {
                    alloc::format!("-{s}")
                };
                Ok(big_literal_to_value(&flipped))
            }
            // v7.37.5 ship triage — fold the unary minus through a
            // `Cast { Literal, target }` wrapper (`-2::smallint`,
            // `-3.14::numeric(10,2)`). We negate the inner literal,
            // re-wrap with the same cast, and re-enter the literal
            // resolver — the cast path handles the typed result.
            Expr::Cast {
                expr: inner,
                target,
            } => {
                let negated_inner = match *inner {
                    Expr::Literal(Literal::Integer(n)) => {
                        let neg = n.checked_neg().ok_or_else(|| {
                            EngineError::Unsupported("integer literal overflow on negation".into())
                        })?;
                        Expr::Literal(Literal::Integer(neg))
                    }
                    Expr::Literal(Literal::Float(x)) => Expr::Literal(Literal::Float(-x)),
                    Expr::Literal(Literal::Numeric { unscaled, scale }) => {
                        Expr::Literal(Literal::Numeric { unscaled: -unscaled, scale })
                    }
                    // v7.38 (read01, T3.C3) — big NUMERIC literal: flip its sign
                    // in the decimal string, re-wrap with the same cast.
                    Expr::Literal(Literal::NumericBig(ref s)) => {
                        let flipped = if let Some(rest) = s.strip_prefix('-') {
                            rest.to_string()
                        } else {
                            alloc::format!("-{s}")
                        };
                        Expr::Literal(Literal::NumericBig(flipped))
                    }
                    other => Expr::Unary {
                        op: spg_sql::ast::UnOp::Neg,
                        expr: alloc::boxed::Box::new(other),
                    },
                };
                literal_expr_to_value(Expr::Cast {
                    expr: alloc::boxed::Box::new(negated_inner),
                    target,
                })
            }
            other => Err(EngineError::Unsupported(alloc::format!(
                "unary minus over non-literal expression: {other:?}"
            ))),
        },
        // v7.10.10 — `ARRAY[lit, lit, …]` constructor accepted at
        // INSERT-time. Each element must reduce to a Value through
        // `literal_expr_to_value`; NULL elements become `None`.
        // v7.11.13 — deduce shape from element values: all Int →
        // IntArray; any BigInt → BigIntArray (widening); any Text
        // → TextArray. Cast targets (`ARRAY[]::INT[]`) flow through
        // the outer Cast arm before reaching here and re-coerce.
        Expr::Array(items) => {
            let mut materialised: alloc::vec::Vec<Value<'static>> =
                alloc::vec::Vec::with_capacity(items.len());
            for elem in items {
                materialised.push(literal_expr_to_value(elem)?);
            }
            Ok(array_literal_widen(materialised))
        }
        // Any other Expr shape — fall back to a general evaluation
        // against an empty row + empty schema. This unblocks the
        // app-common patterns where INSERT VALUES carries a
        // non-correlated function call:
        //   INSERT INTO t VALUES (concat('U-', 42))
        //   INSERT INTO t VALUES (now())
        //   INSERT INTO t VALUES (format('%s-%s', 'a', 'b'))
        // Any expression that references a column or `$N`
        // placeholder fails cleanly inside `eval_expr` with a
        // descriptive error; literals + casts + ARRAY[…] continue
        // to take the fast paths above so the hot INSERT path is
        // unchanged on the common case.
        other => {
            let empty_schema: alloc::vec::Vec<spg_storage::ColumnSchema> = alloc::vec::Vec::new();
            let ctx = EvalContext::new(&empty_schema, None);
            let empty_row = spg_storage::Row::new(alloc::vec::Vec::new());
            crate::eval::eval_expr(&other, &empty_row, &ctx).map_err(EngineError::Eval)
        }
    }
}

pub(crate) fn literal_to_value(l: Literal) -> Value<'static> {
    match l {
        Literal::Integer(n) => int_value_for(n),
        Literal::Float(x) => Value::Float(x),
        Literal::Numeric { unscaled, scale } => Value::Numeric { scaled: unscaled, scale , kind: spg_storage::NumericKind::Finite },
        Literal::NumericBig(s) => big_literal_to_value(&s),
        Literal::String(s) => Value::text(s),
        Literal::Bool(b) => Value::Bool(b),
        Literal::Null => Value::Null,
        Literal::Vector(v) => Value::vector(v),
        Literal::TextArray(items) => Value::TextArray(items),
        Literal::IntArray(items) => Value::IntArray(items),
        Literal::BigIntArray(items) => Value::BigIntArray(items),
        Literal::Interval {
            months,
            days,
            micros,
            ..
        } => Value::Interval {
            months,
            days,
            micros,
        },
    }
}

/// Pick `Int` (`i32`) when the literal fits, else `BigInt`. `INT` vs `BIGINT`
/// columns will still enforce the right tag downstream — this is just the
/// default we synthesise from an unannotated integer literal.
pub(crate) fn int_value_for(n: i64) -> Value<'static> {
    if let Ok(small) = i32::try_from(n) {
        Value::Int(small)
    } else {
        Value::BigInt(n)
    }
}

/// Widen / narrow `v` to fit `expected`. Numerics permit safe widening
/// (`Int → BigInt`, `Int/BigInt → Float`) and best-effort narrowing
/// (`BigInt → Int` succeeds only when the value fits in `i32`). Everything
/// else returns `TypeMismatch` carrying the column name for caller diagnostics.
/// `NULL` is always permitted; the nullability check happens later in storage.
#[allow(clippy::too_many_lines)]
/// v7.17.0 Phase 4.4 — reject negative integer values on UNSIGNED
/// columns. Called after `coerce_value` at each INSERT / UPDATE
/// site that has ColumnSchema context. NULL passes through (a
/// nullable UNSIGNED column can legitimately hold NULL).
pub(crate) fn check_unsigned_range(
    v: &Value,
    schema: &ColumnSchema,
    position: usize,
) -> Result<(), EngineError> {
    if !schema.is_unsigned {
        return Ok(());
    }
    let n = match v {
        Value::SmallInt(x) => i64::from(*x),
        Value::Int(x) => i64::from(*x),
        Value::BigInt(x) => *x,
        _ => return Ok(()), // non-integer cells (NULL, default) skip
    };
    if n < 0 {
        return Err(EngineError::Unsupported(alloc::format!(
            "column {:?} is UNSIGNED but got negative value {n} at position {position}",
            schema.name
        )));
    }
    Ok(())
}

/// Coerce a non-empty `TEXT[]` (how an array literal reaches a typed-array
/// cast) into a typed array by parsing each element through the existing
/// scalar `coerce_value` path. NULL elements pass through. Returns `None` for
/// array targets this helper does not cover (leaving the caller's other arms
/// or the final type-mismatch to handle it).
fn coerce_text_array_to(
    items: alloc::vec::Vec<Option<alloc::string::String>>,
    target: &DataType,
    col: &str,
) -> Result<Option<Value<'static>>, EngineError> {
    let elem_dt = match target {
        DataType::BoolArray => DataType::Bool,
        DataType::NumericArray => DataType::Numeric { precision: 0, scale: 0 },
        DataType::DateArray => DataType::Date,
        DataType::TimestampArray => DataType::Timestamp,
        DataType::TimestamptzArray => DataType::Timestamptz,
        DataType::UuidArray => DataType::Uuid,
        _ => return Ok(None),
    };
    let mut scal: alloc::vec::Vec<Option<Value<'static>>> =
        alloc::vec::Vec::with_capacity(items.len());
    for item in items {
        match item {
            None => scal.push(None),
            Some(s) => scal.push(Some(coerce_value(Value::text(s), elem_dt.clone(), col, 0)?)),
        }
    }
    let out = match target {
        DataType::BoolArray => Value::BoolArray(
            scal.into_iter()
                .map(|o| o.map(|v| matches!(v, Value::Bool(true))))
                .collect(),
        ),
        DataType::NumericArray => Value::NumericArray(
            scal.into_iter()
                .map(|o| {
                    o.map(|v| match v {
                        Value::Numeric { scaled, scale, .. } => (scaled, scale),
                        _ => (0, 0),
                    })
                })
                .collect(),
        ),
        DataType::DateArray => Value::DateArray(
            scal.into_iter()
                .map(|o| {
                    o.map(|v| match v {
                        Value::Date(d) => d,
                        _ => 0,
                    })
                })
                .collect(),
        ),
        DataType::TimestampArray => Value::TimestampArray(
            scal.into_iter()
                .map(|o| {
                    o.map(|v| match v {
                        Value::Timestamp(t) => t,
                        _ => 0,
                    })
                })
                .collect(),
        ),
        DataType::TimestamptzArray => Value::TimestamptzArray(
            scal.into_iter()
                .map(|o| {
                    o.map(|v| match v {
                        Value::Timestamp(t) => t,
                        _ => 0,
                    })
                })
                .collect(),
        ),
        DataType::UuidArray => Value::UuidArray(
            scal.into_iter()
                .map(|o| {
                    o.map(|v| match v {
                        Value::Uuid(u) => u,
                        _ => [0u8; 16],
                    })
                })
                .collect(),
        ),
        _ => return Ok(None),
    };
    Ok(Some(out))
}

/// Parse a PG integer literal in text: decimal, plus the PG 16+ forms —
/// radix prefixes (`0x1F` hex / `0o17` octal / `0b101` binary) and `_` digit
/// separators (`1_000`). An optional leading sign applies to the magnitude.
/// Map a built-in type OID to its SQL-standard name, PG's `format_type`
/// / `oid::regtype` spelling (without the typmod). `None` for OIDs SPG
/// doesn't recognise (callers render the numeric OID, as PG does for an
/// unknown regtype). Shared by the `::regtype` cast and `format_type`.
pub(crate) fn regtype_oid_to_name(oid: i64) -> Option<&'static str> {
    Some(match oid {
        16 => "boolean",
        17 => "bytea",
        18 => "\"char\"",
        19 => "name",
        20 => "bigint",
        21 => "smallint",
        23 => "integer",
        25 => "text",
        26 => "oid",
        114 => "json",
        142 => "xml",
        650 => "cidr",
        700 => "real",
        701 => "double precision",
        774 => "macaddr8",
        790 => "money",
        829 => "macaddr",
        869 => "inet",
        1042 => "character",
        1043 => "character varying",
        1082 => "date",
        1083 => "time without time zone",
        1114 => "timestamp without time zone",
        1184 => "timestamp with time zone",
        1186 => "interval",
        1266 => "time with time zone",
        1700 => "numeric",
        2950 => "uuid",
        3614 => "tsvector",
        3615 => "tsquery",
        3802 => "jsonb",
        3904 => "int4range",
        3906 => "numrange",
        3908 => "tstzrange",
        3910 => "tsrange",
        3912 => "daterange",
        3926 => "int8range",
        _ => return None,
    })
}

pub(crate) fn parse_pg_int(s: &str) -> Option<i64> {
    let s = s.trim();
    let (neg, rest) = if let Some(r) = s.strip_prefix('-') {
        (true, r)
    } else if let Some(r) = s.strip_prefix('+') {
        (false, r)
    } else {
        (false, s)
    };
    // Split off an optional radix prefix (PG 16+: 0x / 0o / 0b), leaving
    // the digit portion. PG allows `_` group separators ONLY between two
    // digits — a leading/trailing/doubled underscore (`_5`, `5_`, `1__2`)
    // or one adjacent to the prefix is "invalid input syntax".
    let (radix, digits, has_prefix) =
        if let Some(h) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
            (16u32, h, true)
        } else if let Some(o) = rest.strip_prefix("0o").or_else(|| rest.strip_prefix("0O")) {
            (8, o, true)
        } else if let Some(b) = rest.strip_prefix("0b").or_else(|| rest.strip_prefix("0B")) {
            (2, b, true)
        } else {
            (10, rest, false)
        };
    let db = digits.as_bytes();
    // Reject a trailing or doubled underscore anywhere, and a leading
    // underscore unless it follows a radix prefix (PG accepts `0x_FF` but
    // not `_5` / `5_` / `1__2` / `0xFF_`).
    if db.last() == Some(&b'_')
        || digits.contains("__")
        || (!has_prefix && db.first() == Some(&b'_'))
    {
        return None;
    }
    let cleaned: alloc::string::String = digits.chars().filter(|&c| c != '_').collect();
    if cleaned.is_empty() {
        return None;
    }
    let mag = i64::from_str_radix(&cleaned, radix).ok()?;
    Some(if neg { mag.checked_neg()? } else { mag })
}

/// v7.38 (read01 P6.38) — well-formedness check for PG's `xml` CONTENT mode.
/// Verifies element tags are balanced and properly nested; comments (`<!-- -->`),
/// processing instructions (`<? ?>`), CDATA sections, `<!DOCTYPE …>`, plain
/// text, self-closing tags and multiple top-level elements are all accepted.
/// Attribute values are quote-aware so a `>` inside an attribute doesn't end a
/// tag early. This catches the common malformedness (unclosed / mismatched
/// tags) libxml2 rejects; deeper libxml2 checks (entity validity, duplicate
/// attributes, char legality) are a documented follow-up.
fn xml_content_is_well_formed(s: &str) -> bool {
    let b = s.as_bytes();
    let is_name = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b':') || c >= 0x80;
    let mut stack: alloc::vec::Vec<&[u8]> = alloc::vec::Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &s[i..];
        if rest.starts_with("<!--") {
            match rest.find("-->") {
                Some(p) => i += p + 3,
                None => return false,
            }
        } else if rest.starts_with("<![CDATA[") {
            match rest.find("]]>") {
                Some(p) => i += p + 3,
                None => return false,
            }
        } else if rest.starts_with("<?") {
            match rest.find("?>") {
                Some(p) => i += p + 2,
                None => return false,
            }
        } else if rest.starts_with("<!") {
            match rest.find('>') {
                Some(p) => i += p + 1,
                None => return false,
            }
        } else {
            // Element open / close / self-close tag.
            let close = i + 1 < b.len() && b[i + 1] == b'/';
            let name_start = if close { i + 2 } else { i + 1 };
            let mut j = name_start;
            while j < b.len() && is_name(b[j]) {
                j += 1;
            }
            if j == name_start {
                return false; // `<` not followed by a tag name
            }
            let name = &b[name_start..j];
            // Scan to the matching `>`, skipping quoted attribute values.
            let mut k = j;
            let mut quote = 0u8;
            let mut prev = 0u8;
            loop {
                if k >= b.len() {
                    return false; // unterminated tag
                }
                let c = b[k];
                if quote != 0 {
                    if c == quote {
                        quote = 0;
                    }
                } else if c == b'"' || c == b'\'' {
                    quote = c;
                } else if c == b'>' {
                    break;
                }
                prev = c;
                k += 1;
            }
            let self_closing = prev == b'/';
            i = k + 1;
            if close {
                match stack.pop() {
                    Some(top) if top == name => {}
                    _ => return false,
                }
            } else if !self_closing {
                stack.push(name);
            }
        }
    }
    stack.is_empty()
}

/// v7.38 (read01) — parse a float8 the way PG's `float8in` does: a
/// numeric literal that overflows to ±∞, or a nonzero magnitude that
/// underflows to 0, is "out of range" (returns None → the caller errors),
/// not a silent Infinity/0. The `inf`/`infinity`/`nan` spellings (a letter
/// after the optional sign) are the legitimate special values and pass.
pub(crate) fn parse_float8(s: &str) -> Option<f64> {
    let t = s.trim();
    let parsed = t.parse::<f64>().ok()?;
    let body = t.strip_prefix(['+', '-']).unwrap_or(t);
    let numeric_looking = body.bytes().next().is_some_and(|c| c.is_ascii_digit() || c == b'.');
    if numeric_looking {
        if parsed.is_infinite() {
            return None; // overflow
        }
        if parsed == 0.0 {
            // A mantissa with a nonzero digit that resolves to 0 underflowed.
            let mantissa = body.split(['e', 'E']).next().unwrap_or(body);
            if mantissa.bytes().any(|c| c.is_ascii_digit() && c != b'0') {
                return None;
            }
        }
    }
    Some(parsed)
}

/// v7.38 (read01) — decode PG's external array form (`{a,b,NULL}`) and coerce
/// each element to `elem` through `coerce_value`, so element semantics (bool
/// spellings, date formats, numeric parsing, float8 range) live in one place.
fn decode_array_elems(
    s: &str,
    elem: DataType,
    col_name: &str,
    position: usize,
) -> Result<Vec<Option<Value<'static>>>, EngineError> {
    let raw = decode_text_array_literal(s).map_err(|e| {
        EngineError::Eval(EvalError::TypeMismatch {
            detail: alloc::format!("cannot parse {s:?} as an array for column `{col_name}`: {e}"),
        })
    })?;
    let mut out = Vec::with_capacity(raw.len());
    for e in raw {
        match e {
            None => out.push(None),
            Some(t) => out.push(Some(coerce_value(Value::text(t), elem, col_name, position)?)),
        }
    }
    Ok(out)
}

pub(crate) fn coerce_value(
    v: Value<'static>,
    expected: DataType,
    col_name: &str,
    position: usize,
) -> Result<Value<'static>, EngineError> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    let actual = v.data_type().expect("non-null");
    if actual == expected {
        return Ok(v);
    }
    let coerced: Option<Value<'static>> = match (v, expected) {
        (Value::Int(n), DataType::BigInt) => Some(Value::BigInt(i64::from(n))),
        (Value::Int(n), DataType::Float) => Some(Value::Float(f64::from(n))),
        (Value::Int(n), DataType::SmallInt) => i16::try_from(n).ok().map(Value::SmallInt),
        (Value::Int(n), DataType::Numeric { precision, scale }) => Some(numeric_from_integer(
            i128::from(n),
            precision,
            scale,
            col_name,
        )?),
        (Value::SmallInt(n), DataType::Int) => Some(Value::Int(i32::from(n))),
        (Value::SmallInt(n), DataType::BigInt) => Some(Value::BigInt(i64::from(n))),
        (Value::SmallInt(n), DataType::Float) => Some(Value::Float(f64::from(n))),
        (Value::SmallInt(n), DataType::Numeric { precision, scale }) => Some(numeric_from_integer(
            i128::from(n),
            precision,
            scale,
            col_name,
        )?),
        (Value::BigInt(n), DataType::Int) => i32::try_from(n).ok().map(Value::Int),
        (Value::BigInt(n), DataType::SmallInt) => i16::try_from(n).ok().map(Value::SmallInt),
        #[allow(clippy::cast_precision_loss)]
        (Value::BigInt(n), DataType::Float) => Some(Value::Float(n as f64)),
        (Value::BigInt(n), DataType::Numeric { precision, scale }) => Some(numeric_from_integer(
            i128::from(n),
            precision,
            scale,
            col_name,
        )?),
        (Value::Float(x), DataType::Numeric { precision, scale }) => {
            // Unconstrained `numeric` (precision 0 is the sentinel —
            // numeric(0,0) is invalid in PG) keeps the value's
            // natural scale instead of truncating to 0 decimals.
            // Route the float through its shortest round-trip decimal
            // text so `3.14::numeric` stays 3.14, not 3.
            if precision == 0 && scale == 0 && x.is_finite() {
                if let Some((mantissa, src_scale)) =
                    parse_numeric_text(&alloc::format!("{x}"))
                {
                    Some(Value::Numeric {
                        scaled: mantissa,
                        scale: src_scale,
                     kind: spg_storage::NumericKind::Finite })
                } else {
                    Some(numeric_from_float(x, precision, scale, col_name)?)
                }
            } else {
                Some(numeric_from_float(x, precision, scale, col_name)?)
            }
        }
        // v7.17.0 Phase 3.P0-67 — Text → NUMERIC. Parse a
        // canonical decimal text (`"-1234.56"` / `"42"` /
        // `"0.0001"`) into `(mantissa, source_scale)` and rescale
        // to the column's declared scale. Required for prepared
        // binds: `value_to_literal` flattens a Value::Numeric
        // into a TEXT literal because Literal carries no native
        // Numeric variant, so the placeholder substitution path
        // reaches coerce_value as Text → Numeric. Without this
        // arm the round-trip surfaces a TypeMismatch even though
        // the cell already left the engine as a valid Numeric.
        (Value::Text(s), DataType::Numeric { precision, scale }) => {
            // v7.38 (read01, T6) — PG's NUMERIC specials (`'NaN'`, `'Infinity'`,
            // `'-Infinity'`) parse before the ordinary decimal path.
            if let Some(kind) = crate::numeric::parse_numeric_special(&s) {
                return Ok(Value::numeric_special(kind));
            }
            let Some((mantissa, src_scale)) = parse_numeric_text(&s) else {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!("cannot parse {s:?} as NUMERIC for column `{col_name}`"),
                }));
            };
            // Unconstrained `numeric` keeps the parsed scale as-is.
            if precision == 0 && scale == 0 {
                Some(Value::Numeric {
                    scaled: mantissa,
                    scale: src_scale,
                 kind: spg_storage::NumericKind::Finite })
            } else {
                Some(numeric_rescale(
                    mantissa, src_scale, precision, scale, col_name,
                )?)
            }
        }
        // Text → DATE / TIMESTAMP: parse canonical text forms.
        (Value::Text(s), DataType::Date) => {
            // PG truncates a full timestamp string on the way into a
            // DATE column (verified vs live PG18.4: INSERT
            // '2020-01-01 12:00:00' into a date column stores
            // 2020-01-01). Try the plain date parser first, then fall
            // back to the timestamp parser (validates the time) floored
            // to the day — mirroring the ::date cast path.
            let d = eval::parse_date_literal(&s)
                .or_else(|| {
                    eval::parse_timestamp_literal(&s).and_then(|t| {
                        i32::try_from(t.div_euclid(86_400_000_000)).ok()
                    })
                })
                .ok_or_else(|| {
                    EngineError::Eval(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "cannot parse {s:?} as DATE for column `{col_name}`"
                        ),
                    })
                })?;
            Some(Value::Date(d))
        }
        // v7.14.0 — MySQL DEFAULT clauses quote integer / float
        // / boolean literals (`DEFAULT '0'`, `DEFAULT '1'`,
        // `DEFAULT '3.14'`, `DEFAULT 'true'`). Coerce the text
        // form to the column's numeric / bool type at DEFAULT-
        // installation time so the storage check sees a typed
        // value. Parse failures fall through to TypeMismatch.
        // PG trims surrounding whitespace on numeric text input, so
        // `'  256  '::int2` / `'  3.14  '::float8` (both of which route
        // through this generic coerce path, unlike `::int` / `::float`
        // that trim in the CAST helper) parse rather than error.
        (Value::Text(s), DataType::SmallInt) => {
            parse_pg_int(&s).and_then(|n| i16::try_from(n).ok()).map(Value::SmallInt)
        }
        (Value::Text(s), DataType::Int) => {
            parse_pg_int(&s).and_then(|n| i32::try_from(n).ok()).map(Value::Int)
        }
        (Value::Text(s), DataType::BigInt) => parse_pg_int(&s).map(Value::BigInt),
        (Value::Text(s), DataType::Float) => parse_float8(&s).map(Value::Float),
        // v7.38 (read01, T-float4) — coerce to REAL narrows to f32.
        (Value::Int(n), DataType::Real) => Some(Value::Real(n as f32)),
        (Value::SmallInt(n), DataType::Real) => Some(Value::Real(f32::from(n))),
        (Value::BigInt(n), DataType::Real) => Some(Value::Real(n as f32)),
        (Value::Float(x), DataType::Real) => Some(Value::Real(x as f32)),
        (Value::Numeric { scaled, scale, .. }, DataType::Real) => {
            let mut div = 1.0f64;
            for _ in 0..scale {
                div *= 10.0;
            }
            Some(Value::Real((scaled as f64 / div) as f32))
        }
        (Value::Real(x), DataType::Float) => Some(Value::Float(f64::from(x))),
        (Value::Text(s), DataType::Real) => s.trim().parse::<f32>().ok().map(Value::Real),
        // PG boolin accepts any unambiguous prefix of true/false/yes/no,
        // plus on/off/1/0, case-insensitively with surrounding whitespace
        // trimmed. `o` alone is ambiguous (on vs off) → error.
        (Value::Text(s), DataType::Bool) => match s.trim().to_ascii_lowercase().as_str() {
            "0" | "f" | "fa" | "fal" | "fals" | "false" | "n" | "no" | "of" | "off" => {
                Some(Value::Bool(false))
            }
            "1" | "t" | "tr" | "tru" | "true" | "y" | "ye" | "yes" | "on" => {
                Some(Value::Bool(true))
            }
            _ => None,
        },
        // v7.17.0 Phase 3.P0-46 — MySQL TINYINT(1) (which Phase 4.3
        // classifies as DataType::Bool) is the storage shape every
        // mysqldump-restored boolean column lands in. mysqldump emits
        // the values as integer `0` / `1` literals, so int → bool
        // coerce on INSERT is required for a 0-change cutover. MySQL's
        // rule is "any non-zero is truthy"; we follow that for all
        // signed int widths so the same coerce path serves an
        // explicit `BOOLEAN` column too.
        (Value::Int(n), DataType::Bool) => Some(Value::Bool(n != 0)),
        (Value::SmallInt(n), DataType::Bool) => Some(Value::Bool(n != 0)),
        (Value::BigInt(n), DataType::Bool) => Some(Value::Bool(n != 0)),
        // v4.9: Text ↔ JSON coercion. No structural validation —
        // any text literal is accepted; the responsibility for
        // valid JSON lies with the producer.
        (Value::Text(s), DataType::Json) => Some(Value::json(s)),
        // v7.38 (read01) — a jsonb column canonicalises its value on
        // assignment, the same as PG's `::jsonb` cast.
        (Value::Text(s), DataType::Jsonb) => Some(Value::json(
            crate::json::canonicalize_jsonb(s.as_ref()).unwrap_or_else(|_| s.into_owned()),
        )),
        (Value::Json(s), DataType::Text) => Some(Value::text(s)),
        // v7.13.3 — mailrs round-7 S10. SPG's storage represents
        // both JSON and JSONB on-disk as `Value::json(String)` —
        // they share the underlying text payload. The cast
        // `'<text>'::jsonb` produces a Value::Json that needs to
        // satisfy a DataType::Jsonb column. Identity coerce in
        // both directions so JSON ↔ JSONB assignments work at all
        // INSERT / ALTER COLUMN TYPE / DEFAULT contexts.
        (Value::Json(s), DataType::Json) => Some(Value::json(s)),
        (Value::Json(s), DataType::Jsonb) => Some(Value::json(
            crate::json::canonicalize_jsonb(s.as_ref()).unwrap_or_else(|_| s.into_owned()),
        )),
        // v7.10.4 — Text → BYTEA. Decode PG-style literal forms:
        //   - Hex:    `\x48656c6c6f`  (case-insensitive hex pairs)
        //   - Escape: `Hello\\000world`  (backslash + octal triples)
        //   - Plain:  any string → raw UTF-8 bytes (PG also accepts)
        // Errors surface as TypeMismatch so the operator gets a
        // clear "this literal isn't a bytea literal" hint.
        (Value::Text(s), DataType::Bytes) => {
            let bytes = decode_bytea_literal(&s).map_err(|e| {
                EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "cannot parse {s:?} as BYTEA for column `{col_name}`: {e}"
                    ),
                })
            })?;
            Some(Value::bytes(bytes))
        }
        // v7.10.4 — BYTEA → Text round-trip uses the PG hex
        // output (lowercase, `\x` prefix). Important when a
        // SELECT pulls a bytea cell through a Text column path.
        (Value::Bytes(b), DataType::Text) => Some(Value::text(encode_bytea_hex(&b))),
        // v7.17.0 — Text → UUID. PG accepts canonical hyphenated,
        // unhyphenated, uppercase, and `{...}`-braced forms; we
        // funnel all four through `spg_storage::parse_uuid_str`.
        // A malformed literal surfaces as a SQL TypeMismatch
        // rather than silently inserting garbage — `0-change
        // cutover` requires that an app inserting bad UUID text
        // sees the same hard error PG would raise.
        (Value::Text(s), DataType::Uuid) => match spg_storage::parse_uuid_str(&s) {
            Some(b) => Some(Value::Uuid(b)),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for type uuid: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        // v7.17.0 — UUID → Text canonical 8-4-4-4-12 lowercase.
        // Surfaces when a SELECT plucks a uuid cell through a
        // Text column path (e.g. INSERT INTO log SELECT id::text
        // FROM other_table).
        (Value::Uuid(b), DataType::Text) => Some(Value::text(spg_storage::format_uuid(&b))),
        // v7.17.0 Phase 3.P0-32 — Text → TIME. Accepts
        // `HH:MM:SS` and `HH:MM:SS.ffffff` (1-6 fractional digits).
        // Out-of-range hour/min/sec is a hard SQL error (no
        // silent truncation — same 0-change-cutover discipline
        // we apply to UUID).
        (Value::Text(s), DataType::Time) => match parse_time_str(&s) {
            Some(us) => Some(Value::Time(us)),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for type time: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        // v7.17.0 Phase 3.P0-32 — TIME → Text canonical `HH:MM:SS[.ffffff]`.
        (Value::Time(us), DataType::Text) => Some(Value::text(eval::format_time(us))),
        // v7.17.0 Phase 3.P0-33 — int / bigint → YEAR. Range
        // check enforces the MySQL canonical 1901..=2155 + 0
        // sentinel; out-of-range is a hard SQL error (no silent
        // truncation, mirrors P0-32 / P0-25 discipline).
        (Value::SmallInt(n), DataType::Year) => Some(coerce_int_to_year(i64::from(n), col_name)?),
        (Value::Int(n), DataType::Year) => Some(coerce_int_to_year(i64::from(n), col_name)?),
        (Value::BigInt(n), DataType::Year) => Some(coerce_int_to_year(n, col_name)?),
        // Text → YEAR. Accepts the 4-digit decimal form only;
        // two-digit YEAR (`'99'` → 1999) was deprecated in MySQL
        // 5.7 and is out of scope for v7.17.0.
        (Value::Text(s), DataType::Year) => match s.trim().parse::<i64>() {
            Ok(n) => Some(coerce_int_to_year(n, col_name)?),
            Err(_) => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for type year: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        // YEAR → Text 4-digit zero-padded.
        (Value::Year(y), DataType::Text) => Some(Value::text(alloc::format!("{y:04}"))),
        // v7.17.0 Phase 3.P0-34 — Text → TIMETZ. Mandatory
        // signed offset suffix; missing offset is a hard error
        // (SPG has no session TZ wired into eval, unlike PG).
        (Value::Text(s), DataType::TimeTz) => match parse_timetz_str(&s) {
            Some((us, offset_secs)) => Some(Value::TimeTz { us, offset_secs }),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for type time with time zone: \
                         {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        // TIMETZ → Text canonical `HH:MM:SS[.ffffff]±HH[:MM]`.
        (Value::TimeTz { us, offset_secs }, DataType::Text) => {
            Some(Value::text(eval::format_timetz(us, offset_secs)))
        }
        // v7.17.0 Phase 3.P0-35 — Text → MONEY. Accepts `$N.NN`,
        // `$N,NNN.NN`, optional leading `-`. Bare numeric literals
        // arrive via the Int/BigInt/Float/Numeric arms below.
        (Value::Text(s), DataType::Money) => match parse_money_str(&s) {
            Some(c) => Some(Value::Money(c)),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for type money: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        // Int / BigInt / SmallInt / Float / Numeric → MONEY.
        // Bare numeric literal is interpreted as a major-unit
        // amount (matches PG: `100`::money → $100.00 = 10000 cents).
        (Value::SmallInt(n), DataType::Money) => {
            Some(Value::Money(i64::from(n).saturating_mul(100)))
        }
        (Value::Int(n), DataType::Money) => Some(Value::Money(i64::from(n).saturating_mul(100))),
        (Value::BigInt(n), DataType::Money) => Some(Value::Money(n.saturating_mul(100))),
        (Value::Float(x), DataType::Money) => {
            // Round half-away-from-zero to cents (no_std — no
            // `f64::round`, so hand-roll via biased truncation).
            let scaled = x * 100.0;
            let cents = if scaled >= 0.0 {
                (scaled + 0.5) as i64
            } else {
                (scaled - 0.5) as i64
            };
            Some(Value::Money(cents))
        }
        (Value::Numeric { scaled, scale, .. }, DataType::Money) => {
            // Convert exact decimal to cents (scale 2). If scale > 2,
            // round half-away-from-zero. If scale < 2, multiply up.
            let cents = if scale == 2 {
                scaled
            } else if scale < 2 {
                let mult = 10_i128.pow(u32::from(2 - scale));
                scaled.saturating_mul(mult)
            } else {
                let div = 10_i128.pow(u32::from(scale - 2));
                let half = div / 2;
                let bias = if scaled >= 0 { half } else { -half };
                (scaled + bias) / div
            };
            Some(Value::Money(i64::try_from(cents).unwrap_or(i64::MAX)))
        }
        // MONEY → Text canonical `$N,NNN.CC`.
        (Value::Money(c), DataType::Text) => Some(Value::text(eval::format_money(c))),
        // MONEY → NUMERIC: integer cents become a scale-2 decimal (dollars).
        (Value::Money(c), DataType::Numeric { .. }) => {
            Some(Value::Numeric { scaled: i128::from(c), scale: 2 , kind: spg_storage::NumericKind::Finite })
        }
        // v7.17.0 Phase 3.P0-38 — Text → Range. Accepts canonical
        // PG forms: `'empty'`, `'[a,b)'`, `'(a,b]'`, `'[a,b]'`,
        // `'(a,b)'`, with empty lower or upper for unbounded.
        (Value::Text(s), DataType::Range(kind)) => match parse_range_str(&s, kind) {
            Some(v) => Some(v),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for range type: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        // Range → Text canonical form (`[a,b)`, `'empty'`, etc).
        (v @ Value::Range { .. }, DataType::Text) => Some(Value::text(format_range_str(&v))),
        // v7.37.5 ζ-A — Text → network / bit / xml / "char" / money[].
        (Value::Text(s), DataType::Inet) => match parse_inet_text(&s) {
            Some((family, bits, addr)) => Some(Value::Inet { family, bits, addr }),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for INET: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        (Value::Text(s), DataType::Cidr) => match parse_inet_text(&s) {
            Some((family, bits, addr)) => Some(Value::Cidr { family, bits, addr }),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for CIDR: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        // INSERT / assignment of a text literal into an INTERVAL column
        // parses it, matching the `::interval` cast (mirrors macaddr/inet).
        (Value::Text(s), DataType::Interval) => {
            match spg_sql::parser::parse_interval_text(&s) {
                Some((months, days, micros)) => Some(Value::Interval { months, days, micros }),
                None => {
                    return Err(EngineError::Eval(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "invalid input syntax for INTERVAL: {s:?} (column `{col_name}`)"
                        ),
                    }));
                }
            }
        }
        (Value::Text(s), DataType::Macaddr) => match parse_macaddr_text(&s) {
            Some(m) => Some(Value::Macaddr(m)),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for MACADDR: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        (Value::Text(s), DataType::Macaddr8) => match parse_macaddr8_text(&s) {
            Some(m) => Some(Value::Macaddr8(m)),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for MACADDR8: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        // v7.37.5 ship triage — `Value::BitString` self-reports as
        // `DataType::BitVarying`(see `Value::data_type`), so an
        // INSERT into a `BIT` column triggers a spurious type
        // mismatch. Accept BitString into either Bit or BitVarying
        // unchanged — the column-side fixed-length contract is a
        // schema concern, not a value-shape one.
        (Value::BitString { nbits, bytes }, DataType::Bit | DataType::BitVarying) => {
            Some(Value::BitString { nbits, bytes })
        }
        (Value::Text(s), DataType::Bit | DataType::BitVarying) => match parse_bit_string_text(&s) {
            Some((nbits, bytes)) => Some(Value::bit_string(nbits, bytes)),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for BIT: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        (Value::Text(s), DataType::Xml) => {
            // v7.38 (read01 P6.38) — `::xml` (PG's CONTENT mode) requires the
            // text to be well-formed: element tags must be balanced and
            // properly nested. Plain text, multiple top-level elements,
            // comments/PIs/CDATA and self-closing tags are all fine.
            if !xml_content_is_well_formed(&s) {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!("invalid XML content: {s:?}"),
                }));
            }
            Some(Value::xml(s))
        }
        (Value::Text(s), DataType::Char1) => {
            let b = s.bytes().next().unwrap_or(0);
            Some(Value::Char1(b))
        }
        // v7.37.5 ζ-A — inverse coerces.
        (Value::Inet { family, bits, addr }, DataType::Text) => {
            Some(Value::text(format_inet(family, bits, &addr)))
        }
        (Value::Cidr { family, bits, addr }, DataType::Text) => {
            Some(Value::text(format_inet(family, bits, &addr)))
        }
        (Value::Macaddr(m), DataType::Text) => Some(Value::text(format_macaddr(&m))),
        (Value::Macaddr8(m), DataType::Text) => Some(Value::text(format_macaddr8(&m))),
        // MACADDR → MACADDR8: PG widens EUI-48 to EUI-64 by inserting the
        // `ff:fe` marker in the middle (08:00:2b:01:02:03 → 08:00:2b:ff:fe:01:02:03).
        (Value::Macaddr(m), DataType::Macaddr8) => Some(Value::Macaddr8([
            m[0], m[1], m[2], 0xff, 0xfe, m[3], m[4], m[5],
        ])),
        (Value::BitString { nbits, bytes }, DataType::Text) => {
            Some(Value::text(format_bit_string(nbits, &bytes)))
        }
        // BIT → integer: MSB-first bit value (PG bit→int cast).
        #[allow(clippy::cast_possible_truncation)]
        (Value::BitString { nbits, bytes }, DataType::SmallInt) => {
            Some(Value::SmallInt(bit_string_to_i64(nbits, &bytes) as i16))
        }
        #[allow(clippy::cast_possible_truncation)]
        (Value::BitString { nbits, bytes }, DataType::Int) => {
            Some(Value::Int(bit_string_to_i64(nbits, &bytes) as i32))
        }
        (Value::BitString { nbits, bytes }, DataType::BigInt) => {
            Some(Value::BigInt(bit_string_to_i64(nbits, &bytes)))
        }
        (Value::Xml(s), DataType::Text) => Some(Value::text(s)),
        (Value::Char1(b), DataType::Text) => Some(Value::text((b as char).to_string())),
        // v7.37.5 ε — Text → geometry coerce. Each parser returns
        // None on malformed input; we surface a TypeMismatch with
        // the column name so the engine error is debuggable.
        (Value::Text(s), DataType::Point) => match parse_point(&s) {
            Some(p) => Some(Value::Point(p)),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for POINT: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        (Value::Text(s), DataType::Lseg) => match parse_lseg_text(&s) {
            Some((p1, p2)) => Some(Value::Lseg(p1, p2)),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for LSEG: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        (Value::Text(s), DataType::PgBox) => match parse_box_text(&s) {
            Some((ur, ll)) => Some(Value::PgBox(ur, ll)),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for BOX: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        (Value::Text(s), DataType::Line) => match parse_line_text(&s) {
            Some((a, b, c)) => Some(Value::Line { a, b, c }),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for LINE: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        (Value::Text(s), DataType::Circle) => match parse_circle_text(&s) {
            Some((center, radius)) => Some(Value::Circle { center, radius }),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for CIRCLE: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        (Value::Text(s), DataType::Path) => match parse_path_text(&s) {
            Some((points, closed)) => Some(Value::Path { points, closed }),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for PATH: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        (Value::Text(s), DataType::Polygon) => match parse_polygon_text(&s) {
            Some(points) => Some(Value::Polygon(points)),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for POLYGON: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        // v7.37.5 ε — geometry → Text canonical forms.
        (Value::Point(p), DataType::Text) => Some(Value::text(format_point(p))),
        (Value::Lseg(p1, p2), DataType::Text) => Some(Value::text(format_lseg(p1, p2))),
        (Value::PgBox(ur, ll), DataType::Text) => Some(Value::text(format_pg_box(ur, ll))),
        (Value::Line { a, b, c }, DataType::Text) => Some(Value::text(format_line(a, b, c))),
        (Value::Circle { center, radius }, DataType::Text) => {
            Some(Value::text(format_circle(center, radius)))
        }
        (Value::Path { points, closed }, DataType::Text) => {
            Some(Value::text(format_path(&points, closed)))
        }
        (Value::Polygon(points), DataType::Text) => Some(Value::text(format_polygon(&points))),
        // v7.37.5 δ — Text → Multirange. Accepts `{}` empty and
        // `{[a,b),[c,d),...}` comma-separated ranges; each
        // subrange parses with the parent kind.
        (Value::Text(s), DataType::Multirange(kind)) => match parse_multirange_str(&s, kind) {
            Some(ranges) => Some(Value::Multirange { kind, ranges }),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for multirange type: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        // Multirange → Text canonical form (`{[a,b),[c,d)}`).
        (Value::Multirange { ranges, .. }, DataType::Text) => {
            Some(Value::text(format_multirange(&ranges)))
        }
        // v7.17.0 Phase 3.P0-39 — Text → Hstore.
        (Value::Text(s), DataType::Hstore) => match parse_hstore_str(&s) {
            Some(pairs) => Some(Value::Hstore(pairs)),
            None => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for type hstore: {s:?} (column `{col_name}`)"
                    ),
                }));
            }
        },
        // Hstore → Text canonical `"k"=>"v"` form.
        (Value::Hstore(pairs), DataType::Text) => Some(Value::text(format_hstore_str(&pairs))),
        // v7.17.0 Phase 3.P0-40 — Text → 2D arrays via PG
        // external `'{{a,b},{c,d}}'` literal.
        (Value::Text(s), DataType::IntArray2D) => match parse_int_2d_literal(&s) {
            Ok(m) => Some(Value::IntArray2D(m)),
            Err(e) => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for INT[][]: {s:?} (column `{col_name}`): {e}"
                    ),
                }));
            }
        },
        (Value::Text(s), DataType::BigIntArray2D) => match parse_bigint_2d_literal(&s) {
            Ok(m) => Some(Value::BigIntArray2D(m)),
            Err(e) => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for BIGINT[][]: {s:?} (column `{col_name}`): {e}"
                    ),
                }));
            }
        },
        (Value::Text(s), DataType::TextArray2D) => match parse_text_2d_literal(&s) {
            Ok(m) => Some(Value::TextArray2D(m)),
            Err(e) => {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid input syntax for TEXT[][]: {s:?} (column `{col_name}`): {e}"
                    ),
                }));
            }
        },
        // 2D arrays → Text canonical nested form.
        (Value::IntArray2D(rows), DataType::Text) => Some(Value::text(format_int_2d_text(&rows))),
        (Value::BigIntArray2D(rows), DataType::Text) => {
            Some(Value::text(format_bigint_2d_text(&rows)))
        }
        (Value::TextArray2D(rows), DataType::Text) => Some(Value::text(format_text_2d_text(&rows))),
        // v7.10.11 — Text → TEXT[]. Decode PG's external array
        // form `'{a,b,NULL}'`. NULL element token (case-insensitive)
        // is the literal `NULL`; everything else is a quoted or
        // unquoted text element. mailrs `'{label1,label2}'::TEXT[]`.
        (Value::Text(s), DataType::TextArray) => {
            let arr = decode_text_array_literal(&s).map_err(|e| {
                EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "cannot parse {s:?} as TEXT[] for column `{col_name}`: {e}"
                    ),
                })
            })?;
            Some(Value::TextArray(arr))
        }
        // v7.16.0 — Text → IntArray / BigIntArray for the
        // spg-sqlx Bind path. Decode the PG external form
        // `{1,2,3}` as a TEXT array first, then parse each
        // element as int. Same shape as the TextArray decode
        // above with an element-wise narrow.
        (Value::Text(s), DataType::IntArray) => {
            let arr = decode_text_array_literal(&s).map_err(|e| {
                EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "cannot parse {s:?} as INT[] for column `{col_name}`: {e}"
                    ),
                })
            })?;
            let mut out: Vec<Option<i32>> = Vec::with_capacity(arr.len());
            for elem in arr {
                match elem {
                    None => out.push(None),
                    Some(t) => {
                        let n: i32 = t.parse().map_err(|_| {
                            EngineError::Eval(EvalError::TypeMismatch {
                                detail: alloc::format!(
                                    "cannot parse {t:?} as INT element for `{col_name}`"
                                ),
                            })
                        })?;
                        out.push(Some(n));
                    }
                }
            }
            Some(Value::IntArray(out))
        }
        // v7.38 (read01) — the remaining Text → typed-array casts
        // (`'{1.5}'::numeric[]`, `'{t}'::bool[]`, `'{2020-01-01}'::date[]`, …),
        // which previously errored while `::int[]` / `::text[]` worked.
        (Value::Text(s), DataType::SmallIntArray) => Some(Value::SmallIntArray(
            decode_array_elems(&s, DataType::SmallInt, col_name, position)?
                .into_iter()
                .map(|o| match o {
                    Some(Value::SmallInt(n)) => Some(n),
                    _ => None,
                })
                .collect(),
        )),
        (Value::Text(s), DataType::BoolArray) => Some(Value::BoolArray(
            decode_array_elems(&s, DataType::Bool, col_name, position)?
                .into_iter()
                .map(|o| match o {
                    Some(Value::Bool(b)) => Some(b),
                    _ => None,
                })
                .collect(),
        )),
        (Value::Text(s), DataType::FloatArray) => Some(Value::FloatArray(
            decode_array_elems(&s, DataType::Float, col_name, position)?
                .into_iter()
                .map(|o| match o {
                    Some(Value::Float(f)) => Some(f),
                    _ => None,
                })
                .collect(),
        )),
        (Value::Text(s), DataType::NumericArray) => Some(Value::NumericArray(
            decode_array_elems(
                &s,
                DataType::Numeric { precision: 0, scale: 0 },
                col_name,
                position,
            )?
            .into_iter()
            .map(|o| match o {
                Some(Value::Numeric { scaled, scale, .. }) => Some((scaled, scale)),
                _ => None,
            })
            .collect(),
        )),
        (Value::Text(s), DataType::DateArray) => Some(Value::DateArray(
            decode_array_elems(&s, DataType::Date, col_name, position)?
                .into_iter()
                .map(|o| match o {
                    Some(Value::Date(d)) => Some(d),
                    _ => None,
                })
                .collect(),
        )),
        (Value::Text(s), DataType::UuidArray) => Some(Value::UuidArray(
            decode_array_elems(&s, DataType::Uuid, col_name, position)?
                .into_iter()
                .map(|o| match o {
                    Some(Value::Uuid(u)) => Some(u),
                    _ => None,
                })
                .collect(),
        )),
        (Value::Text(s), DataType::BigIntArray) => {
            let arr = decode_text_array_literal(&s).map_err(|e| {
                EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "cannot parse {s:?} as BIGINT[] for column `{col_name}`: {e}"
                    ),
                })
            })?;
            let mut out: Vec<Option<i64>> = Vec::with_capacity(arr.len());
            for elem in arr {
                match elem {
                    None => out.push(None),
                    Some(t) => {
                        let n: i64 = t.parse().map_err(|_| {
                            EngineError::Eval(EvalError::TypeMismatch {
                                detail: alloc::format!(
                                    "cannot parse {t:?} as BIGINT element for `{col_name}`"
                                ),
                            })
                        })?;
                        out.push(Some(n));
                    }
                }
            }
            Some(Value::BigIntArray(out))
        }
        // v7.10.11 — TEXT[] → Text round-trip uses PG's
        // external array form (`{a,b,NULL}`). Lets a SELECT
        // pull an array column through any Text-side codepath.
        (Value::TextArray(items), DataType::Text) => Some(Value::text(encode_text_array(&items))),
        // v7.37.5 ship triage — empty `ARRAY[]` literal lands as
        // `Value::TextArray(vec![])`. Allow widening to the typed
        // array sibling so `ARRAY[]::BOOL[]` / `::FLOAT[]` etc.
        // round-trip through INSERT into the typed column. Only
        // empty contents go through silently — non-empty TextArray
        // must round-trip via per-element parsing(handled by the
        // existing element-specific coercion paths above).
        (Value::TextArray(items), DataType::BoolArray) if items.is_empty() => {
            Some(Value::BoolArray(alloc::vec::Vec::new()))
        }
        (Value::TextArray(items), DataType::SmallIntArray) if items.is_empty() => {
            Some(Value::SmallIntArray(alloc::vec::Vec::new()))
        }
        (Value::TextArray(items), DataType::IntArray) if items.is_empty() => {
            Some(Value::IntArray(alloc::vec::Vec::new()))
        }
        (Value::TextArray(items), DataType::BigIntArray) if items.is_empty() => {
            Some(Value::BigIntArray(alloc::vec::Vec::new()))
        }
        (Value::TextArray(items), DataType::FloatArray) if items.is_empty() => {
            Some(Value::FloatArray(alloc::vec::Vec::new()))
        }
        // `expr::float8[]` — an array literal reaches here as TEXT[] (elements
        // rendered to text); parse each element to f64. NULLs pass through.
        (Value::TextArray(items), DataType::FloatArray) => {
            let mut out = alloc::vec::Vec::with_capacity(items.len());
            let mut ok = true;
            for item in items {
                match item {
                    None => out.push(None),
                    Some(s) => match s.trim().parse::<f64>() {
                        Ok(x) => out.push(Some(x)),
                        Err(_) => {
                            ok = false;
                            break;
                        }
                    },
                }
            }
            if ok { Some(Value::FloatArray(out)) } else { None }
        }
        // Identity for an already-float array, and widen integer arrays
        // element-wise (PG accepts `ARRAY[1,2]::float8[]`).
        (Value::FloatArray(items), DataType::FloatArray) => Some(Value::FloatArray(items)),
        #[allow(clippy::cast_precision_loss)]
        (Value::IntArray(items), DataType::FloatArray) => Some(Value::FloatArray(
            items.into_iter().map(|o| o.map(|n| f64::from(n))).collect(),
        )),
        #[allow(clippy::cast_precision_loss)]
        (Value::BigIntArray(items), DataType::FloatArray) => Some(Value::FloatArray(
            items.into_iter().map(|o| o.map(|n| n as f64)).collect(),
        )),
        // v7.38 (read01) — widen a NUMERIC[] into float8[] element-wise (PG
        // accepts `ARRAY[1.5::numeric]::float8[]` and coerces a numeric array
        // into a float8[] column on INSERT). Mirrors the scalar Numeric→Float.
        #[allow(clippy::cast_precision_loss)]
        (Value::NumericArray(items), DataType::FloatArray) => Some(Value::FloatArray(
            items
                .into_iter()
                .map(|o| {
                    o.map(|(scaled, scale)| {
                        let mut div = 1.0_f64;
                        for _ in 0..scale {
                            div *= 10.0;
                        }
                        scaled as f64 / div
                    })
                })
                .collect(),
        )),
        // v7.38 (read01) — the rest of the numeric-array coercion matrix PG
        // accepts on INSERT / cast. Widening int→bigint / int·bigint→numeric /
        // float→numeric never fails; narrowing bigint→int fails the whole
        // coercion (→ None) if any element overflows i32.
        (Value::IntArray(items), DataType::BigIntArray) => Some(Value::BigIntArray(
            items.into_iter().map(|o| o.map(i64::from)).collect(),
        )),
        (Value::BigIntArray(items), DataType::IntArray) => {
            let mut out = alloc::vec::Vec::with_capacity(items.len());
            let mut ok = true;
            for o in items {
                match o {
                    None => out.push(None),
                    Some(n) => match i32::try_from(n) {
                        Ok(v) => out.push(Some(v)),
                        Err(_) => {
                            ok = false;
                            break;
                        }
                    },
                }
            }
            if ok { Some(Value::IntArray(out)) } else { None }
        }
        (Value::IntArray(items), DataType::NumericArray) => Some(Value::NumericArray(
            items
                .into_iter()
                .map(|o| o.map(|n| (i128::from(n), 0_u8)))
                .collect(),
        )),
        (Value::BigIntArray(items), DataType::NumericArray) => Some(Value::NumericArray(
            items
                .into_iter()
                .map(|o| o.map(|n| (i128::from(n), 0_u8)))
                .collect(),
        )),
        (Value::FloatArray(items), DataType::NumericArray) => {
            let mut out = alloc::vec::Vec::with_capacity(items.len());
            let mut ok = true;
            for o in items {
                match o {
                    None => out.push(None),
                    Some(x) => match parse_numeric_text(&alloc::format!("{x}")) {
                        Some((mantissa, scale)) => out.push(Some((mantissa, scale))),
                        None => {
                            ok = false;
                            break;
                        }
                    },
                }
            }
            if ok { Some(Value::NumericArray(out)) } else { None }
        }
        // v7.38 (read01) — narrow a NUMERIC[] into int[] / bigint[] element-wise,
        // rounding half away from zero (PG) like the scalar Numeric→Int coercion.
        // An out-of-range element fails the whole coercion (→ None).
        (Value::NumericArray(items), DataType::IntArray) => {
            let mut out = alloc::vec::Vec::with_capacity(items.len());
            let mut ok = true;
            for o in items {
                match o {
                    None => out.push(None),
                    Some((scaled, scale)) => {
                        match i32::try_from(numeric_round_to_integer(scaled, scale)) {
                            Ok(v) => out.push(Some(v)),
                            Err(_) => {
                                ok = false;
                                break;
                            }
                        }
                    }
                }
            }
            if ok { Some(Value::IntArray(out)) } else { None }
        }
        (Value::NumericArray(items), DataType::BigIntArray) => {
            let mut out = alloc::vec::Vec::with_capacity(items.len());
            let mut ok = true;
            for o in items {
                match o {
                    None => out.push(None),
                    Some((scaled, scale)) => {
                        match i64::try_from(numeric_round_to_integer(scaled, scale)) {
                            Ok(v) => out.push(Some(v)),
                            Err(_) => {
                                ok = false;
                                break;
                            }
                        }
                    }
                }
            }
            if ok { Some(Value::BigIntArray(out)) } else { None }
        }
        // v7.38 (read01, T2) — float8[] → int[] / bigint[], rounding each element
        // half-to-even (PG's float→int rule, distinct from numeric's half-away).
        // A non-finite / out-of-range element fails the whole coercion.
        #[allow(clippy::cast_possible_truncation)]
        (Value::FloatArray(items), DataType::IntArray) => {
            let mut out = alloc::vec::Vec::with_capacity(items.len());
            let mut ok = true;
            for o in items {
                match o {
                    None => out.push(None),
                    Some(x) if x.is_finite() => {
                        let r = crate::eval::math::f64_round_half_even(x);
                        if r >= f64::from(i32::MIN) && r <= f64::from(i32::MAX) {
                            out.push(Some(r as i32));
                        } else {
                            ok = false;
                            break;
                        }
                    }
                    Some(_) => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok { Some(Value::IntArray(out)) } else { None }
        }
        #[allow(clippy::cast_possible_truncation)]
        (Value::FloatArray(items), DataType::BigIntArray) => {
            let mut out = alloc::vec::Vec::with_capacity(items.len());
            let mut ok = true;
            for o in items {
                match o {
                    None => out.push(None),
                    Some(x) if x.is_finite() => {
                        out.push(Some(crate::eval::math::f64_round_half_even(x) as i64));
                    }
                    Some(_) => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok { Some(Value::BigIntArray(out)) } else { None }
        }
        (Value::TextArray(items), DataType::NumericArray) if items.is_empty() => {
            Some(Value::NumericArray(alloc::vec::Vec::new()))
        }
        (Value::TextArray(items), DataType::DateArray) if items.is_empty() => {
            Some(Value::DateArray(alloc::vec::Vec::new()))
        }
        (Value::TextArray(items), DataType::TimestampArray) if items.is_empty() => {
            Some(Value::TimestampArray(alloc::vec::Vec::new()))
        }
        (Value::TextArray(items), DataType::TimestamptzArray) if items.is_empty() => {
            Some(Value::TimestamptzArray(alloc::vec::Vec::new()))
        }
        (Value::TextArray(items), DataType::UuidArray) if items.is_empty() => {
            Some(Value::UuidArray(alloc::vec::Vec::new()))
        }
        (Value::TextArray(items), DataType::JsonArray) if items.is_empty() => {
            Some(Value::JsonArray(alloc::vec::Vec::new()))
        }
        (Value::TextArray(items), DataType::JsonbArray) if items.is_empty() => {
            Some(Value::JsonbArray(alloc::vec::Vec::new()))
        }
        (Value::TextArray(items), DataType::BytesArray) if items.is_empty() => {
            Some(Value::BytesArray(alloc::vec::Vec::new()))
        }
        (Value::TextArray(items), DataType::IntervalArray) if items.is_empty() => {
            Some(Value::IntervalArray(alloc::vec::Vec::new()))
        }
        // Non-empty `TEXT[]` → typed array (`ARRAY[..]::bool[]`, `::numeric[]`,
        // `::date[]`, `::timestamp[]`, `::uuid[]`): parse each element via the
        // scalar path. Empty arrays are handled by the arms above.
        (
            Value::TextArray(items),
            dt @ (DataType::BoolArray
            | DataType::NumericArray
            | DataType::DateArray
            | DataType::TimestampArray
            | DataType::TimestamptzArray
            | DataType::UuidArray),
        ) => coerce_text_array_to(items, &dt, col_name)?,
        (Value::TextArray(items), DataType::MoneyArray) if items.is_empty() => {
            Some(Value::MoneyArray(alloc::vec::Vec::new()))
        }
        // v7.37.5 ship triage — IntArray(empty) widens to
        // SmallIntArray for the `INSERT INTO t (xs) VALUES
        // (ARRAY[1::smallint, …])` path where the array literal
        // collected mixed int widths into IntArray.
        (Value::IntArray(items), DataType::SmallIntArray) => {
            let mut out = alloc::vec::Vec::with_capacity(items.len());
            let mut ok = true;
            for item in items {
                match item {
                    None => out.push(None),
                    Some(n) => match i16::try_from(n) {
                        Ok(x) => out.push(Some(x)),
                        Err(_) => {
                            ok = false;
                            break;
                        }
                    },
                }
            }
            if ok {
                Some(Value::SmallIntArray(out))
            } else {
                None
            }
        }
        // v7.17.0 Phase 3.P0-68 — Text → VECTOR auto-coerce.
        // Matches the existing Text → TsVector arm and the
        // `::vector` cast: PG-canonical pgvector external form
        // (`'[1, 2, -3]'`) becomes a typed Vector value at the
        // column boundary. Dim mismatch surfaces as TypeMismatch.
        // For SQ8 / HALF encodings we chain through the standard
        // quantise helpers so the storage shape matches the
        // declared encoding without a second coerce pass.
        (Value::Text(s), DataType::Vector { dim, encoding }) => {
            let parsed = eval::parse_vector_text(&s).ok_or_else(|| {
                EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!("cannot parse {s:?} as VECTOR for column `{col_name}`"),
                })
            })?;
            if parsed.len() != dim as usize {
                return Err(EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "VECTOR({dim}) column `{col_name}` rejects literal of length {}",
                        parsed.len()
                    ),
                }));
            }
            Some(match encoding {
                VecEncoding::F32 => Value::vector(parsed),
                VecEncoding::Sq8 => Value::Sq8Vector(spg_storage::quantize::quantize(&parsed)),
                VecEncoding::F16 => {
                    Value::HalfVector(spg_storage::halfvec::HalfVector::from_f32_slice(&parsed))
                }
            })
        }
        // v7.16.1 — Text → TSVECTOR auto-coerce for the
        // INSERT-side wire path (mailrs round-9 A.2.a). PG
        // implicitly promotes the TEXT literal at INSERT into a
        // TSVECTOR column; SPG previously rejected with a hard
        // type mismatch, blocking 23,276 pg_dump rows into
        // `messages.search_vector`. We route through the same
        // `decode_tsvector_external` the `::tsvector` cast
        // already uses, so PG-canonical forms (`'word'`,
        // `'word:1A,2B'`, multi-lexeme, empty `''`) all parse.
        (Value::Text(s), DataType::TsVector) => {
            let lexs = eval::decode_tsvector_external(&s).map_err(|e| {
                EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "cannot parse {s:?} as TSVECTOR for column `{col_name}`: {e}"
                    ),
                })
            })?;
            Some(Value::TsVector(lexs))
        }
        (Value::Text(s), DataType::Timestamp | DataType::Timestamptz) => {
            let t = eval::parse_timestamp_literal(&s).ok_or_else(|| {
                EngineError::Eval(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "cannot parse {s:?} as TIMESTAMP for column `{col_name}`"
                    ),
                })
            })?;
            Some(Value::Timestamp(t))
        }
        // DATE ↔ TIMESTAMP convertibility (DATE → midnight,
        // TIMESTAMP → day truncation).
        (Value::Date(d), DataType::Timestamp | DataType::Timestamptz) => {
            Some(Value::Timestamp(i64::from(d) * 86_400_000_000))
        }
        // v7.9.21 — Value::Timestamp lands in either Timestamp
        // or Timestamptz columns; the on-disk layout is the
        // same i64 microseconds UTC.
        (Value::Timestamp(t), DataType::Timestamptz) => Some(Value::Timestamp(t)),
        (Value::Timestamp(t), DataType::Date) => {
            let days = t.div_euclid(86_400_000_000);
            i32::try_from(days).ok().map(Value::Date)
        }
        (
            Value::Numeric {
                scaled,
                scale: src_scale,
             .. },
            DataType::Numeric { precision, scale },
        ) => {
            // v7.38 (read01) — the unconstrained `::numeric` sentinel (0, 0)
            // keeps the value's natural scale, matching the Float/Text→Numeric
            // arms above; only a declared numeric(p, s) rescales. Without this,
            // casting an existing NUMERIC through unconstrained numeric
            // (`n::numeric(5,2)::numeric`) rounded it to scale 0.
            if precision == 0 && scale == 0 {
                Some(Value::Numeric {
                    scaled,
                    scale: src_scale,
                 kind: spg_storage::NumericKind::Finite })
            } else {
                Some(numeric_rescale(scaled, src_scale, precision, scale, col_name)?)
            }
        }
        #[allow(clippy::cast_precision_loss)]
        (Value::Numeric { scaled, scale, .. }, DataType::Float) => {
            let mut div = 1.0_f64;
            for _ in 0..scale {
                div *= 10.0;
            }
            Some(Value::Float((scaled as f64) / div))
        }
        // v7.38 (read01) — coercing NUMERIC into an integer column rounds half
        // away from zero (PG assignment cast: `1.5 → 2`), matching the `::int`
        // cast path; it previously truncated (`1.7 → 1`).
        (Value::Numeric { scaled, scale, .. }, DataType::Int) => {
            let rounded = numeric_round_to_integer(scaled, scale);
            i32::try_from(rounded).ok().map(Value::Int)
        }
        (Value::Numeric { scaled, scale, .. }, DataType::BigInt) => {
            let rounded = numeric_round_to_integer(scaled, scale);
            i64::try_from(rounded).ok().map(Value::BigInt)
        }
        (Value::Numeric { scaled, scale, .. }, DataType::SmallInt) => {
            let rounded = numeric_round_to_integer(scaled, scale);
            i16::try_from(rounded).ok().map(Value::SmallInt)
        }
        // VARCHAR(n) enforces an upper bound on character count. A bare
        // `varchar` (no typmod) is modelled as `Varchar(0)` and, like PG, holds
        // a string of any length — `'a'::varchar` must not read as VARCHAR(0).
        (Value::Text(s), DataType::Varchar(max)) => {
            if max == 0 || u32::try_from(s.chars().count()).unwrap_or(u32::MAX) <= max {
                Some(Value::text(s))
            } else {
                return Err(EngineError::Unsupported(alloc::format!(
                    "value for VARCHAR({max}) column `{col_name}` exceeds length: \
                     {} chars",
                    s.chars().count()
                )));
            }
        }
        // v6.0.1: f32 → SQ8 INSERT-time quantisation. Triggered
        // when the column declares `VECTOR(N) USING SQ8` and
        // the INSERT VALUES expression yields a raw f32 vector
        // (the normal pgvector-shape literal). Dim mismatch
        // falls through the `_ => None` arm and surfaces as
        // `TypeMismatch` with the expected SQ8 column type —
        // matching the F32 path's existing error.
        (
            Value::Vector(v),
            DataType::Vector {
                dim,
                encoding: VecEncoding::Sq8,
            },
        ) if v.len() == dim as usize => Some(Value::Sq8Vector(spg_storage::quantize::quantize(&v))),
        // v6.0.3: f32 → f16 INSERT-time conversion for HALF
        // columns. Bit-exact at the storage layer (modulo
        // half-precision rounding); no rerank pass needed at
        // search time.
        (
            Value::Vector(v),
            DataType::Vector {
                dim,
                encoding: VecEncoding::F16,
            },
        ) if v.len() == dim as usize => Some(Value::HalfVector(
            spg_storage::halfvec::HalfVector::from_f32_slice(&v),
        )),
        // CHAR(n) right-pads with U+0020 to exactly n chars; if the input
        // is already longer we reject (PG truncates trailing-space-only;
        // staying strict for v1).
        (Value::Text(s), DataType::Char(size)) => {
            let len = u32::try_from(s.chars().count()).unwrap_or(u32::MAX);
            if len > size {
                return Err(EngineError::Unsupported(alloc::format!(
                    "value for CHAR({size}) column `{col_name}` exceeds length: \
                     {len} chars"
                )));
            }
            let need = (size - len) as usize;
            let mut padded = s.into_owned();
            padded.reserve(need);
            for _ in 0..need {
                padded.push(' ');
            }
            // v7.38 (read01, T11) — CHAR(n) is bpchar: blank-padded, and
            // length / comparison / ::text ignore the padding (handled at those
            // sites).
            Some(Value::BpChar(alloc::borrow::Cow::Owned(padded)))
        }
        _ => None,
    };
    coerced.ok_or(EngineError::Storage(StorageError::TypeMismatch {
        column: col_name.into(),
        expected,
        actual,
        position,
    }))
}

/// v7.38 (read01, T3.C3) — a lexer-validated big decimal literal → NumericBig,
/// demoted to a plain Numeric if its mantissa happens to fit i128.
pub(crate) fn big_literal_to_value(s: &str) -> Value<'static> {
    let b = spg_storage::bignum::BigNumeric::from_decimal_str(s)
        .expect("lexer-validated decimal");
    match b.to_i128() {
        Some(scaled) => Value::Numeric { scaled, scale: b.scale(), kind: spg_storage::NumericKind::Finite },
        None => Value::NumericBig(alloc::boxed::Box::new(b)),
    }
}
