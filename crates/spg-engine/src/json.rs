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

/// v7.39 (round 205, JSON_TABLE) — parse a document string into a
/// JsonValue tree. Thin pub(crate) wrapper so the executor's
/// JSON_TABLE arm can hold the parsed root across row iteration.
pub(crate) fn parse_doc(src: &str) -> Result<JsonValue, EvalError> {
    parse(src).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("invalid JSON for JSON_TABLE: {e}"),
    })
}

/// v7.39 (round 205, JSON_TABLE) — evaluate a jsonpath string over a
/// pre-parsed JsonValue root, returning the ordered match set. The
/// JSON_TABLE executor drives this for the row pattern (once per doc)
/// and each column path (once per row item). `vars` carries PASSING
/// variables the same way the jsonb_path_* functions do.
pub(crate) fn json_table_path(
    root: &JsonValue,
    path: &str,
    vars: Option<&JsonValue>,
) -> Result<Vec<JsonValue>, EvalError> {
    let (strict, steps) = parse_jsonpath_mode(path)?;
    apply_jsonpath_mode(root, &steps, vars, strict)
}

impl JsonValue {
    /// v7.39 (round 205) — the scalar text of a JSON value for column
    /// coercion: a json string yields its inner text (so
    /// `"2024-01-15"` coerces to a DATE by its content), numbers/bools
    /// their literal, containers their json text.
    pub(crate) fn scalar_text(&self) -> String {
        self.as_text()
    }

    /// v7.39 (round 206) — the PG-canonical jsonb TEXT of this value
    /// (spaces after `,` and `:`, strings quoted): a FORMAT JSON
    /// column returns this, matching PG's `[1, 2, 3]` / `{"x": 1}` /
    /// `"hi"` output.
    pub(crate) fn canonical_json_text(&self) -> String {
        let mut out = String::new();
        write_json_canonical(self, &mut out);
        out
    }

    /// v7.39 (round 205) — true for a JSON null (distinct from "no
    /// match": the caller checks emptiness of the match set first).
    pub(crate) fn is_json_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    fn as_text(&self) -> String {
        match self {
            Self::Null => "null".into(),
            Self::Bool(b) => if *b { "true" } else { "false" }.into(),
            Self::Number(x) => alloc::format!("{x}"),
            Self::NumberText(s) | Self::String(s) => s.clone(),
            Self::Array(_) | Self::Object(_) => self.to_json_text(),
        }
    }

    pub(crate) fn to_json_text(&self) -> String {
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
    // v7.39 (round 619) — the canonical form is the source plus the spaces
    // after `:` and `,`; sizing for that keeps the writer off the allocator.
    let mut out = String::with_capacity(src.len() + src.len() / 4 + 8);
    write_json_canonical(&v, &mut out);
    Ok(out)
}

/// Canonicalise a `Value::Json` payload (a jsonb-typed result); any
/// other value passes through untouched. Used to bring jsonb builder /
/// mutator functions (`jsonb_build_object`, `to_jsonb`, `jsonb_set`, the
/// `||` / `-` / `#-` operators, …) in line with PG, which always emits
/// canonical jsonb from them. The `json_*` siblings stay verbatim.
#[must_use]
/// v7.39 (round 603) — the `JsonValue` a scalar becomes, when it becomes one
/// simply.
///
/// `to_jsonb(5)` used to format the value into JSON text and then hand that
/// text to `canonicalize_value`, which PARSES it and serialises it again —
/// ten allocations a row for an integer, against one for the same projection
/// without it, and `jsonb_build_object('a', id)` eighteen. The canonical
/// form of these scalars is not in doubt, so they skip the round trip. `None`
/// sends the caller down the text-then-reparse path, which is what anything
/// richer (NUMERIC, dates, arrays, composites, already-JSON values) needs.
fn simple_scalar_json(v: &Value<'_>) -> Option<JsonValue> {
    Some(match v {
        Value::Null => JsonValue::Null,
        Value::Bool(b) => JsonValue::Bool(*b),
        Value::SmallInt(n) => JsonValue::NumberText(alloc::format!("{n}")),
        Value::Int(n) => JsonValue::NumberText(alloc::format!("{n}")),
        Value::BigInt(n) => JsonValue::NumberText(alloc::format!("{n}")),
        Value::Text(s) | Value::BpChar(s) => JsonValue::String(s.to_string()),
        _ => return None,
    })
}

/// v7.39 (round 603) — `to_jsonb` over a scalar, without the round trip.
/// `None` when the argument is not one of the simple kinds.
pub(crate) fn to_jsonb_scalar(v: &Value<'_>) -> Option<Value<'static>> {
    // An integer's canonical jsonb IS its decimal spelling, and a bool's and
    // NULL's are their keywords, so those need no `JsonValue` at all — which
    // is the difference between two allocations and five.
    match v {
        Value::Null => return Some(Value::json(String::from("null"))),
        Value::Bool(b) => {
            return Some(Value::json(String::from(if *b { "true" } else { "false" })));
        }
        Value::SmallInt(n) => return Some(Value::json(alloc::format!("{n}"))),
        Value::Int(n) => return Some(Value::json(alloc::format!("{n}"))),
        Value::BigInt(n) => return Some(Value::json(alloc::format!("{n}"))),
        _ => {}
    }
    simple_scalar_json(v).map(|jv| Value::json(json_canonical_string(&jv)))
}

/// v7.39 (round 603) — `jsonb_build_object` built directly as a value and
/// serialised canonically once, instead of writing `json_build_object`'s
/// spacing and re-parsing it to get jsonb's. The ordering, the last-wins
/// duplicate rule and the number canonicalisation all still come from
/// `write_json_canonical`, so this changes when the parse happens and
/// nothing about what it produces. `None` when any argument is richer than
/// the simple kinds.
pub(crate) fn build_object_canonical(args: &[Value<'_>]) -> Option<Value<'static>> {
    if !args.len().is_multiple_of(2) {
        return None;
    }
    let mut entries: alloc::vec::Vec<(String, JsonValue)> =
        alloc::vec::Vec::with_capacity(args.len() / 2);
    for pair in args.chunks_exact(2) {
        // A NULL key is an error the text path words; leave it there.
        let key = match &pair[0] {
            Value::Text(s) | Value::BpChar(s) => s.to_string(),
            Value::SmallInt(n) => alloc::format!("{n}"),
            Value::Int(n) => alloc::format!("{n}"),
            Value::BigInt(n) => alloc::format!("{n}"),
            _ => return None,
        };
        entries.push((key, simple_scalar_json(&pair[1])?));
    }
    Some(Value::json(json_canonical_string(&JsonValue::Object(
        entries,
    ))))
}

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
    let s = json_canonical_string(v);
    if as_text {
        Value::text(s)
    } else {
        Value::json(s)
    }
}

// ---- v7.38 (read01) — verbatim source extraction for the json accessors ----
//
// PG's `->` / `->>` / `#>` / `#>>` return the EXACT source text of the located
// value, never a re-serialization: `('{"a":{ "b" : 1 }}'::json) -> 'a'` yields
// `{ "b" : 1 }`, `2e2` stays `2e2`, and `{"k":1,"k":2}` keeps both members.
// `jsonb` needs no special case — its stored text is already canonical, so
// slicing that text yields canonical text, exactly as before.
//
// Only containers were wrong: SPG already passed scalars through verbatim.

/// First index at or after `i` that is not JSON whitespace.
fn skip_ws_at(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// `i` sits on the opening quote; returns the index just past the closing
/// quote. Escapes are skipped as a unit so `\"` does not end the string.
fn scan_string(b: &[u8], i: usize) -> Option<usize> {
    debug_assert_eq!(b.get(i), Some(&b'"'));
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// `i` sits on the first byte of a JSON value; returns the index just past its
/// last byte. Containers are matched by depth, ignoring braces inside strings;
/// scalars run to the next structural byte. Multi-byte UTF-8 is safe: its
/// continuation bytes are all >= 0x80 and never collide with the ASCII
/// delimiters tested here.
fn scan_value(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'"' => scan_string(b, i),
        open @ (b'{' | b'[') => {
            let close = if open == b'{' { b'}' } else { b']' };
            let mut depth = 0usize;
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'"' => j = scan_string(b, j)?,
                    c if c == open => {
                        depth += 1;
                        j += 1;
                    }
                    c if c == close => {
                        depth -= 1;
                        j += 1;
                        if depth == 0 {
                            return Some(j);
                        }
                    }
                    _ => j += 1,
                }
            }
            None
        }
        _ => {
            let mut j = i;
            while j < b.len() && !matches!(b[j], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
            {
                j += 1;
            }
            (j > i).then_some(j)
        }
    }
}

/// Decode a JSON string token (including its quotes) into its text value.
fn decode_string_token(tok: &str) -> Option<String> {
    match parse(tok).ok()? {
        JsonValue::String(s) => Some(s),
        _ => None,
    }
}

/// v7.38.8 — does a JSON string TOKEN denote exactly `key`, without
/// building the string it denotes?
///
/// `locate_member` compared keys by calling `decode_string_token` on
/// each one, which runs the whole recursive-descent parser over the
/// token and allocates a `String` — once per member, per row, per
/// accessor, to answer a question that is usually a byte comparison.
///
/// A token with no backslash in it denotes its own inner bytes, so it
/// can be compared in place. One with an escape defers to
/// `decode_string_token`, so the two paths cannot disagree about what
/// an escape means.
fn key_token_eq(tok: &str, key: &str) -> bool {
    let inner = match tok.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        Some(i) => i,
        None => return false,
    };
    if inner.as_bytes().contains(&b'\\') {
        return decode_string_token(tok).is_some_and(|d| d == key);
    }
    inner == key
}

/// Verbatim source slice of `key`'s value in the object encoded at `src`.
/// PG resolves a duplicate key to the LAST occurrence, so the scan does not
/// stop early. Keys are compared after unescaping (`{"A":1}` has key `A`).
fn locate_member<'a>(src: &'a str, key: &str) -> Option<&'a str> {
    let b = src.as_bytes();
    let mut i = skip_ws_at(b, 0);
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    let mut found: Option<&'a str> = None;
    loop {
        i = skip_ws_at(b, i);
        match b.get(i)? {
            b'}' => return found,
            b'"' => {}
            _ => return None,
        }
        let key_end = scan_string(b, i)?;
        let key_tok = src.get(i..key_end)?;
        i = skip_ws_at(b, key_end);
        if b.get(i) != Some(&b':') {
            return None;
        }
        i = skip_ws_at(b, i + 1);
        let val_end = scan_value(b, i)?;
        if key_token_eq(key_tok, key) {
            found = Some(src.get(i..val_end)?);
        }
        i = skip_ws_at(b, val_end);
        match b.get(i)? {
            b',' => i += 1,
            b'}' => return found,
            _ => return None,
        }
    }
}

/// Verbatim source slice of element `idx` in the array encoded at `src`.
/// A negative index counts from the end, as in PG.
fn locate_index(src: &str, idx: i64) -> Option<&str> {
    let b = src.as_bytes();
    let mut i = skip_ws_at(b, 0);
    if b.get(i) != Some(&b'[') {
        return None;
    }
    i += 1;
    let mut spans: Vec<(usize, usize)> = Vec::new();
    loop {
        i = skip_ws_at(b, i);
        if b.get(i)? == &b']' {
            break;
        }
        let end = scan_value(b, i)?;
        spans.push((i, end));
        i = skip_ws_at(b, end);
        match b.get(i)? {
            b',' => i += 1,
            b']' => break,
            _ => return None,
        }
    }
    let n = if idx >= 0 {
        usize::try_from(idx).ok()?
    } else {
        usize::try_from(i64::try_from(spans.len()).ok()? + idx).ok()?
    };
    let (s, e) = *spans.get(n)?;
    src.get(s..e)
}

/// Turn a located verbatim slice into the accessor's result. Containers and
/// scalars alike keep their source text; only `->>` unwraps a string token and
/// maps a JSON `null` to SQL NULL (`->` yields the JSON `null` itself).
fn verbatim_accessor_result(slice: &str, as_text: bool) -> Value<'static> {
    match slice.as_bytes().first() {
        Some(b'n') if slice == "null" => {
            if as_text {
                Value::Null
            } else {
                Value::json("null")
            }
        }
        Some(b'"') if as_text => decode_string_token(slice).map_or(Value::Null, Value::text),
        _ if as_text => Value::text(slice.to_string()),
        _ => Value::json(slice.to_string()),
    }
}

/// Serialise a `JsonValue` in PG's canonical jsonb text form.
fn json_canonical_string(v: &JsonValue) -> String {
    let mut s = String::new();
    write_json_canonical(v, &mut s);
    s
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
            // v7.39 (round 619) — one entry needs neither dedup nor sort, and
            // the `Vec` those need was built for every object on every row.
            if entries.len() <= 1 {
                out.push('{');
                if let Some((k, val)) = entries.first() {
                    write_json_string(k, out);
                    out.push_str(": ");
                    write_json_canonical(val, out);
                }
                out.push('}');
                return;
            }
            write_object_general(entries, out);
        }
    }
}

/// v7.39 (round 619) — the dedup-and-sort object writer. Split out so the
/// one-entry shortcut above can be checked against it directly.
fn write_object_general(entries: &[(String, JsonValue)], out: &mut String) {
    {
        {
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
/// v7.39 (round 619) — a plain integer lexeme is ALREADY the canonical form,
/// so it is handed back borrowed instead of rebuilt.
///
/// The slow body below is unchanged and still decides every other shape; the
/// two are asserted to agree over a generated set in this module's tests, so
/// the shortcut is checked mechanically rather than by reading. Canonicalising
/// `{"a":123}` allocated a `String` here for every number, on every row.
fn canon_json_number(lexeme: &str) -> alloc::borrow::Cow<'_, str> {
    let body = lexeme.strip_prefix('-').unwrap_or(lexeme);
    if !body.is_empty()
        && body.bytes().all(|b| b.is_ascii_digit())
        // A leading zero is only canonical when the whole integer IS zero.
        && (body == "0" || !body.starts_with('0'))
        // `-0` canonicalises to `0`, so it is not a pass-through.
        && !(lexeme.starts_with('-') && body == "0")
    {
        return alloc::borrow::Cow::Borrowed(lexeme);
    }
    alloc::borrow::Cow::Owned(canon_json_number_slow(lexeme))
}

fn canon_json_number_slow(lexeme: &str) -> String {
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
            (
                digits[..digits.len() - shift].to_string(),
                digits[digits.len() - shift..].to_string(),
            )
        } else {
            (
                "0".to_string(),
                alloc::format!("{}{}", "0".repeat(shift - digits.len()), digits),
            )
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
                    "{fn_name}: argument must be JSON / JSONB, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                        JsonValue::Number(_) | JsonValue::NumberText(_) | JsonValue::String(_) => {
                            Some(v.as_text())
                        }
                        JsonValue::Array(_) | JsonValue::Object(_) => {
                            Some(json_canonical_string(&v))
                        }
                    }
                } else {
                    Some(json_canonical_string(&v))
                };
                out.push((k, text));
            }
            Ok(out)
        }
        other => Err(EvalError::TypeMismatch {
            detail: alloc::format!("cannot call {fn_name} on a non-object ({other:?})"),
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
                    "{fn_name}: argument must be JSON / JSONB, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                        JsonValue::Number(_) | JsonValue::NumberText(_) | JsonValue::String(_) => {
                            Some(v.as_text())
                        }
                        JsonValue::Array(_) | JsonValue::Object(_) => {
                            Some(json_canonical_string(&v))
                        }
                    }
                } else {
                    // Plain form renders each element as canonical jsonb.
                    Some(json_canonical_string(&v))
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
                    "JSON path walk: left side must be JSON or TEXT, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
                ),
            });
        }
    };
    // v7.39 (round 769, F31 tranche 5 #135) — PG accepts the path as a
    // real TEXT[] value too (`doc #> ARRAY['a','b']`), not only the
    // `'{a,b}'` literal; a NULL element yields NULL (no such key).
    let owned_steps: Vec<String>;
    let path: Vec<String> = match rhs {
        Value::TextArray(items) => {
            if items.iter().any(Option::is_none) {
                return Ok(Value::Null);
            }
            owned_steps = items.iter().flatten().cloned().collect();
            owned_steps
        }
        Value::Text(s) | Value::Json(s) => parse_text_array(s.as_ref())?,
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "JSON path walk: right side must be TEXT, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
                ),
            });
        }
    };
    // Validate once, then narrow a VERBATIM source slice per step — PG's `#>` /
    // `#>>` return the located value's original text, not a re-serialization.
    validate_unless_known_json(lhs, src, "path walk")?;
    let mut cur: &str = src;
    for step in &path {
        let at = skip_ws_at(cur.as_bytes(), 0);
        let next = match cur.as_bytes().get(at) {
            Some(b'{') => locate_member(cur, step),
            Some(b'[') => match step.parse::<i64>() {
                Ok(idx) => locate_index(cur, idx),
                Err(_) => return Ok(Value::Null),
            },
            _ => return Ok(Value::Null),
        };
        cur = match next {
            None => return Ok(Value::Null),
            Some(slice) => slice,
        };
    }
    Ok(verbatim_accessor_result(cur, as_text))
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
                    "JSON ?: left side must be JSON or TEXT, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                    "JSON ?: right side must be TEXT, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                "JSON ?|/?&: right side must be TEXT[] or TEXT, got {}",
                crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                    "JSON ?|: left side must be JSON or TEXT, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                    "JSON ?&: left side must be JSON or TEXT, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                    "JSON @>: left side must be JSON or TEXT, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                    "JSON @>: right side must be JSON or TEXT, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                    "jsonb =: left side must be JSON or TEXT, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
                ),
            });
        }
    };
    let rhs_text = match rhs {
        Value::Json(s) | Value::Text(s) => s.as_ref(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "jsonb =: right side must be JSON or TEXT, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
        c.into_owned()
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
/// v7.38.8 — validate a document only when it is not already known to be
/// one.
///
/// `Value::Json` reaches an accessor from a json/jsonb column or from a
/// cast, and both of those validate at their own boundary (the column
/// one only since v7.38.8 — before that a jsonb column could hold
/// `{bad`, and this is the guarantee that made re-validating here look
/// necessary). `Value::Text` is SPG's own leniency: PG has no
/// `text -> text` operator at all, so a text operand has passed through
/// no boundary and is checked here.
///
/// The cost this removes is the whole document, per row, per accessor:
/// the parse built a `JsonValue` tree — a Vec plus a String per member —
/// and threw it away, and the verbatim scan below did the real work. On
/// a four-member document that was 333 ns a row against PG's 7.5.
fn validate_unless_known_json(lhs: &Value, src: &str, what: &str) -> Result<(), EvalError> {
    if matches!(lhs, Value::Json(_)) {
        return Ok(());
    }
    parse(src).map(|_| ()).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("invalid JSON for {what}: {e}"),
    })
}

pub fn path_get(lhs: &Value, rhs: &Value, as_text: bool) -> Result<Value<'static>, EvalError> {
    let src = match lhs {
        Value::Json(s) | Value::Text(s) => s.as_ref(),
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "JSON path operator: left side must be JSON or TEXT, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
                ),
            });
        }
    };
    // Validate the document (an invalid one still errors), then extract the
    // located value's VERBATIM source text — PG never re-serializes here.
    validate_unless_known_json(lhs, src, "path access")?;
    let located = match rhs {
        Value::Text(k) => locate_member(src, k),
        Value::Int(idx) => locate_index(src, i64::from(*idx)),
        Value::BigInt(idx) => locate_index(src, *idx),
        Value::Null => return Ok(Value::Null),
        _ => None,
    };
    Ok(located.map_or(Value::Null, |slice| {
        verbatim_accessor_result(slice, as_text)
    }))
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

/// v7.38 (read01 P6.24) — PG's `jsonb` total order (ORDER BY / DISTINCT /
/// btree). First by type rank `Null < String < Number < Boolean < Array <
/// Object`; then within a type: strings by content, numbers numerically,
/// booleans `false < true`, arrays by length then element-wise, objects by
/// pair-count then key/value pairwise (keys in canonical stored order).
/// Mirrors the observable behaviour of `jsonb.c`'s `compareJsonbContainers`.
#[must_use]
pub fn jsonb_compare(a: &JsonValue, b: &JsonValue) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    fn rank(v: &JsonValue) -> u8 {
        match v {
            JsonValue::Null => 0,
            JsonValue::String(_) => 1,
            JsonValue::Number(_) | JsonValue::NumberText(_) => 2,
            JsonValue::Bool(_) => 3,
            JsonValue::Array(_) => 4,
            JsonValue::Object(_) => 5,
        }
    }
    fn num(v: &JsonValue) -> f64 {
        match v {
            JsonValue::Number(x) => *x,
            JsonValue::NumberText(s) => s.parse::<f64>().unwrap_or(0.0),
            _ => 0.0,
        }
    }
    let (ra, rb) = (rank(a), rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (JsonValue::String(x), JsonValue::String(y)) => x.cmp(y),
        (JsonValue::Bool(x), JsonValue::Bool(y)) => x.cmp(y),
        (
            JsonValue::Number(_) | JsonValue::NumberText(_),
            JsonValue::Number(_) | JsonValue::NumberText(_),
        ) => num(a).partial_cmp(&num(b)).unwrap_or(Ordering::Equal),
        (JsonValue::Array(x), JsonValue::Array(y)) => x.len().cmp(&y.len()).then_with(|| {
            x.iter()
                .zip(y.iter())
                .map(|(ea, eb)| jsonb_compare(ea, eb))
                .find(|o| *o != Ordering::Equal)
                .unwrap_or(Ordering::Equal)
        }),
        (JsonValue::Object(x), JsonValue::Object(y)) => x.len().cmp(&y.len()).then_with(|| {
            x.iter()
                .zip(y.iter())
                .map(|((ka, va), (kb, vb))| ka.cmp(kb).then_with(|| jsonb_compare(va, vb)))
                .find(|o| *o != Ordering::Equal)
                .unwrap_or(Ordering::Equal)
        }),
        // Same rank, both Null (or the impossible cross-variant) → equal.
        _ => Ordering::Equal,
    }
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

/// v7.39 (jsonpath depth) — an array subscript bound: a plain index or
/// `last - N` (offset back from the final element).
#[derive(Debug, Clone, Copy)]
enum IdxBound {
    At(usize),
    FromLast(usize),
}

impl IdxBound {
    /// Resolve against an array of `len` items; `None` = out of range.
    fn resolve(self, len: usize) -> Option<usize> {
        match self {
            Self::At(n) => (n < len).then_some(n),
            Self::FromLast(off) => len.checked_sub(1 + off),
        }
    }
}

/// v7.39 (jsonpath depth) — numeric item methods.
#[derive(Debug, Clone, Copy)]
enum NumMethod {
    Abs,
    Floor,
    Ceiling,
    Double,
}

#[derive(Debug, Clone)]
enum PathStep {
    Field(String),
    Index(IdxBound),
    Wildcard,
    // v7.38 (read01, T8) — SQL/JSON path filter sublanguage.
    /// `[N to M]` — an inclusive array-index range (bounds may be `last - k`).
    Range(IdxBound, IdxBound),
    /// `? (<predicate>)` — keep the current items whose accessor expression
    /// satisfies the (possibly `&&`/`||`-combined) predicate.
    Filter(FilterExpr),
    /// `.size()` — the length of an array (or 1 for a scalar, per PG lax mode).
    Size,
    /// `.type()` — the JSON type name of the current item.
    TypeOf,
    /// v7.39 — `.abs()` / `.floor()` / `.ceiling()` / `.double()`.
    Num(NumMethod),
    /// v7.39 — `.**` recursive descent: the item plus every descendant.
    RecursiveAll,
}

#[derive(Debug, Clone)]
struct FilterPred {
    /// Accessor after `@`: empty = `@` itself, `["p"]` = `@.p`, etc.
    path: Vec<String>,
    op: FilterOp,
    val: FilterVal,
    /// v7.39 (read01 jsonpath.c) — `like_regex ... flag "izsq..."`.
    /// Only `i` affects evaluation today; the string round-trips
    /// through the canonical printer.
    regex_flags: Option<String>,
}

/// A filter predicate tree — a single comparison or a `&&`/`||` combination.
#[derive(Debug, Clone)]
enum FilterExpr {
    Cmp(FilterPred),
    And(alloc::boxed::Box<FilterExpr>, alloc::boxed::Box<FilterExpr>),
    Or(alloc::boxed::Box<FilterExpr>, alloc::boxed::Box<FilterExpr>),
}

#[derive(Debug, Clone, Copy)]
enum FilterOp {
    Gt,
    Lt,
    Ge,
    Le,
    Eq,
    Ne,
    /// v7.39 — `starts with "prefix"` (string operand only).
    StartsWith,
    /// v7.39 — `like_regex "pattern"` (POSIX search, unanchored).
    LikeRegex,
}

#[derive(Debug, Clone)]
enum FilterVal {
    Num(f64),
    Str(String),
    Bool(bool),
    /// v7.39 — the `null` literal (`@ == null` matches JSON null only).
    Null,
    /// v7.39 — a `$name` variable reference, resolved from the `vars`
    /// document at evaluation time.
    Var(String),
}

/// v7.39 (round 235) — parse a jsonpath, returning its MODE alongside the
/// steps. Before this round the leading `strict` / `lax` word was stripped
/// and thrown away, so every path evaluated with (incomplete) lax
/// semantics and `strict` was silently a no-op.
fn parse_jsonpath_mode(p: &str) -> Result<(bool, Vec<PathStep>), EvalError> {
    let trimmed = p.trim_start();
    let (strict, p) = if let Some(rest) = trimmed.strip_prefix("strict") {
        (true, rest.trim_start())
    } else if let Some(rest) = trimmed.strip_prefix("lax") {
        (false, rest.trim_start())
    } else {
        (false, trimmed)
    };
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
                // v7.39 — `.**` recursive descent (visits the item and
                // every descendant; a following `.field` then selects).
                if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' {
                    i += 2;
                    steps.push(PathStep::RecursiveAll);
                    continue;
                }
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
                        && chars[i] != '('
                        && !chars[i].is_whitespace()
                    {
                        i += 1;
                    }
                    if start == i {
                        return Err(EvalError::TypeMismatch {
                            detail: "jsonpath: missing field name after '.'".into(),
                        });
                    }
                    let name: String = chars[start..i].iter().collect();
                    // v7.38 (read01, T8) — `.size()` / `.type()` item methods.
                    if i < chars.len() && chars[i] == '(' {
                        i += 1;
                        while i < chars.len() && chars[i] != ')' {
                            i += 1;
                        }
                        if i >= chars.len() {
                            return Err(EvalError::TypeMismatch {
                                detail: "jsonpath: unterminated method call".into(),
                            });
                        }
                        i += 1; // )
                        match name.as_str() {
                            "size" => steps.push(PathStep::Size),
                            "type" => steps.push(PathStep::TypeOf),
                            // v7.39 — numeric item methods.
                            "abs" => steps.push(PathStep::Num(NumMethod::Abs)),
                            "floor" => steps.push(PathStep::Num(NumMethod::Floor)),
                            "ceiling" => steps.push(PathStep::Num(NumMethod::Ceiling)),
                            "double" => steps.push(PathStep::Num(NumMethod::Double)),
                            other => {
                                return Err(EvalError::TypeMismatch {
                                    detail: alloc::format!(
                                        "jsonpath: unsupported method .{other}()"
                                    ),
                                });
                            }
                        }
                    } else {
                        steps.push(PathStep::Field(name));
                    }
                }
            }
            '?' => {
                // v7.38 (read01, T8) — filter `? ( @... <op> <literal> )`.
                i += 1;
                let (pred, ni) = parse_filter_pred(&chars, i)?;
                i = ni;
                steps.push(PathStep::Filter(pred));
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
                    // v7.39 — a bound is `N` or `last[ - K]`.
                    let mut parse_bound = |i: &mut usize| -> Result<IdxBound, EvalError> {
                        jp_skip_ws(&chars, i);
                        if chars[*i..].starts_with(&['l', 'a', 's', 't']) {
                            *i += 4;
                            jp_skip_ws(&chars, i);
                            if *i < chars.len() && chars[*i] == '-' {
                                *i += 1;
                                jp_skip_ws(&chars, i);
                                let s = *i;
                                while *i < chars.len() && chars[*i].is_ascii_digit() {
                                    *i += 1;
                                }
                                let off: usize =
                                    chars[s..*i].iter().collect::<String>().parse().map_err(
                                        |_| EvalError::TypeMismatch {
                                            detail: "jsonpath: invalid `last - N` offset".into(),
                                        },
                                    )?;
                                return Ok(IdxBound::FromLast(off));
                            }
                            return Ok(IdxBound::FromLast(0));
                        }
                        let s = *i;
                        while *i < chars.len() && chars[*i].is_ascii_digit() {
                            *i += 1;
                        }
                        if s == *i {
                            return Err(EvalError::TypeMismatch {
                                detail: "jsonpath: expected `N`, `last[ - K]` or `*` subscript"
                                    .into(),
                            });
                        }
                        Ok(IdxBound::At(
                            chars[s..*i]
                                .iter()
                                .collect::<String>()
                                .parse()
                                .map_err(|_| EvalError::TypeMismatch {
                                    detail: "jsonpath: invalid array index".into(),
                                })?,
                        ))
                    };
                    let idx = parse_bound(&mut i)?;
                    // v7.38 (read01, T8) — `[N to M]` inclusive range.
                    while i < chars.len() && chars[i].is_whitespace() {
                        i += 1;
                    }
                    if i + 1 < chars.len() && chars[i] == 't' && chars[i + 1] == 'o' {
                        i += 2;
                        let hi = parse_bound(&mut i)?;
                        while i < chars.len() && chars[i].is_whitespace() {
                            i += 1;
                        }
                        if i >= chars.len() || chars[i] != ']' {
                            return Err(EvalError::TypeMismatch {
                                detail: "jsonpath: expected ']' after range".into(),
                            });
                        }
                        i += 1;
                        steps.push(PathStep::Range(idx, hi));
                    } else {
                        if i >= chars.len() || chars[i] != ']' {
                            return Err(EvalError::TypeMismatch {
                                detail: "jsonpath: expected ']' after array index".into(),
                            });
                        }
                        i += 1;
                        steps.push(PathStep::Index(idx));
                    }
                }
            }
            c if c.is_whitespace() => {
                i += 1;
            }
            c => {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "jsonpath: unexpected char '{c}' (supports `$.field`, `[N]`, `[N to M]`, `[*]`, `? (...)`, `.size()`, `.type()`)"
                    ),
                });
            }
        }
    }
    Ok((strict, steps))
}

/// Lax-mode convenience for the callers that only need the steps.
fn parse_jsonpath(p: &str) -> Result<Vec<PathStep>, EvalError> {
    parse_jsonpath_mode(p).map(|(_, steps)| steps)
}

fn jp_skip_ws(chars: &[char], i: &mut usize) {
    while *i < chars.len() && chars[*i].is_whitespace() {
        *i += 1;
    }
}

/// v7.38 (read01, T8) — parse a filter body `( <expr> )` starting just after
/// the `?`, where `<expr>` is a comparison of a `@` accessor against a literal,
/// optionally combined with `&&` / `||` and grouped with parentheses.
fn parse_filter_pred(chars: &[char], mut i: usize) -> Result<(FilterExpr, usize), EvalError> {
    let err = |m: &str| EvalError::TypeMismatch {
        detail: alloc::format!("jsonpath filter: {m}"),
    };
    jp_skip_ws(chars, &mut i);
    if i >= chars.len() || chars[i] != '(' {
        return Err(err("expected '(' after '?'"));
    }
    i += 1;
    let (expr, ni) = parse_filter_or(chars, i)?;
    i = ni;
    jp_skip_ws(chars, &mut i);
    if i >= chars.len() || chars[i] != ')' {
        return Err(err("expected ')' to close the filter"));
    }
    i += 1;
    Ok((expr, i))
}

/// `<and> ( '||' <and> )*`
fn parse_filter_or(chars: &[char], i: usize) -> Result<(FilterExpr, usize), EvalError> {
    let (mut left, mut i) = parse_filter_and(chars, i)?;
    loop {
        jp_skip_ws(chars, &mut i);
        if i + 1 < chars.len() && chars[i] == '|' && chars[i + 1] == '|' {
            i += 2;
            let (right, ni) = parse_filter_and(chars, i)?;
            i = ni;
            left = FilterExpr::Or(alloc::boxed::Box::new(left), alloc::boxed::Box::new(right));
        } else {
            return Ok((left, i));
        }
    }
}

/// `<atom> ( '&&' <atom> )*`
fn parse_filter_and(chars: &[char], i: usize) -> Result<(FilterExpr, usize), EvalError> {
    let (mut left, mut i) = parse_filter_atom(chars, i)?;
    loop {
        jp_skip_ws(chars, &mut i);
        if i + 1 < chars.len() && chars[i] == '&' && chars[i + 1] == '&' {
            i += 2;
            let (right, ni) = parse_filter_atom(chars, i)?;
            i = ni;
            left = FilterExpr::And(alloc::boxed::Box::new(left), alloc::boxed::Box::new(right));
        } else {
            return Ok((left, i));
        }
    }
}

/// `'(' <or> ')'` | `@[.field]* <op> <literal>`
fn parse_filter_atom(chars: &[char], mut i: usize) -> Result<(FilterExpr, usize), EvalError> {
    let err = |m: &str| EvalError::TypeMismatch {
        detail: alloc::format!("jsonpath filter: {m}"),
    };
    jp_skip_ws(chars, &mut i);
    if i < chars.len() && chars[i] == '(' {
        i += 1;
        let (expr, ni) = parse_filter_or(chars, i)?;
        i = ni;
        jp_skip_ws(chars, &mut i);
        if i >= chars.len() || chars[i] != ')' {
            return Err(err("expected ')' in grouped predicate"));
        }
        i += 1;
        return Ok((expr, i));
    }
    if i >= chars.len() || chars[i] != '@' {
        return Err(err("only `@`-based predicates are supported"));
    }
    i += 1;
    let mut path: Vec<String> = Vec::new();
    while i < chars.len() && chars[i] == '.' {
        i += 1;
        let start = i;
        while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
            i += 1;
        }
        path.push(chars[start..i].iter().collect());
    }
    let (op, val, regex_flags, ni) = parse_cmp_and_literal(chars, i)?;
    i = ni;
    Ok((
        FilterExpr::Cmp(FilterPred {
            path,
            op,
            val,
            regex_flags,
        }),
        i,
    ))
}

/// Parse a comparison operator and its literal operand (`> 8`, `== "b"`,
/// `>= 3`) starting at `i`; returns the op, the literal and the new index.
/// Shared by the `? (...)` filter parser and the top-level `@@` predicate.
fn parse_cmp_and_literal(
    chars: &[char],
    mut i: usize,
) -> Result<(FilterOp, FilterVal, Option<String>, usize), EvalError> {
    let err = |m: &str| EvalError::TypeMismatch {
        detail: alloc::format!("jsonpath predicate: {m}"),
    };
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    let kw = |i: usize, w: &str| -> bool {
        let wc: Vec<char> = w.chars().collect();
        chars[i..].starts_with(&wc)
    };
    let op = if i + 1 < chars.len() && chars[i] == '>' && chars[i + 1] == '=' {
        i += 2;
        FilterOp::Ge
    } else if i + 1 < chars.len() && chars[i] == '<' && chars[i + 1] == '=' {
        i += 2;
        FilterOp::Le
    } else if i + 1 < chars.len() && chars[i] == '=' && chars[i + 1] == '=' {
        i += 2;
        FilterOp::Eq
    } else if i + 1 < chars.len() && chars[i] == '!' && chars[i + 1] == '=' {
        i += 2;
        FilterOp::Ne
    } else if i < chars.len() && chars[i] == '>' {
        i += 1;
        FilterOp::Gt
    } else if i < chars.len() && chars[i] == '<' {
        i += 1;
        FilterOp::Lt
    // v7.39 — `starts with "prefix"` / `like_regex "pattern"`.
    } else if kw(i, "starts") {
        i += 6;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if !kw(i, "with") {
            return Err(err("expected `with` after `starts`"));
        }
        i += 4;
        FilterOp::StartsWith
    } else if kw(i, "like_regex") {
        i += 10;
        FilterOp::LikeRegex
    } else {
        return Err(err(
            "expected a comparison operator (> < >= <= == != starts with like_regex)",
        ));
    };
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    let val = if i < chars.len() && chars[i] == '"' {
        i += 1;
        let start = i;
        while i < chars.len() && chars[i] != '"' {
            i += 1;
        }
        if i >= chars.len() {
            return Err(err("unterminated string literal"));
        }
        let s: String = chars[start..i].iter().collect();
        i += 1;
        FilterVal::Str(s)
    } else if chars[i..].starts_with(&['t', 'r', 'u', 'e']) {
        i += 4;
        FilterVal::Bool(true)
    } else if chars[i..].starts_with(&['f', 'a', 'l', 's', 'e']) {
        i += 5;
        FilterVal::Bool(false)
    // v7.39 — `null` literal and `$name` variable references.
    } else if chars[i..].starts_with(&['n', 'u', 'l', 'l']) {
        i += 4;
        FilterVal::Null
    } else if i < chars.len() && chars[i] == '$' {
        i += 1;
        let start = i;
        while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
            i += 1;
        }
        if start == i {
            return Err(err("expected a variable name after '$'"));
        }
        FilterVal::Var(chars[start..i].iter().collect())
    } else {
        let start = i;
        if i < chars.len() && (chars[i] == '-' || chars[i] == '+') {
            i += 1;
        }
        while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
            i += 1;
        }
        let num: f64 = chars[start..i]
            .iter()
            .collect::<String>()
            .parse()
            .map_err(|_| err("invalid numeric literal"))?;
        FilterVal::Num(num)
    };
    // v7.39 (read01 jsonpath.c) — optional `flag "..."` after a
    // like_regex pattern.
    let mut flags: Option<String> = None;
    if matches!(op, FilterOp::LikeRegex) {
        let mut j = i;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        if chars[j..].starts_with(&['f', 'l', 'a', 'g']) {
            j += 4;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && chars[j] == '"' {
                j += 1;
                let start = j;
                while j < chars.len() && chars[j] != '"' {
                    j += 1;
                }
                if j < chars.len() {
                    flags = Some(chars[start..j].iter().collect());
                    j += 1;
                    i = j;
                }
            }
        }
    }
    Ok((op, val, flags, i))
}

/// v7.38 (read01, T8) — the PG `.type()` name of a JSON value.
fn json_type_name(v: &JsonValue) -> &'static str {
    match v {
        JsonValue::Null => "null",
        JsonValue::Bool(_) => "boolean",
        JsonValue::Number(_) | JsonValue::NumberText(_) => "number",
        JsonValue::String(_) => "string",
        JsonValue::Array(_) => "array",
        JsonValue::Object(_) => "object",
    }
}

/// Numeric value of a JSON scalar for a filter comparison, if it is a number.
fn json_num(v: &JsonValue) -> Option<f64> {
    match v {
        JsonValue::Number(n) => Some(*n),
        JsonValue::NumberText(s) => s.parse().ok(),
        _ => None,
    }
}

/// Resolve `@.a.b` (the accessor `path`) starting from `node`.
fn resolve_accessor<'a>(node: &'a JsonValue, path: &[String]) -> Option<&'a JsonValue> {
    let mut cur = node;
    for key in path {
        match cur {
            JsonValue::Object(entries) => {
                cur = &entries.iter().find(|(k, _)| k == key)?.1;
            }
            _ => return None,
        }
    }
    Some(cur)
}

/// Evaluate a filter predicate against the current item. `vars` is the
/// jsonb `vars` document (third argument of the jsonb_path_* family);
/// `$name` operands resolve against its top-level keys.
fn filter_matches(node: &JsonValue, pred: &FilterPred, vars: Option<&JsonValue>) -> bool {
    let Some(target) = resolve_accessor(node, &pred.path) else {
        return false;
    };
    // v7.39 — a `$name` operand becomes the literal it refers to.
    let resolved;
    let val = match &pred.val {
        FilterVal::Var(name) => {
            let Some(JsonValue::Object(entries)) = vars else {
                return false;
            };
            let Some((_, v)) = entries.iter().find(|(k, _)| k == name) else {
                return false;
            };
            resolved = match v {
                JsonValue::Number(n) => FilterVal::Num(*n),
                JsonValue::NumberText(s) => match s.parse::<f64>() {
                    Ok(n) => FilterVal::Num(n),
                    Err(_) => return false,
                },
                JsonValue::String(s) => FilterVal::Str(s.clone()),
                JsonValue::Bool(b) => FilterVal::Bool(*b),
                JsonValue::Null => FilterVal::Null,
                _ => return false,
            };
            &resolved
        }
        other => other,
    };
    match val {
        FilterVal::Num(rhs) => match json_num(target) {
            Some(lhs) => match pred.op {
                FilterOp::Gt => lhs > *rhs,
                FilterOp::Lt => lhs < *rhs,
                FilterOp::Ge => lhs >= *rhs,
                FilterOp::Le => lhs <= *rhs,
                FilterOp::Eq => lhs == *rhs,
                FilterOp::Ne => lhs != *rhs,
                FilterOp::StartsWith | FilterOp::LikeRegex => false,
            },
            None => false,
        },
        FilterVal::Str(rhs) => match target {
            JsonValue::String(lhs) => match pred.op {
                FilterOp::Eq => lhs == rhs,
                FilterOp::Ne => lhs != rhs,
                FilterOp::Gt => lhs.as_str() > rhs.as_str(),
                FilterOp::Lt => lhs.as_str() < rhs.as_str(),
                FilterOp::Ge => lhs.as_str() >= rhs.as_str(),
                FilterOp::Le => lhs.as_str() <= rhs.as_str(),
                // v7.39 — string pattern predicates.
                FilterOp::StartsWith => lhs.starts_with(rhs.as_str()),
                FilterOp::LikeRegex => {
                    // v7.39 (read01 jsonpath.c) — the `i` flag folds case
                    // (other flags round-trip but don't alter matching yet).
                    if pred.regex_flags.as_deref().is_some_and(|f| f.contains('i')) {
                        crate::eval::regex_is_match(&rhs.to_lowercase(), &lhs.to_lowercase())
                            .unwrap_or(false)
                    } else {
                        crate::eval::regex_is_match(rhs, lhs).unwrap_or(false)
                    }
                }
            },
            _ => false,
        },
        FilterVal::Bool(rhs) => match target {
            JsonValue::Bool(lhs) => match pred.op {
                FilterOp::Eq => lhs == rhs,
                FilterOp::Ne => lhs != rhs,
                _ => false,
            },
            _ => false,
        },
        // v7.39 — `== null` matches JSON null only; `!= null` any non-null.
        FilterVal::Null => match pred.op {
            FilterOp::Eq => matches!(target, JsonValue::Null),
            FilterOp::Ne => !matches!(target, JsonValue::Null),
            _ => false,
        },
        FilterVal::Var(_) => false, // resolved above
    }
}

/// Evaluate a (possibly `&&`/`||`-combined) filter predicate tree.
fn filter_expr_matches(node: &JsonValue, expr: &FilterExpr, vars: Option<&JsonValue>) -> bool {
    match expr {
        FilterExpr::Cmp(pred) => filter_matches(node, pred, vars),
        FilterExpr::And(a, b) => {
            filter_expr_matches(node, a, vars) && filter_expr_matches(node, b, vars)
        }
        FilterExpr::Or(a, b) => {
            filter_expr_matches(node, a, vars) || filter_expr_matches(node, b, vars)
        }
    }
}

/// Lax evaluation, for the callers that never carried a mode.
fn apply_jsonpath(
    root: &JsonValue,
    steps: &[PathStep],
    vars: Option<&JsonValue>,
) -> Vec<JsonValue> {
    apply_jsonpath_mode(root, steps, vars, false).unwrap_or_default()
}

/// v7.39 (round 235) — jsonpath evaluation with PG's two modes.
///
/// LAX (the default) is forgiving in two specific ways SPG did not
/// implement: a member accessor auto-UNWRAPS an array and applies to each
/// element (`lax $.a` over `[{"a":1}]` yields 1), and an array accessor
/// auto-WRAPS a non-array into a one-element array (`lax $[*]` over `1`
/// yields 1, and over `{"a":1}` yields the object). Both used to return
/// nothing.
///
/// STRICT reports what lax quietly skips. Wording probed off PG18.4:
/// a missing object key, an out-of-bounds subscript, a wildcard on a
/// non-array, a member accessor on a non-object. Filters never error in
/// either mode — a predicate that matches nothing is simply empty.
fn apply_jsonpath_mode(
    root: &JsonValue,
    steps: &[PathStep],
    vars: Option<&JsonValue>,
    strict: bool,
) -> Result<Vec<JsonValue>, EvalError> {
    let err = |m: alloc::string::String| Err(EvalError::TypeMismatch { detail: m });
    let mut cur: Vec<JsonValue> = alloc::vec![root.clone()];
    for step in steps {
        // LAX auto-unwrap / auto-wrap, applied to the inputs of this step.
        if !strict {
            match step {
                // A member accessor looks inside an array's elements.
                PathStep::Field(_) => {
                    let mut flat: Vec<JsonValue> = Vec::new();
                    for node in cur {
                        match node {
                            JsonValue::Array(items) => flat.extend(items),
                            other => flat.push(other),
                        }
                    }
                    cur = flat;
                }
                // An array accessor treats a non-array as a single element.
                PathStep::Wildcard | PathStep::Index(_) | PathStep::Range(..) => {
                    cur = cur
                        .into_iter()
                        .map(|n| match n {
                            arr @ JsonValue::Array(_) => arr,
                            other => JsonValue::Array(alloc::vec![other]),
                        })
                        .collect();
                }
                _ => {}
            }
        } else {
            // STRICT refuses the shapes lax would have adapted.
            for node in &cur {
                match step {
                    PathStep::Field(k) => match node {
                        JsonValue::Object(entries) => {
                            if !entries.iter().any(|(name, _)| name == k) {
                                return err(alloc::format!(
                                    "JSON object does not contain key \"{k}\""
                                ));
                            }
                        }
                        _ => {
                            return err(
                                "jsonpath member accessor can only be applied to an object".into(),
                            );
                        }
                    },
                    PathStep::Wildcard => {
                        if !matches!(node, JsonValue::Array(_)) {
                            return err(
                                "jsonpath wildcard array accessor can only be applied to an array"
                                    .into(),
                            );
                        }
                    }
                    PathStep::Index(idx) => match node {
                        JsonValue::Array(items) => {
                            if idx.resolve(items.len()).is_none_or(|p| p >= items.len()) {
                                return err("jsonpath array subscript is out of bounds".into());
                            }
                        }
                        _ => {
                            return err(
                                "jsonpath array accessor can only be applied to an array".into()
                            );
                        }
                    },
                    PathStep::Range(lo, hi) => match node {
                        JsonValue::Array(items) => {
                            let n = items.len();
                            if lo.resolve(n).is_none_or(|p| p >= n)
                                || hi.resolve(n).is_none_or(|p| p >= n)
                            {
                                return err("jsonpath array subscript is out of bounds".into());
                            }
                        }
                        _ => {
                            return err(
                                "jsonpath array accessor can only be applied to an array".into()
                            );
                        }
                    },
                    _ => {}
                }
            }
        }
        let mut next: Vec<JsonValue> = Vec::new();
        for node in &cur {
            match (step, node) {
                (PathStep::Field(k), JsonValue::Object(entries)) => {
                    if let Some((_, v)) = entries.iter().find(|(name, _)| name == k) {
                        next.push(v.clone());
                    }
                }
                (PathStep::Index(idx), JsonValue::Array(items)) => {
                    if let Some(pos) = idx.resolve(items.len())
                        && let Some(v) = items.get(pos)
                    {
                        next.push(v.clone());
                    }
                }
                (PathStep::Wildcard, JsonValue::Array(items)) => {
                    next.extend(items.iter().cloned());
                }
                // v7.38 (read01, T8) — range / filter / methods.
                (PathStep::Range(lo, hi), JsonValue::Array(items)) => {
                    if let (Some(a), Some(b)) = (lo.resolve(items.len()), hi.resolve(items.len())) {
                        for idx in a..=b {
                            if let Some(v) = items.get(idx) {
                                next.push(v.clone());
                            }
                        }
                    }
                }
                (PathStep::Filter(expr), node) => {
                    if filter_expr_matches(node, expr, vars) {
                        next.push(node.clone());
                    }
                }
                (PathStep::Size, JsonValue::Array(items)) => {
                    next.push(JsonValue::Number(items.len() as f64));
                }
                // PG lax mode: `.size()` of a non-array is 1.
                (PathStep::Size, _) => next.push(JsonValue::Number(1.0)),
                (PathStep::TypeOf, node) => {
                    next.push(JsonValue::String(json_type_name(node).into()));
                }
                // v7.39 — `.**`: the item itself plus all descendants,
                // document order.
                (PathStep::RecursiveAll, node) => {
                    fn descend(v: &JsonValue, out: &mut Vec<JsonValue>) {
                        out.push(v.clone());
                        match v {
                            JsonValue::Object(entries) => {
                                for (_, child) in entries {
                                    descend(child, out);
                                }
                            }
                            JsonValue::Array(items) => {
                                for child in items {
                                    descend(child, out);
                                }
                            }
                            _ => {}
                        }
                    }
                    descend(node, &mut next);
                }
                // v7.39 — numeric item methods (lax: non-numbers drop out).
                (PathStep::Num(m), node) => {
                    let n = match m {
                        // `.double()` also accepts numeric strings.
                        NumMethod::Double => match node {
                            JsonValue::String(s) => s.parse::<f64>().ok(),
                            other => json_num(other),
                        },
                        _ => json_num(node),
                    };
                    if let Some(x) = n {
                        let out = match m {
                            NumMethod::Abs => x.abs(),
                            NumMethod::Floor => x.floor(),
                            NumMethod::Ceiling => x.ceil(),
                            NumMethod::Double => x,
                        };
                        next.push(JsonValue::Number(out));
                    }
                }
                _ => {} // no match at this branch
            }
        }
        cur = next;
        if cur.is_empty() {
            return Ok(Vec::new());
        }
    }
    Ok(cur)
}

/// v7.38 (read01, T8) — evaluate a top-level jsonpath boolean predicate like
/// `$.a > 3` (the form the `@@` operator / jsonb_path_match takes). Returns
/// `Some(bool)` when the path is a top-level comparison, or `None` to let the
/// caller fall back to the ordinary path-query match (`$.a ? (...)` etc.).
pub fn path_predicate(doc: &Value, path: &Value) -> Result<Option<bool>, EvalError> {
    path_predicate_vars(doc, path, None)
}

/// v7.39 — `path_predicate` with a jsonb `vars` document.
pub fn path_predicate_vars(
    doc: &Value,
    path: &Value,
    vars: Option<&JsonValue>,
) -> Result<Option<bool>, EvalError> {
    let (src, ptext) = match (doc, path) {
        (Value::Null, _) | (_, Value::Null) => return Ok(None),
        (Value::Json(s) | Value::Text(s), Value::Text(p) | Value::Json(p)) => (s, p),
        _ => return Ok(None),
    };
    // v7.39 — top-level `exists(<path>)` predicate form.
    let trimmed = ptext.trim();
    if let Some(inner) = trimmed
        .strip_prefix("exists")
        .map(str::trim_start)
        .and_then(|r| r.strip_prefix('('))
        .and_then(|r| r.strip_suffix(')'))
    {
        let (strict, steps) = parse_jsonpath_mode(inner.trim())?;
        let root = parse(src).map_err(|e| EvalError::TypeMismatch {
            detail: alloc::format!("{e}"),
        })?;
        return Ok(Some(
            !apply_jsonpath_mode(&root, &steps, vars, strict)?.is_empty(),
        ));
    }
    let chars: Vec<char> = ptext.chars().collect();
    // Find a top-level comparison operator — depth 0, outside quotes, so a `>`
    // inside a `? (...)` filter or `[...]` does not count.
    let mut depth = 0i32;
    let mut i = 0;
    let mut op_at = None;
    while i < chars.len() {
        match chars[i] {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            '"' => {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    i += 1;
                }
            }
            '>' | '<' | '=' | '!' if depth == 0 => {
                op_at = Some(i);
                break;
            }
            _ => {}
        }
        i += 1;
    }
    let Some(pos) = op_at else { return Ok(None) };
    let left: String = chars[..pos].iter().collect();
    let (strict, steps) = parse_jsonpath_mode(left.trim())?;
    let (op, val, regex_flags, _) = parse_cmp_and_literal(&chars, pos)?;
    let root = parse(src).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("{e}"),
    })?;
    // v7.39 (round 235) — a strict refusal travels out of the predicate
    // too; the `@@` / jsonb_path_match callers turn it into NULL.
    let results = apply_jsonpath_mode(&root, &steps, vars, strict)?;
    let pred = FilterPred {
        path: Vec::new(),
        op,
        val,
        regex_flags,
    };
    Ok(Some(results.iter().any(|v| filter_matches(v, &pred, vars))))
}

/// v7.17.0 Phase 3.9 — `jsonb_path_query(doc, path)` — returns the
/// matched JSON values as a TextArray (each element is the JSON
/// encoding of one match).
pub fn path_query(doc: &Value, path: &Value) -> Result<Value<'static>, EvalError> {
    path_query_vars(doc, path, None)
}

/// v7.39 — parse the `vars` argument of the jsonb_path_* family into a
/// JsonValue object (NULL → no vars).
pub fn parse_path_vars(v: &Value) -> Result<Option<JsonValue>, EvalError> {
    match v {
        Value::Null => Ok(None),
        Value::Json(s) | Value::Text(s) => {
            let parsed = parse(s).map_err(|e| EvalError::TypeMismatch {
                detail: alloc::format!("invalid jsonpath vars document: {e}"),
            })?;
            if !matches!(parsed, JsonValue::Object(_)) {
                return Err(EvalError::TypeMismatch {
                    detail: "jsonpath vars must be a JSON object".into(),
                });
            }
            Ok(Some(parsed))
        }
        other => Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "jsonpath vars must be jsonb, got {}",
                crate::conversions::pg_type_name_for_error_opt(other.data_type())
            ),
        }),
    }
}

/// v7.39 — `path_query` with a jsonb `vars` document ($name references).
pub fn path_query_vars(
    doc: &Value,
    path: &Value,
    vars: Option<&JsonValue>,
) -> Result<Value<'static>, EvalError> {
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
    // v7.39 — a top-level `exists(...)` path yields a single boolean.
    let trimmed = path_text.trim();
    if let Some(inner) = trimmed
        .strip_prefix("exists")
        .map(str::trim_start)
        .and_then(|r| r.strip_prefix('('))
        .and_then(|r| r.strip_suffix(')'))
    {
        let steps = parse_jsonpath(inner.trim())?;
        let hit = !apply_jsonpath(&root, &steps, vars).is_empty();
        return Ok(Value::TextArray(alloc::vec![Some(
            if hit { "true" } else { "false" }.into()
        )]));
    }
    // v7.39 (round 235) — the query family propagates a strict-mode
    // refusal; only path_match / `@?` / `@@` suppress it (see below).
    let (strict, steps) = parse_jsonpath_mode(path_text)?;
    let matches = apply_jsonpath_mode(&root, &steps, vars, strict)?;
    let arr: Vec<Option<String>> = matches
        .into_iter()
        .map(|v| Some(json_canonical_string(&v)))
        .collect();
    Ok(Value::TextArray(arr))
}

/// v7.17.0 Phase 3.9 — `jsonb_path_query_first(doc, path)` returns
/// the first matched JSON value as a Json, or NULL on no match.
pub fn path_query_first(doc: &Value, path: &Value) -> Result<Value<'static>, EvalError> {
    path_query_first_vars(doc, path, None)
}

/// v7.39 — `path_query_first` with a jsonb `vars` document.
pub fn path_query_first_vars(
    doc: &Value,
    path: &Value,
    vars: Option<&JsonValue>,
) -> Result<Value<'static>, EvalError> {
    let q = path_query_vars(doc, path, vars)?;
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
    path_query_array_vars(doc, path, None)
}

/// v7.39 — `path_query_array` with a jsonb `vars` document.
pub fn path_query_array_vars(
    doc: &Value,
    path: &Value,
    vars: Option<&JsonValue>,
) -> Result<Value<'static>, EvalError> {
    let q = path_query_vars(doc, path, vars)?;
    let arr = match q {
        Value::TextArray(items) => {
            let mut buf = String::from("[");
            let mut first = true;
            for s in items.into_iter().flatten() {
                if !first {
                    buf.push_str(", ");
                }
                buf.push_str(&s);
                first = false;
            }
            buf.push(']');
            Value::json(buf)
        }
        other => other,
    };
    // jsonb_path_query_array yields a jsonb array — emit PG-canonical
    // text (`[1, 2, 3]`, `, ` after each element) instead of the raw
    // `,`-joined buffer. Matches jsonb_agg / jsonb_build_array output.
    Ok(canonicalize_value(arr))
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
/// v7.39 (read01 jsonpath.c) — canonicalize a jsonpath literal the way
/// PG's jsonpath output function does: `lax` is the implicit default and
/// is not printed, `strict` is; field accessors always print quoted
/// (`$."a"`); filters print as `?(@ <op> <val>)` with spaces around the
/// operator; `last - k` keeps its spaces. Errors surface as 22P02-shaped
/// syntax errors.
pub fn jsonpath_canonical(input: &str) -> Result<String, EvalError> {
    let trimmed = input.trim();
    let (strict, body) = if let Some(rest) = trimmed.strip_prefix("strict ") {
        (true, rest.trim_start())
    } else if let Some(rest) = trimmed.strip_prefix("lax ") {
        (false, rest.trim_start())
    } else {
        (false, trimmed)
    };
    let steps = parse_jsonpath(body).map_err(|_| {
        // PG reports the first offending token; the first character is
        // a close-enough stand-in for the common shapes.
        let tok: String = body.chars().take(1).collect();
        EvalError::TypeMismatch {
            detail: alloc::format!("syntax error at or near {tok:?} of jsonpath input"),
        }
    })?;
    let mut out = String::new();
    if strict {
        out.push_str("strict ");
    }
    out.push('$');
    fn idx(b: &IdxBound, out: &mut String) {
        match b {
            IdxBound::At(n) => {
                let _ = core::fmt::Write::write_fmt(out, format_args!("{n}"));
            }
            IdxBound::FromLast(0) => out.push_str("last"),
            IdxBound::FromLast(k) => {
                let _ = core::fmt::Write::write_fmt(out, format_args!("last - {k}"));
            }
        }
    }
    fn fval(v: &FilterVal, out: &mut String) {
        match v {
            FilterVal::Num(x) => {
                if x.fract() == 0.0 && x.abs() < 1e15 {
                    let _ = core::fmt::Write::write_fmt(out, format_args!("{}", *x as i64));
                } else {
                    let _ = core::fmt::Write::write_fmt(out, format_args!("{x}"));
                }
            }
            FilterVal::Str(s) => {
                let _ = core::fmt::Write::write_fmt(out, format_args!("{s:?}"));
            }
            FilterVal::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            FilterVal::Null => out.push_str("null"),
            FilterVal::Var(n) => {
                let _ = core::fmt::Write::write_fmt(out, format_args!("$\"{n}\""));
            }
        }
    }
    fn fexpr(e: &FilterExpr, out: &mut String) {
        match e {
            FilterExpr::Cmp(p) => {
                out.push('@');
                for seg in &p.path {
                    let _ = core::fmt::Write::write_fmt(out, format_args!(".\"{seg}\""));
                }
                let op = match p.op {
                    FilterOp::Gt => " > ",
                    FilterOp::Lt => " < ",
                    FilterOp::Ge => " >= ",
                    FilterOp::Le => " <= ",
                    FilterOp::Eq => " == ",
                    FilterOp::Ne => " != ",
                    FilterOp::StartsWith => " starts with ",
                    FilterOp::LikeRegex => " like_regex ",
                };
                out.push_str(op);
                fval(&p.val, out);
                if let Some(f) = &p.regex_flags {
                    let _ = core::fmt::Write::write_fmt(out, format_args!(" flag \"{f}\""));
                }
            }
            FilterExpr::And(l, r) => {
                fexpr(l, out);
                out.push_str(" && ");
                fexpr(r, out);
            }
            FilterExpr::Or(l, r) => {
                fexpr(l, out);
                out.push_str(" || ");
                fexpr(r, out);
            }
        }
    }
    for st in &steps {
        match st {
            PathStep::Field(f) => {
                let _ = core::fmt::Write::write_fmt(&mut out, format_args!(".\"{f}\""));
            }
            PathStep::Index(b) => {
                out.push('[');
                idx(b, &mut out);
                out.push(']');
            }
            PathStep::Wildcard => out.push_str("[*]"),
            PathStep::Range(a, b) => {
                out.push('[');
                idx(a, &mut out);
                out.push_str(" to ");
                idx(b, &mut out);
                out.push(']');
            }
            PathStep::Filter(e) => {
                out.push_str("?(");
                fexpr(e, &mut out);
                out.push(')');
            }
            PathStep::Size => out.push_str(".size()"),
            PathStep::TypeOf => out.push_str(".type()"),
            PathStep::Num(m) => out.push_str(match m {
                NumMethod::Abs => ".abs()",
                NumMethod::Floor => ".floor()",
                NumMethod::Ceiling => ".ceiling()",
                NumMethod::Double => ".double()",
            }),
            PathStep::RecursiveAll => out.push_str(".**"),
        }
    }
    Ok(out)
}

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
        // v7.39 (read01 json.c) — non-finite floats are not legal JSON
        // numbers; PG quotes the canonical spellings ("NaN"/"Infinity").
        Value::Float(x) if !x.is_finite() => {
            let txt = if x.is_nan() {
                "NaN"
            } else if *x > 0.0 {
                "Infinity"
            } else {
                "-Infinity"
            };
            write_json(&JsonValue::String(txt.into()), out);
        }
        Value::Float(x) => out.push_str(&alloc::format!("{x}")),
        Value::Real(x) if !x.is_finite() => {
            let txt = if x.is_nan() {
                "NaN"
            } else if *x > 0.0 {
                "Infinity"
            } else {
                "-Infinity"
            };
            write_json(&JsonValue::String(txt.into()), out);
        }
        Value::Numeric {
            scaled,
            scale,
            kind,
        } => {
            use spg_storage::NumericKind as NK;
            match kind {
                NK::NaN => write_json(&JsonValue::String("NaN".into()), out),
                NK::PosInf => write_json(&JsonValue::String("Infinity".into()), out),
                NK::NegInf => write_json(&JsonValue::String("-Infinity".into()), out),
                // Render the exact decimal text — same shape display uses.
                NK::Finite => out.push_str(&render_numeric(*scaled, *scale)),
            }
        }
        Value::Text(s) => write_json(&JsonValue::String(s.to_string()), out),
        Value::Json(s) => {
            // Pass through verbatim; re-parsing would re-format and
            // drift `1.0` → `1` etc. PG's to_json on a json input is
            // identity.
            out.push_str(s);
        }
        // v7.38 (read01, T9) — a composite encodes as a JSON object keyed by
        // field name (`to_json(row(1,'a'))` → `{"f1":1,"f2":"a"}`).
        Value::Composite(fields) => {
            out.push('{');
            for (i, (name, fv)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(&JsonValue::String(name.clone()), out);
                out.push(':');
                encode_value_into(fv, out);
            }
            out.push('}');
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
        //
        // v7.39 (read01 round 76) — but an ARRAY is a JSON array, not a
        // JSON string. The arms above cover only text/int/bigint arrays;
        // every other element type (bool / float / numeric / date / uuid /
        // …) and every 2-D matrix used to reach this fall-through and come
        // out quoted (`to_jsonb(ARRAY[[1,2]])` → `"{{1,2}}"`). Route them
        // through the shared element menu, recursing per element so nesting
        // and per-type spelling both stay canonical.
        other => {
            if let Some(elems) = crate::eval::values::array_elements(other) {
                out.push('[');
                for (i, e) in elems.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    encode_value_into(e, out);
                }
                out.push(']');
                return;
            }
            let txt = crate::eval::values::value_to_text(other);
            write_json(&JsonValue::String(txt), out);
        }
    }
}

fn render_numeric(scaled: i128, scale: u16) -> String {
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
            let real = if n >= 0 {
                i64::from(n)
            } else {
                len + i64::from(n)
            };
            let filtered: Vec<JsonValue> = items
                .into_iter()
                .enumerate()
                .filter(|(i, _)| *i as i64 != real)
                .map(|(_, v)| v)
                .collect();
            JsonValue::Array(filtered)
        }
        // v7.39 (round 234) — this used to be a silent catch-all
        // (`(other, _) => other`), so every unsupported combination handed
        // the document back untouched. PG names each one (probed 18.4):
        // deleting from a scalar has nowhere to delete from, and an
        // integer index is meaningless on an object.
        (JsonValue::Object(_), Value::Int(_) | Value::SmallInt(_) | Value::BigInt(_)) => {
            return Err(EvalError::TypeMismatch {
                detail: "cannot delete from object using integer index".into(),
            });
        }
        (other, _) if !matches!(other, JsonValue::Object(_) | JsonValue::Array(_)) => {
            return Err(EvalError::TypeMismatch {
                detail: "cannot delete from scalar".into(),
            });
        }
        // An array minus a key, or any other container/operand pairing PG
        // accepts as a no-op, keeps the document.
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
            // v7.38 (read01, T-json-ws) — PG's json_build_object uses `, `
            // between pairs and ` : ` (spaces both sides) around the colon;
            // jsonb_build_object canonicalises this to `: `.
            out.push_str(", ");
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
        out.push_str(" : ");
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
            // v7.38 (read01, T-json-ws) — PG's json_build_array separates
            // elements with `, ` (the jsonb variant canonicalises to the same
            // spacing). to_json / array_to_json stay compact via other paths.
            out.push_str(", ");
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
                    "jsonb_set() create_missing must be BOOL, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
    // v7.39 (round 234) — PG's edge rules for the modification family,
    // probed against 18.4. An EMPTY path is a no-op (SPG replaced the whole
    // document with the new value — silently wrong), and a SCALAR target
    // has nowhere to put a path (SPG returned the scalar unchanged).
    if path.is_empty() {
        return Ok(Value::json(root.to_json_text()));
    }
    if is_json_scalar(&root) {
        return Err(EvalError::TypeMismatch {
            detail: "cannot set path in scalar".into(),
        });
    }
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
            detail: alloc::format!("jsonb_delete_path() takes 2 args, got {}", args.len()),
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
    // v7.39 (round 234) — `#-` on a scalar is an error in PG; SPG handed
    // the scalar back unchanged.
    if is_json_scalar(&root) && !path.is_empty() {
        return Err(EvalError::TypeMismatch {
            detail: "cannot delete path in scalar".into(),
        });
    }
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
                    "jsonb_insert() insert_after must be BOOL, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
                ),
            });
        }
    };
    let doc_text = json_text_arg(&args[0], "jsonb_insert", "target")?;
    let path = path_text_arg(&args[1], "jsonb_insert")?;
    let new_text = json_text_arg(&args[2], "jsonb_insert", "new_value")?;
    let mut root = parse(doc_text).map_err(|e| EvalError::TypeMismatch {
        detail: alloc::format!("jsonb_insert(): invalid JSON target — {e}"),
    })?;
    // v7.39 (round 234) — PG returns the document untouched for an empty
    // path (SPG raised its own error) and refuses a scalar target with the
    // same wording jsonb_set uses.
    if path.is_empty() {
        return Ok(Value::json(root.to_json_text()));
    }
    if is_json_scalar(&root) {
        return Err(EvalError::TypeMismatch {
            detail: "cannot set path in scalar".into(),
        });
    }
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
                "{fname}() {role} must be JSON or TEXT, got {}",
                crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                "{fname}() path must be TEXT[] or TEXT, got {}",
                crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
        let eq = |a: &str, b: &str| json_eq(&parse(a).unwrap(), &parse(b).unwrap());
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
        assert_eq!(
            canon(r#"{"e":"café","t":"a\nb"}"#),
            r#"{"e": "café", "t": "a\nb"}"#
        );
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
    fn path_get_nested_subtree_is_verbatim() {
        // v7.38 (read01) — PG returns the located value's EXACT source text, so
        // a compact source stays compact (verified against PG18.4: `->` on this
        // doc yields `{"x":[1,2]}`, not the canonical `{"x": [1, 2]}`). A jsonb
        // column reaches here already canonicalized, so slicing it still yields
        // canonical text.
        let doc = Value::json::<String>(r#"{"k":{"x":[1,2]}}"#.into());
        let v = path_get(&doc, &Value::text("k"), false).unwrap();
        assert_eq!(v, Value::json::<String>(r#"{"x":[1,2]}"#.into()));

        // A canonical (jsonb-shaped) source slices back to canonical text.
        let canon = Value::json::<String>(r#"{"k": {"x": [1, 2]}}"#.into());
        let v = path_get(&canon, &Value::text("k"), false).unwrap();
        assert_eq!(v, Value::json::<String>(r#"{"x": [1, 2]}"#.into()));

        // Whitespace, number lexemes and duplicate keys all survive; a
        // duplicate key resolves to the LAST occurrence, as in PG.
        let raw = Value::json::<String>(r#"{"a":{ "y" : 2e2 },"k":1,"k":2}"#.into());
        assert_eq!(
            path_get(&raw, &Value::text("a"), false).unwrap(),
            Value::json::<String>(r#"{ "y" : 2e2 }"#.into())
        );
        assert_eq!(
            path_get(&raw, &Value::text("k"), false).unwrap(),
            Value::json::<String>("2".into())
        );

        // `->` on a JSON null yields the JSON null; `->>` yields SQL NULL.
        let n = Value::json::<String>(r#"{"a":null}"#.into());
        assert_eq!(
            path_get(&n, &Value::text("a"), false).unwrap(),
            Value::json::<String>("null".into())
        );
        assert_eq!(path_get(&n, &Value::text("a"), true).unwrap(), Value::Null);
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
                    while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
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
pub fn mysql_path_get<'a>(doc: &'a JsonValue, steps: &[MysqlPathStep]) -> Option<&'a JsonValue> {
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
                    "json_extract() document must be json, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                    "json_extract() paths must be text, got {}",
                    crate::conversions::pg_type_name_for_error_opt(path_v.data_type())
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
                    "json_contains_path() document must be json, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                    "json_contains_path() paths must be text, got {}",
                    crate::conversions::pg_type_name_for_error_opt(path_v.data_type())
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
        // v7.38 (read01, T9) — a composite becomes a JSON object keyed by field
        // name (`row_to_json(row(1,'a'))` → `{"f1":1,"f2":"a"}`).
        Value::Composite(fields) => {
            let mut entries = alloc::vec::Vec::with_capacity(fields.len());
            for (name, fv) in fields.iter() {
                entries.push((name.clone(), value_to_jsonvalue(fv)?));
            }
            JsonValue::Object(entries)
        }
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
                    "{fn_name}() document must be json, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                    "{fn_name}() paths must be text, got {}",
                    crate::conversions::pg_type_name_for_error_opt(pair[0].data_type())
                ),
            });
        };
        let steps = mysql_path_steps(p)?;
        let newval = value_to_jsonvalue(&pair[1])?;
        mutate_at(&mut doc, &steps, mode, &newval);
    }
    // v7.39 (round 392) — MariaDB renders JSON with `": "` / `", "` spacing
    // (`{"a": 1, "b": 2}`); canonicalise so JSON_SET / INSERT / REPLACE /
    // REMOVE match, like JSON_OBJECT (r391).
    Ok(canonicalize_value(Value::Json(alloc::borrow::Cow::Owned(
        doc.to_json_text(),
    ))))
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
                    "json_remove() document must be json, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                    "json_remove() paths must be text, got {}",
                    crate::conversions::pg_type_name_for_error_opt(path_v.data_type())
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
    // v7.39 (round 392) — MariaDB's `": "` / `", "` JSON render spacing.
    Ok(canonicalize_value(Value::Json(alloc::borrow::Cow::Owned(
        doc.to_json_text(),
    ))))
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
                    "{fn_name}() document must be json, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                    "json_array_append() paths must be text, got {}",
                    crate::conversions::pg_type_name_for_error_opt(pair[0].data_type())
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
    // v7.39 (round 392) — MariaDB's `": "` / `", "` JSON render spacing.
    Ok(canonicalize_value(Value::Json(alloc::borrow::Cow::Owned(
        doc.to_json_text(),
    ))))
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
                    "json_array_insert() paths must be text, got {}",
                    crate::conversions::pg_type_name_for_error_opt(pair[0].data_type())
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
    // v7.39 (round 392) — MariaDB's `": "` / `", "` JSON render spacing.
    Ok(canonicalize_value(Value::Json(alloc::borrow::Cow::Owned(
        doc.to_json_text(),
    ))))
}

/// MySQL JSON containment recursion: candidate object ⊆ target
/// object (same keys, contained values); each candidate array
/// element contained in some target array element; a candidate
/// scalar is contained in an array when it equals some element.
fn mysql_contains(target: &JsonValue, cand: &JsonValue) -> bool {
    match (target, cand) {
        (JsonValue::Object(t), JsonValue::Object(c)) => c
            .iter()
            .all(|(ck, cv)| t.iter().any(|(tk, tv)| tk == ck && mysql_contains(tv, cv))),
        (JsonValue::Array(t), JsonValue::Array(c)) => {
            c.iter().all(|cv| t.iter().any(|tv| mysql_contains(tv, cv)))
        }
        (JsonValue::Array(t), scalar) => t.iter().any(|tv| mysql_contains(tv, scalar)),
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
                    "json_contains() {which} must be json, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                    "json_contains() path must be text, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
            detail: alloc::format!("{fn_name}() takes at least 2 documents, got {}", args.len()),
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
                        "{fn_name}() arguments must be json, got {}",
                        crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
    // v7.39 (round 392) — MariaDB's `": "` / `", "` JSON render spacing.
    Ok(canonicalize_value(Value::Json(alloc::borrow::Cow::Owned(
        acc.unwrap().to_json_text(),
    ))))
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
                    "json_overlaps() arguments must be json, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
                ),
            }),
        }
    };
    let a = parse_arg(&args[0])?;
    let b = parse_arg(&args[1])?;
    let overlaps = match (&a, &b) {
        (JsonValue::Array(xs), JsonValue::Array(ys)) => xs.iter().any(|x| {
            ys.iter()
                .any(|y| mysql_contains(x, y) && mysql_contains(y, x))
        }),
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
        ['%', rest @ ..] => (0..=text.len()).any(|skip| like_match(&text[skip..], rest, escape)),
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
                    "json_search() document must be json, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                "json_search() pattern must be text, got {}",
                crate::conversions::pg_type_name_for_error_opt(args[2].data_type())
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
    let start_paths: Vec<String> = args
        .get(4..)
        .unwrap_or(&[])
        .iter()
        .map(|v| match v {
            Value::Text(p) => Ok(p.to_string()),
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "json_search() paths must be text, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                    "json_value() document must be json, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                "json_value() path must be text, got {}",
                crate::conversions::pg_type_name_for_error_opt(args[1].data_type())
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

/// v7.39 (round 234) — a JSON scalar (string / number / boolean / null),
/// i.e. anything that isn't a container. PG refuses every path-based
/// modification against one: there is nowhere for a path to point.
fn is_json_scalar(v: &JsonValue) -> bool {
    !matches!(v, JsonValue::Object(_) | JsonValue::Array(_))
}

#[cfg(test)]
mod round619_number_fast_path {
    use super::*;

    /// v7.39 (round 619) — the borrowed shortcut has to be the same string
    /// the full canonicaliser builds, for every lexeme either might see.
    /// Checked over a generated set rather than by reading the two.
    #[test]
    fn fast_path_agrees_with_the_full_canonicaliser() {
        let mut cases: Vec<String> = Vec::new();
        for sign in ["", "-"] {
            for body in [
                "0",
                "1",
                "7",
                "10",
                "123",
                "0123",
                "00",
                "000",
                "9223372036854775807",
                "170141183460469231731687303715884105727",
                "1.0",
                "1.5",
                "0.5",
                ".5",
                "1.",
                "1e3",
                "1E3",
                "1e-3",
                "1.5e2",
                "1.50",
                "100",
                "0.0",
                "0.00",
                "10.010",
                "1e0",
                "1e+3",
                "0e0",
                "12345678901234567890.12345678901234567890",
            ] {
                cases.push(alloc::format!("{sign}{body}"));
            }
        }
        for c in &cases {
            assert_eq!(
                canon_json_number(c).as_ref(),
                canon_json_number_slow(c).as_str(),
                "lexeme {c:?} canonicalises differently through the shortcut"
            );
        }
    }

    /// The single-entry object writer has to spell what the sorting one does.
    #[test]
    fn one_entry_object_writes_what_the_general_writer_writes() {
        for src in [
            r#"{"a":1}"#,
            r#"{"":1}"#,
            r#"{"a":{"b":2}}"#,
            r#"{"a":[1,2,3]}"#,
            r#"{"a\"b":"c\\d"}"#,
            r#"{"日本":"語"}"#,
            r#"{"a":null}"#,
            r#"{}"#,
        ] {
            let JsonValue::Object(entries) = parse(src).expect("valid json") else {
                panic!("{src} is not an object");
            };
            let mut fast = String::new();
            write_json_canonical(&JsonValue::Object(entries.clone()), &mut fast);
            let mut general = String::new();
            write_object_general(&entries, &mut general);
            assert_eq!(fast, general, "{src}");
        }
    }
}
