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
    let (weights, vec, query, norm) = parse_rank_args("ts_rank", args)?;
    match (vec, query) {
        (None, _) | (_, None) => Ok(Value::Null),
        (Some(v), Some(q)) => {
            // Flag 4 (cover-extent distance) is cover-density only — a no-op for
            // ts_rank, matching PG.
            let r = crate::fts::apply_rank_norm(crate::fts::ts_rank(&weights, &v, &q), norm, &v);
            // PG ts_rank returns float4 — keep f32 so the wire text is
            // the shortest-round-trip real form ("0.09148999").
            Ok(Value::Real(r))
        }
    }
}

pub(super) fn fts_ts_rank_cd(args: &[Value<'_>]) -> Result<Value<'static>, EvalError> {
    let (weights, vec, query, norm) = parse_rank_args("ts_rank_cd", args)?;
    if norm & 4 != 0 {
        return Err(EvalError::TypeMismatch {
            detail:
                "ts_rank_cd(): normalization flag 4 (cover-extent distance) is not yet supported"
                    .into(),
        });
    }
    match (vec, query) {
        (None, _) | (_, None) => Ok(Value::Null),
        (Some(v), Some(q)) => {
            let r = crate::fts::apply_rank_norm(crate::fts::ts_rank_cd(&weights, &v, &q), norm, &v);
            Ok(Value::Real(r))
        }
    }
}
/// v7.38 — parsed `ts_rank*` arguments:
/// `(weights, document lexemes, query, normalisation flags)`.
type RankArgs = (
    crate::fts::RankWeights,
    Option<Vec<spg_storage::TsLexeme>>,
    Option<spg_storage::TsQueryAst>,
    i64,
);

/// v7.38 (read01, T12.1) — parse `ts_rank[_cd]([weights,] vec, query [, norm])`.
/// A leading weight array (PG order `[D, C, B, A]`) and a trailing integer
/// normalization flag are both optional. Custom weights are honored; the norm
/// flag bits 1/2/8/16/32 are applied by `apply_rank_norm` (bit 4 is cover-density
/// only, handled by the ts_rank_cd wrapper). Unknown bits error.
fn parse_rank_args(name: &str, args: &[Value<'_>]) -> Result<RankArgs, EvalError> {
    // Split off an optional leading weight array and an optional trailing norm.
    let mut rest = args;
    let mut weights = crate::fts::DEFAULT_RANK_WEIGHTS;
    if matches!(
        rest.first(),
        Some(
            Value::FloatArray(_)
                | Value::NumericArray(_)
                | Value::IntArray(_)
                | Value::SmallIntArray(_)
        )
    ) {
        weights = parse_weight_array(name, &rest[0])?;
        rest = &rest[1..];
    } else if args.len() >= 3
        && let Some(Value::Text(s)) = rest.first()
        && s.trim_start().starts_with('{')
    {
        // v7.39 — an untyped '{0.1, 0.2, 0.4, 1.0}' literal is PG's
        // float4[] weight array via the unknown-literal cast.
        let inner = s.trim().trim_start_matches('{').trim_end_matches('}');
        let parsed: Result<Vec<f64>, _> =
            inner.split(',').map(|x| x.trim().parse::<f64>()).collect();
        let vals = parsed.map_err(|_| EvalError::TypeMismatch {
            detail: format!("{name}(): invalid weight array literal {s:?}"),
        })?;
        weights = parse_weight_array(name, &Value::FloatArray(vals.into_iter().map(Some).collect()))?;
        rest = &rest[1..];
    }
    // A trailing integer is the normalization flag.
    let norm = match rest.last() {
        Some(Value::Int(n)) => Some(i64::from(*n)),
        Some(Value::BigInt(n)) => Some(*n),
        _ => None,
    };
    if norm.is_some() {
        rest = &rest[..rest.len() - 1];
    }
    let norm = norm.unwrap_or(0);
    if norm & !0x3F != 0 {
        return Err(EvalError::TypeMismatch {
            detail: format!("{name}(): unknown normalization flag bits in {norm}"),
        });
    }
    if rest.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "{name}() takes (vec, query) optionally wrapped by a weight array and a norm flag"
            ),
        });
    }
    let vec = match &rest[0] {
        Value::Null => None,
        Value::TsVector(v) => Some(v.clone()),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "{name}() vector arg must be tsvector, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let query = match &rest[1] {
        Value::Null => None,
        Value::TsQuery(q) => Some(q.clone()),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "{name}() query arg must be tsquery, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    Ok((weights, vec, query, norm))
}

/// Read a 4-element weight array in PG order `[D, C, B, A]`.
fn parse_weight_array(name: &str, v: &Value<'_>) -> Result<crate::fts::RankWeights, EvalError> {
    let vals: Vec<f32> = match v {
        Value::FloatArray(a) => a.iter().map(|o| o.unwrap_or(0.0) as f32).collect(),
        Value::IntArray(a) => a.iter().map(|o| o.unwrap_or(0) as f32).collect(),
        Value::SmallIntArray(a) => a.iter().map(|o| f32::from(o.unwrap_or(0))).collect(),
        Value::NumericArray(a) => a
            .iter()
            .map(|o| o.map_or(0.0, |(m, s)| (m as f64 / 10f64.powi(i32::from(s))) as f32))
            .collect(),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: format!("{name}() weight argument must be a numeric array"),
            });
        }
    };
    if vals.len() != 4 {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "{name}() weight array must have 4 elements [D, C, B, A], got {}",
                vals.len()
            ),
        });
    }
    Ok([vals[0], vals[1], vals[2], vals[3]])
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
            // PG spaces the inside of every auto-added group: `( 'a' | 'b' )`.
            out.push_str("( ");
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
                // v7.37 D.51 — PG renders distance-1 phrases with the `<->`
                // adjacency shorthand, and `<N>` for N > 1.
                if *distance == 1 {
                    out.push_str(" <-> ");
                } else {
                    out.push_str(&alloc::format!(" <{distance}> "));
                }
                go(right, own_prec, out);
            }
        }
        write_self(out);
        if need_parens {
            out.push_str(" )");
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
                    // Phrase operator `<N>` (distance N) or `<->` (v7.37 D.51 —
                    // PG's adjacency shorthand, equivalent to `<1>`).
                    self.pos += 1;
                    let n: u16 = if self.peek() == Some(b'-')
                        && self.bytes.get(self.pos + 1) == Some(&b'>')
                    {
                        self.pos += 2; // consume '->'
                        1
                    } else {
                        let start = self.pos;
                        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_digit() {
                            self.pos += 1;
                        }
                        if start == self.pos || self.peek() != Some(b'>') {
                            return Err(EvalError::TypeMismatch {
                                detail: "tsquery literal: malformed <N> / <-> phrase operator"
                                    .into(),
                            });
                        }
                        let val = core::str::from_utf8(&self.bytes[start..self.pos])
                            .expect("ascii digits")
                            .parse()
                            .map_err(|_| EvalError::TypeMismatch {
                                detail: "tsquery literal: phrase distance overflows u16".into(),
                            })?;
                        self.pos += 1; // consume '>'
                        val
                    };
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

/// v7.37.17 (17.6 siblings) — `ts_headline([config,] document,
/// query [, options])`. Wraps every document word whose stemmed
/// form appears as a positive term in the query with StartSel /
/// StopSel (default `<b>` / `</b>`, overridable via the options
/// string). Highlights across the whole document — PG's
/// HighlightAll=true rendering; fragment selection (MaxWords /
/// MinWords / MaxFragments) is accepted in the options string but
/// not applied.
pub(super) fn fts_ts_headline(
    args: &[Value<'_>],
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    // Disambiguate the 2-4 arg forms by where the tsquery sits.
    let is_queryish = |v: &Value<'_>| matches!(v, Value::TsQuery(_));
    let (config_arg, doc_arg, query_arg, opts_arg) = match args {
        [d, q] => (None, d, q, None),
        [d, q, o] if is_queryish(q) => (None, d, q, Some(o)),
        [c, d, q] => (Some(c), d, q, None),
        [c, d, q, o] => (Some(c), d, q, Some(o)),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: format!("ts_headline() takes 2 to 4 args, got {}", args.len()),
            });
        }
    };
    if matches!(doc_arg, Value::Null) || matches!(query_arg, Value::Null) {
        return Ok(Value::Null);
    }
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
        Some(Value::Text(name_str)) => {
            crate::fts::TsConfig::from_name(name_str).ok_or_else(|| EvalError::TypeMismatch {
                detail: format!(
                    "text search config not implemented: {name_str:?} (supported: simple, english)"
                ),
            })?
        }
        Some(other) => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "ts_headline() config must be text, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let doc = match doc_arg {
        Value::Text(s) => s.as_ref(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "ts_headline() document must be text, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let query = match query_arg {
        Value::TsQuery(q) => q.clone(),
        // An unquoted string literal reaches us as Text — PG resolves
        // the unknown literal through the tsquery input parser.
        Value::Text(s) => crate::fts::to_tsquery(config, s)?,
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "ts_headline() query must be tsquery, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    // v7.39 (FTS depth) — full option set: StartSel / StopSel /
    // MaxWords / MinWords / MaxFragments / FragmentDelimiter /
    // HighlightAll. PG defaults per textsearch docs.
    let mut start_sel = String::from("<b>");
    let mut stop_sel = String::from("</b>");
    let mut max_words: usize = 35;
    let mut min_words: usize = 15;
    let mut max_fragments: usize = 0;
    let mut frag_delim = String::from(" ... ");
    let mut highlight_all = false;
    let mut short_word: usize = 3;
    if let Some(opts_v) = opts_arg {
        let opts = match opts_v {
            Value::Null => "",
            Value::Text(s) => s.as_ref(),
            other => {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "ts_headline() options must be text, got {:?}",
                        other.data_type()
                    ),
                });
            }
        };
        // v7.39 (read01, ts_headline validation) — PG validates the
        // option list instead of silently defaulting: malformed pairs
        // are 42601, unknown keys and out-of-range values are 22023,
        // non-integer values are 22P02 (all message-locked vs PG18).
        let parse_int = |v: &str| -> Result<i64, EvalError> {
            v.parse::<i64>().map_err(|_| EvalError::TypeMismatch {
                detail: alloc::format!("invalid input syntax for type integer: {v:?}"),
            })
        };
        let mut short_word_i: i64 = short_word as i64;
        let mut max_fragments_i: i64 = 0;
        let mut min_words_i: i64 = min_words as i64;
        let mut max_words_i: i64 = max_words as i64;
        for pair in opts.split(',') {
            if pair.trim().is_empty() {
                continue;
            }
            let Some((k, v)) = pair.split_once('=') else {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid parameter list format: {:?}",
                        pair.trim()
                    ),
                });
            };
            let v = v.trim().trim_matches('"');
            if v.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "invalid parameter list format: {:?}",
                        pair.trim()
                    ),
                });
            }
            match k.trim().to_ascii_lowercase().as_str() {
                "startsel" => start_sel = v.to_string(),
                "stopsel" => stop_sel = v.to_string(),
                "maxwords" => max_words_i = parse_int(v)?,
                "minwords" => min_words_i = parse_int(v)?,
                "maxfragments" => max_fragments_i = parse_int(v)?,
                "shortword" => short_word_i = parse_int(v)?,
                "fragmentdelimiter" => frag_delim = v.to_string(),
                // PG's boolean reader is lenient: the true spellings
                // flip it on, anything else reads as false (no error).
                "highlightall" => {
                    highlight_all = matches!(
                        v.to_ascii_lowercase().as_str(),
                        "1" | "on" | "t" | "true" | "y" | "yes"
                    );
                }
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "unrecognized headline parameter: {:?}",
                            k.trim()
                        ),
                    });
                }
            }
        }
        // PG's validation order (prsd_headline / mark_hl_fragments
        // observable behavior, both selector modes).
        if min_words_i >= max_words_i {
            return Err(EvalError::TypeMismatch {
                detail: "MinWords must be less than MaxWords".into(),
            });
        }
        if min_words_i <= 0 {
            return Err(EvalError::TypeMismatch {
                detail: "MinWords must be positive".into(),
            });
        }
        if short_word_i < 0 {
            return Err(EvalError::TypeMismatch {
                detail: "ShortWord must be >= 0".into(),
            });
        }
        if max_fragments_i < 0 {
            return Err(EvalError::TypeMismatch {
                detail: "MaxFragments must be >= 0".into(),
            });
        }
        max_words = max_words_i as usize;
        min_words = min_words_i as usize;
        short_word = short_word_i as usize;
        max_fragments = max_fragments_i as usize;
    }
    // Positive query lexemes — Not subtrees excluded.
    fn collect_positive(ast: &spg_storage::TsQueryAst, out: &mut Vec<String>) {
        match ast {
            spg_storage::TsQueryAst::Term { word, .. } => {
                if !word.is_empty() {
                    out.push(word.clone());
                }
            }
            spg_storage::TsQueryAst::And(l, r) | spg_storage::TsQueryAst::Or(l, r) => {
                collect_positive(l, out);
                collect_positive(r, out);
            }
            spg_storage::TsQueryAst::Not(_) => {}
            spg_storage::TsQueryAst::Phrase { left, right, .. } => {
                collect_positive(left, out);
                collect_positive(right, out);
            }
        }
    }
    let mut terms: Vec<String> = Vec::new();
    collect_positive(&query, &mut terms);
    // Tokenise the document into (word, trailing-separator) pairs,
    // marking query matches. Word runs follow the same
    // alphanumeric-or-underscore rule as crate::fts::tokenize so
    // headline matches agree with @@.
    struct HlToken {
        word: String,
        lex: String,
        sep_after: String,
        is_match: bool,
    }
    let mut tokens: Vec<HlToken> = Vec::new();
    let mut leading_sep = String::new();
    let mut word = String::new();
    let mut push_word = |word: &mut String, tokens: &mut Vec<HlToken>| {
        if word.is_empty() {
            return;
        }
        let lowered: String = word.chars().flat_map(|c| c.to_lowercase()).collect();
        let lex = match config {
            crate::fts::TsConfig::Simple => lowered,
            crate::fts::TsConfig::English => crate::fts::porter_stem(&lowered),
        };
        let is_match = terms.iter().any(|t| *t == lex);
        tokens.push(HlToken {
            word: core::mem::take(word),
            lex,
            sep_after: String::new(),
            is_match,
        });
    };
    for c in doc.chars() {
        if c.is_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            push_word(&mut word, &mut tokens);
            match tokens.last_mut() {
                Some(t) => t.sep_after.push(c),
                None => leading_sep.push(c),
            }
        }
    }
    push_word(&mut word, &mut tokens);
    // Render a [lo, hi) token window with highlighting; the final
    // token's separator is dropped (window edges never carry
    // trailing punctuation/whitespace).
    let render = |lo: usize, hi: usize| -> String {
        let mut out = String::new();
        for (i, t) in tokens[lo..hi].iter().enumerate() {
            if t.is_match {
                out.push_str(&start_sel);
                out.push_str(&t.word);
                out.push_str(&stop_sel);
            } else {
                out.push_str(&t.word);
            }
            if lo + i + 1 < hi {
                out.push_str(&t.sep_after);
            }
        }
        out
    };
    let n = tokens.len();
    let match_pos: Vec<usize> = tokens
        .iter()
        .enumerate()
        .filter_map(|(i, t)| t.is_match.then_some(i))
        .collect();
    // HighlightAll / short documents: whole text with its original
    // separators (including the edges).
    if highlight_all || n <= min_words.max(1) {
        let mut out = leading_sep;
        out.push_str(&render(0, n));
        if let Some(t) = tokens.last() {
            out.push_str(&t.sep_after);
        }
        return Ok(Value::text(out));
    }
    // v7.39 (FTS 研读轮) — an unmatched LONG document shows its first
    // MinWords words in both selector modes (PG18 differential; the
    // old whole-text answer was locked against short documents only).
    if match_pos.is_empty() {
        return Ok(Value::text(render(0, min_words.max(1).min(n))));
    }
    if max_fragments > 0 {
        // v7.39 (FTS mark_hl_fragments 研读轮) — PG's MaxFragments
        // selector, clean-room from the studied behaviour of
        // wparser_def.c's mark_hl_fragments/hlCover/get_next_fragment
        // (read01 dir-tsearch note + PG18 source study):
        //   1. hlCover walks minimal windows that contain every
        //      top-level AND branch of the query (an OR branch matches
        //      at any of its terms' positions).
        //   2. Each cover splits into fragments of at most MaxWords
        //      whose both ends are query words.
        //   3. Greedy pick: most interesting words, ties to fewer
        //      words, MaxFragments times; each pick stretches — left
        //      by at most (MaxWords - len) / 2, right with the whole
        //      remainder — never crossing an already-chosen fragment,
        //      then shrinks both ends off BAD endpoints (a short word
        //      of <= ShortWord chars or an all-digit word, unless it
        //      is itself a query word). Overlapping candidates are
        //      excluded, chosen fragments render in document order.
        //   4. No cover at all -> the first MinWords words (the only
        //      place MinWords matters in fragment mode).
        // SPG's token stream has no SPACE/TAG tokens (separators ride
        // on the preceding word), so PG's NONWORDTOKEN skips collapse
        // away and every token counts as one word.
        let interesting: Vec<bool> = tokens.iter().map(|t| t.is_match).collect();
        let is_bad_endpoint = |i: usize| -> bool {
            if interesting[i] {
                return false;
            }
            let w = &tokens[i].word;
            w.chars().count() <= short_word || w.chars().all(|c| c.is_ascii_digit())
        };
        // Top-level AND groups; each group's positions are the union
        // of its terms' matches.
        fn and_groups(ast: &spg_storage::TsQueryAst, out: &mut Vec<Vec<String>>) {
            match ast {
                spg_storage::TsQueryAst::And(l, r) => {
                    and_groups(l, out);
                    and_groups(r, out);
                }
                spg_storage::TsQueryAst::Not(_) => {}
                other => {
                    let mut g = Vec::new();
                    // reuse the positive-term collector on the branch
                    fn collect(ast: &spg_storage::TsQueryAst, out: &mut Vec<String>) {
                        match ast {
                            spg_storage::TsQueryAst::Term { word, .. } => {
                                if !word.is_empty() {
                                    out.push(word.clone());
                                }
                            }
                            spg_storage::TsQueryAst::And(l, r)
                            | spg_storage::TsQueryAst::Or(l, r) => {
                                collect(l, out);
                                collect(r, out);
                            }
                            spg_storage::TsQueryAst::Not(_) => {}
                            spg_storage::TsQueryAst::Phrase { left, right, .. } => {
                                collect(left, out);
                                collect(right, out);
                            }
                        }
                    }
                    collect(other, &mut g);
                    if !g.is_empty() {
                        out.push(g);
                    }
                }
            }
        }
        let mut groups: Vec<Vec<String>> = Vec::new();
        and_groups(&query, &mut groups);
        let group_pos: Vec<Vec<usize>> = groups
            .iter()
            .map(|g| {
                tokens
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| g.iter().any(|term| *term == t.lex))
                    .map(|(i, _)| i)
                    .collect()
            })
            .collect();
        // Candidate fragments: (startpos, endpos inclusive, words, interesting).
        struct Cand {
            st: usize,
            en: usize,
            curlen: usize,
            poslen: usize,
            chosen: bool,
            excluded: bool,
        }
        let mut cands: Vec<Cand> = Vec::new();
        if !group_pos.is_empty() && group_pos.iter().all(|ps| !ps.is_empty()) {
            let mut nextpos = 0usize;
            loop {
                // earliest window at/after nextpos containing one
                // position from every group
                let mut pose = 0usize;
                let mut dead = false;
                for ps in &group_pos {
                    match ps.iter().find(|&&p| p >= nextpos) {
                        Some(&p) => pose = pose.max(p),
                        None => {
                            dead = true;
                            break;
                        }
                    }
                }
                if dead {
                    break;
                }
                let mut posb = usize::MAX;
                for ps in &group_pos {
                    if let Some(&p) = ps.iter().rev().find(|&&p| p <= pose) {
                        posb = posb.min(p);
                    }
                }
                let posb = posb.max(nextpos);
                // split [posb, pose] into fragments of <= MaxWords with
                // query words at both ends
                let (mut st, en_cover) = (posb, pose);
                while st <= en_cover {
                    // advance st to an interesting word
                    let mut i = st;
                    while i < en_cover && !interesting[i] {
                        i += 1;
                    }
                    st = i;
                    let mut curlen = 0usize;
                    let mut poslen = 0usize;
                    i = st;
                    while i <= en_cover && curlen < max_words.max(1) {
                        curlen += 1;
                        if interesting[i] {
                            poslen += 1;
                        }
                        i += 1;
                    }
                    // if the cover was cut, back the end up to a query word
                    let mut en = i - 1;
                    if en < en_cover {
                        while en > st && !interesting[en] {
                            curlen -= 1;
                            en -= 1;
                        }
                    }
                    cands.push(Cand {
                        st,
                        en,
                        curlen,
                        poslen,
                        chosen: false,
                        excluded: false,
                    });
                    st = en + 1;
                }
                nextpos = posb + 1;
            }
        }
        // Greedy selection + stretch + overlap exclusion.
        let mut in_frag: Vec<bool> = alloc::vec![false; n];
        let mut picked = 0usize;
        for _ in 0..max_fragments {
            let mut best: Option<usize> = None;
            for (i, c) in cands.iter().enumerate() {
                if c.chosen || c.excluded {
                    continue;
                }
                let better = match best {
                    None => true,
                    Some(b) => {
                        c.poslen > cands[b].poslen
                            || (c.poslen == cands[b].poslen && c.curlen < cands[b].curlen)
                    }
                };
                if better {
                    best = Some(i);
                }
            }
            let Some(bi) = best else { break };
            let (mut st, mut en, mut curlen) = (cands[bi].st, cands[bi].en, cands[bi].curlen);
            if curlen < max_words {
                // stretch left by at most half the remainder, never
                // crossing an already-chosen fragment
                let maxstretch = (max_words - curlen) / 2;
                let mut stretch = 0usize;
                let mut posmarker = st;
                let mut i = st;
                while i > 0 && stretch < maxstretch && !in_frag[i - 1] {
                    i -= 1;
                    curlen += 1;
                    stretch += 1;
                    posmarker = i;
                }
                // shrink back off bad endpoints
                let mut i = posmarker;
                while i < st && is_bad_endpoint(i) {
                    curlen -= 1;
                    i += 1;
                }
                st = i;
                // stretch right with the whole remainder
                let mut posmarker = en;
                let mut i = en + 1;
                while i < n && curlen < max_words && !in_frag[i] {
                    curlen += 1;
                    posmarker = i;
                    i += 1;
                }
                // shrink back off bad endpoints
                let mut i = posmarker;
                while i > en && is_bad_endpoint(i) {
                    curlen -= 1;
                    i -= 1;
                }
                en = i;
            }
            cands[bi].st = st;
            cands[bi].en = en;
            cands[bi].curlen = curlen;
            cands[bi].chosen = true;
            for k in st..=en {
                in_frag[k] = true;
            }
            picked += 1;
            for (i, c) in cands.iter_mut().enumerate() {
                if i != bi
                    && ((c.st >= st && c.st <= en)
                        || (c.en >= st && c.en <= en)
                        || (c.st < st && c.en > en))
                {
                    c.excluded = true;
                }
            }
        }
        if picked == 0 {
            let hi = min_words.max(1).min(n);
            return Ok(Value::text(render(0, hi)));
        }
        let mut chosen: Vec<(usize, usize)> = cands
            .iter()
            .filter(|c| c.chosen)
            .map(|c| (c.st, c.en))
            .collect();
        chosen.sort_unstable();
        let parts: Vec<String> = chosen
            .iter()
            .map(|&(st, en)| render(st, en + 1))
            .collect();
        return Ok(Value::text(parts.join(&frag_delim)));
    }
    // Window mode: the cover is the smallest span holding every
    // matched position (capped at MaxWords from its start), then
    // extended to MinWords — rightward first, leftward for the
    // remainder (differential-locked against PG18).
    let first = match_pos[0];
    let last = *match_pos.last().expect("non-empty");
    let mut lo = first;
    let mut hi = (last + 1).min(lo + max_words.max(1)).min(n);
    while hi - lo < min_words.max(1) && hi < n {
        hi += 1;
    }
    while hi - lo < min_words.max(1) && lo > 0 {
        lo -= 1;
    }
    Ok(Value::text(render(lo, hi)))
}

/// v7.37.17 (17.6 siblings) — `ts_rewrite(query, target,
/// substitute)`: replaces every occurrence of the `target` subtree
/// inside `query` with `substitute` — the synonym-expansion
/// primitive (`ts_rewrite('a & b', 'a', 'foo|bar')`). Structural
/// subtree equality; the SELECT-driven catalog form
/// (`ts_rewrite(query, 'SELECT t, s FROM aliases')`) is not
/// supported — it needs a query-in-function executor.
pub(super) fn fts_ts_rewrite(
    args: &[Value<'_>],
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
    if args.len() != 3 {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "ts_rewrite() takes 3 args (query, target, substitute), got {}",
                args.len()
            ),
        });
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let config = match ctx.default_text_search_config {
        Some(name_str) => {
            crate::fts::TsConfig::from_name(name_str).unwrap_or(crate::fts::TsConfig::Simple)
        }
        None => crate::fts::TsConfig::Simple,
    };
    let as_query = |v: &Value<'_>, which: &str| -> Result<spg_storage::TsQueryAst, EvalError> {
        match v {
            Value::TsQuery(q) => Ok(q.clone()),
            // Unknown string literals resolve through the tsquery
            // input parser, as in PG.
            Value::Text(s) => crate::fts::to_tsquery(config, s),
            other => Err(EvalError::TypeMismatch {
                detail: format!(
                    "ts_rewrite() {which} must be tsquery, got {:?}",
                    other.data_type()
                ),
            }),
        }
    };
    let query = as_query(&args[0], "query")?;
    let target = as_query(&args[1], "target")?;
    let substitute = as_query(&args[2], "substitute")?;
    fn rewrite(
        node: &spg_storage::TsQueryAst,
        target: &spg_storage::TsQueryAst,
        substitute: &spg_storage::TsQueryAst,
    ) -> spg_storage::TsQueryAst {
        if node == target {
            return substitute.clone();
        }
        use spg_storage::TsQueryAst as A;
        match node {
            A::Term { .. } => node.clone(),
            A::And(l, r) => A::And(
                Box::new(rewrite(l, target, substitute)),
                Box::new(rewrite(r, target, substitute)),
            ),
            A::Or(l, r) => A::Or(
                Box::new(rewrite(l, target, substitute)),
                Box::new(rewrite(r, target, substitute)),
            ),
            A::Not(x) => A::Not(Box::new(rewrite(x, target, substitute))),
            A::Phrase {
                left,
                right,
                distance,
            } => A::Phrase {
                left: Box::new(rewrite(left, target, substitute)),
                right: Box::new(rewrite(right, target, substitute)),
                distance: *distance,
            },
        }
    }
    Ok(Value::TsQuery(rewrite(&query, &target, &substitute)))
}

/// v7.37.17 (17.6 siblings) — the tsquery boolean catalog
/// functions: tsquery_and / tsquery_or (2-arg) and tsquery_not
/// (1-arg) are the function forms of the && / || / !! operators.
/// Unknown string literals resolve through the tsquery input
/// parser, as everywhere else in the FTS surface.
pub(super) fn fts_tsquery_bool(
    args: &[Value<'_>],
    ctx: &EvalContext<'_>,
    op: &str,
) -> Result<Value<'static>, EvalError> {
    let arity = if op == "not" { 1 } else { 2 };
    if args.len() != arity {
        return Err(EvalError::TypeMismatch {
            detail: format!("tsquery_{op}() takes {arity} arg(s), got {}", args.len()),
        });
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let config = match ctx.default_text_search_config {
        Some(name_str) => {
            crate::fts::TsConfig::from_name(name_str).unwrap_or(crate::fts::TsConfig::Simple)
        }
        None => crate::fts::TsConfig::Simple,
    };
    let as_query = |v: &Value<'_>| -> Result<spg_storage::TsQueryAst, EvalError> {
        match v {
            Value::TsQuery(q) => Ok(q.clone()),
            Value::Text(s) => crate::fts::to_tsquery(config, s),
            other => Err(EvalError::TypeMismatch {
                detail: format!(
                    "tsquery_{op}() arguments must be tsquery, got {:?}",
                    other.data_type()
                ),
            }),
        }
    };
    use spg_storage::TsQueryAst as A;
    let out = match op {
        "and" => A::And(Box::new(as_query(&args[0])?), Box::new(as_query(&args[1])?)),
        "or" => A::Or(Box::new(as_query(&args[0])?), Box::new(as_query(&args[1])?)),
        _ => A::Not(Box::new(as_query(&args[0])?)),
    };
    Ok(Value::TsQuery(out))
}
