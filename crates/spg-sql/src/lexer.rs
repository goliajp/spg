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
    /// v7.14.0 — MySQL session / user variable reference
    /// (`@VAR` / `@@VAR`). The wrapped string is the verbatim
    /// source form (including the `@` / `@@` prefix). Used by
    /// mysqldump preamble (`SET @OLD_FOREIGN_KEY_CHECKS =
    /// @@FOREIGN_KEY_CHECKS, …`); SPG accepts the token and
    /// the SET parser treats the assignment as a no-op apart
    /// from any second LHS that targets a real session
    /// parameter (e.g. `FOREIGN_KEY_CHECKS=0`).
    SessionVar(String),

    // Literals
    Integer(i64),
    Float(f64),
    String(String),

    // Operators
    Plus,
    Minus,
    Star,
    Slash,
    /// v7.37.7 C.1.7 — PG `%` integer modulo operator (also short for
    /// `mod(y, x)`). MySQL accepts `MOD` keyword + `%`; SPG follows
    /// the PG form here. Token alone (no `%=` etc., kept simple).
    Percent,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    /// v7.17.0 Phase 3.P0-47 — PG INET / CIDR strict contained-in
    /// `<<`. LHS is strictly inside RHS (no equality).
    InetContainedBy,
    /// v7.17.0 Phase 3.P0-47 — PG INET / CIDR contained-in-or-equal
    /// `<<=`. LHS network ⊆ RHS network.
    InetContainedByEq,
    /// v7.17.0 Phase 3.P0-47 — PG INET / CIDR strict contains `>>`.
    /// LHS strictly contains RHS.
    InetContains,
    /// v7.17.0 Phase 3.P0-47 — PG INET / CIDR contains-or-equal `>>=`.
    /// LHS network ⊇ RHS network.
    InetContainsEq,
    /// v7.17.0 Phase 3.P0-47 — PG INET / CIDR network overlap `&&`.
    /// Either side contains any address of the other.
    InetOverlap,

    // Punctuation
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Semicolon,
    Dot,
    /// v7.37.20 (20.4) — `..` range operator, used by PL/pgSQL
    /// `FOR i IN 1..10 LOOP` bounds. Emitted by the lexer as a
    /// single token so parse_expr doesn't have to distinguish
    /// range-`.` from struct-field-`.`.
    DotDot,
    /// v7.17.0 Phase 2.6 — standalone `@` punctuation. Emitted when
    /// `@` is NOT followed by an ident-start byte (i.e. the
    /// `@VAR` / `@@VAR` SessionVar path doesn't match). Lets the
    /// parser stitch the MySQL `'user'@'host'` DEFINER form back
    /// together as String + At + String. Pre-2.6 this same shape
    /// surfaced as a `LexErrorKind::UnknownChar('@')` and broke
    /// every mysqldump CREATE VIEW with a DEFINER clause at lex
    /// time.
    At,
    /// pgvector L2 distance operator `<->`. Lexed as one token so the
    /// parser can give it its own precedence rung.
    /// v4.14 `->` — JSON object/array element access, returns json.
    JsonGet,
    /// v4.14 `->>` — same access, returns text.
    JsonGetText,
    /// v6.4.5 `#>` — JSON path walk, returns json. Path is the
    /// right-hand TEXT with PG `{a,b,0}` syntax.
    JsonGetPath,
    /// v6.4.5 `#>>` — same walk, returns text.
    JsonGetPathText,
    /// `#-` — delete the value at a nested JSON path. RHS is a PG
    /// text-array literal `{a,b}`.
    JsonDeletePath,
    /// v6.4.5 `@>` — JSON containment. `j @> sub` returns true if
    /// every key/value in `sub` is present in `j` with structural
    /// containment for objects + arrays.
    JsonContains,
    /// `@?` jsonpath existence operator.
    JsonPathExists,
    /// v7.37.6-A `<@` — JSON contained-by. `a <@ b` ⇔ `b @> a`.
    JsonContainedBy,
    /// v7.37.6-A `?` — JSON key exists (object), or element-as-text
    /// exists (array). `j ? 'key'` returns BOOL.
    JsonKeyExists,
    /// v7.37.6-A `?|` — JSON any-key-exists. `j ?| ARRAY['a','b']`
    /// returns BOOL; true if any one of the listed keys exists in `j`.
    JsonKeysAny,
    /// v7.37.6-A `?&` — JSON all-keys-exist. `j ?& ARRAY['a','b']`
    /// returns BOOL; true if every listed key exists in `j`.
    JsonKeysAll,
    /// v7.12.2 `@@` — tsvector / tsquery match. Either ordering
    /// (`vec @@ q` or `q @@ vec`) parses; engine eval normalises
    /// before matching.
    TsMatch,
    L2Distance,
    /// pgvector inner-product operator `<#>` (returns negative dot product
    /// so smaller still means more similar — same semantics as pgvector).
    InnerProduct,
    /// pgvector cosine distance operator `<=>`.
    CosineDistance,
    /// PG-style cast `expr::type` — single token because we want it to bind
    /// at postfix precedence.
    DoubleColon,
    /// v7.12.4 — PL/pgSQL assignment operator `:=`.
    /// Outside PL/pgSQL bodies this token has no SQL-side meaning.
    ColonEq,
    /// v7.12.4 — bare `:` separator. Used inside `tsvector` external-form
    /// literals (`'cat:1 dog:2'::tsvector`) and as the fallback path for
    /// the PL/pgSQL assignment lexer.
    Colon,
    /// Standard SQL string concatenation `||`.
    Concat,
    /// Bitwise OR `|` (single pipe — `||` lexes as Concat first).
    Pipe,
    /// Bitwise AND `&` (single amp — `&&` lexes as InetOverlap first).
    Amp,
    /// Bitwise NOT `~` (prefix); regex match in binary position.
    Tilde,
    /// Case-insensitive regex match `~*`.
    TildeStar,
    /// Negated regex match `!~`.
    NotTilde,
    /// Negated case-insensitive regex match `!~*`.
    NotTildeStar,
    /// LIKE operator `~~` (PG's operator form of `LIKE`).
    DoubleTilde,
    /// ILIKE operator `~~*` (case-insensitive LIKE).
    DoubleTildeStar,
    /// NOT LIKE operator `!~~`.
    NotDoubleTilde,
    /// NOT ILIKE operator `!~~*`.
    NotDoubleTildeStar,
    /// Power operator `^`.
    Caret,
    /// Starts-with operator `^@` (PG 11+).
    CaretAt,
    /// Integer XOR operator `#`.
    Hash,
    /// Range "is adjacent to" operator `-|-`.
    Adjacent,
    /// tsquery prefix negation operator `!!`.
    DoubleBang,
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
    Right,
    Full,
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
    /// v6.1.4 — `CONNECTION` keyword (for
    /// `CREATE SUBSCRIPTION … CONNECTION '<conn_str>' …`).
    Connection,
    /// v7.37.6-B(sentori Epic 2 P0)— `PARTITION` keyword. Drives
    /// both `CREATE TABLE p (…) PARTITION BY RANGE (key)` (declarative
    /// parent) and `CREATE TABLE c PARTITION OF p FOR VALUES FROM
    /// (a) TO (b) | DEFAULT` (child). `OF` / `MINVALUE` / `MAXVALUE`
    /// stay PG-context-sensitive identifiers — the parser matches them
    /// as case-insensitive `Token::Ident` strings off the back of this
    /// reserved keyword, mirroring how `INSERT … RETURNING` handles
    /// `RETURNING` without burning a global keyword slot.
    Partition,

    Eof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LexErrorKind {
    UnknownChar(char),
    UnterminatedString,
    UnterminatedQuotedIdent,
    UnterminatedBlockComment,
    BadNumber(String),
    InvalidUnicodeEscape,
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
            LexErrorKind::InvalidUnicodeEscape => {
                write!(f, "invalid Unicode escape at byte {}", self.pos)
            }
        }
    }
}

/// Tokenize `input` into a `Vec<Token>` ending in `Token::Eof`,
/// with PG string semantics (backslash is a literal byte inside
/// `'…'`; `''` is the only escape).
pub fn tokenize(input: &str) -> Result<Vec<Token>, LexError> {
    tokenize_with(input, false)
}

/// v7.22 (round-13 T3) — dialect-aware tokenizer entry. With
/// `backslash_escapes = true`, plain `'…'` strings honour MySQL /
/// pre-9.1-PG backslash escapes (`\'` `\\` `\n` …, the same decode
/// the `E'…'` form uses). mysqldump ALWAYS emits `\'`-escaped data
/// sections, and pg_dump ALWAYS announces PG semantics via
/// `SET standard_conforming_strings = on` — the engine flips this
/// flag off/on from those deterministic session signals.
#[allow(clippy::too_many_lines)] // big match — splitting would obscure the dispatch table
pub fn tokenize_with(input: &str, backslash_escapes: bool) -> Result<Vec<Token>, LexError> {
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
                // v7.14.0 — MySQL versioned conditional comment
                // `/*!NNNNN <body> */`. The body is real SQL that
                // MySQL/MariaDB executes when the runtime version
                // matches the 5-digit code; PG strips the whole
                // thing as a block comment. SPG sides with MySQL
                // semantics for dump compatibility: skip the
                // `/*!NNNNN ` prefix and continue lexing the body
                // as ordinary tokens. The closing `*/` is later
                // matched + skipped by the symmetric arm below.
                if peek_eq(bytes, i + 2, b'!') {
                    let mut j = i + 3;
                    // skip the optional 5-digit version code +
                    // following single whitespace
                    while j < bytes.len() && bytes[j].is_ascii_digit() {
                        j += 1;
                    }
                    if j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                        j += 1;
                    }
                    i = j;
                    continue;
                }
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
            // v7.14.0 — bare `*/` (closing of the v7.14 MySQL
            // versioned-comment opener that didn't consume the
            // closer). We treat it as an inline comment terminator
            // and skip 2 bytes.
            b'*' if peek_eq(bytes, i + 1, b'/') => {
                i += 2;
            }
            b'\'' => {
                let (tok, consumed) = if backslash_escapes {
                    // MySQL-dialect session: plain strings decode
                    // backslash escapes — same machinery as E'…'.
                    lex_escape_string(input, i)?
                } else {
                    lex_quoted(input, i, b'\'', false)?
                };
                out.push(tok);
                i += consumed;
            }
            // v7.18 — PG escape-string literal `E'...'` / `e'...'`.
            // Closes the mailrs D-pre #3 reverse-acceptance gap:
            // `INSERT INTO oq VALUES (E'\\xdeadbeef'::bytea)` needs
            // the `E` prefix so `\\` decodes to a single `\`. The
            // produced Token::String carries the decoded body so
            // downstream parser / cast paths treat it identically
            // to a regular string literal.
            b'E' | b'e' if peek_eq(bytes, i + 1, b'\'') => {
                let (tok, consumed) = lex_escape_string(input, i + 1)?;
                out.push(tok);
                i += 1 + consumed;
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
            // v7.37.6-A — PG JSONB `?` / `?|` / `?&`. Longest-match
            // order matters: try `?|` and `?&` before bare `?`.
            // SPG doesn't use `?` as a placeholder (uses `$N`
            // instead), so the bare `?` slot is free for JSONB.
            b'?' if peek_eq(bytes, i + 1, b'|') => {
                out.push(Token::JsonKeysAny);
                i += 2;
            }
            b'?' if peek_eq(bytes, i + 1, b'&') => {
                out.push(Token::JsonKeysAll);
                i += 2;
            }
            b'?' => single(&mut out, Token::JsonKeyExists, &mut i),
            b'-' => {
                // Range `-|-` "is adjacent to" — longest match ahead of `->`.
                if peek_eq(bytes, i + 1, b'|') && peek_eq(bytes, i + 2, b'-') {
                    out.push(Token::Adjacent);
                    i += 3;
                }
                // v4.14: `->>` and `->` for JSON path access. `->>`
                // must be tried before `->` (longest match).
                else if peek_eq(bytes, i + 1, b'>') && peek_eq(bytes, i + 2, b'>') {
                    out.push(Token::JsonGetText);
                    i += 3;
                } else if peek_eq(bytes, i + 1, b'>') {
                    out.push(Token::JsonGet);
                    i += 2;
                } else {
                    single(&mut out, Token::Minus, &mut i);
                }
            }
            // v6.4.5: `#>>` and `#>` JSON path walk; bare `#` is
            // the integer XOR operator.
            b'#' => {
                if peek_eq(bytes, i + 1, b'>') && peek_eq(bytes, i + 2, b'>') {
                    out.push(Token::JsonGetPathText);
                    i += 3;
                } else if peek_eq(bytes, i + 1, b'>') {
                    out.push(Token::JsonGetPath);
                    i += 2;
                } else if peek_eq(bytes, i + 1, b'-') {
                    out.push(Token::JsonDeletePath);
                    i += 2;
                } else {
                    single(&mut out, Token::Hash, &mut i);
                }
            }
            // v6.4.5: `@>` JSON containment.
            // v7.12.2: `@@` tsvector / tsquery match.
            // v7.14.0: `@@NAME` MySQL session variable ref +
            //          `@NAME` user variable ref. mysqldump preamble
            //          uses both heavily (`SET @OLD_FOREIGN_KEY_CHECKS
            //          = @@FOREIGN_KEY_CHECKS, FOREIGN_KEY_CHECKS=0`).
            //          We lex both as a single SessionVar token so
            //          the parser can accept and ignore them.
            b'@' => {
                if peek_eq(bytes, i + 1, b'>') {
                    out.push(Token::JsonContains);
                    i += 2;
                } else if peek_eq(bytes, i + 1, b'?') {
                    // v7.37 — `@?` jsonpath existence operator
                    // (`jsonb @? jsonpath` = jsonb_path_exists).
                    out.push(Token::JsonPathExists);
                    i += 2;
                } else if peek_eq(bytes, i + 1, b'@')
                    && !is_session_var_ident_start(bytes.get(i + 2).copied())
                {
                    // `@@` not followed by an ident-start byte is
                    // the tsquery `@@` operator.
                    out.push(Token::TsMatch);
                    i += 2;
                } else {
                    // `@VAR` / `@@VAR` — MySQL user / session
                    // variable reference. Consume the ident-shaped
                    // tail and emit as Token::SessionVar so the
                    // SET parser can accept-and-ignore.
                    let prefix_end = if peek_eq(bytes, i + 1, b'@') {
                        i + 2
                    } else {
                        i + 1
                    };
                    let mut end = prefix_end;
                    while end < bytes.len() && is_session_var_ident_continue(bytes[end]) {
                        end += 1;
                    }
                    if end == prefix_end {
                        // v7.17.0 Phase 2.6 — `@` not followed by an
                        // ident-shaped tail. mysqldump's DEFINER
                        // form `'user'@'host'` lands here (next
                        // byte is `'`). Emit as Token::At so the
                        // parser can stitch the surrounding String
                        // tokens. Single `@@` already short-circuits
                        // to Token::TsMatch above, so this only
                        // fires for a true lone `@`.
                        out.push(Token::At);
                        i = prefix_end;
                        continue;
                    }
                    out.push(Token::SessionVar(input[i..end].to_string()));
                    i = end;
                }
            }
            b'*' => single(&mut out, Token::Star, &mut i),
            b'/' => single(&mut out, Token::Slash, &mut i),
            b'%' => single(&mut out, Token::Percent, &mut i),
            b'(' => single(&mut out, Token::LParen, &mut i),
            b')' => single(&mut out, Token::RParen, &mut i),
            b'[' => single(&mut out, Token::LBracket, &mut i),
            b']' => single(&mut out, Token::RBracket, &mut i),
            b',' => single(&mut out, Token::Comma, &mut i),
            b';' => single(&mut out, Token::Semicolon, &mut i),
            b'.' => {
                // v7.37.20 (20.4) — `..` range operator for PL/pgSQL
                // FOR LOOP bounds emits a single Token::DotDot so the
                // range parser sees one atomic token instead of two
                // consecutive Dots (which parse_expr couldn't reliably
                // distinguish from struct-field access after an atom).
                if peek_eq(bytes, i + 1, b'.') {
                    out.push(Token::DotDot);
                    i += 2;
                } else {
                    single(&mut out, Token::Dot, &mut i);
                }
            }
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
                } else if peek_eq(bytes, i + 1, b'<') && peek_eq(bytes, i + 2, b'=') {
                    // v7.17.0 Phase 3.P0-47 — PG INET `<<=` contained-or-equal.
                    out.push(Token::InetContainedByEq);
                    i += 3;
                } else if peek_eq(bytes, i + 1, b'<') {
                    // v7.17.0 Phase 3.P0-47 — PG INET `<<` strict contained.
                    out.push(Token::InetContainedBy);
                    i += 2;
                } else if peek_eq(bytes, i + 1, b'@') {
                    // v7.37.6-A — PG JSONB `<@` contained-by.
                    out.push(Token::JsonContainedBy);
                    i += 2;
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
            b':' if peek_eq(bytes, i + 1, b'=') => {
                // v7.12.4 — PL/pgSQL assignment operator `:=`.
                out.push(Token::ColonEq);
                i += 2;
            }
            b':' => {
                // v7.12.4 — bare `:`. Used inside `tsvector` external-form
                // literals which the cast parser consumes in-token, and as a
                // separator the PL/pgSQL assignment lexer can recover from.
                out.push(Token::Colon);
                i += 1;
            }
            b'|' if peek_eq(bytes, i + 1, b'|') => {
                out.push(Token::Concat);
                i += 2;
            }
            // Bitwise operators (PG integer ops; mailrs IMAP flag
            // masks: `flags | $1`, `flags & ~$1`).
            b'|' => {
                single(&mut out, Token::Pipe, &mut i);
            }
            // `~~*` (ILIKE) / `~~` (LIKE) — check the double-tilde forms before
            // `~*` and single `~` so PG's operator spellings of LIKE/ILIKE parse.
            b'~' if peek_eq(bytes, i + 1, b'~') && peek_eq(bytes, i + 2, b'*') => {
                out.push(Token::DoubleTildeStar);
                i += 3;
            }
            b'~' if peek_eq(bytes, i + 1, b'~') => {
                out.push(Token::DoubleTilde);
                i += 2;
            }
            b'~' if peek_eq(bytes, i + 1, b'*') => {
                out.push(Token::TildeStar);
                i += 2;
            }
            b'~' => {
                single(&mut out, Token::Tilde, &mut i);
            }
            b'^' if peek_eq(bytes, i + 1, b'@') => {
                out.push(Token::CaretAt);
                i += 2;
            }
            b'^' => {
                single(&mut out, Token::Caret, &mut i);
            }
            b'>' => {
                if peek_eq(bytes, i + 1, b'>') && peek_eq(bytes, i + 2, b'=') {
                    // v7.17.0 Phase 3.P0-47 — PG INET `>>=` contains-or-equal.
                    out.push(Token::InetContainsEq);
                    i += 3;
                } else if peek_eq(bytes, i + 1, b'>') {
                    // v7.17.0 Phase 3.P0-47 — PG INET `>>` strict contains.
                    out.push(Token::InetContains);
                    i += 2;
                } else if peek_eq(bytes, i + 1, b'=') {
                    out.push(Token::GtEq);
                    i += 2;
                } else {
                    out.push(Token::Gt);
                    i += 1;
                }
            }
            b'&' if peek_eq(bytes, i + 1, b'&') => {
                // v7.17.0 Phase 3.P0-47 — PG INET network overlap `&&`.
                out.push(Token::InetOverlap);
                i += 2;
            }
            b'&' => {
                single(&mut out, Token::Amp, &mut i);
            }
            b'!' if peek_eq(bytes, i + 1, b'!') => {
                // tsquery `!!` prefix negation. Two bangs, ahead of `!=`/`!~`.
                out.push(Token::DoubleBang);
                i += 2;
            }
            b'!' if peek_eq(bytes, i + 1, b'=') => {
                out.push(Token::NotEq);
                i += 2;
            }
            // `!~~*` (NOT ILIKE) / `!~~` (NOT LIKE) — before `!~*` / `!~`.
            b'!' if peek_eq(bytes, i + 1, b'~')
                && peek_eq(bytes, i + 2, b'~')
                && peek_eq(bytes, i + 3, b'*') =>
            {
                out.push(Token::NotDoubleTildeStar);
                i += 4;
            }
            b'!' if peek_eq(bytes, i + 1, b'~') && peek_eq(bytes, i + 2, b'~') => {
                out.push(Token::NotDoubleTilde);
                i += 3;
            }
            b'!' if peek_eq(bytes, i + 1, b'~') && peek_eq(bytes, i + 2, b'*') => {
                out.push(Token::NotTildeStar);
                i += 3;
            }
            b'!' if peek_eq(bytes, i + 1, b'~') => {
                out.push(Token::NotTilde);
                i += 2;
            }
            // v7.9.27 — PG dollar-quoted string `$$ … $$` (or
            // `$tag$ … $tag$`). Used in `DO $$ … $$ LANGUAGE
            // plpgsql;` blocks that pg_dump emits for idempotent
            // migrations. SPG has no PL/pgSQL, so the lexer
            // consumes the entire string as a single Token::String
            // and the parser treats the surrounding `DO …;` as a
            // no-op. mailrs follow-up H1.
            b'$' if i + 1 < bytes.len() && bytes[i + 1] == b'$' => {
                // Empty tag form: `$$ … $$`.
                let end = find_dollar_tag_end(bytes, i + 2, b"$$");
                let body = match end {
                    Some(e) => &input[i + 2..e],
                    None => {
                        return Err(LexError {
                            kind: LexErrorKind::UnterminatedString,
                            pos: i,
                        });
                    }
                };
                out.push(Token::String(body.to_string()));
                i = end.unwrap() + 2;
            }
            b'$' if i + 1 < bytes.len()
                && (bytes[i + 1].is_ascii_alphabetic() || bytes[i + 1] == b'_') =>
            {
                // Tagged form: `$foo$ … $foo$`. Scan the tag
                // ident, find the closing copy.
                let mut j = i + 1;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                if j >= bytes.len() || bytes[j] != b'$' {
                    // Not a dollar-quoted string — fall through
                    // to the generic-unknown-char path.
                    let ch = input[i..].chars().next().unwrap_or('?');
                    return Err(LexError {
                        kind: LexErrorKind::UnknownChar(ch),
                        pos: i,
                    });
                }
                let close: alloc::vec::Vec<u8> = bytes[i..=j].to_vec();
                let end = find_dollar_tag_end(bytes, j + 1, &close);
                let body = match end {
                    Some(e) => &input[j + 1..e],
                    None => {
                        return Err(LexError {
                            kind: LexErrorKind::UnterminatedString,
                            pos: i,
                        });
                    }
                };
                out.push(Token::String(body.to_string()));
                i = end.unwrap() + close.len();
            }
            // v6.1.1: `$N` parameter placeholder for the extended
            // query protocol. PG numbers them 1..=N; we reject $0
            // and a bare `$` not followed by a digit.
            b'$' if i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() => {
                let mut j = i + 1;
                let mut n: u32 = 0;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    n = n
                        .saturating_mul(10)
                        .saturating_add(u32::from(bytes[j] - b'0'));
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

/// v7.14.0 — recognise the first byte of a MySQL session/user
/// variable name (after `@` or `@@`). PG-strict idents are ASCII
/// letter or underscore; MySQL also allows leading digits inside
/// quoted names but unquoted vars match the same shape.
fn is_session_var_ident_start(b: Option<u8>) -> bool {
    matches!(b, Some(c) if c.is_ascii_alphabetic() || c == b'_')
}

/// Continuation byte for a `@VAR`/`@@VAR` ident (after the first
/// alphabet/underscore byte). Letters, digits, underscore, dot
/// (MySQL allows session-scope qualifiers like
/// `@@global.sql_mode`) and `$` (some MySQL versions accept it).
fn is_session_var_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'$'
}

/// v7.9.27 — find the start index of the next occurrence of `tag`
/// (e.g. `b"$$"` or `b"$foo$"`) in `bytes` starting at `from`.
fn find_dollar_tag_end(bytes: &[u8], from: usize, tag: &[u8]) -> Option<usize> {
    if tag.is_empty() || from > bytes.len() {
        return None;
    }
    let mut i = from;
    while i + tag.len() <= bytes.len() {
        if &bytes[i..i + tag.len()] == tag {
            return Some(i);
        }
        i += 1;
    }
    None
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
        10 => kw_len10(b),
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
    if eq_ci(b, b"full") {
        return Some(Token::Full);
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
    if eq_ci(b, b"right") {
        return Some(Token::Right);
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
    // 2 keywords: savepoint, partition
    if eq_ci(b, b"savepoint") {
        return Some(Token::Savepoint);
    }
    if eq_ci(b, b"partition") {
        return Some(Token::Partition);
    }
    None
}

#[inline]
fn kw_len10(b: &[u8]) -> Option<Token> {
    // 1 keyword: connection
    if eq_ci(b, b"connection") {
        return Some(Token::Connection);
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

/// v7.18 — Lex a PG escape-string literal `E'...'`. `start` points
/// at the opening single quote (the `E` was matched by the caller
/// and is NOT part of `start`'s offset semantics — the consumed
/// count returned excludes the `E`, which the caller adds).
///
/// Recognised escape sequences:
///   \\ \' \" — literal backslash / quote
///   \n \r \t \b \f — standard whitespace controls
///   \0 — NUL
///   \xHH — single hex byte (1–2 hex digits)
///   \NNN — octal byte (1–3 octal digits)
/// Any other `\X` decodes to the literal byte `X` (PG warns; SPG
/// follows the lenient behaviour pg_dump output relies on).
///
/// Doubled `''` is still a literal `'` (same as the non-E form).
fn lex_escape_string(input: &str, start: usize) -> Result<(Token, usize), LexError> {
    let bytes = input.as_bytes();
    debug_assert_eq!(bytes[start], b'\'');
    let mut i = start + 1;
    let mut s = String::new();
    loop {
        if i >= bytes.len() {
            return Err(LexError {
                kind: LexErrorKind::UnterminatedString,
                pos: start,
            });
        }
        let b = bytes[i];
        if b == b'\'' {
            if peek_eq(bytes, i + 1, b'\'') {
                s.push('\'');
                i += 2;
                continue;
            }
            i += 1;
            break;
        }
        if b == b'\\' && i + 1 < bytes.len() {
            let n = bytes[i + 1];
            match n {
                b'\\' => {
                    s.push('\\');
                    i += 2;
                }
                b'\'' => {
                    s.push('\'');
                    i += 2;
                }
                b'"' => {
                    s.push('"');
                    i += 2;
                }
                b'n' => {
                    s.push('\n');
                    i += 2;
                }
                b'r' => {
                    s.push('\r');
                    i += 2;
                }
                b't' => {
                    s.push('\t');
                    i += 2;
                }
                b'b' => {
                    s.push('\u{0008}');
                    i += 2;
                }
                b'f' => {
                    s.push('\u{000C}');
                    i += 2;
                }
                b'v' => {
                    s.push('\u{000B}');
                    i += 2;
                }
                // \uHHHH (4 hex) / \UHHHHHHHH (8 hex) Unicode escapes. A
                // `\u` high surrogate combines with a following `\uLLLL`
                // low surrogate (PG's `😀` → emoji); a lone
                // surrogate or short/invalid hex run is an error.
                b'u' | b'U' => {
                    let is_u = bytes[i + 1] == b'u';
                    let ndigits = if is_u { 4 } else { 8 };
                    let Some(cp) = read_hex_run(bytes, i + 2, ndigits) else {
                        return Err(LexError {
                            kind: LexErrorKind::InvalidUnicodeEscape,
                            pos: i,
                        });
                    };
                    if is_u && (0xD800..=0xDBFF).contains(&cp) {
                        let lo = (bytes.get(i + 6) == Some(&b'\\')
                            && bytes.get(i + 7) == Some(&b'u'))
                        .then(|| read_hex_run(bytes, i + 8, 4))
                        .flatten()
                        .filter(|l| (0xDC00..=0xDFFF).contains(l));
                        let Some(lo) = lo else {
                            return Err(LexError {
                                kind: LexErrorKind::InvalidUnicodeEscape,
                                pos: i,
                            });
                        };
                        let combined = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                        s.push(char::from_u32(combined).ok_or(LexError {
                            kind: LexErrorKind::InvalidUnicodeEscape,
                            pos: i,
                        })?);
                        i += 12;
                    } else {
                        s.push(char::from_u32(cp).ok_or(LexError {
                            kind: LexErrorKind::InvalidUnicodeEscape,
                            pos: i,
                        })?);
                        i += 2 + ndigits;
                    }
                }
                b'0' if i + 2 >= bytes.len() || !bytes[i + 2].is_ascii_digit() => {
                    s.push('\0');
                    i += 2;
                }
                b'x' => {
                    // \xH or \xHH — single byte by hex.
                    let h1 = bytes.get(i + 2).copied();
                    let h2 = bytes.get(i + 3).copied();
                    let n1 = h1.and_then(hex_digit_value);
                    let n2 = h2.and_then(hex_digit_value);
                    match (n1, n2) {
                        (Some(a), Some(b2)) => {
                            s.push((((a << 4) | b2) as u8) as char);
                            i += 4;
                        }
                        (Some(a), _) => {
                            s.push((a as u8) as char);
                            i += 3;
                        }
                        _ => {
                            // \x with no hex follows — literal x.
                            s.push('x');
                            i += 2;
                        }
                    }
                }
                d if d.is_ascii_digit() && d < b'8' => {
                    // \NNN octal — up to 3 octal digits.
                    let mut value: u32 = u32::from(d - b'0');
                    let mut take = 2;
                    while take < 4 {
                        let next = bytes.get(i + take).copied();
                        match next {
                            Some(c) if c.is_ascii_digit() && c < b'8' => {
                                value = (value << 3) | u32::from(c - b'0');
                                take += 1;
                            }
                            _ => break,
                        }
                    }
                    if let Some(c) = char::from_u32(value) {
                        s.push(c);
                    } else {
                        // Invalid Unicode — preserve as raw byte char.
                        s.push((value & 0xFF) as u8 as char);
                    }
                    i += take;
                }
                other => {
                    // Lenient fallback — same as PG with
                    // `standard_conforming_strings = off` warning:
                    // decode `\X` to literal `X`.
                    s.push(other as char);
                    i += 2;
                }
            }
        } else {
            let ch = input[i..].chars().next().expect("non-empty UTF-8 boundary");
            s.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok((Token::String(s), i - start))
}

/// Read exactly `n` hex digits starting at `start`, returning their value
/// (or `None` if fewer than `n` hex digits are present).
fn read_hex_run(bytes: &[u8], start: usize, n: usize) -> Option<u32> {
    let mut v = 0u32;
    for k in 0..n {
        v = (v << 4) | hex_digit_value(*bytes.get(start + k)?)?;
    }
    Some(v)
}

fn hex_digit_value(b: u8) -> Option<u32> {
    match b {
        b'0'..=b'9' => Some(u32::from(b - b'0')),
        b'a'..=b'f' => Some(u32::from(b - b'a' + 10)),
        b'A'..=b'F' => Some(u32::from(b - b'A' + 10)),
        _ => None,
    }
}

fn lex_number(s: &str) -> Result<(Token, usize), LexErrorKind> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    let mut is_float = false;

    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    // v7.37.20 (20.4) — do NOT consume `.` when it's part of a `..`
    // range operator; leave both dots for the top-level dispatcher
    // which will emit a single Token::DotDot.
    if i < bytes.len()
        && bytes[i] == b'.'
        && !(i + 1 < bytes.len() && bytes[i + 1] == b'.')
    {
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
            lex("= <> != < <= > >= + - * / %"),
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
                Token::Percent,
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
        // v7.17.0 Phase 2.6 — `@` standalone now lexes as
        // Token::At (mysqldump `'user'@'host'` DEFINER stitching).
        // Use `?` for the unknown-char regression; PG `?` operator
        // family is parsed as JSON ops in the prefix `?` shape
        // would land in lex paths; bare `?` is unknown.
        let err = tokenize("\x07").unwrap_err();
        assert!(matches!(err.kind, LexErrorKind::UnknownChar(_)));
    }

    #[test]
    fn at_alone_lexes_as_punctuation() {
        // v7.17.0 Phase 2.6 — the `'user'@'host'` MySQL DEFINER
        // form needs `@` to lex as a standalone token.
        assert_eq!(
            lex("'u'@'h'"),
            vec![
                Token::String("u".into()),
                Token::At,
                Token::String("h".into()),
                Token::Eof,
            ]
        );
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
    fn lone_single_colon_lexes_as_colon_token() {
        // v7.12.4 — single `:` is now a token (PL/pgSQL surface
        // + tsvector external-form literal both need it). The
        // pre-v7.12.4 "single colon = unknown char" behaviour
        // was incidental.
        let toks = tokenize(":x").expect("colon now lexes");
        assert_eq!(toks[0], Token::Colon);
    }

    #[test]
    fn colon_eq_lexes_as_assignment() {
        // v7.12.4 — PL/pgSQL assignment operator.
        let toks = tokenize("x := 1").expect("colon-eq lexes");
        // Tokens: Ident("x"), ColonEq, NumberLiteral
        assert!(matches!(toks[1], Token::ColonEq));
    }

    #[test]
    fn pg_escape_string_double_backslash_decodes_to_single() {
        // v7.18 — E'\\xdeadbeef' decodes to literal `\xdeadbeef`
        // (10 chars: backslash + xdeadbeef). The downstream
        // `::bytea` cast then reads that as the PG hex-form bytea
        // literal. mailrs D-pre #3.
        let toks = tokenize(r"E'\\xdeadbeef'").expect("E-string lexes");
        assert_eq!(toks, vec![Token::String(r"\xdeadbeef".into()), Token::Eof]);
    }

    #[test]
    fn pg_escape_string_supports_basic_escapes() {
        // \n / \t / \' / \\ — the PG standard set.
        let toks = tokenize(r"E'a\nb\tc\'d\\e'").expect("E-string lexes");
        assert_eq!(toks, vec![Token::String("a\nb\tc'd\\e".into()), Token::Eof]);
    }

    #[test]
    fn pg_escape_string_hex_byte() {
        // \xHH single byte. \x41 = 'A'.
        let toks = tokenize(r"E'\x41B\x42'").expect("E-string lexes");
        assert_eq!(toks, vec![Token::String("ABB".into()), Token::Eof]);
    }

    #[test]
    fn pg_escape_string_lowercase_e_prefix() {
        let toks = tokenize(r"e'hi\n'").expect("e-string lexes");
        assert_eq!(toks, vec![Token::String("hi\n".into()), Token::Eof]);
    }

    #[test]
    fn pg_escape_string_doubled_quote() {
        // Even in E-string the doubled '' is a literal '.
        let toks = tokenize(r"E'it''s ok'").expect("E-string lexes");
        assert_eq!(toks, vec![Token::String("it's ok".into()), Token::Eof]);
    }
}
