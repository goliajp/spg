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
//   * `*`, `+`, `?` — greedy quantifiers, plus their lazy /
//     non-greedy `*?` `+?` `??` forms (match the fewest reps, take
//     more only when the continuation fails; PG18-compatible)
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

/// Maximum total number of backtracking steps (matcher entries) the
/// engine will spend on a single `re_find` invocation before it aborts
/// with a clean "too complex" error.
///
/// v7.37.16 Epic Rx P0 — this is the TIME bound, independent of and
/// complementary to `MATCH_DEPTH_LIMIT` (the STACK bound). Catastrophic
/// backtracking (`(a+)+$`, `(a|aa)*b`, …) recurses only shallowly but
/// explores exponentially many paths, so a depth cap alone leaves the
/// matcher able to burn CPU without limit. A single monotonic counter,
/// incremented once per `re_match_at`/`re_match_seq` entry and shared
/// across ALL backtracking branches and ALL start positions of one
/// find, caps the total work so a runaway pattern fails fast instead of
/// hanging the connection thread.
///
/// Chosen generously: 10 million steps is orders of magnitude above any
/// legitimate pattern×input (a linear, non-pathological match spends
/// roughly O(pattern × input) entries — thousands, not millions, even
/// for large inputs), yet a modern core executes it in well under a
/// second, so an exponential backtracker aborts near-instantly. The
/// `redos_catastrophic_backtracking_returns_err_fast` test proves a
/// classic ReDoS pattern trips this bound quickly rather than hanging.
const MATCH_STEP_LIMIT: u64 = 10_000_000;

/// v7.37.16 Epic Rx P2-⑧ (`checkmatchall`) — the largest input length
/// for which the dot-repetition length short-circuit is engaged.
///
/// The short-circuit replaces the backtracker for a whole-string match
/// against a fully-anchored pure dot-repetition (`^.*$`, `^.{m,n}$`, …).
/// It must produce byte-for-byte the SAME answer the backtracker would —
/// including the backtracker's ReDoS `MATCH_STEP_LIMIT` behavior. For a
/// pattern with at most one variable-width quantifier (the restriction
/// `matchall_length_bounds` enforces) the backtracker spends O(len)
/// steps, so gating the short-circuit at a length well below the step
/// budget guarantees the backtracker would NOT have erred at this input —
/// hence the short-circuit's bool is exactly the backtracker's bool.
/// Above this length we fall through to the backtracker unchanged, so
/// whatever it does (match / non-match / step-budget error) is preserved.
/// 2_000_000 leaves a ~5× margin under the 10M step budget.
const MATCHALL_SAFE_LEN: u64 = 2_000_000;

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
    /// Word-boundary zero-width assertion (PG ARE `\y \m \M \b \B \Y`).
    /// Consumes no input; asserts on the word-ness of the chars flanking
    /// the current position. See `WordBoundaryKind`.
    WordBoundary(WordBoundaryKind),
    /// Repetition quantifier. `greedy` = the PG default (`X*`, `X+`,
    /// `X?`, `X{m,n}`): match as MANY reps as possible, give back on
    /// backtrack. `greedy == false` is the lazy / non-greedy form (the
    /// `?`-suffixed `X*?`, `X+?`, `X??`, `X{m,n}?`): match as FEW reps
    /// as possible, take more only when the continuation fails. Both
    /// forms reach the SAME set of end positions and run under the SAME
    /// ReDoS step/depth guards — only the order the matcher tries those
    /// positions differs (longest-first vs shortest-first).
    Quant {
        inner: Box<ReNode>,
        min: usize,
        max: Option<usize>,
        greedy: bool,
    },
    /// Concatenation of sub-nodes.
    Concat(Vec<ReNode>),
    /// Alternation.
    Alt(Vec<ReNode>),
    /// Zero-width lookahead assertion. `(?=inner)` (negative == false) succeeds
    /// at a position iff `inner` matches starting there; `(?!inner)` (negative
    /// == true) succeeds iff it does NOT. Either way it consumes no input. Runs
    /// under the same ReDoS step/depth budget as every other node.
    Lookahead {
        negative: bool,
        inner: Box<ReNode>,
    },
}

#[derive(Debug, Clone)]
enum ClassMember {
    Single(char),
    Range(char, char),
    /// A shortcut-class complement used *inside* a bracket expression:
    /// `[\D]`, `[\W]`, `[\S]`. Matches iff the char is NOT in any of the
    /// held sub-members. PG ARE recognises these shortcuts within
    /// `[...]` (e.g. `[\D]` = a non-digit); the positive forms
    /// (`\d`/`\w`/`\s`) expand inline into ordinary Single/Range members
    /// and need no variant. Union semantics across the whole class are
    /// preserved: `[a\D]` matches `'a'` OR any non-digit.
    NotInSet(Vec<ClassMember>),
}

/// PG ARE word-boundary assertion flavours (regc_locale.c semantics).
/// A "word character" is `[[:alnum:]_]`; `before`/`after` = whether the
/// char immediately left/right of the position is a word char (false at a
/// string edge). All are zero-width — they match a position, not a char.
/// (PG's `\b`/`\B` are NOT word boundaries in ARE — they are the backspace
/// char and a literal backslash respectively — so they are not here.)
#[derive(Debug, Clone, Copy)]
enum WordBoundaryKind {
    /// `\y` — at a word boundary: `before != after`.
    Boundary,
    /// `\Y` — NOT a word boundary: `before == after`.
    NonBoundary,
    /// `\m` — beginning of a word: `!before && after`.
    BegWord,
    /// `\M` — end of a word: `before && !after`.
    EndWord,
}

/// PG word character: alphanumeric or underscore. ASCII-scoped to match
/// this engine's `\w`/`[[:alnum:]]` handling (both ASCII-only here).
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn re_compile(pat: &str) -> Result<ReNode, EvalError> {
    let all: Vec<char> = pat.chars().collect();
    // Leading inline option group `(?flags)` applies to the whole pattern. `i`
    // (case-insensitive) and `x` (extended / whitespace-ignoring) change
    // matching; the rest (m/s/n/…) are accepted and ignored. A `(?:…)`
    // non-capturing group is NOT an option group (its `:` isn't a flag letter)
    // — it is handled in re_parse_atom, so this leading-flag scan skips it.
    let mut fold = false;
    let mut extended = false;
    let mut start = 0;
    if all.len() >= 3 && all[0] == '(' && all[1] == '?' {
        if let Some(close) = all[2..].iter().position(|&c| c == ')') {
            let flags = &all[2..2 + close];
            if !flags.is_empty() && flags.iter().all(|c| "bceimnpqstwx".contains(*c)) {
                fold = flags.contains(&'i');
                extended = flags.contains(&'x');
                start = 2 + close + 1;
            }
        }
    }
    // v7.38 (read01 P6.11) — `x` extended mode: unescaped whitespace outside a
    // character class is ignored and `#` starts a comment to end-of-line, so a
    // pattern can be laid out readably. Previously `x` was silently dropped,
    // which made a spaced-out pattern fail to match instead of matching.
    let body: Vec<char> = if extended {
        strip_regex_extended_whitespace(&all[start..])
    } else {
        all[start..].to_vec()
    };
    let mut p = 0;
    let mut n = re_parse_alt(&body, &mut p, 0)?;
    if p != body.len() {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("regex compile: trailing chars at pos {p} in {pat:?}"),
        });
    }
    if fold {
        fold_case(&mut n);
    }
    Ok(n)
}

/// v7.38 (read01 P6.11) — implement the regex `x` (extended) flag: drop
/// unescaped whitespace and `#`-to-EOL comments, but keep whitespace that is
/// escaped (`\ `) or inside a `[...]` character class, matching PG / POSIX ARE.
fn strip_regex_extended_whitespace(chars: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(chars.len());
    let mut in_class = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            // Escaped pair is literal — keep both characters verbatim.
            out.push(c);
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if in_class {
            out.push(c);
            if c == ']' {
                in_class = false;
            }
            i += 1;
            continue;
        }
        match c {
            '[' => {
                in_class = true;
                out.push(c);
            }
            ' ' | '\t' | '\n' | '\r' | '\x0c' => {} // ignore unescaped whitespace
            '#' => {
                // Comment to end-of-line.
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            _ => out.push(c),
        }
        i += 1;
    }
    out
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
                    // A trailing `?` makes the quantifier lazy
                    // (non-greedy): `X*?`. Consume it and record the
                    // laziness on the node.
                    let greedy = !consume_lazy_suffix(chars, p);
                    ReNode::Quant {
                        inner: Box::new(atom),
                        min: 0,
                        max: None,
                        greedy,
                    }
                }
                '+' => {
                    *p += 1;
                    let greedy = !consume_lazy_suffix(chars, p);
                    ReNode::Quant {
                        inner: Box::new(atom),
                        min: 1,
                        max: None,
                        greedy,
                    }
                }
                '?' => {
                    *p += 1;
                    // `X??` — lazy optional. The second `?` is the
                    // laziness marker, not a literal.
                    let greedy = !consume_lazy_suffix(chars, p);
                    ReNode::Quant {
                        inner: Box::new(atom),
                        min: 0,
                        max: Some(1),
                        greedy,
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
                            // A trailing `?` makes the counted
                            // repetition lazy: `X{m,n}?`.
                            let greedy = !consume_lazy_suffix(chars, p);
                            ReNode::Quant {
                                inner: Box::new(atom),
                                min,
                                max,
                                greedy,
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
            // `(?...)` prefixes: `(?:` non-capturing group, `(?=`/`(?!` lookahead
            // assertions. (Capturing `(...)` groups are matched transparently
            // today; per-group capture extraction — needed for regexp_match
            // arrays / substring(from pattern) / `\N` in regexp_replace — is a
            // separate D.9 slice.)
            let mut lookahead: Option<bool> = None;
            if *p + 1 < chars.len() && chars[*p] == '?' {
                match chars[*p + 1] {
                    ':' => *p += 2,
                    '=' => {
                        lookahead = Some(false);
                        *p += 2;
                    }
                    '!' => {
                        lookahead = Some(true);
                        *p += 2;
                    }
                    _ => {}
                }
            }
            let inner = re_parse_alt(chars, p, depth + 1)?;
            if *p >= chars.len() || chars[*p] != ')' {
                return Err(EvalError::TypeMismatch {
                    detail: "regex compile: unmatched '('".into(),
                });
            }
            *p += 1;
            match lookahead {
                Some(negative) => Ok(ReNode::Lookahead {
                    negative,
                    inner: Box::new(inner),
                }),
                None => Ok(inner),
            }
        }
        '[' => re_parse_class(chars, p),
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
                    members: shortcut_members('s'),
                    negated: false,
                }),
                'S' => Ok(ReNode::Class {
                    members: shortcut_members('s'),
                    negated: true,
                }),
                // PG ARE word-boundary assertions (constraint escapes,
                // regc_lex.c). Only `\m \M \y \Y` are word boundaries.
                // Verified against live PG18: `\b`/`\B` are NOT boundaries
                // in ARE — `\b` is the backspace char and `\B` is a literal
                // backslash (character-entry escapes). See below.
                'y' => Ok(ReNode::WordBoundary(WordBoundaryKind::Boundary)),
                'Y' => Ok(ReNode::WordBoundary(WordBoundaryKind::NonBoundary)),
                'm' => Ok(ReNode::WordBoundary(WordBoundaryKind::BegWord)),
                'M' => Ok(ReNode::WordBoundary(WordBoundaryKind::EndWord)),
                // PG ARE string anchors (constraint escapes, regc_lex.c):
                // `\A` matches only at the start of the string, `\Z` only
                // at the end. This engine has no newline-sensitive mode,
                // so `\A` ≡ `^` (Start) and `\Z` ≡ `$` (End) exactly.
                // Verified against live PG18: `'foobar' ~ '\Afoo'` = t,
                // `'xfoo' ~ '\Afoo'` = f, `'foobar' ~ 'bar\Z'` = t.
                'A' => Ok(ReNode::Start),
                'Z' => Ok(ReNode::End),
                // Character-entry escapes matching PG ARE semantics (regc_lex.c).
                'a' => Ok(ReNode::Literal('\u{07}')), // alert (BEL)
                'e' => Ok(ReNode::Literal('\u{1b}')), // escape (ESC)
                'f' => Ok(ReNode::Literal('\u{0c}')), // form feed
                'n' => Ok(ReNode::Literal('\n')),
                'r' => Ok(ReNode::Literal('\r')),
                't' => Ok(ReNode::Literal('\t')),
                'v' => Ok(ReNode::Literal('\u{0b}')), // vertical tab
                'b' => Ok(ReNode::Literal('\u{08}')), // backspace
                'B' => Ok(ReNode::Literal('\\')),     // literal backslash
                // `\xHH` (1–2 hex digits) and `\uHHHH` (4 hex digits) numeric
                // character escapes.
                'x' | 'u' => {
                    let want = if esc == 'x' { 2 } else { 4 };
                    let mut hex = alloc::string::String::new();
                    while hex.len() < want
                        && *p < chars.len()
                        && chars[*p].is_ascii_hexdigit()
                    {
                        hex.push(chars[*p]);
                        *p += 1;
                    }
                    if hex.is_empty() {
                        return Err(EvalError::TypeMismatch {
                            detail: alloc::format!("regex compile: `\\{esc}` needs hex digits"),
                        });
                    }
                    let code = u32::from_str_radix(&hex, 16).map_err(|_| {
                        EvalError::TypeMismatch {
                            detail: "regex compile: bad numeric escape".into(),
                        }
                    })?;
                    Ok(ReNode::Literal(char::from_u32(code).unwrap_or('\u{fffd}')))
                }
                other => Ok(ReNode::Literal(other)),
            }
        }
        other => {
            *p += 1;
            Ok(ReNode::Literal(other))
        }
    }
}

/// v7.37.16 regex slice — parse a bracket expression `[...]` beginning
/// at `chars[*p] == '['`. Extends the original member/range parser with
/// the PG-ARE bracket features SQL apps rely on, all captured against
/// live PG18:
///
///   * POSIX classes `[[:alpha:]]`, `[[:digit:]]`, … (unknown name or
///     the `[:^name:]` negated form → "invalid character class", exactly
///     as PG rejects them);
///   * shortcut escapes inside the class — `[\d]`, `[\w]`, `[\s]` expand
///     inline; the complements `[\D]`, `[\W]`, `[\S]` become a
///     `NotInSet` member; char escapes `[\t]`, `[\n]`, `[\\]`, `[\]]`, …
///     fold to a literal;
///   * a `]` in the first member position is a literal `]` (`[]a]`,
///     `[^]a]`), matching POSIX/PG.
///
/// A leading `]` is the only member-position special case; `-` range and
/// trailing/leading-`-` handling is unchanged from the original parser.
fn re_parse_class(chars: &[char], p: &mut usize) -> Result<ReNode, EvalError> {
    debug_assert_eq!(chars.get(*p), Some(&'['));
    *p += 1;
    let mut negated = false;
    if *p < chars.len() && chars[*p] == '^' {
        negated = true;
        *p += 1;
    }
    let mut members: Vec<ClassMember> = Vec::new();
    let mut first = true;
    while *p < chars.len() {
        let c = chars[*p];
        // `]` closes the class — except in the first member position,
        // where POSIX/PG treat it as a literal `]`.
        if c == ']' && !first {
            *p += 1; // consume closing ]
            return Ok(ReNode::Class { members, negated });
        }
        first = false;

        // POSIX class `[:name:]` — requires a literal `[` (inside the
        // outer bracket) immediately followed by `:`.
        if c == '[' && chars.get(*p + 1) == Some(&':') {
            let mut q = *p + 2;
            let name_start = q;
            while q < chars.len() && chars[q] != ':' {
                q += 1;
            }
            // Must close with `:]`. A missing `:]`, or a `[:^name:]`
            // (which scans a name of "^name" → unknown), is rejected as
            // an invalid character class — the same error PG raises.
            if q + 1 >= chars.len() || chars[q + 1] != ']' {
                return Err(EvalError::TypeMismatch {
                    detail: "invalid regular expression: invalid character class".into(),
                });
            }
            let name: String = chars[name_start..q].iter().collect();
            members.extend(posix_class_members(&name)?);
            *p = q + 2; // consume through `:]`
            continue;
        }

        // Escape inside the class: positive shortcuts expand inline, the
        // complements become a NotInSet member, char escapes fold to a
        // literal.
        if c == '\\' && *p + 1 < chars.len() {
            let esc = chars[*p + 1];
            *p += 2;
            match esc {
                'd' | 'w' | 's' => members.extend(shortcut_members(esc)),
                'D' => members.push(ClassMember::NotInSet(shortcut_members('d'))),
                'W' => members.push(ClassMember::NotInSet(shortcut_members('w'))),
                'S' => members.push(ClassMember::NotInSet(shortcut_members('s'))),
                't' => members.push(ClassMember::Single('\t')),
                'n' => members.push(ClassMember::Single('\n')),
                'r' => members.push(ClassMember::Single('\r')),
                'f' => members.push(ClassMember::Single('\u{0c}')),
                'v' => members.push(ClassMember::Single('\u{0b}')),
                'b' => members.push(ClassMember::Single('\u{08}')), // backspace
                other => members.push(ClassMember::Single(other)),
            }
            continue;
        }

        // Ordinary char, possibly the start of a range `a-z`. A trailing
        // `-` (next char is the closing `]`) is a literal `-`.
        let start = c;
        *p += 1;
        if *p + 1 < chars.len() && chars[*p] == '-' && chars[*p + 1] != ']' {
            let end = chars[*p + 1];
            *p += 2;
            members.push(ClassMember::Range(start, end));
        } else {
            members.push(ClassMember::Single(start));
        }
    }
    // Fell off the end of the pattern without a closing `]`.
    Err(EvalError::TypeMismatch {
        detail: "regex compile: unmatched '['".into(),
    })
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

/// If the character at `*p` is a `?` (a lazy / non-greedy marker
/// immediately after a quantifier), consume it and return `true`;
/// otherwise leave `*p` unchanged and return `false`. The caller sets
/// `greedy = !consume_lazy_suffix(...)`.
fn consume_lazy_suffix(chars: &[char], p: &mut usize) -> bool {
    if *p < chars.len() && chars[*p] == '?' {
        *p += 1;
        true
    } else {
        false
    }
}

fn class_matches(member: &ClassMember, c: char) -> bool {
    match member {
        ClassMember::Single(s) => *s == c,
        ClassMember::Range(a, b) => c >= *a && c <= *b,
        ClassMember::NotInSet(subs) => !subs.iter().any(|m| class_matches(m, c)),
    }
}

/// v7.37.16 regex slice — the ASCII member list of a `\d`/`\w`/`\s`
/// shortcut, shared by the top-level escape parser (`re_parse_atom`) and
/// the in-bracket parser (`re_parse_class`) so both stay byte-identical.
/// `\v`/`\f` are included in the space set to match PG's `[[:space:]]`.
fn shortcut_members(kind: char) -> Vec<ClassMember> {
    match kind {
        'd' | 'D' => alloc::vec![ClassMember::Range('0', '9')],
        'w' | 'W' => alloc::vec![
            ClassMember::Range('a', 'z'),
            ClassMember::Range('A', 'Z'),
            ClassMember::Range('0', '9'),
            ClassMember::Single('_'),
        ],
        's' | 'S' => alloc::vec![
            ClassMember::Single(' '),
            ClassMember::Single('\t'),
            ClassMember::Single('\n'),
            ClassMember::Single('\r'),
            ClassMember::Single('\u{0b}'), // vertical tab
            ClassMember::Single('\u{0c}'), // form feed
        ],
        _ => Vec::new(),
    }
}

/// v7.37.16 regex slice — the ASCII member list of a POSIX class name
/// (`alpha`, `digit`, …) as it appears inside `[[:name:]]`. Returns
/// `Err` for an unknown name, matching PG18's "invalid character class"
/// compile error. Scoped to ASCII, consistent with this engine's
/// ASCII-only `\w`/`\d` handling (the matcher does not decode UTF-8).
fn posix_class_members(name: &str) -> Result<Vec<ClassMember>, EvalError> {
    let members = match name {
        "alpha" => alloc::vec![ClassMember::Range('a', 'z'), ClassMember::Range('A', 'Z')],
        "digit" => alloc::vec![ClassMember::Range('0', '9')],
        "alnum" => alloc::vec![
            ClassMember::Range('a', 'z'),
            ClassMember::Range('A', 'Z'),
            ClassMember::Range('0', '9'),
        ],
        "upper" => alloc::vec![ClassMember::Range('A', 'Z')],
        "lower" => alloc::vec![ClassMember::Range('a', 'z')],
        "xdigit" => alloc::vec![
            ClassMember::Range('0', '9'),
            ClassMember::Range('a', 'f'),
            ClassMember::Range('A', 'F'),
        ],
        "word" => alloc::vec![
            ClassMember::Range('a', 'z'),
            ClassMember::Range('A', 'Z'),
            ClassMember::Range('0', '9'),
            ClassMember::Single('_'),
        ],
        "space" => alloc::vec![
            ClassMember::Single(' '),
            ClassMember::Single('\t'),
            ClassMember::Single('\n'),
            ClassMember::Single('\r'),
            ClassMember::Single('\u{0b}'),
            ClassMember::Single('\u{0c}'),
        ],
        "blank" => alloc::vec![ClassMember::Single(' '), ClassMember::Single('\t')],
        "cntrl" => alloc::vec![
            ClassMember::Range('\u{00}', '\u{1f}'),
            ClassMember::Single('\u{7f}'),
        ],
        "print" => alloc::vec![ClassMember::Range('\u{20}', '\u{7e}')],
        "graph" => alloc::vec![ClassMember::Range('\u{21}', '\u{7e}')],
        "punct" => alloc::vec![
            ClassMember::Range('\u{21}', '\u{2f}'),
            ClassMember::Range('\u{3a}', '\u{40}'),
            ClassMember::Range('\u{5b}', '\u{60}'),
            ClassMember::Range('\u{7b}', '\u{7e}'),
        ],
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: "invalid regular expression: invalid character class".into(),
            });
        }
    };
    Ok(members)
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
    steps: &mut u64,
) -> Result<Option<usize>, EvalError> {
    // v7.37.16 Epic Rx P0 — abort before the recursive descent can
    // overflow the Rust call stack on an adversarial pattern.
    if depth > MATCH_DEPTH_LIMIT {
        return Err(EvalError::TypeMismatch {
            detail: "invalid regular expression: regular expression is too complex".into(),
        });
    }
    // v7.37.16 Epic Rx P0 — total-work (time) bound: this counter is
    // monotonic across every backtracking branch and start position, so
    // a catastrophic backtracker (shallow depth, exponential paths)
    // fails fast instead of hanging.
    *steps += 1;
    if *steps > MATCH_STEP_LIMIT {
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
        // v7.38 (read01 P6.15) — PG's ARE is non-newline-sensitive by default,
        // so `.` matches ANY character including `\n` (unlike Perl, where a
        // separate `s`/DOTALL flag is needed). SPG previously excluded `\n`,
        // diverging from PG on multi-line input.
        ReNode::AnyChar => Ok(if pos < s.len() {
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
        ReNode::WordBoundary(kind) => {
            // Zero-width: assert on the flanking chars, consume nothing.
            let before = pos > 0 && is_word_char(s[pos - 1]);
            let after = pos < s.len() && is_word_char(s[pos]);
            let ok = match kind {
                WordBoundaryKind::Boundary => before != after,
                WordBoundaryKind::NonBoundary => before == after,
                WordBoundaryKind::BegWord => !before && after,
                WordBoundaryKind::EndWord => before && !after,
            };
            Ok(if ok { Some(pos) } else { None })
        }
        // v7.37.17 (17.6 siblings) — Concat delegates to the
        // backtracking sequence matcher so quantifiers can shrink
        // when the tail fails ('bar.*que' now matches 'barbeque';
        // the old v7.17 stop-gap was greedy-without-backtracking).
        ReNode::Concat(items) => re_match_seq(items, s, pos, d, steps),
        ReNode::Alt(branches) => {
            for b in branches {
                if let Some(p) = re_match_at(b, s, pos, d, steps)? {
                    return Ok(Some(p));
                }
            }
            Ok(None)
        }
        ReNode::Quant {
            inner,
            min,
            max,
            greedy,
        } => {
            // Standalone quantifier (no tail). Greedy → the LONGEST
            // match (match as many reps as fit). Lazy → the FEWEST
            // (match exactly `min` reps, then stop). Tail interaction is
            // handled by re_match_seq; here there is nothing to satisfy
            // beyond the quantifier, so both directions collapse to a
            // single answer.
            let mut count = 0usize;
            let mut p = pos;
            loop {
                // Lazy: once the minimum is reached, take no more reps.
                if !*greedy && count >= *min {
                    break;
                }
                if let Some(cap) = max {
                    if count >= *cap {
                        break;
                    }
                }
                match re_match_at(inner, s, p, d, steps)? {
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
        ReNode::Lookahead { negative, inner } => {
            // Zero-width: try `inner` at the current position; succeed (consuming
            // nothing) per the positive/negative sense, else fail.
            let hit = re_match_at(inner, s, pos, d, steps)?.is_some();
            Ok(if hit != *negative { Some(pos) } else { None })
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
    steps: &mut u64,
) -> Result<Option<usize>, EvalError> {
    // v7.37.16 Epic Rx P0 — same stack-overflow guard as re_match_at.
    if depth > MATCH_DEPTH_LIMIT {
        return Err(EvalError::TypeMismatch {
            detail: "invalid regular expression: regular expression is too complex".into(),
        });
    }
    // v7.37.16 Epic Rx P0 — total-work (time) bound; see re_match_at.
    *steps += 1;
    if *steps > MATCH_STEP_LIMIT {
        return Err(EvalError::TypeMismatch {
            detail: "invalid regular expression: regular expression is too complex".into(),
        });
    }
    let d = depth + 1;
    let Some((first, rest)) = items.split_first() else {
        return Ok(Some(pos));
    };
    match first {
        ReNode::Quant {
            inner,
            min,
            max,
            greedy,
        } => {
            // Enumerate every reachable end position (0, 1, 2, ...
            // repetitions). The reachable set is identical for greedy
            // and lazy; only the ORDER in which we try the tail against
            // those ends differs — greedy tries longest-first (max reps,
            // give back), lazy tries shortest-first (min reps, take
            // more only when the tail fails). Both honor the same
            // `[min, max]` bound and the same step/depth guards.
            let mut ends = alloc::vec![pos];
            let mut p = pos;
            let mut count = 0usize;
            loop {
                if let Some(cap) = max {
                    if count >= *cap {
                        break;
                    }
                }
                match re_match_at(inner, s, p, d, steps)? {
                    Some(np) if np > p => {
                        p = np;
                        count += 1;
                        ends.push(p);
                    }
                    _ => break,
                }
            }
            // Try the tail at each reachable rep count. Greedy walks
            // high→low (longest first, give back); lazy walks low→high
            // (shortest first, take more). A single loop keeps this
            // recursive frame small — the P0 `MATCH_DEPTH_LIMIT` no-
            // overflow proof (`redos_deep_match_returns_err_not_overflow`)
            // is calibrated against this frame size.
            let n = ends.len(); // entries for reps = 0 ..= count
            for i in 0..n {
                let reps = if *greedy { n - 1 - i } else { i };
                if reps < *min {
                    // Greedy descends past min → done; lazy ascends past
                    // the below-min reps → skip and keep climbing.
                    if *greedy {
                        break;
                    }
                    continue;
                }
                if let Some(e) = re_match_seq(rest, s, ends[reps], d, steps)? {
                    return Ok(Some(e));
                }
            }
            Ok(None)
        }
        ReNode::Alt(branches) => {
            for b in branches {
                // Each branch may itself contain quantifiers —
                // match it standalone, then retry the tail.
                if let Some(p) = re_match_at(b, s, pos, d, steps)? {
                    if let Some(e) = re_match_seq(rest, s, p, d, steps)? {
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
            re_match_seq(&combined, s, pos, d, steps)
        }
        other => match re_match_at(other, s, pos, d, steps)? {
            Some(p) => re_match_seq(rest, s, p, d, steps),
            None => Ok(None),
        },
    }
}

/// Find the first match of `node` in `s`, starting at or after
/// `from`. Returns the (start, end) char positions of the match.
fn re_find(node: &ReNode, s: &[char], from: usize) -> Result<Option<(usize, usize)>, EvalError> {
    // v7.37.16 Epic Rx P0 — one monotonic step budget shared across
    // every start position of this find, so total backtracking WORK
    // (time), not just recursion depth, is bounded.
    let mut steps: u64 = 0;
    let mut start = from;
    loop {
        if let Some(end) = re_match_at(node, s, start, 0, &mut steps)? {
            return Ok(Some((start, end)));
        }
        if start >= s.len() {
            return Ok(None);
        }
        start += 1;
    }
}

/// v7.37.16 Epic Rx P2-⑧ — PG's `checkmatchall` (regexec.c). If the
/// ENTIRE compiled pattern is `^ <dot-repetition> $` — fully anchored,
/// with nothing but `.` and dot-quantifiers between the anchors — then a
/// whole-string match reduces to a length test and no backtracking is
/// needed: the string matches iff its CHARACTER length lies in
/// `[min, max]` (`max == None` = unbounded). Since SPG's `.` matches ANY
/// character including `\n` (PG's non-newline-sensitive default — see
/// `ReNode::AnyChar` in `re_match_at`), no separate newline test is needed.
/// Returns `Some((min, max))` for such a pattern, or `None` to leave
/// everything else on the backtracker.
///
/// Equivalence argument (why the caller's length test is byte-for-byte
/// identical to the backtracker):
///
///  * Fully anchored `^…$` forces the dot-run to span the WHOLE string,
///    so exactly `len` dots must match, one per character. Every `.`
///    matches any single character; the reachable total-length set of a
///    sequence of integer ranges `[aᵢ,bᵢ]` is the contiguous interval
///    `[Σaᵢ, Σbᵢ]` (Minkowski sum of consecutive-integer intervals), so
///    `len ∈ [Σmin, Σmax]` is exactly decomposability. Hence match ⇔
///    `Σmin ≤ len ≤ Σmax`.
///  * The "at most one variable-width quantifier" gate keeps the
///    backtracker's cost at O(len) (two or more free `.*`/`.{m,n}` in
///    sequence can enumerate combinatorially on a non-matching input and
///    trip the ReDoS step budget), so the caller's `MATCHALL_SAFE_LEN`
///    length gate can cheaply prove the backtracker would not have erred
///    at this input — the short-circuit therefore never converts a
///    backtracker step-budget error into a bool.
///
/// Only `regexp_like` (which backs `~`, `~*`, `SIMILAR TO`) asks the pure
/// yes/no whole-string question; the span-returning functions
/// (`regexp_match`/`replace`/`substr`/…) still use the backtracker
/// because they need the actual match span, not just its existence.
fn matchall_length_bounds(node: &ReNode) -> Option<(usize, Option<usize>)> {
    // Must be a fully-anchored concatenation: first `^`, last `$`.
    let ReNode::Concat(items) = node else {
        return None;
    };
    if items.len() < 2
        || !matches!(items.first(), Some(ReNode::Start))
        || !matches!(items.last(), Some(ReNode::End))
    {
        return None;
    }
    let mut min_sum: usize = 0;
    let mut max_sum: Option<usize> = Some(0);
    let mut flexible = 0u32;
    // Everything strictly between the anchors must be a dot-repetition
    // atom (a bare `.` or a quantifier whose inner node is `.`).
    for atom in &items[1..items.len() - 1] {
        let (amin, amax) = match atom {
            ReNode::AnyChar => (1usize, Some(1usize)),
            // Greedy vs lazy is irrelevant to a whole-string length
            // test — the reachable length window is the same — so the
            // `greedy` flag is ignored here.
            ReNode::Quant {
                inner, min, max, ..
            } if matches!(**inner, ReNode::AnyChar) => (*min, *max),
            _ => return None,
        };
        if amax != Some(amin) {
            flexible += 1;
        }
        min_sum = min_sum.saturating_add(amin);
        max_sum = match (max_sum, amax) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            _ => None,
        };
    }
    if flexible > 1 {
        return None;
    }
    Some((min_sum, max_sum))
}

/// v7.17.0 Phase 3.7 — `regexp_matches(s, pat)` returns the FIRST
/// match as a single-element TEXT[]. (PG returns one row per match
/// across all captures; SPG simplifies to first-match-only TEXT[].
/// The `g` flag form `regexp_matches(s, pat, 'g')` falls through
/// to all-matches concatenation as a flat array.)
/// PG's regexp functions take a flags string in a trailing argument whose
/// position differs per function. Returns true when that argument is present
/// and contains `i` (case-insensitive matching).
fn flags_have_i(args: &[Value<'_>], idx: usize) -> Result<bool, EvalError> {
    match args.get(idx) {
        Some(v) => Ok(text_arg(v)?.map_or(false, |f| f.contains('i'))),
        None => Ok(false),
    }
}

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
    let mut node = re_compile(&pat)?;
    if flags_have_i(args, 2)? {
        fold_case(&mut node);
    }
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
    let mut node = re_compile(&pat)?;
    if flags_have_i(args, 2)? {
        fold_case(&mut node);
    }
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
    let mut node = re_compile(&pat)?;
    // The `i` flag folds the compiled pattern to match either case, same as
    // the `~*` operator path.
    if flags.contains('i') {
        fold_case(&mut node);
    }
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
    let mut node = re_compile(&pat)?;
    if flags_have_i(args, 5)? {
        fold_case(&mut node);
    }
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
    let mut node = re_compile(&pat)?;
    if flags_have_i(args, 4)? {
        fold_case(&mut node);
    }
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
    // v7.37.16 Epic Rx P2-⑧ — checkmatchall. When the whole pattern is a
    // fully-anchored dot-repetition, a whole-string match is an O(1)
    // length test (plus an O(len) newline scan) with no backtracking. The
    // length gate keeps the answer byte-for-byte identical to the
    // backtracker, including its ReDoS step-budget behavior — see
    // `matchall_length_bounds`. `fold_case` (for `~*`) leaves `.` / dot-
    // quantifiers untouched, so detecting on the folded node is valid.
    if let Some((min, max)) = matchall_length_bounds(&node) {
        let len = chars.len();
        if (len as u64) <= MATCHALL_SAFE_LEN {
            // v7.38 (read01 P6.15) — `.` now matches `\n` (PG default), so a
            // fully-anchored dot-run matches the whole string regardless of
            // embedded newlines; the length test alone is exact.
            let matched = min <= len && max.map_or(true, |mx| len <= mx);
            return Ok(Value::Bool(matched));
        }
    }
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
        ReNode::Quant { inner, .. } | ReNode::Lookahead { inner, .. } => fold_case(inner),
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
    let mut node = re_compile(&pat)?;
    if flags_have_i(args, 3)? {
        fold_case(&mut node);
    }
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

    fn repeat_char_str(s: &str, n: usize) -> String {
        core::iter::repeat(s).take(n).collect()
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

    // (a — TIME bound) A catastrophic-backtracking pattern on a long
    // non-matching input recurses only shallowly (so the depth cap never
    // fires) but explores super-linearly many paths. The total-step
    // budget must abort it with a clean Err *fast* — if this test ever
    // hangs, the step counter is not wired into the hot backtracking
    // loop.
    //
    // NB: this matcher matches a *standalone* quantifier greedily without
    // backtracking, so the textbook nested-quantifier bomb `(a+)+$` is
    // already defused (the inner `a+` grabs the whole run at once). The
    // residual hazard is *sequential* quantifiers: `a*a*…a*b` on an all-
    // `a` string with no `b` forces the seq matcher to enumerate every
    // non-decreasing split of the run across the k stars — C(N+k, k)
    // combinations, ~7.5e10 here — before it can conclude "no match".
    // That is unbounded CPU without a work budget.
    #[test]
    fn redos_catastrophic_backtracking_returns_err_fast() {
        use std::time::Instant;
        // 10 sequential `a*` then a literal `b`; input is 50 `a`s and no
        // `b`, so every combination must be tried and all fail.
        let pat = alloc::format!("{}b", repeat_char_str("a*", 10));
        let node = super::re_compile(&pat).expect("compiles");
        let hay = chars(&repeat_char('a', 50));
        let t0 = Instant::now();
        let res = super::re_find(&node, &hay, 0);
        let elapsed = t0.elapsed();
        assert!(
            res.is_err(),
            "catastrophic backtracking must abort with a clean budget error, got {:?}",
            res
        );
        // The budget aborts at MATCH_STEP_LIMIT (10M) steps regardless of
        // how large the combinatorial space is, so this is well under a
        // second; a generous ceiling guards against a wiring regression
        // (an unbounded matcher would run for many minutes / never).
        assert!(
            elapsed.as_secs() < 10,
            "budget-exceeded abort must be fast, took {:?}",
            elapsed
        );
    }

    // (b) A long but non-pathological input matches correctly — a linear
    // pattern spends only O(input) steps, orders of magnitude under the
    // step budget, so the budget never trips and results are unchanged.
    #[test]
    fn redos_normal_long_input_still_matches() {
        // `^a+b$` on 10000 'a's + 'b' matches (greedy '+' then one 'b').
        let node = super::re_compile("^a+b$").expect("compiles");
        let hay = chars(&alloc::format!("{}b", repeat_char('a', 10_000)));
        assert_eq!(
            super::re_find(&node, &hay, 0).unwrap(),
            Some((0, 10_001)),
            "a legitimate long match must succeed — budget must not trip"
        );
        // Same pattern, no trailing 'b' → clean non-match (not an error).
        let miss = chars(&repeat_char('a', 10_000));
        assert_eq!(super::re_find(&node, &miss, 0).unwrap(), None);
    }
}

// ─── v7.37.16 Epic Rx P2-⑧ — checkmatchall (dot-repetition) tests ──────
#[cfg(test)]
mod matchall_tests {
    use alloc::string::String;
    use alloc::vec::Vec;

    use spg_storage::Value;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    fn repeat_char(c: char, n: usize) -> String {
        core::iter::repeat(c).take(n).collect()
    }

    // Detector recognizes the fully-anchored dot-repetition shapes and
    // returns their exact [min, max] length window.
    #[test]
    fn matchall_detects_dot_repetition_shapes() {
        fn bounds(pat: &str) -> Option<(usize, Option<usize>)> {
            super::matchall_length_bounds(&super::re_compile(pat).unwrap())
        }
        assert_eq!(bounds("^.*$"), Some((0, None)));
        assert_eq!(bounds("^.+$"), Some((1, None)));
        assert_eq!(bounds("^.$"), Some((1, Some(1))));
        assert_eq!(bounds("^.{2,4}$"), Some((2, Some(4))));
        assert_eq!(bounds("^.{3}$"), Some((3, Some(3))));
        assert_eq!(bounds("^.{2,}$"), Some((2, None)));
        // Multi-atom: two fixed dots + a star → [1+.., ..] with a single
        // flexible quantifier is still recognized.
        assert_eq!(bounds("^..*$"), Some((1, None)));
        // Fixed multi-atom `..` = exactly length 2.
        assert_eq!(bounds("^..$"), Some((2, Some(2))));
        // Empty anchored pattern matches only the empty string.
        assert_eq!(bounds("^$"), Some((0, Some(0))));
    }

    // Patterns that must NOT take the fast path (stay on the backtracker).
    #[test]
    fn matchall_rejects_non_dot_or_unanchored() {
        fn bounds(pat: &str) -> Option<(usize, Option<usize>)> {
            super::matchall_length_bounds(&super::re_compile(pat).unwrap())
        }
        // A literal in the pattern → not pure dot-repetition.
        assert_eq!(bounds("^a.*b$"), None);
        // Not fully anchored (either end missing) → not a whole-string
        // length question.
        assert_eq!(bounds(".*"), None);
        assert_eq!(bounds("^.*"), None);
        assert_eq!(bounds(".*$"), None);
        // Two variable-width quantifiers in sequence — excluded so the
        // backtracker's step cost stays O(len) and the length gate can
        // prove equivalence.
        assert_eq!(bounds("^.*.*$"), None);
        assert_eq!(bounds("^.{2,4}.{1,3}$"), None);
        // A character class is not `.` (it may exclude chars a `.` allows).
        assert_eq!(bounds("^[abc]*$"), None);
    }

    // `.*` matches any length including the empty string.
    #[test]
    fn matchall_star_matches_any_length() {
        let node = super::re_compile("^.*$").unwrap();
        let (min, max) = super::matchall_length_bounds(&node).unwrap();
        for s in ["", "a", "abc", "hello world"] {
            let cs = chars(s);
            let len = cs.len();
            let fast = min <= len && max.map_or(true, |mx| len <= mx) && !cs.contains(&'\n');
            assert!(fast, "`.*` must match {s:?}");
        }
    }

    // `.{2,4}` matches lengths 2/3/4 but not 1 or 5.
    #[test]
    fn matchall_bounded_matches_window_only() {
        let node = super::re_compile("^.{2,4}$").unwrap();
        let (min, max) = super::matchall_length_bounds(&node).unwrap();
        let verdict = |s: &str| {
            let cs = chars(s);
            let len = cs.len();
            min <= len && max.map_or(true, |mx| len <= mx) && !cs.contains(&'\n')
        };
        assert!(!verdict("a")); // len 1
        assert!(verdict("ab")); // len 2
        assert!(verdict("abc")); // len 3
        assert!(verdict("abcd")); // len 4
        assert!(!verdict("abcde")); // len 5
    }

    // A NON-dot pattern like `a.*b` is not taken by the fast path — and
    // going through the real backtracker still gives the correct answer.
    #[test]
    fn matchall_non_dot_pattern_uses_backtracker_correctly() {
        let node = super::re_compile("^a.*b$").unwrap();
        assert_eq!(super::matchall_length_bounds(&node), None);
        assert!(super::re_find(&node, &chars("axxxb"), 0).unwrap().is_some());
        assert!(super::re_find(&node, &chars("axxxc"), 0).unwrap().is_none());
    }

    // Differential: for every fast-pathed pattern the length short-circuit
    // must agree with a forced-backtracking match on a spread of inputs —
    // INCLUDING newline-containing inputs (SPG's `.` matches `\n` per PG's
    // default, so the pure length check is exact).
    #[test]
    fn matchall_fast_path_agrees_with_backtracker() {
        let pats = [
            "^.*$", "^.+$", "^.$", "^.{2,4}$", "^.{3}$", "^.{2,}$", "^..*$", "^..$", "^$",
        ];
        let inputs = [
            "", "a", "ab", "abc", "abcd", "abcde", "a\nb", "\n", "ab\n", "\n\n", "hello",
        ];
        for pat in pats {
            let node = super::re_compile(pat).unwrap();
            let (min, max) =
                super::matchall_length_bounds(&node).expect("pattern must be fast-pathed");
            for s in inputs {
                let cs = chars(s);
                let len = cs.len();
                // v7.38 (read01 P6.15) — `.` matches `\n` (PG default), so the
                // fast path is a pure length test with no newline guard.
                let fast = min <= len && max.map_or(true, |mx| len <= mx);
                let slow = super::re_find(&node, &cs, 0).unwrap().is_some();
                assert_eq!(
                    fast, slow,
                    "fast/slow disagree for pat {pat:?} input {s:?}: fast={fast} slow={slow}"
                );
            }
        }
    }

    // End-to-end through `regexp_like` (the `~` / `SIMILAR TO` entry point):
    // the fast path must yield the same bool the backtracker would, incl.
    // the newline case.
    #[test]
    fn matchall_regexp_like_end_to_end() {
        let like = |text: &str, pat: &str| -> bool {
            match super::regexp_like(&[Value::text(text), Value::text(pat)]).unwrap() {
                Value::Bool(b) => b,
                other => panic!("regexp_like returned {other:?}"),
            }
        };
        // v7.38 (read01 P6.15) — `^.*$` matches ANY string, newlines
        // included, because PG's `.` is non-newline-sensitive by default.
        assert!(like("", "^.*$"));
        assert!(like("anything at all", "^.*$"));
        assert!(like("two\nlines", "^.*$"));
        // `^.{2,4}$` window.
        assert!(!like("a", "^.{2,4}$"));
        assert!(like("abc", "^.{2,4}$"));
        assert!(!like("abcde", "^.{2,4}$"));
        // Non-fast-pathed pattern still works.
        assert!(like("axxxb", "^a.*b$"));
        assert!(!like("axxxc", "^a.*b$"));
    }

    // Regex P1 — PG ARE word-boundary assertions (\y \m \M \Y) and the
    // character-entry escapes \b (backspace) / \B (backslash). Every
    // expected value below was captured from live PostgreSQL 18
    // (docker spg-bench-postgres) so this is a true differential fixture:
    // SPG's result must equal PG18's for each case.
    #[test]
    fn word_boundary_assertions_match_pg18() {
        let like = |text: &str, pat: &str| -> bool {
            match super::regexp_like(&[Value::text(text), Value::text(pat)]).unwrap() {
                Value::Bool(b) => b,
                other => panic!("regexp_like returned {other:?}"),
            }
        };
        // (text, pattern, PG18 result)
        let cases: &[(&str, &str, bool)] = &[
            // \y — beginning OR end of word.
            ("foobar", r"\yfoo\y", false),   // foo not at a word edge on the right
            ("foo bar", r"\yfoo\y", true),   // foo is a whole word
            ("a.b", r"\ya\y", true),         // '.' is a non-word char → boundaries
            ("hello world", r"\yworld\y", true),
            ("foobar", r"\yfoo", true),      // \y at string start before a word
            ("foobar", r"bar\y", true),      // \y at string end after a word
            ("foobar", r"foo\ybar", false),  // o|b both word → no boundary
            ("foo_bar", r"foo\ybar", false), // '_' is a word char → no boundary
            // \m — beginning of a word only.
            ("foo bar", r"\mbar", true),
            ("foobar", r"\mfoo", true),
            // \M — end of a word only.
            ("foo bar", r"foo\M", true),
            ("foobar", r"foo\M", false),
            // \Y — NOT a word boundary.
            ("foobar", r"oo\Yba", true),     // o|b both word → non-boundary
            ("foo bar", r"foo\Y bar", false), // o|space is a boundary → \Y fails
            // \b = backspace char (NOT a word boundary in PG ARE).
            ("foo bar", "foo\\bbar", false), // needs a literal backspace → no match
            ("a\u{08}c", "a\\bc", true),     // backspace present → matches
            // \B = literal backslash (NOT a word boundary in PG ARE).
            ("foobar", r"foo\Bbar", false),  // needs a literal '\' → no match
            ("a\\c", r"a\Bc", true),         // literal backslash present → matches
        ];
        for &(text, pat, expected) in cases {
            assert_eq!(
                like(text, pat),
                expected,
                "SPG disagrees with PG18 for {text:?} ~ {pat:?} (PG18={expected})"
            );
        }
    }
}

// ─── PG18 differential corpus (v7.37.16 regex slice) ──────────────────
//
// Every SAFE-ADDITIVE expectation below was captured from live
// PostgreSQL 18.4 (docker spg-bench-postgres, `~` / `regexp_match`).
// A `check`ed row asserts SPG == PG18: SPG must match PG's boolean /
// span. The KNOWN-DEFERRED block at the bottom pins SPG's *current*
// (divergent) behaviour for the SEMANTIC / ARCHITECTURAL gaps that this
// slice deliberately does NOT close — so an accidental change to them is
// caught, without falsely claiming parity.
#[cfg(test)]
mod pg18_differential_tests {
    extern crate std;

    use alloc::string::{String, ToString};
    use alloc::vec::Vec;

    use spg_storage::Value;

    /// `text ~ pat` (optionally case-insensitive) through the real entry
    /// point `regexp_like` — the function backing the `~` / `~*` ops.
    fn like(text: &str, pat: &str, ci: bool) -> bool {
        let mut args = alloc::vec![Value::text(text), Value::text(pat)];
        if ci {
            args.push(Value::text("i"));
        }
        match super::regexp_like(&args).unwrap() {
            Value::Bool(b) => b,
            other => panic!("regexp_like returned {other:?}"),
        }
    }

    /// First-match span through `regexp_match` (the span-returning path
    /// where leftmost/longest differences are visible).
    fn span(text: &str, pat: &str) -> Option<String> {
        match super::regexp_match(&[Value::text(text), Value::text(pat)]).unwrap() {
            Value::Null => None,
            Value::TextArray(v) => v.into_iter().next().flatten(),
            other => panic!("regexp_match returned {other:?}"),
        }
    }

    #[test]
    fn pg18_differential_corpus() {
        let mut fails: Vec<String> = Vec::new();

        // (text, pat, PG18-bool) — SAFE-ADDITIVE boolean cases.
        let bool_cases: &[(&str, &str, bool)] = &[
            // ── POSIX character classes ─────────────────────────────
            ("ab12", "^[[:alpha:]]+", true),
            ("__", "[[:alnum:]]", false),
            ("a", "[[:alnum:]]", true),
            (" ", "[[:space:]]", true),
            ("A", "[[:upper:]]", true),
            ("A", "[[:lower:]]", false),
            ("a", "[[:lower:]]", true),
            ("!", "[[:punct:]]", true),
            ("@", "[[:punct:]]", true),
            ("_", "[[:punct:]]", true),
            (" ", "[[:punct:]]", false),
            ("f", "[[:xdigit:]]", true),
            ("g", "[[:xdigit:]]", false),
            ("_", "[[:word:]]", true),
            (" ", "[[:word:]]", false),
            ("\u{0b}", "[[:space:]]", true), // vertical tab
            ("\u{0c}", "[[:space:]]", true), // form feed
            (" ", "[[:blank:]]", true),
            ("\t", "[[:blank:]]", true),
            ("\n", "[[:blank:]]", false),
            ("\u{01}", "[[:cntrl:]]", true),
            ("a", "[[:cntrl:]]", false),
            (" ", "[[:print:]]", true),
            (" ", "[[:graph:]]", false),
            ("a", "[[:graph:]]", true),
            ("a", "[^[:digit:]]", true), // negated bracket around POSIX
            ("5", "[^[:digit:]]", false),
            // ── \A / \Z string anchors ──────────────────────────────
            ("foobar", r"\Afoo", true),
            ("xfoo", r"\Afoo", false),
            ("foobar", r"bar\Z", true),
            ("barx", r"bar\Z", false),
            ("aAb", r"a\Ab", false), // \A only at string start
            // ── \s now includes vertical-tab / form-feed (PG parity) ─
            ("\u{0b}", r"\s", true),
            ("\u{0c}", r"\s", true),
            ("\u{0b}", r"\S", false),
            // ── escapes / shortcuts inside bracket expressions ──────
            ("5", r"[\d]", true),
            ("a", r"[\d]", false),
            ("_", r"[\w]", true),
            ("a", r"[\D]", true),
            ("5", r"[\D]", false),
            ("A", r"[\dA]", true),
            ("\t", r"[\t]", true),
            ("\t", r"[\s]", true),
            ("x", r"[\S]", true),
            ("\t", r"[\S]", false),
            ("5", r"[\w.]", true),
            // ── bracket edge cases (']' / '[' / '.' literals) ───────
            ("]", "[]a]", true),
            ("a", "[]a]", true),
            ("b", "[^]a]", true),
            ("]", "[^]a]", false),
            (".", "[.]", true),
            ("a", "[.]", false),
            ("[", "[[]", true),
            ("a", "[a-]", true),
            ("-", "[a-]", true),
            ("-", "[-a]", true),
            // ── controls (must already pass — regression guard) ─────
            ("m", "[a-z]", true),
            ("5", "[^a-z]", true),
            ("color", "colou?r", true),
            ("abc", "^abc$", true),
            ("cat", "cat|dog", true),
            ("a.b", r"a\.b", true),
            ("axb", r"a\.b", false),
            ("7", r"\d", true),
            ("a", r"\D", true),
            ("_", r"\w", true),
        ];
        for &(text, pat, want) in bool_cases {
            let got = like(text, pat, false);
            if got != want {
                fails.push(alloc::format!(
                    "BOOL {text:?} ~ {pat:?}: PG18={want} SPG={got}"
                ));
            }
        }

        // Case-insensitive control (`~*`).
        if !like("HELLO", "hello", true) {
            fails.push("BOOL(ci) 'HELLO' ~* 'hello': PG18=true SPG=false".to_string());
        }

        // (text, pat, PG18-span) — SAFE-ADDITIVE + control span cases.
        let span_cases: &[(&str, &str, Option<&str>)] = &[
            ("ab12cd", "[[:digit:]]+", Some("12")),
            ("a1!", "[[:alpha:][:digit:]]+", Some("a1")),
            ("a5", r"[\d]+", Some("5")),
            // controls (greedy)
            ("aXbXb", "a.*b", Some("aXbXb")),
            ("aaab", "a+", Some("aaa")),
            ("aaaa", "a{2,3}", Some("aaa")),
            // ── lazy (non-greedy) quantifiers — PG18-captured spans ──
            // Every span below was read from live PG18 (regexp_match /
            // substring); the greedy control immediately above proves
            // greedy semantics are unchanged by the lazy addition.
            ("axbxb", "a.*?b", Some("axb")), // lazy `.*?` stops at first b
            ("aaa", "a+?", Some("a")),       // lazy `+?` takes the minimum 1
            ("<a><b>", "<.*?>", Some("<a>")),
            ("abcabc", "a.*?c", Some("abc")), // substring(... from ...) span
            ("a", "a??", Some("")),           // lazy optional prefers 0 → empty
            ("xaa", "xa??", Some("x")),       // 0 reps of the lazy optional
            ("abc", ".*?", Some("")),         // lazy star prefers empty match
            ("abc", ".+?", Some("a")),        // lazy plus takes one char
            ("aaaa", "a{2,5}?", Some("aa")),  // lazy bound takes the minimum 2
            ("aaaab", "a{2,5}?b", Some("aaaab")), // takes more until tail fits
            ("abab", "(ab)+?", Some("ab")),   // lazy group takes one rep
        ];
        for &(text, pat, want) in span_cases {
            let got = span(text, pat);
            let want_owned = want.map(|s| s.to_string());
            if got != want_owned {
                fails.push(alloc::format!(
                    "SPAN {text:?} ~ {pat:?}: PG18={want:?} SPG={got:?}"
                ));
            }
        }

        // Invalid POSIX-class forms are a compile ERROR in PG18 — SPG
        // must likewise reject them, not silently mis-parse.
        for pat in ["[[:notaclass:]]", "[[:^upper:]]"] {
            if super::re_compile(pat).is_ok() {
                fails.push(alloc::format!(
                    "COMPILE {pat:?}: PG18=error SPG=compiled-ok"
                ));
            }
        }

        assert!(
            fails.is_empty(),
            "SPG diverges from PG18 on {} case(s):\n  {}",
            fails.len(),
            fails.join("\n  ")
        );
    }

    // KNOWN-DEFERRED gaps — NOT closed by this slice. Each row pins
    // SPG's *current* behaviour and records PG18's (divergent) answer in
    // a comment. These are SEMANTIC (leftmost-vs-longest / greedy-only)
    // or ARCHITECTURAL (backreferences) and need a focused slice each.
    #[test]
    fn pg18_known_deferred_divergences() {
        // POSIX-longest alternation — the biggest known correctness gap.
        // PG18: regexp_match('ab','a|ab') = {ab}  (longest overall)
        // SPG : leftmost-first branch wins → {a}
        assert_eq!(span("ab", "a|ab"), Some("a".to_string()));

        // (Lazy / non-greedy quantifiers `*? +? ?? {m,n}?` are now
        //  implemented — see the lazy span cases in
        //  `pg18_differential_corpus`. No longer deferred.)

        // Backreferences — architectural (ReNode has no capture state).
        // PG18: 'abab' ~ '(ab)\1' = true
        // SPG : \1 is a literal '1' → false
        assert!(!like("abab", r"(ab)\1", false));
    }
}
