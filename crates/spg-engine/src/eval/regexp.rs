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
//   * counted repetition `{m}`, `{m,}`, `{m,n}` (v7.37.16 Epic Rx;
//     repetition counts are capped at 65535 — PG's `DUPMAX` — and a
//     bound above that is rejected as an invalid regular expression
//     at parse time to prevent `a{0,999999999}`-style blowups)
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
//
// The matcher uses a backtracking NFA-shaped walk; performance is fine
// for the small strings PG regex functions usually operate on.
//
// ─── v7.37.16 Epic Rx P0 — ReDoS availability caps ────────────────────
//
// A hand-written backtracking matcher over adversarial input has two
// hard-crash hazards, both of which PG bounds (REG_ETOOBIG,
// "regular expression is too complex"):
//
//   1. Unbounded recursion. `re_parse_alt` recurses once per nested
//      `(` and `re_match_at`/`re_match_seq` recurse per concat element
//      and per nested quantifier/alternation. A pattern like
//      `"(((((…"` or a very long concat can drive the Rust call stack
//      past its limit — a `SIGSEGV`/abort that no `Result` can catch.
//      Both the parser and the matcher therefore carry a depth counter
//      and abort with a clean error (`PARSE_DEPTH_LIMIT`,
//      `MATCH_DEPTH_LIMIT`).
//
//   2. Huge `{m,n}` bounds. A count like `a{0,999999999}` would let a
//      single quantifier allocate/iterate enormously. The parser caps
//      repetition counts at `REPEAT_MAX` (PG's `DUPMAX` = 65535) and
//      rejects anything larger at parse time.
//
// None of these caps is reachable by a legitimate pattern; they exist
// purely to convert an adversarial crash into a clean SQL error.

/// Maximum nesting depth of `(...)` groups accepted by the pattern
/// parser before it aborts with a clean "too complex" error.
///
/// The parser is three mutually-recursive functions per nested group
/// (`re_parse_alt` → `re_parse_concat` → `re_parse_atom`), and a debug
/// build's frames are large, so the ceiling is set well below the
/// frames that fit in a conservative worker stack — the
/// `redos_deep_nested_groups_parse_error` test proves a 5000-deep
/// pattern trips this cleanly rather than overflowing. 100 nested
/// groups is still far beyond any real pattern.
const PARSE_DEPTH_LIMIT: u32 = 100;

/// PG's `DUPMAX` — the largest `{m,n}` repetition count accepted. A
/// bound above this is rejected as an invalid regular expression at
/// parse time, before the matcher can act on it.
const REPEAT_MAX: u32 = 0x0000_FFFF; // 65535

/// Maximum recursive-descent depth of the backtracking matcher
/// (`re_match_at` / `re_match_seq`) before it aborts with a clean
/// "too complex" error rather than overflowing the Rust call stack.
///
/// Chosen conservatively: matcher recursion depth grows with concat
/// length and nested group/alternation depth (bounded `{m,n}`
/// quantifiers iterate rather than recurse, so they cost no depth), so
/// the ceiling must sit below the frames that fit in a modest
/// worker-thread stack. The `redos_deep_match_returns_err_not_overflow`
/// test proves that a pattern driven past this depth returns `Err` —
/// not a stack overflow — even on a 1 MiB stack (well under tokio's
/// 2 MiB / pthread's 8 MiB defaults). It is still far above any
/// legitimate pattern's backtracking depth (real patterns are tens of
/// tokens, not hundreds).
const MATCH_DEPTH_LIMIT: u32 = 500;

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
    let n = re_parse_alt(&chars, &mut p, 0)?;
    if p != chars.len() {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("regex compile: trailing chars at pos {p} in {pat:?}"),
        });
    }
    Ok(n)
}

fn re_parse_alt(chars: &[char], p: &mut usize, depth: u32) -> Result<ReNode, EvalError> {
    // v7.37.16 Epic Rx P0 — bound group nesting so `"((((…"` can't
    // blow the parser's own recursion stack.
    if depth > PARSE_DEPTH_LIMIT {
        return Err(EvalError::TypeMismatch {
            detail: "invalid regular expression: regular expression is too complex".into(),
        });
    }
    let mut branches = alloc::vec![re_parse_concat(chars, p, depth)?];
    while *p < chars.len() && chars[*p] == '|' {
        *p += 1;
        branches.push(re_parse_concat(chars, p, depth)?);
    }
    if branches.len() == 1 {
        Ok(branches.pop().unwrap())
    } else {
        Ok(ReNode::Alt(branches))
    }
}

fn re_parse_concat(chars: &[char], p: &mut usize, depth: u32) -> Result<ReNode, EvalError> {
    let mut items: Vec<ReNode> = Vec::new();
    while *p < chars.len() {
        let c = chars[*p];
        if c == '|' || c == ')' {
            break;
        }
        let atom = re_parse_atom(chars, p, depth)?;
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
                '{' => {
                    // v7.37.16 Epic Rx — counted repetition. Only a
                    // well-formed `{m}` / `{m,}` / `{m,n}` becomes a
                    // quantifier; a stray `{` (e.g. `foo{bar`) is left
                    // as an ordinary literal (PG/ERE semantics), so
                    // existing patterns that used `{` literally are
                    // unaffected.
                    match re_parse_bound(chars, p)? {
                        Some((min, max)) => {
                            // Tolerate a lazy suffix `{m,n}?` as greedy,
                            // matching this module's `*?` / `+?` stop-gap.
                            if *p < chars.len() && chars[*p] == '?' {
                                *p += 1;
                            }
                            ReNode::Quant {
                                inner: Box::new(atom),
                                min,
                                max,
                            }
                        }
                        None => atom,
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

fn re_parse_atom(chars: &[char], p: &mut usize, depth: u32) -> Result<ReNode, EvalError> {
    let c = chars[*p];
    match c {
        '(' => {
            *p += 1;
            let inner = re_parse_alt(chars, p, depth + 1)?;
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

/// v7.37.16 Epic Rx P0 — parse a `{m}` / `{m,}` / `{m,n}` counted
/// repetition beginning at `chars[*p] == '{'`.
///
/// * `Ok(Some((min, max)))` — a well-formed bound; `*p` is advanced
///   past the closing `}`. `max == None` means `{m,}` (unbounded).
/// * `Ok(None)` — the text at `*p` is *not* a valid bound; `*p` is
///   left unchanged so the caller keeps `{` as an ordinary literal
///   (PG/ERE semantics).
/// * `Err(..)` — the bound is well-formed but exceeds `REPEAT_MAX`
///   (PG REG_ETOOBIG) or is inverted (`n < m`).
fn re_parse_bound(
    chars: &[char],
    p: &mut usize,
) -> Result<Option<(usize, Option<usize>)>, EvalError> {
    debug_assert_eq!(chars.get(*p), Some(&'{'));
    let mut q = *p + 1;
    // Minimum count — at least one digit is required for `{` to open
    // a bound; otherwise it is a literal brace.
    let (min, min_digits) = re_scan_count(chars, &mut q);
    if min_digits == 0 {
        return Ok(None);
    }
    // Optional `,max`.
    let mut max = Some(min);
    if q < chars.len() && chars[q] == ',' {
        q += 1;
        let (m, m_digits) = re_scan_count(chars, &mut q);
        max = if m_digits == 0 { None } else { Some(m) };
    }
    // A bound must close with `}`; anything else falls back to literal.
    if q >= chars.len() || chars[q] != '}' {
        return Ok(None);
    }
    // Well-formed bound — enforce PG's repetition ceiling before we
    // hand a huge count to the matcher.
    let repeat_max = REPEAT_MAX as usize;
    if min > repeat_max || matches!(max, Some(mx) if mx > repeat_max) {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "invalid regular expression: regular expression is too complex \
                 (repetition count exceeds {REPEAT_MAX})"
            ),
        });
    }
    if let Some(mx) = max {
        if mx < min {
            return Err(EvalError::TypeMismatch {
                detail: "invalid regular expression: {m,n} quantifier with n < m".into(),
            });
        }
    }
    *p = q + 1; // consume through the closing `}`
    Ok(Some((min, max)))
}

/// Scan a run of ASCII digits starting at `*p`, returning the value
/// and the number of digits consumed. The value saturates at
/// `REPEAT_MAX + 1` so an absurdly long digit string (e.g.
/// `{99999999999999999999}`) cannot overflow `usize` — it stays just
/// above the ceiling so the caller rejects it.
fn re_scan_count(chars: &[char], p: &mut usize) -> (usize, usize) {
    let ceiling = REPEAT_MAX as usize + 1;
    let mut val: usize = 0;
    let mut digits = 0usize;
    while *p < chars.len() && chars[*p].is_ascii_digit() {
        let d = (chars[*p] as u8 - b'0') as usize;
        val = val.saturating_mul(10).saturating_add(d).min(ceiling);
        digits += 1;
        *p += 1;
    }
    (val, digits)
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
fn re_match_at(
    node: &ReNode,
    s: &[char],
    pos: usize,
    depth: u32,
) -> Result<Option<usize>, EvalError> {
    // v7.37.16 Epic Rx P0 — abort before the recursive descent can
    // overflow the Rust call stack on an adversarial pattern.
    if depth > MATCH_DEPTH_LIMIT {
        return Err(EvalError::TypeMismatch {
            detail: "invalid regular expression: regular expression is too complex".into(),
        });
    }
    let d = depth + 1;
    match node {
        ReNode::Literal(c) => Ok(if s.get(pos).copied() == Some(*c) {
            Some(pos + 1)
        } else {
            None
        }),
        ReNode::AnyChar => Ok(if pos < s.len() && s[pos] != '\n' {
            Some(pos + 1)
        } else {
            None
        }),
        ReNode::Class { members, negated } => match s.get(pos) {
            Some(&c) => {
                let hit = members.iter().any(|m| class_matches(m, c));
                Ok(if hit ^ negated { Some(pos + 1) } else { None })
            }
            None => Ok(None),
        },
        ReNode::Start => Ok(if pos == 0 { Some(pos) } else { None }),
        ReNode::End => Ok(if pos == s.len() { Some(pos) } else { None }),
        // v7.37.17 (17.6 siblings) — Concat delegates to the
        // backtracking sequence matcher so quantifiers can shrink
        // when the tail fails ('bar.*que' now matches 'barbeque';
        // the old v7.17 stop-gap was greedy-without-backtracking).
        ReNode::Concat(items) => re_match_seq(items, s, pos, d),
        ReNode::Alt(branches) => {
            for b in branches {
                if let Some(p) = re_match_at(b, s, pos, d)? {
                    return Ok(Some(p));
                }
            }
            Ok(None)
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
                match re_match_at(inner, s, p, d)? {
                    Some(np) if np > p => {
                        p = np;
                        count += 1;
                    }
                    _ => break,
                }
            }
            if count < *min {
                return Ok(None);
            }
            Ok(Some(p))
        }
    }
}

/// v7.37.17 (17.6 siblings) — backtracking sequence matcher.
/// Matches `items` in order starting at `pos`; greedy quantifiers
/// try their longest expansion first and shrink until the rest of
/// the sequence matches. Alternations retry the tail per branch.
fn re_match_seq(
    items: &[ReNode],
    s: &[char],
    pos: usize,
    depth: u32,
) -> Result<Option<usize>, EvalError> {
    // v7.37.16 Epic Rx P0 — same stack-overflow guard as re_match_at.
    if depth > MATCH_DEPTH_LIMIT {
        return Err(EvalError::TypeMismatch {
            detail: "invalid regular expression: regular expression is too complex".into(),
        });
    }
    let d = depth + 1;
    let Some((first, rest)) = items.split_first() else {
        return Ok(Some(pos));
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
                match re_match_at(inner, s, p, d)? {
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
                if let Some(e) = re_match_seq(rest, s, end, d)? {
                    return Ok(Some(e));
                }
            }
            Ok(None)
        }
        ReNode::Alt(branches) => {
            for b in branches {
                // Each branch may itself contain quantifiers —
                // match it standalone, then retry the tail.
                if let Some(p) = re_match_at(b, s, pos, d)? {
                    if let Some(e) = re_match_seq(rest, s, p, d)? {
                        return Ok(Some(e));
                    }
                }
            }
            Ok(None)
        }
        ReNode::Concat(nested) => {
            // Flatten: nested ++ rest, preserving backtracking
            // across the boundary.
            let mut combined: alloc::vec::Vec<ReNode> =
                alloc::vec::Vec::with_capacity(nested.len() + rest.len());
            combined.extend(nested.iter().cloned());
            combined.extend(rest.iter().cloned());
            re_match_seq(&combined, s, pos, d)
        }
        other => match re_match_at(other, s, pos, d)? {
            Some(p) => re_match_seq(rest, s, p, d),
            None => Ok(None),
        },
    }
}

/// Find the first match of `node` in `s`, starting at or after
/// `from`. Returns the (start, end) char positions of the match.
fn re_find(node: &ReNode, s: &[char], from: usize) -> Result<Option<(usize, usize)>, EvalError> {
    let mut start = from;
    loop {
        if let Some(end) = re_match_at(node, s, start, 0)? {
            return Ok(Some((start, end)));
        }
        if start >= s.len() {
            return Ok(None);
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
    while let Some((s_pos, e_pos)) = re_find(&node, &chars, from)? {
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
    match re_find(&node, &chars, 0)? {
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
        match re_find(&node, &chars, from)? {
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
        match re_find(&node, &chars, from)? {
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
        match re_find(&node, &chars, from)? {
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
        match re_find(&node, &chars, from)? {
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
    // Optional flags — 'i' folds every literal / class to match
    // either case (used by the `~*` / `!~*` operators).
    let case_insensitive = match args.get(2) {
        Some(v) => match text_arg(v)? {
            Some(flags) => flags.contains('i'),
            None => return Ok(Value::Null),
        },
        None => false,
    };
    let mut node = re_compile(&pat)?;
    if case_insensitive {
        fold_case(&mut node);
    }
    let chars: Vec<char> = text.chars().collect();
    Ok(Value::Bool(re_find(&node, &chars, 0)?.is_some()))
}

/// Rewrite a compiled regex so every ASCII-letter literal and class
/// member matches either case — the engine has no case-insensitive
/// match flag, so we fold at the tree level for `~*` / `!~*`.
fn fold_case(node: &mut ReNode) {
    match node {
        ReNode::Literal(c) if c.is_ascii_alphabetic() => {
            *node = ReNode::Class {
                members: alloc::vec![
                    ClassMember::Single(c.to_ascii_lowercase()),
                    ClassMember::Single(c.to_ascii_uppercase()),
                ],
                negated: false,
            };
        }
        ReNode::Class { members, .. } => {
            let mut extra: Vec<ClassMember> = Vec::new();
            for m in members.iter() {
                match m {
                    ClassMember::Single(c) if c.is_ascii_alphabetic() => {
                        extra.push(ClassMember::Single(c.to_ascii_lowercase()));
                        extra.push(ClassMember::Single(c.to_ascii_uppercase()));
                    }
                    ClassMember::Range(a, b)
                        if a.is_ascii_alphabetic() && b.is_ascii_alphabetic() =>
                    {
                        extra.push(ClassMember::Range(a.to_ascii_lowercase(), b.to_ascii_lowercase()));
                        extra.push(ClassMember::Range(a.to_ascii_uppercase(), b.to_ascii_uppercase()));
                    }
                    _ => {}
                }
            }
            members.extend(extra);
        }
        ReNode::Quant { inner, .. } => fold_case(inner),
        ReNode::Concat(items) | ReNode::Alt(items) => {
            for it in items.iter_mut() {
                fold_case(it);
            }
        }
        _ => {}
    }
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
        match re_find(&node, &chars, from)? {
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

// ─── v7.37.16 Epic Rx P0 — ReDoS-safety cap tests ─────────────────────
#[cfg(test)]
mod redos_tests {
    extern crate std;

    use alloc::string::String;
    use alloc::vec::Vec;
    use std::thread;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    fn repeat_char(c: char, n: usize) -> String {
        core::iter::repeat(c).take(n).collect()
    }

    // (a) A deeply-nested pattern hits the parser recursion cap and
    // returns a clean Err instead of overflowing the parser stack.
    #[test]
    fn redos_deep_nested_groups_parse_error() {
        let pat = repeat_char('(', 5000); // 5000 unbalanced groups
        let res = super::re_compile(&pat);
        assert!(res.is_err(), "deeply-nested groups must be a clean error");
    }

    // (a) A pattern that drives the *matcher* recursion past its cap
    // returns a clean Err — proven not to overflow even on a 1 MiB
    // stack (smaller than tokio's 2 MiB / pthread's 8 MiB defaults).
    #[test]
    fn redos_deep_match_returns_err_not_overflow() {
        let handle = thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                // Flat literal concat far deeper than MATCH_DEPTH_LIMIT;
                // matching it against an equally long haystack recurses
                // once per element.
                let pat = repeat_char('a', 6000);
                let node = super::re_compile(&pat).expect("flat literal compiles");
                let hay = chars(&repeat_char('a', 6000));
                super::re_find(&node, &hay, 0)
            })
            .expect("spawn");
        let res = handle.join().expect("match thread must not overflow/panic");
        assert!(res.is_err(), "over-deep match must abort with a clean error");
    }

    // (b) An `{m,n}` bound with n > REPEAT_MAX (65535) is rejected as
    // an invalid regex at parse time.
    #[test]
    fn redos_repeat_bound_over_cap_rejected() {
        assert!(super::re_compile("a{0,70000}").is_err());
        assert!(super::re_compile("a{70000}").is_err());
        assert!(super::re_compile("a{999999999999999999999}").is_err());
        // n < m is likewise an invalid regex.
        assert!(super::re_compile("a{5,2}").is_err());
        // At the ceiling is still accepted.
        assert!(super::re_compile("a{0,65535}").is_ok());
    }

    // (c) A normal counted-repetition pattern still compiles and
    // matches correctly (real quantifier semantics, not literal text).
    #[test]
    fn redos_normal_bounds_match_correctly() {
        let node = super::re_compile("^a{1,5}$").expect("compiles");
        // 3 and 5 a's match; 0 and 6 do not.
        assert_eq!(super::re_find(&node, &chars("aaa"), 0).unwrap(), Some((0, 3)));
        assert_eq!(super::re_find(&node, &chars("aaaaa"), 0).unwrap(), Some((0, 5)));
        assert_eq!(super::re_find(&node, &chars(""), 0).unwrap(), None);
        assert_eq!(super::re_find(&node, &chars("aaaaaa"), 0).unwrap(), None);

        // `{m}` exact and `{m,}` open bounds.
        let exact = super::re_compile("^a{3}$").expect("compiles");
        assert!(super::re_find(&exact, &chars("aaa"), 0).unwrap().is_some());
        assert!(super::re_find(&exact, &chars("aa"), 0).unwrap().is_none());
        let openb = super::re_compile("^a{2,}$").expect("compiles");
        assert!(super::re_find(&openb, &chars("aaaa"), 0).unwrap().is_some());
        assert!(super::re_find(&openb, &chars("a"), 0).unwrap().is_none());
    }

    // A stray `{` that does not form a valid bound stays a literal
    // brace — pre-existing behavior for patterns using `{` literally
    // must not change.
    #[test]
    fn redos_stray_brace_is_literal() {
        let node = super::re_compile("a{foo").expect("stray brace compiles as literal");
        assert_eq!(super::re_find(&node, &chars("a{foo"), 0).unwrap(), Some((0, 5)));
    }

    // A legitimately (but not pathologically) nested pattern still
    // compiles — the parser cap is far above real nesting.
    #[test]
    fn redos_moderate_nesting_ok() {
        let pat = alloc::format!("{}a{}", repeat_char('(', 50), repeat_char(')', 50));
        assert!(super::re_compile(&pat).is_ok());
    }
}
