//! Lexer for the PG-dialect subset that SPG accepts.
//!
//! v0.2 token stream is value-only — no source spans yet. Errors do report
//! the byte offset where the offending construct started. Identifiers are
//! ASCII case-folded to lower-case (matches PG when un-quoted). Quoted
//! identifiers (`"..."`) preserve case; `""` is an embedded quote.
//! String literals (`'...'`) follow PG single-quote convention with `''`
//! as the embedded quote. The lexer accepts but does not interpret E-strings
//! or dollar-quoted strings — those land in a later milestone.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // Keywords
    Select,
    From,
    Where,
    As,
    Null,
    True,
    False,
    And,
    Or,
    Not,
    Create,
    Table,
    Insert,
    Into,
    Values,
    Index,
    On,
    Begin,
    Commit,
    Rollback,
    Order,
    By,
    Limit,

    // Identifiers
    Ident(String),       // ASCII case-folded
    QuotedIdent(String), // original case, "" → "

    // Literals
    Integer(i64),
    Float(f64),
    String(String),

    // Operators
    Plus,
    Minus,
    Star,
    Slash,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,

    // Punctuation
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Semicolon,
    Dot,
    /// pgvector L2 distance operator `<->`. Lexed as one token so the
    /// parser can give it its own precedence rung.
    /// v4.14 `->` — JSON object/array element access, returns json.
    JsonGet,
    /// v4.14 `->>` — same access, returns text.
    JsonGetText,
    L2Distance,
    /// pgvector inner-product operator `<#>` (returns negative dot product
    /// so smaller still means more similar — same semantics as pgvector).
    InnerProduct,
    /// pgvector cosine distance operator `<=>`.
    CosineDistance,
    /// PG-style cast `expr::type` — single token because we want it to bind
    /// at postfix precedence.
    DoubleColon,
    /// Standard SQL string concatenation `||`.
    Concat,
    /// `IS` keyword — postfix `IS NULL` / `IS NOT NULL` predicates.
    Is,
    Between,
    In,
    Like,
    Group,
    Distinct,
    Union,
    All,
    Join,
    Inner,
    Left,
    Cross,
    Outer,
    Default,
    Savepoint,
    Release,
    To,
    Having,
    Show,
    Extract,
    Offset,
    Asc,
    Desc,
    /// `INTERVAL` — followed by a string literal carrying the span text
    /// (e.g. `INTERVAL '1 day 2 hours'`).
    Interval,
    /// v6.1.1 — `$N` parameter placeholder for the extended query
    /// protocol. The number N is 1-based per PostgreSQL convention.
    /// `0` and `$0` are not valid; the lexer rejects them.
    Placeholder(u16),

    /// v6.1.2 — `DROP` keyword. Used by `DROP PUBLICATION <name>`.
    /// Reserved for future `DROP TABLE` / `DROP INDEX` / `DROP USER`
    /// surface that currently goes through SHOW-shaped admin SQL.
    Drop,
    /// v6.1.2 — `FOR` keyword (publication scope).
    For,
    /// v6.1.2 — `TABLES` plural keyword (`FOR ALL TABLES`,
    /// `FOR ALL TABLES EXCEPT …`). The existing `TABLE` keyword
    /// stays a separate token so `CREATE TABLE`'s single-table
    /// form keeps lexing as today.
    Tables,
    /// v6.1.3 (reserved at v6.1.2 to keep the AST shape stable) —
    /// `EXCEPT` keyword for `FOR ALL TABLES EXCEPT t1, t2`.
    Except,
    /// v6.1.2 — `PUBLICATION` keyword.
    Publication,
    /// v6.1.4 (reserved at v6.1.2) — `SUBSCRIPTION` keyword.
    Subscription,

    Eof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LexErrorKind {
    UnknownChar(char),
    UnterminatedString,
    UnterminatedQuotedIdent,
    UnterminatedBlockComment,
    BadNumber(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    pub kind: LexErrorKind,
    pub pos: usize,
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            LexErrorKind::UnknownChar(c) => write!(f, "unknown char {c:?} at byte {}", self.pos),
            LexErrorKind::UnterminatedString => {
                write!(f, "unterminated string literal at byte {}", self.pos)
            }
            LexErrorKind::UnterminatedQuotedIdent => {
                write!(f, "unterminated quoted identifier at byte {}", self.pos)
            }
            LexErrorKind::UnterminatedBlockComment => {
                write!(f, "unterminated /* */ comment at byte {}", self.pos)
            }
            LexErrorKind::BadNumber(s) => {
                write!(f, "invalid number literal {s:?} at byte {}", self.pos)
            }
        }
    }
}

/// Tokenize `input` into a `Vec<Token>` ending in `Token::Eof`.
#[allow(clippy::too_many_lines)] // big match — splitting would obscure the dispatch table
pub fn tokenize(input: &str) -> Result<Vec<Token>, LexError> {
    let bytes = input.as_bytes();
    let mut i = 0usize;
    let mut out = Vec::new();

    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b' ' | b'\t' | b'\n' | b'\r' => {
                i += 1;
            }
            b'-' if peek_eq(bytes, i + 1, b'-') => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if peek_eq(bytes, i + 1, b'*') => {
                let start = i;
                i += 2;
                let mut closed = false;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        closed = true;
                        break;
                    }
                    i += 1;
                }
                if !closed {
                    return Err(LexError {
                        kind: LexErrorKind::UnterminatedBlockComment,
                        pos: start,
                    });
                }
            }
            b'\'' => {
                let (tok, consumed) = lex_quoted(input, i, b'\'', false)?;
                out.push(tok);
                i += consumed;
            }
            b'"' => {
                let (tok, consumed) = lex_quoted(input, i, b'"', true)?;
                out.push(tok);
                i += consumed;
            }
            // MySQL-flavoured backtick-quoted identifier. Same semantics
            // as the standard `"..."` form, including embedded "``" as
            // a literal backtick.
            b'`' => {
                let (tok, consumed) = lex_quoted(input, i, b'`', true)?;
                out.push(tok);
                i += consumed;
            }
            b if b.is_ascii_alphabetic() || b == b'_' => {
                let start = i;
                i += 1;
                while i < bytes.len() {
                    let c = bytes[i];
                    if c.is_ascii_alphanumeric() || c == b'_' {
                        i += 1;
                    } else {
                        break;
                    }
                }
                let raw = &input[start..i];
                // v3.0.5: try the keyword table case-insensitively
                // without allocating; only the ident fall-through
                // pays for a lowercase String.
                out.push(keyword_or_ident_raw(raw));
            }
            b if b.is_ascii_digit() => {
                let (tok, consumed) =
                    lex_number(&input[i..]).map_err(|kind| LexError { kind, pos: i })?;
                out.push(tok);
                i += consumed;
            }
            b'.' if peek_pred(bytes, i + 1, u8::is_ascii_digit) => {
                let (tok, consumed) =
                    lex_number(&input[i..]).map_err(|kind| LexError { kind, pos: i })?;
                out.push(tok);
                i += consumed;
            }
            b'+' => single(&mut out, Token::Plus, &mut i),
            b'-' => {
                // v4.14: `->>` and `->` for JSON path access. `->>`
                // must be tried before `->` (longest match).
                if peek_eq(bytes, i + 1, b'>') && peek_eq(bytes, i + 2, b'>') {
                    out.push(Token::JsonGetText);
                    i += 3;
                } else if peek_eq(bytes, i + 1, b'>') {
                    out.push(Token::JsonGet);
                    i += 2;
                } else {
                    single(&mut out, Token::Minus, &mut i);
                }
            }
            b'*' => single(&mut out, Token::Star, &mut i),
            b'/' => single(&mut out, Token::Slash, &mut i),
            b'(' => single(&mut out, Token::LParen, &mut i),
            b')' => single(&mut out, Token::RParen, &mut i),
            b'[' => single(&mut out, Token::LBracket, &mut i),
            b']' => single(&mut out, Token::RBracket, &mut i),
            b',' => single(&mut out, Token::Comma, &mut i),
            b';' => single(&mut out, Token::Semicolon, &mut i),
            b'.' => single(&mut out, Token::Dot, &mut i),
            b'=' => single(&mut out, Token::Eq, &mut i),
            b'<' => {
                if peek_eq(bytes, i + 1, b'=') && peek_eq(bytes, i + 2, b'>') {
                    out.push(Token::CosineDistance);
                    i += 3;
                } else if peek_eq(bytes, i + 1, b'#') && peek_eq(bytes, i + 2, b'>') {
                    out.push(Token::InnerProduct);
                    i += 3;
                } else if peek_eq(bytes, i + 1, b'-') && peek_eq(bytes, i + 2, b'>') {
                    out.push(Token::L2Distance);
                    i += 3;
                } else if peek_eq(bytes, i + 1, b'=') {
                    out.push(Token::LtEq);
                    i += 2;
                } else if peek_eq(bytes, i + 1, b'>') {
                    out.push(Token::NotEq);
                    i += 2;
                } else {
                    out.push(Token::Lt);
                    i += 1;
                }
            }
            b':' if peek_eq(bytes, i + 1, b':') => {
                out.push(Token::DoubleColon);
                i += 2;
            }
            b'|' if peek_eq(bytes, i + 1, b'|') => {
                out.push(Token::Concat);
                i += 2;
            }
            b'>' => {
                if peek_eq(bytes, i + 1, b'=') {
                    out.push(Token::GtEq);
                    i += 2;
                } else {
                    out.push(Token::Gt);
                    i += 1;
                }
            }
            b'!' if peek_eq(bytes, i + 1, b'=') => {
                out.push(Token::NotEq);
                i += 2;
            }
            // v6.1.1: `$N` parameter placeholder for the extended
            // query protocol. PG numbers them 1..=N; we reject $0
            // and a bare `$` not followed by a digit. Dollar-quoted
            // strings ($$ ... $$) are not supported here — they're
            // a separate lexer feature filed for a future release.
            b'$' if i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() => {
                let mut j = i + 1;
                let mut n: u32 = 0;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    n = n.saturating_mul(10).saturating_add(u32::from(bytes[j] - b'0'));
                    j += 1;
                }
                if n == 0 || n > u32::from(u16::MAX) {
                    return Err(LexError {
                        kind: LexErrorKind::BadNumber(input[i..j].to_string()),
                        pos: i,
                    });
                }
                #[allow(clippy::cast_possible_truncation)]
                out.push(Token::Placeholder(n as u16));
                i = j;
            }
            _ => {
                let ch = input[i..].chars().next().unwrap_or('?');
                return Err(LexError {
                    kind: LexErrorKind::UnknownChar(ch),
                    pos: i,
                });
            }
        }
    }
    out.push(Token::Eof);
    Ok(out)
}

fn peek_eq(bytes: &[u8], i: usize, target: u8) -> bool {
    bytes.get(i) == Some(&target)
}

fn peek_pred<F: Fn(&u8) -> bool>(bytes: &[u8], i: usize, pred: F) -> bool {
    bytes.get(i).is_some_and(pred)
}

fn single(out: &mut Vec<Token>, tok: Token, i: &mut usize) {
    out.push(tok);
    *i += 1;
}

/// Length-first ASCII-CI keyword lookup. Avoids allocating a
/// lowercase `String` when the input matches a keyword; only the ident
/// fall-through path pays for the lowercase copy.
///
/// Grouped by length so the outer `match` becomes a small jump table.
/// Within a length bucket every keyword has either a unique first
/// byte (cheap dispatch) or a small set of disambiguating
/// trailing-byte comparisons. All comparisons are ASCII-CI (XOR
/// 0x20 on each byte before the compare).
fn keyword_or_ident_raw(raw: &str) -> Token {
    let b = raw.as_bytes();
    let tok = match b.len() {
        2 => kw_len2(b),
        3 => kw_len3(b),
        4 => kw_len4(b),
        5 => kw_len5(b),
        6 => kw_len6(b),
        7 => kw_len7(b),
        8 => kw_len8(b),
        9 => kw_len9(b),
        11 => kw_len11(b),
        12 => kw_len12(b),
        _ => None,
    };
    match tok {
        Some(t) => t,
        // Ident fall-through: this is the only path that allocates.
        None => Token::Ident(raw.to_ascii_lowercase()),
    }
}

/// ASCII-CI equality on a byte slice against a lowercase literal.
/// Letters that differ only in case satisfy `(a ^ b) == 0x20`; other
/// mismatches set bits outside the 0x20 mask. We compare each byte
/// against its lowercase form via `to_ascii_lowercase` for clarity;
/// the compiler folds the loop into a tight cmov chain.
#[inline]
fn eq_ci(input: &[u8], lower: &[u8]) -> bool {
    if input.len() != lower.len() {
        return false;
    }
    for i in 0..lower.len() {
        if input[i].to_ascii_lowercase() != lower[i] {
            return false;
        }
    }
    true
}

#[inline]
fn kw_len2(b: &[u8]) -> Option<Token> {
    // 7 keywords: as, by, in, is, on, or, to
    if eq_ci(b, b"as") {
        return Some(Token::As);
    }
    if eq_ci(b, b"by") {
        return Some(Token::By);
    }
    if eq_ci(b, b"in") {
        return Some(Token::In);
    }
    if eq_ci(b, b"is") {
        return Some(Token::Is);
    }
    if eq_ci(b, b"on") {
        return Some(Token::On);
    }
    if eq_ci(b, b"or") {
        return Some(Token::Or);
    }
    if eq_ci(b, b"to") {
        return Some(Token::To);
    }
    None
}

#[inline]
fn kw_len3(b: &[u8]) -> Option<Token> {
    // 5 keywords: all, and, asc, not, for
    if eq_ci(b, b"for") {
        return Some(Token::For);
    }
    if eq_ci(b, b"all") {
        return Some(Token::All);
    }
    if eq_ci(b, b"and") {
        return Some(Token::And);
    }
    if eq_ci(b, b"asc") {
        return Some(Token::Asc);
    }
    if eq_ci(b, b"not") {
        return Some(Token::Not);
    }
    None
}

#[inline]
fn kw_len4(b: &[u8]) -> Option<Token> {
    // 10 keywords: from, null, true, into, like, join, left, show, desc, drop
    if eq_ci(b, b"from") {
        return Some(Token::From);
    }
    if eq_ci(b, b"drop") {
        return Some(Token::Drop);
    }
    if eq_ci(b, b"null") {
        return Some(Token::Null);
    }
    if eq_ci(b, b"true") {
        return Some(Token::True);
    }
    if eq_ci(b, b"into") {
        return Some(Token::Into);
    }
    if eq_ci(b, b"like") {
        return Some(Token::Like);
    }
    if eq_ci(b, b"join") {
        return Some(Token::Join);
    }
    if eq_ci(b, b"left") {
        return Some(Token::Left);
    }
    if eq_ci(b, b"show") {
        return Some(Token::Show);
    }
    if eq_ci(b, b"desc") {
        return Some(Token::Desc);
    }
    None
}

#[inline]
fn kw_len5(b: &[u8]) -> Option<Token> {
    // 12 keywords: false, where, table, index, begin, order, limit,
    // group, union, inner, cross, outer
    if eq_ci(b, b"false") {
        return Some(Token::False);
    }
    if eq_ci(b, b"where") {
        return Some(Token::Where);
    }
    if eq_ci(b, b"table") {
        return Some(Token::Table);
    }
    if eq_ci(b, b"index") {
        return Some(Token::Index);
    }
    if eq_ci(b, b"begin") {
        return Some(Token::Begin);
    }
    if eq_ci(b, b"order") {
        return Some(Token::Order);
    }
    if eq_ci(b, b"limit") {
        return Some(Token::Limit);
    }
    if eq_ci(b, b"group") {
        return Some(Token::Group);
    }
    if eq_ci(b, b"union") {
        return Some(Token::Union);
    }
    if eq_ci(b, b"inner") {
        return Some(Token::Inner);
    }
    if eq_ci(b, b"cross") {
        return Some(Token::Cross);
    }
    if eq_ci(b, b"outer") {
        return Some(Token::Outer);
    }
    None
}

#[inline]
fn kw_len6(b: &[u8]) -> Option<Token> {
    // 9 keywords: select, create, insert, values, commit, having, offset, tables, except
    if eq_ci(b, b"select") {
        return Some(Token::Select);
    }
    if eq_ci(b, b"tables") {
        return Some(Token::Tables);
    }
    if eq_ci(b, b"except") {
        return Some(Token::Except);
    }
    if eq_ci(b, b"create") {
        return Some(Token::Create);
    }
    if eq_ci(b, b"insert") {
        return Some(Token::Insert);
    }
    if eq_ci(b, b"values") {
        return Some(Token::Values);
    }
    if eq_ci(b, b"commit") {
        return Some(Token::Commit);
    }
    if eq_ci(b, b"having") {
        return Some(Token::Having);
    }
    if eq_ci(b, b"offset") {
        return Some(Token::Offset);
    }
    None
}

#[inline]
fn kw_len7(b: &[u8]) -> Option<Token> {
    // 4 keywords: between, default, release, extract
    if eq_ci(b, b"between") {
        return Some(Token::Between);
    }
    if eq_ci(b, b"default") {
        return Some(Token::Default);
    }
    if eq_ci(b, b"release") {
        return Some(Token::Release);
    }
    if eq_ci(b, b"extract") {
        return Some(Token::Extract);
    }
    None
}

#[inline]
fn kw_len8(b: &[u8]) -> Option<Token> {
    // 3 keywords: rollback, distinct, interval
    if eq_ci(b, b"rollback") {
        return Some(Token::Rollback);
    }
    if eq_ci(b, b"distinct") {
        return Some(Token::Distinct);
    }
    if eq_ci(b, b"interval") {
        return Some(Token::Interval);
    }
    None
}

#[inline]
fn kw_len9(b: &[u8]) -> Option<Token> {
    // 1 keyword: savepoint
    if eq_ci(b, b"savepoint") {
        return Some(Token::Savepoint);
    }
    None
}

#[inline]
fn kw_len11(b: &[u8]) -> Option<Token> {
    // 1 keyword: publication
    if eq_ci(b, b"publication") {
        return Some(Token::Publication);
    }
    None
}

#[inline]
fn kw_len12(b: &[u8]) -> Option<Token> {
    // 1 keyword: subscription
    if eq_ci(b, b"subscription") {
        return Some(Token::Subscription);
    }
    None
}

/// Lex a `'...'` string literal or `"..."` quoted identifier. The opening
/// quote sits at `input[start]`; `quote` is its byte value. `is_ident` selects
/// the resulting token shape.
///
/// PG-style doubling escapes the quote: `''` inside `'...'` is a literal `'`,
/// same for `""` inside `"..."`.
fn lex_quoted(
    input: &str,
    start: usize,
    quote: u8,
    is_ident: bool,
) -> Result<(Token, usize), LexError> {
    let bytes = input.as_bytes();
    let mut i = start + 1;
    let mut s = String::new();
    loop {
        if i >= bytes.len() {
            return Err(LexError {
                kind: if is_ident {
                    LexErrorKind::UnterminatedQuotedIdent
                } else {
                    LexErrorKind::UnterminatedString
                },
                pos: start,
            });
        }
        if bytes[i] == quote {
            if peek_eq(bytes, i + 1, quote) {
                s.push(quote as char);
                i += 2;
            } else {
                i += 1;
                break;
            }
        } else {
            let ch = input[i..].chars().next().expect("non-empty UTF-8 boundary");
            s.push(ch);
            i += ch.len_utf8();
        }
    }
    let tok = if is_ident {
        Token::QuotedIdent(s)
    } else {
        Token::String(s)
    };
    Ok((tok, i - start))
}

fn lex_number(s: &str) -> Result<(Token, usize), LexErrorKind> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    let mut is_float = false;

    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'.' {
        is_float = true;
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        is_float = true;
        i += 1;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        let exp_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if exp_start == i {
            return Err(LexErrorKind::BadNumber(s[..i].to_string()));
        }
    }

    let lit = &s[..i];
    if is_float {
        lit.parse::<f64>()
            .map(|v| (Token::Float(v), i))
            .map_err(|_| LexErrorKind::BadNumber(lit.to_string()))
    } else {
        lit.parse::<i64>()
            .map(|v| (Token::Integer(v), i))
            .map_err(|_| LexErrorKind::BadNumber(lit.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn lex(s: &str) -> Vec<Token> {
        tokenize(s).expect("lex ok")
    }

    #[test]
    fn empty_yields_only_eof() {
        assert_eq!(lex(""), vec![Token::Eof]);
    }

    #[test]
    fn whitespace_only_yields_only_eof() {
        assert_eq!(lex("   \t\n  "), vec![Token::Eof]);
    }

    #[test]
    fn keywords_are_case_insensitive() {
        assert_eq!(
            lex("SELECT select Select"),
            vec![Token::Select, Token::Select, Token::Select, Token::Eof]
        );
    }

    #[test]
    fn identifiers_lowercase_ascii() {
        assert_eq!(
            lex("hello WORLD _x x1"),
            vec![
                Token::Ident("hello".into()),
                Token::Ident("world".into()),
                Token::Ident("_x".into()),
                Token::Ident("x1".into()),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn quoted_identifier_keeps_case_and_handles_embedded_quote() {
        assert_eq!(
            lex(r#""User Name" "a""b""#),
            vec![
                Token::QuotedIdent("User Name".into()),
                Token::QuotedIdent("a\"b".into()),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn integer_and_float_literals() {
        assert_eq!(
            lex("0 42 1.5 .5 1e10 2.5e-3"),
            vec![
                Token::Integer(0),
                Token::Integer(42),
                Token::Float(1.5),
                Token::Float(0.5),
                Token::Float(1e10),
                Token::Float(2.5e-3),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn negative_number_is_minus_then_integer() {
        // PG follows this: unary minus is a separate token, parser folds it.
        assert_eq!(
            lex("-42"),
            vec![Token::Minus, Token::Integer(42), Token::Eof]
        );
    }

    #[test]
    fn string_literal_doubled_quote_escape() {
        assert_eq!(
            lex("'hello' 'it''s'"),
            vec![
                Token::String("hello".into()),
                Token::String("it's".into()),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn all_comparison_and_arithmetic_operators() {
        assert_eq!(
            lex("= <> != < <= > >= + - * /"),
            vec![
                Token::Eq,
                Token::NotEq,
                Token::NotEq,
                Token::Lt,
                Token::LtEq,
                Token::Gt,
                Token::GtEq,
                Token::Plus,
                Token::Minus,
                Token::Star,
                Token::Slash,
                Token::Eof,
            ]
        );
    }

    #[test]
    fn punctuation() {
        assert_eq!(
            lex("( ) , ; ."),
            vec![
                Token::LParen,
                Token::RParen,
                Token::Comma,
                Token::Semicolon,
                Token::Dot,
                Token::Eof,
            ]
        );
    }

    #[test]
    fn line_comment_skipped() {
        assert_eq!(
            lex("SELECT -- trailing junk\nFROM"),
            vec![Token::Select, Token::From, Token::Eof]
        );
    }

    #[test]
    fn block_comment_skipped() {
        assert_eq!(
            lex("SELECT /* skipped */ 1"),
            vec![Token::Select, Token::Integer(1), Token::Eof]
        );
    }

    #[test]
    fn unterminated_string_errors() {
        let err = tokenize("'oops").unwrap_err();
        assert!(matches!(err.kind, LexErrorKind::UnterminatedString));
        assert_eq!(err.pos, 0);
    }

    #[test]
    fn unterminated_block_comment_errors() {
        let err = tokenize("/* never closed").unwrap_err();
        assert!(matches!(err.kind, LexErrorKind::UnterminatedBlockComment));
    }

    #[test]
    fn unknown_char_errors() {
        let err = tokenize("@").unwrap_err();
        assert!(matches!(err.kind, LexErrorKind::UnknownChar('@')));
    }

    #[test]
    fn dot_in_qualified_column() {
        assert_eq!(
            lex("t.col"),
            vec![
                Token::Ident("t".into()),
                Token::Dot,
                Token::Ident("col".into()),
                Token::Eof,
            ]
        );
    }

    // --- v0.11 brackets + distance op + vector keyword --------------------

    #[test]
    fn brackets_are_distinct_tokens() {
        assert_eq!(
            lex("[ ]"),
            vec![Token::LBracket, Token::RBracket, Token::Eof]
        );
    }

    #[test]
    fn l2_distance_is_three_char_token() {
        assert_eq!(
            lex("a <-> b"),
            vec![
                Token::Ident("a".into()),
                Token::L2Distance,
                Token::Ident("b".into()),
                Token::Eof,
            ]
        );
        // Bare `<-` should NOT match L2Distance.
        assert_eq!(
            lex("a <- b"),
            vec![
                Token::Ident("a".into()),
                Token::Lt,
                Token::Minus,
                Token::Ident("b".into()),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn order_by_limit_are_keywords() {
        assert_eq!(
            lex("ORDER BY LIMIT"),
            vec![Token::Order, Token::By, Token::Limit, Token::Eof]
        );
    }

    // --- v1.2: pgvector distance ops + PG cast --------------------------

    #[test]
    fn inner_product_operator_3char() {
        assert_eq!(
            lex("a <#> b"),
            vec![
                Token::Ident("a".into()),
                Token::InnerProduct,
                Token::Ident("b".into()),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn cosine_distance_operator_3char() {
        assert_eq!(
            lex("a <=> b"),
            vec![
                Token::Ident("a".into()),
                Token::CosineDistance,
                Token::Ident("b".into()),
                Token::Eof,
            ]
        );
        // Make sure `<=` and `<>` and `<->` still lex right when `<=>` is
        // around (greedy match takes the longest).
        assert_eq!(
            lex("a <= b"),
            vec![
                Token::Ident("a".into()),
                Token::LtEq,
                Token::Ident("b".into()),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn double_colon_cast_token() {
        assert_eq!(
            lex("x::INT"),
            vec![
                Token::Ident("x".into()),
                Token::DoubleColon,
                Token::Ident("int".into()),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn lone_single_colon_is_unknown_char() {
        let err = tokenize(":x").unwrap_err();
        assert!(matches!(err.kind, LexErrorKind::UnknownChar(':')));
    }
}
