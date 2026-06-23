//! Full-text-search SQL functions and `tsvector` / `tsquery` codecs.
//! Wraps the lexer/stemmer engine in `crate::fts`: the `to_tsvector` /
//! `*_tsquery` / `ts_rank` / `setweight` / `@@` builtins plus the PG
//! external-form render (`format_*`) and parse (`decode_*_external`)
//! used by the wire layer and `::tsvector` / `::tsquery` casts.
//! Split out of `eval.rs` (cut 26).

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_storage::{TsLexeme, TsQueryAst, Value};

use super::{EvalContext, EvalError};

/// v7.12.2 — `ts_rank([weights,] vec, query [, norm])`. v7.12.2
/// supports the canonical `(vec, query)` two-arg form mailrs uses;
/// optional weight-array / normalisation arguments error with an
/// "unsupported" message rather than silently changing semantics.
pub(super) fn fts_ts_rank(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let (vec, query) = parse_rank_args("ts_rank", args)?;
    match (vec, query) {
        (None, _) | (_, None) => Ok(Value::Null),
        (Some(v), Some(q)) => Ok(Value::Float(f64::from(crate::fts::ts_rank(&v, &q)))),
    }
}

pub(super) fn fts_ts_rank_cd(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let (vec, query) = parse_rank_args("ts_rank_cd", args)?;
    match (vec, query) {
        (None, _) | (_, None) => Ok(Value::Null),
        (Some(v), Some(q)) => Ok(Value::Float(f64::from(crate::fts::ts_rank_cd(&v, &q)))),
    }
}

fn parse_rank_args(
    name: &str,
    args: &[Value<'_>],
) -> Result<
    (
        Option<Vec<spg_storage::TsLexeme>>,
        Option<spg_storage::TsQueryAst>,
    ),
    EvalError,
> {
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "{name}() takes 2 args in v7.12.2 (weights array + normalisation flag are v7.12.x carve-out), got {}",
                args.len()
            ),
        });
    }
    let vec = match &args[0] {
        Value::Null => None,
        Value::TsVector(v) => Some(v.clone()),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "{name}() first arg must be tsvector, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let query = match &args[1] {
        Value::Null => None,
        Value::TsQuery(q) => Some(q.clone()),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "{name}() second arg must be tsquery, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    Ok((vec, query))
}

/// v7.12.2 — `tsvector @@ tsquery` match operator. Either
/// ordering accepted (PG semantics). NULL on either side → NULL.
/// Anything that isn't tsvector/tsquery on either side is a type
/// mismatch. Returns BOOL.
pub(super) fn ts_match(l: Value, r: Value) -> Result<Value<'static>, EvalError> {
    let (vec, query) = match (l, r) {
        (Value::Null, _) | (_, Value::Null) => return Ok(Value::Null),
        (Value::TsVector(v), Value::TsQuery(q)) => (v, q),
        (Value::TsQuery(q), Value::TsVector(v)) => (v, q),
        (l, r) => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "@@ requires (tsvector, tsquery), got ({:?}, {:?})",
                    l.data_type(),
                    r.data_type()
                ),
            });
        }
    };
    Ok(Value::Bool(crate::fts::ts_query_matches(&vec, &query)))
}

/// v7.12.1 — `to_tsvector([config,] text)`. With one arg the
/// session-resolved `default_text_search_config` is used (defaults
/// to `simple` when unset); with two args the first picks the
/// config. NULL text → NULL.
pub(super) fn fts_to_tsvector(
    args: &[Value<'_>],
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    let (config, text) = parse_fts_args("to_tsvector", args, ctx)?;
    match text {
        None => Ok(Value::Null),
        Some(t) => Ok(Value::TsVector(crate::fts::to_tsvector(config, &t))),
    }
}

/// v7.24 (round-16 C) — `setweight(tsvector, "char")`. Relabels
/// every lexeme with the given PG weight letter (A=3 B=2 C=1 D=0).
pub(super) fn fts_setweight(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let [vec_arg, weight_arg] = args else {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("setweight expects 2 arguments, got {}", args.len()),
        });
    };
    if matches!(vec_arg, Value::Null) || matches!(weight_arg, Value::Null) {
        return Ok(Value::Null);
    }
    let Value::TsVector(lexemes) = vec_arg else {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "setweight expects a tsvector, got {:?}",
                vec_arg.data_type()
            ),
        });
    };
    let Value::Text(w) = weight_arg else {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "setweight expects a weight letter, got {:?}",
                weight_arg.data_type()
            ),
        });
    };
    let weight = match w.to_ascii_uppercase().as_str() {
        "A" => 3,
        "B" => 2,
        "C" => 1,
        "D" => 0,
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!("unrecognized weight: {other:?} (expected A, B, C or D)"),
            });
        }
    };
    let mut out = lexemes.clone();
    for lex in &mut out {
        lex.weight = weight;
    }
    Ok(Value::TsVector(out))
}

pub(super) fn fts_plainto_tsquery(
    args: &[Value<'_>],
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    let (config, text) = parse_fts_args("plainto_tsquery", args, ctx)?;
    match text {
        None => Ok(Value::Null),
        Some(t) => Ok(Value::TsQuery(crate::fts::plainto_tsquery(config, &t))),
    }
}

pub(super) fn fts_phraseto_tsquery(
    args: &[Value<'_>],
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    let (config, text) = parse_fts_args("phraseto_tsquery", args, ctx)?;
    match text {
        None => Ok(Value::Null),
        Some(t) => Ok(Value::TsQuery(crate::fts::phraseto_tsquery(config, &t))),
    }
}

pub(super) fn fts_websearch_to_tsquery(
    args: &[Value<'_>],
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    let (config, text) = parse_fts_args("websearch_to_tsquery", args, ctx)?;
    match text {
        None => Ok(Value::Null),
        Some(t) => Ok(Value::TsQuery(crate::fts::websearch_to_tsquery(config, &t))),
    }
}

pub(super) fn fts_to_tsquery(
    args: &[Value<'_>],
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    let (config, text) = parse_fts_args("to_tsquery", args, ctx)?;
    match text {
        None => Ok(Value::Null),
        Some(t) => Ok(Value::TsQuery(crate::fts::to_tsquery(config, &t)?)),
    }
}

/// Parse the `(config, text)` / `(text)` argument pair shared by
/// all FTS builders. Returns the resolved config + the text
/// payload (None when text is NULL). The one-arg form pulls the
/// config from the session's `default_text_search_config`.
fn parse_fts_args(
    name: &str,
    args: &[Value<'_>],
    ctx: &EvalContext<'_>,
) -> Result<(crate::fts::TsConfig, Option<String>), EvalError> {
    let (config_arg, text_arg) = match args {
        [t] => (None, t),
        [c, t] => (Some(c), t),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: format!("{name}() takes 1 or 2 args, got {}", args.len()),
            });
        }
    };
    let config = match config_arg {
        None => match ctx.default_text_search_config {
            Some(name_str) => crate::fts::TsConfig::from_name(name_str).ok_or_else(|| {
                EvalError::TypeMismatch {
                    detail: format!(
                        "text search config not implemented: {name_str:?} (supported: simple, english)"
                    ),
                }
            })?,
            None => crate::fts::TsConfig::Simple,
        },
        Some(Value::Null) => return Ok((crate::fts::TsConfig::Simple, None)),
        Some(Value::Text(name_str)) => crate::fts::TsConfig::from_name(name_str).ok_or_else(|| {
            EvalError::TypeMismatch {
                detail: format!(
                    "text search config not implemented: {name_str:?} (supported: simple, english)"
                ),
            }
        })?,
        Some(other) => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "{name}() config arg must be text, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let text = match text_arg {
        Value::Null => None,
        Value::Text(s) => Some(s.to_string()),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "{name}() text arg must be text, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    Ok((config, text))
}

/// v7.12.0 — render a `tsvector` in PG's external form:
/// `'lex':1,2A 'word':3` (single-quoted lexemes, optional
/// `:positions`, optional weight letter `A/B/C/D` per position).
/// Lexemes already arrive sorted + deduped from the engine. Used
/// by the wire layer (OID 3614) and by SELECT-text output.
pub fn format_tsvector(lexs: &[TsLexeme]) -> String {
    let mut out = String::with_capacity(lexs.len() * 12);
    for (i, l) in lexs.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push('\'');
        for c in l.word.chars() {
            if c == '\'' {
                out.push('\'');
            }
            out.push(c);
        }
        out.push('\'');
        if !l.positions.is_empty() {
            for (pi, p) in l.positions.iter().enumerate() {
                out.push(if pi == 0 { ':' } else { ',' });
                out.push_str(&p.to_string());
            }
            // v7.12.0 — weight is per-lexeme (the v7.12 design
            // collapses PG's per-position weight into one letter).
            // Emit once after the last position; default `D`
            // (weight=0) stays implicit.
            match l.weight {
                3 => out.push('A'),
                2 => out.push('B'),
                1 => out.push('C'),
                _ => {}
            }
        }
    }
    out
}

/// v7.12.0 — render a `tsquery` in PG's external form. Operator
/// precedence: `!` > `&` > `|`. Phrase distance shown as `<N>`.
pub fn format_tsquery(ast: &TsQueryAst) -> String {
    fn go(ast: &TsQueryAst, parent_prec: u8, out: &mut String) {
        // 0 = top, 1 = OR, 2 = AND, 3 = NOT/Phrase, 4 = atom.
        let (own_prec, write_self): (u8, &dyn Fn(&mut String)) = match ast {
            TsQueryAst::Or(_, _) => (1, &|_| {}),
            TsQueryAst::And(_, _) | TsQueryAst::Phrase { .. } => (2, &|_| {}),
            TsQueryAst::Not(_) => (3, &|_| {}),
            TsQueryAst::Term { .. } => (4, &|_| {}),
        };
        let need_parens = own_prec < parent_prec;
        if need_parens {
            out.push('(');
        }
        match ast {
            TsQueryAst::Term { word, .. } => {
                out.push('\'');
                for c in word.chars() {
                    if c == '\'' {
                        out.push('\'');
                    }
                    out.push(c);
                }
                out.push('\'');
            }
            TsQueryAst::And(a, b) => {
                go(a, own_prec, out);
                out.push_str(" & ");
                go(b, own_prec, out);
            }
            TsQueryAst::Or(a, b) => {
                go(a, own_prec, out);
                out.push_str(" | ");
                go(b, own_prec, out);
            }
            TsQueryAst::Not(x) => {
                out.push('!');
                go(x, own_prec, out);
            }
            TsQueryAst::Phrase {
                left,
                right,
                distance,
            } => {
                go(left, own_prec, out);
                out.push_str(&alloc::format!(" <{distance}> "));
                go(right, own_prec, out);
            }
        }
        write_self(out);
        if need_parens {
            out.push(')');
        }
    }
    let mut out = String::new();
    go(ast, 0, &mut out);
    out
}

/// v7.12.0 — decode PG external form `'word':1,2A 'other':3` into
/// a `Vec<TsLexeme>`. Lexemes are sorted ascending by `word` (with
/// duplicates merged on positions) so the output matches the
/// engine invariant. Empty input yields an empty vector.
///
/// v7.12.0 only ships the cast-literal entry. Full `to_tsvector`
/// (Unicode word-split + Porter stemming + stopwords) lands in
/// v7.12.1.
pub fn decode_tsvector_external(s: &str) -> Result<Vec<TsLexeme>, EvalError> {
    let mut out: Vec<TsLexeme> = Vec::new();
    let mut i = 0;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        // Quoted form `'word'` (with embedded `''` for a literal
        // single quote, mirroring PG).
        let word = if bytes[i] == b'\'' {
            i += 1;
            let mut w = String::new();
            loop {
                if i >= bytes.len() {
                    return Err(EvalError::TypeMismatch {
                        detail: "tsvector literal: unterminated quoted lexeme".into(),
                    });
                }
                let b = bytes[i];
                if b == b'\'' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        w.push('\'');
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    w.push(b as char);
                    i += 1;
                }
            }
            w
        } else {
            // Bare form — read until whitespace, ':' or end.
            let start = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b':' {
                i += 1;
            }
            core::str::from_utf8(&bytes[start..i])
                .map_err(|_| EvalError::TypeMismatch {
                    detail: "tsvector literal: non-UTF-8 lexeme".into(),
                })?
                .to_string()
        };
        if word.is_empty() {
            return Err(EvalError::TypeMismatch {
                detail: "tsvector literal: empty lexeme".into(),
            });
        }
        // Optional `:pos[,pos][,pos]`. Each position is u16; each
        // may carry a trailing weight letter A/B/C/D.
        let mut positions: Vec<u16> = Vec::new();
        let mut weight: u8 = 0;
        if i < bytes.len() && bytes[i] == b':' {
            i += 1;
            loop {
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if start == i {
                    return Err(EvalError::TypeMismatch {
                        detail: "tsvector literal: expected digit after ':'".into(),
                    });
                }
                let num: u16 = core::str::from_utf8(&bytes[start..i])
                    .expect("ascii digits")
                    .parse()
                    .map_err(|_| EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "tsvector literal: position {} overflows u16",
                            core::str::from_utf8(&bytes[start..i]).unwrap_or("?")
                        ),
                    })?;
                positions.push(num);
                if i < bytes.len() {
                    let w = bytes[i];
                    if matches!(w, b'A' | b'B' | b'C' | b'D') {
                        weight = match w {
                            b'A' => 3,
                            b'B' => 2,
                            b'C' => 1,
                            _ => 0,
                        };
                        i += 1;
                    }
                }
                if i < bytes.len() && bytes[i] == b',' {
                    i += 1;
                    continue;
                }
                break;
            }
        }
        positions.sort_unstable();
        positions.dedup();
        // Merge into the output vector — sorted insert by word,
        // duplicate words merge positions.
        match out.binary_search_by(|l| l.word.as_str().cmp(word.as_str())) {
            Ok(idx) => {
                for p in positions {
                    if !out[idx].positions.contains(&p) {
                        out[idx].positions.push(p);
                    }
                }
                out[idx].positions.sort_unstable();
                if weight != 0 {
                    out[idx].weight = weight;
                }
            }
            Err(idx) => {
                out.insert(
                    idx,
                    TsLexeme {
                        word,
                        positions,
                        weight,
                    },
                );
            }
        }
    }
    Ok(out)
}

/// v7.12.0 — decode PG external form `'foo' & 'bar' | !'baz'`
/// into a `TsQueryAst`. v7.12.0 supports the canonical
/// `to_tsquery` surface: single-quoted lexemes, `&` / `|` / `!`,
/// parens, and phrase `<N>`. Bare lexemes are accepted too. Full
/// `plainto_tsquery` / `websearch_to_tsquery` arrive in v7.12.1.
pub fn decode_tsquery_external(s: &str) -> Result<TsQueryAst, EvalError> {
    let mut p = TsQueryParser {
        bytes: s.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    if p.pos >= p.bytes.len() {
        return Err(EvalError::TypeMismatch {
            detail: "tsquery literal: empty".into(),
        });
    }
    let ast = p.parse_or()?;
    p.skip_ws();
    if p.pos < p.bytes.len() {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("tsquery literal: trailing garbage at offset {}", p.pos),
        });
    }
    Ok(ast)
}

struct TsQueryParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> TsQueryParser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }
    fn parse_or(&mut self) -> Result<TsQueryAst, EvalError> {
        let mut lhs = self.parse_and()?;
        loop {
            self.skip_ws();
            if self.peek() != Some(b'|') {
                return Ok(lhs);
            }
            self.pos += 1;
            let rhs = self.parse_and()?;
            lhs = TsQueryAst::Or(Box::new(lhs), Box::new(rhs));
        }
    }
    fn parse_and(&mut self) -> Result<TsQueryAst, EvalError> {
        let mut lhs = self.parse_unary()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'&') => {
                    self.pos += 1;
                    let rhs = self.parse_unary()?;
                    lhs = TsQueryAst::And(Box::new(lhs), Box::new(rhs));
                }
                Some(b'<') => {
                    // Phrase distance `<N>`.
                    self.pos += 1;
                    let start = self.pos;
                    while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_digit() {
                        self.pos += 1;
                    }
                    if start == self.pos || self.peek() != Some(b'>') {
                        return Err(EvalError::TypeMismatch {
                            detail: "tsquery literal: malformed <N> phrase operator".into(),
                        });
                    }
                    let n: u16 = core::str::from_utf8(&self.bytes[start..self.pos])
                        .expect("ascii digits")
                        .parse()
                        .map_err(|_| EvalError::TypeMismatch {
                            detail: "tsquery literal: phrase distance overflows u16".into(),
                        })?;
                    self.pos += 1; // consume '>'
                    let rhs = self.parse_unary()?;
                    lhs = TsQueryAst::Phrase {
                        left: Box::new(lhs),
                        right: Box::new(rhs),
                        distance: n,
                    };
                }
                _ => return Ok(lhs),
            }
        }
    }
    fn parse_unary(&mut self) -> Result<TsQueryAst, EvalError> {
        self.skip_ws();
        if self.peek() == Some(b'!') {
            self.pos += 1;
            let inner = self.parse_unary()?;
            return Ok(TsQueryAst::Not(Box::new(inner)));
        }
        self.parse_atom()
    }
    fn parse_atom(&mut self) -> Result<TsQueryAst, EvalError> {
        self.skip_ws();
        match self.peek() {
            Some(b'(') => {
                self.pos += 1;
                let inner = self.parse_or()?;
                self.skip_ws();
                if self.peek() != Some(b')') {
                    return Err(EvalError::TypeMismatch {
                        detail: "tsquery literal: missing ')'".into(),
                    });
                }
                self.pos += 1;
                Ok(inner)
            }
            Some(b'\'') => {
                self.pos += 1;
                let mut w = String::new();
                loop {
                    match self.peek() {
                        None => {
                            return Err(EvalError::TypeMismatch {
                                detail: "tsquery literal: unterminated quoted lexeme".into(),
                            });
                        }
                        Some(b'\'') => {
                            if self.bytes.get(self.pos + 1) == Some(&b'\'') {
                                w.push('\'');
                                self.pos += 2;
                            } else {
                                self.pos += 1;
                                break;
                            }
                        }
                        Some(b) => {
                            w.push(b as char);
                            self.pos += 1;
                        }
                    }
                }
                // Optional `:WEIGHT_MASK` (digit-mask) — v7.12.0
                // accepts but always stores 0 (any).
                self.skip_weight_suffix();
                Ok(TsQueryAst::Term {
                    word: w,
                    weight_mask: 0,
                })
            }
            Some(b) if b.is_ascii_alphanumeric() || b == b'_' => {
                let start = self.pos;
                while self.pos < self.bytes.len() {
                    let c = self.bytes[self.pos];
                    if c.is_ascii_alphanumeric() || c == b'_' {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                let w = core::str::from_utf8(&self.bytes[start..self.pos])
                    .map_err(|_| EvalError::TypeMismatch {
                        detail: "tsquery literal: non-UTF-8 lexeme".into(),
                    })?
                    .to_string();
                self.skip_weight_suffix();
                Ok(TsQueryAst::Term {
                    word: w,
                    weight_mask: 0,
                })
            }
            Some(b) => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "tsquery literal: unexpected byte {:?} at offset {}",
                    b as char,
                    self.pos
                ),
            }),
            None => Err(EvalError::TypeMismatch {
                detail: "tsquery literal: expected term".into(),
            }),
        }
    }
    fn skip_weight_suffix(&mut self) {
        if self.peek() != Some(b':') {
            return;
        }
        self.pos += 1;
        while let Some(b) = self.peek() {
            if matches!(
                b,
                b'A' | b'B' | b'C' | b'D' | b'a' | b'b' | b'c' | b'd' | b'*'
            ) || b.is_ascii_digit()
            {
                self.pos += 1;
            } else {
                break;
            }
        }
    }
}

pub(super) fn tsvector_concat(
    l: &[spg_storage::TsLexeme],
    r: &[spg_storage::TsLexeme],
) -> Value<'static> {
    let shift = l
        .iter()
        .flat_map(|x| x.positions.iter().copied())
        .max()
        .unwrap_or(0);
    let mut out: Vec<spg_storage::TsLexeme> = l.to_vec();
    for lex in r {
        let shifted: Vec<u16> = lex
            .positions
            .iter()
            .map(|p| p.saturating_add(shift))
            .collect();
        if let Some(existing) = out.iter_mut().find(|x| x.word == lex.word) {
            existing.positions.extend(shifted);
            existing.positions.sort_unstable();
            existing.weight = existing.weight.max(lex.weight);
        } else {
            out.push(spg_storage::TsLexeme {
                word: lex.word.clone(),
                positions: shifted,
                weight: lex.weight,
            });
        }
    }
    out.sort_by(|a, b| a.word.cmp(&b.word));
    Value::TsVector(out)
}
