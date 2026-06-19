//! v7.37.8(sentori Epic 5 P2)— JSONB → GIN posting-list tokens.
//!
//! Walks a JSONB cell's text form(SPG stores JSON as raw text via
//! `Value::Json`)and emits one canonical token per indexable
//! (path, leaf) pair so the engine's posting-list intersection can
//! resolve `<jsonb_col> @> <jsonb_literal>` to a hot-tier candidate
//! set in O(matching rows)instead of a full table scan.
//!
//! Token shape — deterministic so the insert-maintenance and
//! query-time paths agree byte-for-byte:
//!   * Top-level object key/leaf:  `k:<key>=v:<leaf_canonical>`
//!   * Nested object descent:      `k:<parent>.<child>=v:<leaf>`
//!   * Array element:              `k:<path>[]=v:<elem_canonical>`
//!   * `<leaf_canonical>` is the canonical JSON rendering of the
//!     leaf value (`"s"` for a string carries no quotes, numbers
//!     keep their lexical form, booleans `true`/`false`, null
//!     `null`). Keys are emitted verbatim so case + unicode survive.
//!
//! The walker is intentionally lenient — it accepts any RFC 7159
//! JSON shape we'd already let into a `JSONB` cell. Garbage input
//! returns an empty token list, which is the correct fallback: the
//! row gets no GIN entries, full-scan still covers `@>`.

extern crate alloc;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Public entry point. `text` is the raw JSONB cell payload.
/// Returns every token the GIN posting list should carry for this
/// cell, in deterministic order(walker traversal). Duplicates
/// are NOT de-duplicated here — the caller already does that as
/// part of `Vec::push` + sorted lookup.
#[must_use]
pub fn extract_tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let bytes = text.as_bytes();
    let mut cur = Cursor { bytes, pos: 0 };
    skip_ws(&mut cur);
    let _ = walk(&mut cur, "", &mut out);
    out
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

#[derive(Debug)]
struct LexError;

fn skip_ws(c: &mut Cursor<'_>) {
    while c.pos < c.bytes.len() && matches!(c.bytes[c.pos], b' ' | b'\t' | b'\n' | b'\r') {
        c.pos += 1;
    }
}

fn peek(c: &Cursor<'_>) -> Option<u8> {
    c.bytes.get(c.pos).copied()
}

fn walk(c: &mut Cursor<'_>, path: &str, out: &mut Vec<String>) -> Result<(), LexError> {
    skip_ws(c);
    let head = peek(c).ok_or(LexError)?;
    match head {
        b'{' => walk_object(c, path, out),
        b'[' => walk_array(c, path, out),
        _ => {
            // A bare leaf at the top of `text` carries no key path.
            // Emit it as `v:<leaf>` so the GIN still gets a probe
            // entry for value-only containment(rare for sentori
            // but PG accepts `'1'::jsonb @> '1'::jsonb`).
            let leaf = parse_leaf(c)?;
            if path.is_empty() {
                out.push(format!("v:{leaf}"));
            } else {
                out.push(format!("k:{path}=v:{leaf}"));
            }
            Ok(())
        }
    }
}

fn walk_object(c: &mut Cursor<'_>, path: &str, out: &mut Vec<String>) -> Result<(), LexError> {
    debug_assert_eq!(peek(c), Some(b'{'));
    c.pos += 1;
    skip_ws(c);
    if peek(c) == Some(b'}') {
        c.pos += 1;
        return Ok(());
    }
    loop {
        skip_ws(c);
        let key = parse_string(c)?;
        skip_ws(c);
        if peek(c) != Some(b':') {
            return Err(LexError);
        }
        c.pos += 1;
        skip_ws(c);
        let child_path = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        let nested_head = peek(c).ok_or(LexError)?;
        if nested_head == b'{' || nested_head == b'[' {
            // Recurse — nested objects / arrays just extend the
            // path. The leaf emission happens at the descent's leaf
            // walk.
            walk(c, &child_path, out)?;
        } else {
            let leaf = parse_leaf(c)?;
            out.push(format!("k:{child_path}=v:{leaf}"));
        }
        skip_ws(c);
        match peek(c) {
            Some(b',') => {
                c.pos += 1;
                continue;
            }
            Some(b'}') => {
                c.pos += 1;
                return Ok(());
            }
            _ => return Err(LexError),
        }
    }
}

fn walk_array(c: &mut Cursor<'_>, path: &str, out: &mut Vec<String>) -> Result<(), LexError> {
    debug_assert_eq!(peek(c), Some(b'['));
    c.pos += 1;
    skip_ws(c);
    if peek(c) == Some(b']') {
        c.pos += 1;
        return Ok(());
    }
    let arr_path = format!("{path}[]");
    loop {
        skip_ws(c);
        let elem_head = peek(c).ok_or(LexError)?;
        if elem_head == b'{' || elem_head == b'[' {
            walk(c, &arr_path, out)?;
        } else {
            let leaf = parse_leaf(c)?;
            out.push(format!("k:{arr_path}=v:{leaf}"));
        }
        skip_ws(c);
        match peek(c) {
            Some(b',') => {
                c.pos += 1;
                continue;
            }
            Some(b']') => {
                c.pos += 1;
                return Ok(());
            }
            _ => return Err(LexError),
        }
    }
}

fn parse_string(c: &mut Cursor<'_>) -> Result<String, LexError> {
    if peek(c) != Some(b'"') {
        return Err(LexError);
    }
    c.pos += 1;
    let mut s = String::new();
    while c.pos < c.bytes.len() {
        let b = c.bytes[c.pos];
        c.pos += 1;
        if b == b'"' {
            return Ok(s);
        }
        if b == b'\\' {
            let esc = *c.bytes.get(c.pos).ok_or(LexError)?;
            c.pos += 1;
            match esc {
                b'"' => s.push('"'),
                b'\\' => s.push('\\'),
                b'/' => s.push('/'),
                b'n' => s.push('\n'),
                b't' => s.push('\t'),
                b'r' => s.push('\r'),
                b'b' => s.push('\u{0008}'),
                b'f' => s.push('\u{000c}'),
                b'u' => {
                    // \uXXXX — accept and decode the BMP form.
                    if c.pos + 4 > c.bytes.len() {
                        return Err(LexError);
                    }
                    let mut cp: u32 = 0;
                    for _ in 0..4 {
                        let hx = c.bytes[c.pos];
                        c.pos += 1;
                        let digit = match hx {
                            b'0'..=b'9' => u32::from(hx - b'0'),
                            b'a'..=b'f' => u32::from(hx - b'a') + 10,
                            b'A'..=b'F' => u32::from(hx - b'A') + 10,
                            _ => return Err(LexError),
                        };
                        cp = (cp << 4) | digit;
                    }
                    if let Some(ch) = char::from_u32(cp) {
                        s.push(ch);
                    }
                }
                _ => return Err(LexError),
            }
        } else {
            // Single-byte ASCII or first byte of a UTF-8 sequence.
            // Either way, push as-is; the input is already valid
            // UTF-8 because it came from a `&str`.
            s.push(b as char);
            if b >= 0x80 {
                // Multi-byte UTF-8: copy the continuation bytes.
                let extra = match b {
                    0xC2..=0xDF => 1,
                    0xE0..=0xEF => 2,
                    0xF0..=0xF4 => 3,
                    _ => return Err(LexError),
                };
                // Pop the first byte we incorrectly emitted as
                // `char` and rebuild from the full sequence.
                let start = c.pos - 1;
                let end = start + extra;
                if end > c.bytes.len() {
                    return Err(LexError);
                }
                let slice = &c.bytes[start..=end];
                s.pop();
                s.push_str(core::str::from_utf8(slice).map_err(|_| LexError)?);
                c.pos = end + 1;
            }
        }
    }
    Err(LexError)
}

/// Parse a non-container leaf:`true` / `false` / `null` / number /
/// `"string"`. Returns the canonical text form to embed in the
/// GIN token. Strings come back without surrounding quotes
/// (containment compares by structural equality, so a string
/// `"ios"` matches `'{"team":"ios"}' @> '{"team":"ios"}'` without
/// the quote noise).
fn parse_leaf(c: &mut Cursor<'_>) -> Result<String, LexError> {
    skip_ws(c);
    let head = peek(c).ok_or(LexError)?;
    if head == b'"' {
        return parse_string(c);
    }
    if matches!(head, b't' | b'f' | b'n') {
        return parse_keyword(c);
    }
    parse_number(c)
}

fn parse_keyword(c: &mut Cursor<'_>) -> Result<String, LexError> {
    let head = peek(c).ok_or(LexError)?;
    match head {
        b't' => match_literal(c, "true"),
        b'f' => match_literal(c, "false"),
        b'n' => match_literal(c, "null"),
        _ => Err(LexError),
    }
}

fn match_literal(c: &mut Cursor<'_>, want: &str) -> Result<String, LexError> {
    let end = c.pos + want.len();
    if end > c.bytes.len() {
        return Err(LexError);
    }
    if &c.bytes[c.pos..end] != want.as_bytes() {
        return Err(LexError);
    }
    c.pos = end;
    Ok(want.to_string())
}

fn parse_number(c: &mut Cursor<'_>) -> Result<String, LexError> {
    let start = c.pos;
    if peek(c) == Some(b'-') {
        c.pos += 1;
    }
    let digit_start = c.pos;
    while let Some(b'0'..=b'9') = peek(c) {
        c.pos += 1;
    }
    if c.pos == digit_start {
        return Err(LexError);
    }
    if peek(c) == Some(b'.') {
        c.pos += 1;
        while let Some(b'0'..=b'9') = peek(c) {
            c.pos += 1;
        }
    }
    if matches!(peek(c), Some(b'e' | b'E')) {
        c.pos += 1;
        if matches!(peek(c), Some(b'+' | b'-')) {
            c.pos += 1;
        }
        while let Some(b'0'..=b'9') = peek(c) {
            c.pos += 1;
        }
    }
    let s = core::str::from_utf8(&c.bytes[start..c.pos]).map_err(|_| LexError)?;
    Ok(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn flat_object_emits_key_value_tokens() {
        let toks = extract_tokens(r#"{"team":"ios","sev":"high"}"#);
        assert_eq!(toks, vec!["k:team=v:ios", "k:sev=v:high"]);
    }

    #[test]
    fn array_emits_array_path_tokens() {
        let toks = extract_tokens(r#"["ios","android"]"#);
        assert_eq!(toks, vec!["k:[]=v:ios", "k:[]=v:android"]);
    }

    #[test]
    fn nested_object_uses_dotted_path() {
        let toks = extract_tokens(r#"{"a":{"b":1,"c":"x"}}"#);
        assert!(toks.contains(&"k:a.b=v:1".to_string()));
        assert!(toks.contains(&"k:a.c=v:x".to_string()));
    }

    #[test]
    fn leaf_value_kinds_canonicalise() {
        let toks = extract_tokens(r#"{"a":true,"b":null,"c":42,"d":-3.5}"#);
        assert!(toks.contains(&"k:a=v:true".to_string()));
        assert!(toks.contains(&"k:b=v:null".to_string()));
        assert!(toks.contains(&"k:c=v:42".to_string()));
        assert!(toks.contains(&"k:d=v:-3.5".to_string()));
    }

    #[test]
    fn malformed_text_returns_empty() {
        let toks = extract_tokens("{ this is not json");
        assert!(toks.is_empty());
    }
}
