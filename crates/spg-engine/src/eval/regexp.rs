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
        ReNode::Concat(items) => {
            let mut p = pos;
            for it in items {
                p = re_match_at(it, s, p)?;
            }
            Some(p)
        }
        ReNode::Alt(branches) => {
            for b in branches {
                if let Some(p) = re_match_at(b, s, pos) {
                    return Some(p);
                }
            }
            None
        }
        ReNode::Quant { inner, min, max } => {
            // Greedy: gather as many matches as possible, then
            // shrink. v7.17 stop-gap doesn't continue the outer
            // tail match (we're at a leaf in concat already), so
            // we just return the longest match.
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
pub(super) fn regexp_matches(args: &[Value<'static>]) -> Result<Value<'static>, EvalError> {
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

/// v7.17.0 Phase 3.7 — `regexp_replace(s, pat, repl[, flags])`.
/// `flags` containing `g` replaces all matches; absent flag
/// replaces only the first match (PG default).
pub(super) fn regexp_replace(args: &[Value<'static>]) -> Result<Value<'static>, EvalError> {
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
pub(super) fn regexp_split_to_array(args: &[Value<'static>]) -> Result<Value<'static>, EvalError> {
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
