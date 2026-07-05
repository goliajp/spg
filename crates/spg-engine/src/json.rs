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
                write_json_string(k, out);
                out.push(':');
                write_json(val, out);
            }
            out.push('}');
        }
    }
}

/// Escape a string into a JSON string literal (shared by the verbatim
/// and canonical serializers). Matches PG: `\n \r \t \" \\`, control
/// chars as `\uXXXX`, everything else (incl. non-ASCII) verbatim UTF-8.
fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&alloc::format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Canonicalise a jsonb text value the way PostgreSQL does on input:
/// object keys sorted by (length, then bytewise) with duplicate keys
/// collapsed last-wins, `, ` / `: ` whitespace, and numbers normalised
/// to plain decimal (exponents expanded, `-0` → `0`, but trailing zeros
/// from the input scale preserved — `1e2` → `100`, `1E-3` → `0.001`,
/// `1.10` stays `1.10`). `json` keeps its input verbatim; only `jsonb`
/// runs through this.
pub fn canonicalize_jsonb(src: &str) -> Result<String, ParseError> {
    let v = parse(src)?;
    let mut out = String::new();
    write_json_canonical(&v, &mut out);
    Ok(out)
}

/// Canonicalise a `Value::Json` payload (a jsonb-typed result); any
/// other value passes through untouched. Used to bring jsonb builder /
/// mutator functions (`jsonb_build_object`, `to_jsonb`, `jsonb_set`, the
/// `||` / `-` / `#-` operators, …) in line with PG, which always emits
/// canonical jsonb from them. The `json_*` siblings stay verbatim.
#[must_use]
pub fn canonicalize_value(v: Value<'static>) -> Value<'static> {
    match v {
        Value::Json(s) => {
            Value::json(canonicalize_jsonb(s.as_ref()).unwrap_or_else(|_| s.into_owned()))
        }
        other => other,
    }
}

/// Render a sub-value extracted by the `->` / `#>` (jsonb) and `->>` /
/// `#>>` (text) accessors. Containers are serialised canonically (PG
/// re-emits the extracted jsonb in canonical form); a scalar under
/// `as_text` returns its raw value — already canonical, since the source
/// jsonb was canonicalised on input.
fn accessor_result(v: &JsonValue, as_text: bool) -> Value<'static> {
    if as_text && !matches!(v, JsonValue::Array(_) | JsonValue::Object(_)) {
        return Value::text(v.as_text());
    }
    let mut s = String::new();
    write_json_canonical(v, &mut s);
    if as_text {
        Value::text(s)
    } else {
        Value::json(s)
    }
}

fn write_json_canonical(v: &JsonValue, out: &mut String) {
    match v {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        JsonValue::Number(x) => out.push_str(&canon_json_number(&alloc::format!("{x}"))),
        JsonValue::NumberText(s) => out.push_str(&canon_json_number(s)),
        JsonValue::String(s) => write_json_string(s, out),
        JsonValue::Array(items) => {
            out.push('[');
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_json_canonical(it, out);
            }
            out.push(']');
        }
        JsonValue::Object(entries) => {
            // Duplicate keys collapse last-wins (keep the final value),
            // preserving first-seen order only until the stable sort.
            let mut deduped: Vec<(&String, &JsonValue)> = Vec::new();
            for (k, val) in entries {
                if let Some(slot) = deduped.iter_mut().find(|(mk, _)| *mk == k) {
                    slot.1 = val;
                } else {
                    deduped.push((k, val));
                }
            }
            deduped.sort_by(|a, b| {
                a.0.len()
                    .cmp(&b.0.len())
                    .then_with(|| a.0.as_bytes().cmp(b.0.as_bytes()))
            });
            out.push('{');
            for (i, (k, val)) in deduped.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_json_string(k, out);
                out.push_str(": ");
                write_json_canonical(val, out);
            }
            out.push('}');
        }
    }
}

/// Render a JSON number lexeme in PostgreSQL's canonical jsonb form: a
/// plain decimal with the exponent applied, `-0` normalised to `0`, and
/// the input's fractional scale preserved. Digits are manipulated as
/// strings so arbitrarily large numbers round-trip without overflow.
fn canon_json_number(lexeme: &str) -> String {
    let neg = lexeme.starts_with('-');
    let body = lexeme.trim_start_matches(['-', '+']);
    // Split into mantissa (int '.' frac) and exponent.
    let (mantissa, exp) = match body.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i64>().unwrap_or(0)),
        None => (body, 0),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };
    let digits: String = alloc::format!("{int_part}{frac_part}");
    // `shift` = number of fractional digits in the output. Applying the
    // exponent moves the point right by `exp`, i.e. reduces the fraction
    // count by `exp`.
    let shift = frac_part.len() as i64 - exp;
    let all_zero = digits.bytes().all(|b| b == b'0');
    let sign = if neg && !all_zero { "-" } else { "" };
    let strip = |s: &str| -> String {
        let t = s.trim_start_matches('0');
        if t.is_empty() { "0".into() } else { t.into() }
    };
    if shift <= 0 {
        // Integer: append `-shift` trailing zeros.
        let zeros = "0".repeat((-shift) as usize);
        alloc::format!("{sign}{}{zeros}", strip(&digits))
    } else {
        let shift = shift as usize;
        let (int_str, frac_str) = if digits.len() > shift {
            (digits[..digits.len() - shift].to_string(), digits[digits.len() - shift..].to_string())
        } else {
            ("0".to_string(), alloc::format!("{}{}", "0".repeat(shift - digits.len()), digits))
        };
        alloc::format!("{sign}{}.{frac_str}", strip(&int_str))
    }
}

/// v6.4.5 — PG `json #> path_text` / `json #>> path_text`. The
/// right-hand side is a PG text-array literal `'{a,0,b}'` whose
/// elements are walked left-to-right; each element is either an
/// object key or (when it parses as a non-negative integer) an
/// v7.37.43-T4.5 — set-returning function `jsonb_each_text(jsonb)`.
/// PG semantics: for each (key, value) pair in the object, emit one
/// row whose `key` column is the literal key and `value` column is
/// the JSON value rendered as text (`null` → SQL NULL, primitives →
/// their lexeme, nested objects/arrays → JSON text).
///
/// Returns the (key, value) tuples as a Vec ready for FROM-clause
/// materialisation. Non-object inputs raise an error (PG's actual
/// behaviour); `NULL` and empty object both produce 0 rows.
pub fn jsonb_each_text_rows(arg: &Value) -> Result<Vec<(String, Option<String>)>, EvalError> {
    each_rows(arg, true, "jsonb_each_text")
}

/// v7.37.17 (17.6 siblings) — shared body for the four `each` SRFs.
/// `as_text` (the `*_each_text` forms) unwraps scalar values to
/// their lexeme and maps JSON null → SQL NULL; the plain forms
/// render every value (including JSON null) as compact JSON text,
/// which the executor wraps as a jsonb-typed column.
pub fn each_rows(
    arg: &Value,
    as_text: bool,
    fn_name: &str,
) -> Result<Vec<(String, Option<String>)>, EvalError> {
    let src = match arg {
        Value::Null => return Ok(Vec::new()),
        Value::Json(s) | Value::Text(s) => s.as_ref(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "{fn_name}: argument must be JSON / JSONB, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let parsed = parse(src).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("{fn_name}: invalid JSON: {e}"),
    })?;
    match parsed {
        JsonValue::Object(entries) => {
            let mut out: Vec<(String, Option<String>)> = Vec::with_capacity(entries.len());
            for (k, v) in entries {
                let text = if as_text {
                    match &v {
                        JsonValue::Null => None,
                        JsonValue::Bool(b) => Some(if *b {
                            "true".to_string()
                        } else {
                            "false".to_string()
                        }),
                        JsonValue::Number(_)
                        | JsonValue::NumberText(_)
                        | JsonValue::String(_) => Some(v.as_text()),
                        JsonValue::Array(_) | JsonValue::Object(_) => {
                            Some(v.to_json_text())
                        }
                    }
                } else {
                    Some(v.to_json_text())
                };
                out.push((k, text));
            }
            Ok(out)
        }
        other => Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "cannot call {fn_name} on a non-object ({other:?})"
            ),
        }),
    }
}

/// v7.37.17 (17.6 siblings) — set-returning function
/// `jsonb_array_elements[_text](json)`. PG semantics: one row per
/// array element. `_text` renders scalars as their lexeme and JSON
/// null as SQL NULL; the plain form renders every element (including
/// JSON null) as compact JSON text. Non-array inputs raise an error
/// (PG's actual behaviour); SQL NULL produces 0 rows.
///
/// Returns the element texts as a Vec ready for FROM-clause
/// materialisation (via the unnest rewrite in the parser).
pub fn array_element_rows(
    arg: &Value,
    as_text: bool,
    fn_name: &str,
) -> Result<Vec<Option<String>>, EvalError> {
    let src = match arg {
        Value::Null => return Ok(Vec::new()),
        Value::Json(s) | Value::Text(s) => s.as_ref(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "{fn_name}: argument must be JSON / JSONB, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let parsed = parse(src).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("{fn_name}: invalid JSON: {e}"),
    })?;
    match parsed {
        JsonValue::Array(items) => {
            let mut out: Vec<Option<String>> = Vec::with_capacity(items.len());
            for v in items {
                let text = if as_text {
                    match &v {
                        JsonValue::Null => None,
                        JsonValue::Bool(b) => Some(if *b {
                            "true".to_string()
                        } else {
                            "false".to_string()
                        }),
                        JsonValue::Number(_)
                        | JsonValue::NumberText(_)
                        | JsonValue::String(_) => Some(v.as_text()),
                        JsonValue::Array(_) | JsonValue::Object(_) => {
                            Some(v.to_json_text())
                        }
                    }
                } else {
                    Some(v.to_json_text())
                };
                out.push(text);
            }
            Ok(out)
        }
        other => Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "cannot extract elements from a non-array ({fn_name}: got {other:?})"
            ),
        }),
    }
}

/// array index. Missing or non-existent steps return `Value::Null`.
pub fn path_walk(lhs: &Value, rhs: &Value, as_text: bool) -> Result<Value<'static>, EvalError> {
    let src = match lhs {
        Value::Json(s) | Value::Text(s) => s.as_ref(),
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
        Value::Text(s) | Value::Json(s) => s.as_ref(),
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
    Ok(accessor_result(&cur, as_text))
}

/// v6.4.5 — PG `json @> sub_json` containment. Returns BOOL.
/// `lhs @> rhs` is true when every member of `rhs` is structurally
/// contained in `lhs`:
///   - Scalars: equal
///   - Objects: every (key, value) in rhs exists in lhs with a
///     containing value
///   - Arrays: every element in rhs has a containing element in lhs
/// v7.37.6-A — PG `jsonb ? text`. Returns BOOL: true iff the key
/// exists at the top level of the document.
///   - Object: true iff `key` is a member name.
///   - Array:  true iff any element is exactly the JSON string `key`.
///   - Scalar string: true iff the scalar equals `key`.
///   - Other scalars / null: false.
/// NULL on either side → NULL (SQL 3VL).
pub fn key_exists(lhs: &Value, rhs: &Value) -> Result<Value<'static>, EvalError> {
    let lhs_text = match lhs {
        Value::Json(s) | Value::Text(s) => s.as_ref(),
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "JSON ?: left side must be JSON or TEXT, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let key = match rhs {
        Value::Text(s) => s.as_ref(),
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "JSON ?: right side must be TEXT, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let doc = parse(lhs_text).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("invalid JSON on left of ?: {e}"),
    })?;
    Ok(Value::Bool(node_has_key(&doc, key)))
}

fn node_has_key(v: &JsonValue, key: &str) -> bool {
    match v {
        JsonValue::Object(members) => members.iter().any(|(k, _)| k == key),
        JsonValue::Array(items) => items
            .iter()
            .any(|item| matches!(item, JsonValue::String(s) if s == key)),
        JsonValue::String(s) => s == key,
        _ => false,
    }
}

/// Helper for `?|` / `?&` — extract a Vec of keys from either a
/// TEXT[] Value or a single TEXT Value (PG accepts both).
fn collect_keys(v: &Value) -> Result<Option<Vec<String>>, EvalError> {
    match v {
        Value::Null => Ok(None),
        Value::TextArray(items) => Ok(Some(items.iter().filter_map(|x| x.clone()).collect())),
        Value::Text(s) => Ok(Some(alloc::vec![s.to_string()])),
        other => Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "JSON ?|/?&: right side must be TEXT[] or TEXT, got {:?}",
                other.data_type()
            ),
        }),
    }
}

/// v7.37.6-A — PG `jsonb ?| text[]`. Returns BOOL: true iff any one
/// of the listed keys exists at the top level.
pub fn keys_any(lhs: &Value, rhs: &Value) -> Result<Value<'static>, EvalError> {
    let lhs_text = match lhs {
        Value::Json(s) | Value::Text(s) => s.as_ref(),
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "JSON ?|: left side must be JSON or TEXT, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let Some(keys) = collect_keys(rhs)? else {
        return Ok(Value::Null);
    };
    let doc = parse(lhs_text).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("invalid JSON on left of ?|: {e}"),
    })?;
    Ok(Value::Bool(keys.iter().any(|k| node_has_key(&doc, k))))
}

/// v7.37.6-A — PG `jsonb ?& text[]`. Returns BOOL: true iff every
/// one of the listed keys exists at the top level.
pub fn keys_all(lhs: &Value, rhs: &Value) -> Result<Value<'static>, EvalError> {
    let lhs_text = match lhs {
        Value::Json(s) | Value::Text(s) => s.as_ref(),
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "JSON ?&: left side must be JSON or TEXT, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let Some(keys) = collect_keys(rhs)? else {
        return Ok(Value::Null);
    };
    let doc = parse(lhs_text).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("invalid JSON on left of ?&: {e}"),
    })?;
    Ok(Value::Bool(keys.iter().all(|k| node_has_key(&doc, k))))
}

pub fn contains(lhs: &Value, rhs: &Value) -> Result<Value<'static>, EvalError> {
    let lhs_text = match lhs {
        Value::Json(s) | Value::Text(s) => s.as_ref(),
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
        Value::Json(s) | Value::Text(s) => s.as_ref(),
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
    // PG special case: a top-level array `@>` a non-array scalar is
    // true when the scalar equals any element (flat equality). This
    // applies ONLY at the top level — inside array/array containment
    // PG still requires a scalar RHS element to match a *scalar* LHS
    // element, so it must NOT be folded into `json_contains`'s
    // recursion (`'[1,[2,3]]' @> '[2,3]'` stays false).
    let result = match (&lhs_doc, &rhs_doc) {
        (JsonValue::Array(items), scalar)
            if !matches!(scalar, JsonValue::Array(_) | JsonValue::Object(_)) =>
        {
            items.iter().any(|it| json_eq(it, scalar))
        }
        _ => json_contains(&lhs_doc, &rhs_doc),
    };
    Ok(Value::Bool(result))
}

/// `jsonb = jsonb` structural equality (PG18-compatible). PG's jsonb
/// equality is order-INDEPENDENT for object keys but order-SENSITIVE
/// for array elements, and it compares numbers by value (so
/// `'1'::jsonb = '1.0'::jsonb` is true). `json_eq` encodes those rules;
/// this parses both operands and delegates. Values reaching here through
/// the `::jsonb` cast / a jsonb column are already canonicalised (keys
/// sorted, duplicates collapsed), so object equality is exact.
pub fn equals(lhs: &Value, rhs: &Value) -> Result<bool, EvalError> {
    let lhs_text = match lhs {
        Value::Json(s) | Value::Text(s) => s.as_ref(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "jsonb =: left side must be JSON or TEXT, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let rhs_text = match rhs {
        Value::Json(s) | Value::Text(s) => s.as_ref(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "jsonb =: right side must be JSON or TEXT, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let lhs_doc = parse(lhs_text).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("invalid JSON on left of =: {e}"),
    })?;
    let rhs_doc = parse(rhs_text).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("invalid JSON on right of =: {e}"),
    })?;
    Ok(json_eq(&lhs_doc, &rhs_doc))
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
        // PG compares jsonb numbers by value, not by lexeme, so
        // `1` == `1.0` == `1e0` and `1.50` == `1.5`. Normalise both to
        // an exact numeric-equality key (canonical decimal with trailing
        // zeros stripped) rather than a lossy f64 subtraction.
        (
            JsonValue::Number(_) | JsonValue::NumberText(_),
            JsonValue::Number(_) | JsonValue::NumberText(_),
        ) => json_number_key(a) == json_number_key(b),
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

/// Normalise a JSON number to a key where numerically-equal values share
/// one string (`1` / `1.0` / `1e0` → `1`, `1.50` → `1.5`), so jsonb `=`
/// and containment compare numbers by value like PG — exactly, without
/// f64 rounding.
fn numeric_eq_key(lexeme: &str) -> String {
    let c = canon_json_number(lexeme);
    if c.contains('.') {
        c.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        c
    }
}

fn json_number_key(v: &JsonValue) -> Option<String> {
    match v {
        JsonValue::NumberText(s) => Some(numeric_eq_key(s)),
        JsonValue::Number(x) => Some(numeric_eq_key(&alloc::format!("{x}"))),
        _ => None,
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
pub fn path_get(lhs: &Value, rhs: &Value, as_text: bool) -> Result<Value<'static>, EvalError> {
    let src = match lhs {
        Value::Json(s) | Value::Text(s) => s.as_ref(),
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
        Some(v) => Ok(accessor_result(&v, as_text)),
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
                            detail:
                                "jsonpath: only `[N]` (non-negative) or `[*]` supported in v7.17"
                                    .into(),
                        });
                    }
                    let idx: usize =
                        chars[start..i]
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
pub fn path_query(doc: &Value, path: &Value) -> Result<Value<'static>, EvalError> {
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
pub fn path_query_first(doc: &Value, path: &Value) -> Result<Value<'static>, EvalError> {
    let q = path_query(doc, path)?;
    match q {
        Value::TextArray(items) => {
            if let Some(Some(first)) = items.into_iter().next() {
                Ok(Value::json(first))
            } else {
                Ok(Value::Null)
            }
        }
        other => Ok(other),
    }
}

/// v7.17.0 Phase 3.9 — `jsonb_path_query_array(doc, path)` returns
/// the matched values wrapped as a single JSON array.
pub fn path_query_array(doc: &Value, path: &Value) -> Result<Value<'static>, EvalError> {
    let q = path_query(doc, path)?;
    match q {
        Value::TextArray(items) => {
            let mut buf = String::from("[");
            let mut first = true;
            for s in items.into_iter().flatten() {
                if !first {
                    buf.push(',');
                }
                buf.push_str(&s);
                first = false;
            }
            buf.push(']');
            Ok(Value::json(buf))
        }
        other => Ok(other),
    }
}

// ─── v7.17.0 Phase 3.P0-28 — JSON builder family ───────────────
//
// Surface: to_json / to_jsonb, json_build_object / jsonb_build_object,
// json_build_array / jsonb_build_array, jsonb_set, jsonb_insert.
//
// PG `json` vs `jsonb` differ in storage shape only — both surface
// as Value::Json textually. The pair just shares an implementation.

/// Encode a Value as its canonical JSON text (no surrounding quotes
/// for non-strings). Used by every builder below.
///
/// Rules:
///   * NULL → "null" (json literal; NOT SQL NULL).
///   * BOOL → "true" / "false".
///   * Numbers → bare decimal text (BigInt prints exact 64-bit form).
///   * Text → quoted+escaped JSON string.
///   * Json/Jsonb → pass-through (assumed valid; parser is forgiving).
///   * Arrays → "[..,..]" with element-wise encoding.
///   * Bytes / Date / Timestamp / Uuid / Numeric → quoted textual
///     form via Display; PG canonical text shape.
pub fn value_to_json_text(v: &Value) -> String {
    let mut out = String::new();
    encode_value_into(v, &mut out);
    out
}

fn encode_value_into(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::SmallInt(n) => out.push_str(&alloc::format!("{n}")),
        Value::Int(n) => out.push_str(&alloc::format!("{n}")),
        Value::BigInt(n) => out.push_str(&alloc::format!("{n}")),
        Value::Float(x) => out.push_str(&alloc::format!("{x}")),
        Value::Numeric { scaled, scale } => {
            // Render the exact decimal text — same shape display uses.
            out.push_str(&render_numeric(*scaled, *scale));
        }
        Value::Text(s) => write_json(&JsonValue::String(s.to_string()), out),
        Value::Json(s) => {
            // Pass through verbatim; re-parsing would re-format and
            // drift `1.0` → `1` etc. PG's to_json on a json input is
            // identity.
            out.push_str(s);
        }
        Value::TextArray(items) => {
            out.push('[');
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                match it {
                    Some(s) => write_json(&JsonValue::String(s.clone()), out),
                    None => out.push_str("null"),
                }
            }
            out.push(']');
        }
        Value::IntArray(items) => {
            out.push('[');
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                match it {
                    Some(n) => out.push_str(&alloc::format!("{n}")),
                    None => out.push_str("null"),
                }
            }
            out.push(']');
        }
        Value::BigIntArray(items) => {
            out.push('[');
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                match it {
                    Some(n) => out.push_str(&alloc::format!("{n}")),
                    None => out.push_str("null"),
                }
            }
            out.push(']');
        }
        // PG's to_json spells a timestamp in ISO 8601 with a `T`
        // separator (`2020-01-15T10:30:00`), unlike the space-separated
        // text-out form, so it needs its own arm ahead of the catch-all.
        Value::Timestamp(_) => {
            let txt = crate::eval::values::value_to_text(v).replacen(' ', "T", 1);
            write_json(&JsonValue::String(txt), out);
        }
        // Fall-through: every other type (Date / Interval / Uuid / Bytea /
        // Time / Money / …) renders via the canonical PG-faithful text
        // renderer, wrapped as a JSON string — never a Rust debug dump.
        other => {
            let txt = crate::eval::values::value_to_text(other);
            write_json(&JsonValue::String(txt), out);
        }
    }
}

fn render_numeric(scaled: i128, scale: u8) -> String {
    let neg = scaled < 0;
    let mag_str = alloc::format!("{}", scaled.unsigned_abs());
    let s = scale as usize;
    let body = if s == 0 {
        mag_str
    } else if mag_str.len() > s {
        let p = mag_str.len() - s;
        alloc::format!("{}.{}", &mag_str[..p], &mag_str[p..])
    } else {
        let pad = s - mag_str.len();
        alloc::format!("0.{}{}", "0".repeat(pad), mag_str)
    };
    if neg { alloc::format!("-{body}") } else { body }
}

/// `json_build_object(k, v, k, v, …)` — variadic, even-length.
/// NULL key → error (PG: "argument cannot be null"). Values encoded
/// via `value_to_json_text`. Returns Value::Json.
/// v7.37.17 (17.6 siblings) — `jsonb_concat(a, b)` — function form
/// of the `||` operator. Object + object merges keys (right wins on
/// duplicates); array + array appends; array + scalar appends the
/// scalar; scalar + scalar makes a 2-element array (PG semantics).
pub fn concat(lhs: &Value, rhs: &Value) -> Result<Value<'static>, EvalError> {
    concat_inner(lhs, rhs).map(canonicalize_value)
}

fn concat_inner(lhs: &Value, rhs: &Value) -> Result<Value<'static>, EvalError> {
    let (a_src, b_src) = match (lhs, rhs) {
        (Value::Null, _) | (_, Value::Null) => return Ok(Value::Null),
        (Value::Json(a) | Value::Text(a), Value::Json(b) | Value::Text(b)) => {
            (a.as_ref(), b.as_ref())
        }
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: "jsonb_concat() expects (JSON, JSON)".into(),
            });
        }
    };
    let a = parse(a_src).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("invalid JSON lhs for concat: {e}"),
    })?;
    let b = parse(b_src).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("invalid JSON rhs for concat: {e}"),
    })?;
    let merged = match (a, b) {
        (JsonValue::Object(mut ea), JsonValue::Object(eb)) => {
            // Right side wins on duplicate keys.
            for (k, v) in eb {
                if let Some(slot) = ea.iter_mut().find(|(ek, _)| *ek == k) {
                    slot.1 = v;
                } else {
                    ea.push((k, v));
                }
            }
            JsonValue::Object(ea)
        }
        (JsonValue::Array(mut ia), JsonValue::Array(ib)) => {
            ia.extend(ib);
            JsonValue::Array(ia)
        }
        (JsonValue::Array(mut ia), scalar) => {
            ia.push(scalar);
            JsonValue::Array(ia)
        }
        (scalar, JsonValue::Array(ib)) => {
            let mut out = alloc::vec![scalar];
            out.extend(ib);
            JsonValue::Array(out)
        }
        (sa, sb) => JsonValue::Array(alloc::vec![sa, sb]),
    };
    Ok(Value::json(merged.to_json_text()))
}

/// v7.37.17 (17.6 siblings) — `jsonb_delete(doc, key)` — function
/// form of the `-` operator. Removes an object key or an array
/// element (by text match for objects, by index for arrays).
pub fn delete_key(lhs: &Value, rhs: &Value) -> Result<Value<'static>, EvalError> {
    delete_key_inner(lhs, rhs).map(canonicalize_value)
}

fn delete_key_inner(lhs: &Value, rhs: &Value) -> Result<Value<'static>, EvalError> {
    let src = match lhs {
        Value::Null => return Ok(Value::Null),
        Value::Json(s) | Value::Text(s) => s.as_ref(),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: "jsonb_delete() expects JSON lhs".into(),
            });
        }
    };
    let doc = parse(src).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("invalid JSON for delete: {e}"),
    })?;
    let out = match (doc, rhs) {
        (_, Value::Null) => return Ok(Value::Null),
        (JsonValue::Object(entries), Value::Text(key)) => {
            let filtered: Vec<(String, JsonValue)> = entries
                .into_iter()
                .filter(|(k, _)| k != key.as_ref())
                .collect();
            JsonValue::Object(filtered)
        }
        // PG `jsonb - text[]` removes every listed key from an object.
        (JsonValue::Object(entries), Value::TextArray(keys)) => {
            let filtered: Vec<(String, JsonValue)> = entries
                .into_iter()
                .filter(|(k, _)| !keys.iter().any(|kk| kk.as_deref() == Some(k.as_str())))
                .collect();
            JsonValue::Object(filtered)
        }
        (JsonValue::Array(items), Value::Int(idx)) => {
            let n = *idx;
            let len = items.len() as i64;
            let real = if n >= 0 { i64::from(n) } else { len + i64::from(n) };
            let filtered: Vec<JsonValue> = items
                .into_iter()
                .enumerate()
                .filter(|(i, _)| *i as i64 != real)
                .map(|(_, v)| v)
                .collect();
            JsonValue::Array(filtered)
        }
        (other, _) => other,
    };
    Ok(Value::json(out.to_json_text()))
}

pub fn build_object(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if !args.len().is_multiple_of(2) {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "json_build_object() needs an even number of args, got {}",
                args.len()
            ),
        });
    }
    let mut out = String::from("{");
    let mut first = true;
    for pair in args.chunks_exact(2) {
        if !first {
            out.push(',');
        }
        first = false;
        let key = match &pair[0] {
            Value::Null => {
                return Err(EvalError::TypeMismatch {
                    detail: "json_build_object() key cannot be NULL".into(),
                });
            }
            Value::Text(s) | Value::Json(s) => s.to_string(),
            other => format_value_as_text(other),
        };
        write_json(&JsonValue::String(key), &mut out);
        out.push(':');
        encode_value_into(&pair[1], &mut out);
    }
    out.push('}');
    Ok(Value::json(out))
}

/// `json_build_array(...)` — variadic; empty → "[]". Each arg
/// encoded via `value_to_json_text`.
pub fn build_array(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let mut out = String::from("[");
    for (i, v) in args.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        encode_value_into(v, &mut out);
    }
    out.push(']');
    Ok(Value::json(out))
}

fn format_value_as_text(v: &Value) -> String {
    match v {
        Value::SmallInt(n) => alloc::format!("{n}"),
        Value::Int(n) => alloc::format!("{n}"),
        Value::BigInt(n) => alloc::format!("{n}"),
        Value::Float(x) => alloc::format!("{x}"),
        Value::Bool(b) => alloc::format!("{b}"),
        other => alloc::format!("{other:?}"),
    }
}

/// `jsonb_set(target, path, new_value [, create_missing])` — replace
/// at PG text-array path. `create_missing` defaults to true.
///
///   * Path step on object: treated as key. If missing & create_missing
///     → insert; else no-op.
///   * Path step on array: integer index, negative counts from end.
///     Out-of-range with create_missing → append; without → no-op.
///   * Type mismatch (e.g. step on a scalar) → no-op (PG semantics).
pub fn set(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if !(3..=4).contains(&args.len()) {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("jsonb_set() takes 3 or 4 args, got {}", args.len()),
        });
    }
    if args.iter().take(3).any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    let create_missing = match args.get(3) {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(other) => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "jsonb_set() create_missing must be BOOL, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let doc_text = json_text_arg(&args[0], "jsonb_set", "target")?;
    let path = path_text_arg(&args[1], "jsonb_set")?;
    let new_text = json_text_arg(&args[2], "jsonb_set", "new_value")?;
    let mut root = parse(doc_text).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("jsonb_set(): invalid JSON target — {e}"),
    })?;
    let new_val = parse(new_text).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("jsonb_set(): invalid JSON new_value — {e}"),
    })?;
    set_at_path(&mut root, &path, new_val, create_missing);
    Ok(Value::json(root.to_json_text()))
}

/// v7.37.17 (17.6 siblings) — `jsonb_delete_path(doc, path[])` —
/// function form of the `#-` operator. Removes the value at the
/// nested path; missing path leaves the doc unchanged.
pub fn delete_path(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    delete_path_inner(args).map(canonicalize_value)
}

fn delete_path_inner(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "jsonb_delete_path() takes 2 args, got {}",
                args.len()
            ),
        });
    }
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    let doc_text = json_text_arg(&args[0], "jsonb_delete_path", "target")?;
    let path = path_text_arg(&args[1], "jsonb_delete_path")?;
    let mut root = parse(doc_text).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("jsonb_delete_path(): invalid JSON target — {e}"),
    })?;
    delete_at_path(&mut root, &path);
    Ok(Value::json(root.to_json_text()))
}

fn delete_at_path(node: &mut JsonValue, path: &[String]) {
    if path.is_empty() {
        return;
    }
    let step = &path[0];
    if path.len() == 1 {
        // Terminal step — remove here.
        match node {
            JsonValue::Object(entries) => {
                entries.retain(|(k, _)| k != step);
            }
            JsonValue::Array(items) => {
                if let Ok(idx) = step.parse::<i64>() {
                    let len = items.len() as i64;
                    let real = if idx >= 0 { idx } else { len + idx };
                    if real >= 0 && real < len {
                        items.remove(real as usize);
                    }
                }
            }
            _ => {}
        }
        return;
    }
    // Navigate deeper.
    match node {
        JsonValue::Object(entries) => {
            if let Some((_, child)) = entries.iter_mut().find(|(k, _)| k == step) {
                delete_at_path(child, &path[1..]);
            }
        }
        JsonValue::Array(items) => {
            if let Ok(idx) = step.parse::<i64>() {
                let len = items.len() as i64;
                let real = if idx >= 0 { idx } else { len + idx };
                if real >= 0 && real < len {
                    delete_at_path(&mut items[real as usize], &path[1..]);
                }
            }
        }
        _ => {}
    }
}

fn set_at_path(node: &mut JsonValue, path: &[String], new_val: JsonValue, create_missing: bool) {
    if path.is_empty() {
        *node = new_val;
        return;
    }
    let step = &path[0];
    let rest = &path[1..];
    match node {
        JsonValue::Object(entries) => {
            if let Some(pos) = entries.iter().position(|(k, _)| k == step) {
                if rest.is_empty() {
                    entries[pos].1 = new_val;
                } else {
                    set_at_path(&mut entries[pos].1, rest, new_val, create_missing);
                }
            } else if create_missing && rest.is_empty() {
                entries.push((step.clone(), new_val));
            }
            // Missing intermediate path with create_missing — PG only
            // creates the LEAF, never intermediate parents. No-op.
        }
        JsonValue::Array(items) => {
            let Some(idx) = resolve_array_index(step, items.len()) else {
                if create_missing && rest.is_empty() {
                    // PG: positive overshoot appends, negative prepends.
                    if let Ok(n) = step.parse::<i64>() {
                        if n < 0 {
                            items.insert(0, new_val);
                        } else {
                            items.push(new_val);
                        }
                    }
                }
                return;
            };
            if rest.is_empty() {
                items[idx] = new_val;
            } else {
                set_at_path(&mut items[idx], rest, new_val, create_missing);
            }
        }
        _ => {
            // Scalar — no replacement possible at non-empty path.
        }
    }
}

fn resolve_array_index(step: &str, len: usize) -> Option<usize> {
    let n = step.parse::<i64>().ok()?;
    if n >= 0 {
        let i = n as usize;
        if i < len { Some(i) } else { None }
    } else {
        let from_end = len as i64 + n;
        if from_end >= 0 {
            Some(from_end as usize)
        } else {
            None
        }
    }
}

/// `jsonb_insert(target, path, new_value [, insert_after])` —
/// insert at path. `insert_after` defaults to false.
///
///   * Array parent: insert before (or after) the index. Out-of-range
///     positive index → append; out-of-range negative → prepend.
///   * Object parent: key must NOT exist (PG raises). insert_after
///     has no effect for objects.
pub fn insert(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if !(3..=4).contains(&args.len()) {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("jsonb_insert() takes 3 or 4 args, got {}", args.len()),
        });
    }
    if args.iter().take(3).any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    let insert_after = match args.get(3) {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(other) => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "jsonb_insert() insert_after must be BOOL, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let doc_text = json_text_arg(&args[0], "jsonb_insert", "target")?;
    let path = path_text_arg(&args[1], "jsonb_insert")?;
    let new_text = json_text_arg(&args[2], "jsonb_insert", "new_value")?;
    if path.is_empty() {
        return Err(EvalError::TypeMismatch {
            detail: "jsonb_insert(): path cannot be empty".into(),
        });
    }
    let mut root = parse(doc_text).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("jsonb_insert(): invalid JSON target — {e}"),
    })?;
    let new_val = parse(new_text).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("jsonb_insert(): invalid JSON new_value — {e}"),
    })?;
    insert_at_path(&mut root, &path, new_val, insert_after)?;
    Ok(Value::json(root.to_json_text()))
}

fn insert_at_path(
    node: &mut JsonValue,
    path: &[String],
    new_val: JsonValue,
    insert_after: bool,
) -> Result<(), EvalError> {
    debug_assert!(!path.is_empty());
    if path.len() == 1 {
        let step = &path[0];
        match node {
            JsonValue::Object(entries) => {
                if entries.iter().any(|(k, _)| k == step) {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "jsonb_insert(): cannot replace existing key {step:?}"
                        ),
                    });
                }
                entries.push((step.clone(), new_val));
                Ok(())
            }
            JsonValue::Array(items) => {
                let Ok(n) = step.parse::<i64>() else {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "jsonb_insert(): array step must be integer, got {step:?}"
                        ),
                    });
                };
                let mut idx = if n >= 0 {
                    let i = n as usize;
                    if i > items.len() { items.len() } else { i }
                } else {
                    let from_end = items.len() as i64 + n;
                    if from_end < 0 { 0 } else { from_end as usize }
                };
                if insert_after && idx < items.len() {
                    idx += 1;
                }
                items.insert(idx, new_val);
                Ok(())
            }
            _ => Err(EvalError::TypeMismatch {
                detail: "jsonb_insert(): parent at path is a scalar".into(),
            }),
        }
    } else {
        let step = &path[0];
        let rest = &path[1..];
        match node {
            JsonValue::Object(entries) => {
                if let Some(pos) = entries.iter().position(|(k, _)| k == step) {
                    insert_at_path(&mut entries[pos].1, rest, new_val, insert_after)
                } else {
                    Err(EvalError::TypeMismatch {
                        detail: alloc::format!("jsonb_insert(): path {step:?} does not exist"),
                    })
                }
            }
            JsonValue::Array(items) => {
                let Some(idx) = resolve_array_index(step, items.len()) else {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!("jsonb_insert(): array index {step:?} out of range"),
                    });
                };
                insert_at_path(&mut items[idx], rest, new_val, insert_after)
            }
            _ => Err(EvalError::TypeMismatch {
                detail: "jsonb_insert(): parent at path is a scalar".into(),
            }),
        }
    }
}

fn json_text_arg<'a>(v: &'a Value, fname: &str, role: &str) -> Result<&'a str, EvalError> {
    match v {
        Value::Json(s) | Value::Text(s) => Ok(s.as_ref()),
        other => Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "{fname}() {role} must be JSON or TEXT, got {:?}",
                other.data_type()
            ),
        }),
    }
}

fn path_text_arg(v: &Value, fname: &str) -> Result<Vec<String>, EvalError> {
    match v {
        Value::Text(s) | Value::Json(s) => parse_text_array(s.as_ref()),
        Value::TextArray(items) => Ok(items
            .iter()
            .map(|o| o.clone().unwrap_or_default())
            .collect()),
        other => Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "{fname}() path must be TEXT[] or TEXT, got {:?}",
                other.data_type()
            ),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(s: &str) -> String {
        canonicalize_jsonb(s).unwrap()
    }

    #[test]
    fn canon_number_rules() {
        // Values from live PG 18.4 jsonb.
        assert_eq!(canon_json_number("1.0"), "1.0");
        assert_eq!(canon_json_number("1e2"), "100");
        assert_eq!(canon_json_number("1.10"), "1.10");
        assert_eq!(canon_json_number("100.00"), "100.00");
        assert_eq!(canon_json_number("0.5"), "0.5");
        assert_eq!(canon_json_number("-0"), "0");
        assert_eq!(canon_json_number("1E-3"), "0.001");
        assert_eq!(canon_json_number("42"), "42");
        assert_eq!(canon_json_number("-2.5"), "-2.5");
        assert_eq!(canon_json_number("2.5e3"), "2500");
    }

    #[test]
    fn canon_key_order_dedup_and_whitespace() {
        // Keys sort by (length, bytes); ""/a/b/z/aa. PG 18.4.
        assert_eq!(
            canon(r#"{"b":1,"a":2,"aa":3,"":9,"z":4}"#),
            r#"{"": 9, "a": 2, "b": 1, "z": 4, "aa": 3}"#
        );
        // Duplicate keys collapse last-wins.
        assert_eq!(canon(r#"{"a":1,"a":2,"a":3}"#), r#"{"a": 3}"#);
        // Arrays get `, ` and are not reordered.
        assert_eq!(canon("[3,2,1]"), "[3, 2, 1]");
    }

    #[test]
    fn json_number_equality_by_value() {
        let eq = |a: &str, b: &str| {
            json_eq(&parse(a).unwrap(), &parse(b).unwrap())
        };
        assert!(eq("1", "1.0"));
        assert!(eq("1.50", "1.5"));
        assert!(eq("1e3", "1000.00"));
        assert!(eq("0", "-0"));
        assert!(eq("2.5e3", "2500"));
        assert!(!eq("1.5", "1.6"));
        // Inside arrays / objects.
        assert!(eq("[1, 2.0]", "[1.0, 2]"));
        assert!(eq(r#"{"a":1}"#, r#"{"a":1.0}"#));
    }

    #[test]
    fn canon_nested_and_scalars() {
        assert_eq!(
            canon(r#"{"x":{"b":1,"a":2},"y":[3,{"d":1,"c":2}]}"#),
            r#"{"x": {"a": 2, "b": 1}, "y": [3, {"c": 2, "d": 1}]}"#
        );
        assert_eq!(canon("  true "), "true");
        assert_eq!(canon(" 42 "), "42");
        assert_eq!(canon("{}"), "{}");
        assert_eq!(canon("[]"), "[]");
        // Non-ASCII stays verbatim UTF-8; escapes preserved.
        assert_eq!(canon(r#"{"e":"café","t":"a\nb"}"#), r#"{"e": "café", "t": "a\nb"}"#);
    }

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
        let doc = Value::json::<String>(r#"{"name":"alice","age":30}"#.into());
        let key = Value::text("name");
        let v = path_get(&doc, &key, true).unwrap();
        assert_eq!(v, Value::text("alice"));
        let v = path_get(&doc, &key, false).unwrap();
        assert_eq!(v, Value::json("\"alice\""));
    }

    #[test]
    fn path_array_index_supports_negative() {
        let doc = Value::json("[10,20,30]");
        let v = path_get(&doc, &Value::Int(1), true).unwrap();
        assert_eq!(v, Value::text("20"));
        let v = path_get(&doc, &Value::Int(-1), true).unwrap();
        assert_eq!(v, Value::text("30"));
    }

    #[test]
    fn path_missing_key_returns_null() {
        let doc = Value::json::<String>(r#"{"a":1}"#.into());
        let v = path_get(&doc, &Value::text("missing"), true).unwrap();
        assert_eq!(v, Value::Null);
    }

    #[test]
    fn path_get_nested_subtree_renders_back() {
        let doc = Value::json::<String>(r#"{"k":{"x":[1,2]}}"#.into());
        let v = path_get(&doc, &Value::text("k"), false).unwrap();
        // The extracted jsonb subtree is re-emitted canonically (PG form).
        assert_eq!(v, Value::json::<String>("{\"x\": [1, 2]}".into()));
    }
}

/// v7.37.17 (17.6 siblings) — one step of a MySQL JSON path
/// (`$.key`, `$."quoted key"`, `$[0]`).
#[derive(Debug)]
pub enum MysqlPathStep {
    Key(String),
    Index(usize),
}

/// Parse a MySQL JSON path. Supports `$`, `.key`, `."quoted key"`
/// and `[N]`; wildcard steps (`*`, `[*]`, `**`) error honestly —
/// they return multiple matches per document and need a different
/// walker shape.
pub fn mysql_path_steps(path: &str) -> Result<Vec<MysqlPathStep>, EvalError> {
    let chars: Vec<char> = path.trim().chars().collect();
    if chars.first() != Some(&'$') {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("invalid JSON path expression (must start with $): {path:?}"),
        });
    }
    let mut steps = Vec::new();
    let mut i = 1;
    while i < chars.len() {
        match chars[i] {
            '.' => {
                i += 1;
                if i < chars.len() && chars[i] == '"' {
                    i += 1;
                    let mut key = String::new();
                    while i < chars.len() && chars[i] != '"' {
                        if chars[i] == '\\' && i + 1 < chars.len() {
                            i += 1;
                        }
                        key.push(chars[i]);
                        i += 1;
                    }
                    if i >= chars.len() {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "invalid JSON path expression (unterminated quote): {path:?}"
                            ),
                        });
                    }
                    i += 1; // closing quote
                    steps.push(MysqlPathStep::Key(key));
                } else {
                    let mut key = String::new();
                    while i < chars.len()
                        && (chars[i].is_alphanumeric() || chars[i] == '_')
                    {
                        key.push(chars[i]);
                        i += 1;
                    }
                    if key.is_empty() {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!(
                                "unsupported JSON path step at position {i} in {path:?} \
                                 (wildcards are not supported)"
                            ),
                        });
                    }
                    steps.push(MysqlPathStep::Key(key));
                }
            }
            '[' => {
                i += 1;
                let mut num = String::new();
                while i < chars.len() && chars[i] != ']' {
                    num.push(chars[i]);
                    i += 1;
                }
                if i >= chars.len() {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "invalid JSON path expression (unterminated bracket): {path:?}"
                        ),
                    });
                }
                i += 1; // ]
                let idx: usize = num.trim().parse().map_err(|_| EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "unsupported JSON path index {num:?} in {path:?} \
                         (wildcards are not supported)"
                    ),
                })?;
                steps.push(MysqlPathStep::Index(idx));
            }
            other => {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid JSON path expression (unexpected {other:?}): {path:?}"
                    ),
                });
            }
        }
    }
    Ok(steps)
}

/// Walk a parsed JSON document along a MySQL path. Returns None
/// when any step misses.
pub fn mysql_path_get<'a>(
    doc: &'a JsonValue,
    steps: &[MysqlPathStep],
) -> Option<&'a JsonValue> {
    let mut cur = doc;
    for step in steps {
        match (step, cur) {
            (MysqlPathStep::Key(k), JsonValue::Object(members)) => {
                cur = members.iter().find(|(mk, _)| mk == k).map(|(_, v)| v)?;
            }
            (MysqlPathStep::Index(idx), JsonValue::Array(items)) => {
                cur = items.get(*idx)?;
            }
            // MySQL: a non-array auto-wraps as a one-element array
            // for [0].
            (MysqlPathStep::Index(0), scalar) => {
                cur = scalar;
            }
            _ => return None,
        }
    }
    Some(cur)
}

/// v7.37.17 (17.6 siblings) — MySQL JSON_EXTRACT(doc, path...).
/// One path → the value at that path (or SQL NULL when it misses);
/// several paths → a JSON array of the values that matched (NULL
/// when none did).
pub fn mysql_json_extract(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() < 2 {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "json_extract() takes a document and at least one path, got {} args",
                args.len()
            ),
        });
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let src = match &args[0] {
        Value::Json(s) | Value::Text(s) => s.as_ref(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "json_extract() document must be json, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let doc = parse(src).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("json_extract(): invalid JSON: {e}"),
    })?;
    let mut hits: Vec<String> = Vec::new();
    for path_v in &args[1..] {
        let Value::Text(p) = path_v else {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "json_extract() paths must be text, got {:?}",
                    path_v.data_type()
                ),
            });
        };
        let steps = mysql_path_steps(p)?;
        if let Some(v) = mysql_path_get(&doc, &steps) {
            hits.push(v.to_json_text());
        }
    }
    match (args.len() - 1, hits.len()) {
        (_, 0) => Ok(Value::Null),
        (1, _) => Ok(Value::Json(alloc::borrow::Cow::Owned(
            hits.into_iter().next().unwrap(),
        ))),
        _ => {
            let mut out = String::from("[");
            for (i, h) in hits.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(h);
            }
            out.push(']');
            Ok(Value::Json(alloc::borrow::Cow::Owned(out)))
        }
    }
}

/// v7.37.17 (17.6 siblings) — MySQL JSON_CONTAINS_PATH(doc,
/// 'one'|'all', path...).
pub fn mysql_json_contains_path(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() < 3 {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "json_contains_path() takes a document, one/all, and at least one path, got {} args",
                args.len()
            ),
        });
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let src = match &args[0] {
        Value::Json(s) | Value::Text(s) => s.as_ref(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "json_contains_path() document must be json, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let doc = parse(src).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("json_contains_path(): invalid JSON: {e}"),
    })?;
    let mode = match &args[1] {
        Value::Text(m) if m.eq_ignore_ascii_case("one") => false,
        Value::Text(m) if m.eq_ignore_ascii_case("all") => true,
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "json_contains_path() second arg must be 'one' or 'all', got {other:?}"
                ),
            });
        }
    };
    let mut found_any = false;
    let mut found_all = true;
    for path_v in &args[2..] {
        let Value::Text(p) = path_v else {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "json_contains_path() paths must be text, got {:?}",
                    path_v.data_type()
                ),
            });
        };
        let steps = mysql_path_steps(p)?;
        if mysql_path_get(&doc, &steps).is_some() {
            found_any = true;
        } else {
            found_all = false;
        }
    }
    Ok(Value::Bool(if mode { found_all } else { found_any }))
}

/// v7.37.17 (17.6 siblings) — convert a SQL value into a JsonValue
/// for the MySQL JSON mutation functions (SQL text becomes a JSON
/// string; JSON passes through parsed).
fn value_to_jsonvalue(v: &Value) -> Result<JsonValue, EvalError> {
    Ok(match v {
        Value::Null => JsonValue::Null,
        Value::Bool(b) => JsonValue::Bool(*b),
        Value::Json(s) => parse(s).map_err(|e| EvalError::TypeMismatch {
            detail: alloc::format!("invalid JSON value: {e}"),
        })?,
        Value::Text(s) => JsonValue::String(s.to_string()),
        other => {
            // Numbers and everything else render through the
            // to_json text form, then parse back.
            let text = value_to_json_text(other);
            parse(&text).map_err(|e| EvalError::TypeMismatch {
                detail: alloc::format!("invalid JSON value: {e}"),
            })?
        }
    })
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum MutateMode {
    /// json_set — replace existing, create missing.
    Set,
    /// json_insert — create missing only.
    Insert,
    /// json_replace — replace existing only.
    Replace,
}

/// Apply one path mutation. Missing intermediate steps are a no-op
/// (MySQL: only the final step may be created).
fn mutate_at(cur: &mut JsonValue, steps: &[MysqlPathStep], mode: MutateMode, newval: &JsonValue) {
    match steps {
        [] => {
            if matches!(mode, MutateMode::Set | MutateMode::Replace) {
                *cur = newval.clone();
            }
        }
        [last] => match (last, &mut *cur) {
            (MysqlPathStep::Key(k), JsonValue::Object(members)) => {
                if let Some(slot) = members.iter_mut().find(|(mk, _)| mk == k) {
                    if matches!(mode, MutateMode::Set | MutateMode::Replace) {
                        slot.1 = newval.clone();
                    }
                } else if matches!(mode, MutateMode::Set | MutateMode::Insert) {
                    members.push((k.clone(), newval.clone()));
                }
            }
            (MysqlPathStep::Index(i), JsonValue::Array(items)) => {
                if *i < items.len() {
                    if matches!(mode, MutateMode::Set | MutateMode::Replace) {
                        items[*i] = newval.clone();
                    }
                } else if matches!(mode, MutateMode::Set | MutateMode::Insert) {
                    // Index past the end appends (MySQL semantics).
                    items.push(newval.clone());
                }
            }
            // Scalar auto-wraps as a one-element array: [0] exists.
            (MysqlPathStep::Index(0), scalar) => {
                if matches!(mode, MutateMode::Set | MutateMode::Replace) {
                    *scalar = newval.clone();
                }
            }
            _ => {}
        },
        [head, rest @ ..] => match (head, cur) {
            (MysqlPathStep::Key(k), JsonValue::Object(members)) => {
                if let Some(slot) = members.iter_mut().find(|(mk, _)| mk == k) {
                    mutate_at(&mut slot.1, rest, mode, newval);
                }
            }
            (MysqlPathStep::Index(i), JsonValue::Array(items)) => {
                if let Some(slot) = items.get_mut(*i) {
                    mutate_at(slot, rest, mode, newval);
                }
            }
            _ => {}
        },
    }
}

fn mysql_json_mutate(
    args: &[Value<'_>],
    mode: MutateMode,
    fn_name: &str,
) -> Result<Value<'static>, EvalError> {
    if args.len() < 3 || args.len() % 2 == 0 {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "{fn_name}() takes a document plus (path, value) pairs, got {} args",
                args.len()
            ),
        });
    }
    if matches!(args[0], Value::Null) {
        return Ok(Value::Null);
    }
    let src = match &args[0] {
        Value::Json(s) | Value::Text(s) => s.as_ref(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "{fn_name}() document must be json, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let mut doc = parse(src).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("{fn_name}(): invalid JSON: {e}"),
    })?;
    for pair in args[1..].chunks(2) {
        let Value::Text(p) = &pair[0] else {
            if matches!(pair[0], Value::Null) {
                return Ok(Value::Null);
            }
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "{fn_name}() paths must be text, got {:?}",
                    pair[0].data_type()
                ),
            });
        };
        let steps = mysql_path_steps(p)?;
        let newval = value_to_jsonvalue(&pair[1])?;
        mutate_at(&mut doc, &steps, mode, &newval);
    }
    Ok(Value::Json(alloc::borrow::Cow::Owned(doc.to_json_text())))
}

/// v7.37.17 (17.6 siblings) — MySQL JSON_SET / JSON_INSERT /
/// JSON_REPLACE ('$.x'-path forms; the PG jsonb_set text-array
/// spelling stays on crate::json::set).
pub fn mysql_json_set(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    mysql_json_mutate(args, MutateMode::Set, "json_set")
}

pub fn mysql_json_insert(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    mysql_json_mutate(args, MutateMode::Insert, "json_insert")
}

pub fn mysql_json_replace(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    mysql_json_mutate(args, MutateMode::Replace, "json_replace")
}

/// v7.37.17 (17.6 siblings) — MySQL JSON_REMOVE(doc, path...).
/// Removing the root path `$` errors, as in MySQL.
pub fn mysql_json_remove(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() < 2 {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "json_remove() takes a document and at least one path, got {} args",
                args.len()
            ),
        });
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let src = match &args[0] {
        Value::Json(s) | Value::Text(s) => s.as_ref(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "json_remove() document must be json, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let mut doc = parse(src).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("json_remove(): invalid JSON: {e}"),
    })?;
    fn remove_at(cur: &mut JsonValue, steps: &[MysqlPathStep]) {
        match steps {
            [] => {}
            [last] => match (last, cur) {
                (MysqlPathStep::Key(k), JsonValue::Object(members)) => {
                    members.retain(|(mk, _)| mk != k);
                }
                (MysqlPathStep::Index(i), JsonValue::Array(items)) => {
                    if *i < items.len() {
                        items.remove(*i);
                    }
                }
                _ => {}
            },
            [head, rest @ ..] => match (head, cur) {
                (MysqlPathStep::Key(k), JsonValue::Object(members)) => {
                    if let Some(slot) = members.iter_mut().find(|(mk, _)| mk == k) {
                        remove_at(&mut slot.1, rest);
                    }
                }
                (MysqlPathStep::Index(i), JsonValue::Array(items)) => {
                    if let Some(slot) = items.get_mut(*i) {
                        remove_at(slot, rest);
                    }
                }
                _ => {}
            },
        }
    }
    for path_v in &args[1..] {
        let Value::Text(p) = path_v else {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "json_remove() paths must be text, got {:?}",
                    path_v.data_type()
                ),
            });
        };
        let steps = mysql_path_steps(p)?;
        if steps.is_empty() {
            return Err(EvalError::TypeMismatch {
                detail: "The path expression '$' is not allowed in this context".into(),
            });
        }
        remove_at(&mut doc, &steps);
    }
    Ok(Value::Json(alloc::borrow::Cow::Owned(doc.to_json_text())))
}

/// Apply `f` to the value AT the full path (not its parent). Missing
/// steps are a no-op.
fn modify_at(cur: &mut JsonValue, steps: &[MysqlPathStep], f: &mut dyn FnMut(&mut JsonValue)) {
    match steps {
        [] => f(cur),
        [head, rest @ ..] => match (head, cur) {
            (MysqlPathStep::Key(k), JsonValue::Object(members)) => {
                if let Some(slot) = members.iter_mut().find(|(mk, _)| mk == k) {
                    modify_at(&mut slot.1, rest, f);
                }
            }
            (MysqlPathStep::Index(i), JsonValue::Array(items)) => {
                if let Some(slot) = items.get_mut(*i) {
                    modify_at(slot, rest, f);
                }
            }
            _ => {}
        },
    }
}

/// Shared arg plumbing for the (doc, path, value)-pairs mutators.
fn mysql_doc_and_pairs<'a>(
    args: &'a [Value<'_>],
    fn_name: &str,
) -> Result<Option<(JsonValue, &'a [Value<'a>])>, EvalError> {
    if args.len() < 3 || args.len() % 2 == 0 {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "{fn_name}() takes a document plus (path, value) pairs, got {} args",
                args.len()
            ),
        });
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(None);
    }
    let src = match &args[0] {
        Value::Json(s) | Value::Text(s) => s.as_ref(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "{fn_name}() document must be json, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let doc = parse(src).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("{fn_name}(): invalid JSON: {e}"),
    })?;
    Ok(Some((doc, &args[1..])))
}

/// v7.37.17 (17.6 siblings) — MySQL JSON_ARRAY_APPEND(doc, path,
/// val, ...). The value at path gains `val` at the end; a non-array
/// value wraps as `[old, val]` (MySQL semantics).
pub fn mysql_json_array_append(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let Some((mut doc, pairs)) = mysql_doc_and_pairs(args, "json_array_append")? else {
        return Ok(Value::Null);
    };
    for pair in pairs.chunks(2) {
        let Value::Text(p) = &pair[0] else {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "json_array_append() paths must be text, got {:?}",
                    pair[0].data_type()
                ),
            });
        };
        let steps = mysql_path_steps(p)?;
        let newval = value_to_jsonvalue(&pair[1])?;
        modify_at(&mut doc, &steps, &mut |v| match v {
            JsonValue::Array(items) => items.push(newval.clone()),
            other => {
                let old = core::mem::replace(other, JsonValue::Null);
                *other = JsonValue::Array(alloc::vec![old, newval.clone()]);
            }
        });
    }
    Ok(Value::Json(alloc::borrow::Cow::Owned(doc.to_json_text())))
}

/// v7.37.17 (17.6 siblings) — MySQL JSON_ARRAY_INSERT(doc, path,
/// val, ...). The path must end in `[N]`; the value is inserted at
/// position N in the parent array, shifting later elements right
/// (past-the-end appends). A non-array parent is a no-op.
pub fn mysql_json_array_insert(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let Some((mut doc, pairs)) = mysql_doc_and_pairs(args, "json_array_insert")? else {
        return Ok(Value::Null);
    };
    for pair in pairs.chunks(2) {
        let Value::Text(p) = &pair[0] else {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "json_array_insert() paths must be text, got {:?}",
                    pair[0].data_type()
                ),
            });
        };
        let steps = mysql_path_steps(p)?;
        let Some(MysqlPathStep::Index(idx)) = steps.last() else {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "json_array_insert() path must end with an array index: {p:?}"
                ),
            });
        };
        let idx = *idx;
        let newval = value_to_jsonvalue(&pair[1])?;
        modify_at(&mut doc, &steps[..steps.len() - 1], &mut |v| {
            if let JsonValue::Array(items) = v {
                let at = idx.min(items.len());
                items.insert(at, newval.clone());
            }
        });
    }
    Ok(Value::Json(alloc::borrow::Cow::Owned(doc.to_json_text())))
}

/// MySQL JSON containment recursion: candidate object ⊆ target
/// object (same keys, contained values); each candidate array
/// element contained in some target array element; a candidate
/// scalar is contained in an array when it equals some element.
fn mysql_contains(target: &JsonValue, cand: &JsonValue) -> bool {
    match (target, cand) {
        (JsonValue::Object(t), JsonValue::Object(c)) => c.iter().all(|(ck, cv)| {
            t.iter()
                .any(|(tk, tv)| tk == ck && mysql_contains(tv, cv))
        }),
        (JsonValue::Array(t), JsonValue::Array(c)) => c
            .iter()
            .all(|cv| t.iter().any(|tv| mysql_contains(tv, cv))),
        (JsonValue::Array(t), scalar) => {
            t.iter().any(|tv| mysql_contains(tv, scalar))
        }
        // Numbers compare numerically across the two lexeme forms.
        (JsonValue::Number(a), JsonValue::NumberText(b))
        | (JsonValue::NumberText(b), JsonValue::Number(a)) => {
            b.parse::<f64>().map(|x| x == *a).unwrap_or(false)
        }
        (JsonValue::NumberText(a), JsonValue::NumberText(b)) => {
            a == b
                || (a.parse::<f64>().ok().zip(b.parse::<f64>().ok()))
                    .map(|(x, y)| x == y)
                    .unwrap_or(false)
        }
        (a, b) => a == b,
    }
}

/// v7.37.17 (17.6 siblings) — MySQL JSON_CONTAINS(target, candidate
/// [, path]).
pub fn mysql_json_contains(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if !matches!(args.len(), 2 | 3) {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("json_contains() takes 2 or 3 args, got {}", args.len()),
        });
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let parse_arg = |v: &Value<'_>, which: &str| -> Result<JsonValue, EvalError> {
        match v {
            Value::Json(s) | Value::Text(s) => parse(s).map_err(|e| EvalError::TypeMismatch {
                detail: alloc::format!("json_contains(): invalid {which} JSON: {e}"),
            }),
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "json_contains() {which} must be json, got {:?}",
                    other.data_type()
                ),
            }),
        }
    };
    let target = parse_arg(&args[0], "target")?;
    let cand = parse_arg(&args[1], "candidate")?;
    let effective = match args.get(2) {
        None => Some(&target),
        Some(Value::Text(p)) => {
            let steps = mysql_path_steps(p)?;
            mysql_path_get(&target, &steps)
        }
        Some(other) => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "json_contains() path must be text, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    match effective {
        None => Ok(Value::Null),
        Some(t) => Ok(Value::Bool(mysql_contains(t, &cand))),
    }
}

/// RFC 7396 merge-patch: a non-object patch replaces the target;
/// an object patch merges key-by-key, with JSON null values
/// removing keys.
fn merge_patch(target: JsonValue, patch: JsonValue) -> JsonValue {
    let JsonValue::Object(patch_members) = patch else {
        return patch;
    };
    let mut out = match target {
        JsonValue::Object(members) => members,
        _ => Vec::new(),
    };
    for (k, v) in patch_members {
        if matches!(v, JsonValue::Null) {
            out.retain(|(mk, _)| *mk != k);
        } else if let Some(slot) = out.iter_mut().find(|(mk, _)| *mk == k) {
            let old = core::mem::replace(&mut slot.1, JsonValue::Null);
            slot.1 = merge_patch(old, v);
        } else {
            // Merging into a missing key still strips nested nulls.
            out.push((k, merge_patch(JsonValue::Null, v)));
        }
    }
    JsonValue::Object(out)
}

/// MySQL JSON_MERGE_PRESERVE pairwise rule: arrays concatenate,
/// objects merge with duplicate-key values merged recursively,
/// scalars combine into arrays (a non-array beside an array wraps
/// first).
fn merge_preserve(a: JsonValue, b: JsonValue) -> JsonValue {
    match (a, b) {
        (JsonValue::Object(mut ma), JsonValue::Object(mb)) => {
            for (k, v) in mb {
                if let Some(pos) = ma.iter().position(|(mk, _)| *mk == k) {
                    let (_, old) = ma.remove(pos);
                    ma.insert(pos, (k, merge_preserve(old, v)));
                } else {
                    ma.push((k, v));
                }
            }
            JsonValue::Object(ma)
        }
        (JsonValue::Array(mut xs), JsonValue::Array(ys)) => {
            xs.extend(ys);
            JsonValue::Array(xs)
        }
        (JsonValue::Array(mut xs), scalar) => {
            xs.push(scalar);
            JsonValue::Array(xs)
        }
        (scalar, JsonValue::Array(ys)) => {
            let mut xs = alloc::vec![scalar];
            xs.extend(ys);
            JsonValue::Array(xs)
        }
        (sa, sb) => JsonValue::Array(alloc::vec![sa, sb]),
    }
}

fn mysql_json_merge(
    args: &[Value<'_>],
    fn_name: &str,
    combine: fn(JsonValue, JsonValue) -> JsonValue,
) -> Result<Value<'static>, EvalError> {
    if args.len() < 2 {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "{fn_name}() takes at least 2 documents, got {}",
                args.len()
            ),
        });
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let mut acc: Option<JsonValue> = None;
    for arg in args {
        let src = match arg {
            Value::Json(s) | Value::Text(s) => s.as_ref(),
            other => {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "{fn_name}() arguments must be json, got {:?}",
                        other.data_type()
                    ),
                });
            }
        };
        let doc = parse(src).map_err(|e| EvalError::TypeMismatch {
            detail: alloc::format!("{fn_name}(): invalid JSON: {e}"),
        })?;
        acc = Some(match acc {
            None => doc,
            Some(prev) => combine(prev, doc),
        });
    }
    Ok(Value::Json(alloc::borrow::Cow::Owned(
        acc.unwrap().to_json_text(),
    )))
}

/// v7.37.17 (17.6 siblings) — MySQL JSON_MERGE_PATCH (RFC 7396).
pub fn mysql_json_merge_patch(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    mysql_json_merge(args, "json_merge_patch", merge_patch)
}

/// v7.37.17 (17.6 siblings) — MySQL JSON_MERGE_PRESERVE (and its
/// deprecated JSON_MERGE alias).
pub fn mysql_json_merge_preserve(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    mysql_json_merge(args, "json_merge_preserve", merge_preserve)
}

/// v7.37.17 (17.6 siblings) — MySQL JSON_OVERLAPS(d1, d2): arrays
/// share any element; objects share any key-value pair; scalars
/// compare equal; an array vs a scalar checks membership.
pub fn mysql_json_overlaps(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("json_overlaps() takes 2 args, got {}", args.len()),
        });
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let parse_arg = |v: &Value<'_>| -> Result<JsonValue, EvalError> {
        match v {
            Value::Json(s) | Value::Text(s) => parse(s).map_err(|e| EvalError::TypeMismatch {
                detail: alloc::format!("json_overlaps(): invalid JSON: {e}"),
            }),
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "json_overlaps() arguments must be json, got {:?}",
                    other.data_type()
                ),
            }),
        }
    };
    let a = parse_arg(&args[0])?;
    let b = parse_arg(&args[1])?;
    let overlaps = match (&a, &b) {
        (JsonValue::Array(xs), JsonValue::Array(ys)) => {
            xs.iter().any(|x| ys.iter().any(|y| mysql_contains(x, y) && mysql_contains(y, x)))
        }
        (JsonValue::Object(ma), JsonValue::Object(mb)) => ma.iter().any(|(k, v)| {
            mb.iter()
                .any(|(k2, v2)| k == k2 && mysql_contains(v, v2) && mysql_contains(v2, v))
        }),
        (JsonValue::Array(xs), scalar) | (scalar, JsonValue::Array(xs)) => xs
            .iter()
            .any(|x| mysql_contains(x, scalar) && mysql_contains(scalar, x)),
        (sa, sb) => mysql_contains(sa, sb) && mysql_contains(sb, sa),
    };
    Ok(Value::Bool(overlaps))
}

/// SQL LIKE matcher for json_search: `%` any run, `_` one char,
/// `escape` literalises the next char.
fn like_match(text: &[char], pat: &[char], escape: char) -> bool {
    match pat {
        [] => text.is_empty(),
        ['%', rest @ ..] => {
            (0..=text.len()).any(|skip| like_match(&text[skip..], rest, escape))
        }
        ['_', rest @ ..] => !text.is_empty() && like_match(&text[1..], rest, escape),
        [e, lit, rest @ ..] if *e == escape => {
            text.first() == Some(lit) && like_match(&text[1..], rest, escape)
        }
        [c, rest @ ..] => text.first() == Some(c) && like_match(&text[1..], rest, escape),
    }
}

/// Render one MySQL path step onto a path string. Identifier-shaped
/// keys render bare (`$.a`); anything else quotes (`$."a b"`).
fn push_path_step(out: &mut String, step_key: Option<&str>, step_idx: Option<usize>) {
    if let Some(k) = step_key {
        let ident_shaped = !k.is_empty()
            && k.chars().all(|c| c.is_alphanumeric() || c == '_')
            && !k.chars().next().unwrap().is_numeric();
        if ident_shaped {
            out.push('.');
            out.push_str(k);
        } else {
            out.push_str(".\"");
            for c in k.chars() {
                if c == '"' || c == '\\' {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push('"');
        }
    }
    if let Some(i) = step_idx {
        out.push('[');
        out.push_str(&alloc::format!("{i}"));
        out.push(']');
    }
}

/// v7.37.17 (17.6 siblings) — MySQL JSON_SEARCH(doc, 'one'|'all',
/// pattern [, escape [, path...]]). Returns the path of the first
/// string value LIKE-matching the pattern ('one') or a JSON array
/// of all such paths ('all'); NULL when nothing matches. The
/// optional path args narrow where the walk starts.
pub fn mysql_json_search(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() < 3 {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "json_search() takes doc, one/all, pattern [, escape [, path...]], got {} args",
                args.len()
            ),
        });
    }
    if args[..3].iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let src = match &args[0] {
        Value::Json(s) | Value::Text(s) => s.as_ref(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "json_search() document must be json, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let doc = parse(src).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("json_search(): invalid JSON: {e}"),
    })?;
    let one = match &args[1] {
        Value::Text(m) if m.eq_ignore_ascii_case("one") => true,
        Value::Text(m) if m.eq_ignore_ascii_case("all") => false,
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "json_search() second arg must be 'one' or 'all', got {other:?}"
                ),
            });
        }
    };
    let Value::Text(pattern) = &args[2] else {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "json_search() pattern must be text, got {:?}",
                args[2].data_type()
            ),
        });
    };
    let escape = match args.get(3) {
        None | Some(Value::Null) => '\\',
        Some(Value::Text(e)) if e.chars().count() == 1 => e.chars().next().unwrap(),
        Some(other) => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "json_search() escape must be a single character, got {other:?}"
                ),
            });
        }
    };
    let pat: Vec<char> = pattern.chars().collect();
    fn walk(
        v: &JsonValue,
        path: &str,
        pat: &[char],
        escape: char,
        hits: &mut Vec<String>,
        stop_at_one: bool,
    ) {
        if stop_at_one && !hits.is_empty() {
            return;
        }
        match v {
            JsonValue::String(s) => {
                let chars: Vec<char> = s.chars().collect();
                if like_match(&chars, pat, escape) {
                    hits.push(path.to_string());
                }
            }
            JsonValue::Object(members) => {
                for (k, mv) in members {
                    let mut p = path.to_string();
                    push_path_step(&mut p, Some(k), None);
                    walk(mv, &p, pat, escape, hits, stop_at_one);
                }
            }
            JsonValue::Array(items) => {
                for (i, iv) in items.iter().enumerate() {
                    let mut p = path.to_string();
                    push_path_step(&mut p, None, Some(i));
                    walk(iv, &p, pat, escape, hits, stop_at_one);
                }
            }
            _ => {}
        }
    }
    let mut hits: Vec<String> = Vec::new();
    let start_paths: Vec<String> = args.get(4..).unwrap_or(&[])
        .iter()
        .map(|v| match v {
            Value::Text(p) => Ok(p.to_string()),
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "json_search() paths must be text, got {:?}",
                    other.data_type()
                ),
            }),
        })
        .collect::<Result<_, _>>()?;
    if start_paths.is_empty() {
        walk(&doc, "$", &pat, escape, &mut hits, one);
    } else {
        for p in &start_paths {
            let steps = mysql_path_steps(p)?;
            if let Some(sub) = mysql_path_get(&doc, &steps) {
                walk(sub, p.trim(), &pat, escape, &mut hits, one);
            }
        }
    }
    match hits.len() {
        0 => Ok(Value::Null),
        1 => Ok(Value::Json(alloc::borrow::Cow::Owned(
            JsonValue::String(hits.into_iter().next().unwrap()).to_json_text(),
        ))),
        _ => {
            let arr = JsonValue::Array(hits.into_iter().map(JsonValue::String).collect());
            Ok(Value::Json(alloc::borrow::Cow::Owned(arr.to_json_text())))
        }
    }
}

/// v7.37.17 (17.6 siblings) — MySQL JSON_VALUE(doc, path). Returns
/// the scalar at the path as unquoted text (MySQL's default
/// RETURNING VARCHAR); containers render as JSON text; a miss is
/// NULL. The RETURNING clause is parser syntax and queued.
pub fn mysql_json_value(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("json_value() takes 2 args, got {}", args.len()),
        });
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let src = match &args[0] {
        Value::Json(s) | Value::Text(s) => s.as_ref(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "json_value() document must be json, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let doc = parse(src).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("json_value(): invalid JSON: {e}"),
    })?;
    let Value::Text(p) = &args[1] else {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "json_value() path must be text, got {:?}",
                args[1].data_type()
            ),
        });
    };
    let steps = mysql_path_steps(p)?;
    match mysql_path_get(&doc, &steps) {
        None => Ok(Value::Null),
        Some(JsonValue::Null) => Ok(Value::Null),
        Some(v) => Ok(Value::text(v.as_text())),
    }
}
