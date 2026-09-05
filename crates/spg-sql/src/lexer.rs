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
    // v7.38 (read01) — exact decimal literal (`1.5`, `0.1`) and any integer
    // literal too large for i64. PG types a dotted literal as NUMERIC (not
    // double) and an over-i64 integer as NUMERIC; the exact source text is
    // carried so no precision is lost before it becomes a Value::Numeric.
    Numeric(String),
    String(String),
    /// v7.39 (round 367, M20) — a MySQL `0x…` hexadecimal literal in the
    /// MySQL dialect: a BINARY STRING, not an integer. Carries the raw hex
    /// digits (parser decodes, left-padding an odd count). The PG dialect
    /// never emits this — there `0x…` stays a radix-16 `Integer`.
    HexBytes(String),

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
    /// v7.39 — range `&<` / `&>`.
    OverLeft,
    OverRight,

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
    /// v7.39 (round 353, M10) — MySQL's `!` (logical negation). Its own
    /// token because its precedence is nothing like `NOT`'s.
    Bang,
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
    /// v7.39 (read01 geo_ops.c) — `?||` "is parallel" (lseg / line).
    GeomParallel,
    /// v7.39 (read01 geo_ops.c) — `?-|` "is perpendicular" (lseg / line).
    GeomPerp,
    /// v7.39 (read01 geo_ops.c) — `~=` "same as" (geometric equality).
    GeomSameAs,
    /// v7.39 (read01 geo_ops.c) — `##` closest point.
    ClosestPoint,
    /// v7.39 (read01 geo_ops.c) — `?-` "is horizontal" (binary points /
    /// prefix lseg-line).
    GeomHoriz,
    /// v7.37.6-A `?&` — JSON all-keys-exist. `j ?& ARRAY['a','b']`
    /// returns BOOL; true if every listed key exists in `j`.
    JsonKeysAll,
    /// v7.12.2 `@@` — tsvector / tsquery match. Either ordering
    /// (`vec @@ q` or `q @@ vec`) parses; engine eval normalises
    /// before matching.
    TsMatch,
    /// v7.39 (round 508) — `@@@`, PG's deprecated spelling of `@@`. Kept
    /// because `pg_operator` still carries it and old application SQL still
    /// writes it.
    TsMatchOld,
    /// v7.39 (round 508) — `@-@`, "length of" (lseg, path).
    AtMinusAt,
    /// v7.39 (round 508) — `?#`, "do these intersect" (box / line / lseg /
    /// path, in every combination PG defines).
    Intersects,
    /// v7.39 (round 508) — `<^` "is strictly below" and `>^` "is strictly
    /// above" (point, box).
    IsBelow,
    IsAbove,
    /// v7.39 (round 508) — the `text_pattern_ops` comparisons `~<~`, `~<=~`,
    /// `~>~`, `~>=~`. They compare BYTES, ignoring collation, which is what
    /// makes them index-usable for LIKE prefixes: `'A' ~<~ 'a'` is true
    /// where `'A' < 'a'` is false under a non-C collation. pg_dump emits
    /// them, so a dump of an ordinary database would not restore.
    PatternLt,
    PatternLtEq,
    PatternGt,
    PatternGtEq,
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
    /// v7.38 (read01, T14) — `=>` names a function argument
    /// (`make_date(year => 2024, …)`).
    FatArrow,
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
    /// v7.39 (round 773, F31 J3) — an E-string's byte escapes decoded
    /// to an invalid UTF-8 sequence. PG decodes `\NNN` / `\xHH` as
    /// BYTES and validates the whole literal (`E'\303\251'` is `é`;
    /// `E'\777'` is byte 0xFF and refuses); the old decoder mapped
    /// each byte to its Latin-1 codepoint, silently mangling every
    /// multi-byte sequence.
    InvalidByteSequence(u8),
    UnknownChar(char),
    UnterminatedString,
    UnterminatedQuotedIdent,
    UnterminatedBlockComment,
    BadNumber(String),
    /// v7.39 (round 184) — a numeric literal followed directly by an
    /// identifier character (`12__34`, `123_`, `1.5_`, `123abc`). PG
    /// rejects at scan time; pre-r184 SPG silently lexed the number
    /// and let the tail become a column alias (`SELECT 12__34` → 12).
    TrailingJunkAfterNumber(String),
    /// v7.39 (round 184) — a radix prefix with no digits (`0x`, `0o`,
    /// `0b`); pre-r184 the `0` lexed alone and the letter aliased.
    /// Payload: (radix-name, literal-text).
    InvalidRadixLiteral(&'static str, String),
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
            LexErrorKind::InvalidByteSequence(b) => {
                write!(f, "invalid byte sequence for encoding \"UTF8\": 0x{b:02x}")
            }
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
            LexErrorKind::TrailingJunkAfterNumber(s) => {
                write!(f, "trailing junk after numeric literal at or near \"{s}\"")
            }
            LexErrorKind::InvalidRadixLiteral(radix, s) => {
                write!(f, "invalid {radix} integer at or near \"{s}\"")
            }
            LexErrorKind::InvalidUnicodeEscape => {
                write!(f, "invalid Unicode escape at byte {}", self.pos)
            }
        }
    }
}

/// r1038 — whether the text between two string literals makes them ONE
/// literal: SQL's implicit concatenation.
///
/// PG18.4, measured rather than read — every one of these was run:
///
/// ```text
/// 'a' 'b'            same line              error
/// 'a'\n'b'                                  ab
/// 'a' -- c\n'b'      line comment           ab
/// 'a' /* c */ 'b'    block comment          error
/// 'a' /* c\n*/ 'b'   block comment, newline error   <- the newline in a
///                                                      block comment does
///                                                      NOT count
/// ```
///
/// So: whitespace and line comments only, with a real newline among them.
/// The newline is what tells a continued literal from two arguments
/// someone forgot a comma between, which is why `'a' 'b'` must stay an
/// error.
fn gap_continues_a_literal(gap: &str, speaks_mysql: bool) -> bool {
    let mut newline = false;
    let mut rest = gap;
    loop {
        let trimmed = rest.trim_start_matches(|c: char| {
            if c == '\n' {
                newline = true;
            }
            c.is_whitespace()
        });
        match trimmed.strip_prefix("--") {
            // A line comment runs to the newline that ends it, and that
            // newline is the one PG counts.
            Some(after) => match after.split_once('\n') {
                Some((_, tail)) => {
                    newline = true;
                    rest = tail;
                }
                // Unterminated: nothing follows it, so nothing to join.
                None => return false,
            },
            // v7.39.2 — MySQL does not want the newline. Measured on
            // 9.7.2, `SELECT 'a' 'b'` on ONE line answers `ab`, where
            // PostgreSQL 18.6 answers `syntax error at or near "'b'"`
            // and only joins them across a line break. Requiring the
            // newline on both sides made ordinary MySQL SQL a syntax
            // error, and it is also what the string-ALIAS rule needs
            // decided first: without this, `'a' 'b'` would look like a
            // literal aliased `b`.
            None => return trimmed.is_empty() && (newline || speaks_mysql),
        }
    }
}

/// Tokenize `input` into a `Vec<Token>` ending in `Token::Eof`,
/// with PG string semantics (backslash is a literal byte inside
/// `'…'`; `''` is the only escape).
pub fn tokenize(input: &str) -> Result<Vec<Token>, LexError> {
    tokenize_with(input, Dialect::PG)
}

/// v7.22 (round-13 T3) — dialect-aware tokenizer entry. With
/// `backslash_escapes = true`, plain `'…'` strings honour MySQL /
/// pre-9.1-PG backslash escapes (`\'` `\\` `\n` …, the same decode
/// the `E'…'` form uses). mysqldump ALWAYS emits `\'`-escaped data
/// sections, and pg_dump ALWAYS announces PG semantics via
/// `SET standard_conforming_strings = on` — the engine flips this
/// flag off/on from those deterministic session signals.
/// How a statement's text is to be read.
///
/// v7.39 — this was a lone `bool`. The second axis is `ANSI_QUOTES`,
/// which SPG behaved as though were always on: measured on MySQL 9.7.2,
/// `SELECT "abc"` answers `abc`, while a MySQL session on SPG answered
/// `ERROR 1054 column "abc" does not exist`. Ordinary MySQL SQL that
/// quotes a string with `"` — which a great deal of it does — failed
/// with an error naming a column the author never wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dialect {
    /// `\` escapes inside a string and `#` starts a comment: MySQL and
    /// MariaDB, unless the session's `sql_mode` says
    /// `NO_BACKSLASH_ESCAPES`.
    pub backslash_escapes: bool,
    /// `"…"` quotes an IDENTIFIER rather than a string literal.
    ///
    /// PostgreSQL, always. MySQL only when `ANSI_QUOTES` is in
    /// `sql_mode`, which its default list does not carry.
    pub double_quoted_identifiers: bool,
    /// v7.39.2 — does the session speak MySQL at all?
    ///
    /// The parser asked `backslash_escapes` this, which is a different
    /// question: `SET sql_mode='NO_BACKSLASH_ESCAPES'` turned the
    /// escapes off and took the whole grammar with it, so `7 DIV 2`
    /// stopped parsing and `'a' || 'b'` went back to concatenating
    /// where MySQL 9.7.2 answers 0.
    pub speaks_mysql: bool,
}

impl Dialect {
    /// PostgreSQL: no backslash escapes, `"…"` is an identifier.
    pub const PG: Self = Self {
        backslash_escapes: false,
        double_quoted_identifiers: true,
        speaks_mysql: false,
    };
}

impl Default for Dialect {
    fn default() -> Self {
        Self::PG
    }
}

pub fn tokenize_with(input: &str, dialect: Dialect) -> Result<Vec<Token>, LexError> {
    tokenize_with_offsets(input, dialect).map(|(tokens, _)| tokens)
}

/// v7.39 (read01 round 95) — like [`tokenize_with`] but also returns, for each
/// token, the byte offset in `input` where it started (the `Eof` token maps to
/// `input.len()`). The parser uses this to translate a failing token index into
/// PG's 1-based character error position (the ErrorResponse `P` field that psql
/// renders as `LINE n: … ^`).
#[allow(clippy::too_many_lines)] // big match — splitting would obscure the dispatch table
pub fn tokenize_with_offsets(
    input: &str,
    dialect: Dialect,
) -> Result<(Vec<Token>, Vec<usize>), LexError> {
    tokenize_with_merges(input, dialect).map(|(t, o, _)| (t, o))
}

/// v7.39.3 — like [`tokenize_with_offsets`], and additionally: for each
/// string literal this lexer built by IMPLICIT CONCATENATION, the byte
/// length of its first segment inside the merged body.
///
/// MySQL 9.7.2 labels `SELECT 'a' 'b'` `a` while its value is `ab`
/// (measured) — the label is the first literal as written, and the
/// merge happens here, so the length has to leave here too. Recovering
/// it from the source afterwards would mean decoding escapes a second
/// time, in a second place, which is exactly the kind of pair that
/// drifts.
#[allow(clippy::too_many_lines)] // big match — splitting would obscure the dispatch table
pub fn tokenize_with_merges(
    input: &str,
    dialect: Dialect,
) -> Result<(Vec<Token>, Vec<usize>, Vec<(usize, usize)>), LexError> {
    let backslash_escapes = dialect.backslash_escapes;
    // v7.39.2 — three of the rules below are about MySQL's GRAMMAR, not
    // about what `\` does inside a string: `#` starts a comment, block
    // comments do not nest, and `0x41` is a binary-string literal. They
    // read the escapes flag, so `SET sql_mode='NO_BACKSLASH_ESCAPES'`
    // took all three away with the escapes; measured on MySQL 9.7.2,
    // `0x41` then answered 65 instead of 'A'.
    let speaks_mysql = dialect.speaks_mysql;
    let bytes = input.as_bytes();
    let mut i = 0usize;
    let mut out = Vec::new();
    // r1038 — byte offset just past the last string literal pushed, so a
    // following one can tell whether only whitespace-with-a-newline
    // separates them. `None` whenever the previous token was anything
    // else.
    let mut last_string_end: Option<usize> = None;
    // Parallel to `out`: the start byte of each token. Filled at the tail of
    // every loop iteration for whatever token(s) that iteration pushed, so no
    // per-push-site bookkeeping is needed. (The only `continue` inside a
    // token-producing arm — the lone `@` — was rewritten to fall through.)
    let mut offsets: Vec<usize> = Vec::new();
    // v7.39.3 — (token index, byte length of the first segment) for every
    // literal built by implicit concatenation.
    let mut merges: Vec<(usize, usize)> = Vec::new();

    while i < bytes.len() {
        let start = i;
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
            // v7.38.18 — `#` to end of line, in the MySQL dialect only.
            //
            // MySQL 9 answers `SELECT 1 # hash comment` with `1`; PG
            // 18.4 answers `column "x" does not exist`, which is what
            // SPG already did in both dialects. So this is a dialect
            // split rather than a fix: a MySQL session gains the
            // comment, a PostgreSQL session keeps the error. A
            // mysqldump carries these.
            b'#' if speaks_mysql => {
                i += 1;
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
                    // v7.38.18 — a body made ONLY of optimiser hints is
                    // skipped whole.
                    //
                    // MySQL executes what is inside `/*! … */`, so SPG
                    // lexes it as SQL — and `SELECT /*! STRAIGHT_JOIN */ 1`
                    // was therefore a syntax error, because SPG has no
                    // such keyword. MySQL 9 answers `1`. A hint is a
                    // planner instruction, not a statement: the right
                    // reading of one SPG does not implement is to
                    // ignore it, which is what MySQL does for a hint
                    // its own planner has retired.
                    if let Some(end) = find_comment_end(bytes, j)
                        && body_is_only_hints(&bytes[j..end])
                    {
                        i = end + 2;
                        continue;
                    }
                    i = j;
                    continue;
                }
                // v7.38.18 — `/*+ … */`, MySQL 8's optimiser hint. It
                // is a comment to everyone who does not implement the
                // hint, which is SPG.
                if peek_eq(bytes, i + 2, b'+') {
                    let Some(end) = find_comment_end(bytes, i + 3) else {
                        return Err(LexError {
                            kind: LexErrorKind::UnterminatedBlockComment,
                            pos: start,
                        });
                    };
                    i = end + 2;
                    continue;
                }
                i += 2;
                let mut closed = false;
                // v7.38.18 — PostgreSQL's block comments NEST and
                // MySQL's do not, and the two rules disagree on the
                // same input. `SELECT /* a /* b */ c */ 1` is `1` on PG
                // 18.4 and a syntax error here; `SELECT /* a /* b */ 1`
                // is `1` on MySQL 9, where the first `*/` closes it.
                // Both were measured before this was written.
                //
                // The dialect flag SPG already threads for backslash
                // escapes picks the rule, because there is no reading
                // that satisfies both.
                let mut depth = 1usize;
                while i + 1 < bytes.len() {
                    if !speaks_mysql && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        depth -= 1;
                        if depth == 0 {
                            closed = true;
                            break;
                        }
                        continue;
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
            // v7.39 — `"` joins this arm in a MySQL session without
            // ANSI_QUOTES, where it opens a STRING. Measured on MySQL
            // 9.7.2: `"a""b"` is `a"b` (doubling), `"a\"b"` is `a"b`
            // (escape), `"a'b"` is `a'b` (the other quote is ordinary
            // inside), and `LENGTH("\n")` is 1 unless the session says
            // NO_BACKSLASH_ESCAPES. Every one of those falls out of
            // passing the quote byte down rather than a second copy of
            // the machinery.
            q @ (b'\'' | b'"') if q == b'\'' || !dialect.double_quoted_identifiers => {
                let (tok, consumed) = if backslash_escapes {
                    // MySQL-dialect session: plain strings decode
                    // backslash escapes — same machinery as E'…'.
                    lex_escape_string(input, i, true, q)?
                } else {
                    lex_quoted(input, i, q, false)?
                };
                // r1038 — SQL's implicit concatenation: two string
                // literals separated by whitespace CONTAINING A NEWLINE
                // are one literal. PG requires the newline, and so does
                // this: `'a' 'b'` on one line stays an error, which is
                // what distinguishes a continued literal from two
                // arguments someone forgot a comma between.
                //
                // sentori hit it in a `COMMENT ON`, which is how a
                // migration written for PostgreSQL failed to apply.
                let idx_of_last = out.len().saturating_sub(1);
                if let (Token::String(body), Some(prev_end)) = (&tok, last_string_end)
                    && let Some(gap) = input.get(prev_end..i)
                    && gap_continues_a_literal(gap, speaks_mysql)
                    && let Some(Token::String(head)) = out.last_mut()
                {
                    if merges.last().is_none_or(|(k, _)| *k != idx_of_last) {
                        merges.push((idx_of_last, head.len()));
                    }
                    head.push_str(body);
                    i += consumed;
                    last_string_end = Some(i);
                    continue;
                }
                let was_string = matches!(tok, Token::String(_));
                out.push(tok);
                i += consumed;
                last_string_end = was_string.then_some(i);
            }
            // v7.18 — PG escape-string literal `E'...'` / `e'...'`.
            // Closes the mailrs D-pre #3 reverse-acceptance gap:
            // `INSERT INTO oq VALUES (E'\\xdeadbeef'::bytea)` needs
            // the `E` prefix so `\\` decodes to a single `\`. The
            // produced Token::String carries the decoded body so
            // downstream parser / cast paths treat it identically
            // to a regular string literal.
            b'E' | b'e' if peek_eq(bytes, i + 1, b'\'') => {
                let (tok, consumed) = lex_escape_string(input, i + 1, false, b'\'')?;
                out.push(tok);
                i += 1 + consumed;
                // r1038 — an `E'…'` may LEAD a continued literal (PG18.4:
                // `E'a'\n'b'` is `ab`) though it may not continue one
                // (`'a'\nE'b'` is a syntax error there, and here, because
                // this arm never joins). Recording the end is what lets the
                // plain-string arm above see it as the head.
                last_string_end = Some(i);
            }
            // v7.38 (read01, T18) — PG `U&'...'` Unicode string literal.
            b'U' | b'u' if peek_eq(bytes, i + 1, b'&') && peek_eq(bytes, i + 2, b'\'') => {
                let (tok, consumed) = lex_unicode_string(input, i + 2)?;
                out.push(tok);
                i += 2 + consumed;
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
                let (tok, consumed) = lex_number(&input[i..], speaks_mysql)
                    .map_err(|kind| LexError { kind, pos: i })?;
                out.push(tok);
                i += consumed;
            }
            b'.' if peek_pred(bytes, i + 1, u8::is_ascii_digit) => {
                let (tok, consumed) = lex_number(&input[i..], speaks_mysql)
                    .map_err(|kind| LexError { kind, pos: i })?;
                out.push(tok);
                i += consumed;
            }
            b'+' => single(&mut out, Token::Plus, &mut i),
            // v7.37.6-A — PG JSONB `?` / `?|` / `?&`. Longest-match
            // order matters: try `?|` and `?&` before bare `?`.
            // SPG doesn't use `?` as a placeholder (uses `$N`
            // instead), so the bare `?` slot is free for JSONB.
            // v7.39 (read01 geo_ops.c) — `?||` (parallel) and `?-|`
            // (perpendicular) must win over `?|` / bare `?`.
            b'?' if peek_eq(bytes, i + 1, b'|') && peek_eq(bytes, i + 2, b'|') => {
                out.push(Token::GeomParallel);
                i += 3;
            }
            b'?' if peek_eq(bytes, i + 1, b'-') && peek_eq(bytes, i + 2, b'|') => {
                out.push(Token::GeomPerp);
                i += 3;
            }
            b'?' if peek_eq(bytes, i + 1, b'|') => {
                out.push(Token::JsonKeysAny);
                i += 2;
            }
            b'?' if peek_eq(bytes, i + 1, b'&') => {
                out.push(Token::JsonKeysAll);
                i += 2;
            }
            // v7.39 (read01 geo_ops.c) — `?-` "is horizontal" (after `?-|`
            // above claims the perpendicular spelling).
            b'?' if peek_eq(bytes, i + 1, b'-') => {
                out.push(Token::GeomHoriz);
                i += 2;
            }
            b'?' if peek_eq(bytes, i + 1, b'#') => {
                // v7.39 (round 508) — `?#` "do these intersect".
                out.push(Token::Intersects);
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
                // v7.39 (read01 geo_ops.c) — `##` closest-point operator.
                } else if peek_eq(bytes, i + 1, b'#') {
                    out.push(Token::ClosestPoint);
                    i += 2;
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
                } else if peek_eq(bytes, i + 1, b'@') && peek_eq(bytes, i + 2, b'@') {
                    // v7.39 (round 508) — `@@@`, before `@@`: longest match.
                    out.push(Token::TsMatchOld);
                    i += 3;
                } else if peek_eq(bytes, i + 1, b'-') && peek_eq(bytes, i + 2, b'@') {
                    // v7.39 (round 508) — `@-@` "length of".
                    out.push(Token::AtMinusAt);
                    i += 3;
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
                        // v7.39 (read01 round 95) — falls through to the
                        // per-token offset fill at the loop tail (was a
                        // `continue`, which would have skipped it).
                        out.push(Token::At);
                        i = prefix_end;
                    } else {
                        out.push(Token::SessionVar(input[i..end].to_string()));
                        i = end;
                    }
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
            b'=' => {
                // v7.38 (read01, T14) — `=>` names a function argument.
                if peek_eq(bytes, i + 1, b'>') {
                    out.push(Token::FatArrow);
                    i += 2;
                } else {
                    single(&mut out, Token::Eq, &mut i);
                }
            }
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
                } else if peek_eq(bytes, i + 1, b'^') {
                    // v7.39 (round 508) — `<^` "is strictly below".
                    out.push(Token::IsBelow);
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
            // v7.39 (read01 geo_ops.c) — `~=` geometric "same as".
            b'~' if peek_eq(bytes, i + 1, b'=') => {
                out.push(Token::GeomSameAs);
                i += 2;
            }
            // v7.39 (round 508) — the `text_pattern_ops` comparisons, longest
            // match first so `~<=~` beats `~<~`.
            b'~' if peek_eq(bytes, i + 1, b'<')
                && peek_eq(bytes, i + 2, b'=')
                && peek_eq(bytes, i + 3, b'~') =>
            {
                out.push(Token::PatternLtEq);
                i += 4;
            }
            b'~' if peek_eq(bytes, i + 1, b'>')
                && peek_eq(bytes, i + 2, b'=')
                && peek_eq(bytes, i + 3, b'~') =>
            {
                out.push(Token::PatternGtEq);
                i += 4;
            }
            b'~' if peek_eq(bytes, i + 1, b'<') && peek_eq(bytes, i + 2, b'~') => {
                out.push(Token::PatternLt);
                i += 3;
            }
            b'~' if peek_eq(bytes, i + 1, b'>') && peek_eq(bytes, i + 2, b'~') => {
                out.push(Token::PatternGt);
                i += 3;
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
                if peek_eq(bytes, i + 1, b'^') {
                    // v7.39 (round 508) — `>^` "is strictly above".
                    out.push(Token::IsAbove);
                    i += 2;
                } else if peek_eq(bytes, i + 1, b'>') && peek_eq(bytes, i + 2, b'=') {
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
            // v7.39 (read01 rangetypes.c) — range `&<` (does not extend to
            // the right of) / `&>` (does not extend to the left of).
            b'&' if peek_eq(bytes, i + 1, b'<') => {
                out.push(Token::OverLeft);
                i += 2;
            }
            b'&' if peek_eq(bytes, i + 1, b'>') => {
                out.push(Token::OverRight);
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
            // v7.39 (round 353, M10) — MySQL's `!` negation, after every
            // two- and three-byte `!…` operator above so none is stolen.
            // It reuses the NOT token; the parser gives it MySQL's tight
            // precedence (`!1 + 1` is 1 — `(!1)+1` — while `NOT 1 + 1`
            // is 0, measured on MariaDB 11).
            b'!' => {
                out.push(Token::Bang);
                i += 1;
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
        // Assign the iteration's start byte to any token(s) pushed above.
        // Whitespace/comment arms push nothing, so this adds nothing for them.
        while offsets.len() < out.len() {
            offsets.push(start);
        }
    }
    out.push(Token::Eof);
    offsets.push(bytes.len());
    Ok((out, offsets, merges))
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
    // v7.39 (round 621) — 6 keywords: as, in, is, on, or, to.
    //
    // `by` used to be here, and lexing it made it unusable as a name: a
    // `by` column could not be created, read, written, indexed or aliased.
    // `pg_get_keywords()` classes it `U` (unreserved) — alone among these
    // seven — so it is an ordinary identifier, and the clauses that own the
    // word (GROUP BY, ORDER BY, PARTITION BY) recognise it as one.
    if eq_ci(b, b"as") {
        return Some(Token::As);
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
/// v7.39 (round 332, V35) — `mysql` selects MySQL's escape table instead
/// of PG's `E'…'` one. Measured on MariaDB 11 vs PG 18.4, the two agree on
/// everything except three points:
///
/// | escape | PG `E'…'` | MySQL |
/// |---|---|---|
/// | `\Z` | `Z` | **0x1A** (ctrl-Z) |
/// | `\%` / `\_` | `%` / `_` | **both characters kept** — the backslash is
///   what makes LIKE treat the wildcard literally |
/// | `\xHH` / `\NNN` | decoded | **not special**: the backslash is dropped
///   and the rest is literal text |
///
/// Sharing one table meant a MySQL client's `'\Z'` arrived as the letter
/// `Z`, and `'a\%b'` lost the escape LIKE needed — silently wrong bytes,
/// not an error.
fn lex_escape_string(
    input: &str,
    start: usize,
    mysql: bool,
    quote: u8,
) -> Result<(Token, usize), LexError> {
    let bytes = input.as_bytes();
    debug_assert_eq!(bytes[start], quote);
    let mut i = start + 1;
    // v7.39 (round 773, F31 J3) — PG decodes byte escapes into a BYTE
    // buffer and validates the whole literal as UTF-8 at the end
    // (E'\303\251' is é; E'\777' is byte 0xFF and refuses with the
    // encoding sentence). The old char-per-escape model mapped each
    // byte to its Latin-1 codepoint, silently mangling multi-byte
    // sequences.
    let mut buf: Vec<u8> = Vec::new();
    let mut push_char = |buf: &mut Vec<u8>, c: char| {
        let mut tmp = [0u8; 4];
        buf.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
    };
    loop {
        if i >= bytes.len() {
            return Err(LexError {
                kind: LexErrorKind::UnterminatedString,
                pos: start,
            });
        }
        let b = bytes[i];
        if b == quote {
            if peek_eq(bytes, i + 1, quote) {
                push_char(&mut buf, char::from(quote));
                i += 2;
                continue;
            }
            i += 1;
            break;
        }
        if b == b'\\' && i + 1 < bytes.len() {
            let n = bytes[i + 1];
            // MySQL's own three points; everything below is shared.
            if mysql {
                match n {
                    // `\Z` is ctrl-Z, not the letter Z.
                    b'Z' => {
                        push_char(&mut buf, '\u{001A}');
                        i += 2;
                        continue;
                    }
                    // `\%` / `\_` keep BOTH characters: the backslash is
                    // what LIKE reads as "this wildcard is literal".
                    b'%' | b'_' => {
                        push_char(&mut buf, '\\');
                        push_char(&mut buf, n as char);
                        i += 2;
                        continue;
                    }
                    // `\xHH` and `\NNN` are not escapes at all here.
                    b'x' | b'X' => {
                        push_char(&mut buf, 'x');
                        i += 2;
                        continue;
                    }
                    d if d.is_ascii_digit() && d != b'0' => {
                        push_char(&mut buf, d as char);
                        i += 2;
                        continue;
                    }
                    _ => {}
                }
            }
            match n {
                b'\\' => {
                    push_char(&mut buf, '\\');
                    i += 2;
                }
                b'\'' => {
                    push_char(&mut buf, '\'');
                    i += 2;
                }
                b'"' => {
                    push_char(&mut buf, '"');
                    i += 2;
                }
                b'n' => {
                    push_char(&mut buf, '\n');
                    i += 2;
                }
                b'r' => {
                    push_char(&mut buf, '\r');
                    i += 2;
                }
                b't' => {
                    push_char(&mut buf, '\t');
                    i += 2;
                }
                b'b' => {
                    push_char(&mut buf, '\u{0008}');
                    i += 2;
                }
                b'f' => {
                    push_char(&mut buf, '\u{000C}');
                    i += 2;
                }
                b'v' => {
                    push_char(&mut buf, '\u{000B}');
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
                        push_char(
                            &mut buf,
                            char::from_u32(combined).ok_or(LexError {
                                kind: LexErrorKind::InvalidUnicodeEscape,
                                pos: i,
                            })?,
                        );
                        i += 12;
                    } else {
                        push_char(
                            &mut buf,
                            char::from_u32(cp).ok_or(LexError {
                                kind: LexErrorKind::InvalidUnicodeEscape,
                                pos: i,
                            })?,
                        );
                        i += 2 + ndigits;
                    }
                }
                b'0' if i + 2 >= bytes.len() || !bytes[i + 2].is_ascii_digit() => {
                    push_char(&mut buf, '\0');
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
                            buf.push(((a << 4) | b2) as u8);
                            i += 4;
                        }
                        (Some(a), _) => {
                            buf.push(a as u8);
                            i += 3;
                        }
                        _ => {
                            // \x with no hex follows — literal x.
                            push_char(&mut buf, 'x');
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
                    // A byte, as PG: \777 masks to 0xFF and the final
                    // UTF-8 validation refuses it.
                    buf.push((value & 0xFF) as u8);
                    i += take;
                }
                other => {
                    // Lenient fallback — same as PG with
                    // `standard_conforming_strings = off` warning:
                    // decode `\X` to literal `X`.
                    push_char(&mut buf, other as char);
                    i += 2;
                }
            }
        } else {
            let ch = input[i..].chars().next().expect("non-empty UTF-8 boundary");
            push_char(&mut buf, ch);
            i += ch.len_utf8();
        }
    }
    match String::from_utf8(buf) {
        Ok(decoded) => Ok((Token::String(decoded), i - start)),
        Err(e) => {
            let bad = e.as_bytes()[e.utf8_error().valid_up_to()];
            Err(LexError {
                kind: LexErrorKind::InvalidByteSequence(bad),
                pos: start,
            })
        }
    }
}

/// v7.38 (read01, T18) — lex a PG `U&'...'` Unicode string literal. `start`
/// points at the opening quote. Decodes `\XXXX` (4 hex), `\+XXXXXX` (6 hex),
/// `\\` → backslash, `''` → quote; the default escape is `\`. (A trailing
/// `UESCAPE 'c'` clause and the `U&"..."` identifier form are separate
/// follow-ups.)
fn lex_unicode_string(input: &str, start: usize) -> Result<(Token, usize), LexError> {
    let bytes = input.as_bytes();
    debug_assert_eq!(bytes[start], b'\'');
    let hex_char = |hex: &str, pos: usize| -> Result<char, LexError> {
        u32::from_str_radix(hex, 16)
            .ok()
            .and_then(char::from_u32)
            .ok_or(LexError {
                kind: LexErrorKind::InvalidUnicodeEscape,
                pos,
            })
    };
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
        if b == b'\\' {
            if peek_eq(bytes, i + 1, b'\\') {
                s.push('\\');
                i += 2;
                continue;
            }
            let (lo, hi) = if peek_eq(bytes, i + 1, b'+') {
                (i + 2, i + 8) // \+XXXXXX
            } else {
                (i + 1, i + 5) // \XXXX
            };
            if hi > bytes.len() || !input.is_char_boundary(lo) || !input.is_char_boundary(hi) {
                return Err(LexError {
                    kind: LexErrorKind::InvalidUnicodeEscape,
                    pos: i,
                });
            }
            s.push(hex_char(&input[lo..hi], i)?);
            i = hi;
            continue;
        }
        let ch = input[i..].chars().next().expect("valid utf-8 boundary");
        s.push(ch);
        i += ch.len_utf8();
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

fn lex_number(s: &str, mysql: bool) -> Result<(Token, usize), LexErrorKind> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    // v7.39 (round 184) — PG scan.l rejects a numeric literal that is
    // followed directly by an identifier character: `12__34`, `123_`,
    // `1.5_`, `123abc` are "trailing junk after numeric literal", not
    // "number + alias". Pre-r184 the tail silently became a column
    // alias (`SELECT 12__34` returned 12). The reported text spans the
    // number plus its identifier-shaped tail, like PG's error cursor.
    let junk_end = |from: usize| -> usize {
        let mut j = from;
        while j < bytes.len() && (bytes[j] == b'_' || bytes[j].is_ascii_alphanumeric()) {
            j += 1;
        }
        j
    };
    let junk_check = |end: usize| -> Result<(), LexErrorKind> {
        if end < bytes.len() && (bytes[end] == b'_' || bytes[end].is_ascii_alphabetic()) {
            return Err(LexErrorKind::TrailingJunkAfterNumber(
                s[..junk_end(end)].to_string(),
            ));
        }
        Ok(())
    };
    // v7.38 (read01) — PG 16+ non-decimal integer literals: `0x1F` (hex),
    // `0o17` (octal), `0b101` (binary), with optional `_` separators. Read the
    // radix digits, strip `_`, parse as i64 (NUMERIC on overflow).
    if bytes.len() >= 2 && bytes[0] == b'0' {
        let (radix, radix_name) = match bytes[1] {
            b'x' | b'X' => (Some(16u32), "hexadecimal"),
            b'o' | b'O' => (Some(8), "octal"),
            b'b' | b'B' => (Some(2), "binary"),
            _ => (None, ""),
        };
        if let Some(radix) = radix {
            // PG's shape is `0x(_?digit)+`: every `_` must be followed
            // by a radix digit (leading `_` allowed, trailing not).
            let mut j = 2;
            loop {
                let mut k = j;
                if k < bytes.len() && bytes[k] == b'_' {
                    k += 1;
                }
                if k < bytes.len() && (bytes[k] as char).is_digit(radix) {
                    j = k + 1;
                } else {
                    break;
                }
            }
            let digits: alloc::string::String = s[2..j].chars().filter(|c| *c != '_').collect();
            if digits.is_empty() {
                // `0x` / `0x_` — a radix prefix with no digits. PG:
                // "invalid hexadecimal integer"; pre-r184 the `0`
                // lexed alone and the rest aliased.
                return Err(LexErrorKind::InvalidRadixLiteral(
                    radix_name,
                    s[..junk_end(0)].to_string(),
                ));
            }
            junk_check(j)?;
            // v7.39 (round 367, M20) — in the MySQL dialect a `0x…`
            // hexadecimal literal is a BINARY STRING, not an integer
            // (mysqldump emits `0x…` for BINARY / BLOB column data, and
            // `0x41` is the string 'A'). The octal / binary radices keep
            // their integer reading — only `0x` diverges.
            if mysql && radix == 16 {
                return Ok((Token::HexBytes(digits), j));
            }
            // v7.40.0 — and `0b…` is one too. The comment above used to
            // say "only `0x` diverges"; measured on MySQL 9.7.2,
            // `HEX(CAST(0b101 AS BINARY))` is `05`, the same byte
            // `b'101'` gives, and SPG answered the integer 5. The bits
            // pack big-endian, left-padded to a byte — the same rule
            // `b'…'` already follows — so they are lowered onto the
            // same token by way of their hex spelling.
            if mysql && radix == 2 {
                let mut bits = digits;
                while bits.len() % 4 != 0 {
                    bits.insert(0, '0');
                }
                let mut hex = alloc::string::String::with_capacity(bits.len() / 4);
                for nibble in bits.as_bytes().chunks(4) {
                    let v = nibble.iter().fold(0u8, |acc, c| acc * 2 + (*c - b'0'));
                    hex.push(char::from_digit(u32::from(v), 16).unwrap_or('0'));
                }
                if hex.len() % 2 == 1 {
                    hex.insert(0, '0');
                }
                return Ok((Token::HexBytes(hex), j));
            }
            return match i64::from_str_radix(&digits, radix) {
                Ok(v) => Ok((Token::Integer(v), j)),
                // Over i64 → keep as decimal NUMERIC text.
                Err(_) => match u128::from_str_radix(&digits, radix) {
                    Ok(v) => Ok((Token::Numeric(alloc::format!("{v}")), j)),
                    Err(_) => Err(LexErrorKind::BadNumber(s[..j].to_string())),
                },
            };
        }
    }
    // v7.38 (read01) — track the dot and exponent separately. PG: a dotted
    // literal with NO exponent is NUMERIC; an exponent (`1e5`, `1.5e3`) makes
    // it double precision; a bare integer is INTEGER unless it overflows i64,
    // in which case it is NUMERIC too.
    let mut has_dot = false;
    let mut has_exp = false;

    // v7.38 (read01) — accept `_` digit separators between digits (PG 16+:
    // `1_000_000`, `1_000.5`). Stripped before parsing below.
    let digit_or_sep = |bytes: &[u8], i: usize| -> bool {
        bytes[i].is_ascii_digit()
            || (bytes[i] == b'_' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit())
    };

    while i < bytes.len() && digit_or_sep(bytes, i) {
        i += 1;
    }
    // v7.37.20 (20.4) — do NOT consume `.` when it's part of a `..`
    // range operator; leave both dots for the top-level dispatcher
    // which will emit a single Token::DotDot.
    if i < bytes.len() && bytes[i] == b'.' && !(i + 1 < bytes.len() && bytes[i + 1] == b'.') {
        has_dot = true;
        i += 1;
        // r184 — a fraction may only START with a digit: `1._5` is
        // trailing junk in PG (`_` is a separator BETWEEN digits),
        // not 1.5. Leaving the `_` unconsumed routes it into the
        // junk check below.
        if i < bytes.len() && bytes[i].is_ascii_digit() {
            while i < bytes.len() && digit_or_sep(bytes, i) {
                i += 1;
            }
        }
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        has_exp = true;
        i += 1;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        let exp_start = i;
        // r184 — same rule as the fraction: the exponent must start
        // with a digit (`1e_5` is junk, not 1e5).
        if i < bytes.len() && bytes[i].is_ascii_digit() {
            while i < bytes.len() && digit_or_sep(bytes, i) {
                i += 1;
            }
        }
        if exp_start == i {
            return Err(LexErrorKind::BadNumber(s[..i].to_string()));
        }
    }
    // r184 — reject an identifier-shaped tail glued to the number.
    junk_check(i)?;

    // Strip the `_` separators for parsing / storage (source span keeps `i`).
    let owned;
    let lit: &str = if s[..i].contains('_') {
        owned = s[..i].replace('_', "");
        &owned
    } else {
        &s[..i]
    };
    if has_exp {
        // v7.39 (read01 numeric.c) — an exponent literal is NUMERIC in PG
        // (`pg_typeof(1e5)` → numeric), not double precision. Keep the source
        // text; the parser expands the notation into a plain decimal.
        Ok((Token::Numeric(lit.to_string()), i))
    } else if has_dot {
        // Dotted literal → exact NUMERIC (keep the source text verbatim).
        Ok((Token::Numeric(lit.to_string()), i))
    } else {
        // Bare integer → INTEGER, or NUMERIC if it overflows i64.
        match lit.parse::<i64>() {
            Ok(v) => Ok((Token::Integer(v), i)),
            Err(_) => Ok((Token::Numeric(lit.to_string()), i)),
        }
    }
}

/// v7.38.18 — the index of the `*/` that closes a comment body starting
/// at `from`, or `None` when it never closes.
fn find_comment_end(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 1 < bytes.len() {
        if bytes[i] == b'*' && bytes[i + 1] == b'/' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Is this `/*! … */` body made only of optimiser hints?
///
/// A hint is a bare word, or a word with a parenthesised argument, and
/// nothing else — `STRAIGHT_JOIN`, `SQL_NO_CACHE`,
/// `MAX_EXECUTION_TIME(1000)`. A body with a comma, an operator or a
/// keyword SPG knows is real SQL and goes to the parser, which is what
/// `/*!40000 , 2 */` in a mysqldump relies on.
fn body_is_only_hints(body: &[u8]) -> bool {
    let text = core::str::from_utf8(body).unwrap_or("");
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    // v7.38.18 — SEVERAL words, because a hint can be more than one:
    // `FORCE INDEX (PRIMARY)`, `SQL_SMALL_RESULT`, `STRAIGHT_JOIN`. The
    // first version accepted one word plus an optional argument and
    // `FORCE INDEX (…)` stayed a syntax error.
    let mut chars = trimmed.chars().peekable();
    let mut saw_word = false;
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            saw_word = true;
            while chars
                .peek()
                .is_some_and(|n| n.is_ascii_alphanumeric() || *n == '_')
            {
                chars.next();
            }
            // An optional parenthesised argument, which may be
            // separated by a space: `FORCE INDEX (PRIMARY)` is a hint
            // and `FORCE INDEX(PRIMARY)` is the same hint. Peeking for
            // `(` without skipping the space left the first one a
            // syntax error while the second parsed.
            while chars.peek().is_some_and(|n| n.is_whitespace()) {
                chars.next();
            }
            if chars.peek() == Some(&'(') {
                let mut depth = 0usize;
                for n in chars.by_ref() {
                    if n == '(' {
                        depth += 1;
                    } else if n == ')' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                }
            }
            continue;
        }
        return false;
    }
    saw_word
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
        // v7.38 (read01) — a dotted literal lexes as NUMERIC (exact source
        // text); an exponent form stays double precision.
        assert_eq!(
            lex("0 42 1.5 .5 1e10 2.5e-3"),
            vec![
                Token::Integer(0),
                Token::Integer(42),
                Token::Numeric("1.5".to_string()),
                Token::Numeric(".5".to_string()),
                Token::Numeric("1e10".to_string()),
                Token::Numeric("2.5e-3".to_string()),
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
            vec![
                Token::Order,
                Token::Ident("by".into()),
                Token::Limit,
                Token::Eof,
            ]
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
