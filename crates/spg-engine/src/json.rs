// Recursive-descent JSON parser. Several lints are inherent to the
// hand-rolled byte-scan style and don't add clarity here.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::format_push_string,
    clippy::needless_continue,
    clippy::needless_range_loop,
    clippy::single_match,
    clippy::uninlined_format_args
)]

//! v4.14 minimal JSON parser for the `->` / `->>` operators.
//!
//! Hand-rolled, no external dep — same policy as the rest of the
//! engine. Supports the JSON grammar from RFC 8259: objects,
//! arrays, strings (with `\"` / `\\` / `\/` / `\b` / `\f` / `\n`
//! / `\r` / `\t` / `\uXXXX` escapes), numbers, true / false /
//! null. The parser returns a tree we walk by key (object) or
//! integer index (array); accesses that miss return `Value::Null`
//! per PG semantics.
//!
//! `path_get(doc, key, as_text)` is the public entry. When
//! `as_text` is true (`->>` operator), JSON strings unwrap to
//! raw text and other scalars render as their canonical text;
//! when false (`->`), the result is wrapped back into a Json
//! value (the inner subtree rendered to its canonical JSON
//! string form).

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_storage::Value;

use crate::eval::EvalError;

#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    /// Original numeric text, so integer round-trips don't drift to
    /// `1.0`. We render either the raw lexeme (when present) or
    /// `Number`'s default formatting.
    NumberText(String),
    String(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

impl JsonValue {
    fn as_text(&self) -> String {
        match self {
            Self::Null => "null".into(),
            Self::Bool(b) => if *b { "true" } else { "false" }.into(),
            Self::Number(x) => alloc::format!("{x}"),
            Self::NumberText(s) | Self::String(s) => s.clone(),
            Self::Array(_) | Self::Object(_) => self.to_json_text(),
        }
    }

    fn to_json_text(&self) -> String {
        let mut out = String::new();
        write_json(self, &mut out);
        out
    }
}

fn write_json(v: &JsonValue, out: &mut String) {
    match v {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Bool(true) => out.push_str("true"),
        JsonValue::Bool(false) => out.push_str("false"),
        JsonValue::Number(x) => out.push_str(&alloc::format!("{x}")),
        JsonValue::NumberText(s) => out.push_str(s),
        JsonValue::String(s) => {
            out.push('"');
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if (c as u32) < 0x20 => {
                        out.push_str(&alloc::format!("\\u{:04x}", c as u32));
                    }
                    c => out.push(c),
                }
            }
            out.push('"');
        }
        JsonValue::Array(items) => {
            out.push('[');
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(it, out);
            }
            out.push(']');
        }
        JsonValue::Object(entries) => {
            out.push('{');
            for (i, (k, val)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(&JsonValue::String(k.clone()), out);
                out.push(':');
                write_json(val, out);
            }
            out.push('}');
        }
    }
}

/// v6.4.5 — PG `json #> path_text` / `json #>> path_text`. The
/// right-hand side is a PG text-array literal `'{a,0,b}'` whose
/// elements are walked left-to-right; each element is either an
/// object key or (when it parses as a non-negative integer) an
/// array index. Missing or non-existent steps return `Value::Null`.
pub fn path_walk(lhs: &Value, rhs: &Value, as_text: bool) -> Result<Value, EvalError> {
    let src = match lhs {
        Value::Json(s) | Value::Text(s) => s.as_str(),
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "JSON path walk: left side must be JSON or TEXT, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let path_text = match rhs {
        Value::Text(s) | Value::Json(s) => s.as_str(),
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "JSON path walk: right side must be TEXT, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let path = parse_text_array(path_text)?;
    let mut cur = parse(src).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("invalid JSON for path walk: {e}"),
    })?;
    for step in &path {
        let next = match (&cur, step.as_str()) {
            (JsonValue::Object(entries), key) => entries
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone()),
            (JsonValue::Array(items), key) => {
                let Ok(idx) = key.parse::<i64>() else {
                    return Ok(Value::Null);
                };
                if idx >= 0 {
                    items.get(idx as usize).cloned()
                } else {
                    let from_end = items.len() as i64 + idx;
                    if from_end >= 0 {
                        items.get(from_end as usize).cloned()
                    } else {
                        None
                    }
                }
            }
            _ => return Ok(Value::Null),
        };
        cur = match next {
            None => return Ok(Value::Null),
            Some(v) => v,
        };
    }
    if matches!(cur, JsonValue::Null) {
        return Ok(Value::Null);
    }
    if as_text {
        Ok(Value::Text(cur.as_text()))
    } else {
        Ok(Value::Json(cur.to_json_text()))
    }
}

/// v6.4.5 — PG `json @> sub_json` containment. Returns BOOL.
/// `lhs @> rhs` is true when every member of `rhs` is structurally
/// contained in `lhs`:
///   - Scalars: equal
///   - Objects: every (key, value) in rhs exists in lhs with a
///     containing value
///   - Arrays: every element in rhs has a containing element in lhs
pub fn contains(lhs: &Value, rhs: &Value) -> Result<Value, EvalError> {
    let lhs_text = match lhs {
        Value::Json(s) | Value::Text(s) => s.as_str(),
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "JSON @>: left side must be JSON or TEXT, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let rhs_text = match rhs {
        Value::Json(s) | Value::Text(s) => s.as_str(),
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "JSON @>: right side must be JSON or TEXT, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let lhs_doc = parse(lhs_text).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("invalid JSON on left of @>: {e}"),
    })?;
    let rhs_doc = parse(rhs_text).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("invalid JSON on right of @>: {e}"),
    })?;
    Ok(Value::Bool(json_contains(&lhs_doc, &rhs_doc)))
}

fn json_contains(lhs: &JsonValue, rhs: &JsonValue) -> bool {
    match (lhs, rhs) {
        (JsonValue::Object(l), JsonValue::Object(r)) => r
            .iter()
            .all(|(rk, rv)| l.iter().any(|(lk, lv)| lk == rk && json_contains(lv, rv))),
        (JsonValue::Array(l), JsonValue::Array(r)) => {
            r.iter().all(|rv| l.iter().any(|lv| json_contains(lv, rv)))
        }
        _ => json_eq(lhs, rhs),
    }
}

fn json_eq(a: &JsonValue, b: &JsonValue) -> bool {
    match (a, b) {
        (JsonValue::Null, JsonValue::Null) => true,
        (JsonValue::Bool(x), JsonValue::Bool(y)) => x == y,
        (JsonValue::String(x), JsonValue::String(y)) => x == y,
        (JsonValue::Number(x), JsonValue::Number(y)) => (x - y).abs() < 1e-12,
        (JsonValue::NumberText(x), JsonValue::NumberText(y)) => x == y,
        (JsonValue::NumberText(x), JsonValue::Number(y))
        | (JsonValue::Number(y), JsonValue::NumberText(x)) => {
            x.parse::<f64>().is_ok_and(|xn| (xn - y).abs() < 1e-12)
        }
        (JsonValue::Array(x), JsonValue::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| json_eq(a, b))
        }
        (JsonValue::Object(x), JsonValue::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.iter().any(|(k2, v2)| k == k2 && json_eq(v, v2)))
        }
        _ => false,
    }
}

/// Parse PG's text-array literal `'{a,b,c}'` into a Vec<String>.
/// Whitespace around elements is trimmed; quoted elements (`"x,y"`)
/// preserve embedded commas (minimal support — full PG array
/// escaping is OOS).
fn parse_text_array(s: &str) -> Result<Vec<String>, EvalError> {
    let trimmed = s.trim();
    let inner = if let Some(stripped) = trimmed.strip_prefix('{').and_then(|s| s.strip_suffix('}'))
    {
        stripped
    } else {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("path walk: expected PG array literal `{{…}}`, got {s:?}"),
        });
    };
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                out.push(cur.trim().to_string());
                cur = String::new();
            }
            '\\' => {
                if let Some(&next) = chars.peek() {
                    cur.push(next);
                    chars.next();
                }
            }
            _ => cur.push(c),
        }
    }
    out.push(cur.trim().to_string());
    Ok(out)
}

/// PG `json -> key` / `json ->> key`. `lhs` must be JSON or TEXT
/// containing JSON. `rhs` is either a TEXT key (object access) or
/// an INT index (array access). `as_text=true` for `->>` (returns
/// `Value::Text`); `false` for `->` (returns `Value::Json`).
pub fn path_get(lhs: &Value, rhs: &Value, as_text: bool) -> Result<Value, EvalError> {
    let src = match lhs {
        Value::Json(s) | Value::Text(s) => s.as_str(),
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "JSON path operator: left side must be JSON or TEXT, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let doc = parse(src).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("invalid JSON for path access: {e}"),
    })?;
    let inner = match (&doc, rhs) {
        (JsonValue::Object(entries), Value::Text(k)) => entries
            .iter()
            .find(|(name, _)| name == k)
            .map(|(_, v)| v.clone()),
        (JsonValue::Array(items), Value::Int(idx)) => {
            let n = *idx;
            if n >= 0 {
                items.get(n as usize).cloned()
            } else {
                let from_end = items.len() as i64 + i64::from(n);
                if from_end >= 0 {
                    items.get(from_end as usize).cloned()
                } else {
                    None
                }
            }
        }
        (JsonValue::Array(items), Value::BigInt(idx)) => {
            let n = *idx;
            if n >= 0 {
                items.get(n as usize).cloned()
            } else {
                let from_end = items.len() as i64 + n;
                if from_end >= 0 {
                    items.get(from_end as usize).cloned()
                } else {
                    None
                }
            }
        }
        (_, Value::Null) => return Ok(Value::Null),
        _ => None,
    };
    match inner {
        None | Some(JsonValue::Null) => Ok(Value::Null),
        Some(v) => {
            if as_text {
                Ok(Value::Text(v.as_text()))
            } else {
                Ok(Value::Json(v.to_json_text()))
            }
        }
    }
}

// ---- Tiny recursive-descent JSON parser ----

#[derive(Debug)]
pub enum ParseError {
    Unexpected(char, usize),
    Truncated,
    InvalidEscape(usize),
    InvalidNumber(usize),
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unexpected(c, p) => write!(f, "unexpected {c:?} at offset {p}"),
            Self::Truncated => f.write_str("unexpected end of JSON input"),
            Self::InvalidEscape(p) => write!(f, "invalid string escape at offset {p}"),
            Self::InvalidNumber(p) => write!(f, "invalid number at offset {p}"),
        }
    }
}

pub fn parse(src: &str) -> Result<JsonValue, ParseError> {
    let bytes = src.as_bytes();
    let mut p = 0;
    skip_ws(bytes, &mut p);
    let value = parse_value(bytes, &mut p)?;
    skip_ws(bytes, &mut p);
    if p != bytes.len() {
        return Err(ParseError::Unexpected(bytes[p] as char, p));
    }
    Ok(value)
}

fn skip_ws(bytes: &[u8], p: &mut usize) {
    while *p < bytes.len() && matches!(bytes[*p], b' ' | b'\t' | b'\n' | b'\r') {
        *p += 1;
    }
}

fn parse_value(bytes: &[u8], p: &mut usize) -> Result<JsonValue, ParseError> {
    skip_ws(bytes, p);
    if *p >= bytes.len() {
        return Err(ParseError::Truncated);
    }
    match bytes[*p] {
        b'{' => parse_object(bytes, p),
        b'[' => parse_array(bytes, p),
        b'"' => parse_string(bytes, p).map(JsonValue::String),
        b't' | b'f' => parse_bool(bytes, p),
        b'n' => parse_null(bytes, p),
        b'-' | b'0'..=b'9' => parse_number(bytes, p),
        c => Err(ParseError::Unexpected(c as char, *p)),
    }
}

fn parse_object(bytes: &[u8], p: &mut usize) -> Result<JsonValue, ParseError> {
    debug_assert_eq!(bytes[*p], b'{');
    *p += 1;
    let mut entries = Vec::new();
    skip_ws(bytes, p);
    if *p < bytes.len() && bytes[*p] == b'}' {
        *p += 1;
        return Ok(JsonValue::Object(entries));
    }
    loop {
        skip_ws(bytes, p);
        if *p >= bytes.len() || bytes[*p] != b'"' {
            return Err(ParseError::Unexpected(
                bytes.get(*p).copied().unwrap_or(0) as char,
                *p,
            ));
        }
        let key = parse_string(bytes, p)?;
        skip_ws(bytes, p);
        if *p >= bytes.len() || bytes[*p] != b':' {
            return Err(ParseError::Unexpected(
                bytes.get(*p).copied().unwrap_or(0) as char,
                *p,
            ));
        }
        *p += 1;
        let value = parse_value(bytes, p)?;
        entries.push((key, value));
        skip_ws(bytes, p);
        if *p >= bytes.len() {
            return Err(ParseError::Truncated);
        }
        match bytes[*p] {
            b',' => {
                *p += 1;
                continue;
            }
            b'}' => {
                *p += 1;
                return Ok(JsonValue::Object(entries));
            }
            c => return Err(ParseError::Unexpected(c as char, *p)),
        }
    }
}

fn parse_array(bytes: &[u8], p: &mut usize) -> Result<JsonValue, ParseError> {
    debug_assert_eq!(bytes[*p], b'[');
    *p += 1;
    let mut items = Vec::new();
    skip_ws(bytes, p);
    if *p < bytes.len() && bytes[*p] == b']' {
        *p += 1;
        return Ok(JsonValue::Array(items));
    }
    loop {
        items.push(parse_value(bytes, p)?);
        skip_ws(bytes, p);
        if *p >= bytes.len() {
            return Err(ParseError::Truncated);
        }
        match bytes[*p] {
            b',' => {
                *p += 1;
                continue;
            }
            b']' => {
                *p += 1;
                return Ok(JsonValue::Array(items));
            }
            c => return Err(ParseError::Unexpected(c as char, *p)),
        }
    }
}

fn parse_string(bytes: &[u8], p: &mut usize) -> Result<String, ParseError> {
    debug_assert_eq!(bytes[*p], b'"');
    *p += 1;
    let mut out = String::new();
    while *p < bytes.len() {
        match bytes[*p] {
            b'"' => {
                *p += 1;
                return Ok(out);
            }
            b'\\' => {
                let start = *p;
                *p += 1;
                if *p >= bytes.len() {
                    return Err(ParseError::Truncated);
                }
                match bytes[*p] {
                    b'"' => {
                        out.push('"');
                        *p += 1;
                    }
                    b'\\' => {
                        out.push('\\');
                        *p += 1;
                    }
                    b'/' => {
                        out.push('/');
                        *p += 1;
                    }
                    b'b' => {
                        out.push('\u{08}');
                        *p += 1;
                    }
                    b'f' => {
                        out.push('\u{0c}');
                        *p += 1;
                    }
                    b'n' => {
                        out.push('\n');
                        *p += 1;
                    }
                    b'r' => {
                        out.push('\r');
                        *p += 1;
                    }
                    b't' => {
                        out.push('\t');
                        *p += 1;
                    }
                    b'u' => {
                        if *p + 5 > bytes.len() {
                            return Err(ParseError::Truncated);
                        }
                        let hex = &bytes[*p + 1..*p + 5];
                        let n = u32::from_str_radix(
                            core::str::from_utf8(hex)
                                .map_err(|_| ParseError::InvalidEscape(start))?,
                            16,
                        )
                        .map_err(|_| ParseError::InvalidEscape(start))?;
                        out.push(char::from_u32(n).ok_or(ParseError::InvalidEscape(start))?);
                        *p += 5;
                    }
                    _ => return Err(ParseError::InvalidEscape(start)),
                }
            }
            c if c < 0x20 => return Err(ParseError::Unexpected(c as char, *p)),
            _ => {
                // Multi-byte UTF-8: consume the whole codepoint.
                let s = core::str::from_utf8(&bytes[*p..])
                    .map_err(|_| ParseError::Unexpected(bytes[*p] as char, *p))?;
                let c = s.chars().next().unwrap();
                out.push(c);
                *p += c.len_utf8();
            }
        }
    }
    Err(ParseError::Truncated)
}

fn parse_bool(bytes: &[u8], p: &mut usize) -> Result<JsonValue, ParseError> {
    if bytes[*p..].starts_with(b"true") {
        *p += 4;
        Ok(JsonValue::Bool(true))
    } else if bytes[*p..].starts_with(b"false") {
        *p += 5;
        Ok(JsonValue::Bool(false))
    } else {
        Err(ParseError::Unexpected(bytes[*p] as char, *p))
    }
}

fn parse_null(bytes: &[u8], p: &mut usize) -> Result<JsonValue, ParseError> {
    if bytes[*p..].starts_with(b"null") {
        *p += 4;
        Ok(JsonValue::Null)
    } else {
        Err(ParseError::Unexpected(bytes[*p] as char, *p))
    }
}

fn parse_number(bytes: &[u8], p: &mut usize) -> Result<JsonValue, ParseError> {
    let start = *p;
    if bytes[*p] == b'-' {
        *p += 1;
    }
    while *p < bytes.len() && bytes[*p].is_ascii_digit() {
        *p += 1;
    }
    if *p < bytes.len() && bytes[*p] == b'.' {
        *p += 1;
        while *p < bytes.len() && bytes[*p].is_ascii_digit() {
            *p += 1;
        }
    }
    if *p < bytes.len() && matches!(bytes[*p], b'e' | b'E') {
        *p += 1;
        if *p < bytes.len() && matches!(bytes[*p], b'+' | b'-') {
            *p += 1;
        }
        while *p < bytes.len() && bytes[*p].is_ascii_digit() {
            *p += 1;
        }
    }
    let text = core::str::from_utf8(&bytes[start..*p])
        .map_err(|_| ParseError::InvalidNumber(start))?
        .to_string();
    // Validate the parse so the wire side can trust the value.
    if text.parse::<f64>().is_err() {
        return Err(ParseError::InvalidNumber(start));
    }
    Ok(JsonValue::NumberText(text))
}

// ─── v7.17.0 Phase 3.9 — minimal JSONPath subset for jsonb_path_query ───
//
// Supported path syntax (PG-flavoured JSONPath subset):
//   * `$` — document root (required leading segment)
//   * `.field` — object field access (bare ident only; quoted form
//                `."field with space"` accepted)
//   * `[N]` — array index (non-negative integer; negative indices
//             out of v7.17 scope)
//   * `[*]` — array wildcard (fan-out — each element matched separately)
//   * Chained: `$.a.b[0].c[*].name`
//
// NOT supported (errors clearly):
//   * Filter expressions `? (@.price > 100)`
//   * Range slices `[1:3]`
//   * Recursive descent `..field`
//   * Functions `keyvalue()`, `size()`, etc.
//   * Path variables `$varname`

#[derive(Debug, Clone)]
enum PathStep {
    Field(String),
    Index(usize),
    Wildcard,
}

fn parse_jsonpath(p: &str) -> Result<Vec<PathStep>, EvalError> {
    let chars: Vec<char> = p.chars().collect();
    let mut i = 0;
    if i >= chars.len() || chars[i] != '$' {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("jsonpath must start with '$', got {p:?}"),
        });
    }
    i += 1;
    let mut steps: Vec<PathStep> = Vec::new();
    while i < chars.len() {
        match chars[i] {
            '.' => {
                i += 1;
                if i < chars.len() && chars[i] == '"' {
                    i += 1;
                    let start = i;
                    while i < chars.len() && chars[i] != '"' {
                        i += 1;
                    }
                    if i >= chars.len() {
                        return Err(EvalError::TypeMismatch {
                            detail: "jsonpath: unterminated quoted field".into(),
                        });
                    }
                    steps.push(PathStep::Field(chars[start..i].iter().collect()));
                    i += 1;
                } else {
                    let start = i;
                    while i < chars.len()
                        && chars[i] != '.'
                        && chars[i] != '['
                        && !chars[i].is_whitespace()
                    {
                        i += 1;
                    }
                    if start == i {
                        return Err(EvalError::TypeMismatch {
                            detail: "jsonpath: missing field name after '.'".into(),
                        });
                    }
                    steps.push(PathStep::Field(chars[start..i].iter().collect()));
                }
            }
            '[' => {
                i += 1;
                if i < chars.len() && chars[i] == '*' {
                    i += 1;
                    if i >= chars.len() || chars[i] != ']' {
                        return Err(EvalError::TypeMismatch {
                            detail: "jsonpath: expected ']' after '[*'".into(),
                        });
                    }
                    i += 1;
                    steps.push(PathStep::Wildcard);
                } else {
                    let start = i;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                    if start == i {
                        return Err(EvalError::TypeMismatch {
                            detail: "jsonpath: only `[N]` (non-negative) or `[*]` supported in v7.17".into(),
                        });
                    }
                    let idx: usize = chars[start..i]
                        .iter()
                        .collect::<String>()
                        .parse()
                        .map_err(|_| EvalError::TypeMismatch {
                            detail: "jsonpath: invalid array index".into(),
                        })?;
                    if i >= chars.len() || chars[i] != ']' {
                        return Err(EvalError::TypeMismatch {
                            detail: "jsonpath: expected ']' after array index".into(),
                        });
                    }
                    i += 1;
                    steps.push(PathStep::Index(idx));
                }
            }
            c if c.is_whitespace() => {
                i += 1;
            }
            c => {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "jsonpath: unexpected char '{c}' (v7.17 supports `$.field`, `[N]`, `[*]` only)"
                    ),
                });
            }
        }
    }
    Ok(steps)
}

fn apply_jsonpath(root: &JsonValue, steps: &[PathStep]) -> Vec<JsonValue> {
    let mut cur: Vec<JsonValue> = alloc::vec![root.clone()];
    for step in steps {
        let mut next: Vec<JsonValue> = Vec::new();
        for node in &cur {
            match (step, node) {
                (PathStep::Field(k), JsonValue::Object(entries)) => {
                    if let Some((_, v)) = entries.iter().find(|(name, _)| name == k) {
                        next.push(v.clone());
                    }
                }
                (PathStep::Index(idx), JsonValue::Array(items)) => {
                    if let Some(v) = items.get(*idx) {
                        next.push(v.clone());
                    }
                }
                (PathStep::Wildcard, JsonValue::Array(items)) => {
                    next.extend(items.iter().cloned());
                }
                _ => {} // no match at this branch
            }
        }
        cur = next;
        if cur.is_empty() {
            return Vec::new();
        }
    }
    cur
}

/// v7.17.0 Phase 3.9 — `jsonb_path_query(doc, path)` — returns the
/// matched JSON values as a TextArray (each element is the JSON
/// encoding of one match).
pub fn path_query(doc: &Value, path: &Value) -> Result<Value, EvalError> {
    let (src, path_text) = match (doc, path) {
        (Value::Null, _) | (_, Value::Null) => return Ok(Value::Null),
        (Value::Json(s) | Value::Text(s), Value::Text(p) | Value::Json(p)) => (s, p),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: "jsonb_path_query() expects (JSON, TEXT)".into(),
            });
        }
    };
    let root = parse(src).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("invalid JSON for jsonb_path_query: {e}"),
    })?;
    let steps = parse_jsonpath(path_text)?;
    let matches = apply_jsonpath(&root, &steps);
    let arr: Vec<Option<String>> = matches
        .into_iter()
        .map(|v| Some(v.to_json_text()))
        .collect();
    Ok(Value::TextArray(arr))
}

/// v7.17.0 Phase 3.9 — `jsonb_path_query_first(doc, path)` returns
/// the first matched JSON value as a Json, or NULL on no match.
pub fn path_query_first(doc: &Value, path: &Value) -> Result<Value, EvalError> {
    let q = path_query(doc, path)?;
    match q {
        Value::TextArray(items) => {
            if let Some(Some(first)) = items.into_iter().next() {
                Ok(Value::Json(first))
            } else {
                Ok(Value::Null)
            }
        }
        other => Ok(other),
    }
}

/// v7.17.0 Phase 3.9 — `jsonb_path_query_array(doc, path)` returns
/// the matched values wrapped as a single JSON array.
pub fn path_query_array(doc: &Value, path: &Value) -> Result<Value, EvalError> {
    let q = path_query(doc, path)?;
    match q {
        Value::TextArray(items) => {
            let mut buf = String::from("[");
            let mut first = true;
            for it in items {
                if let Some(s) = it {
                    if !first {
                        buf.push(',');
                    }
                    buf.push_str(&s);
                    first = false;
                }
            }
            buf.push(']');
            Ok(Value::Json(buf))
        }
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_atoms() {
        assert_eq!(parse("null").unwrap(), JsonValue::Null);
        assert_eq!(parse("true").unwrap(), JsonValue::Bool(true));
        assert_eq!(parse("false").unwrap(), JsonValue::Bool(false));
        assert_eq!(
            parse("\"hello\"").unwrap(),
            JsonValue::String("hello".into())
        );
        assert!(matches!(
            parse("42").unwrap(),
            JsonValue::NumberText(ref s) if s == "42"
        ));
    }

    #[test]
    fn parse_nested() {
        let doc = parse(r#"{"a":1,"b":[true,null,"x"]}"#).unwrap();
        let JsonValue::Object(entries) = doc else {
            panic!("expected object");
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "a");
        assert_eq!(entries[1].0, "b");
    }

    #[test]
    fn parse_string_escapes() {
        let s = parse(r#""he said \"hi\" and\\then\n""#).unwrap();
        assert_eq!(s, JsonValue::String("he said \"hi\" and\\then\n".into()));
    }

    #[test]
    fn parse_unicode_escape() {
        assert_eq!(parse(r#""é""#).unwrap(), JsonValue::String("é".into()));
    }

    #[test]
    fn path_object_key_returns_value() {
        let doc = Value::Json(r#"{"name":"alice","age":30}"#.into());
        let key = Value::Text("name".into());
        let v = path_get(&doc, &key, true).unwrap();
        assert_eq!(v, Value::Text("alice".into()));
        let v = path_get(&doc, &key, false).unwrap();
        assert_eq!(v, Value::Json("\"alice\"".into()));
    }

    #[test]
    fn path_array_index_supports_negative() {
        let doc = Value::Json("[10,20,30]".into());
        let v = path_get(&doc, &Value::Int(1), true).unwrap();
        assert_eq!(v, Value::Text("20".into()));
        let v = path_get(&doc, &Value::Int(-1), true).unwrap();
        assert_eq!(v, Value::Text("30".into()));
    }

    #[test]
    fn path_missing_key_returns_null() {
        let doc = Value::Json(r#"{"a":1}"#.into());
        let v = path_get(&doc, &Value::Text("missing".into()), true).unwrap();
        assert_eq!(v, Value::Null);
    }

    #[test]
    fn path_get_nested_subtree_renders_back() {
        let doc = Value::Json(r#"{"k":{"x":[1,2]}}"#.into());
        let v = path_get(&doc, &Value::Text("k".into()), false).unwrap();
        assert_eq!(v, Value::Json("{\"x\":[1,2]}".into()));
    }
}
