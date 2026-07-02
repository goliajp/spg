//! v7.17.0 Phase 3.7 — minimal POSIX-ERE-shaped regex matcher.
//!
//! SPG-engine is `#![no_std]` and has no external regex dependency, so
//! this module hand-implements the subset of PG's regex needed by the
//! dominant customer patterns (see the supported / unsupported syntax
//! list below). Split out of `eval.rs` (cut 23) as a submodule so it
//! keeps `super`-visibility into the shared eval helpers.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use spg_storage::Value;

use super::{EvalError, text_arg};

// ─── v7.17.0 Phase 3.7 — minimal POSIX-ERE-shaped regex matcher ───────
//
// SPG-engine is `#![no_std]` and has no external regex dependency, so
// this module hand-implements the subset of PG's regex needed by the
// dominant customer patterns. Supported syntax:
//
//   * literal characters (with `\.`, `\*`, `\+`, `\?`, `\(`, `\)`,
//     `\[`, `\]`, `\\`, `\^`, `\$`, `\|` escapes)
//   * `.` — any single character
//   * `*`, `+`, `?` — greedy quantifiers
//   * character classes: `[abc]`, `[^abc]`, `[a-z0-9_]`
//   * shortcut classes: `\d` `\D` `\w` `\W` `\s` `\S`
//   * anchors `^` `$`
//   * non-capturing groups `(...)`
//   * alternation `|`
//
// NOT supported in v7.17 (errors clearly):
//   * backreferences `\1`
//   * lookaround `(?=…)` `(?<=…)`
//   * named captures
//   * inline flag groups `(?i)`
//   * lazy quantifiers `*?` `+?` `??` — patterns containing `?` after
//     a quantifier are accepted but interpreted as the greedy form
//     (this is the v7.17 stop-gap; customers needing lazy semantics
//     should preprocess the pattern)
//   * counted repetition `{n,m}`
//
// The matcher uses a backtracking NFA-shaped walk; performance is fine
// for the small strings PG regex functions usually operate on.

#[derive(Debug, Clone)]
enum ReNode {
    /// Single literal byte. ASCII fast-path; non-ASCII falls through
    /// to Any since the engine doesn't decode UTF-8 here.
    Literal(char),
    /// Any single character.
    AnyChar,
    /// Character class: (positive members list, negated flag).
    Class {
        members: Vec<ClassMember>,
        negated: bool,
    },
    /// Anchor start.
    Start,
    /// Anchor end.
    End,
    /// Greedy quantifier.
    Quant {
        inner: Box<ReNode>,
        min: usize,
        max: Option<usize>,
    },
    /// Concatenation of sub-nodes.
    Concat(Vec<ReNode>),
    /// Alternation.
    Alt(Vec<ReNode>),
}

#[derive(Debug, Clone)]
enum ClassMember {
    Single(char),
    Range(char, char),
}

fn re_compile(pat: &str) -> Result<ReNode, EvalError> {
    let chars: Vec<char> = pat.chars().collect();
    let mut p = 0;
    let n = re_parse_alt(&chars, &mut p)?;
    if p != chars.len() {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("regex compile: trailing chars at pos {p} in {pat:?}"),
        });
    }
    Ok(n)
}

fn re_parse_alt(chars: &[char], p: &mut usize) -> Result<ReNode, EvalError> {
    let mut branches = alloc::vec![re_parse_concat(chars, p)?];
    while *p < chars.len() && chars[*p] == '|' {
        *p += 1;
        branches.push(re_parse_concat(chars, p)?);
    }
    if branches.len() == 1 {
        Ok(branches.pop().unwrap())
    } else {
        Ok(ReNode::Alt(branches))
    }
}

fn re_parse_concat(chars: &[char], p: &mut usize) -> Result<ReNode, EvalError> {
    let mut items: Vec<ReNode> = Vec::new();
    while *p < chars.len() {
        let c = chars[*p];
        if c == '|' || c == ')' {
            break;
        }
        let atom = re_parse_atom(chars, p)?;
        // Optional quantifier suffix.
        let quantified = if *p < chars.len() {
            match chars[*p] {
                '*' => {
                    *p += 1;
                    // v7.17 stop-gap: tolerate `*?` lazy quantifier
                    // by treating it as greedy. Skip the trailing
                    // `?` if present.
                    if *p < chars.len() && chars[*p] == '?' {
                        *p += 1;
                    }
                    ReNode::Quant {
                        inner: Box::new(atom),
                        min: 0,
                        max: None,
                    }
                }
                '+' => {
                    *p += 1;
                    if *p < chars.len() && chars[*p] == '?' {
                        *p += 1;
                    }
                    ReNode::Quant {
                        inner: Box::new(atom),
                        min: 1,
                        max: None,
                    }
                }
                '?' => {
                    *p += 1;
                    ReNode::Quant {
                        inner: Box::new(atom),
                        min: 0,
                        max: Some(1),
                    }
                }
                _ => atom,
            }
        } else {
            atom
        };
        items.push(quantified);
    }
    if items.len() == 1 {
        Ok(items.pop().unwrap())
    } else {
        Ok(ReNode::Concat(items))
    }
}

fn re_parse_atom(chars: &[char], p: &mut usize) -> Result<ReNode, EvalError> {
    let c = chars[*p];
    match c {
        '(' => {
            *p += 1;
            let inner = re_parse_alt(chars, p)?;
            if *p >= chars.len() || chars[*p] != ')' {
                return Err(EvalError::TypeMismatch {
                    detail: "regex compile: unmatched '('".into(),
                });
            }
            *p += 1;
            Ok(inner)
        }
        '[' => {
            *p += 1;
            let mut negated = false;
            if *p < chars.len() && chars[*p] == '^' {
                negated = true;
                *p += 1;
            }
            let mut members: Vec<ClassMember> = Vec::new();
            while *p < chars.len() && chars[*p] != ']' {
                let start = chars[*p];
                *p += 1;
                if *p + 1 < chars.len() && chars[*p] == '-' && chars[*p + 1] != ']' {
                    let end = chars[*p + 1];
                    *p += 2;
                    members.push(ClassMember::Range(start, end));
                } else {
                    members.push(ClassMember::Single(start));
                }
            }
            if *p >= chars.len() {
                return Err(EvalError::TypeMismatch {
                    detail: "regex compile: unmatched '['".into(),
                });
            }
            *p += 1; // consume ]
            Ok(ReNode::Class { members, negated })
        }
        '.' => {
            *p += 1;
            Ok(ReNode::AnyChar)
        }
        '^' => {
            *p += 1;
            Ok(ReNode::Start)
        }
        '$' => {
            *p += 1;
            Ok(ReNode::End)
        }
        '\\' => {
            *p += 1;
            if *p >= chars.len() {
                return Err(EvalError::TypeMismatch {
                    detail: "regex compile: dangling backslash".into(),
                });
            }
            let esc = chars[*p];
            *p += 1;
            match esc {
                'd' => Ok(ReNode::Class {
                    members: alloc::vec![ClassMember::Range('0', '9')],
                    negated: false,
                }),
                'D' => Ok(ReNode::Class {
                    members: alloc::vec![ClassMember::Range('0', '9')],
                    negated: true,
                }),
                'w' => Ok(ReNode::Class {
                    members: alloc::vec![
                        ClassMember::Range('a', 'z'),
                        ClassMember::Range('A', 'Z'),
                        ClassMember::Range('0', '9'),
                        ClassMember::Single('_'),
                    ],
                    negated: false,
                }),
                'W' => Ok(ReNode::Class {
                    members: alloc::vec![
                        ClassMember::Range('a', 'z'),
                        ClassMember::Range('A', 'Z'),
                        ClassMember::Range('0', '9'),
                        ClassMember::Single('_'),
                    ],
                    negated: true,
                }),
                's' => Ok(ReNode::Class {
                    members: alloc::vec![
                        ClassMember::Single(' '),
                        ClassMember::Single('\t'),
                        ClassMember::Single('\n'),
                        ClassMember::Single('\r'),
                    ],
                    negated: false,
                }),
                'S' => Ok(ReNode::Class {
                    members: alloc::vec![
                        ClassMember::Single(' '),
                        ClassMember::Single('\t'),
                        ClassMember::Single('\n'),
                        ClassMember::Single('\r'),
                    ],
                    negated: true,
                }),
                other => Ok(ReNode::Literal(other)),
            }
        }
        other => {
            *p += 1;
            Ok(ReNode::Literal(other))
        }
    }
}

fn class_matches(member: &ClassMember, c: char) -> bool {
    match member {
        ClassMember::Single(s) => *s == c,
        ClassMember::Range(a, b) => c >= *a && c <= *b,
    }
}

/// Try to match `node` starting at `pos` in `s`. Returns Some(end)
/// of the matched span (exclusive), or None if no match. Greedy
/// backtracking: each quantifier tries the longest viable repeat
/// and shrinks if the tail doesn't fit.
fn re_match_at(node: &ReNode, s: &[char], pos: usize) -> Option<usize> {
    match node {
        ReNode::Literal(c) => {
            if s.get(pos).copied() == Some(*c) {
                Some(pos + 1)
            } else {
                None
            }
        }
        ReNode::AnyChar => {
            if pos < s.len() && s[pos] != '\n' {
                Some(pos + 1)
            } else {
                None
            }
        }
        ReNode::Class { members, negated } => {
            let c = *s.get(pos)?;
            let hit = members.iter().any(|m| class_matches(m, c));
            if hit ^ negated { Some(pos + 1) } else { None }
        }
        ReNode::Start => {
            if pos == 0 {
                Some(pos)
            } else {
                None
            }
        }
        ReNode::End => {
            if pos == s.len() {
                Some(pos)
            } else {
                None
            }
        }
        // v7.37.17 (17.6 siblings) — Concat delegates to the
        // backtracking sequence matcher so quantifiers can shrink
        // when the tail fails ('bar.*que' now matches 'barbeque';
        // the old v7.17 stop-gap was greedy-without-backtracking).
        ReNode::Concat(items) => re_match_seq(items, s, pos),
        ReNode::Alt(branches) => {
            for b in branches {
                if let Some(p) = re_match_at(b, s, pos) {
                    return Some(p);
                }
            }
            None
        }
        ReNode::Quant { inner, min, max } => {
            // Standalone quantifier (no tail) — the longest match
            // IS correct here; tail interaction is handled by
            // re_match_seq.
            let mut count = 0usize;
            let mut p = pos;
            loop {
                if let Some(cap) = max {
                    if count >= *cap {
                        break;
                    }
                }
                match re_match_at(inner, s, p) {
                    Some(np) if np > p => {
                        p = np;
                        count += 1;
                    }
                    _ => break,
                }
            }
            if count < *min {
                return None;
            }
            Some(p)
        }
    }
}

/// v7.37.17 (17.6 siblings) — backtracking sequence matcher.
/// Matches `items` in order starting at `pos`; greedy quantifiers
/// try their longest expansion first and shrink until the rest of
/// the sequence matches. Alternations retry the tail per branch.
fn re_match_seq(items: &[ReNode], s: &[char], pos: usize) -> Option<usize> {
    let Some((first, rest)) = items.split_first() else {
        return Some(pos);
    };
    match first {
        ReNode::Quant { inner, min, max } => {
            // Enumerate every reachable end position (0, 1, 2, ...
            // repetitions), then try the tail longest-first.
            let mut ends = alloc::vec![pos];
            let mut p = pos;
            let mut count = 0usize;
            loop {
                if let Some(cap) = max {
                    if count >= *cap {
                        break;
                    }
                }
                match re_match_at(inner, s, p) {
                    Some(np) if np > p => {
                        p = np;
                        count += 1;
                        ends.push(p);
                    }
                    _ => break,
                }
            }
            for (reps, &end) in ends.iter().enumerate().rev() {
                if reps < *min {
                    break;
                }
                if let Some(e) = re_match_seq(rest, s, end) {
                    return Some(e);
                }
            }
            None
        }
        ReNode::Alt(branches) => {
            for b in branches {
                // Each branch may itself contain quantifiers —
                // match it standalone, then retry the tail.
                if let Some(p) = re_match_at(b, s, pos) {
                    if let Some(e) = re_match_seq(rest, s, p) {
                        return Some(e);
                    }
                }
            }
            None
        }
        ReNode::Concat(nested) => {
            // Flatten: nested ++ rest, preserving backtracking
            // across the boundary.
            let mut combined: alloc::vec::Vec<ReNode> =
                alloc::vec::Vec::with_capacity(nested.len() + rest.len());
            combined.extend(nested.iter().cloned());
            combined.extend(rest.iter().cloned());
            re_match_seq(&combined, s, pos)
        }
        other => {
            let p = re_match_at(other, s, pos)?;
            re_match_seq(rest, s, p)
        }
    }
}

/// Find the first match of `node` in `s`, starting at or after
/// `from`. Returns the (start, end) char positions of the match.
fn re_find(node: &ReNode, s: &[char], from: usize) -> Option<(usize, usize)> {
    let mut start = from;
    loop {
        if let Some(end) = re_match_at(node, s, start) {
            return Some((start, end));
        }
        if start >= s.len() {
            return None;
        }
        start += 1;
    }
}

/// v7.17.0 Phase 3.7 — `regexp_matches(s, pat)` returns the FIRST
/// match as a single-element TEXT[]. (PG returns one row per match
/// across all captures; SPG simplifies to first-match-only TEXT[].
/// The `g` flag form `regexp_matches(s, pat, 'g')` falls through
/// to all-matches concatenation as a flat array.)
pub(super) fn regexp_matches(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let (text, pat, all_matches) = match args.len() {
        2 => (text_arg(&args[0])?, text_arg(&args[1])?, false),
        3 => {
            let flags = text_arg(&args[2])?.unwrap_or_default();
            (
                text_arg(&args[0])?,
                text_arg(&args[1])?,
                flags.contains('g'),
            )
        }
        n => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!("regexp_matches() takes 2 or 3 args, got {n}"),
            });
        }
    };
    let Some(text) = text else {
        return Ok(Value::Null);
    };
    let Some(pat) = pat else {
        return Ok(Value::Null);
    };
    let node = re_compile(&pat)?;
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Option<String>> = Vec::new();
    let mut from = 0usize;
    while let Some((s_pos, e_pos)) = re_find(&node, &chars, from) {
        out.push(Some(chars[s_pos..e_pos].iter().collect()));
        if !all_matches {
            break;
        }
        // Advance past the match; if zero-width, step one.
        from = if e_pos > s_pos { e_pos } else { e_pos + 1 };
        if from > chars.len() {
            break;
        }
    }
    Ok(Value::TextArray(out))
}

/// v7.37.17 (17.6 siblings) — PG 10+ `regexp_match(s, pat[, flags])`
/// (singular): the FIRST match as a 1-element text[], or SQL NULL
/// when nothing matches. SPG's regex engine reports whole-match
/// spans (capture-group extraction queues with the regex epic), so
/// the array holds the whole match — identical to PG for patterns
/// without parenthesized groups.
pub(super) fn regexp_match(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let (text, pat) = match args.len() {
        2 | 3 => (text_arg(&args[0])?, text_arg(&args[1])?),
        n => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!("regexp_match() takes 2 or 3 args, got {n}"),
            });
        }
    };
    let Some(text) = text else {
        return Ok(Value::Null);
    };
    let Some(pat) = pat else {
        return Ok(Value::Null);
    };
    let node = re_compile(&pat)?;
    let chars: Vec<char> = text.chars().collect();
    match re_find(&node, &chars, 0) {
        Some((s_pos, e_pos)) => Ok(Value::TextArray(alloc::vec![Some(
            chars[s_pos..e_pos].iter().collect(),
        )])),
        None => Ok(Value::Null),
    }
}

/// v7.17.0 Phase 3.7 — `regexp_replace(s, pat, repl[, flags])`.
/// `flags` containing `g` replaces all matches; absent flag
/// replaces only the first match (PG default).
pub(super) fn regexp_replace(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let (text, pat, repl, flags) = match args.len() {
        3 => (
            text_arg(&args[0])?,
            text_arg(&args[1])?,
            text_arg(&args[2])?,
            String::new(),
        ),
        4 => (
            text_arg(&args[0])?,
            text_arg(&args[1])?,
            text_arg(&args[2])?,
            text_arg(&args[3])?.unwrap_or_default(),
        ),
        n => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!("regexp_replace() takes 3 or 4 args, got {n}"),
            });
        }
    };
    let Some(text) = text else {
        return Ok(Value::Null);
    };
    let Some(pat) = pat else {
        return Ok(Value::Null);
    };
    let Some(repl) = repl else {
        return Ok(Value::Null);
    };
    let global = flags.contains('g');
    let node = re_compile(&pat)?;
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut from = 0usize;
    loop {
        match re_find(&node, &chars, from) {
            Some((s_pos, e_pos)) => {
                out.extend(chars[from..s_pos].iter());
                out.push_str(&repl);
                let step = if e_pos > s_pos { e_pos } else { e_pos + 1 };
                from = step;
                if !global {
                    if from <= chars.len() {
                        out.extend(chars[from..].iter());
                    }
                    return Ok(Value::text(out));
                }
                if from > chars.len() {
                    break;
                }
            }
            None => {
                out.extend(chars[from..].iter());
                break;
            }
        }
    }
    Ok(Value::text(out))
}

/// v7.17.0 Phase 3.7 — `regexp_split_to_array(s, pat)`. Returns
/// TEXT[] of the pieces between matches.
pub(super) fn regexp_split_to_array(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("regexp_split_to_array() takes 2 args, got {}", args.len()),
        });
    }
    let text = text_arg(&args[0])?;
    let pat = text_arg(&args[1])?;
    let Some(text) = text else {
        return Ok(Value::Null);
    };
    let Some(pat) = pat else {
        return Ok(Value::Null);
    };
    let node = re_compile(&pat)?;
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Option<String>> = Vec::new();
    let mut piece_start = 0usize;
    let mut from = 0usize;
    loop {
        match re_find(&node, &chars, from) {
            Some((s_pos, e_pos)) => {
                let piece: String = chars[piece_start..s_pos].iter().collect();
                out.push(Some(piece));
                let step = if e_pos > s_pos { e_pos } else { e_pos + 1 };
                from = step;
                piece_start = step;
                if from > chars.len() {
                    break;
                }
            }
            None => {
                let tail: String = chars[piece_start..].iter().collect();
                out.push(Some(tail));
                break;
            }
        }
    }
    Ok(Value::TextArray(out))
}

/// v7.37.17 (17.6 siblings) — PG 15+ `regexp_instr(source, pattern
/// [, start [, N [, endoption [, flags]]]])` returns the 1-based
/// index of the start (or end, if `endoption=1`) of the Nth match.
/// Returns 0 if no match.
pub(super) fn regexp_instr(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() < 2 || args.len() > 6 {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "regexp_instr() takes 2-6 args, got {}",
                args.len()
            ),
        });
    }
    let text = text_arg(&args[0])?;
    let pat = text_arg(&args[1])?;
    let Some(text) = text else {
        return Ok(Value::Null);
    };
    let Some(pat) = pat else {
        return Ok(Value::Null);
    };
    fn int_arg(v: &Value<'_>) -> Result<Option<i64>, EvalError> {
        match v {
            Value::Null => Ok(None),
            Value::SmallInt(n) => Ok(Some(i64::from(*n))),
            Value::Int(n) => Ok(Some(i64::from(*n))),
            Value::BigInt(n) => Ok(Some(*n)),
            _ => Err(EvalError::TypeMismatch {
                detail: "regexp_instr(): integer arg required".into(),
            }),
        }
    }
    let start_1based = if args.len() >= 3 {
        match int_arg(&args[2])? {
            None => return Ok(Value::Null),
            Some(n) => n,
        }
    } else {
        1
    };
    let nth = if args.len() >= 4 {
        match int_arg(&args[3])? {
            None => return Ok(Value::Null),
            Some(n) => n,
        }
    } else {
        1
    };
    let endoption = if args.len() >= 5 {
        match int_arg(&args[4])? {
            None => return Ok(Value::Null),
            Some(n) => n,
        }
    } else {
        0
    };
    if start_1based < 1 || nth < 1 {
        return Err(EvalError::TypeMismatch {
            detail: "regexp_instr(): start and N must be >= 1".into(),
        });
    }
    let node = re_compile(&pat)?;
    let chars: Vec<char> = text.chars().collect();
    let mut from = (start_1based - 1) as usize;
    let mut hits = 0i64;
    loop {
        match re_find(&node, &chars, from) {
            Some((s_pos, e_pos)) => {
                hits += 1;
                if hits == nth {
                    let idx = if endoption == 1 { e_pos } else { s_pos };
                    return Ok(Value::Int((idx + 1) as i32));
                }
                let step = if e_pos > s_pos { e_pos } else { e_pos + 1 };
                from = step;
                if from > chars.len() {
                    break;
                }
            }
            None => break,
        }
    }
    Ok(Value::Int(0))
}

/// v7.37.17 (17.6 siblings) — PG 15+ `regexp_substr(source, pattern
/// [, start [, N [, flags]]])` returns the Nth match as TEXT.
/// Returns NULL if no match.
pub(super) fn regexp_substr(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() < 2 || args.len() > 5 {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "regexp_substr() takes 2-5 args, got {}",
                args.len()
            ),
        });
    }
    let text = text_arg(&args[0])?;
    let pat = text_arg(&args[1])?;
    let Some(text) = text else {
        return Ok(Value::Null);
    };
    let Some(pat) = pat else {
        return Ok(Value::Null);
    };
    fn int_arg(v: &Value<'_>) -> Result<Option<i64>, EvalError> {
        match v {
            Value::Null => Ok(None),
            Value::SmallInt(n) => Ok(Some(i64::from(*n))),
            Value::Int(n) => Ok(Some(i64::from(*n))),
            Value::BigInt(n) => Ok(Some(*n)),
            _ => Err(EvalError::TypeMismatch {
                detail: "regexp_substr(): integer arg required".into(),
            }),
        }
    }
    let start_1based = if args.len() >= 3 {
        match int_arg(&args[2])? {
            None => return Ok(Value::Null),
            Some(n) => n,
        }
    } else {
        1
    };
    let nth = if args.len() >= 4 {
        match int_arg(&args[3])? {
            None => return Ok(Value::Null),
            Some(n) => n,
        }
    } else {
        1
    };
    if start_1based < 1 || nth < 1 {
        return Err(EvalError::TypeMismatch {
            detail: "regexp_substr(): start and N must be >= 1".into(),
        });
    }
    let node = re_compile(&pat)?;
    let chars: Vec<char> = text.chars().collect();
    let mut from = (start_1based - 1) as usize;
    let mut hits = 0i64;
    loop {
        match re_find(&node, &chars, from) {
            Some((s_pos, e_pos)) => {
                hits += 1;
                if hits == nth {
                    let substr: String = chars[s_pos..e_pos].iter().collect();
                    return Ok(Value::text(substr));
                }
                let step = if e_pos > s_pos { e_pos } else { e_pos + 1 };
                from = step;
                if from > chars.len() {
                    break;
                }
            }
            None => break,
        }
    }
    Ok(Value::Null)
}

/// v7.37.17 (17.6 siblings) — PG 15+ `regexp_like(source, pattern
/// [, flags])` returns TRUE if the pattern matches anywhere in
/// source; FALSE otherwise.
pub(super) fn regexp_like(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() < 2 || args.len() > 3 {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "regexp_like() takes 2 or 3 args, got {}",
                args.len()
            ),
        });
    }
    let text = text_arg(&args[0])?;
    let pat = text_arg(&args[1])?;
    let Some(text) = text else {
        return Ok(Value::Null);
    };
    let Some(pat) = pat else {
        return Ok(Value::Null);
    };
    let node = re_compile(&pat)?;
    let chars: Vec<char> = text.chars().collect();
    Ok(Value::Bool(re_find(&node, &chars, 0).is_some()))
}

/// v7.37.17 (17.6 siblings) — PG 15+ `regexp_count(source, pattern)`
/// returns the number of matches. Optional third arg for start
/// position (1-based); optional fourth for flags (currently
/// ignored — SPG's re engine has no case-insensitive flag).
pub(super) fn regexp_count(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    if args.len() < 2 || args.len() > 4 {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "regexp_count() takes 2-4 args, got {}",
                args.len()
            ),
        });
    }
    let text = text_arg(&args[0])?;
    let pat = text_arg(&args[1])?;
    let Some(text) = text else {
        return Ok(Value::Null);
    };
    let Some(pat) = pat else {
        return Ok(Value::Null);
    };
    let start_1based = if args.len() >= 3 {
        match &args[2] {
            Value::Null => return Ok(Value::Null),
            Value::Int(n) => *n as i64,
            Value::BigInt(n) => *n,
            _ => {
                return Err(EvalError::TypeMismatch {
                    detail: "regexp_count(): start must be integer".into(),
                });
            }
        }
    } else {
        1
    };
    if start_1based < 1 {
        return Err(EvalError::TypeMismatch {
            detail: "regexp_count(): start must be >= 1".into(),
        });
    }
    let node = re_compile(&pat)?;
    let chars: Vec<char> = text.chars().collect();
    let mut count: i64 = 0;
    let mut from = (start_1based - 1) as usize;
    loop {
        match re_find(&node, &chars, from) {
            Some((s_pos, e_pos)) => {
                count += 1;
                let step = if e_pos > s_pos { e_pos } else { e_pos + 1 };
                from = step;
                if from > chars.len() {
                    break;
                }
            }
            None => break,
        }
    }
    Ok(Value::BigInt(count))
}
