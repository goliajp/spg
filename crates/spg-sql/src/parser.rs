//! Recursive-descent parser with a Pratt (precedence-climbing) sub-parser for
//! expressions.
//!
//! Precedence (lowest → highest binding):
//! `OR` (1) `<` `AND` (2) `<` `NOT` unary (3) `<`
//! comparisons `=` `<>` `<` `<=` `>` `>=` (4) `<`
//! `+` `-` (5) `<` `*` `/` (6) `<` unary `-` (7) `<` parens / atom.
//!
//! This matches PG's behaviour for the operators we support — e.g. `NOT a = b`
//! parses as `NOT (a = b)` and `-a * b` as `(-a) * b`.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::mem;

use crate::ast::{
    AssignTarget, BinOp, CastTarget, Collation, ColumnDef, ColumnName, ColumnTypeName,
    CreateFunctionStatement, CreateIndexStatement, CreatePublicationStatement,
    CreateSubscriptionStatement, CreateTableStatement, CreateTriggerStatement, Expr, ExtractField,
    FkAction, ForeignKeyConstraint, FrameBound, FrameKind, FromClause, FromJoin, FunctionArg,
    FunctionArgMode, FunctionArgType, FunctionBody, FunctionReturn, IndexMethod, InsertStatement,
    IsolationLevel, JoinKind, Literal, NullTreatment, OrderBy, PlPgSqlBlock, PlPgSqlDeclare,
    PlPgSqlStmt, PublicationScope, RaiseLevel, RangeKindAst, ReturnTarget, SelectItem,
    SelectStatement, Statement, TableRef, TriggerEvent, TriggerForEach, TriggerTiming, UnOp,
    UnionKind, VecEncoding, WindowFrame,
};
use crate::lexer::{self, LexError, Token};

/// v7.14.0 — true when the leading keyword of a top-level
/// statement is one of the dump-emitted DDL forms SPG accepts
/// as a no-op (no behavioural effect on the single-schema /
/// single-database model). These statements are consumed up to
/// the next `;` / EOF and returned as `Statement::Empty`.
fn is_dump_noise_statement(lc: &str) -> bool {
    matches!(
        lc,
        // Object comments / privileges / ownership — none of
        // these change schema semantics on SPG.
        "comment"
            | "grant"
            | "revoke"
            // MySQL bulk-load brackets.
            | "lock"
            | "unlock"
            // MySQL OPTIMIZE / ANALYZE TABLE / CHECK TABLE
            // diagnostics that pg_dump-style tools also emit
            // post-restore.
            | "optimize"
            | "check"
            | "use"
            // PG psql backslash meta-commands that newer
            // pg_dump versions emit unescaped (\restrict /
            // \unrestrict). Real psql intercepts these; SPG's
            // PG-wire sees them as raw text.
            | "\\restrict"
            | "\\unrestrict"
            // v7.17.0 Phase 4.1 — MySQL `DELIMITER //` and
            // `DELIMITER ;` directives. Technically client-side
            // (the `mysql` CLI uses them to set the statement
            // terminator), not SQL — but mysqldump and stored-
            // procedure scripts emit them inline. SPG's parser
            // sees one statement at a time and doesn't care
            // about the terminator, so consume DELIMITER lines
            // as Empty.
            | "delimiter"
            // v7.37.17 (17.6 siblings) — additional PG maintenance /
            // session-state statements pg_dump + application startup
            // scripts emit. SPG has no matching session-state to
            // discard (no prepared-plan cache surface, no temp
            // sequences), no matching security-label / storage-
            // option to apply, no separate CREATE/DROP CAST that
            // affects execution.
            | "discard"
            | "deallocate"
            | "security"
            // v7.37.17 (17.6 siblings) — PG role-cleanup statements
            // pg_dump / pg_dumpall emit around DROP ROLE:
            //   REASSIGN OWNED BY <role> [, ...] TO <newrole>
            //   DROP OWNED BY <role> [, ...] [CASCADE | RESTRICT]
            // Both operate on the role's owned objects; SPG has no
            // role-owner model, so accept-and-no-op.
            | "reassign"
            // v7.37.17 (17.6 siblings) — PG's SQL-level prepared
            // statement surface. PREPARE / EXECUTE / DEALLOCATE
            // (already listed above) are runtime prepared-statement
            // primitives; PG-JDBC + ORMs emit them in some paths.
            // SPG's extended-query protocol handles per-connection
            // named plans directly, but the SQL-level surface itself
            // has no matching machinery. Accept-and-no-op so drivers
            // that fall back to SQL PREPARE don't break — real
            // execution of the target query still happens via the
            // extended-query flow.
            | "prepare"
            | "execute"
            // v7.37.17 (17.6 sibling) — LOAD '<library>'. pg_dump
            // + extension scripts use LOAD to preload shared
            // libraries. SPG doesn't have a shared-library extension
            // point today (extensions ship as first-class crates
            // linked at build time); accept as a no-op.
            | "load"
            // v7.37.17 (17.6 sibling) — CALL <procedure>(<args>).
            // PG 11+ procedure call syntax. SPG's stored-procedure
            // surface is v7.40 PL/pgSQL epic; accept as a no-op so
            // migrations that reference stored procs don't stall at
            // parse.
            | "call"
    )
}

/// v7.37.43-T4 — PG-unreserved keywords that are legal identifiers
/// per `pg_get_keywords()`. SPG tokenizes these as named variants
/// so the parser can dispatch on them in their owning contexts
/// (`RELEASE SAVEPOINT`, `SHOW name`, `BEGIN`/`COMMIT`/`ROLLBACK`,
/// `CREATE INDEX`, etc.), but they MUST stay usable as table /
/// column / alias names — that's the PG contract for unreserved
/// keywords (see PG docs Appendix C.1).
///
/// Before this generalisation, sentori migration 0001_init.sql
/// `release TEXT NOT NULL` blew up the parser with "expected
/// identifier, got Release", and the same gap stalked every
/// SPG drop-in user whose schema had a column / alias named
/// `release` / `index` / `tables` / `show` / `savepoint` /
/// `begin` / `commit` / `rollback` / `drop` / `insert` / `values`
/// / `limit` / `partition`. PG accepts all of them as identifiers
/// when unquoted, so SPG must too.
///
/// Returns the canonical lowercase identifier text when the token
/// belongs to PG's unreserved class, `None` otherwise. Used by
/// `expect_ident_like` (column / table / alias names) so the
/// generalisation applies everywhere an identifier may appear,
/// not just in the contexts these tokens were introduced for.
fn unreserved_keyword_text(tok: &Token) -> Option<String> {
    let s = match tok {
        // PG keyword class: unreserved or col_name.
        Token::Release => "release",
        Token::Savepoint => "savepoint",
        Token::Show => "show",
        Token::Index => "index",
        Token::Begin => "begin",
        Token::Commit => "commit",
        Token::Rollback => "rollback",
        Token::Drop => "drop",
        Token::Insert => "insert",
        Token::Values => "values",
        Token::Limit => "limit",
        Token::Partition => "partition",
        Token::Tables => "tables",
        Token::Connection => "connection",
        Token::Publication => "publication",
        Token::Subscription => "subscription",
        Token::Interval => "interval",
        // `extract` is non-reserved in PG too (it's a function the
        // parser dispatches via context — outside that context it's
        // a plain identifier).
        Token::Extract => "extract",
        Token::Offset => "offset",
        // `to` is reserved in PG (used in many "AS … TO …" forms), so
        // it is NOT relaxed here. Same for `from`, `where`, `as`,
        // `select`, `not`, `and`, `or`, `null`, `true`, `false`,
        // `create`, `table`, `into`, `on`, `order`, `by`, `having`,
        // `group`, `distinct`, `union`, `all`, `join`, `inner`,
        // `left`, `cross`, `outer`, `default`, `is`, `between`,
        // `in`, `like`, `for`, `except`, `desc`, `asc`, `partition`
        // (partial — keep partition as unreserved per modern PG).
        _ => return None,
    };
    Some(s.to_string())
}

/// v7.9.22 — recognise pgvector / SPG vector-index opclass names
/// in CREATE INDEX. SPG's HNSW already routes by query operator;
/// the opclass is accepted for `pg_dump` compatibility (mailrs
/// migration follow-up G5).
/// v7.13.0 — extended to recognise PG built-in / pg_trgm opclasses
/// (mailrs round-5 G5). These are tokens-only acceptance — SPG
/// doesn't change index behaviour based on them.
/// v7.37.17 (17.6 siblings) — the four PG `each` SRFs share one
/// FROM-clause pipeline; the stored name tells the executor whether
/// the value column keeps JSON rendering (`jsonb_each` / `json_each`)
/// or unwraps to text (`*_each_text`).
fn is_json_each_name(s: &str) -> bool {
    s.eq_ignore_ascii_case("jsonb_each_text")
        || s.eq_ignore_ascii_case("jsonb_each")
        || s.eq_ignore_ascii_case("json_each_text")
        || s.eq_ignore_ascii_case("json_each")
}

fn is_vector_opclass_name(name: &str) -> bool {
    let lc = name.to_ascii_lowercase();
    matches!(
        lc.as_str(),
        "vector_cosine_ops"
            | "vector_l2_ops"
            | "vector_ip_ops"
            | "halfvec_cosine_ops"
            | "halfvec_l2_ops"
            | "halfvec_ip_ops"
            | "sq8_cosine_ops"
            | "sq8_l2_ops"
            | "sq8_ip_ops"
            // pg_trgm — trigram operator class. SPG's GIN index
            // already uses tsvector tokens; trigram-style LIKE
            // pattern matching still routes through a sequential
            // scan, but the opclass name is accepted so PG schemas
            // load.
            | "gin_trgm_ops"
            | "gist_trgm_ops"
            // PG built-in btree opclasses occasionally appear in
            // pg_dump output for column types with multiple
            // sort orders (text_pattern_ops, varchar_pattern_ops,
            // bpchar_pattern_ops).
            | "text_pattern_ops"
            | "varchar_pattern_ops"
            | "bpchar_pattern_ops"
            | "int4_ops"
            | "int8_ops"
            | "text_ops"
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    /// Index into the token stream where parsing tripped. Not a byte offset.
    pub token_pos: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "parse error at token #{}: {}",
            self.token_pos, self.message
        )
    }
}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        Self {
            message: format!("lex: {e}"),
            token_pos: 0,
        }
    }
}

/// v7.9.30 — parse a single expression (no trailing junk). Used by
/// the engine to re-hydrate stored partial-index / unique-index
/// predicates from their canonical Display form. The same Pratt
/// parser the statement path uses; this entry point just skips the
/// statement dispatch.
pub fn parse_expression(input: &str) -> Result<Expr, ParseError> {
    let tokens = lexer::tokenize(input)?;
    let mut p = Parser::new(tokens);
    let expr = p.parse_expr(0)?;
    p.expect_eof()?;
    Ok(expr)
}

/// Parse exactly one statement, swallow an optional trailing `;`, and require
/// the token stream to end there. PG string semantics.
pub fn parse_statement(input: &str) -> Result<Statement, ParseError> {
    parse_statement_with(input, false)
}

/// v7.22 (round-13 T3) — dialect-aware entry: `backslash_escapes`
/// selects MySQL-style string lexing (see `lexer::tokenize_with`).
/// The engine threads its session flag through here.
pub fn parse_statement_with(input: &str, backslash_escapes: bool) -> Result<Statement, ParseError> {
    let tokens = lexer::tokenize_with(input, backslash_escapes)?;
    let mut p = Parser::new(tokens);
    let stmt = p.parse_one_statement()?;
    if matches!(p.peek(), Token::Semicolon) {
        p.advance();
    }
    p.expect_eof()?;
    Ok(stmt)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// v7.30.2 (mailrs round-25 ask 2) — live nesting depth of the
    /// mutually recursive expr/select parsers. Bounded so a deeply
    /// nested input returns a parse error instead of overflowing
    /// the stack (embed hosts die on overflow — it is an abort,
    /// not a catchable error).
    nest_depth: usize,
}

/// Max expr/select parser nesting (parens, subqueries, CASE, …).
/// Real SQL nests a few dozen levels at the extreme. Each nesting
/// level costs a parse_expr→parse_unary→parse_atom frame chain —
/// over 10 KiB in debug builds (parse_atom is a giant match) — so
/// 64 is the highest budget that stays comfortably inside a 2 MiB
/// worker stack in BOTH debug and release builds.
const MAX_NEST_DEPTH: usize = 64;

/// Max consecutive binary operators at ONE precedence level
/// (`a OR b OR c …`, `1+1+1…`). The chain builds iteratively at
/// parse time but evaluates and drops recursively — depth beyond
/// this overflows 2 MiB worker stacks (debug eval frames run
/// multiple KiB). `IN (…)` lists are flat and unaffected.
const MAX_BINARY_CHAIN: usize = 256;

/// v7.22 (round-13 gap 5) — the kind keyword after `CONSTRAINT
/// <name>` in a CREATE TABLE column list. FOREIGN KEY is not here:
/// it keeps its dedicated path (`parse_table_level_fk`).
enum NamedTableConstraintKind {
    Check,
    Unique,
    PrimaryKey,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            pos: 0,
            nest_depth: 0,
        }
    }

    /// v7.30.2 (mailrs round-25 ask 2) — bump the expr/select
    /// nesting depth, erroring out cleanly past the budget.
    fn enter_nested(&mut self) -> Result<(), ParseError> {
        self.nest_depth += 1;
        if self.nest_depth > MAX_NEST_DEPTH {
            self.nest_depth -= 1;
            return Err(self.err(alloc::format!(
                "statement nests deeper than {MAX_NEST_DEPTH} levels"
            )));
        }
        Ok(())
    }

    fn peek(&self) -> &Token {
        // tokens always ends with Eof; pos is clamped in advance().
        &self.tokens[self.pos]
    }

    fn advance(&mut self) -> Token {
        let t = mem::replace(&mut self.tokens[self.pos], Token::Eof);
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        t
    }

    fn err(&self, message: String) -> ParseError {
        ParseError {
            message,
            token_pos: self.pos,
        }
    }

    fn expect_eof(&self) -> Result<(), ParseError> {
        if matches!(self.peek(), Token::Eof) {
            Ok(())
        } else {
            Err(self.err(format!("expected end of input, got {:?}", self.peek())))
        }
    }

    /// v7.14.0 — swallow every token up to (but not including) the
    /// next semicolon / EOF. Used by the dump-noise dispatcher
    /// to consume `COMMENT ON …`, `GRANT …`, `LOCK TABLES …`,
    /// etc. without modeling each grammar.
    fn consume_until_statement_boundary(&mut self) {
        loop {
            match self.peek() {
                Token::Semicolon | Token::Eof => return,
                _ => self.advance(),
            };
        }
    }

    /// v7.22 (round-13 T2) — consume to the statement boundary like
    /// `consume_until_statement_boundary`, but pick out the sequence
    /// name on the way: either `SEQUENCE NAME <ident>` (identity
    /// columns) or the first string literal (`nextval('<seq>')`).
    /// Schema qualifiers and `::regclass` casts are stripped.
    fn scan_sequence_name_until_boundary(&mut self) -> Option<String> {
        let mut seq: Option<String> = None;
        let mut after_sequence_kw = false;
        let mut after_name_kw = false;
        loop {
            match self.peek().clone() {
                Token::Semicolon | Token::Eof => break,
                Token::Ident(s) | Token::QuotedIdent(s) => {
                    if after_name_kw && seq.is_none() {
                        self.advance();
                        let mut name = s;
                        // `SEQUENCE NAME public.groups_id_seq` — keep
                        // the bare name, drop qualifiers.
                        while matches!(self.peek(), Token::Dot) {
                            self.advance();
                            if let Token::Ident(n) | Token::QuotedIdent(n) = self.advance() {
                                name = n;
                            }
                        }
                        seq = Some(name);
                        after_name_kw = false;
                        continue;
                    }
                    if after_sequence_kw && s.eq_ignore_ascii_case("name") {
                        after_name_kw = true;
                        after_sequence_kw = false;
                    } else {
                        after_sequence_kw = s.eq_ignore_ascii_case("sequence");
                    }
                    self.advance();
                }
                Token::String(s) => {
                    if seq.is_none() {
                        // `nextval('public.groups_id_seq'::regclass)`
                        let bare = s
                            .rsplit_once('.')
                            .map_or_else(|| s.clone(), |(_, b)| b.to_string());
                        seq = Some(bare);
                    }
                    self.advance();
                }
                _ => {
                    after_sequence_kw = false;
                    after_name_kw = false;
                    self.advance();
                }
            }
        }
        seq
    }

    fn expect_ident_like(&mut self) -> Result<String, ParseError> {
        let first = match self.advance() {
            Token::Ident(s) | Token::QuotedIdent(s) => s,
            // v7.37.43-T4 — PG-unreserved keywords are legal identifiers
            // per PG's `pg_get_keywords()` classification. SPG tokenizes
            // these as named variants for parsing leverage in the
            // contexts that own them (`RELEASE SAVEPOINT`, `SHOW name`,
            // `BEGIN`, etc.), but they MUST still be usable as table /
            // column / alias names in DDL+DML. Sentori migrations like
            // 0001_init.sql ship `release TEXT NOT NULL` in the events
            // table — the `events.release` column carries the release
            // identifier string. Pre-T4 this triggered "expected
            // identifier, got Release" and blocked every drop-in user
            // whose schema had a column / alias with one of these names.
            other if unreserved_keyword_text(&other).is_some() => {
                unreserved_keyword_text(&other).unwrap()
            }
            other => {
                return Err(ParseError {
                    message: format!("expected identifier, got {other:?}"),
                    token_pos: self.pos.saturating_sub(1),
                });
            }
        };
        // v7.14.0 — strip optional `<schema>.` prefix. PG dumps
        // qualify every name with `public.` (and pg_catalog.* for
        // functions); SPG is single-schema so we discard the
        // prefix and return only the trailing ident. Same shape
        // also handles MySQL `db.tbl` cross-database refs (SPG
        // ignores the db part).
        if matches!(self.peek(), Token::Dot) {
            self.advance();
            match self.advance() {
                Token::Ident(s) | Token::QuotedIdent(s) => return Ok(s),
                other if unreserved_keyword_text(&other).is_some() => {
                    return Ok(unreserved_keyword_text(&other).unwrap());
                }
                other => {
                    return Err(ParseError {
                        message: format!("expected identifier after '{first}.', got {other:?}"),
                        token_pos: self.pos.saturating_sub(1),
                    });
                }
            }
        }
        Ok(first)
    }

    #[allow(clippy::too_many_lines)]
    fn parse_one_statement(&mut self) -> Result<Statement, ParseError> {
        // v7.14.0 — empty / comment-only / semicolon-only input
        // (after the lexer strips line + block + MySQL
        // conditional comments) lands as Statement::Empty.
        // pg_dump and mysqldump emit several wrappers that
        // collapse to nothing after stripping (`/*!40101 SET …
        // */;`, blank lines between statements); the engine
        // returns CommandOk no-op so the dump loads cleanly.
        if matches!(self.peek(), Token::Eof | Token::Semicolon) {
            return Ok(Statement::Empty);
        }
        // v7.14.0 — pg_dump / mysqldump "noise" statements:
        // catalog / metadata DDL that has no behavioural effect
        // on SPG's single-schema, single-database, single-user
        // model. Consume the whole statement up to the next
        // semicolon / EOF and return Empty. This is broader than
        // the per-keyword DROP / SET / COMMENT arms but lets the
        // long tail of `LOCK TABLES`, `UNLOCK TABLES`, `GRANT`,
        // `REVOKE`, `ALTER OWNER TO`, `\restrict`, `\unrestrict`,
        // `BEGIN; COMMIT;` wrappers, etc. all pass through.
        if let Token::Ident(s) | Token::QuotedIdent(s) = self.peek() {
            let lc = s.to_ascii_lowercase();
            if is_dump_noise_statement(&lc) {
                self.consume_until_statement_boundary();
                return Ok(Statement::Empty);
            }
        }
        match self.peek() {
            Token::Select => self.parse_select_stmt(),
            // v7.37.17 (17.6 siblings) — a statement opening with a
            // parenthesized query group: `(SELECT … UNION …)
            // INTERSECT …`. parse_bare_select's group arm consumes
            // the parens; the select parser handles the outer chain
            // and tail.
            Token::LParen
                if matches!(
                    self.tokens.get(self.pos + 1),
                    Some(Token::Select | Token::LParen)
                ) =>
            {
                self.parse_select_stmt()
            }
            // v7.37.17 (17.6 siblings) — top-level bare VALUES
            // statement (`VALUES (1), (2) [ORDER BY …] [LIMIT …]`).
            // Lowers to the same UNION ALL chain the FROM-position
            // form uses, then reuses the shared SELECT tail.
            Token::Values => {
                self.advance(); // VALUES
                let mut head = self.parse_values_rows_body()?;
                self.parse_select_tail_into(&mut head)?;
                Ok(Statement::Select(head))
            }
            // v7.9.27 — `DO $$ … $$ [LANGUAGE plpgsql]`. The
            // body is a dollar-quoted plpgsql block (lexer already
            // collapsed `$$…$$` into a single Token::String).
            // v7.16.2 — mailrs round-10 A.2: parse the body as a
            // real PlPgSqlBlock so the engine can EXECUTE it at
            // top level instead of silently swallowing. Pre-
            // v7.16.2 the parser threw the body away and the
            // engine returned CommandOk for the entire DO; that
            // turned `DO BEGIN … IF EXISTS ... THEN ALTER …; END
            // $$` into a SEV-1 silent no-op (the IF + the rename
            // were both invisible — mailrs's migrate-042 didn't
            // actually run). Now the body parses + executes;
            // EmbeddedSql inside the block runs immediately
            // against the engine (not deferred — we're at top
            // level, not inside a trigger row-write loop).
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("do") => {
                self.advance();
                let body_text = match self.advance() {
                    Token::String(s) => s,
                    other => {
                        return Err(self.err(alloc::format!(
                            "expected dollar-quoted body after DO, got {other:?}"
                        )));
                    }
                };
                // Optional `LANGUAGE <name>` trailer (idents only).
                if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("language")) {
                    self.advance();
                    let _ = self.expect_ident_like()?;
                }
                // Parse the body — same shape CREATE FUNCTION
                // uses for trigger function bodies. If the body
                // doesn't parse cleanly we surface the error
                // (better than silent no-op).
                let block = parse_plpgsql_body(&body_text)?;
                Ok(Statement::DoBlock(block))
            }
            // v4.11: `WITH name AS (SELECT ...) [, ...] SELECT ...`.
            // WITH isn't a reserved token in our lexer — comes through
            // as `Token::Ident("with")` (case-insensitive).
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("with") => {
                self.advance();
                self.parse_with_cte_then_select()
            }
            // v4.26: `EXPLAIN [ANALYZE] <select>`. Comes through as
            // an identifier — not a reserved keyword.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("explain") => {
                self.advance();
                let mut analyze = false;
                let mut suggest = false;
                let mut costs_off = false;
                let mut buffers = false;
                let mut timing_off = false;
                let mut settings = false;
                let mut wal = false;
                let mut format = crate::ast::ExplainFormat::Text;
                // v6.8.3 + v7.37.7 — `EXPLAIN (option [, option…])`
                // syntax accepts SUGGEST + COSTS ON|OFF. Multiple
                // options are comma-separated. Booleans default to ON
                // when the value token is omitted (matches PG).
                if matches!(self.peek(), Token::LParen) {
                    self.advance();
                    loop {
                        let opt = match self.peek().clone() {
                            Token::Ident(s) | Token::QuotedIdent(s) => s,
                            other => {
                                return Err(self.err(format!(
                                    "expected option keyword inside EXPLAIN (…), got {other:?}"
                                )));
                            }
                        };
                        self.advance();
                        if opt.eq_ignore_ascii_case("suggest") {
                            suggest = true;
                            // SUGGEST takes no explicit value today.
                        } else if opt.eq_ignore_ascii_case("costs") {
                            // PG syntax: `COSTS [ON | OFF]`. Default
                            // when value omitted is ON, so plain
                            // `COSTS` is a no-op. `COSTS OFF` flips.
                            // `ON` lexes to `Token::On` (reserved
                            // keyword in JOIN ... ON contexts); accept
                            // it alongside the bare Ident form so the
                            // grammar matches PG verbatim.
                            let value = match self.peek().clone() {
                                Token::On => {
                                    self.advance();
                                    true
                                }
                                Token::Ident(v) | Token::QuotedIdent(v)
                                    if v.eq_ignore_ascii_case("off") =>
                                {
                                    self.advance();
                                    false
                                }
                                Token::Ident(v) | Token::QuotedIdent(v)
                                    if v.eq_ignore_ascii_case("true") =>
                                {
                                    self.advance();
                                    true
                                }
                                _ => true,
                            };
                            costs_off = !value;
                        } else if opt.eq_ignore_ascii_case("analyze")
                            || opt.eq_ignore_ascii_case("analyse")
                        {
                            // v7.37.22 — `EXPLAIN (ANALYZE [ON|OFF]) <S>`.
                            // Same default-ON rule as ANALYZE keyword form.
                            let value = match self.peek().clone() {
                                Token::On => {
                                    self.advance();
                                    true
                                }
                                Token::Ident(v) | Token::QuotedIdent(v)
                                    if v.eq_ignore_ascii_case("off") =>
                                {
                                    self.advance();
                                    false
                                }
                                Token::Ident(v) | Token::QuotedIdent(v)
                                    if v.eq_ignore_ascii_case("true") =>
                                {
                                    self.advance();
                                    true
                                }
                                _ => true,
                            };
                            analyze = value;
                        } else if opt.eq_ignore_ascii_case("buffers") {
                            // v7.37.22 — `BUFFERS [ON|OFF]`.
                            let value = match self.peek().clone() {
                                Token::On => {
                                    self.advance();
                                    true
                                }
                                Token::Ident(v) | Token::QuotedIdent(v)
                                    if v.eq_ignore_ascii_case("off") =>
                                {
                                    self.advance();
                                    false
                                }
                                Token::Ident(v) | Token::QuotedIdent(v)
                                    if v.eq_ignore_ascii_case("true") =>
                                {
                                    self.advance();
                                    true
                                }
                                _ => true,
                            };
                            buffers = value;
                        } else if opt.eq_ignore_ascii_case("timing") {
                            // v7.37.22 — `TIMING [ON|OFF]`. OFF strips
                            // the measured wall-clock annotation.
                            let value = match self.peek().clone() {
                                Token::On => {
                                    self.advance();
                                    true
                                }
                                Token::Ident(v) | Token::QuotedIdent(v)
                                    if v.eq_ignore_ascii_case("off") =>
                                {
                                    self.advance();
                                    false
                                }
                                Token::Ident(v) | Token::QuotedIdent(v)
                                    if v.eq_ignore_ascii_case("true") =>
                                {
                                    self.advance();
                                    true
                                }
                                _ => true,
                            };
                            timing_off = !value;
                        } else if opt.eq_ignore_ascii_case("settings") {
                            settings = true;
                        } else if opt.eq_ignore_ascii_case("wal") {
                            wal = true;
                        } else if opt.eq_ignore_ascii_case("verbose")
                            || opt.eq_ignore_ascii_case("format")
                            || opt.eq_ignore_ascii_case("summary")
                        {
                            // v7.37.22 — accept-but-no-op the remaining
                            // PG options so EXPLAIN-using clients
                            // (pgAdmin / DataGrip) don't see syntax
                            // errors. FORMAT takes a value (text /
                            // json / yaml / xml); skip the next token
                            // if it's an ident.
                            if opt.eq_ignore_ascii_case("format") {
                                if let Token::Ident(v) | Token::QuotedIdent(v) = self.peek().clone()
                                {
                                    self.advance();
                                    format = match v.to_ascii_lowercase().as_str() {
                                        "text" => crate::ast::ExplainFormat::Text,
                                        "json" => crate::ast::ExplainFormat::Json,
                                        "xml" => crate::ast::ExplainFormat::Xml,
                                        "yaml" => crate::ast::ExplainFormat::Yaml,
                                        other => {
                                            return Err(self.err(format!(
                                                "EXPLAIN (FORMAT …): unknown format {other:?}; \
                                                 supports text, json, xml, yaml"
                                            )));
                                        }
                                    };
                                }
                            } else {
                                // VERBOSE / SUMMARY take optional ON/OFF;
                                // consume if present.
                                if matches!(self.peek(), Token::On) {
                                    self.advance();
                                } else if let Token::Ident(v) | Token::QuotedIdent(v) =
                                    self.peek().clone()
                                    && (v.eq_ignore_ascii_case("off")
                                        || v.eq_ignore_ascii_case("true"))
                                {
                                    self.advance();
                                    let _ = v;
                                }
                            }
                        } else {
                            return Err(self.err(format!(
                                "unknown EXPLAIN option {opt:?}; supports ANALYZE, COSTS, BUFFERS, TIMING, SETTINGS, WAL, SUGGEST, VERBOSE, FORMAT, SUMMARY"
                            )));
                        }
                        if matches!(self.peek(), Token::Comma) {
                            self.advance();
                            continue;
                        }
                        break;
                    }
                    if !matches!(self.peek(), Token::RParen) {
                        return Err(self.err(format!(
                            "expected ')' after EXPLAIN options, got {:?}",
                            self.peek()
                        )));
                    }
                    self.advance();
                } else if let Token::Ident(s) | Token::QuotedIdent(s) = self.peek()
                    && (s.eq_ignore_ascii_case("analyze") || s.eq_ignore_ascii_case("analyse"))
                {
                    self.advance();
                    analyze = true;
                }
                let inner = self.parse_select_stmt()?;
                let Statement::Select(s) = inner else {
                    return Err(self.err(format!("EXPLAIN body must be a SELECT, got {inner:?}")));
                };
                Ok(Statement::Explain(crate::ast::ExplainStatement {
                    analyze,
                    inner: Box::new(s),
                    suggest,
                    costs_off,
                    buffers,
                    timing_off,
                    settings,
                    wal,
                    format,
                }))
            }
            Token::Create => self.parse_create_stmt(),
            Token::Insert => self.parse_insert_stmt(),
            Token::Begin => {
                self.advance();
                // v7.38 轴 4 — PG-standard `BEGIN [WORK|TRANSACTION]
                // [ISOLATION LEVEL …] [READ ONLY|WRITE]
                // [[NOT] DEFERRABLE]`. We accept the optional
                // TRANSACTION/WORK noise word and parse-and-ignore
                // trailing iso modes (parser doesn't reject the
                // syntax; the iso level is only honoured when set
                // via the dedicated `SET TRANSACTION` statement
                // until the v7.38 isolation framework lands a
                // per-TX level field on the engine).
                if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("work") || s.eq_ignore_ascii_case("transaction"))
                {
                    self.advance();
                    // Parse-and-ignore any trailing modes.
                    let _ = self.parse_isolation_level_clauses()?;
                }
                Ok(Statement::Begin)
            }
            // v7.38 轴 4 — PG-standard `START TRANSACTION …` synonym
            // for BEGIN. START is contextual in PG too; pattern-match
            // on the ident here. Iso clauses are parse-and-ignored,
            // same as BEGIN above.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("start") => {
                self.advance();
                if !matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("transaction"))
                {
                    return Err(self.err(alloc::format!(
                        "expected TRANSACTION after START, got {:?}",
                        self.peek()
                    )));
                }
                self.advance();
                let _ = self.parse_isolation_level_clauses()?;
                Ok(Statement::Begin)
            }
            Token::Commit => {
                self.advance();
                Ok(Statement::Commit)
            }
            Token::Rollback => {
                self.advance();
                // `ROLLBACK TO [SAVEPOINT] <name>` returns to that
                // savepoint without ending the transaction. Bare
                // `ROLLBACK` drops the whole TX.
                if matches!(self.peek(), Token::To) {
                    self.advance();
                    if matches!(self.peek(), Token::Savepoint) {
                        self.advance();
                    }
                    let name = self.expect_ident_like()?;
                    Ok(Statement::RollbackToSavepoint(name))
                } else {
                    Ok(Statement::Rollback)
                }
            }
            Token::Savepoint => {
                self.advance();
                let name = self.expect_ident_like()?;
                Ok(Statement::Savepoint(name))
            }
            Token::Release => {
                self.advance();
                // `RELEASE [SAVEPOINT] <name>` — the `SAVEPOINT` keyword
                // is optional in standard SQL.
                if matches!(self.peek(), Token::Savepoint) {
                    self.advance();
                }
                let name = self.expect_ident_like()?;
                Ok(Statement::ReleaseSavepoint(name))
            }
            Token::Show => {
                self.advance();
                // `SHOW TABLES` / `SHOW USERS` / `SHOW COLUMNS FROM <table>`.
                // v6.1.2 promoted TABLES to a reserved keyword (for
                // `CREATE PUBLICATION … FOR ALL TABLES`), so it now
                // arrives as `Token::Tables` rather than a bare ident.
                // USERS / COLUMNS remain bare idents.
                let target = match self.advance() {
                    Token::Tables => "tables".to_string(),
                    // v7.17.0 Phase 3.P0-59 — CREATE is a reserved
                    // keyword token; recognise it as the SHOW CREATE
                    // dispatch keyword too.
                    Token::Create => "create".to_string(),
                    // v7.17.0 Phase 3.P0-60 — INDEX is a reserved
                    // keyword too; let SHOW INDEX FROM parse.
                    Token::Index => "index".to_string(),
                    // v7.37.17 (17.6 sibling) — SHOW ALL. ALL is
                    // reserved (used in aggregate function calls);
                    // recognise it here so the parser dispatches
                    // to ShowParameter("all") — the engine returns
                    // the curated parameter inventory.
                    Token::All => "all".to_string(),
                    Token::Ident(s) | Token::QuotedIdent(s) => s.to_ascii_lowercase(),
                    other => {
                        return Err(self.err(format!(
                            "expected SHOW target, got {other:?}"
                        )));
                    }
                };
                match target.as_str() {
                    "tables" => Ok(Statement::ShowTables),
                    "users" => Ok(Statement::ShowUsers),
                    // v7.38 轴 4 — `SHOW transaction_isolation`
                    // returns the currently-selected isolation level.
                    "transaction_isolation" => Ok(Statement::ShowParameter(
                        "transaction_isolation".to_string(),
                    )),
                    // v7.17.0 Phase 3.P0-59 — MySQL `SHOW CREATE
                    // TABLE <t>` returns a 2-column row: (Table,
                    // Create Table). mysqldump emits this for every
                    // table at scrape time; without it the dump
                    // round-trip stalls.
                    // v7.17.0 Phase 3.P0-60 — MySQL `SHOW INDEXES
                    // FROM <t>` (also spelled `SHOW INDEX` and
                    // `SHOW KEYS`). admin / mysqldump probes use
                    // it to list per-table indexes.
                    "indexes" | "index" | "keys" => {
                        if !matches!(self.peek(), Token::From) {
                            return Err(self.err(format!(
                                "expected FROM after SHOW INDEXES, got {:?}",
                                self.peek()
                            )));
                        }
                        self.advance();
                        let table = self.expect_ident_like()?;
                        Ok(Statement::ShowIndexes(table))
                    }
                    // v7.17.0 Phase 3.P0-61 — MySQL `SHOW STATUS` /
                    // `SHOW VARIABLES`. Both return a 2-column row
                    // set listing server-side state; clients probe
                    // them at connect time.
                    "status" => Ok(Statement::ShowStatus),
                    "variables" => Ok(Statement::ShowVariables),
                    // v7.17.0 Phase 3.P0-62 — MySQL `SHOW PROCESSLIST`.
                    "processlist" => Ok(Statement::ShowProcesslist),
                    "create" => {
                        // SHOW CREATE TABLE / VIEW / DATABASE — only
                        // TABLE is supported in v7.17.
                        let kind = match self.advance() {
                            Token::Ident(s) | Token::QuotedIdent(s) => s,
                            Token::Table => "table".to_string(),
                            other => {
                                return Err(self.err(format!(
                                    "expected TABLE after SHOW CREATE, got {other:?}"
                                )));
                            }
                        };
                        if !kind.eq_ignore_ascii_case("table") {
                            return Err(self.err(format!(
                                "unsupported SHOW CREATE {kind:?}; v7.17 supports TABLE only"
                            )));
                        }
                        let name = self.expect_ident_like()?;
                        Ok(Statement::ShowCreateTable(name))
                    }
                    // v7.17.0 Phase 3.P0-58 — MySQL `SHOW DATABASES`
                    // (and `SHOW SCHEMAS` alias). The mysql client uses
                    // it to populate the database selector at connect
                    // time; without it `mysql -p` errors before the
                    // first user query.
                    "databases" | "schemas" => Ok(Statement::ShowDatabases),
                    // v6.1.3 — PUBLICATIONS plural is NOT a reserved
                    // keyword on its own; it lands here as a bare
                    // ident. Returning all publications + their
                    // scope summary.
                    "publications" => Ok(Statement::ShowPublications),
                    // v6.1.4 — same shape for SUBSCRIPTIONS plural.
                    "subscriptions" => Ok(Statement::ShowSubscriptions),
                    "columns" => {
                        if !matches!(self.peek(), Token::From) {
                            return Err(self.err(format!(
                                "expected FROM after SHOW COLUMNS, got {:?}",
                                self.peek()
                            )));
                        }
                        self.advance();
                        let table = self.expect_ident_like()?;
                        Ok(Statement::ShowColumns(table))
                    }
                    // v7.38 轴 4 surface — `SHOW <param>` for any
                    // remaining session / preset parameter name
                    // (server_version, search_path, client_encoding,
                    // …). The engine's ShowParameter handler does the
                    // dispatch; unrecognised names error there with
                    // a pointer to pg_settings, not at parse time —
                    // so a driver that issues `SHOW spam_setting`
                    // gets a clear runtime error instead of a
                    // confusing "unknown SHOW target".
                    other => Ok(Statement::ShowParameter(other.to_string())),
                }
            }
            // v6.1.2: `DROP` is now a reserved keyword (it dispatches
            // to DROP USER and DROP PUBLICATION today; DROP TABLE /
            // DROP INDEX are still SHOW-shaped admin ops). Pre-6.1.2
            // arrived as a bare ident; tokenising it dedicatedly
            // keeps the dispatch tree small.
            Token::Drop => {
                self.advance();
                match self.peek() {
                    // v7.37.17 (17.6 sibling) — DROP OWNED BY <role>
                    // [, ...] [CASCADE | RESTRICT]. pg_dumpall emits
                    // around DROP ROLE cleanup. SPG has no role-owner
                    // model, so consume to boundary as a no-op.
                    Token::Ident(s) | Token::QuotedIdent(s)
                        if s.eq_ignore_ascii_case("owned") =>
                    {
                        self.advance();
                        self.consume_until_statement_boundary();
                        return Ok(Statement::Empty);
                    }
                    Token::Publication => {
                        self.advance();
                        let name = self.expect_ident_or_string()?;
                        Ok(Statement::DropPublication(name))
                    }
                    Token::Subscription => {
                        self.advance();
                        let name = self.expect_ident_or_string()?;
                        Ok(Statement::DropSubscription(name))
                    }
                    Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("user") => {
                        self.advance();
                        let name = self.expect_ident_or_string()?;
                        Ok(Statement::DropUser(name))
                    }
                    // v7.12.4 — DROP TRIGGER [IF EXISTS] name ON table.
                    Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("trigger") => {
                        self.advance();
                        let if_exists = self.consume_if_exists();
                        let name = self.expect_ident_like()?;
                        // ON <table>
                        if !matches!(self.peek(), Token::On) {
                            return Err(self.err(alloc::format!(
                                "expected ON <table> after DROP TRIGGER {name:?}, got {:?}",
                                self.peek()
                            )));
                        }
                        self.advance();
                        let table = self.expect_ident_like()?;
                        Ok(Statement::DropTrigger {
                            name,
                            table,
                            if_exists,
                        })
                    }
                    // v7.12.4 — DROP FUNCTION [IF EXISTS] name [(args)].
                    // v7.12.4 ignores any optional arg-list (signature-
                    // based overload disambiguation lands in v7.12.5+).
                    Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("function") => {
                        self.advance();
                        let if_exists = self.consume_if_exists();
                        let name = self.expect_ident_like()?;
                        // Optional `()` — consume + discard.
                        if matches!(self.peek(), Token::LParen) {
                            self.advance();
                            // Skip until matching RParen, accepting any tokens (typed args we don't model yet).
                            let mut depth = 1usize;
                            while depth > 0 {
                                match self.peek() {
                                    Token::LParen => depth += 1,
                                    Token::RParen => depth -= 1,
                                    Token::Eof => {
                                        return Err(self.err(alloc::format!(
                                            "unterminated arg list in DROP FUNCTION {name:?}"
                                        )));
                                    }
                                    _ => {}
                                }
                                self.advance();
                            }
                        }
                        Ok(Statement::DropFunction { name, if_exists })
                    }
                    // v7.14.0 — DROP TABLE [IF EXISTS] name [, name…]
                    // [CASCADE|RESTRICT]. pg_dump and mysqldump both
                    // emit DROP TABLE IF EXISTS at the head of every
                    // CREATE TABLE block so re-importing a dump
                    // overwrites prior state. SPG accepts and removes
                    // matching tables; CASCADE/RESTRICT trailers
                    // accepted silently.
                    Token::Table => {
                        self.advance();
                        let if_exists = self.consume_if_exists();
                        let mut names: Vec<String> = Vec::new();
                        loop {
                            names.push(self.expect_ident_like()?);
                            if matches!(self.peek(), Token::Comma) {
                                self.advance();
                                continue;
                            }
                            break;
                        }
                        if matches!(
                            self.peek(),
                            Token::Ident(s) if s.eq_ignore_ascii_case("cascade")
                                || s.eq_ignore_ascii_case("restrict")
                        ) {
                            self.advance();
                        }
                        Ok(Statement::DropTable { names, if_exists })
                    }
                    // v7.14.0 — DROP INDEX [IF EXISTS] name
                    // [CASCADE|RESTRICT]. PG / mysqldump emit this
                    // for partial-index renames and pgvector
                    // migrations. SPG removes the matching index;
                    // IF EXISTS makes the drop idempotent.
                    Token::Index => {
                        self.advance();
                        let if_exists = self.consume_if_exists();
                        let name = self.expect_ident_like()?;
                        if matches!(
                            self.peek(),
                            Token::Ident(s) if s.eq_ignore_ascii_case("cascade")
                                || s.eq_ignore_ascii_case("restrict")
                        ) {
                            self.advance();
                        }
                        Ok(Statement::DropIndex { name, if_exists })
                    }
                    // v7.14.0 — DROP SCHEMA [IF EXISTS] name
                    // [CASCADE|RESTRICT]. SPG is single-database;
                    // v7.17.0 Phase 1.6 — DROP SCHEMA [IF EXISTS]
                    // name [, name…] [CASCADE | RESTRICT]. Real
                    // unregister (was silent no-op pre-v7.17).
                    Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("schema") => {
                        self.advance();
                        let if_exists = self.consume_if_exists();
                        let mut names = vec![self.expect_ident_like()?];
                        while matches!(self.peek(), Token::Comma) {
                            self.advance();
                            names.push(self.expect_ident_like()?);
                        }
                        if matches!(
                            self.peek(),
                            Token::Ident(s) if s.eq_ignore_ascii_case("cascade")
                                || s.eq_ignore_ascii_case("restrict")
                        ) {
                            self.advance();
                        }
                        Ok(Statement::DropSchema { names, if_exists })
                    }
                    // v7.17.0 Phase 1.4 — DROP TYPE [IF EXISTS]
                    // name [, name…] [CASCADE|RESTRICT].
                    Token::Ident(s) | Token::QuotedIdent(s)
                        if s.eq_ignore_ascii_case("type") =>
                    {
                        self.advance();
                        let if_exists = self.consume_if_exists();
                        let mut names = vec![self.expect_ident_like()?];
                        while matches!(self.peek(), Token::Comma) {
                            self.advance();
                            names.push(self.expect_ident_like()?);
                        }
                        if matches!(
                            self.peek(),
                            Token::Ident(s) if s.eq_ignore_ascii_case("cascade")
                                || s.eq_ignore_ascii_case("restrict")
                        ) {
                            self.advance();
                        }
                        Ok(Statement::DropType { names, if_exists })
                    }
                    // v7.17.0 Phase 1.5 — DROP DOMAIN [IF EXISTS]
                    // name [, name…] [CASCADE|RESTRICT].
                    Token::Ident(s) | Token::QuotedIdent(s)
                        if s.eq_ignore_ascii_case("domain") =>
                    {
                        self.advance();
                        let if_exists = self.consume_if_exists();
                        let mut names = vec![self.expect_ident_like()?];
                        while matches!(self.peek(), Token::Comma) {
                            self.advance();
                            names.push(self.expect_ident_like()?);
                        }
                        if matches!(
                            self.peek(),
                            Token::Ident(s) if s.eq_ignore_ascii_case("cascade")
                                || s.eq_ignore_ascii_case("restrict")
                        ) {
                            self.advance();
                        }
                        Ok(Statement::DropDomain { names, if_exists })
                    }
                    // v7.17.0 Phase 1.3 — DROP MATERIALIZED VIEW
                    // [IF EXISTS] name [, name…] [CASCADE|RESTRICT].
                    Token::Ident(s) | Token::QuotedIdent(s)
                        if s.eq_ignore_ascii_case("materialized") =>
                    {
                        self.advance();
                        let nxt = self.peek().clone();
                        if !matches!(&nxt, Token::Ident(s2) | Token::QuotedIdent(s2) if s2.eq_ignore_ascii_case("view"))
                        {
                            return Err(self.err(alloc::format!(
                                "expected VIEW after DROP MATERIALIZED, got {nxt:?}"
                            )));
                        }
                        self.advance();
                        let if_exists = self.consume_if_exists();
                        let mut names = vec![self.expect_ident_like()?];
                        while matches!(self.peek(), Token::Comma) {
                            self.advance();
                            names.push(self.expect_ident_like()?);
                        }
                        if matches!(
                            self.peek(),
                            Token::Ident(s) if s.eq_ignore_ascii_case("cascade")
                                || s.eq_ignore_ascii_case("restrict")
                        ) {
                            self.advance();
                        }
                        Ok(Statement::DropMaterializedView { names, if_exists })
                    }
                    // v7.17.0 Phase 1.2 — DROP VIEW [IF EXISTS]
                    // name [, name…] [CASCADE|RESTRICT].
                    Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("view") => {
                        self.advance();
                        let if_exists = self.consume_if_exists();
                        let mut names = vec![self.expect_ident_like()?];
                        while matches!(self.peek(), Token::Comma) {
                            self.advance();
                            names.push(self.expect_ident_like()?);
                        }
                        if matches!(
                            self.peek(),
                            Token::Ident(s) if s.eq_ignore_ascii_case("cascade")
                                || s.eq_ignore_ascii_case("restrict")
                        ) {
                            self.advance();
                        }
                        Ok(Statement::DropView { names, if_exists })
                    }
                    // v7.17.0 — DROP SEQUENCE [IF EXISTS] name [,name…]
                    // [CASCADE|RESTRICT]. Real removal from catalog
                    // (was a silent no-op pre-v7.17).
                    Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("sequence") => {
                        self.advance();
                        let if_exists = self.consume_if_exists();
                        let mut names = vec![self.expect_ident_like()?];
                        while matches!(self.peek(), Token::Comma) {
                            self.advance();
                            names.push(self.expect_ident_like()?);
                        }
                        if matches!(
                            self.peek(),
                            Token::Ident(s) if s.eq_ignore_ascii_case("cascade")
                                || s.eq_ignore_ascii_case("restrict")
                        ) {
                            self.advance();
                        }
                        Ok(Statement::DropSequence { names, if_exists })
                    }
                    // v7.37.17 (17.6 siblings) — DROP <target> for
                    // targets SPG doesn't natively track. pg_dump
                    // emits DROP EXTENSION / DROP TYPE / DROP DOMAIN
                    // / DROP AGGREGATE / DROP OPERATOR / DROP CAST /
                    // DROP COLLATION / DROP LANGUAGE / DROP CONVERSION
                    // / DROP TEXT SEARCH / DROP FOREIGN * / DROP
                    // SERVER / DROP MATERIALIZED VIEW / DROP EVENT
                    // TRIGGER / DROP TABLESPACE / DROP RULE / DROP
                    // POLICY / DROP LARGE OBJECT / DROP ROLE / DROP
                    // ACCESS METHOD / DROP OPERATOR CLASS/FAMILY /
                    // etc. — accept + Empty-return so pg_dump tails
                    // load through. Materialized-view drop dispatches
                    // to the existing DropTable path when the token
                    // is Materialized-View-shaped (elsewhere in
                    // this parser).
                    Token::Ident(s) | Token::QuotedIdent(s)
                        if matches!(
                            s.to_ascii_lowercase().as_str(),
                            "extension"
                                | "type"
                                | "domain"
                                | "aggregate"
                                | "operator"
                                | "cast"
                                | "collation"
                                | "language"
                                | "conversion"
                                | "text"
                                | "foreign"
                                | "server"
                                | "materialized"
                                | "event"
                                | "tablespace"
                                | "rule"
                                | "policy"
                                | "large"
                                | "role"
                                | "access"
                                | "statistics"
                                | "procedure"
                                | "routine"
                        ) =>
                    {
                        self.consume_until_statement_boundary();
                        Ok(Statement::Empty)
                    }
                    other => Err(self.err(format!(
                        "expected TABLE / INDEX / SCHEMA / SEQUENCE / USER / PUBLICATION / \
                         SUBSCRIPTION / TRIGGER / FUNCTION after DROP, got {other:?}"
                    ))),
                }
            }
            // v7.17.0 Phase 1.3 — REFRESH MATERIALIZED VIEW name [WITH [NO] DATA].
            // v7.37.19 (19.8) — `CONCURRENTLY` modifier (PG 9.4+) parsed
            // and accepted before the view name. SPG materialised
            // views re-evaluate on read (always-fresh semantics), so
            // the CONCURRENTLY-vs-serial distinction has no runtime
            // effect — the refresh body does not block readers either
            // way. Same accept-and-no-op pattern as DETACH PARTITION
            // CONCURRENTLY (16.5).
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("refresh") => {
                self.advance();
                let nxt = self.peek().clone();
                if !matches!(&nxt, Token::Ident(s2) | Token::QuotedIdent(s2) if s2.eq_ignore_ascii_case("materialized"))
                {
                    return Err(self.err(alloc::format!(
                        "expected MATERIALIZED after REFRESH, got {nxt:?}"
                    )));
                }
                self.advance();
                let nxt2 = self.peek().clone();
                if !matches!(&nxt2, Token::Ident(s2) | Token::QuotedIdent(s2) if s2.eq_ignore_ascii_case("view"))
                {
                    return Err(self.err(alloc::format!(
                        "expected VIEW after REFRESH MATERIALIZED, got {nxt2:?}"
                    )));
                }
                self.advance();
                // Optional CONCURRENTLY noise word — consumed without
                // changing semantics.
                if matches!(self.peek(), Token::Ident(s2) | Token::QuotedIdent(s2) if s2.eq_ignore_ascii_case("concurrently"))
                {
                    self.advance();
                }
                let name = self.expect_ident_like()?;
                let with_data = self.parse_optional_with_data(true)?;
                Ok(Statement::RefreshMaterializedView { name, with_data })
            }
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("update") => {
                self.advance();
                self.parse_update_after_keyword()
            }
            // v7.37.17 (17.6 sibling) — TRUNCATE [TABLE] [ONLY]
            // <name> [, ...] [RESTART IDENTITY | CONTINUE IDENTITY]
            // [CASCADE | RESTRICT]. Clears every row from each named
            // table. Parses at the top level; the engine dispatcher
            // walks Statement::Truncate.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("truncate") => {
                self.advance();
                // Optional TABLE noise word (PG accepts both).
                if matches!(self.peek(), Token::Table) {
                    self.advance();
                } else if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("table"))
                {
                    self.advance();
                }
                // Optional ONLY qualifier (skip partitions). SPG's
                // declarative partitions are always truncated
                // together, so it's an accepted no-op.
                if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("only"))
                {
                    self.advance();
                }
                // Table names (comma-separated).
                let mut tables = Vec::new();
                loop {
                    tables.push(self.expect_ident_like()?);
                    if matches!(self.peek(), Token::Comma) {
                        self.advance();
                        continue;
                    }
                    break;
                }
                // Optional RESTART IDENTITY / CONTINUE IDENTITY.
                let mut restart_identity = false;
                if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("restart"))
                {
                    self.advance();
                    if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("identity"))
                    {
                        self.advance();
                        restart_identity = true;
                    }
                } else if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("continue"))
                {
                    self.advance();
                    if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("identity"))
                    {
                        self.advance();
                    }
                }
                // Optional CASCADE / RESTRICT.
                let mut cascade = false;
                if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("cascade"))
                {
                    self.advance();
                    cascade = true;
                } else if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("restrict"))
                {
                    self.advance();
                }
                Ok(Statement::Truncate {
                    tables,
                    restart_identity,
                    cascade,
                })
            }
            // v7.37.17 (17.6 sibling) — REINDEX [(OPTION [, ...])]
            // [CONCURRENTLY] { INDEX | TABLE | SCHEMA | DATABASE |
            // SYSTEM } [IF EXISTS] <name>. SPG rebuilds indexes as
            // rows change so the index tree is always up-to-date;
            // REINDEX is a strict no-op. Accept the whole statement
            // shape to boundary for pg_dump round-trip compatibility.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("reindex") => {
                self.advance();
                self.consume_until_statement_boundary();
                Ok(Statement::Empty)
            }
            // v7.37.17 (17.6 sibling) — VACUUM [(OPTION [, ...])]
            // [FULL] [FREEZE] [VERBOSE] [ANALYZE] [<table> [(cols)]].
            // SPG has no MVCC bloat today (Phase D visibility map
            // queues with v7.38); the freezer collapses hot-tier
            // rows into cold segments automatically. VACUUM is a
            // no-op — pg_dump maintenance scripts and Discourse's
            // periodic-maintenance path both emit it.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("vacuum") => {
                self.advance();
                self.consume_until_statement_boundary();
                Ok(Statement::Empty)
            }
            // v7.37.17 (17.6 sibling) — CLUSTER [VERBOSE] <table>
            // [USING <index>] / CLUSTER (VERBOSE) <table> USING
            // <index>. PG stores rows in physical order matching
            // an index; SPG's hot-tier is append-only + cold-tier
            // is segment-frozen, so clustering has no persistent
            // effect. Accept-and-no-op for pg_dump compat.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("cluster") => {
                self.advance();
                self.consume_until_statement_boundary();
                Ok(Statement::Empty)
            }
            // v7.37.17 (17.6 sibling) — LISTEN / NOTIFY / UNLISTEN.
            // PG's async notification channels. SPG has no LISTEN/
            // NOTIFY delivery machinery yet; accept the syntax so
            // pg_dump / migration scripts that reference channels
            // don't fail at parse. Notifications get dropped on
            // the floor at execute time (Statement::Empty).
            Token::Ident(s) | Token::QuotedIdent(s)
                if s.eq_ignore_ascii_case("listen")
                    || s.eq_ignore_ascii_case("notify")
                    || s.eq_ignore_ascii_case("unlisten") =>
            {
                self.advance();
                self.consume_until_statement_boundary();
                Ok(Statement::Empty)
            }
            // v7.37.17 (17.6 sibling) — LOCK [TABLE] [ONLY] <table>
            // [IN <mode> MODE] [NOWAIT]. SPG's engine holds a
            // process-wide write lock today; explicit LOCK has no
            // effect. Accept-and-no-op for pg_dump / migration
            // compat.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("lock") => {
                self.advance();
                self.consume_until_statement_boundary();
                Ok(Statement::Empty)
            }
            // v7.37.17 (17.6 sibling) — CHECKPOINT. Forces a WAL
            // durability marker + snapshot in PG. SPG has WAL
            // checkpointing on a byte / time schedule (v7.37.10
            // 60s / 4 MiB defaults); explicit CHECKPOINT is a
            // no-op today (an internal `SPG_FORCE_CHECKPOINT`
            // path could tie into this on future customer demand).
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("checkpoint") => {
                self.advance();
                self.consume_until_statement_boundary();
                Ok(Statement::Empty)
            }
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("delete") => {
                self.advance();
                self.parse_delete_after_keyword()
            }
            // v6.0.4: ALTER INDEX <name> REBUILD [WITH (encoding = ...)].
            // ALTER is not a reserved keyword in the lexer — handled
            // as a bare ident here.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("alter") => {
                self.advance();
                self.parse_alter_after_keyword()
            }
            // v6.1.7: WAIT FOR WAL POSITION <pos> [WITH TIMEOUT <ms>].
            // WAIT / POSITION / TIMEOUT are bare idents — no lexer
            // additions needed.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("wait") => {
                self.advance();
                self.parse_wait_after_keyword()
            }
            // v6.2.0: ANALYZE [<table>]. ANALYZE is a bare ident.
            // Bare ANALYZE → analyse every user table; ANALYZE
            // <name> → re-stats one. The argument is an optional
            // ident (or quoted ident); anything else is a parse
            // error.
            // v6.7.3 — `COMPACT COLD SEGMENTS`. No arguments, no
            // `WHERE` filter (carved out per V6_7_DESIGN.md
            // STABILITY). Lex order: identifier "compact" → "cold"
            // → "segments". Anything else after `COMPACT` is a
            // parse error.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("compact") => {
                self.advance();
                let next = self.peek().clone();
                let cold = match next {
                    Token::Ident(s) | Token::QuotedIdent(s) => s,
                    _ => {
                        return Err(
                            self.err(format!("expected COLD after COMPACT, got {:?}", self.peek()))
                        );
                    }
                };
                if !cold.eq_ignore_ascii_case("cold") {
                    return Err(self.err(format!("expected COLD after COMPACT, got {cold:?}")));
                }
                self.advance();
                let next = self.peek().clone();
                let segments = match next {
                    Token::Ident(s) | Token::QuotedIdent(s) => s,
                    _ => {
                        return Err(self.err(format!(
                            "expected SEGMENTS after COMPACT COLD, got {:?}",
                            self.peek()
                        )));
                    }
                };
                if !segments.eq_ignore_ascii_case("segments") {
                    return Err(self.err(format!(
                        "expected SEGMENTS after COMPACT COLD, got {segments:?}"
                    )));
                }
                self.advance();
                Ok(Statement::CompactColdSegments)
            }
            // v7.17.0 Phase 3.P0-42 — SQL:2003 / PG 15+ MERGE.
            // Parsed as a case-insensitive identifier since MERGE
            // isn't a reserved lexer keyword (collides with the
            // mysqldump `ALGORITHM = MERGE` view clause if it
            // were); the inner parser drives the rest of the
            // surface (USING / ON / WHEN [NOT] MATCHED / THEN).
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("merge") => {
                self.advance();
                self.parse_merge_after_keyword()
            }
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("analyze") => {
                self.advance();
                let target = match self.peek() {
                    Token::Eof | Token::Semicolon => None,
                    Token::Ident(_) | Token::QuotedIdent(_) => {
                        Some(self.expect_ident_like()?)
                    }
                    other => {
                        return Err(self.err(format!(
                            "expected table name or end of statement after ANALYZE, got {other:?}"
                        )));
                    }
                };
                Ok(Statement::Analyze(target))
            }
            // v7.12.1 — `SET <name> [TO|=] <value>`. The
            // `default_text_search_config` parameter is consumed
            // by the FTS function dispatcher; other parameter
            // names are recorded but treated as a no-op so PG
            // dump output loads.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("set") => {
                self.advance();
                // PG allows `SET LOCAL` / `SET SESSION` qualifiers
                // — accept and ignore. MySQL adds `SET GLOBAL` too
                // (and the alias `SET @@global.name = …` which the
                // SessionVar path handles).
                if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("local") || s.eq_ignore_ascii_case("session") || s.eq_ignore_ascii_case("global"))
                {
                    self.advance();
                }
                // v7.14.0 — MySQL `SET NAMES <charset> [COLLATE
                // <collation>]` — change the connection client
                // charset. SPG stores UTF-8 always and orders
                // bytewise; accept as a no-op.
                if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("names"))
                {
                    self.advance();
                    // Charset ident-or-string.
                    if matches!(
                        self.peek(),
                        Token::Ident(_) | Token::QuotedIdent(_) | Token::String(_)
                    ) {
                        self.advance();
                    }
                    // Optional `COLLATE <name>`.
                    if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("collate"))
                    {
                        self.advance();
                        if matches!(
                            self.peek(),
                            Token::Ident(_) | Token::QuotedIdent(_) | Token::String(_)
                        ) {
                            self.advance();
                        }
                    }
                    return Ok(Statement::Empty);
                }
                // v7.37.17 (17.6 sibling) — PG `SET ROLE
                // { NONE | DEFAULT | <role_name> }`. pg_dump preamble
                // uses this to switch to the object owner before
                // recreating tables. SPG has no role system so this
                // is a no-op.
                if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("role"))
                {
                    self.advance(); // ROLE
                    match self.peek().clone() {
                        Token::Default
                        | Token::String(_)
                        | Token::Ident(_)
                        | Token::QuotedIdent(_) => {
                            self.advance();
                        }
                        _ => {}
                    }
                    return Ok(Statement::Empty);
                }
                // v7.37.17 (17.6 sibling) — PG `SET SESSION
                // CHARACTERISTICS AS TRANSACTION <mode>` (per PG
                // ISO SQL surface). pg_dump prepends this to fix
                // the isolation level for the restore session. SPG
                // defaults to READ COMMITTED and doesn't yet honor
                // session-set isolation across statements — accept
                // and no-op. SET (LOCAL/SESSION) TRANSACTION AS ...
                // per-tx form is handled elsewhere.
                if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("characteristics"))
                {
                    self.advance(); // CHARACTERISTICS
                    self.consume_until_statement_boundary();
                    return Ok(Statement::Empty);
                }
                // v7.37.17 (17.6 sibling) — PG `SET CONSTRAINTS
                // { ALL | <name>[, ...] } { DEFERRED | IMMEDIATE }`.
                // pg_dump emits this to control the deferrability of
                // FK / UNIQUE constraints across a bulk restore. SPG
                // has no deferrable-constraint machinery today; the
                // FK checker is strict-immediate. Accept-and-no-op
                // for pg_dump round-trip compatibility.
                if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("constraints"))
                {
                    self.advance(); // CONSTRAINTS
                    self.consume_until_statement_boundary();
                    return Ok(Statement::Empty);
                }
                // v7.16.2 — PG `SET [SESSION] AUTHORIZATION
                // { DEFAULT | '<role>' | <ident> }` (mailrs
                // round-10 A.1). pg_dump preamble emits the
                // `DEFAULT` form to reset session authorization;
                // SPG has no role system so this is a strict
                // no-op. PG also accepts `RESET SESSION
                // AUTHORIZATION` (handled by the RESET parser
                // elsewhere). Reference:
                // <https://www.postgresql.org/docs/current/sql-set-session-authorization.html>
                if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("authorization"))
                {
                    self.advance(); // AUTHORIZATION
                    match self.peek().clone() {
                        Token::Default => {
                            self.advance();
                        }
                        Token::String(_)
                        | Token::Ident(_)
                        | Token::QuotedIdent(_) => {
                            self.advance();
                        }
                        other => {
                            return Err(self.err(alloc::format!(
                                "expected DEFAULT / '<role>' / <ident> after SET SESSION AUTHORIZATION, got {other:?}"
                            )));
                        }
                    }
                    return Ok(Statement::Empty);
                }
                // v7.38 轴 4 — `SET [SESSION] TRANSACTION
                // ISOLATION LEVEL { READ COMMITTED | READ
                // UNCOMMITTED | REPEATABLE READ | SERIALIZABLE }
                // [, READ {ONLY|WRITE}] [, [NOT] DEFERRABLE]`.
                // PG-standard surface. v7.37.8 accepts the syntax
                // and tracks the selected level on
                // `Engine::current_isolation_level()`; the actual
                // MVCC / SSI semantics implementation lands in
                // the 轴 4 isolation framework (separate train).
                // PG itself maps READ UNCOMMITTED to READ COMMITTED
                // internally; SPG behaves the same (effectively
                // READ COMMITTED at every level today).
                if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("transaction"))
                {
                    self.advance(); // TRANSACTION
                    let level = self.parse_isolation_level_clauses()?;
                    return Ok(Statement::SetTransaction { isolation: level });
                }
                // v7.14.0 — MySQL `SET CHARACTER SET <charset>`
                // alias — same accept-as-no-op as SET NAMES.
                if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("character"))
                    && matches!(self.tokens.get(self.pos + 1), Some(Token::Ident(s)) if s.eq_ignore_ascii_case("set"))
                {
                    self.advance(); // CHARACTER
                    self.advance(); // SET
                    if matches!(
                        self.peek(),
                        Token::Ident(_) | Token::QuotedIdent(_) | Token::String(_)
                    ) {
                        self.advance();
                    }
                    return Ok(Statement::Empty);
                }
                // v7.14.0 — multi-assignment form
                // `SET a = 1, b = 2, …`. Single-assignment is the
                // 1-element case. Each LHS may be a regular ident
                // or a SessionVar (`@VAR` / `@@VAR`).
                let mut pairs: Vec<(String, crate::ast::SetValue)> = Vec::new();
                loop {
                    let lhs = match self.peek().clone() {
                        Token::SessionVar(s) => {
                            self.advance();
                            s
                        }
                        Token::Ident(_) | Token::QuotedIdent(_) => self.parse_set_param_name()?,
                        other => {
                            return Err(self.err(format!(
                                "expected parameter name after SET, got {other:?}"
                            )));
                        }
                    };
                    // Accept either `=` or the bare `TO` keyword.
                    match self.peek() {
                        Token::Eq => {
                            self.advance();
                        }
                        Token::To => {
                            self.advance();
                        }
                        other => {
                            return Err(self.err(format!(
                                "expected `=` or TO after SET {lhs}, got {other:?}"
                            )));
                        }
                    }
                    let value = self.parse_set_value()?;
                    pairs.push((lhs, value));
                    if matches!(self.peek(), Token::Comma) {
                        self.advance();
                        continue;
                    }
                    break;
                }
                if pairs.len() == 1 {
                    let (name, value) = pairs.into_iter().next().unwrap();
                    Ok(Statement::SetParameter { name, value })
                } else {
                    Ok(Statement::SetParameterList(pairs))
                }
            }
            // v7.12.1 — `RESET <name>` / `RESET ALL`.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("reset") => {
                self.advance();
                match self.peek().clone() {
                    Token::All => {
                        self.advance();
                        Ok(Statement::ResetParameter(None))
                    }
                    Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("all") => {
                        self.advance();
                        Ok(Statement::ResetParameter(None))
                    }
                    _ => {
                        let name = self.parse_set_param_name()?;
                        Ok(Statement::ResetParameter(Some(name)))
                    }
                }
            }
            other => Err(self.err(format!(
                "expected SELECT / CREATE / DROP / INSERT / UPDATE / DELETE / ALTER / BEGIN / COMMIT / \
                 ROLLBACK / SAVEPOINT / RELEASE / SHOW at start of statement, got {other:?}"
            ))),
        }
    }

    fn parse_create_stmt(&mut self) -> Result<Statement, ParseError> {
        debug_assert!(matches!(self.peek(), Token::Create));
        self.advance();
        match self.peek() {
            Token::Table => self.parse_create_table_stmt_after_create(),
            Token::Index => self.parse_create_index_stmt_after_create(false),
            // v7.9.29 — `CREATE UNIQUE INDEX … [WHERE pred]`.
            // The `UNIQUE` modifier turns a partial index into a
            // partial-uniqueness invariant (only rows matching the
            // WHERE predicate are checked for duplicates). mailrs
            // K1 (3 hits: email_templates default, calendar_events
            // master, calendar_events instance).
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("unique") => {
                self.advance();
                if !matches!(self.peek(), Token::Index) {
                    return Err(self.err(alloc::format!(
                        "expected INDEX after CREATE UNIQUE, got {:?}",
                        self.peek()
                    )));
                }
                self.parse_create_index_stmt_after_create(true)
            }
            Token::Publication => {
                self.advance();
                self.parse_create_publication_after_keyword()
            }
            Token::Subscription => {
                self.advance();
                self.parse_create_subscription_after_keyword()
            }
            // v4.1: CREATE USER 'name' WITH PASSWORD 'pw' [ROLE 'role'].
            // USER isn't a reserved keyword — we look for the bare
            // identifier so the lexer doesn't have to grow a token.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("user") => {
                self.advance();
                self.parse_create_user_after_keyword()
            }
            // v7.9.15 — `CREATE EXTENSION [IF NOT EXISTS] <name>
            // [WITH SCHEMA …] [VERSION '…'] [CASCADE]` as a
            // no-op. mailrs follow-up F3.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("extension") => {
                self.advance();
                self.parse_create_extension_after_keyword()
            }
            // v7.12.4 — `CREATE [OR REPLACE] FUNCTION …` and
            // `CREATE [OR REPLACE] TRIGGER …`. `OR REPLACE` is
            // optional; absorb it here and forward to the
            // per-kind parsers with the flag. OR is a reserved
            // keyword token.
            Token::Or => {
                self.advance();
                let next = self.peek();
                let (Token::Ident(s2) | Token::QuotedIdent(s2)) = next else {
                    return Err(self.err(alloc::format!(
                        "expected REPLACE after CREATE OR, got {next:?}"
                    )));
                };
                if !s2.eq_ignore_ascii_case("replace") {
                    return Err(self.err(alloc::format!(
                        "expected REPLACE after CREATE OR, got {s2:?}"
                    )));
                }
                self.advance();
                self.parse_create_function_or_trigger_after_or_replace(true)
            }
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("function") => {
                self.advance();
                self.parse_create_function_after_keyword(false)
            }
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("trigger") => {
                self.advance();
                self.parse_create_trigger_after_keyword(false)
            }
            // v7.17.0 — CREATE [TEMPORARY] SEQUENCE …
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("sequence") => {
                self.advance();
                self.parse_create_sequence_after_keyword(false)
            }
            // v7.17.0 Phase 1.2 — CREATE [TEMPORARY] VIEW …
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("view") => {
                self.advance();
                self.parse_create_view_after_keyword(false, false, false)
            }
            // v7.17.0 Phase 2.6 — MySQL view prefix clauses
            // `ALGORITHM = {UNDEFINED|MERGE|TEMPTABLE}` /
            // `DEFINER = <user>` / `SQL SECURITY {DEFINER|INVOKER}`
            // appear (in any order) between `CREATE` and `VIEW` in
            // every mysqldump-emitted view. Pre-2.6 the parser
            // rejected the prefix and the customer's whole view
            // backup failed on the first view. The hints are pure
            // planner / permission metadata; SPG's view-rewrite
            // path is semantically equivalent for all three
            // algorithms in v7.17 (TEMPTABLE differs only in
            // perf for huge views — out of v7.17 scope), and
            // DEFINER / SQL SECURITY are pure single-user
            // permissioning that SPG ignores by design.
            Token::Ident(s) | Token::QuotedIdent(s)
                if s.eq_ignore_ascii_case("algorithm")
                    || s.eq_ignore_ascii_case("definer")
                    || s.eq_ignore_ascii_case("sql") =>
            {
                self.consume_mysql_view_prefix()?;
                // After absorbing ALGORITHM / DEFINER / SQL SECURITY
                // (in any order, in any combination), the next
                // keyword must be VIEW. mysqldump never emits these
                // prefixes on non-view statements.
                let next = self.peek().clone();
                if matches!(&next, Token::Ident(s2) | Token::QuotedIdent(s2)
                    if s2.eq_ignore_ascii_case("view"))
                {
                    self.advance();
                    self.parse_create_view_after_keyword(false, false, false)
                } else {
                    Err(self.err(alloc::format!(
                        "expected VIEW after MySQL view prefix (ALGORITHM/DEFINER/SQL SECURITY), got {next:?}"
                    )))
                }
            }
            // v7.17.0 Phase 1.4 — CREATE TYPE name AS ENUM (…).
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("type") => {
                self.advance();
                self.parse_create_type_after_keyword()
            }
            // v7.17.0 Phase 1.5 — CREATE DOMAIN name AS base
            // [DEFAULT expr] [NOT NULL] [CHECK (expr)]*.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("domain") => {
                self.advance();
                self.parse_create_domain_after_keyword()
            }
            // v7.17.0 Phase 1.6 — CREATE SCHEMA [IF NOT EXISTS]
            // name [AUTHORIZATION user]. Real catalog registry
            // (was silent-no-op'd pre-v7.17).
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("schema") => {
                self.advance();
                let if_not_exists = self.parse_if_not_exists();
                let name = self.expect_ident_like()?;
                // Optional `AUTHORIZATION <user>` trailer — accepted,
                // ignored (single-user catalog).
                if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                    if s.eq_ignore_ascii_case("authorization"))
                {
                    self.advance();
                    let _ = self.expect_ident_like()?;
                }
                Ok(Statement::CreateSchema { name, if_not_exists })
            }
            // v7.17.0 Phase 1.3 — CREATE MATERIALIZED VIEW …
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("materialized") => {
                self.advance();
                let next = self.peek().clone();
                if matches!(&next, Token::Ident(s2) | Token::QuotedIdent(s2) if s2.eq_ignore_ascii_case("view"))
                {
                    self.advance();
                    self.parse_create_materialized_view_after_keyword()
                } else {
                    Err(self.err(alloc::format!(
                        "expected VIEW after CREATE MATERIALIZED, got {next:?}"
                    )))
                }
            }
            Token::Ident(s) | Token::QuotedIdent(s)
                if s.eq_ignore_ascii_case("temporary") || s.eq_ignore_ascii_case("temp") =>
            {
                self.advance();
                // TEMPORARY/TEMP followed by SEQUENCE / VIEW.
                let next = self.peek().clone();
                if matches!(&next, Token::Ident(s2) | Token::QuotedIdent(s2) if s2.eq_ignore_ascii_case("sequence"))
                {
                    self.advance();
                    self.parse_create_sequence_after_keyword(true)
                } else if matches!(&next, Token::Ident(s2) | Token::QuotedIdent(s2) if s2.eq_ignore_ascii_case("view"))
                {
                    self.advance();
                    self.parse_create_view_after_keyword(false, false, true)
                } else {
                    // TEMP TABLE etc — consume to boundary as noop for now.
                    self.consume_until_statement_boundary();
                    Ok(Statement::Empty)
                }
            }
            // v7.17.0 Phase 4.2 — MySQL `CREATE PROCEDURE name (…)
            // BEGIN <body> END`. The body may reference `@var`
            // session variables, SET statements, internal `;`
            // terminators, etc. SPG has no procedure runtime, so
            // consume the whole `CREATE PROCEDURE … END` block as
            // a no-op so mysqldump scripts that include stored
            // routines load through. The matching-END consumer
            // tracks BEGIN/END nesting depth to handle nested
            // BEGIN blocks correctly.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("procedure") => {
                self.consume_mysql_routine_body();
                Ok(Statement::Empty)
            }
            // v7.14.0 — pg_dump / mysqldump emit
            // `CREATE SCHEMA / VIEW / MATERIALIZED VIEW /
            // TYPE / DOMAIN / DATABASE / ROLE / POLICY / OPERATOR`.
            // SPG is single-schema / single-database; these have
            // no behavioural effect, so consume + return Empty.
            // v7.17.0 NOTE: SEQUENCE / VIEW / MATERIALIZED VIEW /
            // TYPE / DOMAIN / SCHEMA were here pre-v7.17; all
            // moved up to real parser branches. DATABASE / ROLE /
            // POLICY / OPERATOR stay no-op forever
            // (single-database, hardcoded roles).
            Token::Ident(s) | Token::QuotedIdent(s)
                if matches!(
                    s.to_ascii_lowercase().as_str(),
                    "database"
                        | "role"
                        | "policy"
                        | "operator"
                        | "cast"
                        | "rule"
                        | "aggregate"
                        | "language"
                        | "collation"
                        | "conversion"
                        // v7.17.0 Phase 8 (audit N6) — rarely-
                        // emitted pg_dump shapes that should
                        // load through without a parser error.
                        // SPG has no planner statistics catalog,
                        // no event-trigger hooks, no foreign-
                        // data-wrapper infrastructure; consume
                        // + return Empty.
                        | "statistics"
                        | "event"
                        | "foreign"
                        // v7.37.17 (17.6 siblings) — additional CREATE
                        // targets pg_dump / operator install scripts
                        // may emit that SPG has no matching machinery
                        // for. Consume + Empty-return.
                        | "text"
                        | "server"
                        | "tablespace"
                        | "access"
                        | "large"
                ) =>
            {
                self.consume_until_statement_boundary();
                Ok(Statement::Empty)
            }
            other => Err(self.err(format!(
                "expected TABLE / INDEX / USER / EXTENSION / PUBLICATION / SUBSCRIPTION / FUNCTION / TRIGGER / SEQUENCE / SCHEMA / VIEW / TYPE / DOMAIN [OR REPLACE …] after CREATE, got {other:?}"
            ))),
        }
    }

    /// v7.12.4 — `CREATE OR REPLACE` already consumed; the next
    /// keyword decides whether we parse a function or trigger
    /// body. PG accepts other `OR REPLACE`-able objects (VIEW,
    /// PROCEDURE) — those land in later releases.
    fn parse_create_function_or_trigger_after_or_replace(
        &mut self,
        or_replace: bool,
    ) -> Result<Statement, ParseError> {
        let tok = self.peek();
        let (Token::Ident(s) | Token::QuotedIdent(s)) = tok else {
            return Err(self.err(alloc::format!(
                "expected FUNCTION / TRIGGER / VIEW after CREATE OR REPLACE, got {tok:?}"
            )));
        };
        if s.eq_ignore_ascii_case("function") {
            self.advance();
            self.parse_create_function_after_keyword(or_replace)
        } else if s.eq_ignore_ascii_case("trigger") {
            self.advance();
            self.parse_create_trigger_after_keyword(or_replace)
        } else if s.eq_ignore_ascii_case("view") {
            // v7.17.0 Phase 1.2 — CREATE OR REPLACE VIEW name AS SELECT …
            self.advance();
            self.parse_create_view_after_keyword(or_replace, false, false)
        } else if s.eq_ignore_ascii_case("temporary") || s.eq_ignore_ascii_case("temp") {
            // CREATE OR REPLACE TEMPORARY VIEW … (rare but legal).
            self.advance();
            let nxt = self.peek().clone();
            if matches!(&nxt, Token::Ident(n) | Token::QuotedIdent(n) if n.eq_ignore_ascii_case("view"))
            {
                self.advance();
                self.parse_create_view_after_keyword(or_replace, false, true)
            } else {
                Err(self.err(alloc::format!(
                    "expected VIEW after CREATE OR REPLACE TEMPORARY, got {nxt:?}"
                )))
            }
        } else {
            Err(self.err(alloc::format!(
                "expected FUNCTION / TRIGGER / VIEW after CREATE OR REPLACE, got {s:?}"
            )))
        }
    }

    /// v7.9.15 — accept and discard `CREATE EXTENSION` DDL.
    /// SPG doesn't have a registry; pgvector / similar are
    /// either builtin (VECTOR(N) ↔ pgvector) or n/a. Parsing
    /// the syntax lets dual-target schemas keep the line.
    fn parse_create_extension_after_keyword(&mut self) -> Result<Statement, ParseError> {
        // Optional `IF NOT EXISTS`.
        self.consume_if_not_exists();
        let name = self.expect_ident_like()?;
        // Drain optional WITH SCHEMA <ident> / VERSION '<v>' /
        // CASCADE / FROM '<v>' clauses; we don't model them.
        loop {
            match self.peek() {
                Token::Ident(s) if s.eq_ignore_ascii_case("with") => {
                    self.advance();
                    continue;
                }
                Token::Ident(s) if s.eq_ignore_ascii_case("schema") => {
                    self.advance();
                    let _ = self.expect_ident_like()?;
                    continue;
                }
                Token::Ident(s) if s.eq_ignore_ascii_case("version") => {
                    self.advance();
                    // String or ident literal.
                    let _ = self.advance();
                    continue;
                }
                Token::Ident(s) if s.eq_ignore_ascii_case("from") => {
                    self.advance();
                    let _ = self.advance();
                    continue;
                }
                Token::Ident(s) if s.eq_ignore_ascii_case("cascade") => {
                    self.advance();
                    continue;
                }
                _ => break,
            }
        }
        Ok(Statement::CreateExtension(name))
    }

    /// v7.12.4 — body of `CREATE [OR REPLACE] FUNCTION`. The
    /// `[OR REPLACE]` flag (and the `FUNCTION` keyword) have
    /// already been consumed by the caller. Grammar accepted:
    ///
    ///   name `(` arg-list `)`
    ///   `RETURNS` return-type
    ///   [ `LANGUAGE` ident ]
    ///   `AS` $$ body $$
    ///   [ `LANGUAGE` ident ]
    ///
    /// Either `LANGUAGE` position is allowed; PG accepts both.
    fn parse_create_function_after_keyword(
        &mut self,
        or_replace: bool,
    ) -> Result<Statement, ParseError> {
        let name = self.expect_ident_like()?;
        // Argument list. v7.12.4 commonly sees the empty `()`
        // (trigger functions); typed args parse and round-trip
        // but the executor only invokes nullary functions.
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(alloc::format!(
                "expected '(' after function name {name:?}, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let args = self.parse_function_arg_list()?;
        // RETURNS clause.
        let tok = self.peek();
        let (Token::Ident(s) | Token::QuotedIdent(s)) = tok else {
            return Err(self.err(alloc::format!(
                "expected RETURNS after function arg list, got {tok:?}"
            )));
        };
        if !s.eq_ignore_ascii_case("returns") {
            return Err(self.err(alloc::format!(
                "expected RETURNS after function arg list, got {s:?}"
            )));
        }
        self.advance();
        let returns = self.parse_function_return()?;
        // Optional LANGUAGE clause (PG also accepts after AS — we'll
        // re-check after the body too).
        let mut language: Option<String> = self.parse_optional_language()?;
        // `AS` followed by a $$-quoted body (lexer already
        // collapses both `$$…$$` and `$tag$…$tag$` to a single
        // Token::String). AS is a reserved keyword (Token::As).
        if !matches!(self.peek(), Token::As) {
            return Err(self.err(alloc::format!(
                "expected AS before function body, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let body_text = match self.peek() {
            Token::String(s) => {
                let body = s.clone();
                self.advance();
                body
            }
            other => {
                return Err(self.err(alloc::format!(
                    "expected $$-quoted function body after AS, got {other:?}"
                )));
            }
        };
        // Trailing optional LANGUAGE clause (the other PG position).
        if language.is_none() {
            language = self.parse_optional_language()?;
        }
        let language = language.unwrap_or_else(|| String::from("sql"));
        // PL/pgSQL bodies get structure-parsed. Other languages
        // (or PL/pgSQL bodies the v7.12.4 parser doesn't yet
        // recognise) round-trip as Raw text — the executor errors
        // when invoked with a clear unsupported message.
        let body = if language.eq_ignore_ascii_case("plpgsql") {
            match parse_plpgsql_body(&body_text) {
                Ok(block) => FunctionBody::PlPgSql(block),
                // Best-effort: if the body parser doesn't yet
                // support a construct used inside, fall back to
                // raw — keeps `CREATE FUNCTION` itself working
                // (catalogue accepts), executor errors on
                // invocation only.
                Err(_) => FunctionBody::Raw(body_text),
            }
        } else {
            FunctionBody::Raw(body_text)
        };
        Ok(Statement::CreateFunction(CreateFunctionStatement {
            name,
            or_replace,
            args,
            returns,
            language,
            body,
        }))
    }

    /// Closing `)`-terminated argument list. v7.12.4 commonly
    /// sees the empty `()`; typed args round-trip but the
    /// executor (yet) doesn't invoke them.
    fn parse_function_arg_list(&mut self) -> Result<Vec<FunctionArg>, ParseError> {
        let mut args: Vec<FunctionArg> = Vec::new();
        if matches!(self.peek(), Token::RParen) {
            self.advance();
            return Ok(args);
        }
        loop {
            // Optional `IN` / `OUT` / `INOUT` mode keyword. IN is
            // a reserved token; OUT / INOUT are bare idents.
            let mode = if matches!(self.peek(), Token::In) {
                self.advance();
                FunctionArgMode::In
            } else if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("out"))
            {
                self.advance();
                FunctionArgMode::Out
            } else if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("inout"))
            {
                self.advance();
                FunctionArgMode::InOut
            } else {
                FunctionArgMode::In
            };
            // Optional name. The next token is either a name
            // (followed by a type ident) or the type itself.
            // Disambiguate by peeking ahead: if the token after
            // the next ident is also an ident, we treat the
            // first as the name.
            let (name, ty_token) = {
                let first = self.expect_ident_like()?;
                // Peek next: if it's an ident (i.e. a type
                // name) the `first` was the arg name.
                match self.peek() {
                    Token::Ident(_) | Token::QuotedIdent(_) => {
                        let ty = self.expect_ident_like()?;
                        (Some(first), ty)
                    }
                    _ => (None, first),
                }
            };
            // Type — try to map to ColumnTypeName, else Raw.
            let ty = match map_type_ident_to_column_type_name(&ty_token) {
                Some(t) => FunctionArgType::Typed(t),
                None => FunctionArgType::Raw(ty_token),
            };
            args.push(FunctionArg { mode, name, ty });
            match self.peek() {
                Token::Comma => {
                    self.advance();
                    continue;
                }
                Token::RParen => {
                    self.advance();
                    return Ok(args);
                }
                other => {
                    return Err(self.err(alloc::format!(
                        "expected , or ) in function arg list, got {other:?}"
                    )));
                }
            }
        }
    }

    fn parse_function_return(&mut self) -> Result<FunctionReturn, ParseError> {
        let ident = self.expect_ident_like()?;
        if ident.eq_ignore_ascii_case("trigger") {
            return Ok(FunctionReturn::Trigger);
        }
        if ident.eq_ignore_ascii_case("void") {
            return Ok(FunctionReturn::Void);
        }
        match map_type_ident_to_column_type_name(&ident) {
            Some(t) => Ok(FunctionReturn::Type(t)),
            None => Ok(FunctionReturn::Other(ident)),
        }
    }

    fn parse_optional_language(&mut self) -> Result<Option<String>, ParseError> {
        match self.peek() {
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("language") => {
                self.advance();
                let lang = self.expect_ident_like()?;
                Ok(Some(lang.to_ascii_lowercase()))
            }
            _ => Ok(None),
        }
    }

    /// v7.17.0 Phase 1.5 — body of `CREATE DOMAIN name AS
    /// base_type [DEFAULT expr] [NOT NULL | NULL] [CHECK
    /// (expr)]*`. The `DOMAIN` keyword has already been
    /// consumed. PG allows the trailing constraints in any
    /// order; we approximate with a small loop.
    fn parse_create_domain_after_keyword(&mut self) -> Result<Statement, ParseError> {
        let name = self.expect_ident_like()?;
        // Optional `AS`.
        if matches!(self.peek(), Token::As) {
            self.advance();
        }
        let base_type = self.parse_column_type_name()?;
        let mut default: Option<Expr> = None;
        let mut not_null = false;
        let mut checks: Vec<Expr> = Vec::new();
        loop {
            match self.peek() {
                Token::Default => {
                    if default.is_some() {
                        return Err(self.err("DOMAIN DEFAULT specified twice".into()));
                    }
                    self.advance();
                    default = Some(self.parse_expr(0)?);
                }
                Token::Not => {
                    self.advance();
                    if !matches!(self.peek(), Token::Null) {
                        return Err(self.err(alloc::format!(
                            "expected NULL after NOT in DOMAIN, got {:?}",
                            self.peek()
                        )));
                    }
                    self.advance();
                    not_null = true;
                }
                Token::Null => {
                    self.advance();
                    // NULL after a NOT NULL is contradictory, but
                    // PG accepts bare NULL as the default-nullable
                    // marker. No-op.
                }
                Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("check") => {
                    self.advance();
                    if !matches!(self.peek(), Token::LParen) {
                        return Err(self.err(alloc::format!(
                            "expected '(' after CHECK in DOMAIN, got {:?}",
                            self.peek()
                        )));
                    }
                    self.advance();
                    let expr = self.parse_expr(0)?;
                    if !matches!(self.peek(), Token::RParen) {
                        return Err(self.err(alloc::format!(
                            "expected ')' after CHECK expr, got {:?}",
                            self.peek()
                        )));
                    }
                    self.advance();
                    checks.push(expr);
                }
                // CONSTRAINT <name> CHECK (…) — PG accepts a name
                // prefix on the constraint; we drop the name and
                // recurse into the constraint parsing.
                Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("constraint") => {
                    self.advance();
                    let _ = self.expect_ident_like()?;
                }
                _ => break,
            }
        }
        Ok(Statement::CreateDomain(crate::ast::CreateDomainStatement {
            name,
            base_type,
            default,
            not_null,
            checks,
        }))
    }

    /// v7.17.0 Phase 1.4 — body of `CREATE TYPE name AS ENUM
    /// ('a', 'b', …)`. The `TYPE` keyword has already been
    /// consumed.
    fn parse_create_type_after_keyword(&mut self) -> Result<Statement, ParseError> {
        let name = self.expect_ident_like()?;
        // Required `AS`.
        if !matches!(self.peek(), Token::As) {
            return Err(self.err(alloc::format!(
                "expected AS after CREATE TYPE {name:?}, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        // v7.37.x (ζ-B composite Phase 1) — `AS (` is the composite-
        // type shape: `CREATE TYPE foo AS (a INT, b TEXT)`. Branch
        // on the next token: `(` = composite, ident `ENUM` = enum.
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            let mut fields: Vec<(String, ColumnTypeName)> = Vec::new();
            loop {
                let field_name = self.expect_ident_like()?;
                let field_type = self.parse_column_type_name()?;
                fields.push((field_name, field_type));
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                    continue;
                }
                if matches!(self.peek(), Token::RParen) {
                    self.advance();
                    break;
                }
                return Err(self.err(alloc::format!(
                    "expected , or ) in composite field list, got {:?}",
                    self.peek()
                )));
            }
            if fields.is_empty() {
                return Err(self.err("CREATE TYPE … AS (…) must declare at least one field".into()));
            }
            return Ok(Statement::CreateType(crate::ast::CreateTypeStatement {
                name,
                kind: crate::ast::TypeKind::Composite { fields },
            }));
        }
        // Required `ENUM` ident.
        let kind_ident = match self.peek().clone() {
            Token::Ident(s) | Token::QuotedIdent(s) => s,
            other => {
                return Err(self.err(alloc::format!(
                    "expected ENUM or '(' after CREATE TYPE {name:?} AS, got {other:?}"
                )));
            }
        };
        if !kind_ident.eq_ignore_ascii_case("enum") {
            return Err(self.err(alloc::format!(
                "Phase 1.4 only supports ENUM or composite '(…)'; got {kind_ident:?}"
            )));
        }
        self.advance();
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(alloc::format!(
                "expected '(' after ENUM, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let mut labels: Vec<String> = Vec::new();
        loop {
            match self.peek().clone() {
                Token::String(s) => {
                    self.advance();
                    labels.push(s);
                }
                other => {
                    return Err(
                        self.err(alloc::format!("expected enum label string, got {other:?}"))
                    );
                }
            }
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            if matches!(self.peek(), Token::RParen) {
                self.advance();
                break;
            }
            return Err(self.err(alloc::format!(
                "expected , or ) in ENUM label list, got {:?}",
                self.peek()
            )));
        }
        if labels.is_empty() {
            return Err(self.err("CREATE TYPE … AS ENUM must declare at least one label".into()));
        }
        Ok(Statement::CreateType(crate::ast::CreateTypeStatement {
            name,
            kind: crate::ast::TypeKind::Enum { labels },
        }))
    }

    /// v7.17.0 Phase 1.3 — body of `CREATE MATERIALIZED VIEW
    /// [IF NOT EXISTS] name [(col, …)] AS <SELECT …> [WITH [NO] DATA]`.
    /// The `CREATE MATERIALIZED VIEW` keywords have already been
    /// consumed.
    fn parse_create_materialized_view_after_keyword(&mut self) -> Result<Statement, ParseError> {
        let if_not_exists = self.parse_if_not_exists();
        let name = self.expect_ident_like()?;
        let mut columns: Vec<String> = Vec::new();
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            loop {
                let c = self.expect_ident_like()?;
                columns.push(c);
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                    continue;
                }
                if matches!(self.peek(), Token::RParen) {
                    self.advance();
                    break;
                }
                return Err(self.err(alloc::format!(
                    "expected , or ) in MATERIALIZED VIEW column list, got {:?}",
                    self.peek()
                )));
            }
        }
        if !matches!(self.peek(), Token::As) {
            return Err(self.err(alloc::format!(
                "expected AS <SELECT …> after CREATE MATERIALIZED VIEW {name:?}, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let body_stmt = self.parse_select_stmt()?;
        let Statement::Select(body) = body_stmt else {
            return Err(self.err(alloc::format!(
                "CREATE MATERIALIZED VIEW body must be a SELECT, got {body_stmt:?}"
            )));
        };
        // Optional trailing `WITH [NO] DATA`.
        let with_data = self.parse_optional_with_data(true)?;
        Ok(Statement::CreateMaterializedView(
            crate::ast::CreateMaterializedViewStatement {
                name,
                if_not_exists,
                columns,
                body,
                with_data,
            },
        ))
    }

    /// v7.17.0 Phase 1.3 — `WITH [NO] DATA` trailer.
    /// `default_when_absent` is what to return if the tail is
    /// missing (CREATE defaults to WITH DATA, REFRESH defaults to
    /// WITH DATA).
    fn parse_optional_with_data(&mut self, default_when_absent: bool) -> Result<bool, ParseError> {
        let save = self.pos;
        // `WITH` is an Ident (not reserved in the lexer).
        let is_with = match self.peek() {
            Token::Ident(s) | Token::QuotedIdent(s) => s.eq_ignore_ascii_case("with"),
            _ => false,
        };
        if !is_with {
            return Ok(default_when_absent);
        }
        self.advance();
        // Optional `NO`.
        let mut with_data = true;
        let is_no = match self.peek() {
            Token::Ident(s) | Token::QuotedIdent(s) => s.eq_ignore_ascii_case("no"),
            _ => false,
        };
        if is_no {
            self.advance();
            with_data = false;
        }
        // Required `DATA` ident.
        let is_data = match self.peek() {
            Token::Ident(s) | Token::QuotedIdent(s) => s.eq_ignore_ascii_case("data"),
            _ => false,
        };
        if is_data {
            self.advance();
            Ok(with_data)
        } else {
            // Caller's WITH wasn't WITH-DATA — rewind so the outer
            // parser can interpret it.
            self.pos = save;
            Ok(default_when_absent)
        }
    }

    /// v7.17.0 Phase 1.2 — body of `CREATE [OR REPLACE]
    /// [TEMPORARY] VIEW [IF NOT EXISTS] name [(col, …)] AS <SELECT>`.
    /// All keyword prefixes have already been consumed; the flags
    /// say which were present.
    fn parse_create_view_after_keyword(
        &mut self,
        or_replace: bool,
        _materialized_unused: bool,
        temporary: bool,
    ) -> Result<Statement, ParseError> {
        let if_not_exists = self.parse_if_not_exists();
        let name = self.expect_ident_like()?;
        // Optional `(col, col, …)` rename list.
        let mut columns: Vec<String> = Vec::new();
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            loop {
                let c = self.expect_ident_like()?;
                columns.push(c);
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                    continue;
                }
                if matches!(self.peek(), Token::RParen) {
                    self.advance();
                    break;
                }
                return Err(self.err(alloc::format!(
                    "expected , or ) in VIEW column list, got {:?}",
                    self.peek()
                )));
            }
        }
        // Required `AS`.
        if !matches!(self.peek(), Token::As) {
            return Err(self.err(alloc::format!(
                "expected AS <SELECT …> after CREATE VIEW {name:?}, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        // Body: a regular SELECT statement.
        let body_stmt = self.parse_select_stmt()?;
        let Statement::Select(body) = body_stmt else {
            return Err(self.err(alloc::format!(
                "CREATE VIEW body must be a SELECT statement, got {body_stmt:?}"
            )));
        };
        Ok(Statement::CreateView(crate::ast::CreateViewStatement {
            name,
            or_replace,
            if_not_exists,
            temporary,
            columns,
            body,
        }))
    }

    /// v7.17.0 — body of `CREATE [TEMPORARY] SEQUENCE`. The
    /// `[TEMPORARY]` and `SEQUENCE` tokens have already been
    /// consumed; `temporary` carries whether TEMPORARY was seen.
    fn parse_create_sequence_after_keyword(
        &mut self,
        temporary: bool,
    ) -> Result<Statement, ParseError> {
        let if_not_exists = self.parse_if_not_exists();
        let name = self.expect_ident_like()?;
        // Optional `AS data_type`.
        let data_type = if matches!(self.peek(), Token::As) {
            self.advance();
            Some(self.parse_sequence_data_type()?)
        } else {
            None
        };
        let options = self.parse_sequence_options(/* allow_restart = */ false)?;
        Ok(Statement::CreateSequence(
            crate::ast::CreateSequenceStatement {
                name,
                if_not_exists,
                temporary,
                data_type,
                options,
            },
        ))
    }

    /// v7.17.0 — body of `ALTER SEQUENCE`. The `ALTER` keyword has
    /// already been consumed; this is reached after `SEQUENCE`.
    fn parse_alter_sequence_after_keyword(&mut self) -> Result<Statement, ParseError> {
        let if_exists = self.parse_if_exists();
        let name = self.expect_ident_like()?;
        let options = self.parse_sequence_options(/* allow_restart = */ true)?;
        Ok(Statement::AlterSequence(
            crate::ast::AlterSequenceStatement {
                name,
                if_exists,
                options,
            },
        ))
    }

    fn parse_sequence_data_type(&mut self) -> Result<crate::ast::SequenceDataType, ParseError> {
        let kw = self.expect_ident_like()?;
        match kw.to_ascii_lowercase().as_str() {
            "smallint" | "int2" => Ok(crate::ast::SequenceDataType::SmallInt),
            "integer" | "int" | "int4" => Ok(crate::ast::SequenceDataType::Int),
            "bigint" | "int8" => Ok(crate::ast::SequenceDataType::BigInt),
            other => Err(self.err(alloc::format!(
                "expected SMALLINT / INTEGER / BIGINT after SEQUENCE AS, got {other:?}"
            ))),
        }
    }

    fn parse_sequence_options(
        &mut self,
        allow_restart: bool,
    ) -> Result<crate::ast::SequenceOptions, ParseError> {
        use crate::ast::{SeqBound, SequenceOptions, SequenceOwnedBy};
        let mut opts = SequenceOptions::default();
        #[allow(clippy::while_let_loop)]
        loop {
            // Match an ident; stop at any non-ident token (sentinel,
            // semicolon, end of statement).
            let kw_lc = match self.peek() {
                Token::Ident(s) | Token::QuotedIdent(s) => s.to_ascii_lowercase(),
                _ => break,
            };
            match kw_lc.as_str() {
                "increment" => {
                    self.advance();
                    // Optional BY.
                    if matches!(self.peek(), Token::By) {
                        self.advance();
                    }
                    opts.increment = Some(self.expect_signed_int()?);
                }
                "minvalue" => {
                    self.advance();
                    opts.min_value = Some(SeqBound::Value(self.expect_signed_int()?));
                }
                "maxvalue" => {
                    self.advance();
                    opts.max_value = Some(SeqBound::Value(self.expect_signed_int()?));
                }
                "no" => {
                    self.advance();
                    let what = self.expect_ident_like()?;
                    match what.to_ascii_lowercase().as_str() {
                        "minvalue" => opts.min_value = Some(SeqBound::NoBound),
                        "maxvalue" => opts.max_value = Some(SeqBound::NoBound),
                        "cycle" => opts.cycle = Some(false),
                        other => {
                            return Err(self.err(alloc::format!(
                                "expected MINVALUE / MAXVALUE / CYCLE after NO, got {other:?}"
                            )));
                        }
                    }
                }
                "start" => {
                    self.advance();
                    // Optional WITH.
                    if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                        if s.eq_ignore_ascii_case("with"))
                    {
                        self.advance();
                    }
                    opts.start = Some(self.expect_signed_int()?);
                }
                "restart" if allow_restart => {
                    self.advance();
                    // Optional WITH n; bare RESTART means restart at START.
                    let mut with_val: Option<i64> = None;
                    if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                        if s.eq_ignore_ascii_case("with"))
                    {
                        self.advance();
                        with_val = Some(self.expect_signed_int()?);
                    } else if matches!(self.peek(), Token::Integer(_) | Token::Minus) {
                        with_val = Some(self.expect_signed_int()?);
                    }
                    opts.restart = Some(with_val);
                }
                "cache" => {
                    self.advance();
                    opts.cache = Some(self.expect_signed_int()?);
                }
                "cycle" => {
                    self.advance();
                    opts.cycle = Some(true);
                }
                "owned" => {
                    self.advance();
                    // BY is a reserved Token::By; accept either form.
                    match self.peek() {
                        Token::By => {
                            self.advance();
                        }
                        Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("by") => {
                            self.advance();
                        }
                        other => {
                            return Err(
                                self.err(alloc::format!("expected BY after OWNED, got {other:?}"))
                            );
                        }
                    }
                    // OWNED BY {NONE | tab.col}. Read just one ident
                    // (NOT expect_ident_like which would auto-strip
                    // a schema prefix and consume the `.col` we need).
                    let first = match self.advance() {
                        Token::Ident(s) | Token::QuotedIdent(s) => s,
                        other => {
                            return Err(self.err(alloc::format!(
                                "expected identifier or NONE after OWNED BY, got {other:?}"
                            )));
                        }
                    };
                    if first.eq_ignore_ascii_case("none") {
                        opts.owned_by = Some(SequenceOwnedBy::None);
                    } else if matches!(self.peek(), Token::Dot) {
                        self.advance();
                        let second = match self.advance() {
                            Token::Ident(s) | Token::QuotedIdent(s) => s,
                            other => {
                                return Err(self.err(alloc::format!(
                                    "expected column name after OWNED BY {first}., got {other:?}"
                                )));
                            }
                        };
                        // v7.17 dump-compat fix — pg_dump emits
                        // OWNED BY clauses as
                        // `schema.table.column` (three segments).
                        // If a third `.<ident>` follows, treat the
                        // first ident as schema (drop it; SPG is
                        // single-schema) and the middle / last
                        // pair as table.column. Otherwise it's
                        // the two-segment form table.column.
                        if matches!(self.peek(), Token::Dot) {
                            self.advance();
                            let third = match self.advance() {
                                Token::Ident(s) | Token::QuotedIdent(s) => s,
                                other => {
                                    return Err(self.err(alloc::format!(
                                        "expected column name after OWNED BY {first}.{second}., got {other:?}"
                                    )));
                                }
                            };
                            let _ = first; // schema prefix discarded
                            opts.owned_by = Some(SequenceOwnedBy::Column {
                                table: second,
                                column: third,
                            });
                        } else {
                            opts.owned_by = Some(SequenceOwnedBy::Column {
                                table: first,
                                column: second,
                            });
                        }
                    } else {
                        return Err(self.err(alloc::format!(
                            "expected table.column or NONE after OWNED BY, got {first:?}"
                        )));
                    }
                }
                _ => break,
            }
        }
        Ok(opts)
    }

    fn expect_signed_int(&mut self) -> Result<i64, ParseError> {
        let neg = if matches!(self.peek(), Token::Minus) {
            self.advance();
            true
        } else {
            false
        };
        match self.peek() {
            Token::Integer(n) => {
                let v = *n;
                self.advance();
                Ok(if neg { -v } else { v })
            }
            other => Err(self.err(alloc::format!("expected signed integer, got {other:?}"))),
        }
    }

    /// v7.17.0 Phase 3.1 — absorb `[NOT] DEFERRABLE [INITIALLY
    /// {DEFERRED | IMMEDIATE}]` constraint-timing clauses. Each
    /// clause is fully accepted and discarded — SPG always runs
    /// constraint checks immediately (single-writer model). The
    /// loop allows DEFERRABLE and the INITIALLY suffix to appear
    /// in either order (per the SQL spec they're independent),
    /// though pg_dump always emits them in the canonical
    /// `[NOT] DEFERRABLE INITIALLY {DEFERRED|IMMEDIATE}` shape.
    /// Stops at the first token that isn't part of the clause.
    fn consume_optional_deferrable_clauses(&mut self) -> Result<(), ParseError> {
        loop {
            // Bare `DEFERRABLE` (Phase 3.1 — was hard-error pre-3.1).
            if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("deferrable")) {
                self.advance();
                self.consume_optional_initially_clause()?;
                continue;
            }
            // `NOT DEFERRABLE` — already worked pre-3.1.
            if matches!(self.peek(), Token::Not) {
                let look = self.tokens.get(self.pos + 1);
                if matches!(look, Some(Token::Ident(s)) if s.eq_ignore_ascii_case("deferrable")) {
                    self.advance(); // NOT
                    self.advance(); // DEFERRABLE
                    self.consume_optional_initially_clause()?;
                    continue;
                }
                break;
            }
            // Standalone `INITIALLY {DEFERRED|IMMEDIATE}` — PG
            // accepts this without a leading [NOT] DEFERRABLE
            // (the timing keyword alone). pg_dump occasionally
            // emits it on FK constraints that inherit timing.
            if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("initially")) {
                self.consume_optional_initially_clause()?;
                continue;
            }
            break;
        }
        Ok(())
    }

    /// Helper for [`consume_optional_deferrable_clauses`]. When the
    /// next token is `INITIALLY`, consume it plus the required
    /// `DEFERRED` | `IMMEDIATE` trailer. No-op otherwise.
    fn consume_optional_initially_clause(&mut self) -> Result<(), ParseError> {
        if !matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("initially")) {
            return Ok(());
        }
        self.advance(); // INITIALLY
        match self.advance() {
            Token::Ident(s)
                if s.eq_ignore_ascii_case("deferred") || s.eq_ignore_ascii_case("immediate") =>
            {
                Ok(())
            }
            other => Err(self.err(alloc::format!(
                "expected DEFERRED or IMMEDIATE after INITIALLY, got {other:?}"
            ))),
        }
    }

    /// v7.17.0 Phase 4.2 — consume a MySQL `CREATE PROCEDURE` body
    /// in its entirety so the parser returns Empty without
    /// touching the runtime. The CREATE+PROCEDURE keywords are
    /// already consumed; this swallows everything from the
    /// procedure name through the matching `END`, including
    /// nested `BEGIN`/`END` blocks, internal `;` terminators
    /// (DELIMITER `//` makes the script splitter forward the
    /// whole block as one statement), `@var` session-variable
    /// references, and the trailing terminator.
    ///
    /// Tracks nesting depth so:
    ///   BEGIN
    ///     IF cond THEN
    ///       BEGIN ... END;
    ///     END IF;
    ///   END
    /// terminates at the outer END.
    fn consume_mysql_routine_body(&mut self) {
        // Outer skeleton: name, (...), optional clauses, BEGIN
        // <body> END [;]. Scan for the first BEGIN — anything
        // before it is signature decoration we don't care about.
        // Once inside BEGIN, count up on BEGIN, down on END.
        let mut depth: i32 = 0;
        let mut started = false;
        loop {
            match self.peek().clone() {
                Token::Begin => {
                    self.advance();
                    depth += 1;
                    started = true;
                }
                Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("end") => {
                    self.advance();
                    if started {
                        depth -= 1;
                        if depth <= 0 {
                            // Optional trailing ident (`END IF`,
                            // `END LOOP`, `END WHILE`, `END CASE`,
                            // `END label_name`) — eat the next
                            // ident if present so we don't
                            // mistake `END IF;` for the outer
                            // close.
                            if matches!(self.peek(), Token::Ident(_) | Token::QuotedIdent(_)) {
                                // If the next token is one of the
                                // PL/SQL block-closer keywords,
                                // the END belongs to an inner
                                // block; bump depth back up.
                                let is_inner_close = matches!(
                                    self.peek(),
                                    Token::Ident(s) | Token::QuotedIdent(s)
                                        if matches!(
                                            s.to_ascii_lowercase().as_str(),
                                            "if" | "loop" | "while" | "case" | "repeat"
                                        )
                                );
                                if is_inner_close {
                                    self.advance();
                                    depth += 1;
                                    continue;
                                }
                            }
                            // Eat optional trailing `;`.
                            if matches!(self.peek(), Token::Semicolon) {
                                self.advance();
                            }
                            return;
                        }
                    }
                }
                Token::Eof => return,
                _ => {
                    self.advance();
                }
            }
        }
    }

    /// v7.17.0 Phase 2.6 — absorb the MySQL view-prefix clauses
    /// that appear between `CREATE` and `VIEW` in mysqldump output:
    ///
    /// * `ALGORITHM = {UNDEFINED|MERGE|TEMPTABLE}`
    /// * `DEFINER = <user>`  (user may be a quoted string, a bare
    ///   ident, or `ident @ ident-or-quoted-string` host form)
    /// * `SQL SECURITY {DEFINER|INVOKER}`
    ///
    /// Each clause may appear at most once but in any order.
    /// The hints are pure planner / permission metadata that
    /// SPG's view-rewrite engine handles uniformly; we accept
    /// and discard. Returns `Ok(())` once a non-clause token is
    /// peeked (the caller then checks for the `VIEW` keyword).
    fn consume_mysql_view_prefix(&mut self) -> Result<(), ParseError> {
        loop {
            match self.peek().clone() {
                Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("algorithm") => {
                    self.advance(); // ALGORITHM
                    // Optional `=`. MySQL spec requires it but be
                    // generous.
                    if matches!(self.peek(), Token::Eq) {
                        self.advance();
                    }
                    // UNDEFINED / MERGE / TEMPTABLE — accept any
                    // bare ident; unknown values still parse so
                    // future MySQL versions don't break.
                    if matches!(
                        self.peek(),
                        Token::Ident(_) | Token::QuotedIdent(_) | Token::String(_)
                    ) {
                        self.advance();
                    }
                }
                Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("definer") => {
                    self.advance(); // DEFINER
                    if matches!(self.peek(), Token::Eq) {
                        self.advance();
                    }
                    // User: quoted string, ident, OR ident @ host
                    // (host may itself be quoted or bare).
                    match self.peek().clone() {
                        Token::String(_) | Token::Ident(_) | Token::QuotedIdent(_) => {
                            self.advance();
                            // Optional `@host`.
                            if matches!(self.peek(), Token::At) {
                                self.advance();
                                if matches!(
                                    self.peek(),
                                    Token::Ident(_) | Token::QuotedIdent(_) | Token::String(_)
                                ) {
                                    self.advance();
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("sql") => {
                    // `SQL SECURITY {DEFINER|INVOKER}`. Only honoured
                    // when followed by SECURITY — the dispatcher must
                    // not consume a bare `SQL` token (it's not a
                    // legal CREATE prefix on its own).
                    let save = self.pos;
                    self.advance(); // SQL
                    if matches!(self.peek(), Token::Ident(s2) | Token::QuotedIdent(s2)
                        if s2.eq_ignore_ascii_case("security"))
                    {
                        self.advance(); // SECURITY
                        // DEFINER / INVOKER trailing ident.
                        if matches!(self.peek(), Token::Ident(_) | Token::QuotedIdent(_)) {
                            self.advance();
                        }
                    } else {
                        // Not a SQL SECURITY clause — roll back and
                        // bail; the caller will error out cleanly.
                        self.pos = save;
                        return Ok(());
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn parse_if_not_exists(&mut self) -> bool {
        if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("if"))
        {
            let save = self.pos;
            self.advance();
            if matches!(self.peek(), Token::Not) {
                self.advance();
                if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("exists"))
                {
                    self.advance();
                    return true;
                }
            }
            self.pos = save;
        }
        false
    }

    fn parse_if_exists(&mut self) -> bool {
        if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("if"))
        {
            let save = self.pos;
            self.advance();
            if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("exists"))
            {
                self.advance();
                return true;
            }
            self.pos = save;
        }
        false
    }

    /// v7.12.4 — body of `CREATE [OR REPLACE] TRIGGER`. The
    /// `[OR REPLACE]` flag and the `TRIGGER` keyword have already
    /// been consumed.
    fn parse_create_trigger_after_keyword(
        &mut self,
        or_replace: bool,
    ) -> Result<Statement, ParseError> {
        let name = self.expect_ident_like()?;
        let timing = {
            let ident = self.expect_ident_like()?;
            if ident.eq_ignore_ascii_case("before") {
                TriggerTiming::Before
            } else if ident.eq_ignore_ascii_case("after") {
                TriggerTiming::After
            } else if ident.eq_ignore_ascii_case("instead") {
                let next = self.expect_ident_like()?;
                if !next.eq_ignore_ascii_case("of") {
                    return Err(self.err(alloc::format!(
                        "expected OF after INSTEAD in trigger timing, got {next:?}"
                    )));
                }
                TriggerTiming::InsteadOf
            } else {
                return Err(self.err(alloc::format!(
                    "expected BEFORE / AFTER / INSTEAD OF in trigger timing, got {ident:?}"
                )));
            }
        };
        // Events: INSERT [ OR UPDATE [ OR DELETE [ OR TRUNCATE ] ] ].
        // OR is a reserved keyword token (Token::Or), not an Ident.
        // v7.13.0 — after an UPDATE event we may optionally see
        // `OF col, col, …` (mailrs round-5 G7). Columns are
        // captured into `update_columns` once across the whole
        // events list; multiple `UPDATE OF` clauses are rejected.
        let mut events: Vec<TriggerEvent> = Vec::new();
        let mut update_columns: Vec<String> = Vec::new();
        let (first_ev, first_cols) = self.parse_trigger_event_with_optional_of()?;
        events.push(first_ev);
        if !first_cols.is_empty() {
            update_columns = first_cols;
        }
        while matches!(self.peek(), Token::Or) {
            self.advance();
            let (ev, cols) = self.parse_trigger_event_with_optional_of()?;
            events.push(ev);
            if !cols.is_empty() {
                if !update_columns.is_empty() {
                    return Err(
                        self.err("CREATE TRIGGER: `UPDATE OF cols` may appear at most once".into())
                    );
                }
                update_columns = cols;
            }
        }
        // ON <table>
        let tok = self.peek();
        let Token::On = tok else {
            return Err(self.err(alloc::format!(
                "expected ON after trigger events, got {tok:?}"
            )));
        };
        self.advance();
        let table = self.expect_ident_like()?;
        // FOR EACH ROW / FOR EACH STATEMENT. FOR is a reserved
        // keyword (Token::For); EACH / ROW / STATEMENT are bare
        // idents.
        if !matches!(self.peek(), Token::For) {
            return Err(self.err(alloc::format!(
                "expected FOR EACH ROW / STATEMENT, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let for_each = {
            let e = self.expect_ident_like()?;
            if !e.eq_ignore_ascii_case("each") {
                return Err(self.err(alloc::format!("expected EACH after FOR, got {e:?}")));
            }
            let unit = self.expect_ident_like()?;
            if unit.eq_ignore_ascii_case("row") {
                TriggerForEach::Row
            } else if unit.eq_ignore_ascii_case("statement") {
                TriggerForEach::Statement
            } else {
                return Err(self.err(alloc::format!(
                    "expected ROW / STATEMENT after FOR EACH, got {unit:?}"
                )));
            }
        };
        // EXECUTE FUNCTION/PROCEDURE name(...)
        let exec = self.expect_ident_like()?;
        if !exec.eq_ignore_ascii_case("execute") {
            return Err(self.err(alloc::format!(
                "expected EXECUTE FUNCTION/PROCEDURE in CREATE TRIGGER, got {exec:?}"
            )));
        }
        let fn_or_proc = self.expect_ident_like()?;
        if !(fn_or_proc.eq_ignore_ascii_case("function")
            || fn_or_proc.eq_ignore_ascii_case("procedure"))
        {
            return Err(self.err(alloc::format!(
                "expected FUNCTION / PROCEDURE after EXECUTE, got {fn_or_proc:?}"
            )));
        }
        let function = self.expect_ident_like()?;
        // Optional empty arg list `()`.
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            if !matches!(self.peek(), Token::RParen) {
                return Err(self.err(alloc::format!(
                    "v7.12.4 trigger function calls take no args; got {:?}",
                    self.peek()
                )));
            }
            self.advance();
        }
        Ok(Statement::CreateTrigger(CreateTriggerStatement {
            name,
            or_replace,
            timing,
            events,
            table,
            for_each,
            function,
            update_columns,
        }))
    }

    /// v7.13.0 — parse one trigger event, then optionally consume
    /// `OF col, col, …` after `UPDATE` (mailrs round-5 G7). Other
    /// events (INSERT/DELETE/TRUNCATE) don't accept the OF tail.
    fn parse_trigger_event_with_optional_of(
        &mut self,
    ) -> Result<(TriggerEvent, Vec<String>), ParseError> {
        let ev = self.parse_trigger_event()?;
        if !matches!(ev, TriggerEvent::Update) {
            return Ok((ev, Vec::new()));
        }
        // `OF` is a bare ident.
        if !matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("of")) {
            return Ok((ev, Vec::new()));
        }
        self.advance(); // OF
        let mut cols: Vec<String> = Vec::new();
        loop {
            cols.push(self.expect_ident_like()?);
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        if cols.is_empty() {
            return Err(
                self.err("CREATE TRIGGER: `UPDATE OF` requires at least one column name".into())
            );
        }
        Ok((ev, cols))
    }

    /// v7.12.4 — `BEGIN stmt; stmt; … END[;]` PL/pgSQL block.
    /// v7.12.6 — optional `DECLARE var TYPE [:= init];` prelude
    /// before `BEGIN`, and IF / RAISE / embedded SQL statements
    /// inside the body.
    /// Called by [`parse_plpgsql_body`] after the body's tokens
    /// have been lexed into this temporary parser.
    pub(crate) fn parse_plpgsql_block(&mut self) -> Result<PlPgSqlBlock, ParseError> {
        // v7.12.6 — optional DECLARE prelude.
        let declarations = if matches!(
            self.peek(),
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("declare")
        ) {
            self.advance();
            self.parse_plpgsql_declare_block()?
        } else {
            Vec::new()
        };
        // BEGIN keyword (PL/pgSQL — distinct from the SQL
        // `BEGIN` transaction-start, but we can reuse the
        // reserved Token::Begin since the body is a separate
        // lex/parse context).
        if !matches!(self.peek(), Token::Begin) {
            return Err(self.err(alloc::format!(
                "expected BEGIN at start of plpgsql block, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let statements = self.parse_plpgsql_stmt_list_until_end()?;
        // v7.37.20 (20.10) — optional EXCEPTION clause between the
        // body's last statement and the trailing END. When present
        // it's a series of `WHEN <cond> [OR <cond>]* THEN <body>`
        // arms terminated by END.
        let exception_handlers = if matches!(
            self.peek(),
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("exception")
        ) {
            self.advance();
            self.parse_plpgsql_exception_handlers()?
        } else {
            Vec::new()
        };
        Ok(PlPgSqlBlock {
            declarations,
            statements,
            exception_handlers,
        })
    }

    /// v7.37.20 (20.10) — parse EXCEPTION handlers `WHEN <cond>
    /// [OR <cond>]* THEN <body>` sequence up to the trailing END.
    fn parse_plpgsql_exception_handlers(
        &mut self,
    ) -> Result<Vec<crate::ast::ExceptionHandler>, ParseError> {
        let mut out: Vec<crate::ast::ExceptionHandler> = Vec::new();
        loop {
            // Stop at END — the block-level trailing END LOOP / END;
            // is handled by the caller.
            if matches!(
                self.peek(),
                Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("end")
            ) {
                return Ok(out);
            }
            // WHEN <cond> [OR <cond>]* THEN <body>
            if !matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("when"))
            {
                return Err(self.err(alloc::format!(
                    "expected WHEN or END inside EXCEPTION clause, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            let mut conditions: Vec<String> = Vec::new();
            conditions.push(self.expect_ident_like()?);
            while matches!(self.peek(), Token::Or) {
                self.advance();
                conditions.push(self.expect_ident_like()?);
            }
            let then_kw = self.expect_ident_like()?;
            if !then_kw.eq_ignore_ascii_case("then") {
                return Err(self.err(alloc::format!(
                    "expected THEN after WHEN condition list, got {then_kw:?}"
                )));
            }
            let body = self.parse_plpgsql_stmt_list_until_end()?;
            out.push(crate::ast::ExceptionHandler { conditions, body });
        }
    }

    /// v7.12.6 — parse the `DECLARE ... [var TYPE [:= init];]+`
    /// prelude. Caller has already consumed `DECLARE`. We stop
    /// reading entries when we hit `BEGIN`.
    fn parse_plpgsql_declare_block(&mut self) -> Result<Vec<PlPgSqlDeclare>, ParseError> {
        let mut out: Vec<PlPgSqlDeclare> = Vec::new();
        loop {
            if matches!(self.peek(), Token::Begin) {
                return Ok(out);
            }
            let name = self.expect_ident_like()?;
            // v7.37.20 (20.7) — type inference: if the next token is
            // `:=` or `=` (no explicit type), infer from the default
            // expression. Otherwise the ident that follows is the
            // declared type.
            //
            // v7.37.20 (20.8) — `<table>.<col>%TYPE` / `<table>%ROWTYPE`
            // (PG-standard). SPG parse-accepts and treats identically
            // to inference — the eventual runtime value determines
            // the local's type, which is faithful to how SPG handles
            // untyped locals today (see 20.7). Full compile-time
            // catalog lookup queues with v7.40 PL/pgSQL epic.
            let ty = if matches!(self.peek(), Token::ColonEq | Token::Eq) {
                // Sentinel: `FunctionArgType::Raw("_infer_")` tells the
                // downstream declaration walker to type the local by
                // the runtime type of the default expression.
                FunctionArgType::Raw("_infer_".into())
            } else {
                let ty_token = self.expect_ident_like()?;
                // Detect `<ident>[.<ident>][%TYPE | %ROWTYPE]`:
                // consume optional `.<ident>` qualifier + `%<KW>`
                // suffix. Both qualifier and suffix map to _infer_.
                if matches!(self.peek(), Token::Dot) {
                    self.advance();
                    let _ = self.expect_ident_like()?;
                }
                if matches!(self.peek(), Token::Percent) {
                    self.advance();
                    // Consume the trailing TYPE / ROWTYPE ident.
                    let _ = self.expect_ident_like()?;
                    FunctionArgType::Raw("_infer_".into())
                } else {
                    match map_type_ident_to_column_type_name(&ty_token) {
                        Some(t) => FunctionArgType::Typed(t),
                        None => FunctionArgType::Raw(ty_token),
                    }
                }
            };
            let default = match self.peek() {
                Token::ColonEq => {
                    self.advance();
                    Some(self.parse_expr(0)?)
                }
                Token::Eq => {
                    // PL/pgSQL also accepts `=` for the
                    // DECLARE default (PG treats them the same
                    // in this position).
                    self.advance();
                    Some(self.parse_expr(0)?)
                }
                _ => None,
            };
            // Mandatory `;` between declarations.
            if !matches!(self.peek(), Token::Semicolon) {
                return Err(self.err(alloc::format!(
                    "expected ; after DECLARE entry for {name:?}, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            out.push(PlPgSqlDeclare { name, ty, default });
        }
    }

    /// v7.12.6 — parse PL/pgSQL statements up to (and consuming)
    /// the terminating `END;` (or `END IF;` etc — handled by the
    /// per-construct sub-parsers). Used by both the outer block
    /// and the IF/ELSE branch bodies.
    fn parse_plpgsql_stmt_list_until_end(&mut self) -> Result<Vec<PlPgSqlStmt>, ParseError> {
        let mut statements: Vec<PlPgSqlStmt> = Vec::new();
        loop {
            // Allow trailing semicolons + END.
            while matches!(self.peek(), Token::Semicolon) {
                self.advance();
            }
            // END / ELSE / ELSIF / EXCEPTION — handled by the caller.
            if matches!(
                self.peek(),
                Token::Ident(s) | Token::QuotedIdent(s)
                    if s.eq_ignore_ascii_case("end")
                        || s.eq_ignore_ascii_case("else")
                        || s.eq_ignore_ascii_case("elsif")
                        || s.eq_ignore_ascii_case("elseif")
                        || s.eq_ignore_ascii_case("exception")
                        || s.eq_ignore_ascii_case("when")
            ) {
                return Ok(statements);
            }
            // Otherwise: one statement, then expect `;` or
            // a block-terminator keyword.
            let stmt = self.parse_plpgsql_stmt()?;
            statements.push(stmt);
            match self.peek() {
                Token::Semicolon => {
                    self.advance();
                }
                Token::Ident(s) | Token::QuotedIdent(s)
                    if s.eq_ignore_ascii_case("end")
                        || s.eq_ignore_ascii_case("else")
                        || s.eq_ignore_ascii_case("elsif")
                        || s.eq_ignore_ascii_case("elseif")
                        || s.eq_ignore_ascii_case("exception")
                        || s.eq_ignore_ascii_case("when") =>
                {
                    // Final statement of the block without `;`.
                }
                other => {
                    return Err(self.err(alloc::format!(
                        "expected ; or END/ELSE/ELSIF after plpgsql statement, got {other:?}"
                    )));
                }
            }
        }
    }

    fn parse_plpgsql_stmt(&mut self) -> Result<PlPgSqlStmt, ParseError> {
        // RETURN keyword?
        if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("return"))
        {
            self.advance();
            return self.parse_plpgsql_return();
        }
        // v7.12.6 — IF block.
        if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("if"))
        {
            self.advance();
            return self.parse_plpgsql_if();
        }
        // v7.37.20 (20.6) — FOR <var> IN EXECUTE <string_expr> LOOP.
        // Detected by peeking that token pos+3 is Ident("execute").
        if matches!(self.peek(), Token::For)
            && matches!(
                self.tokens.get(self.pos + 1),
                Some(Token::Ident(_) | Token::QuotedIdent(_))
            )
            && matches!(self.tokens.get(self.pos + 2), Some(Token::In))
            && matches!(
                self.tokens.get(self.pos + 3),
                Some(Token::Ident(s) | Token::QuotedIdent(s)) if s.eq_ignore_ascii_case("execute")
            )
        {
            self.advance(); // FOR
            let var = self.expect_ident_like()?;
            self.advance(); // IN
            self.advance(); // EXECUTE
            // Prescan for LOOP at paren depth 0 so parse_expr stops
            // before the LOOP keyword (same trick as the bare-SELECT
            // ForQuery arm).
            let mut depth: i32 = 0;
            let mut loop_pos: Option<usize> = None;
            let mut scan = self.pos;
            while scan < self.tokens.len() {
                match self.tokens.get(scan) {
                    Some(Token::LParen) => depth += 1,
                    Some(Token::RParen) => depth -= 1,
                    Some(Token::Ident(s) | Token::QuotedIdent(s))
                        if depth == 0 && s.eq_ignore_ascii_case("loop") =>
                    {
                        loop_pos = Some(scan);
                        break;
                    }
                    _ => {}
                }
                scan += 1;
            }
            let loop_pos = loop_pos.ok_or_else(|| {
                self.err(alloc::format!(
                    "FOR <var> IN EXECUTE <expr> ... LOOP: no LOOP keyword found"
                ))
            })?;
            let saved_loop = self.tokens[loop_pos].clone();
            self.tokens[loop_pos] = Token::Semicolon;
            let expr_result = self.parse_expr(0);
            self.tokens[loop_pos] = saved_loop;
            let sql_expr = expr_result?;
            let loop_kw = self.expect_ident_like()?;
            if !loop_kw.eq_ignore_ascii_case("loop") {
                return Err(self.err(alloc::format!(
                    "expected LOOP after FOR <var> IN EXECUTE <expr>, got {loop_kw:?}"
                )));
            }
            let body = self.parse_plpgsql_stmt_list_until_end()?;
            let end_kw = self.expect_ident_like()?;
            if !end_kw.eq_ignore_ascii_case("end") {
                return Err(self.err(alloc::format!(
                    "expected END LOOP after FOR IN EXECUTE body, got {end_kw:?}"
                )));
            }
            let loop_kw2 = self.expect_ident_like()?;
            if !loop_kw2.eq_ignore_ascii_case("loop") {
                return Err(self.err(alloc::format!(
                    "expected END LOOP after FOR IN EXECUTE body, got END {loop_kw2:?}"
                )));
            }
            return Ok(PlPgSqlStmt::ForExecute {
                var,
                sql_expr,
                body,
            });
        }
        // v7.37.20 (20.5) — FOR <var> IN <SELECT> LOOP.
        //
        // Two syntactic forms:
        //   FOR var IN SELECT ... ORDER BY ... LOOP ...
        //   FOR var IN (SELECT ...) LOOP ...
        //
        // Bare-SELECT form: to keep parse_select_stmt from swallowing
        // the trailing `LOOP` keyword as a table alias, we prescan
        // forward to find LOOP at paren depth 0, splice a fake
        // Semicolon at that position (so SELECT parses cleanly),
        // then re-splice LOOP back in.
        //
        // Paren-wrapped form: parse `(` `SELECT ...` `)` then expect
        // LOOP directly — no scan required.
        if matches!(self.peek(), Token::For)
            && matches!(
                self.tokens.get(self.pos + 1),
                Some(Token::Ident(_) | Token::QuotedIdent(_))
            )
            && matches!(self.tokens.get(self.pos + 2), Some(Token::In))
            && (matches!(self.tokens.get(self.pos + 3), Some(Token::Select))
                || matches!(self.tokens.get(self.pos + 3), Some(Token::LParen)))
        {
            self.advance(); // FOR
            let var = self.expect_ident_like()?;
            // IN
            self.advance();
            let query = if matches!(self.peek(), Token::LParen) {
                // Paren-wrapped SELECT.
                self.advance();
                let inner = self.parse_select_stmt()?;
                let Statement::Select(q) = inner else {
                    return Err(self.err(alloc::format!(
                        "expected SELECT inside (…), got {:?}",
                        self.peek()
                    )));
                };
                if !matches!(self.peek(), Token::RParen) {
                    return Err(self.err(alloc::format!(
                        "expected ')' after FOR-IN-SELECT body, got {:?}",
                        self.peek()
                    )));
                }
                self.advance();
                q
            } else {
                // Bare SELECT: prescan to find the LOOP boundary.
                let mut depth: i32 = 0;
                let mut loop_pos: Option<usize> = None;
                let mut scan = self.pos;
                while scan < self.tokens.len() {
                    match self.tokens.get(scan) {
                        Some(Token::LParen) => depth += 1,
                        Some(Token::RParen) => depth -= 1,
                        Some(Token::Ident(s) | Token::QuotedIdent(s))
                            if depth == 0 && s.eq_ignore_ascii_case("loop") =>
                        {
                            loop_pos = Some(scan);
                            break;
                        }
                        _ => {}
                    }
                    scan += 1;
                }
                let loop_pos = loop_pos.ok_or_else(|| {
                    self.err(alloc::format!(
                        "FOR <var> IN <SELECT> ... LOOP: no LOOP keyword found"
                    ))
                })?;
                // Swap the LOOP token with a synthetic Semicolon so
                // parse_select_stmt stops there, then restore afterward.
                let saved_loop = self.tokens[loop_pos].clone();
                self.tokens[loop_pos] = Token::Semicolon;
                let parse_result = self.parse_select_stmt();
                self.tokens[loop_pos] = saved_loop;
                let inner = parse_result?;
                let Statement::Select(q) = inner else {
                    return Err(self.err(alloc::format!(
                        "expected SELECT after FOR <var> IN, got {:?}",
                        self.peek()
                    )));
                };
                q
            };
            let loop_kw = self.expect_ident_like()?;
            if !loop_kw.eq_ignore_ascii_case("loop") {
                return Err(self.err(alloc::format!(
                    "expected LOOP after FOR <var> IN <SELECT>, got {loop_kw:?}"
                )));
            }
            let body = self.parse_plpgsql_stmt_list_until_end()?;
            let end_kw = self.expect_ident_like()?;
            if !end_kw.eq_ignore_ascii_case("end") {
                return Err(self.err(alloc::format!(
                    "expected END LOOP after FOR IN SELECT body, got {end_kw:?}"
                )));
            }
            let loop_kw2 = self.expect_ident_like()?;
            if !loop_kw2.eq_ignore_ascii_case("loop") {
                return Err(self.err(alloc::format!(
                    "expected END LOOP after FOR IN SELECT body, got END {loop_kw2:?}"
                )));
            }
            return Ok(PlPgSqlStmt::ForQuery {
                var,
                query: Box::new(query),
                body,
            });
        }
        // v7.37.20 (20.4) — FOR <var> IN [REVERSE] <start>..<end> LOOP.
        // FOR is a reserved keyword token (Token::For).
        if matches!(self.peek(), Token::For)
            && matches!(
                self.tokens.get(self.pos + 1),
                Some(Token::Ident(_) | Token::QuotedIdent(_))
            )
            && matches!(self.tokens.get(self.pos + 2), Some(Token::In))
        {
            self.advance(); // FOR
            let var = self.expect_ident_like()?;
            if !matches!(self.peek(), Token::In) {
                return Err(self.err(alloc::format!(
                    "expected IN after FOR <var>, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            let reverse = matches!(
                self.peek(),
                Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("reverse")
            );
            if reverse {
                self.advance();
            }
            let start = self.parse_expr(0)?;
            if !matches!(self.peek(), Token::DotDot) {
                return Err(self.err(alloc::format!(
                    "expected '..' between FOR loop bounds, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            let end = self.parse_expr(0)?;
            let loop_kw = self.expect_ident_like()?;
            if !loop_kw.eq_ignore_ascii_case("loop") {
                return Err(self.err(alloc::format!(
                    "expected LOOP after FOR <var> IN start..end, got {loop_kw:?}"
                )));
            }
            let body = self.parse_plpgsql_stmt_list_until_end()?;
            let end_kw = self.expect_ident_like()?;
            if !end_kw.eq_ignore_ascii_case("end") {
                return Err(self.err(alloc::format!(
                    "expected END LOOP after FOR body, got {end_kw:?}"
                )));
            }
            let loop_kw2 = self.expect_ident_like()?;
            if !loop_kw2.eq_ignore_ascii_case("loop") {
                return Err(self.err(alloc::format!(
                    "expected END LOOP after FOR body, got END {loop_kw2:?}"
                )));
            }
            return Ok(PlPgSqlStmt::ForRange {
                var,
                start,
                end,
                reverse,
                body,
            });
        }
        // v7.37.20 (20.2) — bare `LOOP <body> END LOOP;`.
        if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("loop"))
        {
            self.advance();
            let body = self.parse_plpgsql_stmt_list_until_end()?;
            let end_kw = self.expect_ident_like()?;
            if !end_kw.eq_ignore_ascii_case("end") {
                return Err(self.err(alloc::format!(
                    "expected END LOOP after LOOP body, got {end_kw:?}"
                )));
            }
            let loop_kw = self.expect_ident_like()?;
            if !loop_kw.eq_ignore_ascii_case("loop") {
                return Err(self.err(alloc::format!(
                    "expected END LOOP after LOOP body, got END {loop_kw:?}"
                )));
            }
            return Ok(PlPgSqlStmt::Loop { body });
        }
        // v7.37.20 (20.2) — `EXIT [WHEN <cond>]` inside a loop.
        if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("exit"))
        {
            self.advance();
            let when = if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("when"))
            {
                self.advance();
                Some(self.parse_expr(0)?)
            } else {
                None
            };
            return Ok(PlPgSqlStmt::Exit { when });
        }
        // v7.37.20 (20.13) — `EXECUTE <string_expr>`. Dispatches an
        // already-parsed Statement or a runtime-computed SQL string.
        // The disambiguator vs the extended-query-protocol `EXECUTE
        // <stmt_name>` (which is a top-level Statement, not a
        // plpgsql line) is that inside a DO block / trigger body the
        // EXECUTE keyword ALWAYS refers to dynamic SQL.
        if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("execute"))
        {
            self.advance();
            let sql = self.parse_expr(0)?;
            return Ok(PlPgSqlStmt::ExecuteDynamic { sql });
        }
        // v7.37.20 (20.2) — `CONTINUE [WHEN <cond>]` inside a loop.
        if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("continue"))
        {
            self.advance();
            let when = if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("when"))
            {
                self.advance();
                Some(self.parse_expr(0)?)
            } else {
                None
            };
            return Ok(PlPgSqlStmt::Continue { when });
        }
        // v7.37.20 (20.3) — WHILE <cond> LOOP <body> END LOOP.
        if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("while"))
        {
            self.advance();
            let condition = self.parse_expr(0)?;
            let loop_kw = self.expect_ident_like()?;
            if !loop_kw.eq_ignore_ascii_case("loop") {
                return Err(self.err(alloc::format!(
                    "expected LOOP after WHILE <condition>, got {loop_kw:?}"
                )));
            }
            let body = self.parse_plpgsql_stmt_list_until_end()?;
            // Expect END LOOP.
            let end_kw = self.expect_ident_like()?;
            if !end_kw.eq_ignore_ascii_case("end") {
                return Err(self.err(alloc::format!(
                    "expected END LOOP after WHILE body, got {end_kw:?}"
                )));
            }
            let loop_kw2 = self.expect_ident_like()?;
            if !loop_kw2.eq_ignore_ascii_case("loop") {
                return Err(self.err(alloc::format!(
                    "expected END LOOP after WHILE body, got END {loop_kw2:?}"
                )));
            }
            return Ok(PlPgSqlStmt::While { condition, body });
        }
        // v7.12.6 — RAISE.
        if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("raise"))
        {
            self.advance();
            return self.parse_plpgsql_raise();
        }
        // v7.37.20 (20.14) — ASSERT <cond> [, <msg>].
        if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("assert"))
        {
            self.advance();
            let condition = self.parse_expr(0)?;
            let message = if matches!(self.peek(), Token::Comma) {
                self.advance();
                Some(self.parse_expr(0)?)
            } else {
                None
            };
            return Ok(PlPgSqlStmt::Assert { condition, message });
        }
        // v7.37.20 (20.12) — PERFORM <select>. Per PG docs:
        //   "PERFORM is equivalent to SELECT but discards the
        //    result." Side effects (function calls, RAISE inside
        //    SQL functions, etc.) still execute. We desugar to
        //    `SELECT <body>` and wrap in EmbeddedSql so the engine's
        //    existing embedded-statement path handles execution +
        //    result-discard cleanly. The result is naturally
        //    discarded because EmbeddedSql doesn't propagate row
        //    sets back to the plpgsql interpreter.
        if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("perform"))
        {
            self.advance();
            // Splice a synthetic Token::Select into the stream at
            // the current position so parse_select_stmt parses the
            // remainder as a normal SELECT body. Token-stream
            // surgery mirrors the try_parse_plpgsql_select_into
            // pattern used for SELECT … INTO desugaring.
            self.tokens.insert(self.pos, Token::Select);
            let select = self.parse_select_stmt()?;
            let Statement::Select(s) = select else {
                return Err(self.err(alloc::format!(
                    "expected SELECT body after PERFORM, got {:?}",
                    self.peek()
                )));
            };
            return Ok(PlPgSqlStmt::EmbeddedSql(Box::new(Statement::Select(s))));
        }
        // v7.16.2 — `SELECT <projection> INTO <var> [FROM …]`
        // plpgsql-specific shape (mailrs round-10 migrate-042).
        // PG's SELECT INTO at top-level SQL would CREATE a new
        // table; inside plpgsql it ASSIGNS the query result to
        // a local variable. We detect the INTO at paren-depth
        // 0 between SELECT and the statement boundary; if
        // found, split the token stream into "pre-INTO
        // projection" + "var" + "post-INTO FROM/WHERE…" and
        // rebuild as a SelectInto with a regular SELECT body
        // (no INTO clause).
        if matches!(self.peek(), Token::Select)
            && let Some((select_body, var_name)) = self.try_parse_plpgsql_select_into()?
        {
            return Ok(PlPgSqlStmt::SelectInto {
                var: var_name,
                body: Box::new(select_body),
            });
        }
        // v7.12.6 — embedded SQL statements. INSERT/UPDATE/DELETE/
        // SELECT can appear directly inside a trigger body; we
        // recurse into the regular Statement parser, which will
        // stop at the trailing `;` (which our caller then
        // consumes).
        // v7.16.2 — top-level DO blocks (mailrs round-10 A.2)
        // also embed ALTER / CREATE / DROP statements; route
        // those through the same parser so the DO body parses
        // cleanly.
        if matches!(self.peek(), Token::Insert)
            || matches!(self.peek(), Token::Select)
            || matches!(self.peek(), Token::Create)
            || matches!(self.peek(), Token::Drop)
            || matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                if s.eq_ignore_ascii_case("update")
                    || s.eq_ignore_ascii_case("delete")
                    || s.eq_ignore_ascii_case("alter"))
        {
            let stmt = self.parse_one_statement()?;
            return Ok(PlPgSqlStmt::EmbeddedSql(Box::new(stmt)));
        }
        // Otherwise: assignment. `NEW.col` / `OLD.col` / `var`
        // followed by `:=` and an expression.
        let target = self.parse_plpgsql_assign_target()?;
        // PL/pgSQL assignment uses `:=`. The lexer represents
        // this as a colon followed by `=`; check both shapes.
        match self.peek() {
            Token::ColonEq => {
                self.advance();
            }
            Token::Colon => {
                self.advance();
                if !matches!(self.peek(), Token::Eq) {
                    return Err(self.err(alloc::format!(
                        "expected := after plpgsql assign target, got `:` then {:?}",
                        self.peek()
                    )));
                }
                self.advance();
            }
            other => {
                return Err(self.err(alloc::format!(
                    "expected := after plpgsql assign target, got {other:?}"
                )));
            }
        }
        let value = self.parse_expr(0)?;
        Ok(PlPgSqlStmt::Assign { target, value })
    }

    /// v7.12.6 — `IF cond THEN body [ELSIF cond THEN body]*
    /// [ELSE body] END IF`. `IF` keyword already consumed.
    fn parse_plpgsql_if(&mut self) -> Result<PlPgSqlStmt, ParseError> {
        let mut branches: Vec<(Expr, Vec<PlPgSqlStmt>)> = Vec::new();
        let mut else_branch: Vec<PlPgSqlStmt> = Vec::new();
        loop {
            // <expr> THEN
            let cond = self.parse_expr(0)?;
            let then_kw = self.expect_ident_like()?;
            if !then_kw.eq_ignore_ascii_case("then") {
                return Err(self.err(alloc::format!(
                    "expected THEN after IF/ELSIF condition, got {then_kw:?}"
                )));
            }
            let body = self.parse_plpgsql_stmt_list_until_end()?;
            branches.push((cond, body));
            // Look at terminator: ELSIF/ELSEIF, ELSE, or END IF.
            match self.peek() {
                Token::Ident(s) | Token::QuotedIdent(s)
                    if s.eq_ignore_ascii_case("elsif") || s.eq_ignore_ascii_case("elseif") =>
                {
                    self.advance();
                    continue;
                }
                Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("else") => {
                    self.advance();
                    else_branch = self.parse_plpgsql_stmt_list_until_end()?;
                    break;
                }
                Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("end") => {
                    break;
                }
                other => {
                    return Err(self.err(alloc::format!(
                        "expected ELSIF / ELSE / END after IF branch body, got {other:?}"
                    )));
                }
            }
        }
        // Expect `END IF` (the END keyword is the one we're
        // looking at right now).
        let end_kw = self.expect_ident_like()?;
        if !end_kw.eq_ignore_ascii_case("end") {
            return Err(self.err(alloc::format!("expected END IF, got {end_kw:?}")));
        }
        let if_kw = self.expect_ident_like()?;
        if !if_kw.eq_ignore_ascii_case("if") {
            return Err(self.err(alloc::format!("expected END IF, got END {if_kw:?}")));
        }
        Ok(PlPgSqlStmt::If {
            branches,
            else_branch,
        })
    }

    /// v7.12.6 — `RAISE { NOTICE | WARNING | INFO | LOG | DEBUG
    /// | EXCEPTION } '<message>' [, args]*`. The `RAISE` keyword
    /// is already consumed.
    fn parse_plpgsql_raise(&mut self) -> Result<PlPgSqlStmt, ParseError> {
        let lvl_ident = self.expect_ident_like()?;
        let level = match lvl_ident.to_ascii_lowercase().as_str() {
            "notice" => RaiseLevel::Notice,
            "warning" => RaiseLevel::Warning,
            "info" => RaiseLevel::Info,
            "log" => RaiseLevel::Log,
            "debug" => RaiseLevel::Debug,
            "exception" => RaiseLevel::Exception,
            other => {
                return Err(self.err(alloc::format!(
                    "expected RAISE level (NOTICE/WARNING/INFO/LOG/DEBUG/EXCEPTION), got {other:?}"
                )));
            }
        };
        // Message: required for v7.12.6. PG accepts a bare
        // RAISE-rethrow form (no message), reserved for future
        // RAISE-no-args support.
        let Token::String(msg) = self.peek() else {
            return Err(self.err(alloc::format!(
                "expected RAISE message string, got {:?}",
                self.peek()
            )));
        };
        let message = msg.clone();
        self.advance();
        // Optional comma-separated args (PG `%` format substitution).
        let mut args: Vec<Expr> = Vec::new();
        while matches!(self.peek(), Token::Comma) {
            self.advance();
            args.push(self.parse_expr(0)?);
        }
        Ok(PlPgSqlStmt::Raise {
            level,
            message,
            args,
        })
    }

    /// v7.16.2 — scan ahead for a plpgsql-flavoured `SELECT
    /// <projection> INTO <var> [FROM …]` (mailrs round-10
    /// migrate-042). Returns `(rebuilt_select_without_into,
    /// var_name)` when the pattern matches; `None` for
    /// regular SELECTs (those go through the embedded-SQL
    /// path). Token-stream surgery so the rebuilt SELECT
    /// parses through the regular `parse_select_stmt`.
    #[allow(clippy::too_many_lines)]
    fn try_parse_plpgsql_select_into(
        &mut self,
    ) -> Result<Option<(SelectStatement, String)>, ParseError> {
        // Scan forward from `self.pos + 1` (past Token::Select)
        // for Token::Into at paren-depth 0, stopping at the
        // first `;`, `END`, `ELSE`, `ELSIF` keyword that would
        // end the plpgsql statement.
        let start = self.pos;
        let mut into_pos: Option<usize> = None;
        let mut depth: i32 = 0;
        let mut i = start + 1;
        while i < self.tokens.len() {
            match &self.tokens[i] {
                Token::LParen => depth += 1,
                Token::RParen => depth -= 1,
                Token::Semicolon if depth == 0 => break,
                Token::Ident(s)
                    if depth == 0
                        && (s.eq_ignore_ascii_case("end")
                            || s.eq_ignore_ascii_case("else")
                            || s.eq_ignore_ascii_case("elsif")) =>
                {
                    break;
                }
                Token::Into if depth == 0 => {
                    into_pos = Some(i);
                    break;
                }
                _ => {}
            }
            i += 1;
        }
        let Some(into_at) = into_pos else {
            return Ok(None);
        };
        // The token immediately after INTO must be the target
        // var ident; anything else (e.g. INSERT INTO table)
        // ruled out by the depth-0 check above. Capture it.
        let var = match self.tokens.get(into_at + 1) {
            Some(Token::Ident(s) | Token::QuotedIdent(s)) => s.clone(),
            other => {
                return Err(self.err(alloc::format!(
                    "expected variable name after SELECT … INTO, got {other:?}"
                )));
            }
        };
        // Find the end of the plpgsql SELECT INTO statement —
        // same boundary rules as the depth-0 scan above.
        let mut end = into_at + 2;
        let mut depth2: i32 = 0;
        while end < self.tokens.len() {
            match &self.tokens[end] {
                Token::LParen => depth2 += 1,
                Token::RParen => depth2 -= 1,
                Token::Semicolon if depth2 == 0 => break,
                Token::Ident(s)
                    if depth2 == 0
                        && (s.eq_ignore_ascii_case("end")
                            || s.eq_ignore_ascii_case("else")
                            || s.eq_ignore_ascii_case("elsif")) =>
                {
                    break;
                }
                _ => {}
            }
            end += 1;
        }
        // Rebuild a token stream that represents the SELECT
        // WITHOUT the INTO clause: [SELECT .. up-to-INTO] + [
        // post-var tokens up to statement end]. Run the
        // regular `parse_select_stmt` against it.
        let mut rebuilt: Vec<Token> = Vec::with_capacity(end - start);
        for j in start..into_at {
            rebuilt.push(self.tokens[j].clone());
        }
        for j in (into_at + 2)..end {
            rebuilt.push(self.tokens[j].clone());
        }
        rebuilt.push(Token::Eof);
        let saved_pos = self.pos;
        let saved_tokens = core::mem::replace(&mut self.tokens, rebuilt);
        self.pos = 0;
        // parse_select_stmt → parse_bare_select consumes Token::Select itself.
        if !matches!(self.peek(), Token::Select) {
            self.tokens = saved_tokens;
            self.pos = saved_pos;
            return Err(self.err("plpgsql SELECT … INTO: rebuilt stream missing SELECT".into()));
        }
        let sel = self.parse_select_stmt();
        self.tokens = saved_tokens;
        self.pos = end;
        let sel = sel?;
        let Statement::Select(body) = sel else {
            return Err(self.err(alloc::format!(
                "plpgsql SELECT … INTO: rebuilt SELECT did not produce a Select node, got {sel:?}"
            )));
        };
        Ok(Some((body, var)))
    }

    fn parse_plpgsql_assign_target(&mut self) -> Result<AssignTarget, ParseError> {
        // v7.16.1 — read the head token DIRECTLY rather than
        // via `expect_ident_like`. The v7.14.0 schema-qualifier
        // strip (`public.t` → `t`) inside `expect_ident_like`
        // greedily consumes any `ident . ident` pair, which
        // silently turned every `NEW.col := …` /
        // `OLD.col := …` plpgsql assignment into a Local("col")
        // assignment — the head "new"/"old" was eaten as if it
        // were a schema name and the Dot was consumed too, so
        // this function's own `peek() == Token::Dot` check
        // below never fired. Every BEFORE trigger that rewrote
        // a NEW cell was a silent no-op for two major releases
        // (v7.14.0 + v7.15.0) until the e2e_trigger workspace-
        // gate failures were investigated as v7.16.1 backlog.
        let head = match self.advance() {
            Token::Ident(s) | Token::QuotedIdent(s) => s,
            other => {
                return Err(self.err(alloc::format!(
                    "expected NEW / OLD / <local_var> as plpgsql assign target, got {other:?}"
                )));
            }
        };
        if matches!(self.peek(), Token::Dot) {
            self.advance();
            let col = self.expect_ident_like()?;
            if head.eq_ignore_ascii_case("new") {
                return Ok(AssignTarget::NewColumn(col));
            }
            if head.eq_ignore_ascii_case("old") {
                return Ok(AssignTarget::OldColumn(col));
            }
            return Err(self.err(alloc::format!(
                "plpgsql assign target must be NEW.<col> / OLD.<col> / <local_var>; \
                 got {head:?}.<col>"
            )));
        }
        Ok(AssignTarget::Local(head))
    }

    fn parse_plpgsql_return(&mut self) -> Result<PlPgSqlStmt, ParseError> {
        // RETURN NEW / OLD / NULL — bare-ident forms.
        match self.peek() {
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("new") => {
                self.advance();
                return Ok(PlPgSqlStmt::Return(ReturnTarget::New));
            }
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("old") => {
                self.advance();
                return Ok(PlPgSqlStmt::Return(ReturnTarget::Old));
            }
            Token::Null => {
                self.advance();
                return Ok(PlPgSqlStmt::Return(ReturnTarget::Null));
            }
            // Bare `RETURN;` (no value) — treated as `RETURN NULL`
            // per PL/pgSQL convention.
            Token::Semicolon => {
                return Ok(PlPgSqlStmt::Return(ReturnTarget::Null));
            }
            _ => {}
        }
        // v7.37.20 (20.11) — RETURN QUERY <select> / RETURN QUERY
        // EXECUTE <expr>. In a DO block context RETURN QUERY has no
        // caller-visible effect (blocks don't return sets), so we
        // desugar it identically to PERFORM: parse the SELECT (or
        // EXECUTE dynamic) as embedded SQL that runs for side
        // effects and discards the result. RETURN NEXT <expr>
        // (single-row accumulator) queues with v7.40 SETOF function
        // infrastructure.
        if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("query"))
        {
            self.advance();
            if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("execute"))
            {
                self.advance();
                let sql = self.parse_expr(0)?;
                return Ok(PlPgSqlStmt::ExecuteDynamic { sql });
            }
            // Bare RETURN QUERY <select>. If the current token is
            // not already SELECT (e.g., the user wrote `RETURN QUERY
            // <projection> FROM ...` in a shorthand — rare but PG
            // accepts a bare projection here), splice one in. Same
            // trick as PERFORM.
            if !matches!(self.peek(), Token::Select) {
                self.tokens.insert(self.pos, Token::Select);
            }
            let select = self.parse_select_stmt()?;
            let Statement::Select(s) = select else {
                return Err(self.err(alloc::format!(
                    "expected SELECT body after RETURN QUERY, got {:?}",
                    self.peek()
                )));
            };
            return Ok(PlPgSqlStmt::EmbeddedSql(Box::new(Statement::Select(s))));
        }
        // Fall through: parse a full expression.
        let e = self.parse_expr(0)?;
        Ok(PlPgSqlStmt::Return(ReturnTarget::Expr(e)))
    }

    fn parse_trigger_event(&mut self) -> Result<TriggerEvent, ParseError> {
        // INSERT is a reserved Token; UPDATE / DELETE / TRUNCATE
        // are ident-shaped (the parser keys off case-insensitive
        // match — same shape used by the top-level Update / Delete
        // dispatchers at parse_one_statement).
        if matches!(self.peek(), Token::Insert) {
            self.advance();
            return Ok(TriggerEvent::Insert);
        }
        match self.peek() {
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("update") => {
                self.advance();
                Ok(TriggerEvent::Update)
            }
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("delete") => {
                self.advance();
                Ok(TriggerEvent::Delete)
            }
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("truncate") => {
                self.advance();
                Ok(TriggerEvent::Truncate)
            }
            other => Err(self.err(alloc::format!(
                "expected INSERT / UPDATE / DELETE / TRUNCATE in trigger event list, got {other:?}"
            ))),
        }
    }

    /// v6.1.2 → v6.1.3 — `CREATE PUBLICATION <name>` body. Accepts:
    ///   - (no clause) → implicit `FOR ALL TABLES`
    ///   - `FOR ALL TABLES`
    ///   - `FOR ALL TABLES EXCEPT t1, t2, …` (v6.1.3)
    ///   - `FOR TABLE t1, t2, …` (v6.1.3) — `FOR TABLES …` also
    ///     accepted (PG accepts both forms in PG 19).
    fn parse_create_publication_after_keyword(&mut self) -> Result<Statement, ParseError> {
        let name = self.expect_ident_or_string()?;
        // Bare DDL maps to FOR ALL TABLES — matches the v6.1.2
        // shape so existing publications keep parsing identically.
        let scope = if matches!(self.peek(), Token::For) {
            self.advance();
            if matches!(self.peek(), Token::All) {
                self.advance();
                if !matches!(self.peek(), Token::Tables) {
                    return Err(self.err(format!(
                        "expected TABLES after FOR ALL, got {:?}",
                        self.peek()
                    )));
                }
                self.advance();
                if matches!(self.peek(), Token::Except) {
                    self.advance();
                    let tables = self.parse_publication_table_list()?;
                    PublicationScope::AllTablesExcept(tables)
                } else {
                    PublicationScope::AllTables
                }
            } else if matches!(self.peek(), Token::Table | Token::Tables) {
                // PG 19 accepts both `FOR TABLE …` (singular) and
                // `FOR TABLES …` (plural); SPG matches.
                self.advance();
                let tables = self.parse_publication_table_list()?;
                PublicationScope::ForTables(tables)
            } else {
                return Err(self.err(format!(
                    "expected ALL TABLES or TABLE <list> after FOR, got {:?}",
                    self.peek()
                )));
            }
        } else {
            PublicationScope::AllTables
        };
        Ok(Statement::CreatePublication(CreatePublicationStatement {
            name,
            scope,
        }))
    }

    /// v6.1.3 — Comma-separated identifier list for the publication
    /// FOR-clause. Requires at least one entry; empty list is a
    /// parse error (PG behaviour). Quoted idents are accepted; the
    /// names round-trip through `Display` as `quote_ident(name)`.
    ///
    /// v7.37.21 (21.2 + 21.3) — accept-and-discard the per-table
    /// `(col_list) WHERE (predicate)` modifiers PG 15+ emits in
    /// pg_dump output. SPG's publication state today is per-table
    /// only (matching the pre-PG-15 surface); the col list + WHERE
    /// are parsed so dumps load through and the table name reaches
    /// `PublicationScope::ForTables`, but the filter is not enforced
    /// at publish time. Re-open when a customer dogfood gate
    /// requires per-row-filter or column-subset publish semantics
    /// (which gates on persistent slot state landing first, 21.12).
    fn parse_publication_table_list(&mut self) -> Result<Vec<String>, ParseError> {
        let first = self.parse_publication_table_entry()?;
        let mut out = alloc::vec![first];
        while matches!(self.peek(), Token::Comma) {
            self.advance();
            out.push(self.parse_publication_table_entry()?);
        }
        Ok(out)
    }

    /// One table entry inside a FOR TABLE clause:
    ///     tab_name [ (col, col, …) ] [ WHERE (predicate) ]
    /// Returns just the table name; the column list + WHERE predicate
    /// are consumed and discarded per the parse-accept-discard
    /// commitment above.
    fn parse_publication_table_entry(&mut self) -> Result<String, ParseError> {
        let name = self.expect_ident_like()?;
        // Optional column list — `(col, col, …)`.
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            // Empty parens are a PG error too; require ≥ 1 column.
            let _ = self.expect_ident_like()?;
            while matches!(self.peek(), Token::Comma) {
                self.advance();
                let _ = self.expect_ident_like()?;
            }
            if !matches!(self.peek(), Token::RParen) {
                return Err(self.err(alloc::format!(
                    "expected ')' to close publication column list, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
        }
        // Optional row filter — `WHERE (predicate)`.
        if matches!(self.peek(), Token::Where) {
            self.advance();
            if !matches!(self.peek(), Token::LParen) {
                return Err(self.err(alloc::format!(
                    "expected '(' after WHERE in publication row filter, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            let _ = self.parse_expr(0)?;
            if !matches!(self.peek(), Token::RParen) {
                return Err(self.err(alloc::format!(
                    "expected ')' to close publication WHERE filter, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
        }
        Ok(name)
    }

    /// v6.1.4 — `CREATE SUBSCRIPTION <name>
    ///                 CONNECTION '<conn>'
    ///                 PUBLICATION <pub> [, <pub> ...]`.
    ///
    /// The clause order is fixed (CONNECTION first, then
    /// PUBLICATION) to match PG. No WITH-options accepted in
    /// v6.1.4 — `enabled` defaults to true, no other knobs ship.
    fn parse_create_subscription_after_keyword(&mut self) -> Result<Statement, ParseError> {
        let name = self.expect_ident_or_string()?;
        if !matches!(self.peek(), Token::Connection) {
            return Err(self.err(format!(
                "expected CONNECTION after CREATE SUBSCRIPTION <name>, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let conn_str = self.expect_string_literal()?;
        if !matches!(self.peek(), Token::Publication) {
            return Err(self.err(format!(
                "expected PUBLICATION after CONNECTION '<conn>', got {:?}",
                self.peek()
            )));
        }
        self.advance();
        // Reuse the publication FOR-list parser shape: at least one
        // identifier, comma-separated.
        let first = self.expect_ident_like()?;
        let mut publications = alloc::vec![first];
        while matches!(self.peek(), Token::Comma) {
            self.advance();
            publications.push(self.expect_ident_like()?);
        }
        Ok(Statement::CreateSubscription(CreateSubscriptionStatement {
            name,
            conn_str,
            publications,
        }))
    }

    /// v6.1.7 — `WAIT FOR WAL POSITION <pos> [WITH TIMEOUT <ms>]`.
    /// All keywords after `WAIT` are bare idents in v6.1.x; no
    /// lexer churn. Both `<pos>` and `<ms>` are positive integers
    /// that fit `u64`.
    /// v7.12.1 — parameter name in `SET <name>` may be dotted
    /// (`pg_catalog.default_text_search_config` etc).
    fn parse_set_param_name(&mut self) -> Result<String, ParseError> {
        let mut name = self.expect_ident_like()?;
        while matches!(self.peek(), Token::Dot) {
            self.advance();
            let next = self.expect_ident_like()?;
            name.push('.');
            name.push_str(&next);
        }
        Ok(name.to_ascii_lowercase())
    }

    fn parse_set_value(&mut self) -> Result<crate::ast::SetValue, ParseError> {
        match self.advance() {
            Token::String(s) => Ok(crate::ast::SetValue::String(s)),
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("default") => {
                Ok(crate::ast::SetValue::Default)
            }
            Token::Ident(s) | Token::QuotedIdent(s) => {
                let mut accum = s;
                while matches!(self.peek(), Token::Dot) {
                    self.advance();
                    let next = self.expect_ident_like()?;
                    accum.push('.');
                    accum.push_str(&next);
                }
                Ok(crate::ast::SetValue::Ident(accum))
            }
            Token::Integer(n) => Ok(crate::ast::SetValue::Number(n.to_string())),
            Token::Float(f) => Ok(crate::ast::SetValue::Number(f.to_string())),
            // v7.22 (mailrs round-13 gap 2) — PG boolean parameter
            // spellings that lex as keyword tokens, not idents:
            // `SET standard_conforming_strings = on` is in every
            // pg_dump preamble (`off` already lexes as an ident).
            Token::On => Ok(crate::ast::SetValue::Ident("on".to_string())),
            Token::True => Ok(crate::ast::SetValue::Ident("true".to_string())),
            Token::False => Ok(crate::ast::SetValue::Ident("false".to_string())),
            // v7.14.0 — MySQL session/user variable RHS
            // (e.g. `SET OLD_FOREIGN_KEY_CHECKS = @@FOREIGN_KEY_CHECKS`).
            // Wrap as Ident so the SET handler can record it; the
            // engine treats `@VAR` / `@@VAR` values as opaque
            // strings.
            Token::SessionVar(s) => Ok(crate::ast::SetValue::Ident(s)),
            // v7.14.0 — `SET sql_mode = 'NO_AUTO_VALUE_ON_ZERO,STRICT_TRANS_TABLES'`
            // is the common MySQL preamble shape. Allow a `+` or
            // `-` prefix on negative numerics for parity with PG
            // (some param defaults are negative).
            Token::Minus => match self.advance() {
                Token::Integer(n) => Ok(crate::ast::SetValue::Number(alloc::format!("-{n}"))),
                Token::Float(f) => Ok(crate::ast::SetValue::Number(alloc::format!("-{f}"))),
                other => Err(self.err(format!(
                    "expected numeric after `-` in SET value, got {other:?}"
                ))),
            },
            other => Err(self.err(format!(
                "expected literal, identifier, or DEFAULT after `=` in SET, got {other:?}"
            ))),
        }
    }

    /// v7.38 轴 4 — `[ISOLATION LEVEL …] [READ ONLY|WRITE]
    /// [[NOT] DEFERRABLE]` modes after `SET TRANSACTION` or
    /// `START TRANSACTION` / `BEGIN`. Returns the isolation level
    /// (default `ReadCommitted` if no `ISOLATION LEVEL` clause was
    /// present). Modes are comma-separated per PG; SPG also
    /// accepts space-separated for tolerance. READ ONLY / WRITE
    /// / DEFERRABLE are parsed-and-ignored (recorded for future
    /// surface but not behaviorally honoured today).
    fn parse_isolation_level_clauses(&mut self) -> Result<IsolationLevel, ParseError> {
        let mut level = IsolationLevel::default();
        let mut have_level = false;
        loop {
            // ISOLATION LEVEL …
            let saw_isolation =
                matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("isolation"));
            if saw_isolation {
                self.advance(); // ISOLATION
                if !matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("level")) {
                    return Err(self.err(alloc::format!(
                        "expected LEVEL after ISOLATION, got {:?}",
                        self.peek()
                    )));
                }
                self.advance(); // LEVEL
                // SERIALIZABLE | REPEATABLE READ | READ COMMITTED | READ UNCOMMITTED
                let w1 = self
                    .expect_ident_like()
                    .map_err(|e| self.err(alloc::format!("isolation level: {e:?}")))?;
                let lc = w1.to_ascii_lowercase();
                level = match lc.as_str() {
                    "serializable" => IsolationLevel::Serializable,
                    "repeatable" => {
                        // Expect READ
                        let w2 = self
                            .expect_ident_like()
                            .map_err(|e| self.err(alloc::format!("REPEATABLE …: {e:?}")))?;
                        if !w2.eq_ignore_ascii_case("read") {
                            return Err(self.err(alloc::format!(
                                "expected READ after REPEATABLE, got {w2:?}"
                            )));
                        }
                        IsolationLevel::RepeatableRead
                    }
                    "read" => {
                        let w2 = self
                            .expect_ident_like()
                            .map_err(|e| self.err(alloc::format!("READ …: {e:?}")))?;
                        match w2.to_ascii_lowercase().as_str() {
                            "committed" => IsolationLevel::ReadCommitted,
                            "uncommitted" => IsolationLevel::ReadUncommitted,
                            other => {
                                return Err(self.err(alloc::format!(
                                    "expected COMMITTED or UNCOMMITTED after READ, got {other:?}"
                                )));
                            }
                        }
                    }
                    other => {
                        return Err(self.err(alloc::format!("unknown isolation level {other:?}")));
                    }
                };
                have_level = true;
            } else if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("read")) {
                // READ ONLY | READ WRITE — parsed, not behaviorally honoured.
                self.advance();
                match self.peek().clone() {
                    Token::Ident(s) if s.eq_ignore_ascii_case("only") => {
                        self.advance();
                    }
                    Token::Ident(s) if s.eq_ignore_ascii_case("write") => {
                        self.advance();
                    }
                    other => {
                        return Err(self.err(alloc::format!(
                            "expected ONLY or WRITE after READ, got {other:?}"
                        )));
                    }
                }
            } else if matches!(self.peek(), Token::Not) {
                // NOT DEFERRABLE — `NOT` lexes as a reserved keyword.
                self.advance();
                if !matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("deferrable")) {
                    return Err(self.err(alloc::format!(
                        "expected DEFERRABLE after NOT, got {:?}",
                        self.peek()
                    )));
                }
                self.advance();
            } else if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("deferrable"))
            {
                self.advance();
            } else {
                break;
            }
            // Optional comma between modes.
            if matches!(self.peek(), Token::Comma) {
                self.advance();
            }
        }
        let _ = have_level;
        Ok(level)
    }

    fn parse_wait_after_keyword(&mut self) -> Result<Statement, ParseError> {
        // FOR is a v6.1.2-reserved keyword (Token::For). The
        // other two are bare idents — they've never needed lexer
        // support and we keep it that way.
        if !matches!(self.peek(), Token::For) {
            return Err(self.err(format!("expected FOR after WAIT, got {:?}", self.peek())));
        }
        self.advance();
        self.expect_keyword_ident("wal")?;
        self.expect_keyword_ident("position")?;
        let pos = self.expect_u64_literal()?;
        let timeout_ms = if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("with"))
        {
            self.advance();
            self.expect_keyword_ident("timeout")?;
            Some(self.expect_u64_literal()?)
        } else {
            None
        };
        Ok(Statement::WaitForWalPosition { pos, timeout_ms })
    }

    /// v6.1.7 helper — consume a `Token::Integer` and check it
    /// fits `u64`. WAL positions and millisecond timeouts are
    /// non-negative.
    fn expect_u64_literal(&mut self) -> Result<u64, ParseError> {
        match self.advance() {
            Token::Integer(n) if n >= 0 => Ok(n as u64),
            Token::Integer(n) => Err(ParseError {
                message: format!("expected non-negative integer, got {n}"),
                token_pos: self.pos.saturating_sub(1),
            }),
            other => Err(ParseError {
                message: format!("expected integer literal, got {other:?}"),
                token_pos: self.pos.saturating_sub(1),
            }),
        }
    }

    /// `CREATE USER` body — name + WITH PASSWORD '<pw>' + optional
    /// ROLE '<role>' (defaults to readonly). All string slots accept
    /// either a quoted ident or a quoted string literal.
    fn parse_create_user_after_keyword(&mut self) -> Result<Statement, ParseError> {
        let name = self.expect_ident_or_string()?;
        self.expect_keyword_ident("with")?;
        self.expect_keyword_ident("password")?;
        let password = self.expect_string_literal()?;
        let role = if let Token::Ident(s) | Token::QuotedIdent(s) = self.peek()
            && s.eq_ignore_ascii_case("role")
        {
            self.advance();
            self.expect_string_literal()?
        } else {
            "readonly".to_string()
        };
        Ok(Statement::CreateUser(crate::ast::CreateUserStatement {
            name,
            password,
            role,
        }))
    }

    /// v4.4 `UPDATE <table> SET col = expr [, col = expr]* [WHERE cond]`.
    /// Caller already consumed the leading `UPDATE` ident.
    fn parse_update_after_keyword(&mut self) -> Result<Statement, ParseError> {
        let table = self.expect_ident_like()?;
        self.expect_keyword_ident("set")?;
        let mut assignments = Vec::new();
        loop {
            let col = self.expect_ident_like()?;
            if !matches!(self.peek(), Token::Eq) {
                return Err(self.err(format!(
                    "expected `=` after column name in UPDATE SET, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            let value = self.parse_expr(0)?;
            assignments.push((col, value));
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        let where_ = if matches!(self.peek(), Token::Where) {
            self.advance();
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        let returning = self.parse_optional_returning()?;
        Ok(Statement::Update(crate::ast::UpdateStatement {
            ctes: Vec::new(),
            table,
            assignments,
            where_,
            returning,
        }))
    }

    /// v4.4 `DELETE FROM <table> [WHERE cond]`. Caller already consumed
    /// the leading `DELETE` ident.
    fn parse_delete_after_keyword(&mut self) -> Result<Statement, ParseError> {
        if !matches!(self.peek(), Token::From) {
            return Err(self.err(format!("expected FROM after DELETE, got {:?}", self.peek())));
        }
        self.advance();
        let table = self.expect_ident_like()?;
        let where_ = if matches!(self.peek(), Token::Where) {
            self.advance();
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        let returning = self.parse_optional_returning()?;
        Ok(Statement::Delete(crate::ast::DeleteStatement {
            ctes: Vec::new(),
            table,
            where_,
            returning,
        }))
    }

    /// v7.17.0 Phase 3.P0-42 — parse `MERGE INTO <target> [alias]
    /// USING <source> [alias] ON <expr> WHEN [NOT] MATCHED [AND
    /// <expr>] THEN <action> [WHEN …]` after the leading `MERGE`
    /// keyword. v7.17 surface:
    ///   * source: table reference (subquery source is a follow-up)
    ///   * actions: UPDATE SET / DELETE / DO NOTHING (matched);
    ///     INSERT (cols) VALUES (vals) / DO NOTHING (not matched)
    ///   * AND-conditioned WHEN clauses; clauses tried in declaration
    ///     order
    fn parse_merge_after_keyword(&mut self) -> Result<Statement, ParseError> {
        // INTO
        let is_into_kw = matches!(self.peek(), Token::Into)
            || matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("into"));
        if !is_into_kw {
            return Err(self.err(format!("expected INTO after MERGE, got {:?}", self.peek())));
        }
        self.advance();
        let target = self.expect_ident_like()?;
        // Optional alias — bare ident before USING.
        let target_alias = match self.peek() {
            Token::Ident(s) | Token::QuotedIdent(s) if !s.eq_ignore_ascii_case("using") => {
                Some(self.expect_ident_like()?)
            }
            _ => None,
        };
        // USING
        let is_using_kw = matches!(
            self.peek(),
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("using")
        );
        if !is_using_kw {
            return Err(self.err(format!(
                "expected USING after MERGE INTO target, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let source = self.expect_ident_like()?;
        let source_alias = match self.peek() {
            Token::Ident(s) | Token::QuotedIdent(s) if !s.eq_ignore_ascii_case("on") => {
                Some(self.expect_ident_like()?)
            }
            _ => None,
        };
        // ON
        if !matches!(self.peek(), Token::On) {
            return Err(self.err(format!(
                "expected ON after MERGE … USING source, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let on = self.parse_expr(0)?;
        // One or more WHEN clauses.
        let mut clauses: Vec<crate::ast::MergeWhenClause> = Vec::new();
        loop {
            let is_when_kw = matches!(
                self.peek(),
                Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("when")
            );
            if !is_when_kw {
                break;
            }
            self.advance(); // WHEN
            // [NOT] MATCHED
            let matched = if matches!(self.peek(), Token::Not) {
                self.advance();
                crate::ast::MergeMatched::NotMatched
            } else {
                crate::ast::MergeMatched::Matched
            };
            let is_matched_kw = matches!(
                self.peek(),
                Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("matched")
            );
            if !is_matched_kw {
                return Err(self.err(format!(
                    "expected MATCHED in WHEN clause, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            // Optional AND <expr>
            let condition = if matches!(self.peek(), Token::And) {
                self.advance();
                Some(self.parse_expr(0)?)
            } else {
                None
            };
            // THEN
            let is_then_kw = matches!(
                self.peek(),
                Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("then")
            );
            if !is_then_kw {
                return Err(self.err(format!(
                    "expected THEN in WHEN clause, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            // Action: INSERT / UPDATE / DELETE / DO NOTHING
            let action = match self.peek().clone() {
                Token::Insert => {
                    self.advance();
                    // (cols)
                    if !matches!(self.peek(), Token::LParen) {
                        return Err(self.err(format!(
                            "expected '(' after INSERT in MERGE, got {:?}",
                            self.peek()
                        )));
                    }
                    self.advance();
                    let mut columns: Vec<String> = Vec::new();
                    loop {
                        columns.push(self.expect_ident_like()?);
                        if matches!(self.peek(), Token::Comma) {
                            self.advance();
                            continue;
                        }
                        break;
                    }
                    if !matches!(self.peek(), Token::RParen) {
                        return Err(self.err(format!(
                            "expected ')' after INSERT column list, got {:?}",
                            self.peek()
                        )));
                    }
                    self.advance();
                    // VALUES (...)
                    if !matches!(self.peek(), Token::Values) {
                        return Err(self.err(format!(
                            "expected VALUES in MERGE INSERT, got {:?}",
                            self.peek()
                        )));
                    }
                    self.advance();
                    if !matches!(self.peek(), Token::LParen) {
                        return Err(self.err(format!(
                            "expected '(' after VALUES in MERGE INSERT, got {:?}",
                            self.peek()
                        )));
                    }
                    self.advance();
                    let mut values: Vec<crate::ast::Expr> = Vec::new();
                    loop {
                        values.push(self.parse_expr(0)?);
                        if matches!(self.peek(), Token::Comma) {
                            self.advance();
                            continue;
                        }
                        break;
                    }
                    if !matches!(self.peek(), Token::RParen) {
                        return Err(self.err(format!(
                            "expected ')' after MERGE INSERT values, got {:?}",
                            self.peek()
                        )));
                    }
                    self.advance();
                    if columns.len() != values.len() {
                        return Err(self.err(format!(
                            "MERGE INSERT column count ({}) ≠ value count ({})",
                            columns.len(),
                            values.len()
                        )));
                    }
                    crate::ast::MergeAction::Insert { columns, values }
                }
                Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("update") => {
                    self.advance();
                    // SET
                    let is_set_kw = matches!(
                        self.peek(),
                        Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("set")
                    );
                    if !is_set_kw {
                        return Err(self.err(format!(
                            "expected SET after UPDATE in MERGE, got {:?}",
                            self.peek()
                        )));
                    }
                    self.advance();
                    let mut assignments: Vec<(String, crate::ast::Expr)> = Vec::new();
                    loop {
                        let col = self.expect_ident_like()?;
                        if !matches!(self.peek(), Token::Eq) {
                            return Err(self.err(format!(
                                "expected '=' in MERGE UPDATE assignment, got {:?}",
                                self.peek()
                            )));
                        }
                        self.advance();
                        let expr = self.parse_expr(0)?;
                        assignments.push((col, expr));
                        if matches!(self.peek(), Token::Comma) {
                            self.advance();
                            continue;
                        }
                        break;
                    }
                    crate::ast::MergeAction::Update { assignments }
                }
                Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("delete") => {
                    self.advance();
                    crate::ast::MergeAction::Delete
                }
                Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("do") => {
                    self.advance();
                    let is_nothing_kw = matches!(
                        self.peek(),
                        Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("nothing")
                    );
                    if !is_nothing_kw {
                        return Err(self.err(format!(
                            "expected NOTHING after DO in MERGE clause, got {:?}",
                            self.peek()
                        )));
                    }
                    self.advance();
                    crate::ast::MergeAction::DoNothing
                }
                other => {
                    return Err(self.err(format!(
                        "expected INSERT / UPDATE / DELETE / DO NOTHING in MERGE clause, got {other:?}"
                    )));
                }
            };
            clauses.push(crate::ast::MergeWhenClause {
                matched,
                condition,
                action,
            });
        }
        if clauses.is_empty() {
            return Err(self.err(String::from("MERGE requires at least one WHEN clause")));
        }
        Ok(Statement::Merge(crate::ast::MergeStatement {
            target,
            target_alias,
            source,
            source_alias,
            on,
            clauses,
        }))
    }

    /// v7.9.4 — parse the optional trailing `RETURNING <projection>`
    /// clause on INSERT / UPDATE / DELETE. Same projection grammar
    /// as SELECT, so `RETURNING *`, `RETURNING col`,
    /// `RETURNING expr AS alias`, and `RETURNING a, b, c` all work.
    fn parse_optional_returning(
        &mut self,
    ) -> Result<Option<Vec<crate::ast::SelectItem>>, ParseError> {
        let is_returning_kw = matches!(
            self.peek(),
            Token::Ident(s) if s.eq_ignore_ascii_case("returning")
        );
        if !is_returning_kw {
            return Ok(None);
        }
        self.advance();
        let mut items = Vec::new();
        loop {
            items.push(self.parse_select_item()?);
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        Ok(Some(items))
    }

    /// v6.0.4 — parse the tail of an ALTER statement after the
    /// leading `ALTER` keyword has been consumed. Only one form is
    /// supported in v6.0.4:
    ///
    /// ```text
    /// ALTER INDEX <name> REBUILD [WITH (encoding = <enc>)]
    /// ```
    fn parse_alter_after_keyword(&mut self) -> Result<Statement, ParseError> {
        // ALTER INDEX <name> ... | ALTER TABLE <name> SET hot_tier_bytes = <n>
        // v7.14.0 — `ALTER TABLE ONLY` modifier (PG partition-
        // exclusion) is accepted by stripping the `ONLY` keyword
        // before the table parse.
        // v7.14.0 — `ALTER SEQUENCE / ALTER VIEW / ALTER OWNER`
        // and the long PG-dump tail are accepted as no-ops.
        match self.advance() {
            Token::Index => {}
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("index") => {}
            // v6.7.2 — ALTER TABLE t SET hot_tier_bytes = X
            // v7.14.0 — ALTER TABLE ONLY t … strip the `ONLY`.
            Token::Table => {
                if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("only")) {
                    self.advance();
                }
                return self.parse_alter_table_after_keyword();
            }
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("table") => {
                if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("only")) {
                    self.advance();
                }
                return self.parse_alter_table_after_keyword();
            }
            // v7.17.0 — ALTER SEQUENCE name <options>. Moved out
            // of the silent-noop tail.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("sequence") => {
                return self.parse_alter_sequence_after_keyword();
            }
            // v7.14.0 — ALTER VIEW / ALTER FUNCTION / ALTER TYPE /
            // ALTER DOMAIN / ALTER DATABASE / ALTER USER / ALTER
            // ROLE / ALTER SCHEMA / ALTER OWNER / ALTER DEFAULT
            // PRIVILEGES — accept as no-op so pg_dump's tail loads.
            // v7.17.0 NOTE: ALTER SEQUENCE moved out (above).
            Token::Ident(s) | Token::QuotedIdent(s)
                if matches!(
                    s.to_ascii_lowercase().as_str(),
                    "view"
                        | "function"
                        | "type"
                        | "domain"
                        | "database"
                        | "role"
                        | "schema"
                        | "owner"
                        | "default"
                        | "extension"
                        | "materialized"
                        | "policy"
                        | "publication"
                        | "subscription"
                        // v7.37.17 (17.6 siblings) — additional ALTER
                        // targets pg_dump / pg_dumpall / operator DB
                        // migration scripts commonly emit. SPG has
                        // no matching machinery for any of these; the
                        // parser accepts + Empty-returns so pg_dump
                        // tail statements don't stall.
                        | "system"
                        | "user"
                        | "tablespace"
                        | "collation"
                        | "aggregate"
                        | "language"
                        | "operator"
                        | "conversion"
                        | "statistics"
                        | "server"
                        | "foreign"
                        | "text"
                        | "event"
                        | "large"
                ) =>
            {
                self.consume_until_statement_boundary();
                return Ok(Statement::Empty);
            }
            other => {
                return Err(self.err(format!(
                    "expected INDEX / TABLE / SEQUENCE / VIEW / FUNCTION / TYPE / OWNER / etc \
                     after ALTER, got {other:?}"
                )));
            }
        }
        // v7.16.2 — optional `IF EXISTS` after ALTER INDEX
        // (mailrs migrate-042 ships these). The presence of an
        // IF EXISTS makes the subsequent name lookup tolerate
        // a missing index — engine returns CommandOk no-op.
        let if_exists = if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("if")) {
            let next = self.tokens.get(self.pos + 1);
            if matches!(next, Some(Token::Ident(s)) if s.eq_ignore_ascii_case("exists")) {
                self.advance();
                self.advance();
                true
            } else {
                false
            }
        } else {
            false
        };
        let name = self.expect_ident_like()?;
        // v7.16.2 — RENAME TO new_name shape (mailrs migrate-042).
        // Detect BEFORE the REBUILD path so the existing REBUILD
        // arm stays untouched.
        if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("rename")) {
            self.advance();
            if matches!(self.peek(), Token::To) {
                self.advance();
            } else {
                self.expect_keyword_ident("to")?;
            }
            let new = self.expect_ident_like()?;
            return Ok(Statement::AlterIndex(crate::ast::AlterIndexStatement {
                name,
                target: crate::ast::AlterIndexTarget::Rename { new, if_exists },
            }));
        }
        // REBUILD
        self.expect_keyword_ident("rebuild")?;
        // Optional: WITH (encoding = <enc>)
        let encoding = if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("with")) {
            self.advance();
            if !matches!(self.peek(), Token::LParen) {
                return Err(self.err(format!(
                    "expected '(' after WITH in ALTER INDEX REBUILD, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            self.expect_keyword_ident("encoding")?;
            if !matches!(self.peek(), Token::Eq) {
                return Err(self.err(format!(
                    "expected '=' after encoding in ALTER INDEX REBUILD, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            let enc_ident = match self.advance() {
                Token::Ident(s) | Token::QuotedIdent(s) => s,
                other => {
                    return Err(self.err(format!("expected encoding name after =, got {other:?}")));
                }
            };
            let enc = match enc_ident.to_ascii_lowercase().as_str() {
                "f32" => VecEncoding::F32,
                "sq8" => VecEncoding::Sq8,
                "half" => VecEncoding::F16,
                other => {
                    return Err(self.err(format!(
                        "unknown vector encoding {other:?} in ALTER INDEX REBUILD; supported: F32, SQ8, HALF"
                    )));
                }
            };
            if !matches!(self.peek(), Token::RParen) {
                return Err(self.err(format!(
                    "expected ')' after encoding value, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            Some(enc)
        } else {
            None
        };
        Ok(Statement::AlterIndex(crate::ast::AlterIndexStatement {
            name,
            target: crate::ast::AlterIndexTarget::Rebuild { encoding },
        }))
    }

    /// v6.7.2 — `ALTER TABLE <name> SET hot_tier_bytes = <n>`. The
    /// only `SET` form currently supported; future v6.7.x can add
    /// more SET subjects without changing the dispatch shape.
    /// v7.13.2 — mailrs round-6 S1: accepts comma-separated
    /// subactions. Single-subaction shape stays a 1-element vec.
    fn parse_alter_table_after_keyword(&mut self) -> Result<Statement, ParseError> {
        let table_name = self.expect_ident_like()?;
        let mut targets: Vec<crate::ast::AlterTableTarget> = Vec::new();
        loop {
            let subaction = self.parse_alter_table_subaction()?;
            // ADD COLUMN with inline REFERENCES emits both an
            // AddColumn and an AddForeignKey subaction; the
            // helper returns 1 or 2 items.
            targets.extend(subaction);
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        Ok(Statement::AlterTable(crate::ast::AlterTableStatement {
            name: table_name,
            targets,
        }))
    }

    /// Parse one ALTER TABLE subaction. Returns a Vec because
    /// inline `REFERENCES` on `ADD COLUMN` produces both an
    /// AddColumn and an AddForeignKey entry (mailrs round-6 S3).
    fn parse_alter_table_subaction(
        &mut self,
    ) -> Result<Vec<crate::ast::AlterTableTarget>, ParseError> {
        match self.peek() {
            Token::Ident(s) if s.eq_ignore_ascii_case("set") => {
                self.advance();
                // v7.37.18 (18.7-18.15) — SET ( option = value, … )
                // storage parameters: paren-prefixed; consume.
                if matches!(self.peek(), Token::LParen) {
                    self.consume_until_statement_boundary();
                    return Ok(Vec::new());
                }
                let setting = self.expect_ident_like()?;
                if setting.eq_ignore_ascii_case("hot_tier_bytes") {
                    if !matches!(self.peek(), Token::Eq) {
                        return Err(self.err(alloc::format!(
                            "expected '=' after hot_tier_bytes, got {:?}",
                            self.peek()
                        )));
                    }
                    self.advance();
                    let n = self.expect_u64_literal()?;
                    return Ok(alloc::vec![crate::ast::AlterTableTarget::SetHotTierBytes(n)]);
                }
                // v7.37.18 (18.7 / 18.8 / 18.11 / 18.13 / 18.14) —
                // accept-and-no-op for ALTER TABLE SET <subject>
                // forms that pg_dump emits but SPG either treats
                // as N/A (single-tenant, single-owner, no shared
                // tablespaces) or accepts the dump-side declaration
                // without runtime effect:
                //   SET SCHEMA <name>            (18.11)
                //   SET TABLESPACE <name>        (18.8)
                //   SET LOGGED / UNLOGGED        (18.7 alt-form)
                //   SET WITHOUT CLUSTER          (18.13)
                //   SET WITHOUT OIDS             (PG legacy)
                //   SET (option = value, …)      (storage parameters)
                //   SET REPLICA IDENTITY {…}     (18.14)
                if setting.eq_ignore_ascii_case("schema")
                    || setting.eq_ignore_ascii_case("tablespace")
                    || setting.eq_ignore_ascii_case("logged")
                    || setting.eq_ignore_ascii_case("unlogged")
                    || setting.eq_ignore_ascii_case("without")
                {
                    self.consume_until_statement_boundary();
                    return Ok(Vec::new());
                }
                if setting.eq_ignore_ascii_case("replica") {
                    // SET REPLICA IDENTITY {DEFAULT|FULL|NOTHING|USING INDEX <name>}
                    self.consume_until_statement_boundary();
                    return Ok(Vec::new());
                }
                // SET (option=value, …) — storage parameters.
                if matches!(self.peek(), Token::LParen) {
                    self.consume_until_statement_boundary();
                    return Ok(Vec::new());
                }
                Err(self.err(alloc::format!(
                    "ALTER TABLE SET: unknown setting {setting:?}; supported: \
                     hot_tier_bytes / SCHEMA / TABLESPACE / LOGGED / UNLOGGED / \
                     WITHOUT CLUSTER / WITHOUT OIDS / REPLICA IDENTITY / (storage_params)"
                )))
            }
            // v7.37.18 (18.9) — ALTER TABLE INHERIT / NO INHERIT.
            // SPG doesn't support PG-style inheritance (declarative
            // partitioning v7.37.16 covers the common case); accept
            // and ignore.
            Token::Ident(s) if s.eq_ignore_ascii_case("inherit") => {
                self.advance();
                self.consume_until_statement_boundary();
                Ok(Vec::new())
            }
            Token::Ident(s) if s.eq_ignore_ascii_case("no") => {
                // `NO INHERIT <parent>`
                self.advance();
                self.consume_until_statement_boundary();
                Ok(Vec::new())
            }
            // v7.37.18 (18.10) — ALTER TABLE OWNER TO <user>. SPG
            // is single-owner; accept-and-no-op.
            Token::Ident(s) if s.eq_ignore_ascii_case("owner") => {
                self.advance();
                if matches!(self.peek(), Token::To) {
                    self.advance();
                }
                let _ = self.expect_ident_like().ok();
                Ok(Vec::new())
            }
            // v7.37.18 (18.13) — ALTER TABLE CLUSTER ON <index>.
            // PG sets a hint; SPG doesn't have clustered storage.
            // Accept-and-no-op.
            Token::Ident(s) if s.eq_ignore_ascii_case("cluster") => {
                self.advance();
                self.consume_until_statement_boundary();
                Ok(Vec::new())
            }
            // v7.37.18 (18.15) — ALTER TABLE VALIDATE CONSTRAINT
            // <name>. SPG validates inline at ADD CONSTRAINT time
            // (no NOT VALID / VALIDATE separation), so VALIDATE is
            // an accept-and-no-op for pg_dump round-trip.
            Token::Ident(s) if s.eq_ignore_ascii_case("validate") => {
                self.advance();
                self.consume_until_statement_boundary();
                Ok(Vec::new())
            }
            // v7.37.18 (18.18) — RESET ( option [, …] ). Inverse of
            // SET (option = value, …). PG uses it to clear per-table
            // storage params like fillfactor or autovacuum_*. SPG
            // engine-manages those parameters; accept-and-no-op.
            Token::Ident(s) if s.eq_ignore_ascii_case("reset") => {
                self.advance();
                self.consume_until_statement_boundary();
                Ok(Vec::new())
            }
            // v7.37.18 (18.18) — OF <type_name> / NOT OF. Composite-
            // type-of binding (PG 9.0+). SPG composite types
            // (v7.37.5 ζ-B sub-commit) follow CREATE TYPE; ALTER
            // TABLE OF is rare and inverse of CREATE TABLE OF.
            // Accept-and-no-op until a customer dump round-trips it.
            Token::Ident(s) if s.eq_ignore_ascii_case("of") => {
                self.advance();
                self.consume_until_statement_boundary();
                Ok(Vec::new())
            }
            // v7.37.18 (18.18) — `NOT OF` lexes NOT as Token::Not
            // (reserved keyword) rather than Token::Ident("not"),
            // so it needs its own arm. Accept-and-no-op same as OF.
            Token::Not => {
                self.advance();
                self.consume_until_statement_boundary();
                Ok(Vec::new())
            }
            // v7.37.18 (18.18) — FORCE / NO FORCE ROW LEVEL SECURITY
            // (PG 9.5+). SPG doesn't enforce RLS at the engine layer
            // yet (v7.41 roadmap); accept-and-no-op so RLS-aware
            // dumps load straight through.
            Token::Ident(s) if s.eq_ignore_ascii_case("force") => {
                self.advance();
                self.consume_until_statement_boundary();
                Ok(Vec::new())
            }
            // v7.37.18 (18.18) — ENABLE/DISABLE ROW LEVEL SECURITY
            // (PG 9.5+). The guard requires the next token to be
            // `ROW` so the v7.37.18.12 ENABLE/DISABLE TRIGGER arm
            // (further down) still matches its case unchanged.
            Token::Ident(s)
                if (s.eq_ignore_ascii_case("enable") || s.eq_ignore_ascii_case("disable"))
                    && matches!(
                        self.tokens.get(self.pos + 1),
                        Some(Token::Ident(t)) if t.eq_ignore_ascii_case("row")
                    ) =>
            {
                self.advance(); // ENABLE/DISABLE
                self.consume_until_statement_boundary();
                Ok(Vec::new())
            }
            Token::Ident(s) if s.eq_ignore_ascii_case("add") => {
                self.advance();
                // v7.14.0 — ADD CONSTRAINT <name> { FOREIGN KEY |
                // PRIMARY KEY | UNIQUE | CHECK }. pg_dump emits
                // PRIMARY KEY this way; mysqldump emits both.
                // Peek-only dispatch (no advance) — `advance()`
                // destructively replaces consumed tokens with Eof,
                // so saved-pos restore would land on Eofs.
                if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("constraint"))
                {
                    // The next-but-one ident is the constraint
                    // name; the one after THAT is the kind.
                    let kind_pos = self.pos + 2;
                    let kind = self.tokens.get(kind_pos).cloned();
                    if matches!(&kind, Some(Token::Ident(s)) if s.eq_ignore_ascii_case("foreign"))
                    {
                        let fk = self.parse_table_level_fk()?;
                        return Ok(alloc::vec![
                            crate::ast::AlterTableTarget::AddForeignKey(fk)
                        ]);
                    }
                    if matches!(&kind, Some(Token::Ident(s)) if s.eq_ignore_ascii_case("primary"))
                    {
                        self.advance(); // CONSTRAINT
                        let _name = self.expect_ident_like()?;
                        self.advance(); // PRIMARY
                        self.expect_keyword_ident("key")?;
                        let cols = self.parse_paren_ident_list("PRIMARY KEY")?;
                        return Ok(alloc::vec![
                            crate::ast::AlterTableTarget::AddTableConstraint(
                                crate::ast::TableConstraint::PrimaryKey {
                                    name: None,
                                    columns: cols,
                                }
                            )
                        ]);
                    }
                    if matches!(&kind, Some(Token::Ident(s)) if s.eq_ignore_ascii_case("unique"))
                    {
                        self.advance(); // CONSTRAINT
                        let _name = self.expect_ident_like()?;
                        // v7.22 (mailrs round-13 gap 6) — delegate so
                        // the optional `NULLS [NOT] DISTINCT` modifier
                        // parses here too (pg_dump emits the ALTER
                        // form; semantics enforced by the engine
                        // since v7.13).
                        let uc = self.parse_table_level_unique()?;
                        return Ok(alloc::vec![
                            crate::ast::AlterTableTarget::AddTableConstraint(uc)
                        ]);
                    }
                    if matches!(&kind, Some(Token::Ident(s)) if s.eq_ignore_ascii_case("check"))
                    {
                        self.advance(); // CONSTRAINT
                        let _name = self.expect_ident_like()?;
                        self.advance(); // CHECK
                        if !matches!(self.peek(), Token::LParen) {
                            return Err(self.err(alloc::format!(
                                "expected '(' after CHECK, got {:?}", self.peek()
                            )));
                        }
                        self.advance();
                        let expr = self.parse_expr(0)?;
                        if matches!(self.peek(), Token::RParen) {
                            self.advance();
                        }
                        return Ok(alloc::vec![
                            crate::ast::AlterTableTarget::AddTableConstraint(
                                crate::ast::TableConstraint::Check { name: None, expr }
                            )
                        ]);
                    }
                    // Unknown kind — fall through to FK path which
                    // produces a descriptive parse error.
                }
                let is_fk = matches!(
                    self.peek(),
                    Token::Ident(s) if s.eq_ignore_ascii_case("constraint")
                        || s.eq_ignore_ascii_case("foreign")
                );
                if is_fk {
                    let fk = self.parse_table_level_fk()?;
                    return Ok(alloc::vec![crate::ast::AlterTableTarget::AddForeignKey(fk)]);
                }
                // v7.14.0 — bare ADD PRIMARY KEY / UNIQUE / CHECK
                // (no CONSTRAINT prefix) — same dispatch.
                match self.peek().clone() {
                    Token::Ident(s) if s.eq_ignore_ascii_case("primary") => {
                        self.advance();
                        self.expect_keyword_ident("key")?;
                        let cols = self.parse_paren_ident_list("PRIMARY KEY")?;
                        return Ok(alloc::vec![
                            crate::ast::AlterTableTarget::AddTableConstraint(
                                crate::ast::TableConstraint::PrimaryKey {
                                    name: None,
                                    columns: cols,
                                }
                            )
                        ]);
                    }
                    Token::Ident(s) if s.eq_ignore_ascii_case("unique") => {
                        // v7.22 — delegate (NULLS [NOT] DISTINCT).
                        let uc = self.parse_table_level_unique()?;
                        return Ok(alloc::vec![
                            crate::ast::AlterTableTarget::AddTableConstraint(uc)
                        ]);
                    }
                    _ => {}
                }
                if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("column")) {
                    self.advance();
                }
                let mut if_not_exists = false;
                if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("if")) {
                    self.advance();
                    if !matches!(self.peek(), Token::Not) {
                        return Err(self.err(alloc::format!(
                            "expected NOT after IF in ALTER TABLE ADD COLUMN, got {:?}",
                            self.peek()
                        )));
                    }
                    self.advance();
                    if !matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("exists")) {
                        return Err(self.err(alloc::format!(
                            "expected EXISTS after IF NOT in ALTER TABLE ADD COLUMN, got {:?}",
                            self.peek()
                        )));
                    }
                    self.advance();
                    if_not_exists = true;
                }
                // v7.13.2 — mailrs round-6 S3: `ADD COLUMN col TYPE
                // REFERENCES other(col) [ON DELETE …]`. parse_column_def
                // returns ColumnDef + an optional inline FK.
                let (column, col_level_fk) = self.parse_column_def_with_fk()?;
                let col_name = column.name.clone();
                let mut out = alloc::vec![crate::ast::AlterTableTarget::AddColumn {
                    column,
                    if_not_exists,
                }];
                if let Some(mut fk) = col_level_fk {
                    if fk.columns.is_empty() {
                        fk.columns.push(col_name);
                    }
                    out.push(crate::ast::AlterTableTarget::AddForeignKey(fk));
                }
                Ok(out)
            }
            Token::Drop => {
                self.advance();
                // v7.13.3 — dispatch on the next token. mailrs round-7
                // S8 closed DROP COLUMN; round-6 S7 closed
                // DROP CONSTRAINT. Both share IF EXISTS / CASCADE /
                // RESTRICT modifiers.
                //   DROP CONSTRAINT [IF EXISTS] <name> [CASCADE|RESTRICT]
                //   DROP [COLUMN] [IF EXISTS] <col> [CASCADE|RESTRICT]
                let subject = match self.peek() {
                    Token::Ident(s) if s.eq_ignore_ascii_case("constraint") => {
                        self.advance();
                        "constraint"
                    }
                    Token::Ident(s) if s.eq_ignore_ascii_case("column") => {
                        self.advance();
                        "column"
                    }
                    // PG-canonical bare `DROP <col>` without COLUMN
                    // keyword is also valid; treat any other ident
                    // as the column name.
                    Token::Ident(_) | Token::QuotedIdent(_) => "column",
                    other => {
                        return Err(self.err(alloc::format!(
                            "expected COLUMN / CONSTRAINT after DROP in ALTER TABLE, got {other:?}"
                        )));
                    }
                };
                let mut if_exists = false;
                if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("if")) {
                    let n1 = self.tokens.get(self.pos + 1);
                    if matches!(n1, Some(Token::Ident(s)) if s.eq_ignore_ascii_case("exists")) {
                        self.advance();
                        self.advance();
                        if_exists = true;
                    }
                }
                let name = self.expect_ident_like()?;
                let mut cascade = false;
                if matches!(
                    self.peek(),
                    Token::Ident(s) if s.eq_ignore_ascii_case("cascade")
                        || s.eq_ignore_ascii_case("restrict")
                ) {
                    if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("cascade"))
                    {
                        cascade = true;
                    }
                    self.advance();
                }
                if subject == "constraint" {
                    Ok(alloc::vec![crate::ast::AlterTableTarget::DropForeignKey {
                        name,
                        if_exists,
                    }])
                } else {
                    Ok(alloc::vec![crate::ast::AlterTableTarget::DropColumn {
                        column: name,
                        if_exists,
                        cascade,
                    }])
                }
            }
            Token::Ident(s) if s.eq_ignore_ascii_case("alter") => {
                self.advance();
                // v7.37.18 (18.16) — `ALTER TABLE … ALTER CONSTRAINT
                // <name> {DEFERRABLE|NOT DEFERRABLE} [INITIALLY
                // {IMMEDIATE|DEFERRED}]`. SPG enforces constraints
                // immediately; accept-and-no-op.
                if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("constraint")) {
                    self.advance();
                    self.consume_until_statement_boundary();
                    return Ok(Vec::new());
                }
                if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("column")) {
                    self.advance();
                }
                let col_name = self.expect_ident_like()?;
                match self.peek() {
                    Token::Ident(s) if s.eq_ignore_ascii_case("type") => {
                        self.advance();
                    }
                    // v7.14.0 — pg_dump emits BIGSERIAL via
                    // `ALTER TABLE … ALTER COLUMN id SET DEFAULT
                    // nextval('seq')` (the sequence is created
                    // separately). SPG's BIGSERIAL already uses
                    // AUTO_INCREMENT; accept SET DEFAULT / DROP
                    // DEFAULT / SET NOT NULL / DROP NOT NULL as
                    // engine no-ops by consuming the tail.
                    Token::Ident(s) if s.eq_ignore_ascii_case("set") => {
                        // v7.22 (round-13 T2) — `SET DEFAULT
                        // nextval('…')` is how pg_dump spells a
                        // SERIAL column (plain integer in CREATE
                        // TABLE + this ALTER). It used to be
                        // swallowed as a no-op, which silently
                        // STRIPPED auto-increment from imported
                        // schemas — the first post-import INSERT
                        // without an explicit id then violated NOT
                        // NULL. Lower it to the auto-increment
                        // marker instead.
                        let is_default_nextval =
                            matches!(self.tokens.get(self.pos + 1), Some(Token::Default))
                                && matches!(
                                    self.tokens.get(self.pos + 2),
                                    Some(Token::Ident(f)) if f.eq_ignore_ascii_case("nextval")
                                );
                        if is_default_nextval {
                            let seq_name = self.scan_sequence_name_until_boundary();
                            return Ok(alloc::vec![
                                crate::ast::AlterTableTarget::SetColumnAutoIncrement {
                                    column: col_name,
                                    seq_name,
                                }
                            ]);
                        }
                        // v7.37.18 (18.1 + 18.2) — proper lowering.
                        self.advance(); // consume "set"
                        match self.peek().clone() {
                            Token::Default => {
                                self.advance();
                                let default_expr = self.parse_expr(0)?;
                                return Ok(alloc::vec![
                                    crate::ast::AlterTableTarget::AlterColumnSetDefault {
                                        column: col_name,
                                        default_expr,
                                    }
                                ]);
                            }
                            Token::Not => {
                                self.advance();
                                if !matches!(self.peek(), Token::Null) {
                                    return Err(self.err(alloc::format!(
                                        "expected NULL after ALTER COLUMN SET NOT, got {:?}",
                                        self.peek()
                                    )));
                                }
                                self.advance();
                                return Ok(alloc::vec![
                                    crate::ast::AlterTableTarget::AlterColumnSetNotNull {
                                        column: col_name,
                                    }
                                ]);
                            }
                            other => {
                                // Other SET subjects (STATISTICS,
                                // STORAGE, COMPRESSION, …) currently
                                // stay no-ops — they'll surface in
                                // their own 18.x sub-items.
                                let _ = other;
                                self.consume_until_statement_boundary();
                                return Ok(Vec::new());
                            }
                        }
                    }
                    Token::Ident(s) if s.eq_ignore_ascii_case("drop") => {
                        self.advance(); // consume "drop"
                        return self.parse_alter_column_drop_tail(col_name);
                    }
                    Token::Drop => {
                        self.advance(); // consume Drop token
                        return self.parse_alter_column_drop_tail(col_name);
                    }
                    Token::Ident(s) if s.eq_ignore_ascii_case("add") => {
                        // v7.22 (round-13 T2) — `ALTER COLUMN c ADD
                        // GENERATED { ALWAYS | BY DEFAULT } AS
                        // IDENTITY ( … )`: pg_dump's spelling for
                        // identity columns. Same auto-increment
                        // lowering as the nextval default; the
                        // sequence options inside the parens are
                        // no-ops under SPG's max+1 semantics.
                        let is_generated = matches!(
                            self.tokens.get(self.pos + 1),
                            Some(Token::Ident(g)) if g.eq_ignore_ascii_case("generated")
                        );
                        if !is_generated {
                            return Err(self.err(alloc::format!(
                                "expected GENERATED after ALTER COLUMN {col_name} ADD, got {:?}",
                                self.tokens.get(self.pos + 1)
                            )));
                        }
                        let seq_name = self.scan_sequence_name_until_boundary();
                        return Ok(alloc::vec![
                            crate::ast::AlterTableTarget::SetColumnAutoIncrement {
                                column: col_name,
                                seq_name,
                            }
                        ]);
                    }
                    other => {
                        return Err(self.err(alloc::format!(
                            "expected TYPE / SET / DROP / ADD after ALTER COLUMN <name>, got {other:?}"
                        )));
                    }
                }
                let new_type = self.parse_column_type_name()?;
                let using = if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("using"))
                {
                    self.advance();
                    Some(self.parse_expr(0)?)
                } else {
                    None
                };
                Ok(alloc::vec![crate::ast::AlterTableTarget::AlterColumnType {
                    column: col_name,
                    new_type,
                    using,
                }])
            }
            // v7.15.0 — `ALTER TABLE t RENAME [COLUMN] old TO new`.
            // PG also supports `RENAME TO new_table` for table-name
            // rename; that surface is deferred (pg_dump never emits
            // it). If the first post-RENAME ident is `TO`, the user
            // is asking for table rename — error with a clear
            // message rather than misparsing `TO` as a column name.
            Token::Ident(s) if s.eq_ignore_ascii_case("rename") => {
                self.advance();
                // v7.16.2 — `ALTER TABLE t RENAME TO new_table`
                // table-name rename (mailrs round-10 A.5 — used
                // by migrate-042's `RENAME TO email_contacts`).
                // `TO` lexes as Token::To.
                if matches!(self.peek(), Token::To)
                    || matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("to"))
                {
                    self.advance();
                    let new = self.expect_ident_like()?;
                    return Ok(alloc::vec![crate::ast::AlterTableTarget::RenameTable {
                        new,
                    }]);
                }
                if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("column")) {
                    self.advance();
                }
                let old = self.expect_ident_like()?;
                // `TO` is a reserved keyword token; accept both
                // Token::To and Token::Ident("to") for consistency.
                if matches!(self.peek(), Token::To) {
                    self.advance();
                } else {
                    self.expect_keyword_ident("to")?;
                }
                let new = self.expect_ident_like()?;
                Ok(alloc::vec![crate::ast::AlterTableTarget::RenameColumn {
                    old,
                    new,
                }])
            }
            // v7.16.1 — `ALTER TABLE t { ENABLE | DISABLE } TRIGGER
            // { ALL | <name> }`. pg_dump --disable-triggers wraps
            // every data block with these. Real disable semantics —
            // not no-op — because reload correctness assumes the
            // triggers don't fire (rows already carry their
            // computed values from prod).
            Token::Ident(s)
                if s.eq_ignore_ascii_case("enable") || s.eq_ignore_ascii_case("disable") =>
            {
                let enabled = s.eq_ignore_ascii_case("enable");
                self.advance();
                // PG also accepts ENABLE/DISABLE { REPLICA | ALWAYS }
                // TRIGGER … and ENABLE/DISABLE RULE / ROW LEVEL
                // SECURITY. v7.16.1 only matches TRIGGER (mailrs's
                // pg_dump output) — anything else falls through to
                // the catch-all error below.
                // v7.22 (round-13 T3) — mysqldump wraps every data
                // section in `/*!40000 ALTER TABLE t DISABLE KEYS */`
                // + ENABLE KEYS (a MyISAM index-rebuild hint). SPG
                // maintains indexes incrementally — engine no-op.
                if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("keys")) {
                    self.advance();
                    return Ok(Vec::new());
                }
                // v7.37.18 (18.12) — ENABLE/DISABLE ALWAYS TRIGGER
                // and ENABLE/DISABLE REPLICA TRIGGER. PG uses these
                // to gate triggers on session_replication_role; SPG
                // has no replica role, so the prefix is consumed and
                // treated identically to the plain ENABLE/DISABLE
                // TRIGGER form.
                if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("always"))
                    || matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("replica"))
                {
                    self.advance();
                }
                if !matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("trigger")) {
                    return Err(self.err(alloc::format!(
                        "expected TRIGGER after {}, got {:?}",
                        if enabled { "ENABLE" } else { "DISABLE" },
                        self.peek()
                    )));
                }
                self.advance();
                // `ALL` lexes as Token::All (reserved); also
                // accept Token::Ident("all") for symmetry.
                // v7.37.18 (18.12) — USER / REPLICA / ALWAYS post-
                // TRIGGER selectors. USER (= all user triggers) is
                // semantically ALL here; REPLICA / ALWAYS gate on
                // session_replication_role which SPG doesn't track.
                // All map to TriggerSelector::All.
                let which = if matches!(self.peek(), Token::All)
                    || matches!(self.peek(), Token::Ident(s)
                        if s.eq_ignore_ascii_case("all")
                            || s.eq_ignore_ascii_case("user")
                            || s.eq_ignore_ascii_case("replica")
                            || s.eq_ignore_ascii_case("always"))
                {
                    self.advance();
                    crate::ast::TriggerSelector::All
                } else {
                    let name = self.expect_ident_like()?;
                    crate::ast::TriggerSelector::Named(name)
                };
                Ok(alloc::vec![crate::ast::AlterTableTarget::SetTriggerEnabled {
                    which,
                    enabled,
                }])
            }
            // v7.37.16 (16.3) — ATTACH PARTITION child <bounds>
            Token::Ident(s) if s.eq_ignore_ascii_case("attach") => {
                self.advance();
                if !matches!(self.peek(), Token::Partition)
                    && !matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                        if s.eq_ignore_ascii_case("partition"))
                {
                    return Err(self.err(alloc::format!(
                        "expected PARTITION after ATTACH, got {:?}",
                        self.peek()
                    )));
                }
                self.advance();
                let child = self.expect_ident_like()?;
                let bounds = self.parse_partition_bounds_tail()?;
                Ok(alloc::vec![
                    crate::ast::AlterTableTarget::AttachPartition { child, bounds }
                ])
            }
            // v7.37.16 (16.4 + 16.5) — DETACH PARTITION child [CONCURRENTLY] [FINALIZE]
            Token::Ident(s) if s.eq_ignore_ascii_case("detach") => {
                self.advance();
                if !matches!(self.peek(), Token::Partition)
                    && !matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                        if s.eq_ignore_ascii_case("partition"))
                {
                    return Err(self.err(alloc::format!(
                        "expected PARTITION after DETACH, got {:?}",
                        self.peek()
                    )));
                }
                self.advance();
                let child = self.expect_ident_like()?;
                let mut concurrently = false;
                let mut finalize = false;
                loop {
                    match self.peek().clone() {
                        Token::Ident(s) | Token::QuotedIdent(s)
                            if s.eq_ignore_ascii_case("concurrently") =>
                        {
                            self.advance();
                            concurrently = true;
                        }
                        Token::Ident(s) | Token::QuotedIdent(s)
                            if s.eq_ignore_ascii_case("finalize") =>
                        {
                            self.advance();
                            finalize = true;
                        }
                        _ => break,
                    }
                }
                Ok(alloc::vec![crate::ast::AlterTableTarget::DetachPartition {
                    child,
                    concurrently,
                    finalize,
                }])
            }
            other => Err(self.err(alloc::format!(
                "expected SET / ADD / DROP / ALTER / RENAME / ENABLE / DISABLE / ATTACH / DETACH in ALTER TABLE, got {other:?}"
            ))),
        }
    }

    /// v7.37.16 (16.3) — parse the `FOR VALUES …` / `DEFAULT`
    /// tail used by both CREATE TABLE … PARTITION OF and ALTER
    /// TABLE … ATTACH PARTITION. Shares the same grammar as
    /// `parse_partition_of_tail`'s bounds branch.
    /// v7.37.18 (18.1 + 18.2) — parse the tail of `ALTER COLUMN
    /// col DROP …`. Accepts `DROP DEFAULT` and `DROP NOT NULL`,
    /// lowering each to the respective AlterTableTarget. Any
    /// other DROP subject (IDENTITY, EXPRESSION, etc.) stays a
    /// no-op via consume_until_statement_boundary.
    fn parse_alter_column_drop_tail(
        &mut self,
        col_name: String,
    ) -> Result<Vec<crate::ast::AlterTableTarget>, ParseError> {
        match self.peek().clone() {
            Token::Default => {
                self.advance();
                Ok(alloc::vec![
                    crate::ast::AlterTableTarget::AlterColumnDropDefault { column: col_name }
                ])
            }
            Token::Not => {
                self.advance();
                if !matches!(self.peek(), Token::Null) {
                    return Err(self.err(alloc::format!(
                        "expected NULL after ALTER COLUMN DROP NOT, got {:?}",
                        self.peek()
                    )));
                }
                self.advance();
                Ok(alloc::vec![
                    crate::ast::AlterTableTarget::AlterColumnDropNotNull { column: col_name }
                ])
            }
            _ => {
                self.consume_until_statement_boundary();
                Ok(Vec::new())
            }
        }
    }

    fn parse_partition_bounds_tail(&mut self) -> Result<crate::ast::PartitionOfBoundsAst, ParseError> {
        use crate::ast::PartitionOfBoundsAst;
        match self.peek() {
            Token::Default => {
                self.advance();
                Ok(PartitionOfBoundsAst::Default)
            }
            Token::For => {
                self.advance();
                if !matches!(self.peek(), Token::Values) {
                    return Err(
                        self.err(format!("expected VALUES after FOR, got {:?}", self.peek()))
                    );
                }
                self.advance();
                let want_with = matches!(
                    self.peek(),
                    Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("with")
                );
                if want_with {
                    self.advance();
                    if !matches!(self.peek(), Token::LParen) {
                        return Err(self.err(format!(
                            "expected '(' after FOR VALUES WITH, got {:?}",
                            self.peek()
                        )));
                    }
                    self.advance();
                    let (mut modulus, mut remainder): (Option<u32>, Option<u32>) = (None, None);
                    loop {
                        let key = self.expect_ident_like()?;
                        let n = match self.peek().clone() {
                            Token::Integer(v) if v >= 0 && v <= i64::from(u32::MAX) => {
                                self.advance();
                                v as u32
                            }
                            other => {
                                return Err(self.err(format!(
                                    "FOR VALUES WITH: expected unsigned integer literal, got {other:?}"
                                )));
                            }
                        };
                        match key.to_ascii_uppercase().as_str() {
                            "MODULUS" => modulus = Some(n),
                            "REMAINDER" => remainder = Some(n),
                            other => {
                                return Err(self.err(format!(
                                    "FOR VALUES WITH: unknown key {other:?}; \
                                     expected MODULUS or REMAINDER"
                                )));
                            }
                        }
                        match self.peek() {
                            Token::Comma => {
                                self.advance();
                            }
                            Token::RParen => {
                                self.advance();
                                break;
                            }
                            other => {
                                return Err(self.err(format!(
                                    "expected ',' or ')' in FOR VALUES WITH list, got {other:?}"
                                )));
                            }
                        }
                    }
                    let modulus =
                        modulus.ok_or_else(|| self.err("FOR VALUES WITH: missing MODULUS".to_string()))?;
                    let remainder = remainder
                        .ok_or_else(|| self.err("FOR VALUES WITH: missing REMAINDER".to_string()))?;
                    if modulus == 0 {
                        return Err(self.err("FOR VALUES WITH: MODULUS must be > 0".to_string()));
                    }
                    if remainder >= modulus {
                        return Err(self.err(format!(
                            "FOR VALUES WITH: REMAINDER ({remainder}) must be < MODULUS ({modulus})"
                        )));
                    }
                    return Ok(PartitionOfBoundsAst::Hash { modulus, remainder });
                }
                match self.peek() {
                    Token::From => {
                        self.advance();
                        let lower = Box::new(self.parse_partition_bound_expr()?);
                        if !matches!(self.peek(), Token::To) {
                            return Err(self.err(format!(
                                "expected TO after FROM (...), got {:?}",
                                self.peek()
                            )));
                        }
                        self.advance();
                        let upper = Box::new(self.parse_partition_bound_expr()?);
                        Ok(PartitionOfBoundsAst::Range { lower, upper })
                    }
                    Token::In => {
                        self.advance();
                        if !matches!(self.peek(), Token::LParen) {
                            return Err(self.err(format!(
                                "expected '(' after FOR VALUES IN, got {:?}",
                                self.peek()
                            )));
                        }
                        self.advance();
                        let mut values = Vec::new();
                        loop {
                            values.push(self.parse_expr(0)?);
                            match self.peek() {
                                Token::Comma => {
                                    self.advance();
                                }
                                Token::RParen => {
                                    self.advance();
                                    break;
                                }
                                other => {
                                    return Err(self.err(format!(
                                        "expected ',' or ')' in FOR VALUES IN list, got {other:?}"
                                    )));
                                }
                            }
                        }
                        if values.is_empty() {
                            return Err(self.err(
                                "FOR VALUES IN requires at least one literal".to_string(),
                            ));
                        }
                        Ok(PartitionOfBoundsAst::List { values })
                    }
                    other => Err(self.err(format!(
                        "expected FROM / IN / WITH after FOR VALUES, got {other:?}"
                    ))),
                }
            }
            other => Err(self.err(format!(
                "expected DEFAULT or FOR VALUES after ATTACH PARTITION child, got {other:?}"
            ))),
        }
    }

    /// v7.16.2 — peek for `information_schema.<tbl>` /
    /// `pg_catalog.<tbl>` triples and, if matched, consume all
    /// three tokens + return a synthetic table name the engine's
    /// SELECT path recognises as a virtual view. Returns `None`
    /// when the head doesn't look like a meta-qualified name.
    /// Used by `parse_table_ref` to bypass the
    /// `expect_ident_like` schema-strip for these specific PG
    /// meta schemas (mailrs round-10 A.3).
    fn try_peek_meta_qualified(&mut self) -> Option<String> {
        // Extract the schema name. Must be a plain ident token.
        let schema = match self.tokens.get(self.pos) {
            Some(Token::Ident(s) | Token::QuotedIdent(s)) => s.clone(),
            _ => return None,
        };
        // Dot.
        if !matches!(self.tokens.get(self.pos + 1), Some(Token::Dot)) {
            return None;
        }
        // The table-side ident may lex as a reserved keyword
        // (e.g. `Token::Tables`). Tolerate the common ones via a
        // helper that reads the trailing token's underlying name.
        let tbl = match self.tokens.get(self.pos + 2)? {
            Token::Ident(t) | Token::QuotedIdent(t) => t.clone(),
            Token::Tables => "tables".to_string(),
            // Other PG meta table names that may collide with
            // reserved keywords land here as needed.
            _ => return None,
        };
        // Strip the `pg_` prefix from `pg_catalog.pg_class`-style
        // names so the synthetic name doesn't double-prefix
        // (`__spg_pg_class`, not `__spg_pg_pg_class`).
        let (prefix, normalised) = if schema.eq_ignore_ascii_case("information_schema") {
            ("__spg_info_", tbl.to_ascii_lowercase())
        } else if schema.eq_ignore_ascii_case("pg_catalog") {
            let bare = tbl
                .to_ascii_lowercase()
                .strip_prefix("pg_")
                .map(alloc::string::String::from)
                .unwrap_or_else(|| tbl.to_ascii_lowercase());
            ("__spg_pg_", bare)
        } else if schema.eq_ignore_ascii_case("mysql") {
            // v7.17.0 Phase 3.P0-65 — MySQL system schema
            // (`mysql.user`, `mysql.db`). Same synthetic-name
            // shape as pg_catalog.
            ("__spg_mysql_", tbl.to_ascii_lowercase())
        } else {
            return None;
        };
        self.advance(); // schema
        self.advance(); // dot
        self.advance(); // tbl
        Some(alloc::format!("{prefix}{normalised}"))
    }

    /// Unqualified PG meta-table names (`FROM pg_extension`, `FROM
    /// pg_class`) resolve the same way: PG puts `pg_catalog` at the
    /// implicit front of every search_path, so a bare reference to a
    /// known catalog table always means the catalog table. Only the
    /// names the engine actually synthesises are recognised — any
    /// other `pg_*` ident stays a user table (mailrs embed round-12).
    fn try_peek_meta_bare(&mut self) -> Option<String> {
        const PG_META_TABLES: &[&str] = &[
            "pg_attribute",
            "pg_class",
            "pg_constraint",
            "pg_database",
            "pg_extension",
            "pg_index",
            "pg_indexes",
            "pg_matviews",
            "pg_namespace",
            "pg_proc",
            "pg_roles",
            "pg_settings",
            "pg_trigger",
            "pg_type",
            "pg_user",
            "pg_views",
        ];
        let name = match self.tokens.get(self.pos) {
            Some(Token::Ident(s)) => s.to_ascii_lowercase(),
            _ => return None,
        };
        // A following dot means this ident is a schema qualifier,
        // not a table name — let the qualified path handle it.
        if matches!(self.tokens.get(self.pos + 1), Some(Token::Dot)) {
            return None;
        }
        if !PG_META_TABLES.contains(&name.as_str()) {
            return None;
        }
        self.advance();
        let bare = name.strip_prefix("pg_").unwrap_or(&name);
        Some(alloc::format!("__spg_pg_{bare}"))
    }

    /// Consume a bare ident if its lowercase matches `kw`, else err.
    fn expect_keyword_ident(&mut self, kw: &str) -> Result<(), ParseError> {
        match self.advance() {
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case(kw) => Ok(()),
            other => Err(ParseError {
                message: format!("expected {kw:?}, got {other:?}"),
                token_pos: self.pos.saturating_sub(1),
            }),
        }
    }

    /// Accept either a quoted identifier (`"foo"`) or a quoted string
    /// literal (`'foo'`) — same shape used by CREATE USER for the
    /// username slot.
    fn expect_ident_or_string(&mut self) -> Result<String, ParseError> {
        match self.advance() {
            Token::Ident(s) | Token::QuotedIdent(s) | Token::String(s) => Ok(s),
            other => Err(ParseError {
                message: format!("expected identifier or string, got {other:?}"),
                token_pos: self.pos.saturating_sub(1),
            }),
        }
    }

    fn expect_string_literal(&mut self) -> Result<String, ParseError> {
        match self.advance() {
            Token::String(s) => Ok(s),
            other => Err(ParseError {
                message: format!("expected quoted string, got {other:?}"),
                token_pos: self.pos.saturating_sub(1),
            }),
        }
    }

    fn parse_select_stmt(&mut self) -> Result<Statement, ParseError> {
        // v7.30.2 (mailrs round-25 ask 2) — derived tables /
        // subqueries recurse through here without passing
        // parse_expr; share the same nesting budget.
        self.enter_nested()?;
        let r = self.parse_select_stmt_inner();
        self.nest_depth -= 1;
        r
    }

    fn parse_select_stmt_inner(&mut self) -> Result<Statement, ParseError> {
        // Caller dispatches on Token::Select; the inner helper handles
        // the rest. ORDER BY / LIMIT bind at this top level; UNION peers
        // get a fresh bare-select parse and may not have their own ORDER
        // BY / LIMIT.
        let mut head = self.parse_bare_select()?;
        self.parse_setop_chain_into(&mut head)?;
        self.parse_select_tail_into(&mut head)?;
        Ok(Statement::Select(head))
    }

    /// v7.37.17 (17.6 siblings) — the three SQL set operations
    /// share the peer chain: UNION [ALL], EXCEPT [ALL] (a reserved
    /// token), and INTERSECT [ALL] (a bare ident — it was never
    /// reserved in SPG's lexer). PG precedence: INTERSECT binds
    /// tighter than UNION / EXCEPT — the executor folds the chain
    /// left-to-right, which is already correct for LEADING
    /// intersects; an INTERSECT pair that FOLLOWS a union/except
    /// pair nests into that previous peer, so A UNION B INTERSECT C
    /// = A ∪ (B ∩ C). Shared by the top level and parenthesized
    /// groups.
    fn parse_setop_chain_into(&mut self, head: &mut SelectStatement) -> Result<(), ParseError> {
        // A parenthesized group arrives with its own (already
        // regrouped) unions on `head`; only the pairs THIS chain
        // appends participate in the precedence regroup below —
        // nesting an outer INTERSECT into a group-internal peer
        // would dissolve the explicit grouping.
        let boundary = head.unions.len();
        loop {
            let base = match self.peek() {
                Token::Union => UnionKind::Distinct,
                Token::Except => UnionKind::Except,
                Token::Ident(s) if s.eq_ignore_ascii_case("intersect") => UnionKind::Intersect,
                _ => break,
            };
            self.advance();
            let kind = if matches!(self.peek(), Token::All) {
                self.advance();
                match base {
                    UnionKind::Distinct => UnionKind::All,
                    UnionKind::Except => UnionKind::ExceptAll,
                    _ => UnionKind::IntersectAll,
                }
            } else {
                base
            };
            let peer = self.parse_bare_select()?;
            head.unions.push((kind, peer));
        }
        let mut pairs = core::mem::take(&mut head.unions);
        let tail = pairs.split_off(boundary);
        let mut regrouped: Vec<(UnionKind, SelectStatement)> = pairs;
        for (kind, peer) in tail {
            let is_intersect = matches!(kind, UnionKind::Intersect | UnionKind::IntersectAll);
            // An intersect nests into the previous element of THIS
            // chain only; with no new previous element it stays at
            // the outer level (the left fold applies it to the
            // whole head, group included).
            match (is_intersect, regrouped.len() > boundary, regrouped.last_mut()) {
                (true, true, Some((_, prev))) => prev.unions.push((kind, peer)),
                _ => regrouped.push((kind, peer)),
            }
        }
        head.unions = regrouped;
        Ok(())
    }


    /// v7.37.17 (17.6 siblings) — the shared SELECT tail: ORDER BY /
    /// LIMIT / OFFSET / FETCH FIRST / FOR-lock clauses. Extracted so
    /// the top-level bare VALUES statement reuses it verbatim.
    fn parse_select_tail_into(&mut self, head: &mut SelectStatement) -> Result<(), ParseError> {
        head.order_by = if matches!(self.peek(), Token::Order) {
            self.advance();
            if !matches!(self.peek(), Token::By) {
                return Err(self.err(format!("expected BY after ORDER, got {:?}", self.peek())));
            }
            self.advance();
            // v6.4.0 — multi-key ORDER BY. Loop over comma-separated
            // `<expr> [ASC|DESC]` items.
            let mut keys = Vec::new();
            loop {
                let expr = self.parse_expr(0)?;
                let desc = if matches!(self.peek(), Token::Desc) {
                    self.advance();
                    true
                } else if matches!(self.peek(), Token::Asc) {
                    self.advance();
                    false
                } else {
                    false
                };
                // v7.24 (round-16 A) — explicit NULLS FIRST/LAST.
                let nulls_first = self.parse_optional_nulls_placement()?;
                keys.push(OrderBy {
                    expr,
                    desc,
                    nulls_first,
                });
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
            keys
        } else {
            Vec::new()
        };
        head.limit = if matches!(self.peek(), Token::Limit) {
            self.advance();
            // v7.17.0 Phase 5.1 — `LIMIT NULL` / `LIMIT ALL` are
            // PG synonyms for "no limit". Treat both as None
            // (no head.limit set) so the engine's existing
            // unlimited-result path takes over. Reject was the
            // pre-5.1 behaviour and broke pg_dump-flavoured
            // tooling that occasionally emits LIMIT NULL.
            if self.consume_limit_unbounded_sentinel() {
                None
            } else {
                Some(self.parse_limit_expr("LIMIT")?)
            }
        } else {
            None
        };
        head.offset = if matches!(self.peek(), Token::Offset) {
            self.advance();
            // PG also accepts an optional `ROW` / `ROWS` trailer
            // after the offset value (`OFFSET 10 ROWS`). The
            // FETCH-FIRST branch below relies on the same.
            let off = self.parse_limit_expr("OFFSET")?;
            self.consume_optional_rows_keyword();
            Some(off)
        } else {
            None
        };
        // v7.17.0 Phase 5.1 — `FETCH FIRST <int|$N> ROWS ONLY` is
        // the SQL-standard alias for LIMIT. PG accepts both
        // spellings interchangeably; pg_dump emits FETCH FIRST in
        // newer versions. We map it onto `head.limit` so the
        // engine path is unified.
        if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("fetch"))
        {
            self.advance(); // FETCH
            // `FIRST` or `NEXT` (both legal per SQL standard).
            if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                if s.eq_ignore_ascii_case("first") || s.eq_ignore_ascii_case("next"))
            {
                self.advance();
            }
            // Count (optional in the bare `FETCH FIRST ROW ONLY` —
            // implicit 1 — but we always consume one if present).
            let count = if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                if s.eq_ignore_ascii_case("row") || s.eq_ignore_ascii_case("rows"))
            {
                // Bare `FETCH FIRST ROW ONLY` = LIMIT 1.
                crate::ast::LimitExpr::Literal(1)
            } else {
                self.parse_limit_expr("FETCH FIRST")?
            };
            // Eat `ROW` / `ROWS` if not already consumed above.
            self.consume_optional_rows_keyword();
            // Optional `ONLY` (the spec form) — or the SQL:2008
            // `WITH TIES` form. v7.17.0 Phase 3.P0-49: the executor
            // now honours WITH TIES by extending past the LIMIT
            // truncation point through every row that shares the
            // last-kept row's ORDER BY key.
            if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                if s.eq_ignore_ascii_case("only"))
            {
                self.advance();
            } else if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                if s.eq_ignore_ascii_case("with"))
            {
                self.advance(); // WITH
                if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                    if s.eq_ignore_ascii_case("ties"))
                {
                    self.advance();
                    head.limit_with_ties = true;
                }
            }
            head.limit = Some(count);
        }
        // v7.17.0 Phase 3.4 — trailing row-lock clauses:
        //   FOR { UPDATE | NO KEY UPDATE | SHARE | KEY SHARE }
        //       [ OF table_name [, …] ]
        //       [ NOWAIT | SKIP LOCKED ]
        // Multiple FOR clauses may stack (PG: `FOR UPDATE OF t1
        // FOR SHARE OF t2`). SPG is a single-writer engine — every
        // SELECT already returns a consistent snapshot — so these
        // are accept-and-discard: the parser absorbs them so
        // mailrs / Rails / Django code paths that emit `SELECT
        // … FOR UPDATE` for advisory pessimistic locking load
        // without a parser error. The on-disk locking model is
        // unchanged; callers that rely on FOR UPDATE for read-
        // through-write ordering still get the right answer
        // because SPG serialises writes anyway.
        self.consume_optional_for_lock_clauses();
        Ok(())
    }

    /// v7.17.0 Phase 3.4 — eat zero or more `FOR { UPDATE | NO KEY
    /// UPDATE | SHARE | KEY SHARE } [ OF tbl[, …] ] [ NOWAIT | SKIP
    /// LOCKED ]` trailers. Each clause is fully accepted and
    /// discarded — SPG's single-writer model already satisfies the
    /// callers' implicit ordering requirement. Stops at the first
    /// token that isn't `FOR`.
    fn consume_optional_for_lock_clauses(&mut self) {
        while matches!(self.peek(), Token::For) {
            // v7.37.14 (A2.5-stub) — record that this query asked
            // for a row lock the parser is about to silently
            // discard. Operators surface the count via
            // `spg_sql::silent_for_update_count()` so they can
            // gauge how much of the workload depends on advisory
            // FOR UPDATE / FOR SHARE / FOR KEY SHARE semantics
            // before v7.37.15's per-row tuple locking lands.
            crate::record_silent_for_update_clause();
            self.advance(); // FOR
            // `NO KEY` prefix (PG) — `NO` is reserved-keyword-shaped
            // (`Token::Not` isn't it; PG `NO` lexes as Token::Ident).
            if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                if s.eq_ignore_ascii_case("no"))
            {
                self.advance(); // NO
                // The next ident should be KEY but be generous;
                // anything followed by UPDATE/SHARE is accepted.
                if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                    if s.eq_ignore_ascii_case("key"))
                {
                    self.advance(); // KEY
                }
            }
            // `KEY` prefix (PG `FOR KEY SHARE`).
            if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                if s.eq_ignore_ascii_case("key"))
            {
                self.advance(); // KEY
            }
            // Lock-strength keyword: UPDATE / SHARE. Required, but
            // we're lenient — an unexpected token here just bails
            // (we already consumed FOR; caller's downstream
            // dispatch will error if anything actually depends on
            // the trailing tokens).
            if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                if s.eq_ignore_ascii_case("update") || s.eq_ignore_ascii_case("share"))
            {
                self.advance();
            } else {
                // FOR by itself (or `FOR KEY` with nothing after) —
                // give up on the lock-clause path. We've already
                // advanced past FOR; further attempts to parse
                // here would clobber state.
                return;
            }
            // Optional `OF tbl[, tbl …]`. mailrs emits this when
            // joining and locking only a subset of tables.
            if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                if s.eq_ignore_ascii_case("of"))
            {
                self.advance(); // OF
                #[allow(clippy::while_let_loop)]
                loop {
                    match self.peek() {
                        Token::Ident(_) | Token::QuotedIdent(_) => {
                            self.advance();
                            // Optional schema-qualified `schema.table`.
                            if matches!(self.peek(), Token::Dot) {
                                self.advance();
                                if matches!(self.peek(), Token::Ident(_) | Token::QuotedIdent(_)) {
                                    self.advance();
                                }
                            }
                        }
                        _ => break,
                    }
                    if matches!(self.peek(), Token::Comma) {
                        self.advance();
                    } else {
                        break;
                    }
                }
            }
            // Optional `NOWAIT` | `SKIP LOCKED`.
            match self.peek().clone() {
                Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("nowait") => {
                    self.advance();
                }
                Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("skip") => {
                    self.advance(); // SKIP
                    if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                        if s.eq_ignore_ascii_case("locked"))
                    {
                        self.advance(); // LOCKED
                    }
                }
                _ => {}
            }
            // Loop: PG allows multiple FOR clauses chained.
        }
    }

    /// v7.9.24 — accept `LIMIT <int>` or `LIMIT $N`. mailrs H2.
    /// Bind value gets resolved during prepared-statement Execute;
    /// the Pratt expression parser would over-accept here (e.g.
    /// `LIMIT 5 + 5`), so we narrowly accept only the two PG forms.
    /// v7.17.0 Phase 5.1 — consume the `LIMIT NULL` / `LIMIT ALL`
    /// sentinel tokens (PG synonyms for "no limit"). Returns true
    /// when one was consumed; caller skips the regular
    /// limit-value parse and leaves `head.limit` at None.
    fn consume_limit_unbounded_sentinel(&mut self) -> bool {
        if matches!(self.peek(), Token::Null) {
            self.advance();
            return true;
        }
        if matches!(self.peek(), Token::All) {
            self.advance();
            return true;
        }
        false
    }

    /// v7.17.0 Phase 5.1 — eat an optional trailing `ROW` / `ROWS`
    /// keyword after a LIMIT / OFFSET / FETCH FIRST value, the
    /// SQL-standard shape. No-op when missing.
    fn consume_optional_rows_keyword(&mut self) {
        if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
            if s.eq_ignore_ascii_case("row") || s.eq_ignore_ascii_case("rows"))
        {
            self.advance();
        }
    }

    fn parse_limit_expr(&mut self, label: &str) -> Result<crate::ast::LimitExpr, ParseError> {
        match self.advance() {
            Token::Integer(n) if n >= 0 => u32::try_from(n)
                .map(crate::ast::LimitExpr::Literal)
                .map_err(|_| ParseError {
                    message: alloc::format!("{label} value too large: {n}"),
                    token_pos: self.pos.saturating_sub(1),
                }),
            Token::Placeholder(n) => Ok(crate::ast::LimitExpr::Placeholder(n)),
            other => Err(ParseError {
                message: alloc::format!(
                    "expected non-negative integer or $N placeholder after {label}, got {other:?}"
                ),
                token_pos: self.pos.saturating_sub(1),
            }),
        }
    }

    /// Parse one SELECT block without ORDER BY / LIMIT / UNION chaining —
    /// just `[DISTINCT] items [FROM] [WHERE] [GROUP BY]`. Returned with
    /// `unions` empty and `order_by` / `limit` `None`; the top-level
    /// `parse_select_stmt` is responsible for filling those in.
    /// v7.37.17 (17.6 siblings) — rewrite every `grouping(keys…)`
    /// call in the expression tree to the per-set integer bitmask
    /// (PG semantics: one bit per argument, MSB first; 1 = the key
    /// is dropped in this grouping set). Runs during the ROLLUP /
    /// CUBE / GROUPING SETS expansion, where the set is known.
    fn substitute_grouping_calls(expr: &mut Expr, dropped: &[Expr]) {
        if let Expr::FunctionCall { name, args } = expr
            && name.eq_ignore_ascii_case("grouping")
        {
            let mut mask: i64 = 0;
            for a in args.iter() {
                mask <<= 1;
                if dropped.iter().any(|d| d == a) {
                    mask |= 1;
                }
            }
            *expr = Expr::Literal(Literal::Integer(mask));
            return;
        }
        // Generic recursion over the common expression shapes the
        // SELECT list uses; anything without child expressions is
        // left alone.
        match expr {
            Expr::FunctionCall { args, .. } => {
                for a in args {
                    Self::substitute_grouping_calls(a, dropped);
                }
            }
            Expr::Binary { lhs, rhs, .. } => {
                Self::substitute_grouping_calls(lhs, dropped);
                Self::substitute_grouping_calls(rhs, dropped);
            }
            Expr::Unary { expr: inner, .. } => {
                Self::substitute_grouping_calls(inner, dropped);
            }
            Expr::Cast { expr: inner, .. } => {
                Self::substitute_grouping_calls(inner, dropped);
            }
            Expr::Case {
                operand,
                branches,
                else_branch,
            } => {
                if let Some(op) = operand {
                    Self::substitute_grouping_calls(op, dropped);
                }
                for (w, t) in branches {
                    Self::substitute_grouping_calls(w, dropped);
                    Self::substitute_grouping_calls(t, dropped);
                }
                if let Some(e) = else_branch {
                    Self::substitute_grouping_calls(e, dropped);
                }
            }
            _ => {}
        }
    }

    fn parse_bare_select(&mut self) -> Result<SelectStatement, ParseError> {
        // v7.37.17 (17.6 siblings) — parenthesized set-operation
        // group: `( <select chain> )` usable anywhere a query block
        // is (head or peer of an outer chain). The group's own
        // unions ride the returned SelectStatement; the executor's
        // nested-peer recursion runs them.
        if matches!(self.peek(), Token::LParen)
            && matches!(
                self.tokens.get(self.pos + 1),
                Some(Token::Select | Token::LParen)
            )
        {
            self.advance(); // (
            self.enter_nested()?;
            let mut head = self.parse_bare_select().and_then(|mut h| {
                self.parse_setop_chain_into(&mut h)?;
                Ok(h)
            });
            self.nest_depth -= 1;
            let mut head = match &mut head {
                Ok(h) => core::mem::take(h),
                Err(_) => return head,
            };
            // v7.37.17 (17.6 siblings) — group-internal tail:
            // `(A UNION B ORDER BY 1 LIMIT 5)`. Parse it into the
            // group head, then wrap the group as a derived table
            // (SELECT * FROM (group)) so the outer chain / outer
            // tail can't clobber the group's own ordering or limit.
            let has_tail = matches!(
                self.peek(),
                Token::Order | Token::Limit | Token::Offset
            ) || matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                    if s.eq_ignore_ascii_case("fetch"));
            if has_tail {
                self.parse_select_tail_into(&mut head)?;
                head = SelectStatement {
                    ctes: Vec::new(),
                    distinct: false,
                    distinct_on: Vec::new(),
                    items: alloc::vec![SelectItem::Wildcard],
                    from: Some(FromClause {
                        primary: TableRef {
                            name: "subquery".to_string(),
                            alias: None,
                            as_of_segment: None,
                            unnest_expr: None,
                            unnest_column_aliases: Vec::new(),
                            generate_series_args: None,
                            lateral_subquery: Some(Box::new(head)),
                            jsonb_each_text_arg: None,
                        },
                        joins: Vec::new(),
                    }),
                    where_: None,
                    group_by: None,
                    group_by_all: false,
                    having: None,
                    unions: Vec::new(),
                    order_by: Vec::new(),
                    limit: None,
                    offset: None,
                    limit_with_ties: false,
                };
            }
            if !matches!(self.peek(), Token::RParen) {
                return Err(self.err(format!(
                    "expected ')' after parenthesized query group, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            return Ok(head);
        }
        if !matches!(self.peek(), Token::Select) {
            return Err(self.err(format!(
                "expected SELECT to start a query block, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let distinct = if matches!(self.peek(), Token::Distinct) {
            self.advance();
            true
        } else {
            false
        };
        // v7.37.17 (17.6 siblings) — `DISTINCT ON (expr [, …])`:
        // keep the first row (per ORDER BY) of each group the
        // expressions define. Django's .distinct('field') shape.
        let distinct_on: Vec<Expr> = if distinct && matches!(self.peek(), Token::On) {
            self.advance(); // ON
            if !matches!(self.peek(), Token::LParen) {
                return Err(self.err(format!(
                    "expected '(' after DISTINCT ON, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            let mut exprs = Vec::new();
            loop {
                exprs.push(self.parse_expr(0)?);
                match self.peek() {
                    Token::Comma => {
                        self.advance();
                    }
                    Token::RParen => break,
                    other => {
                        return Err(self.err(format!(
                            "expected ',' or ')' in DISTINCT ON list, got {other:?}"
                        )));
                    }
                }
            }
            self.advance(); // )
            exprs
        } else {
            Vec::new()
        };
        let items = self.parse_select_list()?;
        let from = if matches!(self.peek(), Token::From) {
            self.advance();
            Some(self.parse_from_clause()?)
        } else {
            None
        };
        let where_ = if matches!(self.peek(), Token::Where) {
            self.advance();
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        let mut group_by_all = false;
        // v7.37.17 (17.6 siblings) — ROLLUP / CUBE / GROUPING SETS
        // share one expansion: `grouping_sets` lists the key subsets
        // (first = primary, assigned to stmt.group_by; the rest
        // become UNION ALL peers), `grouping_universe` is the full
        // key list used to compute each peer's dropped keys.
        let mut grouping_sets: Vec<Vec<Expr>> = Vec::new();
        let mut grouping_universe: Vec<Expr> = Vec::new();
        let group_by = if matches!(self.peek(), Token::Group) {
            self.advance();
            if !matches!(self.peek(), Token::By) {
                return Err(self.err(format!("expected BY after GROUP, got {:?}", self.peek())));
            }
            self.advance();
            // v6.4.1 — `GROUP BY ALL` shortcut. Planner expands to
            // every non-aggregate SELECT-list item later.
            if matches!(self.peek(), Token::All) {
                self.advance();
                group_by_all = true;
                None
            } else if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                if s.eq_ignore_ascii_case("rollup") || s.eq_ignore_ascii_case("cube"))
                && matches!(self.tokens.get(self.pos + 1), Some(Token::LParen))
            {
                // GROUP BY ROLLUP(a, b) — prefix subsets;
                // GROUP BY CUBE(a, b) — every subset.
                let is_cube = matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                    if s.eq_ignore_ascii_case("cube"));
                self.advance(); // ROLLUP / CUBE
                self.advance(); // (
                let mut keys = Vec::new();
                loop {
                    keys.push(self.parse_expr(0)?);
                    match self.peek() {
                        Token::Comma => {
                            self.advance();
                        }
                        Token::RParen => break,
                        other => {
                            return Err(self.err(format!(
                                "expected ',' or ')' in grouping list, got {other:?}"
                            )));
                        }
                    }
                }
                self.advance(); // )
                grouping_universe = keys.clone();
                if is_cube {
                    // All subsets, largest first (mask high→low
                    // keeps the full set as the primary).
                    let n = keys.len();
                    let mut subsets: Vec<Vec<Expr>> = (0..(1u32 << n))
                        .map(|mask| {
                            keys.iter()
                                .enumerate()
                                .filter(|(i, _)| mask & (1 << *i) != 0)
                                .map(|(_, k)| k.clone())
                                .collect()
                        })
                        .collect();
                    subsets.sort_by_key(|s| core::cmp::Reverse(s.len()));
                    grouping_sets = subsets;
                } else {
                    grouping_sets = (0..=keys.len())
                        .rev()
                        .map(|keep| keys[..keep].to_vec())
                        .collect();
                }
                Some(keys)
            } else if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                if s.eq_ignore_ascii_case("grouping"))
                && matches!(self.tokens.get(self.pos + 1), Some(Token::Ident(s2) | Token::QuotedIdent(s2))
                    if s2.eq_ignore_ascii_case("sets"))
            {
                // GROUP BY GROUPING SETS ((a, b), (a), ()) — the
                // explicit list; an empty () set is the grand total.
                self.advance(); // GROUPING
                self.advance(); // SETS
                if !matches!(self.peek(), Token::LParen) {
                    return Err(self.err(format!(
                        "expected '(' after GROUPING SETS, got {:?}",
                        self.peek()
                    )));
                }
                self.advance(); // outer (
                let mut sets: Vec<Vec<Expr>> = Vec::new();
                loop {
                    if !matches!(self.peek(), Token::LParen) {
                        return Err(self.err(format!(
                            "expected '(' to start a grouping set, got {:?}",
                            self.peek()
                        )));
                    }
                    self.advance(); // inner (
                    let mut set = Vec::new();
                    if !matches!(self.peek(), Token::RParen) {
                        loop {
                            set.push(self.parse_expr(0)?);
                            match self.peek() {
                                Token::Comma => {
                                    self.advance();
                                }
                                Token::RParen => break,
                                other => {
                                    return Err(self.err(format!(
                                        "expected ',' or ')' in grouping set, got {other:?}"
                                    )));
                                }
                            }
                        }
                    }
                    self.advance(); // inner )
                    sets.push(set);
                    match self.peek() {
                        Token::Comma => {
                            self.advance();
                        }
                        Token::RParen => break,
                        other => {
                            return Err(self.err(format!(
                                "expected ',' or ')' after a grouping set, got {other:?}"
                            )));
                        }
                    }
                }
                self.advance(); // outer )
                // Universe = every key that appears in any set.
                let mut universe: Vec<Expr> = Vec::new();
                for set in &sets {
                    for k in set {
                        if !universe.iter().any(|u| u == k) {
                            universe.push(k.clone());
                        }
                    }
                }
                grouping_universe = universe;
                let primary = sets.first().cloned().unwrap_or_default();
                grouping_sets = sets;
                if primary.is_empty() {
                    None
                } else {
                    Some(primary)
                }
            } else {
                let mut groups = Vec::new();
                loop {
                    groups.push(self.parse_expr(0)?);
                    if matches!(self.peek(), Token::Comma) {
                        self.advance();
                    } else {
                        break;
                    }
                }
                Some(groups)
            }
        } else {
            None
        };
        let having = if matches!(self.peek(), Token::Having) {
            self.advance();
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        let mut stmt = SelectStatement {
            ctes: Vec::new(),
            distinct,
            distinct_on,
            items,
            from,
            where_,
            group_by,
            group_by_all,
            having,
            unions: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            limit_with_ties: false,
        };
        // Grouping expansion (ROLLUP / CUBE / GROUPING SETS): the
        // first set is the primary (already on stmt.group_by); each
        // further set becomes a UNION ALL peer with its dropped
        // keys (universe minus the set) replaced by NULL literals
        // in the peer's items and group_by. PG-legal: non-grouped
        // select items must be group keys or aggregates, so a
        // dropped key's occurrences in the projection are exactly
        // the ones to nullify.
        if grouping_sets.len() > 1 {
            // The primary set's own dropped keys nullify in the
            // HEAD's projection too (GROUPING SETS's first set may
            // omit keys other sets use).
            let primary = grouping_sets[0].clone();
            let head_dropped: Vec<Expr> = grouping_universe
                .iter()
                .filter(|u| !primary.iter().any(|k| k == *u))
                .cloned()
                .collect();
            for set in grouping_sets.iter().skip(1) {
                let mut peer = stmt.clone();
                peer.unions = Vec::new();
                let dropped: Vec<&Expr> = grouping_universe
                    .iter()
                    .filter(|u| !set.iter().any(|k| k == *u))
                    .collect();
                peer.group_by = if set.is_empty() {
                    None
                } else {
                    Some(set.clone())
                };
                let dropped_owned: Vec<Expr> = dropped.iter().map(|d| (*d).clone()).collect();
                for item in &mut peer.items {
                    if let SelectItem::Expr { expr, .. } = item {
                        if dropped.iter().any(|d| *d == expr) {
                            *expr = Expr::Literal(Literal::Null);
                        } else {
                            Self::substitute_grouping_calls(expr, &dropped_owned);
                        }
                    }
                }
                if let Some(h) = &mut peer.having {
                    Self::substitute_grouping_calls(h, &dropped_owned);
                }
                stmt.unions.push((UnionKind::All, peer));
            }
            for item in &mut stmt.items {
                if let SelectItem::Expr { expr, .. } = item {
                    if head_dropped.iter().any(|d| d == expr) {
                        *expr = Expr::Literal(Literal::Null);
                    } else {
                        Self::substitute_grouping_calls(expr, &head_dropped);
                    }
                }
            }
            if let Some(h) = &mut stmt.having {
                Self::substitute_grouping_calls(h, &head_dropped);
            }
        }
        Ok(stmt)
    }

    fn parse_create_table_stmt_after_create(&mut self) -> Result<Statement, ParseError> {
        // Caller already consumed CREATE; we're sitting on TABLE.
        debug_assert!(matches!(self.peek(), Token::Table));
        self.advance();
        let if_not_exists = self.consume_if_not_exists();
        let name = self.expect_ident_like()?;
        // v7.37.6-B — `CREATE TABLE c PARTITION OF parent <bounds>`
        // child shape has no column list; the child inherits its
        // columns from the parent at engine-DDL time. Detect it
        // before the `(` requirement below.
        if matches!(self.peek(), Token::Partition)
            && Self::tokens_match_ident_ci(self.tokens.get(self.pos + 1), "of")
        {
            self.advance(); // PARTITION
            self.advance(); // of
            let partition_of = self.parse_partition_of_tail()?;
            return Ok(Statement::CreateTable(CreateTableStatement {
                name,
                columns: Vec::new(),
                if_not_exists,
                foreign_keys: Vec::new(),
                table_constraints: Vec::new(),
                partition_by: None,
                partition_of: Some(partition_of),
            }));
        }
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(format!(
                "expected '(' after table name, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let mut columns = Vec::new();
        let mut foreign_keys: Vec<ForeignKeyConstraint> = Vec::new();
        let mut table_constraints: Vec<crate::ast::TableConstraint> = Vec::new();
        loop {
            // v7.6.0 / v7.9.18 — distinguish table-level constraint
            // clauses from column definitions. Constraints start
            // with `CONSTRAINT <name> …`, `FOREIGN KEY (…)`,
            // `PRIMARY KEY (…)`, or `UNIQUE (…)`. Anything else is
            // a column.
            if self.peek_table_level_pk_start() {
                table_constraints.push(self.parse_table_level_primary_key()?);
            } else if self.peek_table_level_unique_start() {
                table_constraints.push(self.parse_table_level_unique()?);
            } else if self.peek_table_level_check_start() {
                // v7.13.0 — table-level CHECK (mailrs round-5 G3).
                table_constraints.push(self.parse_table_level_check()?);
            } else if self.peek_mysql_inline_key_start() {
                // v7.14.0 — mysqldump emits inline `KEY name (cols)`,
                // `INDEX name (cols)`, `UNIQUE KEY name (cols)`,
                // `FULLTEXT KEY name (cols)`, `SPATIAL KEY name (cols)`
                // inside the column list. Skip name + paren list;
                // for UNIQUE KEY, register as a UC.
                if let Some(uc) = self.parse_mysql_inline_key()? {
                    table_constraints.push(uc);
                }
            } else if let Some(kind) = self.peek_named_table_constraint_kind() {
                // v7.22 (mailrs round-13 gap 5) — `CONSTRAINT <name>
                // { CHECK | UNIQUE | PRIMARY KEY }`: every pg_dump'd
                // CHECK is named, and the named-CONSTRAINT arm used
                // to accept FOREIGN KEY only. The name is accepted
                // and discarded — same handling as every other SPG
                // constraint name.
                self.advance(); // CONSTRAINT
                let _name = self.expect_ident_like()?;
                table_constraints.push(match kind {
                    NamedTableConstraintKind::Check => self.parse_table_level_check()?,
                    NamedTableConstraintKind::Unique => self.parse_table_level_unique()?,
                    NamedTableConstraintKind::PrimaryKey => self.parse_table_level_primary_key()?,
                });
            } else if self.peek_constraint_or_fk_start() {
                foreign_keys.push(self.parse_table_level_fk()?);
            } else {
                let (col, col_level_fk) = self.parse_column_def_with_fk()?;
                // v7.13.0 — fold inline UNIQUE / CHECK column
                // constraints into table-level entries so the
                // engine path stays uniform.
                if col.is_unique {
                    table_constraints.push(crate::ast::TableConstraint::Unique {
                        name: None,
                        columns: alloc::vec![col.name.clone()],
                        nulls_not_distinct: false,
                    });
                }
                if let Some(check_expr) = col.check.clone() {
                    table_constraints.push(crate::ast::TableConstraint::Check {
                        name: None,
                        expr: check_expr,
                    });
                }
                columns.push(col);
                if let Some(fk) = col_level_fk {
                    foreign_keys.push(fk);
                }
            }
            match self.peek() {
                Token::Comma => {
                    self.advance();
                }
                Token::RParen => {
                    self.advance();
                    break;
                }
                other => {
                    return Err(
                        self.err(format!("expected ',' or ')' in column list, got {other:?}"))
                    );
                }
            }
        }
        if columns.is_empty() {
            return Err(self.err("CREATE TABLE requires at least one column".into()));
        }
        // v7.14.0 — consume MySQL/MariaDB table options after the
        // closing `)`. mysqldump emits things like
        // `ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci
        // AUTO_INCREMENT=42 ROW_FORMAT=DYNAMIC COMMENT='blog posts'`.
        // SPG accepts all forms as no-ops (each option is
        // `<ident> [=] <ident-or-string>` separated by whitespace).
        self.consume_mysql_table_options();
        // v7.37.6-B — declarative-partition-parent suffix
        // (`PARTITION BY RANGE (key_col)`) sits after the column
        // list + MySQL table-options. v7.37.6-B only accepts RANGE
        // and locks the key column at one ident; the engine then
        // verifies the column type is TIMESTAMPTZ.
        let partition_by = if matches!(self.peek(), Token::Partition) {
            self.advance(); // PARTITION
            if !matches!(self.peek(), Token::By) {
                return Err(self.err(format!(
                    "expected BY after PARTITION, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            Some(self.parse_partition_by_tail()?)
        } else {
            None
        };
        Ok(Statement::CreateTable(CreateTableStatement {
            name,
            columns,
            if_not_exists,
            foreign_keys,
            table_constraints,
            partition_by,
            partition_of: None,
        }))
    }

    /// v7.37.6-B — case-insensitive ident match helper for the
    /// `PARTITION OF` / `MINVALUE` / `MAXVALUE` keywords. They lex
    /// as `Token::Ident("of"/"minvalue"/"maxvalue")` because we
    /// didn't burn a global keyword slot for each (see the
    /// `Token::Partition` doc-comment in `lexer.rs`).
    fn tokens_match_ident_ci(t: Option<&Token>, want: &str) -> bool {
        matches!(t, Some(Token::Ident(s) | Token::QuotedIdent(s)) if s.eq_ignore_ascii_case(want))
    }

    /// v7.37.6-B — after `PARTITION BY`, expect `RANGE (key_col [, ...])`.
    /// v7.37.16 (16.1/16.2) — extended to LIST + HASH.
    fn parse_partition_by_tail(&mut self) -> Result<crate::ast::PartitionBySpec, ParseError> {
        use crate::ast::{PartitionBySpec, PartitionKindAst};
        let kind = match self.peek() {
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("range") => {
                self.advance();
                PartitionKindAst::Range
            }
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("list") => {
                self.advance();
                PartitionKindAst::List
            }
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("hash") => {
                self.advance();
                PartitionKindAst::Hash
            }
            other => {
                return Err(self.err(format!(
                    "PARTITION BY: expected RANGE / LIST / HASH, got {other:?}"
                )));
            }
        };
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(format!(
                "expected '(' after PARTITION BY <strategy>, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let mut key_columns = Vec::new();
        loop {
            key_columns.push(self.expect_ident_like()?);
            match self.peek() {
                Token::Comma => {
                    self.advance();
                }
                Token::RParen => {
                    self.advance();
                    break;
                }
                other => {
                    return Err(self.err(format!(
                        "expected ',' or ')' in PARTITION BY key list, got {other:?}"
                    )));
                }
            }
        }
        if key_columns.is_empty() {
            return Err(self.err(
                "PARTITION BY requires at least one key column".to_string(),
            ));
        }
        Ok(PartitionBySpec { kind, key_columns })
    }

    /// v7.37.6-B — after `PARTITION OF`, expect
    ///   <parent> FOR VALUES FROM ( <expr> ) TO ( <expr> )
    /// or
    ///   <parent> DEFAULT
    fn parse_partition_of_tail(&mut self) -> Result<crate::ast::PartitionOfSpec, ParseError> {
        use crate::ast::{PartitionOfBoundsAst, PartitionOfSpec};
        let parent_name = self.expect_ident_like()?;
        // v7.37.6-B rejects an explicit column list — the child
        // inherits from the parent. mailrs round-7 taught us that
        // CREATE TABLE-side schema reconciliation hides drift, so
        // we surface this as a parse error rather than silently
        // ignoring user columns.
        if matches!(self.peek(), Token::LParen) {
            return Err(self.err(
                "CREATE TABLE … PARTITION OF parent: explicit column list not supported \
                 at v7.37.6-B; the child inherits its columns from the parent"
                    .to_string(),
            ));
        }
        let bounds = match self.peek() {
            Token::Default => {
                self.advance();
                PartitionOfBoundsAst::Default
            }
            Token::For => {
                self.advance();
                if !matches!(self.peek(), Token::Values) {
                    return Err(
                        self.err(format!("expected VALUES after FOR, got {:?}", self.peek()))
                    );
                }
                self.advance();
                // WITH is not a reserved Token in the lexer — it lexes
                // as Token::Ident("with"). Disambiguate manually.
                let want_with = matches!(
                    self.peek(),
                    Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("with")
                );
                if want_with {
                    self.advance();
                    if !matches!(self.peek(), Token::LParen) {
                        return Err(self.err(format!(
                            "expected '(' after FOR VALUES WITH, got {:?}",
                            self.peek()
                        )));
                    }
                    self.advance();
                    let (mut modulus, mut remainder): (Option<u32>, Option<u32>) = (None, None);
                    loop {
                        let key = self.expect_ident_like()?;
                        let n = match self.peek().clone() {
                            Token::Integer(v) if v >= 0 && v <= i64::from(u32::MAX) => {
                                self.advance();
                                v as u32
                            }
                            other => {
                                return Err(self.err(format!(
                                    "FOR VALUES WITH: expected unsigned integer literal, got {other:?}"
                                )));
                            }
                        };
                        match key.to_ascii_uppercase().as_str() {
                            "MODULUS" => modulus = Some(n),
                            "REMAINDER" => remainder = Some(n),
                            other => {
                                return Err(self.err(format!(
                                    "FOR VALUES WITH: unknown key {other:?}; \
                                     expected MODULUS or REMAINDER"
                                )));
                            }
                        }
                        match self.peek() {
                            Token::Comma => {
                                self.advance();
                            }
                            Token::RParen => {
                                self.advance();
                                break;
                            }
                            other => {
                                return Err(self.err(format!(
                                    "expected ',' or ')' in FOR VALUES WITH list, got {other:?}"
                                )));
                            }
                        }
                    }
                    let modulus = modulus.ok_or_else(|| {
                        self.err("FOR VALUES WITH: missing MODULUS".to_string())
                    })?;
                    let remainder = remainder.ok_or_else(|| {
                        self.err("FOR VALUES WITH: missing REMAINDER".to_string())
                    })?;
                    if modulus == 0 {
                        return Err(self.err(
                            "FOR VALUES WITH: MODULUS must be > 0".to_string(),
                        ));
                    }
                    if remainder >= modulus {
                        return Err(self.err(format!(
                            "FOR VALUES WITH: REMAINDER ({remainder}) \
                             must be < MODULUS ({modulus})"
                        )));
                    }
                    PartitionOfBoundsAst::Hash { modulus, remainder }
                } else {
                match self.peek() {
                    Token::From => {
                        self.advance();
                        let lower = Box::new(self.parse_partition_bound_expr()?);
                        if !matches!(self.peek(), Token::To) {
                            return Err(self.err(format!(
                                "expected TO after FROM (...), got {:?}",
                                self.peek()
                            )));
                        }
                        self.advance();
                        let upper = Box::new(self.parse_partition_bound_expr()?);
                        PartitionOfBoundsAst::Range { lower, upper }
                    }
                    // v7.37.16 (16.1) — FOR VALUES IN (lit [, lit, …])
                    Token::In => {
                        self.advance();
                        if !matches!(self.peek(), Token::LParen) {
                            return Err(self.err(format!(
                                "expected '(' after FOR VALUES IN, got {:?}",
                                self.peek()
                            )));
                        }
                        self.advance();
                        let mut values = Vec::new();
                        loop {
                            values.push(self.parse_expr(0)?);
                            match self.peek() {
                                Token::Comma => {
                                    self.advance();
                                }
                                Token::RParen => {
                                    self.advance();
                                    break;
                                }
                                other => {
                                    return Err(self.err(format!(
                                        "expected ',' or ')' in FOR VALUES IN list, got {other:?}"
                                    )));
                                }
                            }
                        }
                        if values.is_empty() {
                            return Err(self.err(
                                "FOR VALUES IN requires at least one literal".to_string(),
                            ));
                        }
                        PartitionOfBoundsAst::List { values }
                    }
                    other => {
                        return Err(self.err(format!(
                            "expected FROM / IN / WITH after FOR VALUES, got {other:?}"
                        )));
                    }
                }
                }
            }
            other => {
                return Err(self.err(format!(
                    "expected FOR VALUES or DEFAULT after PARTITION OF parent, got {other:?}"
                )));
            }
        };
        Ok(PartitionOfSpec {
            parent_name,
            bounds,
        })
    }

    /// v7.37.6-B — a single `( <expr> )` bound. `MINVALUE` /
    /// `MAXVALUE` lex as Ident; rewrite them into FunctionCall
    /// markers (no-arg builtins) so the engine resolves them
    /// against [`spg_storage::PartitionBound::{MinValue, MaxValue}`].
    fn parse_partition_bound_expr(&mut self) -> Result<crate::ast::Expr, ParseError> {
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(format!(
                "expected '(' before partition bound, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let expr = match self.peek() {
            Token::Ident(s) | Token::QuotedIdent(s)
                if s.eq_ignore_ascii_case("minvalue") || s.eq_ignore_ascii_case("maxvalue") =>
            {
                let name = s.to_ascii_uppercase();
                self.advance();
                crate::ast::Expr::FunctionCall {
                    name,
                    args: Vec::new(),
                }
            }
            _ => self.parse_expr(0)?,
        };
        if !matches!(self.peek(), Token::RParen) {
            return Err(self.err(format!(
                "expected ')' after partition bound, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        Ok(expr)
    }

    /// v7.14.0 — true when the next tokens look like an inline
    /// MySQL index declaration: KEY / INDEX / UNIQUE KEY /
    /// UNIQUE INDEX / FULLTEXT [KEY|INDEX] / SPATIAL [KEY|INDEX]
    /// — each followed by an optional name + `(...)`. Critical:
    /// a column NAMED `key` / `index` (PG accepts as ident) must
    /// NOT be mistaken for the KEY constraint shape. We disambig
    /// by requiring the keyword to be followed by either `(` or
    /// `<ident> (`.
    fn peek_mysql_inline_key_start(&self) -> bool {
        let cur = self.peek();
        // Shapes:
        //   KEY (cols)
        //   KEY name (cols)
        //   INDEX (cols)
        //   INDEX name (cols)
        //   UNIQUE KEY [name] (cols)
        //   UNIQUE INDEX [name] (cols)
        //   FULLTEXT [KEY|INDEX] [name] (cols)
        //   SPATIAL [KEY|INDEX] [name] (cols)
        let after_keyword_followed_by_paren_or_ident_paren = |skip: usize| -> bool {
            // tokens at skip = the position AFTER the index-form
            // keywords (KEY/INDEX) have been consumed.
            match self.tokens.get(skip) {
                Some(Token::LParen) => true,
                Some(Token::Ident(_) | Token::QuotedIdent(_)) => {
                    matches!(self.tokens.get(skip + 1), Some(Token::LParen))
                }
                _ => false,
            }
        };
        // `INDEX` lexes as Token::Index (reserved), not as
        // Token::Ident("index"). Both shapes count as a KEY/INDEX
        // start; the peek helper below handles either.
        let is_key_or_index_tok = |t: &Token| -> bool {
            matches!(t, Token::Index)
                || matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("key") || s.eq_ignore_ascii_case("index"))
        };
        match cur {
            Token::Index => after_keyword_followed_by_paren_or_ident_paren(self.pos + 1),
            Token::Ident(s) if s.eq_ignore_ascii_case("key") || s.eq_ignore_ascii_case("index") => {
                after_keyword_followed_by_paren_or_ident_paren(self.pos + 1)
            }
            Token::Ident(s)
                if s.eq_ignore_ascii_case("fulltext") || s.eq_ignore_ascii_case("spatial") =>
            {
                let nxt = self.tokens.get(self.pos + 1);
                let after_after = if nxt.is_some_and(is_key_or_index_tok) {
                    self.pos + 2
                } else {
                    self.pos + 1
                };
                after_keyword_followed_by_paren_or_ident_paren(after_after)
            }
            Token::Ident(s) if s.eq_ignore_ascii_case("unique") => {
                let nxt = self.tokens.get(self.pos + 1);
                if !nxt.is_some_and(is_key_or_index_tok) {
                    return false;
                }
                after_keyword_followed_by_paren_or_ident_paren(self.pos + 2)
            }
            _ => false,
        }
    }

    /// v7.14.0 — parse the MySQL inline KEY/INDEX form. Returns
    /// Some(TableConstraint::Unique) for UNIQUE KEY (so SPG
    /// enforces uniqueness on INSERT). v7.15.0: plain KEY/INDEX
    /// returns Some(TableConstraint::Index) so the engine builds
    /// a real BTree index on the leading column (mysqldump
    /// `KEY idx_posts_author (author_id)` shape).
    /// FULLTEXT / SPATIAL still return None — accepted-as-no-op
    /// (the storage layer has no matching AM).
    fn parse_mysql_inline_key(
        &mut self,
    ) -> Result<Option<crate::ast::TableConstraint>, ParseError> {
        // Detect UNIQUE prefix.
        let is_unique = if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("unique"))
        {
            self.advance();
            true
        } else {
            false
        };
        // Consume FULLTEXT / SPATIAL prefix and record which one
        // it was. v7.17.0 Phase 2.2 — FULLTEXT routes through a
        // dedicated TableConstraint variant so the engine can
        // build a tsvector-GIN; SPATIAL still has no matching
        // AM, so it falls back to accept-as-no-op.
        let mut is_fulltext = false;
        let mut is_spatial = false;
        if let Token::Ident(s) = self.peek().clone() {
            if s.eq_ignore_ascii_case("fulltext") {
                self.advance();
                is_fulltext = true;
            } else if s.eq_ignore_ascii_case("spatial") {
                self.advance();
                is_spatial = true;
            }
        }
        // KEY / INDEX keyword. `INDEX` lexes as Token::Index
        // (reserved); accept either token shape.
        match self.peek() {
            Token::Index => {
                self.advance();
            }
            Token::Ident(s) if s.eq_ignore_ascii_case("key") || s.eq_ignore_ascii_case("index") => {
                self.advance();
            }
            other => {
                return Err(self.err(alloc::format!(
                    "expected KEY/INDEX in inline index declaration, got {other:?}"
                )));
            }
        }
        // Optional index name (an ident before the `(`).
        // v7.15.0 — capture the name when present so the engine
        // builds the secondary index under the user's chosen
        // name (matches mysqldump's `KEY idx_x (col)` shape).
        let mut idx_name: Option<String> = None;
        if matches!(self.peek(), Token::Ident(_) | Token::QuotedIdent(_))
            && matches!(self.tokens.get(self.pos + 1), Some(Token::LParen))
        {
            if let Token::Ident(s) | Token::QuotedIdent(s) = self.advance() {
                idx_name = Some(s);
            }
        }
        // Optional `USING BTREE` / `USING HASH` (MySQL).
        if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("using")) {
            self.advance();
            if matches!(self.peek(), Token::Ident(_) | Token::QuotedIdent(_)) {
                self.advance();
            }
        }
        // Required column list `(col [, col]*)`.
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(alloc::format!(
                "expected '(' in inline KEY/INDEX, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let mut cols: Vec<String> = Vec::new();
        while let Token::Ident(s) | Token::QuotedIdent(s) = self.peek().clone() {
            self.advance();
            cols.push(s);
            // Skip optional `(length)` per-column prefix.
            if matches!(self.peek(), Token::LParen) {
                let mut depth = 1usize;
                self.advance();
                while depth > 0 {
                    match self.peek() {
                        Token::LParen => depth += 1,
                        Token::RParen => depth -= 1,
                        Token::Eof => break,
                        _ => {}
                    }
                    self.advance();
                }
            }
            // Skip optional ASC / DESC.
            if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("asc") || s.eq_ignore_ascii_case("desc"))
                || matches!(self.peek(), Token::Asc | Token::Desc)
            {
                self.advance();
            }
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        if matches!(self.peek(), Token::RParen) {
            self.advance();
        }
        // Trailing options on the inline index — comment / etc.
        // Skip until comma or `)`.
        while !matches!(self.peek(), Token::Comma | Token::RParen | Token::Eof) {
            self.advance();
        }
        if cols.is_empty() {
            return Ok(None);
        }
        if is_unique {
            // Carry the captured idx_name on UNIQUE too so future
            // engine work can name the underlying BTree
            // accordingly; today the unique-constraint installer
            // synthesises the name itself, but Display round-trip
            // benefits from preserving it.
            Ok(Some(crate::ast::TableConstraint::Unique {
                name: idx_name,
                columns: cols,
                nulls_not_distinct: false,
            }))
        } else if is_fulltext {
            // v7.17.0 Phase 2.2 — MySQL `FULLTEXT KEY` now
            // routes through `TableConstraint::FulltextIndex`;
            // the engine builds a tsvector-GIN over each named
            // column so MATCH AGAINST gets a real inverted
            // index instead of a silently-dropped declaration.
            Ok(Some(crate::ast::TableConstraint::FulltextIndex {
                name: idx_name,
                columns: cols,
            }))
        } else if is_spatial {
            // SPG has no native SPATIAL AM. Accept-as-no-op
            // (declaration is parsed, but no index is built).
            Ok(None)
        } else {
            // v7.15.0 — plain KEY / INDEX builds a real BTree
            // secondary index.
            Ok(Some(crate::ast::TableConstraint::Index {
                name: idx_name,
                columns: cols,
            }))
        }
    }

    /// v7.14.0 — consume MySQL/MariaDB table-options tail after
    /// the closing `)`: ENGINE=..., DEFAULT CHARSET=...,
    /// COLLATE=..., AUTO_INCREMENT=N, ROW_FORMAT=..., COMMENT='...'
    /// (in any order, separated by whitespace).
    fn consume_mysql_table_options(&mut self) {
        loop {
            // Heuristic: a table option is an ident (or `DEFAULT`
            // reserved keyword) followed by `=` and an
            // ident / string / integer.
            let name_lc = match self.peek().clone() {
                Token::Ident(s) | Token::QuotedIdent(s) => s.to_ascii_lowercase(),
                Token::Default => alloc::string::String::from("default"),
                _ => break,
            };
            let known = matches!(
                name_lc.as_str(),
                "engine"
                    | "default"
                    | "charset"
                    | "collate"
                    | "auto_increment"
                    | "row_format"
                    | "comment"
                    | "pack_keys"
                    | "stats_persistent"
                    | "stats_auto_recalc"
                    | "stats_sample_pages"
                    | "key_block_size"
                    | "tablespace"
                    | "min_rows"
                    | "max_rows"
                    | "checksum"
                    | "delay_key_write"
                    | "insert_method"
                    | "data"
                    | "index"
                    | "encryption"
                    | "compression"
            );
            if !known {
                break;
            }
            self.advance(); // option name
            // `DEFAULT` optional prefix is followed by `CHARSET` /
            // `COLLATE`; consume the next ident too.
            if name_lc == "default" {
                if matches!(self.peek(), Token::Ident(_) | Token::QuotedIdent(_)) {
                    self.advance();
                }
            }
            if matches!(self.peek(), Token::Eq) {
                self.advance();
            }
            match self.peek() {
                Token::Ident(_) | Token::QuotedIdent(_) | Token::String(_) | Token::Integer(_) => {
                    self.advance();
                }
                _ => {}
            }
        }
    }

    /// v7.9.18 — true when the next tokens are `PRIMARY KEY (…)`.
    /// PRIMARY and KEY are bare idents; we look-ahead 2 to be
    /// sure (otherwise a column literally named `primary` would
    /// be mistaken).
    fn peek_table_level_pk_start(&self) -> bool {
        let cur = self.peek();
        let nxt = self.tokens.get(self.pos + 1);
        let nxt2 = self.tokens.get(self.pos + 2);
        let is_primary = matches!(cur, Token::Ident(s) if s.eq_ignore_ascii_case("primary"));
        let is_key = matches!(nxt, Some(Token::Ident(s)) if s.eq_ignore_ascii_case("key"));
        let is_lparen = matches!(nxt2, Some(Token::LParen));
        is_primary && is_key && is_lparen
    }

    /// v7.9.18 — true when the next tokens are `UNIQUE (…)`.
    /// v7.13.0 — also matches `UNIQUE NULLS [NOT] DISTINCT (…)`
    /// (mailrs round-5 G10).
    fn peek_table_level_unique_start(&self) -> bool {
        let cur = self.peek();
        let is_unique = matches!(cur, Token::Ident(s) if s.eq_ignore_ascii_case("unique"));
        if !is_unique {
            return false;
        }
        let n1 = self.tokens.get(self.pos + 1);
        // Plain `UNIQUE (…)`.
        if matches!(n1, Some(Token::LParen)) {
            return true;
        }
        // `UNIQUE NULLS [NOT] DISTINCT (…)`.
        let is_nulls = matches!(n1, Some(Token::Ident(s)) if s.eq_ignore_ascii_case("nulls"));
        if !is_nulls {
            return false;
        }
        let n2 = self.tokens.get(self.pos + 2);
        let n3 = self.tokens.get(self.pos + 3);
        let n4 = self.tokens.get(self.pos + 4);
        // `UNIQUE NULLS DISTINCT (…)` — 4 tokens before `(`.
        if matches!(n2, Some(Token::Distinct)) && matches!(n3, Some(Token::LParen)) {
            return true;
        }
        // `UNIQUE NULLS NOT DISTINCT (…)` — 5 tokens before `(`.
        if matches!(n2, Some(Token::Not))
            && matches!(n3, Some(Token::Distinct))
            && matches!(n4, Some(Token::LParen))
        {
            return true;
        }
        false
    }

    fn parse_table_level_primary_key(&mut self) -> Result<crate::ast::TableConstraint, ParseError> {
        self.advance(); // PRIMARY
        self.advance(); // KEY
        let columns = self.parse_paren_ident_list("PRIMARY KEY")?;
        Ok(crate::ast::TableConstraint::PrimaryKey {
            name: None,
            columns,
        })
    }

    fn parse_table_level_unique(&mut self) -> Result<crate::ast::TableConstraint, ParseError> {
        self.advance(); // UNIQUE
        // v7.13.0 — optional `NULLS NOT DISTINCT` modifier
        // (mailrs round-5 G10, PG 15+ surface). Default behaviour
        // is `NULLS DISTINCT` per the SQL standard.
        let mut nulls_not_distinct = false;
        if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("nulls")) {
            let n1 = self.tokens.get(self.pos + 1);
            let n2 = self.tokens.get(self.pos + 2);
            let is_not = matches!(n1, Some(Token::Not));
            let is_distinct = matches!(n2, Some(Token::Distinct));
            if is_not && is_distinct {
                self.advance(); // NULLS
                self.advance(); // NOT
                self.advance(); // DISTINCT
                nulls_not_distinct = true;
            } else if matches!(n1, Some(Token::Distinct)) {
                self.advance(); // NULLS
                self.advance(); // DISTINCT
            }
        }
        let columns = self.parse_paren_ident_list("UNIQUE")?;
        Ok(crate::ast::TableConstraint::Unique {
            name: None,
            columns,
            nulls_not_distinct,
        })
    }

    /// v7.13.0 — table-level `CHECK (<expr>)` constraint
    /// (mailrs round-5 G3). Consumes `CHECK` then a parenthesised
    /// expression.
    fn parse_table_level_check(&mut self) -> Result<crate::ast::TableConstraint, ParseError> {
        self.advance(); // CHECK
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(alloc::format!(
                "expected '(' after CHECK, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let expr = self.parse_expr(0)?;
        if !matches!(self.peek(), Token::RParen) {
            return Err(self.err(alloc::format!(
                "expected ')' to close CHECK predicate, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        Ok(crate::ast::TableConstraint::Check { name: None, expr })
    }

    /// v7.13.0 — `true` when the next token is `CHECK` (a bare ident).
    fn peek_table_level_check_start(&self) -> bool {
        matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("check"))
    }

    /// v7.22 (round-13 gap 5) — `Some(kind)` when the next tokens are
    /// `CONSTRAINT <name> { CHECK | UNIQUE | PRIMARY }`. FOREIGN stays
    /// on the dedicated FK path (`parse_table_level_fk` consumes its
    /// own CONSTRAINT prefix).
    fn peek_named_table_constraint_kind(&self) -> Option<NamedTableConstraintKind> {
        if !matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("constraint")) {
            return None;
        }
        // tokens[pos+1] is the constraint name (any ident-like);
        // tokens[pos+2] is the kind keyword.
        match self.tokens.get(self.pos + 2) {
            Some(Token::Ident(s)) if s.eq_ignore_ascii_case("check") => {
                Some(NamedTableConstraintKind::Check)
            }
            Some(Token::Ident(s)) if s.eq_ignore_ascii_case("unique") => {
                Some(NamedTableConstraintKind::Unique)
            }
            Some(Token::Ident(s)) if s.eq_ignore_ascii_case("primary") => {
                Some(NamedTableConstraintKind::PrimaryKey)
            }
            _ => None,
        }
    }

    fn parse_paren_ident_list(&mut self, ctx: &str) -> Result<Vec<String>, ParseError> {
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(alloc::format!(
                "expected '(' after {ctx}, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let mut out = Vec::new();
        loop {
            out.push(self.expect_ident_like()?);
            match self.peek() {
                Token::Comma => {
                    self.advance();
                }
                Token::RParen => {
                    self.advance();
                    break;
                }
                other => {
                    return Err(self.err(alloc::format!(
                        "expected ',' or ')' in {ctx} list, got {other:?}"
                    )));
                }
            }
        }
        if out.is_empty() {
            return Err(self.err(alloc::format!("{ctx} requires at least one column")));
        }
        Ok(out)
    }

    /// v7.6.0 — true when the next tokens are `CONSTRAINT <name>
    /// FOREIGN KEY` or bare `FOREIGN KEY`. Both introduce a
    /// table-level FK; a column def never starts with either keyword
    /// (column names are not in this reserved set).
    fn peek_constraint_or_fk_start(&self) -> bool {
        let is_constraint_kw = matches!(
            self.peek(),
            Token::Ident(s) if s.eq_ignore_ascii_case("constraint")
        );
        let is_foreign_kw = matches!(
            self.peek(),
            Token::Ident(s) if s.eq_ignore_ascii_case("foreign")
        );
        is_constraint_kw || is_foreign_kw
    }

    /// v7.6.0 — parse a table-level FK clause:
    /// `[CONSTRAINT <name>] FOREIGN KEY (<col>[,<col>]*) REFERENCES
    /// <tbl> [(<pcol>[,<pcol>]*)] [ON DELETE <action>] [ON UPDATE <action>]`.
    fn parse_table_level_fk(&mut self) -> Result<ForeignKeyConstraint, ParseError> {
        let mut name: Option<String> = None;
        if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("constraint")) {
            self.advance();
            name = Some(self.expect_ident_like()?);
        }
        // `FOREIGN`
        match self.advance() {
            Token::Ident(s) if s.eq_ignore_ascii_case("foreign") => {}
            other => return Err(self.err(format!("expected FOREIGN, got {other:?}"))),
        }
        // `KEY`
        match self.advance() {
            Token::Ident(s) if s.eq_ignore_ascii_case("key") => {}
            other => return Err(self.err(format!("expected KEY after FOREIGN, got {other:?}"))),
        }
        // `(col, col, ...)`
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(format!(
                "expected '(' after FOREIGN KEY, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let mut columns = Vec::new();
        loop {
            columns.push(self.expect_ident_like()?);
            match self.peek() {
                Token::Comma => {
                    self.advance();
                }
                Token::RParen => {
                    self.advance();
                    break;
                }
                other => {
                    return Err(self.err(format!(
                        "expected ',' or ')' in FK column list, got {other:?}"
                    )));
                }
            }
        }
        if columns.is_empty() {
            return Err(self.err("FOREIGN KEY requires at least one column".into()));
        }
        let (parent_table, parent_columns, on_delete, on_update) =
            self.parse_references_tail(columns.len())?;
        Ok(ForeignKeyConstraint {
            name,
            columns,
            parent_table,
            parent_columns,
            on_delete,
            on_update,
        })
    }

    /// v7.6.0 — parse the tail `REFERENCES <tbl> [(<pcol>...)] [ON
    /// DELETE <action>] [ON UPDATE <action>]`. `expected_arity` is
    /// the local column count, used to default the parent column
    /// list when omitted (SQL spec: parent's PK is implied).
    fn parse_references_tail(
        &mut self,
        expected_arity: usize,
    ) -> Result<(String, Vec<String>, FkAction, FkAction), ParseError> {
        match self.advance() {
            Token::Ident(s) if s.eq_ignore_ascii_case("references") => {}
            other => return Err(self.err(format!("expected REFERENCES, got {other:?}"))),
        }
        let parent_table = self.expect_ident_like()?;
        let mut parent_columns: Vec<String> = Vec::new();
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            loop {
                parent_columns.push(self.expect_ident_like()?);
                match self.peek() {
                    Token::Comma => {
                        self.advance();
                    }
                    Token::RParen => {
                        self.advance();
                        break;
                    }
                    other => {
                        return Err(self.err(format!(
                            "expected ',' or ')' in REFERENCES column list, got {other:?}"
                        )));
                    }
                }
            }
        }
        if !parent_columns.is_empty() && parent_columns.len() != expected_arity {
            return Err(self.err(format!(
                "FK arity mismatch: {} local column(s) vs {} parent column(s)",
                expected_arity,
                parent_columns.len()
            )));
        }
        // v7.6.7 / v7.17.0 Phase 3.1 — interleave `[NOT] DEFERRABLE
        // [INITIALLY {DEFERRED | IMMEDIATE}]` and `ON DELETE
        // <action>` / `ON UPDATE <action>` in either order. PG /
        // pg_dump emits the timing clause AFTER the ON clauses
        // (`ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED`),
        // but the SQL spec allows either order. We loop over
        // every possible trailer and dispatch on the next token,
        // stopping when nothing matches. Phase 3.1 changes the
        // bare DEFERRABLE form from hard-error to accept-as-
        // immediate; SPG is single-writer with no deferred-
        // constraint window so the runtime semantics are always
        // immediate even when INITIALLY DEFERRED is requested.
        let mut on_delete = FkAction::Restrict;
        let mut on_update = FkAction::Restrict;
        let mut seen_on_delete = false;
        let mut seen_on_update = false;
        loop {
            // DEFERRABLE / NOT DEFERRABLE / INITIALLY shapes.
            let before = self.pos;
            self.consume_optional_deferrable_clauses()?;
            if self.pos != before {
                continue;
            }
            // ON DELETE / ON UPDATE.
            if !matches!(self.peek(), Token::On) {
                break;
            }
            self.advance();
            let which = self.advance();
            let action = self.parse_fk_action()?;
            match which {
                Token::Ident(ref s) if s.eq_ignore_ascii_case("delete") => {
                    if seen_on_delete {
                        return Err(self.err("ON DELETE specified twice".into()));
                    }
                    seen_on_delete = true;
                    on_delete = action;
                }
                Token::Ident(ref s) if s.eq_ignore_ascii_case("update") => {
                    if seen_on_update {
                        return Err(self.err("ON UPDATE specified twice".into()));
                    }
                    seen_on_update = true;
                    on_update = action;
                }
                other => {
                    return Err(
                        self.err(format!("expected DELETE or UPDATE after ON, got {other:?}"))
                    );
                }
            }
        }
        Ok((parent_table, parent_columns, on_delete, on_update))
    }

    /// v7.6.0 — parse `CASCADE | RESTRICT | SET NULL | SET DEFAULT |
    /// NO ACTION`.
    fn parse_fk_action(&mut self) -> Result<FkAction, ParseError> {
        match self.advance() {
            Token::Ident(s) if s.eq_ignore_ascii_case("cascade") => Ok(FkAction::Cascade),
            Token::Ident(s) if s.eq_ignore_ascii_case("restrict") => Ok(FkAction::Restrict),
            Token::Ident(s) if s.eq_ignore_ascii_case("set") => match self.advance() {
                Token::Null => Ok(FkAction::SetNull),
                Token::Default => Ok(FkAction::SetDefault),
                other => Err(self.err(format!(
                    "expected NULL or DEFAULT after SET in FK action, got {other:?}"
                ))),
            },
            Token::Ident(s) if s.eq_ignore_ascii_case("no") => match self.advance() {
                Token::Ident(s) if s.eq_ignore_ascii_case("action") => Ok(FkAction::NoAction),
                other => Err(self.err(format!(
                    "expected ACTION after NO in FK action, got {other:?}"
                ))),
            },
            other => Err(self.err(format!(
                "expected CASCADE | RESTRICT | SET NULL | SET DEFAULT | NO ACTION, got {other:?}"
            ))),
        }
    }

    /// Recognise the optional `IF NOT EXISTS` prefix shared by `CREATE
    /// TABLE` and `CREATE INDEX`. Returns `true` if consumed.
    fn consume_if_not_exists(&mut self) -> bool {
        // `IF` arrives as a bare Ident (we don't reserve it because it
        // also appears mid-expression in PG, though we don't support
        // those forms yet).
        let looks_like_if = matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("if"));
        if !looks_like_if {
            return false;
        }
        // Peek one ahead before committing: only consume IF when it's
        // actually `IF NOT EXISTS`.
        if !matches!(self.tokens.get(self.pos + 1), Some(Token::Not)) {
            return false;
        }
        if !matches!(
            self.tokens.get(self.pos + 2),
            Some(Token::Ident(s)) if s.eq_ignore_ascii_case("exists")
        ) {
            return false;
        }
        self.advance(); // IF
        self.advance(); // NOT
        self.advance(); // EXISTS
        true
    }

    /// v7.12.4 — `IF EXISTS` modifier for DROP statements.
    /// Consumes IF EXISTS as a pair; returns false otherwise
    /// without consuming any tokens.
    fn consume_if_exists(&mut self) -> bool {
        let looks_like_if = matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("if"));
        if !looks_like_if {
            return false;
        }
        if !matches!(
            self.tokens.get(self.pos + 1),
            Some(Token::Ident(s)) if s.eq_ignore_ascii_case("exists")
        ) {
            return false;
        }
        self.advance(); // IF
        self.advance(); // EXISTS
        true
    }

    /// v7.9.14 — consume `ASC | DESC | NULLS FIRST | NULLS LAST`
    /// qualifiers after an index column ref. ASC / DESC are
    /// reserved tokens; NULLS / FIRST / LAST are bare idents.
    /// We accept and discard them since single-column BTree
    /// stores rows in natural key order today.
    /// v7.24 (round-16 A) — `NULLS FIRST` / `NULLS LAST` after an
    /// ORDER BY key. Returns None when absent.
    fn parse_optional_nulls_placement(&mut self) -> Result<Option<bool>, ParseError> {
        if !matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("nulls")) {
            return Ok(None);
        }
        self.advance();
        match self.advance() {
            Token::Ident(s) if s.eq_ignore_ascii_case("first") => Ok(Some(true)),
            Token::Ident(s) if s.eq_ignore_ascii_case("last") => Ok(Some(false)),
            other => Err(self.err(alloc::format!(
                "expected FIRST or LAST after NULLS, got {other:?}"
            ))),
        }
    }

    fn consume_optional_index_column_qualifiers(&mut self) {
        loop {
            match self.peek() {
                Token::Asc | Token::Desc => {
                    self.advance();
                }
                Token::Ident(s) if s.eq_ignore_ascii_case("nulls") => {
                    let look = self.tokens.get(self.pos + 1);
                    if matches!(
                        look,
                        Some(Token::Ident(k)) if k.eq_ignore_ascii_case("first")
                            || k.eq_ignore_ascii_case("last")
                    ) {
                        self.advance();
                        self.advance();
                    } else {
                        break;
                    }
                }
                _ => break,
            }
        }
    }

    fn parse_create_index_stmt_after_create(
        &mut self,
        is_unique: bool,
    ) -> Result<Statement, ParseError> {
        // Caller consumed CREATE (and the optional UNIQUE); we're on INDEX.
        debug_assert!(matches!(self.peek(), Token::Index));
        self.advance();
        // v7.37.17 (17.6 partial) — CONCURRENTLY noise word (PG 8.2+).
        // SPG's CREATE INDEX is synchronous end-to-end today (real
        // CONCURRENTLY variant with restartable scans queues with
        // v7.39 indexes epic), so the modifier has no runtime effect
        // — same accept-and-no-op shape as v7.37.16.5 DETACH
        // PARTITION CONCURRENTLY and v7.37.19.8 REFRESH MATERIALIZED
        // VIEW CONCURRENTLY.
        if matches!(
            self.peek(),
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("concurrently")
        ) {
            self.advance();
        }
        let if_not_exists = self.consume_if_not_exists();
        let name = self.expect_ident_like()?;
        if !matches!(self.peek(), Token::On) {
            return Err(self.err(format!(
                "expected ON after CREATE INDEX <name>, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let table = self.expect_ident_like()?;
        // Optional `USING <method>` — only recognised method in v2.0 is
        // `hnsw` (a single-layer NSW graph for kNN). `USING` is the bare
        // ident `using` (we don't promote it to a reserved keyword
        // because it isn't reserved anywhere else in our SQL surface).
        let method = if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("using")) {
            self.advance();
            let m = self.expect_ident_like()?;
            match m.to_ascii_lowercase().as_str() {
                "hnsw" => IndexMethod::Hnsw,
                "btree" => IndexMethod::BTree,
                "brin" => IndexMethod::Brin,
                // v7.12.3 — real GIN inverted index over `tsvector`.
                // v7.9.26b's `USING gin` → BTree silent fallback is
                // gone; the engine validates that the indexed column
                // is `tsvector` at CREATE INDEX time.
                "gin" => IndexMethod::Gin,
                // v7.9.26b — PG `pg_dump` emits `USING gist` /
                // `USING spgist` / `USING hash` for their built-in
                // AMs that SPG doesn't have a matching
                // implementation for; degrade to BTree on the
                // leading column so the schema loads + the index
                // catalogue stays consistent. Operator pays the
                // planner cost only for the queries that would have
                // used the specialised AM.
                "gist" | "spgist" | "hash" => IndexMethod::BTree,
                // v7.11.3 — pgvector ships both `ivfflat` and
                // `hnsw`. Customers shouldn't have to choose
                // their on-disk index method based on what SPG
                // implements; accept `ivfflat` as a synonym for
                // `hnsw` so PG schemas using either method drop
                // in. The vector distance op (`<->` / `<#>` /
                // `<=>`) at query time still picks the metric.
                "ivfflat" => IndexMethod::Hnsw,
                other => {
                    return Err(self.err(alloc::format!(
                        "unknown index method {other:?}; supported: hnsw, btree, brin, gin (gist/spgist/hash accepted as BTree fallback)"
                    )));
                }
            }
        } else {
            IndexMethod::BTree
        };
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(format!(
                "expected '(' before indexed column, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        // v6.8.2 — accept either a bare column ident (legacy) or
        // an expression `fn(col, …)` for expression indexes.
        // Distinguish by peeking the token *after* the current
        // ident: `ident )` is the legacy column-only path;
        // anything else triggers the Pratt expression parser.
        // (`advance()` uses `mem::replace` to nil out the current
        // slot, so we can't save+rewind cleanly — peek-ahead via
        // direct index avoids the mutation.)
        let mut opclass: Option<String> = None;
        let (column, expression): (String, Option<Expr>) = match self.peek().clone() {
            // Single column with `)` immediately after — fast path.
            // v7.9.29 — also: bare column followed by `,` (the
            // multi-column form `(a, b, c)`). Without this branch
            // the leading ident gets pulled into `parse_expr`
            // which then sets `expression = Some(Column(a))` and
            // breaks Display round-trip on the multi-column shape.
            Token::Ident(s) | Token::QuotedIdent(s)
                if matches!(
                    self.tokens.get(self.pos + 1),
                    Some(Token::RParen | Token::Comma)
                ) =>
            {
                self.advance();
                (s, None)
            }
            // v7.9.22 — single column followed by a pgvector
            // opclass ident: `(col vector_cosine_ops)`. mailrs G5.
            // v7.15.0 — capture the opclass instead of discarding
            // it so the engine can dispatch (e.g. `gin_trgm_ops`
            // → real trigram-shingle GIN over a TEXT column).
            // Vector/HNSW opclasses still take their distance
            // metric from the query operator (`<->` / `<#>` /
            // `<=>`), so for those callers the opclass stays
            // informational.
            // v7.22 (mailrs round-13 gap 7) — pg_dump qualifies the
            // opclass: `(embedding public.vector_cosine_ops)`. Strip
            // the schema and dispatch on the bare opclass, the same
            // treatment table/type names get.
            Token::Ident(s) | Token::QuotedIdent(s)
                if matches!(
                    self.tokens.get(self.pos + 1),
                    Some(Token::Ident(_) | Token::QuotedIdent(_))
                ) && matches!(self.tokens.get(self.pos + 2), Some(Token::Dot))
                    && matches!(
                        self.tokens.get(self.pos + 3),
                        Some(Token::Ident(op) | Token::QuotedIdent(op))
                            if is_vector_opclass_name(op)
                    ) =>
            {
                self.advance(); // column name
                self.advance(); // schema qualifier
                self.advance(); // dot
                let op_tok = self.advance();
                if let Token::Ident(op) | Token::QuotedIdent(op) = op_tok {
                    opclass = Some(op.to_ascii_lowercase());
                }
                (s, None)
            }
            Token::Ident(s) | Token::QuotedIdent(s)
                if matches!(
                    self.tokens.get(self.pos + 1),
                    Some(Token::Ident(op) | Token::QuotedIdent(op))
                        if is_vector_opclass_name(op)
                ) =>
            {
                self.advance(); // column name
                // Capture the opclass token, lower-cased for
                // case-insensitive engine dispatch.
                let op_tok = self.advance();
                if let Token::Ident(op) | Token::QuotedIdent(op) = op_tok {
                    opclass = Some(op.to_ascii_lowercase());
                }
                (s, None)
            }
            Token::Ident(_) | Token::QuotedIdent(_) => {
                let key_expr = self.parse_expr(0)?;
                let primary = extract_first_column(&key_expr).ok_or_else(|| {
                    self.err("expression index key must reference at least one column".into())
                })?;
                (primary, Some(key_expr))
            }
            // v7.37.43-T4 — parenthesised expression index key
            // `CREATE INDEX … ON t ((payload->'bundle'->>'id'))`.
            // PG's CREATE INDEX requires the expression to be in
            // its own parens to disambiguate function calls from
            // column lists, so this `LParen` is the inner open-paren
            // of an expression key. parse_expr handles the recursive
            // descent and consumes the matching `RParen`.
            Token::LParen => {
                let key_expr = self.parse_expr(0)?;
                let primary = extract_first_column(&key_expr).ok_or_else(|| {
                    self.err("expression index key must reference at least one column".into())
                })?;
                (primary, Some(key_expr))
            }
            other => {
                return Err(self.err(format!(
                    "expected column ident or expression, got {other:?}"
                )));
            }
        };
        // v7.9.14 — accept extra comma-separated columns inside
        // the index key parens (`CREATE INDEX … (a, b, c)`).
        // mailrs F2. Each extra column may carry an optional
        // `ASC` / `DESC` / `NULLS FIRST` / `NULLS LAST` clause
        // — parsed and discarded; SPG doesn't honour direction
        // on a BTree index today (column ordering is intrinsic
        // to the storage). v7.10 will widen to genuine composite
        // index keys.
        let mut extra_columns: Vec<String> = Vec::new();
        // The leading column may also have ASC/DESC after it.
        self.consume_optional_index_column_qualifiers();
        while matches!(self.peek(), Token::Comma) {
            self.advance();
            let extra = self.expect_ident_like()?;
            self.consume_optional_index_column_qualifiers();
            extra_columns.push(extra);
        }
        if !matches!(self.peek(), Token::RParen) {
            return Err(self.err(format!(
                "expected ')' after indexed column / expression, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        // v6.8.0 — optional `INCLUDE (col1, col2, …)` clause for
        // index-only-scan annotation. Bare ident (not a reserved
        // keyword) so we test by case-insensitive string match.
        let included_columns = if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("include"))
        {
            self.advance();
            if !matches!(self.peek(), Token::LParen) {
                return Err(self.err(format!("expected '(' after INCLUDE, got {:?}", self.peek())));
            }
            self.advance();
            let mut cols = Vec::new();
            loop {
                cols.push(self.expect_ident_like()?);
                match self.peek() {
                    Token::Comma => {
                        self.advance();
                    }
                    Token::RParen => {
                        self.advance();
                        break;
                    }
                    other => {
                        return Err(self.err(format!(
                            "expected ',' or ')' in INCLUDE list, got {other:?}"
                        )));
                    }
                }
            }
            cols
        } else {
            Vec::new()
        };
        // v7.11.3 — accept and discard PG `WITH (k = v, ...)` index
        // storage parameters. pgvector emits `WITH (lists = N)` for
        // ivfflat and `WITH (m = N, ef_construction = M)` for hnsw;
        // SPG's HNSW picks its own parameters today (tunable via
        // env vars), so the WITH clause is informational and dropped.
        if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("with")) {
            self.advance();
            if !matches!(self.peek(), Token::LParen) {
                return Err(self.err(format!(
                    "expected '(' after WITH in CREATE INDEX, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            loop {
                if matches!(self.peek(), Token::RParen) {
                    self.advance();
                    break;
                }
                // Drain `key = value` or bare `key` tokens.
                let _ = self.advance(); // key
                if matches!(self.peek(), Token::Eq) {
                    self.advance();
                    let _ = self.advance(); // value (int / string / ident)
                }
                match self.peek() {
                    Token::Comma => {
                        self.advance();
                    }
                    Token::RParen => {
                        self.advance();
                        break;
                    }
                    other => {
                        return Err(self.err(format!(
                            "expected ',' or ')' in WITH (…) clause, got {other:?}"
                        )));
                    }
                }
            }
        }
        // v6.8.1 — optional `WHERE <expr>` partial-index predicate.
        let partial_predicate = if matches!(self.peek(), Token::Where) {
            self.advance();
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        // v7.9.29 — UNIQUE on a vector index (HNSW) makes no
        // sense: uniqueness over an ANN structure has no clean
        // semantics. Reject early. (BRIN UNIQUE is similarly
        // meaningless — block both.)
        if is_unique && !matches!(method, IndexMethod::BTree) {
            return Err(self.err(alloc::format!(
                "UNIQUE is only supported on BTree indexes, got USING {:?}",
                method
            )));
        }
        Ok(Statement::CreateIndex(CreateIndexStatement {
            name,
            table,
            column,
            method,
            if_not_exists,
            included_columns,
            partial_predicate,
            extra_columns: extra_columns.clone(),
            expression,
            is_unique,
            opclass,
        }))
    }

    /// v7.6.0 — wraps `parse_column_def` and consumes an optional
    /// column-level `REFERENCES ...` clause. The trailing FK is
    /// normalised into table-level shape (single-element columns +
    /// parent_columns) so the engine sees one uniform constraint list.
    fn parse_column_def_with_fk(
        &mut self,
    ) -> Result<(ColumnDef, Option<ForeignKeyConstraint>), ParseError> {
        let col = self.parse_column_def()?;
        // Inline form: `col INT REFERENCES tbl(pcol) [ON DELETE ...] [ON UPDATE ...]`.
        let inline_references = matches!(
            self.peek(),
            Token::Ident(s) if s.eq_ignore_ascii_case("references")
        );
        if !inline_references {
            return Ok((col, None));
        }
        let (parent_table, parent_columns, on_delete, on_update) = self.parse_references_tail(1)?;
        let fk = ForeignKeyConstraint {
            name: None,
            columns: vec![col.name.clone()],
            parent_table,
            parent_columns,
            on_delete,
            on_update,
        };
        Ok((col, Some(fk)))
    }

    /// v7.13.0 — parse a column type (consuming the type ident and
    /// any trailing parameters / `[]`), without surrounding column
    /// constraints. Used by ALTER COLUMN TYPE (mailrs round-5 G8).
    /// Returns the resolved `ColumnTypeName` plus implied
    /// `(auto_increment, not_null)` flags from PG SERIAL family
    /// shorthands — callers that don't expect those (ALTER COLUMN
    /// TYPE) can discard them.
    fn parse_column_type_name(&mut self) -> Result<ColumnTypeName, ParseError> {
        let (ty, _, _, _, _, _, _, _) = self.parse_type_with_implied_flags()?;
        Ok(ty)
    }

    #[allow(clippy::type_complexity)]
    fn parse_type_with_implied_flags(
        &mut self,
    ) -> Result<
        (
            ColumnTypeName,
            bool,
            bool,
            Option<String>,
            Collation,
            bool,
            // v7.17.0 Phase 3.P0-36 — MySQL inline ENUM variant
            // list captured at type-parse time. None for all
            // non-ENUM types.
            Option<Vec<String>>,
            // v7.17.0 Phase 3.P0-37 — MySQL inline SET variant
            // list. Distinct from ENUM (subset semantics).
            Option<Vec<String>>,
        ),
        ParseError,
    > {
        let mut ty_ident = match self.advance() {
            Token::Ident(s) => s,
            // v7.37.5 β-P2 — `INTERVAL` lexes as a reserved keyword
            // (Token::Interval) since v7.9.25 to drive the `INTERVAL
            // '<span>'` literal grammar. As a column type it lands
            // here directly; downstream resolution still uses the
            // canonical lowercase string.
            Token::Interval => "interval".to_string(),
            other => {
                return Err(ParseError {
                    message: format!("expected column type, got {other:?}"),
                    token_pos: self.pos.saturating_sub(1),
                });
            }
        };
        // v7.22 (mailrs round-13 gap 4) — schema-qualified type names:
        // pg_dump qualifies extension types (`public.vector(1024)`).
        // SPG is single-namespace; drop the schema and resolve the
        // bare type — same treatment table names already get.
        while matches!(self.peek(), Token::Dot) {
            self.advance();
            ty_ident = self.expect_ident_like()?;
        }
        let mut implied_auto_increment = false;
        let mut implied_not_null = false;
        let mut user_type_ref: Option<String> = None;
        // v7.17.0 Phase 3.P0-36 — MySQL inline ENUM('a','b','c')
        // value list, captured here and bubbled up through the
        // ColumnDef so the engine can attach it to the column
        // schema (and validate INSERT cells against it).
        let mut inline_enum_variants: Option<Vec<String>> = None;
        // v7.17.0 Phase 3.P0-37 — MySQL inline SET variant list.
        let mut inline_set_variants: Option<Vec<String>> = None;
        let mut ty = match ty_ident.as_str() {
            // PG SERIAL family. Implies NOT NULL + AUTO_INCREMENT.
            "smallserial" | "serial2" => {
                implied_auto_increment = true;
                implied_not_null = true;
                ColumnTypeName::SmallInt
            }
            "serial" | "serial4" => {
                implied_auto_increment = true;
                implied_not_null = true;
                ColumnTypeName::Int
            }
            "bigserial" | "serial8" => {
                implied_auto_increment = true;
                implied_not_null = true;
                ColumnTypeName::BigInt
            }
            // MySQL flavours we accept by aliasing to the closest SPG
            // type. TINYINT covers MySQL's i8 — held inside SMALLINT
            // since SPG doesn't have a dedicated i8. MEDIUMINT (MySQL
            // 24-bit) → INT. UNSIGNED modifiers are consumed below
            // without semantic effect.
            "smallint" => {
                // v7.14.0 — MySQL display-width on integers
                // (`SMALLINT(5)`, `INT(11)`, `BIGINT(20)`). The
                // parenthesised number is purely cosmetic — it
                // doesn't change storage. Accept + discard.
                self.consume_optional_paren_size();
                ColumnTypeName::SmallInt
            }
            // v7.17.0 Phase 4.3 — MySQL `TINYINT(1)` is the
            // canonical encoding for BOOLEAN. Every MySQL driver
            // (JDBC `tinyInt1isBit=true`, PHP `mysql_field_type`,
            // .NET `MySqlConnection`, sqlx) maps it to bit. Pre-
            // 4.3 SPG classified TINYINT(1) as SmallInt, which
            // gave the customer i16-shaped values where the app
            // expected bool — a Tier-A silent type drift on
            // mysqldump restores. Now: `TINYINT(1)` → Bool;
            // `TINYINT` (no width) and `TINYINT(N)` for N ≠ 1
            // stay SmallInt (the legacy width-agnostic path).
            "tinyint" => {
                let width = self.peek_optional_paren_size_value();
                self.consume_optional_paren_size();
                if width == Some(1) {
                    ColumnTypeName::Bool
                } else {
                    ColumnTypeName::SmallInt
                }
            }
            "int" | "integer" | "mediumint" => {
                self.consume_optional_paren_size();
                ColumnTypeName::Int
            }
            "bigint" => {
                self.consume_optional_paren_size();
                ColumnTypeName::BigInt
            }
            // DOUBLE / REAL are 64-bit IEEE — same as our FLOAT.
            // v7.13.0 — `DOUBLE PRECISION` (PG canonical spelling)
            // (mailrs round-5 G6). Consume the optional `PRECISION`
            // tail when the type keyword was `double` / `DOUBLE`.
            "float" | "double" | "real" => {
                if ty_ident.eq_ignore_ascii_case("double")
                    && matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("precision"))
                {
                    self.advance();
                }
                ColumnTypeName::Float
            }
            // v7.13.0 — `FLOAT8` (PG short form) maps the same as FLOAT.
            "float4" | "float8" => ColumnTypeName::Float,
            "text" => ColumnTypeName::Text,
            "bool" | "boolean" => ColumnTypeName::Bool,
            "varchar" => ColumnTypeName::Varchar(self.parse_paren_size("VARCHAR")?),
            "char" => ColumnTypeName::Char(self.parse_paren_size("CHAR")?),
            "vector" => {
                let dim = self.parse_paren_size("VECTOR")?;
                let encoding = self.parse_optional_vector_encoding()?;
                ColumnTypeName::Vector { dim, encoding }
            }
            "numeric" => {
                let (precision, scale) = self.parse_optional_numeric_params()?;
                ColumnTypeName::Numeric(precision, scale)
            }
            "date" => ColumnTypeName::Date,
            // MySQL's `DATETIME` is the same domain as standard
            // `TIMESTAMP` — accept both spellings.
            "timestamp" | "datetime" => {
                // v7.14.0 — PG canonical `TIMESTAMP WITH TIME ZONE`
                // / `TIMESTAMP WITHOUT TIME ZONE`. pg_dump emits
                // the full form. SPG canonicalises:
                //   - WITH TIME ZONE    → Timestamptz
                //   - WITHOUT TIME ZONE → Timestamp
                if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("with"))
                    && matches!(self.tokens.get(self.pos + 1), Some(Token::Ident(s)) if s.eq_ignore_ascii_case("time"))
                    && matches!(self.tokens.get(self.pos + 2), Some(Token::Ident(s)) if s.eq_ignore_ascii_case("zone"))
                {
                    self.advance(); // WITH
                    self.advance(); // TIME
                    self.advance(); // ZONE
                    ColumnTypeName::Timestamptz
                } else if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("without"))
                    && matches!(self.tokens.get(self.pos + 1), Some(Token::Ident(s)) if s.eq_ignore_ascii_case("time"))
                    && matches!(self.tokens.get(self.pos + 2), Some(Token::Ident(s)) if s.eq_ignore_ascii_case("zone"))
                {
                    self.advance(); // WITHOUT
                    self.advance(); // TIME
                    self.advance(); // ZONE
                    ColumnTypeName::Timestamp
                } else {
                    // Optional `(precision)` parenthesised modifier
                    // (PG fractional seconds precision). SPG stores
                    // µs always; accept + discard.
                    self.consume_optional_paren_size();
                    ColumnTypeName::Timestamp
                }
            }
            // v7.9.2 — `TIMESTAMPTZ` and full PG spelling
            // `TIMESTAMP WITH TIME ZONE`. Same storage as TIMESTAMP;
            // only PG-wire OID differs.
            "timestamptz" => ColumnTypeName::Timestamptz,
            // v4.9: JSON / JSONB. Stored as raw text — no parse-time
            // validation. We accept the JSONB spelling too because
            // most PG clients default to it; SPG doesn't distinguish
            // the two (no path-operator perf advantage to model).
            "json" => ColumnTypeName::Json,
            "jsonb" => ColumnTypeName::Jsonb,
            // v7.10.4 — PG `BYTEA` and the SPG `BYTES` alias both
            // surface here. Same storage shape; mapping happens at
            // the engine side via the ColumnTypeName → DataType
            // resolver. Literal forms are handled at coerce_value
            // time so the lexer stays untouched.
            "bytea" | "bytes" => ColumnTypeName::Bytes,
            // v7.17.0 Phase 7 — PG network address types
            // v7.17.0 had a Text-backed fallback here for
            // `inet` / `cidr` / `macaddr`. v7.37.5 ζ-A promoted
            // each to a first-class type; the keywords are
            // bound below in the ζ-A block.
            // v7.12.0 — PG full-text search types. mailrs G-CRIT-3.
            // The actual `to_tsvector` / `@@` / `ts_rank` surface
            // arrives in v7.12.1+; the type itself loads here so
            // mailrs's `scripts/init-schema.sql` runs unmodified.
            "tsvector" => ColumnTypeName::TsVector,
            "tsquery" => ColumnTypeName::TsQuery,
            // v7.17.0 — PG `UUID`. Wire OID 2950. The drop-in PG
            // surface for Django / Rails / Hibernate's default
            // PK pattern.
            "uuid" => ColumnTypeName::Uuid,
            // v7.37.5 β-P2 — PG `INTERVAL` as a column type.
            // Storage = three-field {months, days, micros}, catalog
            // tag 34, FILE_VERSION 48+, wire OID 1186. Prior to this
            // line `INTERVAL` was parser-rejected at CREATE TABLE.
            "interval" => ColumnTypeName::Interval,
            // v7.17.0 Phase 3.P0-32 — PG `TIME` (without time zone).
            // i64 microseconds since 00:00:00. Wire OID 1083.
            "time" => ColumnTypeName::Time,
            // v7.17.0 Phase 3.P0-33 — MySQL `YEAR`. u16 in
            // 1901..=2155 + zero-year sentinel 0. Wire = INT4.
            "year" => ColumnTypeName::Year,
            // v7.17.0 Phase 3.P0-34 — PG `TIMETZ` / `TIME WITH
            // TIME ZONE`. i64 us + i32 offset_secs. Wire OID 1266.
            "timetz" => ColumnTypeName::TimeTz,
            // v7.17.0 Phase 3.P0-35 — PG `MONEY` — i64 cents.
            // Wire OID 790.
            "money" => ColumnTypeName::Money,
            // v7.17.0 Phase 3.P0-38 — PG range types.
            "int4range" => ColumnTypeName::Range(RangeKindAst::Int4),
            "int8range" => ColumnTypeName::Range(RangeKindAst::Int8),
            "numrange" => ColumnTypeName::Range(RangeKindAst::Num),
            "tsrange" => ColumnTypeName::Range(RangeKindAst::Ts),
            "tstzrange" => ColumnTypeName::Range(RangeKindAst::TsTz),
            "daterange" => ColumnTypeName::Range(RangeKindAst::Date),
            // v7.37.5 δ — PG 14+ multirange keywords.
            "int4multirange" => ColumnTypeName::Multirange(RangeKindAst::Int4),
            "int8multirange" => ColumnTypeName::Multirange(RangeKindAst::Int8),
            "nummultirange" => ColumnTypeName::Multirange(RangeKindAst::Num),
            "tsmultirange" => ColumnTypeName::Multirange(RangeKindAst::Ts),
            "tstzmultirange" => ColumnTypeName::Multirange(RangeKindAst::TsTz),
            "datemultirange" => ColumnTypeName::Multirange(RangeKindAst::Date),
            // v7.37.5 ε — PG geometry scalar keywords.
            "point" => ColumnTypeName::Point,
            "lseg" => ColumnTypeName::Lseg,
            "path" => ColumnTypeName::Path,
            "box" => ColumnTypeName::PgBox,
            "polygon" => ColumnTypeName::Polygon,
            "line" => ColumnTypeName::Line,
            "circle" => ColumnTypeName::Circle,
            // v7.37.5 ζ-A — network / bit / xml / "char" keywords.
            "inet" => ColumnTypeName::Inet,
            "cidr" => ColumnTypeName::Cidr,
            "macaddr" => ColumnTypeName::Macaddr,
            "macaddr8" => ColumnTypeName::Macaddr8,
            "bit" => ColumnTypeName::Bit,
            "varbit" => ColumnTypeName::BitVarying,
            "xml" => ColumnTypeName::Xml,
            // v7.17.0 Phase 3.P0-39 — PG hstore extension type.
            "hstore" => ColumnTypeName::Hstore,
            // v7.17.0 Phase 3.P0-36 — MySQL inline ENUM
            // `ENUM('a','b','c')`. Storage is TEXT; the value
            // list lands on `inline_enum_variants` for the
            // engine to validate INSERT cells against. Empty
            // value list is a parse error (matches MySQL).
            "enum" => {
                // Expect the opening `(`.
                if !matches!(self.peek(), Token::LParen) {
                    return Err(self.err(alloc::format!(
                        "expected '(' after ENUM, got {:?}",
                        self.peek()
                    )));
                }
                self.advance();
                let mut variants: Vec<String> = Vec::new();
                loop {
                    match self.advance() {
                        Token::String(s) => variants.push(s),
                        other => {
                            return Err(self.err(alloc::format!(
                                "ENUM(...) expects string literal variants, got {other:?}"
                            )));
                        }
                    }
                    match self.peek() {
                        Token::Comma => {
                            self.advance();
                            continue;
                        }
                        Token::RParen => {
                            self.advance();
                            break;
                        }
                        other => {
                            return Err(self.err(alloc::format!(
                                "expected ',' or ')' in ENUM(...), got {other:?}"
                            )));
                        }
                    }
                }
                if variants.is_empty() {
                    return Err(self.err("ENUM(...) must declare at least one variant".into()));
                }
                inline_enum_variants = Some(variants);
                // Storage is plain TEXT; the variant list lives on
                // the ColumnSchema side.
                ColumnTypeName::Text
            }
            // v7.17.0 Phase 3.P0-37 — MySQL inline SET
            // `SET('a','b','c')`. Same parse shape as ENUM;
            // semantics differ (subset rather than pick-one).
            "set" => {
                if !matches!(self.peek(), Token::LParen) {
                    return Err(self.err(alloc::format!(
                        "expected '(' after SET, got {:?}",
                        self.peek()
                    )));
                }
                self.advance();
                let mut variants: Vec<String> = Vec::new();
                loop {
                    match self.advance() {
                        Token::String(s) => variants.push(s),
                        other => {
                            return Err(self.err(alloc::format!(
                                "SET(...) expects string literal variants, got {other:?}"
                            )));
                        }
                    }
                    match self.peek() {
                        Token::Comma => {
                            self.advance();
                            continue;
                        }
                        Token::RParen => {
                            self.advance();
                            break;
                        }
                        other => {
                            return Err(self.err(alloc::format!(
                                "expected ',' or ')' in SET(...), got {other:?}"
                            )));
                        }
                    }
                }
                if variants.is_empty() {
                    return Err(self.err("SET(...) must declare at least one variant".into()));
                }
                inline_set_variants = Some(variants);
                ColumnTypeName::Text
            }
            _other => {
                // v7.17.0 Phase 1.4 — unknown ident → defer
                // resolution to the engine. Stored as Text in
                // ColumnTypeName + the original name carried as
                // `user_type_ref` so CREATE TABLE can look up
                // user-defined enum / domain types.
                user_type_ref = Some(ty_ident.clone());
                ColumnTypeName::Text
            }
        };
        // v7.17.0 Phase 4.4 — MySQL's `UNSIGNED` modifier sits
        // right after the type keyword. Pre-4.4 SPG consumed +
        // discarded the keyword, leaving a customer column
        // declared `id INT UNSIGNED NOT NULL` silently accepting
        // negative values — a Tier-A correctness drift where
        // application invariants (auto-increment-IDs never
        // negative) silently broke on cutover. Now: capture as
        // a column flag, persist on the schema, enforce at
        // INSERT / UPDATE time.
        let is_unsigned = if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("unsigned"))
        {
            self.advance();
            true
        } else {
            false
        };
        // v7.14.0 — mysqldump emits `<type> CHARACTER SET <name>` and
        // `<type> COLLATE <name>` post-fixes on text columns. SPG
        // stores text as UTF-8 always so CHARACTER SET is still a
        // no-op. v7.17.0 Phase 2.5 — COLLATE no longer drops the
        // name: it gets classified into a `Collation` variant the
        // engine consults at WHERE-eval time. PG `default` /
        // `pg_catalog.default` / `C` / `POSIX` collations all
        // resolve to `Binary` (the prior behaviour); `_ci` /
        // `case_insensitive` / `nocase` shift to CaseInsensitive.
        // The schema-qualifier form (`pg_catalog.default`) lexes
        // as `Ident '.' Ident` — peek for the `.` and consume both
        // halves so it's treated as one collation name. PG's
        // `IDENT.IDENT` collation form (which can appear here) is
        // resolved by Collation::from_collation_name on the bare
        // identifier after the dot.
        let mut collation = Collation::Binary;
        loop {
            if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("character"))
                && matches!(self.tokens.get(self.pos + 1), Some(Token::Ident(s)) if s.eq_ignore_ascii_case("set"))
            {
                self.advance(); // CHARACTER
                self.advance(); // SET
                if matches!(
                    self.peek(),
                    Token::Ident(_) | Token::QuotedIdent(_) | Token::String(_)
                ) {
                    self.advance();
                }
                continue;
            }
            if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("collate")) {
                self.advance(); // COLLATE
                // Accept Ident / QuotedIdent / String AND the
                // keyword-tokenised `Default` (PG `pg_catalog.default`
                // and bare `DEFAULT` collation names — `default` is a
                // reserved word so the lexer hands back Token::Default
                // not Token::Ident).
                let read_collation_atom = |this: &mut Self| -> Option<alloc::string::String> {
                    match this.peek().clone() {
                        Token::Ident(s) | Token::QuotedIdent(s) | Token::String(s) => {
                            this.advance();
                            Some(s)
                        }
                        Token::Default => {
                            this.advance();
                            Some(alloc::string::String::from("default"))
                        }
                        _ => None,
                    }
                };
                let raw = if let Some(head) = read_collation_atom(self) {
                    // Schema-qualified PG form: `pg_catalog.default`.
                    if matches!(self.peek(), Token::Dot) {
                        self.advance();
                        let tail = read_collation_atom(self).unwrap_or_default();
                        alloc::format!("{head}.{tail}")
                    } else {
                        head
                    }
                } else {
                    alloc::string::String::new()
                };
                if !raw.is_empty() {
                    let parsed = Collation::from_collation_name(&raw);
                    // Last COLLATE clause wins, but `Binary` from a
                    // bare keyword like `default` should not
                    // silently downgrade a stronger one set earlier
                    // on the same column. v7.17 only ships one
                    // non-Binary variant so a simple OR is enough.
                    if parsed != Collation::Binary {
                        collation = parsed;
                    }
                }
                continue;
            }
            break;
        }
        // v7.10.10 — postfix `[]` widens TEXT → TEXT[]. PG accepts
        // `TYPE[]` after any base type; v7.10 only models TEXT[]
        // so we reject other base types here. mailrs uses TEXT[]
        // for labels / addresses / message-on-thread.
        if matches!(self.peek(), Token::LBracket) {
            self.advance();
            if !matches!(self.peek(), Token::RBracket) {
                return Err(self.err(alloc::format!(
                    "TEXT[] takes no dimension; got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            // v7.11.13 — widened to INT[] and BIGINT[] in addition
            // to TEXT[]. Other base types (BOOL[], NUMERIC[], etc.)
            // still error here.
            ty = match ty {
                ColumnTypeName::Text => ColumnTypeName::TextArray,
                ColumnTypeName::Int => ColumnTypeName::IntArray,
                ColumnTypeName::BigInt => ColumnTypeName::BigIntArray,
                // v7.37.5 β-P4 — INTERVAL[] via the same postfix
                // `[]` grammar. Wire OID 1187.
                ColumnTypeName::Interval => ColumnTypeName::IntervalArray,
                // v7.37.5 γ — full PG array-of-scalar family.
                ColumnTypeName::Bool => ColumnTypeName::BoolArray,
                ColumnTypeName::SmallInt => ColumnTypeName::SmallIntArray,
                ColumnTypeName::Float => ColumnTypeName::FloatArray,
                // NUMERIC(p, s) loses its precision params at the
                // array level (matches PG: `NUMERIC[]` is untyped,
                // per-element precision flows through values).
                ColumnTypeName::Numeric(_, _) => ColumnTypeName::NumericArray,
                ColumnTypeName::Date => ColumnTypeName::DateArray,
                ColumnTypeName::Timestamp => ColumnTypeName::TimestampArray,
                ColumnTypeName::Timestamptz => ColumnTypeName::TimestamptzArray,
                ColumnTypeName::Uuid => ColumnTypeName::UuidArray,
                ColumnTypeName::Json => ColumnTypeName::JsonArray,
                ColumnTypeName::Jsonb => ColumnTypeName::JsonbArray,
                ColumnTypeName::Bytes => ColumnTypeName::BytesArray,
                // VARCHAR(n)[] / CHAR(n)[] drop the length cap at
                // the array level (matches PG semantics where the
                // element precision is per-row, not column-wide).
                ColumnTypeName::Varchar(_) => ColumnTypeName::VarcharArray,
                ColumnTypeName::Char(_) => ColumnTypeName::CharArray,
                // v7.37.5 ζ-A — MONEY[] (OID 791) ship-triage
                // follow-up.
                ColumnTypeName::Money => ColumnTypeName::MoneyArray,
                other => {
                    return Err(self.err(alloc::format!("{other:?}[] not yet supported")));
                }
            };
            // v7.17.0 Phase 3.P0-40 — second `[]` widens 1D → 2D
            // for INT/TEXT/BIGINT. Anything else is an error.
            if matches!(self.peek(), Token::LBracket) {
                self.advance();
                if !matches!(self.peek(), Token::RBracket) {
                    return Err(self.err(alloc::format!(
                        "TYPE[][] second dimension takes no size; got {:?}",
                        self.peek()
                    )));
                }
                self.advance();
                ty = match ty {
                    ColumnTypeName::IntArray => ColumnTypeName::IntArray2D,
                    ColumnTypeName::BigIntArray => ColumnTypeName::BigIntArray2D,
                    ColumnTypeName::TextArray => ColumnTypeName::TextArray2D,
                    other => {
                        return Err(self.err(alloc::format!(
                            "v7.17 2D arrays support INT[][] / BIGINT[][] / \
                             TEXT[][] only; got {other:?}"
                        )));
                    }
                };
            }
        }
        Ok((
            ty,
            implied_auto_increment,
            implied_not_null,
            user_type_ref,
            collation,
            is_unsigned,
            inline_enum_variants,
            inline_set_variants,
        ))
    }

    fn parse_column_def(&mut self) -> Result<ColumnDef, ParseError> {
        // v7.20 — PG reserves the table-constraint keywords, so a
        // BARE `UNIQUE` / `PRIMARY` / … in column position is a
        // malformed constraint clause (e.g. `UNIQUE a` missing its
        // parens), not a column named "unique". Since v7.17's
        // unknown-type leniency (`user_type_ref`) such a clause
        // would otherwise parse as a column with a user-defined
        // type — silently accepting invalid DDL. Quoted
        // identifiers ("unique" / `unique`) remain valid names.
        if let Token::Ident(s) = self.peek()
            && [
                "unique",
                "primary",
                "foreign",
                "constraint",
                "check",
                "references",
                "exclude",
            ]
            .iter()
            .any(|kw| s.eq_ignore_ascii_case(kw))
        {
            return Err(self.err(alloc::format!(
                "unexpected reserved keyword '{s}' at start of column definition \
                 (malformed table constraint?)"
            )));
        }
        let name = self.expect_ident_like()?;
        let (
            ty,
            implied_auto_increment,
            implied_not_null,
            user_type_ref,
            collation,
            is_unsigned,
            inline_enum_variants,
            inline_set_variants,
        ) = self.parse_type_with_implied_flags()?;
        // Column constraints: `DEFAULT <expr>`, `NOT NULL`, and the
        // MySQL-flavoured `AUTO_INCREMENT` may appear in any order;
        // each at most once.
        let mut default: Option<Expr> = None;
        let mut nullable = !implied_not_null;
        let mut nullability_seen = implied_not_null;
        let mut auto_increment = implied_auto_increment;
        let mut is_primary_key = false;
        let mut is_unique = false;
        let mut check: Option<Expr> = None;
        let mut on_update_runtime: Option<Expr> = None;
        let mut generated_stored_expr: Option<Box<Expr>> = None;
        loop {
            // v7.22 (mailrs round-13 gap 3) — PG 18 catalogs
            // not-null constraints by name and pg_dump emits them
            // inline: `id bigint CONSTRAINT contacts_id_not_null1
            // NOT NULL`. Accept and discard the name; whatever
            // constraint follows is parsed by the arms below.
            if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("constraint")) {
                self.advance();
                let _name = self.expect_ident_like()?;
                continue;
            }
            // v7.22 (round-13 T2) — inline `GENERATED { ALWAYS |
            // BY DEFAULT } AS IDENTITY [(seq options)]` (PG 10+;
            // the modern replacement for SERIAL in hand-written
            // schemas). Both flavours map onto the auto-increment
            // machinery — SPG's serial semantics ≈ BY DEFAULT;
            // ALWAYS's reject-explicit-values nuance is documented
            // leniency. Generated EXPRESSION columns
            // (`AS (expr) STORED`) are not supported: error loudly
            // instead of silently storing NULLs.
            if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("generated")) {
                self.advance();
                match self.peek().clone() {
                    Token::Ident(s) if s.eq_ignore_ascii_case("always") => {
                        self.advance();
                    }
                    // `BY` is a reserved keyword token (GROUP BY).
                    Token::By => {
                        self.advance();
                        if !matches!(self.peek(), Token::Default) {
                            return Err(self.err(alloc::format!(
                                "expected DEFAULT after GENERATED BY, got {:?}",
                                self.peek()
                            )));
                        }
                        self.advance();
                    }
                    other => {
                        return Err(self.err(alloc::format!(
                            "expected ALWAYS or BY DEFAULT after GENERATED, got {other:?}"
                        )));
                    }
                }
                if !matches!(self.peek(), Token::As) {
                    return Err(self.err(alloc::format!(
                        "expected AS after GENERATED ALWAYS/BY DEFAULT, got {:?}",
                        self.peek()
                    )));
                }
                self.advance();
                // v7.37.7(sentori Epic 3 P1)— `GENERATED ALWAYS AS
                // ( <expr> ) STORED` stored computed-column. The
                // expression is captured for the engine to recompute
                // on every INSERT / UPDATE. v7.37.7 accepts the
                // STORED keyword only; PG also has VIRTUAL, which
                // v7.37.7 carves out (sentori only uses STORED).
                if matches!(self.peek(), Token::LParen) {
                    self.advance();
                    let expr = self.parse_expr(0)?;
                    if !matches!(self.peek(), Token::RParen) {
                        return Err(self.err(alloc::format!(
                            "expected ')' after GENERATED ALWAYS AS (<expr>), got {:?}",
                            self.peek()
                        )));
                    }
                    self.advance();
                    let stored = match self.peek() {
                        Token::Ident(s) | Token::QuotedIdent(s)
                            if s.eq_ignore_ascii_case("stored") =>
                        {
                            self.advance();
                            true
                        }
                        Token::Ident(s) | Token::QuotedIdent(s)
                            if s.eq_ignore_ascii_case("virtual") =>
                        {
                            return Err(self.err(
                                "GENERATED ALWAYS AS (expr) VIRTUAL is not supported \
                                 at v7.37.7; use STORED"
                                    .into(),
                            ));
                        }
                        other => {
                            return Err(self.err(alloc::format!(
                                "expected STORED after GENERATED ALWAYS AS (<expr>), \
                                 got {other:?}"
                            )));
                        }
                    };
                    let _ = stored; // currently STORED-only; flag reserved for VIRTUAL.
                    generated_stored_expr = Some(Box::new(expr));
                    continue;
                }
                self.expect_keyword_ident("identity")?;
                // Optional `(START WITH 1 INCREMENT BY 1 …)` —
                // consume the balanced parens and discard (SPG's
                // auto-increment is max+1-scan based).
                if matches!(self.peek(), Token::LParen) {
                    let mut depth = 0usize;
                    loop {
                        match self.advance() {
                            Token::LParen => depth += 1,
                            Token::RParen => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            Token::Eof => {
                                return Err(self.err(
                                    "unterminated sequence-options parens after IDENTITY".into(),
                                ));
                            }
                            _ => {}
                        }
                    }
                }
                auto_increment = true;
                // PG identity columns are implicitly NOT NULL.
                nullable = false;
                continue;
            }
            // v7.17.0 Phase 2.1 — MySQL `ON UPDATE
            // CURRENT_TIMESTAMP[(N)]`. Only CURRENT_TIMESTAMP
            // is accepted today. The "ON" token is an Ident
            // (not reserved) — peek before consuming.
            if matches!(self.peek(), Token::On)
                && matches!(self.tokens.get(self.pos + 1), Some(Token::Ident(s)) if s.eq_ignore_ascii_case("update"))
            {
                self.advance(); // ON
                self.advance(); // update
                // Accept CURRENT_TIMESTAMP / CURRENT_TIMESTAMP(N).
                let next = self.peek().clone();
                match next {
                    Token::Ident(s) | Token::QuotedIdent(s)
                        if s.eq_ignore_ascii_case("current_timestamp") =>
                    {
                        self.advance();
                        // Optional `(N)` precision.
                        if matches!(self.peek(), Token::LParen) {
                            self.advance();
                            if !matches!(self.peek(), Token::Integer(_)) {
                                return Err(self.err(alloc::format!(
                                    "expected integer precision inside CURRENT_TIMESTAMP(…), got {:?}",
                                    self.peek()
                                )));
                            }
                            self.advance();
                            if !matches!(self.peek(), Token::RParen) {
                                return Err(self.err(alloc::format!(
                                    "expected ')' after CURRENT_TIMESTAMP precision, got {:?}",
                                    self.peek()
                                )));
                            }
                            self.advance();
                        }
                        on_update_runtime = Some(Expr::FunctionCall {
                            name: "now".into(),
                            args: Vec::new(),
                        });
                        continue;
                    }
                    other => {
                        return Err(self.err(alloc::format!(
                            "v7.17 only supports ON UPDATE CURRENT_TIMESTAMP, got {other:?}"
                        )));
                    }
                }
            }
            if matches!(self.peek(), Token::Default) {
                if default.is_some() {
                    return Err(self.err("DEFAULT specified twice".into()));
                }
                self.advance();
                default = Some(self.parse_expr(0)?);
                continue;
            }
            if matches!(self.peek(), Token::Not) {
                if nullability_seen {
                    return Err(self.err("NOT NULL specified twice".into()));
                }
                self.advance();
                if !matches!(self.peek(), Token::Null) {
                    return Err(self.err(format!(
                        "expected NULL after NOT in column def, got {:?}",
                        self.peek()
                    )));
                }
                self.advance();
                nullable = false;
                nullability_seen = true;
                continue;
            }
            // v7.14.0 — MySQL accepts a bare `NULL` as an explicit
            // "this column is nullable" marker (the default in
            // standard SQL anyway). mysqldump emits it routinely
            // (`col TYPE NULL DEFAULT NULL` for nullable
            // timestamps etc). Accept + no-op.
            if matches!(self.peek(), Token::Null) {
                if nullability_seen && !nullable {
                    return Err(self.err("column declared NOT NULL then NULL — pick one".into()));
                }
                self.advance();
                nullable = true;
                nullability_seen = true;
                continue;
            }
            // `AUTO_INCREMENT` or its abbreviated form `AUTOINCREMENT`
            // arrives as a bare Ident. Match either, case-insensitive.
            if let Token::Ident(s) = self.peek()
                && (s.eq_ignore_ascii_case("auto_increment")
                    || s.eq_ignore_ascii_case("autoincrement"))
            {
                if auto_increment {
                    return Err(self.err("AUTO_INCREMENT specified twice".into()));
                }
                self.advance();
                auto_increment = true;
                continue;
            }
            // v7.9.13 — inline `PRIMARY KEY` column constraint
            // (mailrs F1). Implies `NOT NULL`. The engine creates
            // a BTree index for the PK column at CREATE TABLE time
            // so FK parent-side index lookups resolve.
            if let Token::Ident(s) = self.peek()
                && s.eq_ignore_ascii_case("primary")
            {
                if is_primary_key {
                    return Err(self.err("PRIMARY KEY specified twice".into()));
                }
                // Peek-ahead for the required `KEY` token.
                let next = self.tokens.get(self.pos + 1);
                let next_is_key = matches!(
                    next,
                    Some(Token::Ident(k)) if k.eq_ignore_ascii_case("key")
                );
                if !next_is_key {
                    return Err(self.err(format!(
                        "expected KEY after PRIMARY in column def, got {:?}",
                        next
                    )));
                }
                self.advance(); // PRIMARY
                self.advance(); // KEY
                is_primary_key = true;
                if nullability_seen && nullable {
                    return Err(self.err(
                        "column declared NULL but inline PRIMARY KEY implies NOT NULL".into(),
                    ));
                }
                nullable = false;
                nullability_seen = true;
                continue;
            }
            // v7.13.0 — inline `UNIQUE` column constraint
            // (mailrs round-5 G2). Fold into a single-column
            // table-level UNIQUE at CREATE TABLE post-process time.
            if let Token::Ident(s) = self.peek()
                && s.eq_ignore_ascii_case("unique")
            {
                if is_unique {
                    return Err(self.err("UNIQUE specified twice".into()));
                }
                self.advance();
                is_unique = true;
                continue;
            }
            // v7.13.0 — inline `CHECK (<expr>)` column constraint
            // (mailrs round-5 G3). PG semantics: column-level
            // CHECK is equivalent to a table-level CHECK. Multiple
            // inline CHECKs on the same column AND together.
            if let Token::Ident(s) = self.peek()
                && s.eq_ignore_ascii_case("check")
            {
                self.advance();
                if !matches!(self.peek(), Token::LParen) {
                    return Err(self.err(alloc::format!(
                        "expected '(' after CHECK in column def, got {:?}",
                        self.peek()
                    )));
                }
                self.advance();
                let pred = self.parse_expr(0)?;
                if !matches!(self.peek(), Token::RParen) {
                    return Err(self.err(alloc::format!(
                        "expected ')' to close CHECK predicate, got {:?}",
                        self.peek()
                    )));
                }
                self.advance();
                check = Some(match check.take() {
                    Some(prev) => Expr::Binary {
                        op: BinOp::And,
                        lhs: Box::new(prev),
                        rhs: Box::new(pred),
                    },
                    None => pred,
                });
                continue;
            }
            break;
        }
        Ok(ColumnDef {
            name,
            ty,
            nullable,
            default,
            auto_increment,
            is_primary_key,
            is_unique,
            check,
            user_type_ref,
            on_update_runtime,
            collation,
            is_unsigned,
            inline_enum_variants,
            inline_set_variants,
            generated_stored_expr,
        })
    }

    /// `NUMERIC` may appear without parameters, with one (precision
    /// only, scale=0), or with both. Returns `(precision, scale)` with
    /// 0 = unspecified for the bare form.
    fn parse_optional_numeric_params(&mut self) -> Result<(u8, u8), ParseError> {
        if !matches!(self.peek(), Token::LParen) {
            // Bare `NUMERIC` — PG treats this as "unlimited precision";
            // we surface it as precision=0 to mean "unconstrained" so
            // the engine doesn't need a separate variant.
            return Ok((0, 0));
        }
        self.advance();
        let precision = match self.advance() {
            Token::Integer(n) if (1..=38).contains(&n) => u8::try_from(n).expect("range-checked"),
            other => {
                return Err(ParseError {
                    message: format!(
                        "NUMERIC precision must be an integer in 1..=38, got {other:?}"
                    ),
                    token_pos: self.pos.saturating_sub(1),
                });
            }
        };
        let scale = if matches!(self.peek(), Token::Comma) {
            self.advance();
            match self.advance() {
                Token::Integer(n) if (0..=i64::from(precision)).contains(&n) => {
                    u8::try_from(n).expect("range-checked")
                }
                other => {
                    return Err(ParseError {
                        message: format!(
                            "NUMERIC scale must be a non-negative integer ≤ precision, got {other:?}"
                        ),
                        token_pos: self.pos.saturating_sub(1),
                    });
                }
            }
        } else {
            0
        };
        if !matches!(self.peek(), Token::RParen) {
            return Err(self.err(format!(
                "expected ')' to close NUMERIC params, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        Ok((precision, scale))
    }

    /// Parse `(N)` where `N` is a positive integer literal — used by the
    /// `VARCHAR`/`CHAR`/`VECTOR` column types. `label` is the type name
    /// for the error message.
    /// v6.0.1: parse the optional `USING <encoding>` clause that
    /// follows `VECTOR(N)` in a column definition. Missing clause
    /// → `VecEncoding::F32` (pre-v6 default). Unknown encoding
    /// ident → `ParseError` listing the encodings recognised today.
    fn parse_optional_vector_encoding(&mut self) -> Result<VecEncoding, ParseError> {
        if !matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("using")) {
            return Ok(VecEncoding::F32);
        }
        // v7.13.2 — mailrs round-6 S6: `USING` after a vector type
        // overlaps with `ALTER COLUMN TYPE … USING <expr>`. Only
        // consume the token when the very next token is a known
        // vector-encoding keyword (SQ8 / HALF). Otherwise leave
        // `USING` for the caller — it's the rewrite-expression form.
        let n1 = self.tokens.get(self.pos + 1);
        let next_is_encoding = matches!(
            n1,
            Some(Token::Ident(s))
                if s.eq_ignore_ascii_case("sq8") || s.eq_ignore_ascii_case("half")
        );
        if !next_is_encoding {
            return Ok(VecEncoding::F32);
        }
        self.advance();
        let enc_ident = match self.advance() {
            Token::Ident(s) => s,
            other => {
                return Err(self.err(format!(
                    "expected vector encoding after USING, got {other:?}"
                )));
            }
        };
        match enc_ident.to_ascii_lowercase().as_str() {
            "sq8" => Ok(VecEncoding::Sq8),
            // v6.0.3: `HALF` (pgvector convention) selects IEEE-754
            // binary16 per-element storage.
            "half" => Ok(VecEncoding::F16),
            other => Err(self.err(format!(
                "unknown vector encoding {other:?}; supported: SQ8, HALF"
            ))),
        }
    }

    /// v7.17.0 Phase 4.3 — peek at the MySQL display-width
    /// without consuming it. Returns `Some(N)` when the next
    /// tokens are `( <int> )`; None otherwise. Used by the
    /// TINYINT classifier to decide whether to map to Bool or
    /// SmallInt.
    fn peek_optional_paren_size_value(&self) -> Option<i64> {
        if !matches!(self.peek(), Token::LParen) {
            return None;
        }
        let next = self.tokens.get(self.pos + 1)?;
        let n = match next {
            Token::Integer(n) => *n,
            _ => return None,
        };
        if !matches!(self.tokens.get(self.pos + 2), Some(Token::RParen)) {
            return None;
        }
        Some(n)
    }

    /// v7.14.0 — consume an optional MySQL display-width
    /// parenthesised number after an integer type, returning
    /// nothing. `TINYINT(1)` etc.
    fn consume_optional_paren_size(&mut self) {
        if !matches!(self.peek(), Token::LParen) {
            return;
        }
        self.advance();
        // Skip until matching RParen (allow nested or any tokens).
        let mut depth = 1usize;
        while depth > 0 {
            match self.peek() {
                Token::LParen => depth += 1,
                Token::RParen => depth -= 1,
                Token::Eof => return,
                _ => {}
            }
            self.advance();
        }
    }

    fn parse_paren_size(&mut self, label: &str) -> Result<u32, ParseError> {
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(format!("{label} type requires (N), got {:?}", self.peek())));
        }
        self.advance();
        let n = match self.advance() {
            Token::Integer(n) if n > 0 => u32::try_from(n).map_err(|_| ParseError {
                message: format!("{label} size too large: {n}"),
                token_pos: self.pos.saturating_sub(1),
            })?,
            other => {
                return Err(ParseError {
                    message: format!("expected positive integer {label} size, got {other:?}"),
                    token_pos: self.pos.saturating_sub(1),
                });
            }
        };
        if !matches!(self.peek(), Token::RParen) {
            return Err(self.err(format!(
                "expected ')' after {label} size, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        Ok(n)
    }

    fn parse_insert_stmt(&mut self) -> Result<Statement, ParseError> {
        debug_assert!(matches!(self.peek(), Token::Insert));
        self.advance();
        if !matches!(self.peek(), Token::Into) {
            return Err(self.err(format!("expected INTO after INSERT, got {:?}", self.peek())));
        }
        self.advance();
        let table = self.expect_ident_like()?;
        // Optional column list — `INSERT INTO t (a, b) VALUES ...`.
        let columns = if matches!(self.peek(), Token::LParen) {
            self.advance();
            let mut names = Vec::new();
            loop {
                names.push(self.expect_ident_like()?);
                match self.peek() {
                    Token::Comma => {
                        self.advance();
                    }
                    Token::RParen => {
                        self.advance();
                        break;
                    }
                    other => {
                        return Err(self.err(format!(
                            "expected ',' or ')' in INSERT column list, got {other:?}"
                        )));
                    }
                }
            }
            Some(names)
        } else {
            None
        };
        // PG 10+ `OVERRIDING {SYSTEM | USER} VALUE` — pg_dump emits
        // it for identity columns. SPG always uses the values the
        // statement supplies (identity defaults only fill omitted
        // columns), which is exactly OVERRIDING SYSTEM VALUE
        // semantics; accept and absorb both spellings.
        if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("overriding")) {
            self.advance();
            let which = self.expect_ident_like()?;
            if !which.eq_ignore_ascii_case("system") && !which.eq_ignore_ascii_case("user") {
                return Err(self.err(format!(
                    "expected SYSTEM or USER after OVERRIDING, got {which:?}"
                )));
            }
            if !matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("value")) {
                return Err(self.err(format!(
                    "expected VALUE after OVERRIDING {}, got {:?}",
                    which.to_ascii_uppercase(),
                    self.peek()
                )));
            }
            self.advance();
        }
        // `INSERT INTO t DEFAULT VALUES` — a single row made
        // entirely of column defaults. Lower to the permuted
        // column-list path with an empty list: every schema column
        // is unmapped, so the engine fills each from its default
        // (serials advance, plain defaults evaluate, the rest NULL).
        if matches!(self.peek(), Token::Default) {
            self.advance();
            if !matches!(self.peek(), Token::Values) {
                return Err(self.err(format!(
                    "expected VALUES after DEFAULT in INSERT, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            if columns.is_some() {
                return Err(self.err(
                    "DEFAULT VALUES cannot follow an INSERT column list".into(),
                ));
            }
            let on_conflict = self.parse_optional_on_conflict()?;
            let returning = self.parse_optional_returning()?;
            return Ok(Statement::Insert(InsertStatement {
                ctes: Vec::new(),
                table,
                columns: Some(Vec::new()),
                rows: alloc::vec![Vec::new()],
                select_source: None,
                on_conflict,
                returning,
            }));
        }
        // v7.13.0 — `INSERT INTO t [(cols)] SELECT …` (mailrs
        // round-5 G4). Dispatch on VALUES vs SELECT.
        if matches!(self.peek(), Token::Select) {
            let select_stmt = match self.parse_select_stmt()? {
                Statement::Select(s) => s,
                other => {
                    return Err(self.err(alloc::format!(
                        "expected SELECT after INSERT INTO ... target, got {other:?}"
                    )));
                }
            };
            let on_conflict = self.parse_optional_on_conflict()?;
            let returning = self.parse_optional_returning()?;
            return Ok(Statement::Insert(InsertStatement {
                ctes: Vec::new(),
                table,
                columns,
                rows: Vec::new(),
                select_source: Some(Box::new(select_stmt)),
                on_conflict,
                returning,
            }));
        }
        if !matches!(self.peek(), Token::Values) {
            return Err(self.err(format!(
                "expected VALUES or SELECT after table name, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(format!("expected '(' after VALUES, got {:?}", self.peek())));
        }
        let mut rows = Vec::new();
        loop {
            // Each iteration consumes one `(expr, expr, …)` tuple.
            if !matches!(self.peek(), Token::LParen) {
                return Err(self.err(format!(
                    "expected '(' for next VALUES tuple, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            let mut tuple = Vec::new();
            loop {
                tuple.push(self.parse_expr(0)?);
                match self.peek() {
                    Token::Comma => {
                        self.advance();
                    }
                    Token::RParen => {
                        self.advance();
                        break;
                    }
                    other => {
                        return Err(self.err(format!(
                            "expected ',' or ')' in VALUES tuple, got {other:?}"
                        )));
                    }
                }
            }
            if tuple.is_empty() {
                return Err(self.err("INSERT VALUES tuple requires at least one value".into()));
            }
            rows.push(tuple);
            // Continue with comma-separated tuples.
            if matches!(self.peek(), Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        let on_conflict = self.parse_optional_on_conflict()?;
        let returning = self.parse_optional_returning()?;
        Ok(Statement::Insert(InsertStatement {
            ctes: Vec::new(),
            table,
            columns,
            rows,
            select_source: None,
            on_conflict,
            returning,
        }))
    }

    /// v7.9.7 — parse the optional `ON CONFLICT (cols) DO …`
    /// clause sitting between the INSERT body and the trailing
    /// RETURNING. All keywords come in as bare idents; `ON` is
    /// a reserved Token though.
    fn parse_optional_on_conflict(
        &mut self,
    ) -> Result<Option<crate::ast::OnConflictClause>, ParseError> {
        if !matches!(self.peek(), Token::On) {
            return Ok(None);
        }
        // Peek further: we want exactly "ON CONFLICT ...". If the
        // next ident isn't "conflict", let some other parser handle.
        let next_is_conflict = matches!(
            self.tokens.get(self.pos + 1),
            Some(Token::Ident(s) | Token::QuotedIdent(s)) if s.eq_ignore_ascii_case("conflict")
        );
        if !next_is_conflict {
            return Ok(None);
        }
        self.advance(); // ON
        self.advance(); // CONFLICT
        // v7.37.17 (17.6 siblings) — `ON CONSTRAINT <name>` names
        // the constraint instead of listing columns (the pg_dump
        // form); the engine resolves it.
        let mut constraint_name: Option<String> = None;
        if matches!(self.peek(), Token::On) {
            self.advance(); // ON
            match self.advance() {
                Token::Ident(s) | Token::QuotedIdent(s)
                    if s.eq_ignore_ascii_case("constraint") => {}
                other => {
                    return Err(self.err(alloc::format!(
                        "expected CONSTRAINT after ON CONFLICT ON, got {other:?}"
                    )));
                }
            }
            constraint_name = Some(self.expect_ident_like()?);
        }
        // Optional `(col [, col]*)` target list.
        let mut target_columns: Vec<String> = Vec::new();
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            loop {
                target_columns.push(self.expect_ident_like()?);
                match self.peek() {
                    Token::Comma => {
                        self.advance();
                    }
                    Token::RParen => {
                        self.advance();
                        break;
                    }
                    other => {
                        return Err(self.err(alloc::format!(
                            "expected ',' or ')' in ON CONFLICT target list, got {other:?}"
                        )));
                    }
                }
            }
        }
        // Required `DO`.
        match self.advance() {
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("do") => {}
            other => {
                return Err(self.err(alloc::format!(
                    "expected DO after ON CONFLICT [(…)], got {other:?}"
                )));
            }
        }
        // Action: NOTHING | UPDATE SET …
        let action = match self.advance() {
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("nothing") => {
                crate::ast::OnConflictAction::Nothing
            }
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("update") => {
                self.parse_on_conflict_update_action()?
            }
            other => {
                return Err(self.err(alloc::format!(
                    "expected NOTHING or UPDATE after ON CONFLICT DO, got {other:?}"
                )));
            }
        };
        Ok(Some(crate::ast::OnConflictClause {
            constraint_name,
            target_columns,
            action,
        }))
    }

    /// v7.9.7 — tail of `ON CONFLICT … DO UPDATE`: parse
    /// `SET col = expr [, …] [WHERE cond]`. Caller already
    /// consumed `UPDATE`.
    fn parse_on_conflict_update_action(
        &mut self,
    ) -> Result<crate::ast::OnConflictAction, ParseError> {
        // `SET`
        match self.advance() {
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("set") => {}
            other => {
                return Err(self.err(alloc::format!(
                    "expected SET after ON CONFLICT DO UPDATE, got {other:?}"
                )));
            }
        }
        let mut assignments: Vec<(String, Expr)> = Vec::new();
        loop {
            let col = self.expect_ident_like()?;
            if !matches!(self.peek(), Token::Eq) {
                return Err(self.err(alloc::format!(
                    "expected `=` after column in ON CONFLICT DO UPDATE SET, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            let value = self.parse_expr(0)?;
            assignments.push((col, value));
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        let where_ = if matches!(self.peek(), Token::Where) {
            self.advance();
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        Ok(crate::ast::OnConflictAction::Update {
            assignments,
            where_,
        })
    }

    fn parse_select_list(&mut self) -> Result<Vec<SelectItem>, ParseError> {
        let mut items = Vec::new();
        loop {
            items.push(self.parse_select_item()?);
            if matches!(self.peek(), Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        Ok(items)
    }

    fn parse_select_item(&mut self) -> Result<SelectItem, ParseError> {
        if matches!(self.peek(), Token::Star) {
            self.advance();
            return Ok(SelectItem::Wildcard);
        }
        let expr = self.parse_expr(0)?;
        let alias = self.parse_optional_alias();
        Ok(SelectItem::Expr { expr, alias })
    }

    /// v7.37.17 (17.6 siblings) — parse `(row), (row), …` after a
    /// consumed VALUES keyword. Each row lowers to a constant SELECT
    /// with PG's default column1..columnN names; subsequent rows
    /// chain as UNION ALL peers. Shared by the FROM-position
    /// `( VALUES … )` arm and the top-level bare VALUES statement.
    fn parse_values_rows_body(&mut self) -> Result<SelectStatement, ParseError> {
        let mut row_selects: Vec<SelectStatement> = Vec::new();
        loop {
            if !matches!(self.peek(), Token::LParen) {
                return Err(self.err(alloc::format!(
                    "expected '(' to start a VALUES row, got {:?}",
                    self.peek()
                )));
            }
            self.advance(); // (
            let mut items: Vec<SelectItem> = Vec::new();
            loop {
                let expr = self.parse_expr(0)?;
                items.push(SelectItem::Expr {
                    expr,
                    alias: Some(alloc::format!("column{}", items.len() + 1)),
                });
                match self.peek() {
                    Token::Comma => {
                        self.advance();
                    }
                    Token::RParen => break,
                    other => {
                        return Err(self.err(alloc::format!(
                            "expected ',' or ')' in VALUES row, got {other:?}"
                        )));
                    }
                }
            }
            self.advance(); // )
            row_selects.push(SelectStatement {
                ctes: Vec::new(),
                distinct: false,
                distinct_on: Vec::new(),
                items,
                from: None,
                where_: None,
                group_by: None,
                group_by_all: false,
                having: None,
                unions: Vec::new(),
                order_by: Vec::new(),
                limit: None,
                offset: None,
                limit_with_ties: false,
            });
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        let mut head = row_selects.remove(0);
        head.unions = row_selects
            .into_iter()
            .map(|s| (UnionKind::All, s))
            .collect();
        Ok(head)
    }

    fn parse_table_ref(&mut self) -> Result<TableRef, ParseError> {
        // v7.37.43-T4.5 — `LATERAL jsonb_each_text(<expr>)` —
        // set-returning function whose argument may reference a
        // preceding FROM item. We rewrite this to
        // `LATERAL (SELECT key, value FROM jsonb_each_text(<expr>)
        // AS __srf__) AS <alias>` so the existing LATERAL subquery
        // executor handles per-outer-row evaluation and the
        // SRF-primary jsonb_each_text path handles the inner
        // materialisation. Sentori 0067 backfill is the dogfood
        // shape.
        if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("lateral"))
            && matches!(self.tokens.get(self.pos + 1), Some(Token::Ident(s) | Token::QuotedIdent(s)) if is_json_each_name(s))
            && matches!(self.tokens.get(self.pos + 2), Some(Token::LParen))
        {
            self.advance(); // LATERAL
            let each_fn = match self.peek() {
                Token::Ident(s) | Token::QuotedIdent(s) => s.to_ascii_lowercase(),
                _ => unreachable!(),
            };
            self.advance(); // jsonb_each[_text] / json_each[_text]
            self.advance(); // (
            let arg = self.parse_expr(0)?;
            if !matches!(self.peek(), Token::RParen) {
                return Err(self.err(alloc::format!(
                    "expected ')' after LATERAL {each_fn}() argument, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            let (alias_ident, column_aliases) = self.parse_optional_alias_with_columns();
            let alias = alias_ident.clone().unwrap_or_else(|| each_fn.clone());
            // Synthesise: SELECT __srf__.key AS <key_alias>, __srf__.value AS <value_alias>
            //               FROM jsonb_each_text(<arg>) AS __srf__
            // PG's `AS kv(key, value)` column-alias list maps
            // positions to names; default to (key, value) when
            // omitted (matching the SRF's natural column names).
            let srf_alias = "__srf__".to_string();
            let key_alias = column_aliases
                .first()
                .cloned()
                .unwrap_or_else(|| "key".to_string());
            let value_alias = column_aliases
                .get(1)
                .cloned()
                .unwrap_or_else(|| "value".to_string());
            let inner_select = crate::ast::SelectStatement {
                ctes: Vec::new(),
                distinct: false,
                distinct_on: Vec::new(),
                items: alloc::vec![
                    crate::ast::SelectItem::Expr {
                        expr: crate::ast::Expr::Column(crate::ast::ColumnName {
                            qualifier: Some(srf_alias.clone()),
                            name: "key".to_string(),
                        }),
                        alias: Some(key_alias),
                    },
                    crate::ast::SelectItem::Expr {
                        expr: crate::ast::Expr::Column(crate::ast::ColumnName {
                            qualifier: Some(srf_alias.clone()),
                            name: "value".to_string(),
                        }),
                        alias: Some(value_alias),
                    },
                ],
                from: Some(crate::ast::FromClause {
                    primary: TableRef {
                        name: srf_alias.clone(),
                        alias: Some(srf_alias.clone()),
                        as_of_segment: None,
                        unnest_expr: None,
                        unnest_column_aliases: Vec::new(),
                        generate_series_args: None,
                        lateral_subquery: None,
                        jsonb_each_text_arg: Some((each_fn, Box::new(arg))),
                    },
                    joins: Vec::new(),
                }),
                where_: None,
                group_by: None,
                group_by_all: false,
                having: None,
                unions: Vec::new(),
                order_by: Vec::new(),
                limit: None,
                offset: None,
                limit_with_ties: false,
            };
            return Ok(TableRef {
                name: alias.clone(),
                alias: Some(alias),
                as_of_segment: None,
                unnest_expr: None,
                unnest_column_aliases: Vec::new(),
                generate_series_args: None,
                lateral_subquery: Some(Box::new(inner_select)),
                jsonb_each_text_arg: None,
            });
        }
        // v7.37.43-T4.5 — bare `CROSS JOIN jsonb_each_text(t.col)`
        // without an explicit `LATERAL` keyword is the same shape
        // PG accepts (SRF naturally licences lateral correlation).
        // We mirror the LATERAL rewrite when the argument syntactic-
        // ally references an outer column (Column { qualifier:
        // Some(_), … }). For simplicity we apply the rewrite
        // whenever the SRF directly follows JOIN/CROSS JOIN/comma
        // in the FROM-list — caller-side join parsing positions
        // this peek correctly.
        // (Implementation note: detection lives below; the LATERAL
        // branch above already covers the explicit form; the bare
        // form falls through to the plain SRF arm and the engine
        // treats it as a constant-arg SRF if no outer reference is
        // present.)
        // v7.17.0 Phase 3.P0-41 — `LATERAL ( SELECT … )` derived
        // table. Detect at the head so it claims precedence over
        // every other table-ref shape (unnest / generate_series /
        // bare ident); the lateral subquery itself follows the
        // regular SELECT grammar.
        // v7.37.17 (17.6 siblings) — `FROM ( VALUES (…), (…) ) [AS]
        // t(cols)`. Each row lowers to a constant SELECT with PG's
        // default column1..columnN names; subsequent rows chain as
        // UNION ALL peers. The result rides the derived-table
        // lateral_subquery channel — zero executor work.
        if matches!(self.peek(), Token::LParen)
            && matches!(self.tokens.get(self.pos + 1), Some(Token::Values))
        {
            self.advance(); // (
            self.advance(); // VALUES
            let head = self.parse_values_rows_body()?;
            if !matches!(self.peek(), Token::RParen) {
                return Err(self.err(alloc::format!(
                    "expected ')' after VALUES list, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            let (alias_ident, column_aliases) = self.parse_optional_alias_with_columns();
            let name = alias_ident.clone().unwrap_or_else(|| "values".to_string());
            return Ok(TableRef {
                name,
                alias: alias_ident,
                as_of_segment: None,
                unnest_expr: None,
                unnest_column_aliases: column_aliases,
                generate_series_args: None,
                lateral_subquery: Some(Box::new(head)),
                jsonb_each_text_arg: None,
            });
        }
        // v7.37.17 (17.6 siblings) — plain derived table:
        // `FROM ( SELECT … ) [AS] alias`. Rides the same
        // lateral_subquery channel the explicit LATERAL form uses —
        // an uncorrelated inner SELECT executes identically. The
        // inner parse carries UNION tails (they live on
        // SelectStatement.unions).
        if matches!(self.peek(), Token::LParen)
            && matches!(self.tokens.get(self.pos + 1), Some(Token::Select))
        {
            self.advance(); // (
            let inner = match self.parse_one_statement()? {
                Statement::Select(s) => s,
                other => {
                    return Err(self.err(alloc::format!(
                        "expected SELECT inside derived table ( … ), got {other:?}"
                    )));
                }
            };
            if !matches!(self.peek(), Token::RParen) {
                return Err(self.err(alloc::format!(
                    "expected ')' after derived-table subquery, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            // `AS t(a, b)` column-alias list rides the
            // unnest_column_aliases field (same positional-rename
            // contract the unnest SRFs use).
            let (alias_ident, column_aliases) = self.parse_optional_alias_with_columns();
            let name = alias_ident
                .clone()
                .unwrap_or_else(|| "subquery".to_string());
            return Ok(TableRef {
                name,
                alias: alias_ident,
                as_of_segment: None,
                unnest_expr: None,
                unnest_column_aliases: column_aliases,
                generate_series_args: None,
                lateral_subquery: Some(Box::new(inner)),
                jsonb_each_text_arg: None,
            });
        }
        if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("lateral"))
            && matches!(self.tokens.get(self.pos + 1), Some(Token::LParen))
        {
            self.advance(); // LATERAL
            self.advance(); // (
            // Parse the inner SELECT.
            let inner = match self.parse_one_statement()? {
                Statement::Select(s) => s,
                other => {
                    return Err(self.err(alloc::format!(
                        "expected SELECT inside LATERAL ( … ), got {other:?}"
                    )));
                }
            };
            if !matches!(self.peek(), Token::RParen) {
                return Err(self.err(alloc::format!(
                    "expected ')' after LATERAL subquery, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            let alias_ident = self.parse_optional_alias();
            let name = alias_ident.clone().unwrap_or_else(|| "lateral".to_string());
            return Ok(TableRef {
                name,
                alias: alias_ident,
                as_of_segment: None,
                unnest_expr: None,
                unnest_column_aliases: Vec::new(),
                generate_series_args: None,
                lateral_subquery: Some(Box::new(inner)),
                jsonb_each_text_arg: None,
            });
        }
        // v7.37.43-T4.5 — `jsonb_each_text(<expr>)` set-returning
        // function as a FROM item. Emits one row per (key, value)
        // pair in the JSONB object argument as TEXT columns. May
        // be wrapped in CROSS JOIN LATERAL when the argument
        // references a preceding FROM item (sentori migration
        // 0067 backfill shape: `CROSS JOIN LATERAL
        // jsonb_each_text(t.json_col) AS kv(key, value)`).
        if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if is_json_each_name(s))
            && matches!(self.tokens.get(self.pos + 1), Some(Token::LParen))
        {
            let each_fn = match self.peek() {
                Token::Ident(s) | Token::QuotedIdent(s) => s.to_ascii_lowercase(),
                _ => unreachable!(),
            };
            self.advance(); // jsonb_each[_text] / json_each[_text]
            self.advance(); // (
            let arg = self.parse_expr(0)?;
            if !matches!(self.peek(), Token::RParen) {
                return Err(self.err(alloc::format!(
                    "expected ')' after {each_fn}() argument, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            let (alias_ident, _column_aliases) = self.parse_optional_alias_with_columns();
            let name = alias_ident.clone().unwrap_or_else(|| each_fn.clone());
            return Ok(TableRef {
                name,
                alias: alias_ident,
                as_of_segment: None,
                unnest_expr: None,
                unnest_column_aliases: Vec::new(),
                generate_series_args: None,
                lateral_subquery: None,
                jsonb_each_text_arg: Some((each_fn, Box::new(arg))),
            });
        }
        // v7.37.17 (17.6 siblings) — `jsonb_array_elements[_text](<expr>)`
        // / json_ variants as a FROM item. Rewritten into
        // `unnest(<same fn>(<expr>))`: the scalar form returns the
        // elements as a TEXT array, and the existing unnest SRF path
        // materialises one row per element. PG's natural column name
        // is `value`; an `AS a(col)` column-alias list overrides it.
        if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                if s.eq_ignore_ascii_case("jsonb_array_elements")
                    || s.eq_ignore_ascii_case("json_array_elements")
                    || s.eq_ignore_ascii_case("jsonb_array_elements_text")
                    || s.eq_ignore_ascii_case("json_array_elements_text")
                    || s.eq_ignore_ascii_case("jsonb_object_keys")
                    || s.eq_ignore_ascii_case("json_object_keys")
                    || s.eq_ignore_ascii_case("generate_subscripts")
                    || s.eq_ignore_ascii_case("string_to_table")
                    || s.eq_ignore_ascii_case("regexp_split_to_table"))
            && matches!(self.tokens.get(self.pos + 1), Some(Token::LParen))
        {
            let fn_name = match self.peek() {
                Token::Ident(s) | Token::QuotedIdent(s) => s.to_ascii_lowercase(),
                _ => unreachable!(),
            };
            self.advance(); // fn name
            self.advance(); // (
            let mut fn_args: Vec<Expr> = Vec::new();
            loop {
                fn_args.push(self.parse_expr(0)?);
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                    continue;
                }
                break;
            }
            if !matches!(self.peek(), Token::RParen) {
                return Err(self.err(alloc::format!(
                    "expected ')' after {fn_name}() arguments, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            let (alias_ident, column_aliases) = self.parse_optional_alias_with_columns();
            let name = alias_ident.clone().unwrap_or_else(|| fn_name.clone());
            // PG's natural column name: the array-elements SRFs
            // declare an OUT parameter `value`; jsonb_object_keys
            // and generate_subscripts have none, so the column is
            // named after the function. A bare table alias on a
            // single-column SRF renames the column too (PG: `FROM
            // generate_subscripts(a, 1) AS s` projects column s) —
            // except for the OUT-parameter SRFs, whose column stays
            // `value` under a bare alias.
            let natural_col = if fn_name.ends_with("_array_elements")
                || fn_name.ends_with("_array_elements_text")
            {
                "value".to_string()
            } else {
                alias_ident.clone().unwrap_or_else(|| fn_name.clone())
            };
            let col_name = column_aliases.first().cloned().unwrap_or(natural_col);
            // The *_to_table SRFs are row-streams over the existing
            // *_to_array scalars — map the call target; the display
            // name (alias / column defaults) keeps the SRF spelling.
            let call_name = match fn_name.as_str() {
                "string_to_table" => "string_to_array".to_string(),
                "regexp_split_to_table" => "regexp_split_to_array".to_string(),
                _ => fn_name,
            };
            return Ok(TableRef {
                name,
                alias: alias_ident,
                as_of_segment: None,
                unnest_expr: Some(Box::new(crate::ast::Expr::FunctionCall {
                    name: call_name,
                    args: fn_args,
                })),
                unnest_column_aliases: alloc::vec![col_name],
                generate_series_args: None,
                lateral_subquery: None,
                jsonb_each_text_arg: None,
            });
        }
        // v7.11.7 — `FROM unnest(<expr>) [AS] <alias>` set-returning
        // source. Detect at the head before the bare-ident fallback;
        // unnest is not a reserved token.
        if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("unnest"))
            && matches!(self.tokens.get(self.pos + 1), Some(Token::LParen))
        {
            self.advance(); // unnest
            self.advance(); // (
            let expr = self.parse_expr(0)?;
            if !matches!(self.peek(), Token::RParen) {
                return Err(self.err(alloc::format!(
                    "expected ')' after unnest() argument, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            let (alias_ident, unnest_column_aliases) = self.parse_optional_alias_with_columns();
            let name = alias_ident.clone().unwrap_or_else(|| "unnest".to_string());
            return Ok(TableRef {
                name,
                alias: alias_ident,
                as_of_segment: None,
                unnest_expr: Some(Box::new(expr)),
                unnest_column_aliases,
                generate_series_args: None,
                lateral_subquery: None,
                jsonb_each_text_arg: None,
            });
        }
        // v7.17.0 Phase 3.10 — `FROM generate_series(start, stop
        // [, step])` set-returning source. Same shape as unnest:
        // detect at the head, parse the comma-separated arg list,
        // dispatch downstream through the engine's set-returning
        // path. Supports integer triplets (mailrs's `WITH row_no AS
        // (SELECT * FROM generate_series(1, N))` pattern) and
        // TIMESTAMP + INTERVAL triplets (the Tier-A audit's
        // date-range iteration pattern, which pre-3.10 had no
        // direct equivalent in SPG).
        if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("generate_series"))
            && matches!(self.tokens.get(self.pos + 1), Some(Token::LParen))
        {
            self.advance(); // generate_series
            self.advance(); // (
            let mut args: Vec<Expr> = Vec::new();
            loop {
                args.push(self.parse_expr(0)?);
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                    continue;
                }
                break;
            }
            if !matches!(self.peek(), Token::RParen) {
                return Err(self.err(alloc::format!(
                    "expected ')' after generate_series() arguments, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            if args.len() < 2 || args.len() > 3 {
                return Err(self.err(alloc::format!(
                    "generate_series() expects 2 or 3 arguments (start, stop [, step]); got {}",
                    args.len()
                )));
            }
            let (alias_ident, _column_aliases) = self.parse_optional_alias_with_columns();
            let name = alias_ident
                .clone()
                .unwrap_or_else(|| "generate_series".to_string());
            return Ok(TableRef {
                name,
                alias: alias_ident,
                as_of_segment: None,
                unnest_expr: None,
                unnest_column_aliases: Vec::new(),
                generate_series_args: Some(args),
                lateral_subquery: None,
                jsonb_each_text_arg: None,
            });
        }
        // v7.16.2 — preserve information_schema / pg_catalog
        // qualifiers (mailrs round-10 A.3). The generic
        // `expect_ident_like` strip silently drops the schema;
        // we want the engine to recognise these PG meta tables
        // and synthesise rows from the live catalog. Produce a
        // synthetic name (`__spg_info_columns` etc.) so the
        // engine's SELECT-side router can dispatch without
        // clashing with any user-defined `columns` table.
        let name = if let Some(synth) = self.try_peek_meta_qualified() {
            synth
        } else if let Some(synth) = self.try_peek_meta_bare() {
            synth
        } else {
            self.expect_ident_like()?
        };
        // v6.10.2 — optional `AS OF SEGMENT '<id>'` cold-tier
        // time-travel clause. Parse BEFORE the alias so the
        // alias can still ride at the tail (`tbl AS OF SEGMENT
        // '5' alias`). `AS` is a reserved keyword token, while
        // `OF` and `SEGMENT` are bare idents.
        let as_of_segment = if matches!(self.peek(), Token::As)
            && matches!(self.tokens.get(self.pos + 1), Some(Token::Ident(s) | Token::QuotedIdent(s)) if s.eq_ignore_ascii_case("of"))
        {
            self.advance(); // AS
            self.advance(); // OF
            let kw = match self.peek().clone() {
                Token::Ident(s) | Token::QuotedIdent(s) => s,
                other => {
                    return Err(self.err(format!("expected SEGMENT after AS OF, got {other:?}")));
                }
            };
            if !kw.eq_ignore_ascii_case("segment") {
                return Err(self.err(format!(
                    "expected SEGMENT after AS OF, got {kw:?}; v6.10.2 supports SEGMENT only"
                )));
            }
            self.advance();
            // Segment id literal — accept either a string or
            // integer for operator ergonomics.
            let id = match self.advance() {
                Token::String(s) => s
                    .parse::<u32>()
                    .map_err(|e| self.err(format!("AS OF SEGMENT id parse: {e}")))?,
                Token::Integer(n) => u32::try_from(n)
                    .map_err(|e| self.err(format!("AS OF SEGMENT id parse: {e}")))?,
                other => {
                    return Err(self.err(format!(
                        "expected segment id literal after AS OF SEGMENT, got {other:?}"
                    )));
                }
            };
            Some(id)
        } else {
            None
        };
        let alias = self.parse_optional_alias();
        Ok(TableRef {
            name,
            alias,
            as_of_segment,
            unnest_expr: None,
            unnest_column_aliases: Vec::new(),
            generate_series_args: None,
            lateral_subquery: None,
            jsonb_each_text_arg: None,
        })
    }

    /// v7.13.2 — mailrs round-6 S5. Like `parse_optional_alias`
    /// but also accepts `AS alias(col [, col, …])` — the
    /// PG-standard table-function column-list form. The column
    /// list is only honoured when paired with `UNNEST(...)` in
    /// the parent; other call sites currently discard it.
    fn parse_optional_alias_with_columns(&mut self) -> (Option<String>, Vec<String>) {
        let alias = self.parse_optional_alias();
        if alias.is_none() {
            return (None, Vec::new());
        }
        let mut cols: Vec<String> = Vec::new();
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            while let Token::Ident(s) | Token::QuotedIdent(s) = self.peek().clone() {
                self.advance();
                cols.push(s);
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                    continue;
                }
                break;
            }
            if matches!(self.peek(), Token::RParen) {
                self.advance();
            }
        }
        (alias, cols)
    }

    /// FROM-clause: a primary table reference plus zero-or-more joined
    /// peers expressed via either `, <table>` (cross-product, no ON) or
    /// `[INNER|LEFT [OUTER]|CROSS] JOIN <table> [ON expr]`. v1.10 keeps
    /// the join list flat (left-associative nested-loop semantics).
    fn parse_from_clause(&mut self) -> Result<FromClause, ParseError> {
        let primary = self.parse_table_ref()?;
        let mut joins = Vec::new();
        loop {
            // `, <table>` — cross-product with no ON.
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                let table = self.parse_table_ref()?;
                joins.push(FromJoin {
                    kind: JoinKind::Cross,
                    table,
                    on: None,
                });
                continue;
            }
            // Explicit JOIN syntax. Accept INNER JOIN, LEFT [OUTER] JOIN,
            // CROSS JOIN, and bare JOIN (defaults to INNER).
            let kind =
                match self.peek() {
                    Token::Inner => {
                        self.advance();
                        if !matches!(self.peek(), Token::Join) {
                            return Err(self
                                .err(format!("expected JOIN after INNER, got {:?}", self.peek())));
                        }
                        self.advance();
                        JoinKind::Inner
                    }
                    Token::Left => {
                        self.advance();
                        if matches!(self.peek(), Token::Outer) {
                            self.advance();
                        }
                        if !matches!(self.peek(), Token::Join) {
                            return Err(self.err(format!(
                                "expected JOIN after LEFT [OUTER], got {:?}",
                                self.peek()
                            )));
                        }
                        self.advance();
                        JoinKind::Left
                    }
                    Token::Cross => {
                        self.advance();
                        if !matches!(self.peek(), Token::Join) {
                            return Err(self
                                .err(format!("expected JOIN after CROSS, got {:?}", self.peek())));
                        }
                        self.advance();
                        JoinKind::Cross
                    }
                    Token::Join => {
                        self.advance();
                        JoinKind::Inner
                    }
                    _ => break,
                };
            let table = self.parse_table_ref()?;
            // v7.37.7 C.1 — USING (col_list) sugar. Desugars to
            // `prev_table.col1 = table.col1 AND prev_table.col2 = table.col2 …`
            // where prev_table is the most-recent left-side table
            // (the previous join's table if any, else the FROM primary).
            // PG semantics around column merging are richer (USING'd
            // cols become deduplicated single output columns); for
            // sugar purposes the predicate-only form covers the
            // baseline corpus shape and chained `… JOIN x USING (k)
            // JOIN y USING (k)` calls.
            let using_match = matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("using"));
            let on = if matches!(self.peek(), Token::On) {
                self.advance();
                Some(self.parse_expr(0)?)
            } else if using_match {
                self.advance();
                if !matches!(self.peek(), Token::LParen) {
                    return Err(
                        self.err(format!("expected '(' after USING, got {:?}", self.peek()))
                    );
                }
                self.advance();
                let mut cols: Vec<String> = Vec::new();
                loop {
                    match self.peek().clone() {
                        Token::Ident(s) | Token::QuotedIdent(s) => {
                            self.advance();
                            cols.push(s);
                        }
                        other => {
                            return Err(self.err(format!(
                                "expected column name inside USING (…), got {other:?}"
                            )));
                        }
                    }
                    match self.peek() {
                        Token::Comma => {
                            self.advance();
                            continue;
                        }
                        Token::RParen => {
                            self.advance();
                            break;
                        }
                        other => {
                            return Err(self.err(format!(
                                "expected ',' or ')' inside USING (…), got {other:?}"
                            )));
                        }
                    }
                }
                if cols.is_empty() {
                    return Err(self.err("USING (…) requires at least one column".to_string()));
                }
                // Pick the left-side alias: prev join's table if any,
                // else FROM primary. Use alias when present, else
                // table name (PG-equivalent qualifier).
                let left_qual: String = joins
                    .last()
                    .map(|j| {
                        j.table
                            .alias
                            .clone()
                            .unwrap_or_else(|| j.table.name.clone())
                    })
                    .unwrap_or_else(|| {
                        primary
                            .alias
                            .clone()
                            .unwrap_or_else(|| primary.name.clone())
                    });
                let right_qual = table.alias.clone().unwrap_or_else(|| table.name.clone());
                let mut iter = cols.into_iter().map(|c| Expr::Binary {
                    lhs: alloc::boxed::Box::new(Expr::Column(crate::ast::ColumnName {
                        qualifier: Some(left_qual.clone()),
                        name: c.clone(),
                    })),
                    op: crate::ast::BinOp::Eq,
                    rhs: alloc::boxed::Box::new(Expr::Column(crate::ast::ColumnName {
                        qualifier: Some(right_qual.clone()),
                        name: c,
                    })),
                });
                let first = iter.next().expect("at least one col");
                Some(iter.fold(first, |acc, pred| Expr::Binary {
                    lhs: alloc::boxed::Box::new(acc),
                    op: crate::ast::BinOp::And,
                    rhs: alloc::boxed::Box::new(pred),
                }))
            } else if kind == JoinKind::Cross {
                None
            } else {
                return Err(self.err(format!(
                    "expected ON or USING after {:?} JOIN, got {:?}",
                    kind,
                    self.peek()
                )));
            };
            joins.push(FromJoin { kind, table, on });
        }
        Ok(FromClause { primary, joins })
    }

    /// Optional alias after an expression or table:
    /// `AS <ident>` is unambiguous; a bare `<ident>` directly after is also
    /// accepted (PG-style implicit alias). Returns `None` if the next token
    /// is not alias-shaped (e.g. comma, FROM, WHERE, semicolon, EOF, operator).
    fn parse_optional_alias(&mut self) -> Option<String> {
        if matches!(self.peek(), Token::As) {
            self.advance();
            // After AS, the next token MUST be an identifier-like — if not,
            // we still return None and let the caller surface the error on the
            // next expectation. v0.2 keeps the alias path forgiving; the
            // corpus tests don't exercise the malformed case.
            if let Token::Ident(_) | Token::QuotedIdent(_) = self.peek() {
                return self.expect_ident_like().ok();
            }
            return None;
        }
        // v7.17.0 Phase 1.3 — implicit alias (no `AS`). PG's
        // grammar reserves a long list of follow-keywords from the
        // alias slot. SPG's bareword approximation: skip a small
        // set of idents that would otherwise be swallowed as the
        // table alias and break trailing clauses like CREATE
        // MATERIALIZED VIEW … WITH [NO] DATA or future ON
        // CONFLICT WHERE shapes.
        if let Token::Ident(s) | Token::QuotedIdent(s) = self.peek() {
            if is_alias_stopword(s) {
                return None;
            }
            return self.expect_ident_like().ok();
        }
        None
    }

    /// Pratt loop. `min_prec` is the minimum binary-op precedence we'll accept.
    fn parse_expr(&mut self, min_prec: u8) -> Result<Expr, ParseError> {
        // v7.30.2 (mailrs round-25 ask 2) — nesting budget: a parse
        // error beats a stack overflow (an overflow aborts the
        // embedding host process).
        self.enter_nested()?;
        let r = self.parse_expr_inner(min_prec);
        self.nest_depth -= 1;
        r
    }

    fn parse_expr_inner(&mut self, min_prec: u8) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_unary()?;
        let mut chain_len = 0usize;
        while let Some((op, prec)) = binop_from(self.peek()) {
            if prec < min_prec {
                break;
            }
            // v7.30.2 (mailrs round-25 ask 2) — the chain builds
            // iteratively but evaluates and drops recursively;
            // depth beyond the budget overflows worker stacks.
            chain_len += 1;
            if chain_len > MAX_BINARY_CHAIN {
                return Err(self.err(alloc::format!(
                    "more than {MAX_BINARY_CHAIN} chained binary operators; rewrite long OR-equality chains as IN (…)"
                )));
            }
            self.advance();
            // v7.10.12 — `x <op> ANY(arr)` / `x <op> ALL(arr)`.
            // ANY is a bare ident; ALL is a reserved Token. Both
            // require an immediate `(` to disambiguate from
            // identifier columns named `any` / `all`.
            let any_kind = match self.peek() {
                Token::All if matches!(self.tokens.get(self.pos + 1), Some(Token::LParen)) => {
                    Some(false)
                }
                Token::Ident(s) | Token::QuotedIdent(s)
                    if (s.eq_ignore_ascii_case("any") || s.eq_ignore_ascii_case("all"))
                        && matches!(self.tokens.get(self.pos + 1), Some(Token::LParen)) =>
                {
                    Some(s.eq_ignore_ascii_case("any"))
                }
                _ => None,
            };
            if let Some(is_any) = any_kind {
                self.advance(); // ident
                self.advance(); // (
                let arr = self.parse_expr(0)?;
                if !matches!(self.peek(), Token::RParen) {
                    return Err(self.err(alloc::format!(
                        "expected ')' after ANY/ALL argument, got {:?}",
                        self.peek()
                    )));
                }
                self.advance();
                lhs = Expr::AnyAll {
                    expr: Box::new(lhs),
                    op,
                    array: Box::new(arr),
                    is_any,
                };
                continue;
            }
            let rhs = self.parse_expr(prec + 1)?;
            lhs = Expr::Binary {
                lhs: Box::new(lhs),
                op,
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        match self.peek() {
            Token::Not => {
                self.advance();
                // NOT sits between AND (2) and comparisons (4) — bind everything
                // ≥3, which leaves AND/OR outside.
                let e = self.parse_expr(3)?;
                Ok(Expr::Unary {
                    op: UnOp::Not,
                    expr: Box::new(e),
                })
            }
            Token::Minus => {
                self.advance();
                // Unary minus binds tighter than `*`/`/` (now at prec 7 after
                // `<->` slotted into 5 and arithmetic shifted up).
                let e = self.parse_expr(8)?;
                Ok(Expr::Unary {
                    op: UnOp::Neg,
                    expr: Box::new(e),
                })
            }
            Token::Tilde => {
                self.advance();
                // Bitwise NOT binds like unary minus.
                let e = self.parse_expr(8)?;
                Ok(Expr::Unary {
                    op: UnOp::BitNot,
                    expr: Box::new(e),
                })
            }
            _ => self.parse_atom(),
        }
    }

    fn parse_atom(&mut self) -> Result<Expr, ParseError> {
        let tok_pos = self.pos;
        match self.advance() {
            Token::Integer(n) => Ok(Expr::Literal(Literal::Integer(n))),
            Token::Float(x) => Ok(Expr::Literal(Literal::Float(x))),
            Token::String(s) => Ok(Expr::Literal(Literal::String(s))),
            Token::True => Ok(Expr::Literal(Literal::Bool(true))),
            Token::False => Ok(Expr::Literal(Literal::Bool(false))),
            Token::Null => Ok(Expr::Literal(Literal::Null)),
            // v6.1.1 — `$N` placeholder. The actual Value lookup
            // happens in the engine eval path against the prepared-
            // statement bind buffer.
            Token::Placeholder(n) => Ok(Expr::Placeholder(n)),
            Token::LParen => {
                // v4.10: `(SELECT ...)` in expression position is a
                // scalar subquery; otherwise it's a parenthesised
                // expression. Peek for SELECT keyword to dispatch.
                if matches!(self.peek(), Token::Select) {
                    let inner = self.parse_select_stmt()?;
                    match self.advance() {
                        Token::RParen => {
                            let Statement::Select(s) = inner else {
                                unreachable!("parse_select_stmt returns Select")
                            };
                            Ok(Expr::ScalarSubquery(Box::new(s)))
                        }
                        other => Err(ParseError {
                            message: format!("expected ')' after scalar subquery, got {other:?}"),
                            token_pos: self.pos.saturating_sub(1),
                        }),
                    }
                } else {
                    let e = self.parse_expr(0)?;
                    match self.advance() {
                        Token::RParen => Ok(e),
                        other => Err(ParseError {
                            message: format!("expected ')', got {other:?}"),
                            token_pos: self.pos.saturating_sub(1),
                        }),
                    }
                }
            }
            Token::LBracket => self.parse_vector_literal_body(),
            Token::Extract => self.parse_extract_atom(),
            Token::Interval => self.parse_interval_atom(),
            // `LEFT` is a reserved-keyword token because the
            // grammar dedicates an arm for `LEFT [OUTER] JOIN`.
            // When `left` is followed by `(` we're in expression
            // position calling the PG `left(string, n)` function;
            // rebuild the AST as a regular function call so the
            // engine's apply_function dispatch picks it up.
            Token::Left if matches!(self.peek(), Token::LParen) => {
                self.advance(); // (
                let mut args = Vec::new();
                if !matches!(self.peek(), Token::RParen) {
                    loop {
                        args.push(self.parse_expr(0)?);
                        match self.peek() {
                            Token::Comma => {
                                self.advance();
                            }
                            Token::RParen => break,
                            other => {
                                return Err(self.err(alloc::format!(
                                    "expected ',' or ')' in left() args, got {other:?}"
                                )));
                            }
                        }
                    }
                }
                self.advance(); // )
                Ok(Expr::FunctionCall {
                    name: "left".into(),
                    args,
                })
            }
            // v4.10: EXISTS / NOT EXISTS. EXISTS isn't a reserved
            // token; we match on the bare ident. NOT is a token
            // (consumed in the comparison rung), but `EXISTS (...)`
            // at the top of an expression starts here.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("exists") => {
                self.parse_exists_atom(false)
            }
            // v7.13.0 — `CASE [<operand>] WHEN <cond> THEN <val>
            // [WHEN ...] [ELSE <val>] END` (mailrs round-5 G9).
            // CASE is a bare ident; we dispatch on lowercase match.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("case") => {
                self.parse_case_atom()
            }
            // v7.37.17 (17.6 siblings) — PG typed datetime literals:
            // `DATE '2003-01-02'` / `TIMESTAMP '…'` / `TIMESTAMPTZ
            // '…'`. Lower onto the ::cast node so the existing
            // runtime text→date/timestamp paths do the parsing. The
            // string must follow immediately, else the ident stays a
            // plain column reference.
            Token::Ident(s)
                if matches!(
                    s.to_ascii_lowercase().as_str(),
                    "date" | "timestamp" | "timestamptz"
                ) && matches!(self.peek(), Token::String(_)) =>
            {
                let target = match s.to_ascii_lowercase().as_str() {
                    "date" => CastTarget::Date,
                    "timestamp" => CastTarget::Timestamp,
                    _ => CastTarget::Timestamptz,
                };
                let Token::String(lit) = self.advance() else {
                    unreachable!("peek guaranteed a string token");
                };
                Ok(Expr::Cast {
                    expr: Box::new(Expr::Literal(Literal::String(lit))),
                    target,
                })
            }
            // v7.10.10 — `ARRAY[expr, expr, …]` constructor. ARRAY
            // is not a reserved token; we match by case-insensitive
            // ident. The opening `[` must follow immediately.
            Token::Ident(s) | Token::QuotedIdent(s)
                if s.eq_ignore_ascii_case("array") && matches!(self.peek(), Token::LBracket) =>
            {
                self.advance(); // consume `[`
                let mut items: Vec<Expr> = Vec::new();
                if !matches!(self.peek(), Token::RBracket) {
                    loop {
                        items.push(self.parse_expr(0)?);
                        match self.peek() {
                            Token::Comma => {
                                self.advance();
                            }
                            Token::RBracket => break,
                            other => {
                                return Err(self.err(alloc::format!(
                                    "expected ',' or ']' in ARRAY literal, got {other:?}"
                                )));
                            }
                        }
                    }
                }
                self.advance(); // consume `]`
                Ok(Expr::Array(items))
            }
            // v7.17.0 Phase 2.2 — MySQL `MATCH(col, ...) AGAINST
            // ('term' [IN BOOLEAN MODE | IN NATURAL LANGUAGE MODE])`.
            // We special-case before the generic ident dispatch so
            // the AGAINST clause never reaches the function-call
            // loop (which would mis-read `(cols) AGAINST` as a
            // call with no trailing modifier). The shape is
            // rewritten to a Boolean OR over per-column
            // `to_tsvector('simple', col) @@ plainto_tsquery('simple',
            // term)` so the existing FTS evaluator handles
            // semantics — the fulltext-GIN built at CREATE TABLE
            // time is currently a "real index that survives dump
            // round-trip"; the planner hook that actually uses
            // it for posting-list intersection lands in a later
            // sub-phase (Phase 2.2b) without touching this surface.
            Token::Ident(s) | Token::QuotedIdent(s)
                if s.eq_ignore_ascii_case("match") && matches!(self.peek(), Token::LParen) =>
            {
                self.parse_match_against_atom()
            }
            Token::Ident(s) | Token::QuotedIdent(s) => self.finish_ident_atom(s),
            // v7.37.43-T4 — PG-unreserved keywords are legal column /
            // alias names in expression context too. `release` appears
            // in sentori `0003_partition_events.sql` as both a column
            // reference (SELECT … release …) and an INSERT column list
            // entry. Mirrors `expect_ident_like`'s expansion of the
            // identifier set.
            other if unreserved_keyword_text(&other).is_some() => {
                let s = unreserved_keyword_text(&other).unwrap();
                self.finish_ident_atom(s)
            }
            other => Err(ParseError {
                message: format!("unexpected token {other:?} in expression"),
                token_pos: tok_pos,
            }),
        }
        // After parsing the atom, fold any postfix `::vector` casts.
        .and_then(|atom| self.finish_postfix_casts(atom))
    }

    /// Postfix operators on an atom: `::TYPE` cast and `IS [NOT] NULL`.
    /// Both bind tighter than any binary op.
    /// Shared cast-target parser for postfix `::TYPE` and the
    /// standard `CAST(expr AS TYPE)` form (v7.25, round-17).
    fn parse_cast_target(&mut self) -> Result<CastTarget, ParseError> {
        let target = match self.advance() {
            Token::Ident(s) => match s.to_ascii_lowercase().as_str() {
                "int" | "integer" | "int4" => {
                    if matches!(self.peek(), Token::LBracket)
                        && matches!(self.tokens.get(self.pos + 1), Some(Token::RBracket))
                    {
                        self.advance();
                        self.advance();
                        CastTarget::IntArray
                    } else {
                        CastTarget::Int
                    }
                }
                "bigint" | "int8" => {
                    if matches!(self.peek(), Token::LBracket)
                        && matches!(self.tokens.get(self.pos + 1), Some(Token::RBracket))
                    {
                        self.advance();
                        self.advance();
                        CastTarget::BigIntArray
                    } else {
                        CastTarget::BigInt
                    }
                }
                "float" | "double" | "real" => CastTarget::Float,
                "text" => {
                    // v7.10.11 — `::TEXT[]` widens to TextArray.
                    if matches!(self.peek(), Token::LBracket)
                        && matches!(self.tokens.get(self.pos + 1), Some(Token::RBracket))
                    {
                        self.advance();
                        self.advance();
                        CastTarget::TextArray
                    } else {
                        CastTarget::Text
                    }
                }
                "bool" | "boolean" => CastTarget::Bool,
                "vector" => CastTarget::Vector,
                "date" => CastTarget::Date,
                "timestamp" | "datetime" => CastTarget::Timestamp,
                "timestamptz" => CastTarget::Timestamptz,
                "interval" => CastTarget::Interval,
                "json" => CastTarget::Json,
                "jsonb" => CastTarget::Jsonb,
                "regtype" => CastTarget::RegType,
                "regclass" => CastTarget::RegClass,
                // v7.12.0 — `::tsvector` / `::tsquery`.
                // Engine decodes the LHS text via the PG
                // external form parser.
                "tsvector" => CastTarget::TsVector,
                "tsquery" => CastTarget::TsQuery,
                // v7.17.0 — `::uuid`. Engine decodes the LHS
                // text via `spg_storage::parse_uuid_str`.
                "uuid" => CastTarget::Uuid,
                // v7.18 — `::bytea`. Engine decodes the LHS
                // text via the PG hex form (`'\xdeadbeef'`)
                // or escape form (`'\\x05\\x00'`). Closes
                // mailrs D-pre #3 reverse-acceptance gap.
                "bytea" => CastTarget::Bytea,
                // v7.37.5 ship triage — generic typed-cast escape.
                // Anything the long-tail PG type ident table knows
                // about(network/bit/geometry/multirange/etc.)flows
                // through `CastTarget::Named(canonical)`; the engine
                // resolves via `column_type_to_data_type` and dispatches
                // through the typed `coerce_value` path. Truly
                // unrecognised idents still hit the error arm below
                // because the engine rejects them.
                other => {
                    // Optional `(N[, M])` precision args — `::numeric(10,2)`,
                    // `::varchar(255)`, etc. Capture into the canonical
                    // `name(p,s)` form so `type_name_to_data_type` can
                    // reconstruct the `DataType::Numeric { precision,
                    // scale }` (and similar param-carrying types).
                    let mut name = other.to_string();
                    if matches!(self.peek(), Token::LParen) {
                        let mut buf = alloc::string::String::from("(");
                        let mut depth = 0usize;
                        loop {
                            match self.advance() {
                                Token::LParen => {
                                    depth += 1;
                                    if depth > 1 {
                                        buf.push('(');
                                    }
                                }
                                Token::RParen => {
                                    depth -= 1;
                                    if depth == 0 {
                                        buf.push(')');
                                        break;
                                    }
                                    buf.push(')');
                                }
                                Token::Comma => buf.push(','),
                                Token::Integer(n) => buf.push_str(&alloc::format!("{n}")),
                                Token::Eof => break,
                                _ => {}
                            }
                        }
                        name.push_str(&buf);
                    }
                    // Optional postfix `[]` widens to the array form —
                    // `::BOOL[]`, `::NUMERIC[]`, `::SMALLINT[]`, etc.
                    // The engine's `type_name_to_data_type` recognises
                    // the canonical `<ty>_array` form.
                    if matches!(self.peek(), Token::LBracket)
                        && matches!(self.tokens.get(self.pos + 1), Some(Token::RBracket))
                    {
                        self.advance();
                        self.advance();
                        name.push_str("_array");
                    }
                    CastTarget::Named(name)
                }
            },
            Token::Interval => CastTarget::Interval,
            other => {
                return Err(ParseError {
                    message: format!("expected type ident after `::`, got {other:?}"),
                    token_pos: self.pos.saturating_sub(1),
                });
            }
        };
        // v7.37.5 ship triage — postfix `[]` widens a scalar cast
        // target to its array sibling. Closed-enum arms (Bool /
        // SmallInt / Numeric / Float / Date / …) didn't carry the
        // explicit widening that Text / Int / BigInt did, so
        // `::BOOL[]` / `::NUMERIC[]` etc. surfaced as a parse
        // error. The widening here mirrors the per-arm Text /
        // Int / BigInt logic above + folds the new ζ-A first-class
        // types through `CastTarget::Named("<ty>_array")`.
        if matches!(self.peek(), Token::LBracket)
            && matches!(self.tokens.get(self.pos + 1), Some(Token::RBracket))
        {
            let widened = match &target {
                CastTarget::Bool => Some(CastTarget::Named("bool_array".to_string())),
                CastTarget::Date => Some(CastTarget::Named("date_array".to_string())),
                CastTarget::Timestamp | CastTarget::Timestamptz => {
                    Some(CastTarget::Named("timestamptz_array".to_string()))
                }
                CastTarget::Uuid => Some(CastTarget::Named("uuid_array".to_string())),
                CastTarget::Json | CastTarget::Jsonb => {
                    Some(CastTarget::Named("jsonb_array".to_string()))
                }
                CastTarget::Bytea => Some(CastTarget::Named("bytea_array".to_string())),
                CastTarget::Interval => Some(CastTarget::Named("interval_array".to_string())),
                CastTarget::Float => Some(CastTarget::Named("float_array".to_string())),
                CastTarget::Named(name) => {
                    let mut a = name.clone();
                    a.push_str("_array");
                    Some(CastTarget::Named(a))
                }
                // Int / BigInt / Text / Vector / TsVector / TsQuery /
                // RegType / RegClass / TextArray / IntArray /
                // BigIntArray already finalised — leave as is.
                _ => None,
            };
            if let Some(w) = widened {
                self.advance();
                self.advance();
                return Ok(w);
            }
        }
        Ok(target)
    }

    fn finish_postfix_casts(&mut self, mut expr: Expr) -> Result<Expr, ParseError> {
        loop {
            if matches!(self.peek(), Token::DoubleColon) {
                self.advance();
                // v7.9.25 / v7.9.26 — broaden the postfix `::` cast
                // target set to include INTERVAL (reserved Token),
                // TIMESTAMPTZ, and PG catalog regtype / regclass.
                // mailrs follow-up H3a + H3b.
                let target = self.parse_cast_target()?;
                expr = Expr::Cast {
                    expr: Box::new(expr),
                    target,
                };
                continue;
            }
            if matches!(self.peek(), Token::Is) {
                self.advance();
                let negated = if matches!(self.peek(), Token::Not) {
                    self.advance();
                    true
                } else {
                    false
                };
                // v7.9.27b — `IS [NOT] DISTINCT FROM <rhs>`.
                // mailrs pg_dump.
                if matches!(self.peek(), Token::Distinct) {
                    self.advance();
                    if !matches!(self.peek(), Token::From) {
                        return Err(self.err(format!(
                            "expected FROM after IS{} DISTINCT, got {:?}",
                            if negated { " NOT" } else { "" },
                            self.peek()
                        )));
                    }
                    self.advance();
                    // Right-hand side: parse at the same precedence
                    // tier as comparison so `x IS DISTINCT FROM a + b`
                    // groups as `x IS DISTINCT FROM (a + b)`.
                    let rhs = self.parse_expr(20)?;
                    let op = if negated {
                        BinOp::IsNotDistinctFrom
                    } else {
                        BinOp::IsDistinctFrom
                    };
                    expr = Expr::Binary {
                        op,
                        lhs: Box::new(expr),
                        rhs: Box::new(rhs),
                    };
                    continue;
                }
                // v7.37.17 (17.6 siblings) — SQL:2016 / PG 16
                // `IS [NOT] JSON [VALUE|OBJECT|ARRAY|SCALAR]`.
                // Lowers onto pg_is_json(x, kind); NOT wraps the
                // call in a logical negation.
                if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                    if s.eq_ignore_ascii_case("json"))
                {
                    self.advance(); // JSON
                    let kind = match self.peek() {
                        Token::Ident(s) | Token::QuotedIdent(s)
                            if matches!(
                                s.to_ascii_lowercase().as_str(),
                                "value" | "object" | "array" | "scalar"
                            ) =>
                        {
                            let k = s.to_ascii_lowercase();
                            self.advance();
                            k
                        }
                        _ => "value".to_string(),
                    };
                    let call = Expr::FunctionCall {
                        name: "pg_is_json".to_string(),
                        args: alloc::vec![
                            expr,
                            Expr::Literal(Literal::String(kind)),
                        ],
                    };
                    expr = if negated {
                        Expr::Unary {
                            op: UnOp::Not,
                            expr: Box::new(call),
                        }
                    } else {
                        call
                    };
                    continue;
                }
                if !matches!(self.peek(), Token::Null) {
                    return Err(self.err(format!(
                        "expected NULL, DISTINCT or JSON after IS{}, got {:?}",
                        if negated { " NOT" } else { "" },
                        self.peek()
                    )));
                }
                self.advance();
                expr = Expr::IsNull {
                    expr: Box::new(expr),
                    negated,
                };
                continue;
            }
            // `x [NOT] BETWEEN a AND b`, `x [NOT] IN (...)`, `x [NOT] LIKE p`.
            // Look one token ahead so a stray `NOT` not followed by any of
            // these flows through to the early return below untouched.
            let negated = if matches!(self.peek(), Token::Not) {
                let next = self.tokens.get(self.pos + 1);
                matches!(next, Some(Token::Between | Token::In | Token::Like))
                    || matches!(next, Some(Token::Ident(s)) if s.eq_ignore_ascii_case("ilike"))
            } else {
                false
            };
            if negated {
                self.advance();
            }
            if matches!(self.peek(), Token::Between) {
                expr = self.parse_between_tail(expr, negated)?;
                continue;
            }
            if matches!(self.peek(), Token::In) {
                expr = self.parse_in_tail(expr, negated)?;
                continue;
            }
            if matches!(self.peek(), Token::Like) {
                self.advance();
                // Pattern at the same precedence as other comparison RHSes —
                // 5 leaves AND/OR alone so `a LIKE 'x%' AND b` parses right.
                let pattern = self.parse_expr(5)?;
                expr = Expr::Like {
                    expr: Box::new(expr),
                    pattern: Box::new(pattern),
                    negated,
                    case_insensitive: false,
                };
                continue;
            }
            // v7.25 (round-17) — ILIKE: case-insensitive LIKE. The
            // keyword reaches us as a plain identifier.
            if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("ilike")) {
                self.advance();
                let pattern = self.parse_expr(5)?;
                expr = Expr::Like {
                    expr: Box::new(expr),
                    pattern: Box::new(pattern),
                    negated,
                    case_insensitive: true,
                };
                continue;
            }
            // v7.10.12 — `arr[i]` subscript. PG 1-based; engine
            // returns NULL for out-of-range. Multiple subscripts
            // chain: `a[i][j]` parses left-to-right.
            if matches!(self.peek(), Token::LBracket) {
                self.advance();
                let index = self.parse_expr(0)?;
                if !matches!(self.peek(), Token::RBracket) {
                    return Err(self.err(alloc::format!(
                        "expected ']' after array index, got {:?}",
                        self.peek()
                    )));
                }
                self.advance();
                expr = Expr::ArraySubscript {
                    target: Box::new(expr),
                    index: Box::new(index),
                };
                continue;
            }
            return Ok(expr);
        }
    }

    /// `x BETWEEN low AND high`  →  `(x >= low) AND (x <= high)`, wrapped in
    /// `NOT` when `negated`. Bounds parse at precedence 5 so the trailing
    /// `AND` is not swallowed.
    fn parse_between_tail(&mut self, expr: Expr, negated: bool) -> Result<Expr, ParseError> {
        self.advance(); // BETWEEN
        let low = self.parse_expr(5)?;
        if !matches!(self.peek(), Token::And) {
            return Err(self.err(format!(
                "expected AND after BETWEEN low bound, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let high = self.parse_expr(5)?;
        let target = Box::new(expr);
        let combined = Expr::Binary {
            lhs: Box::new(Expr::Binary {
                lhs: target.clone(),
                op: BinOp::GtEq,
                rhs: Box::new(low),
            }),
            op: BinOp::And,
            rhs: Box::new(Expr::Binary {
                lhs: target,
                op: BinOp::LtEq,
                rhs: Box::new(high),
            }),
        };
        Ok(maybe_not(combined, negated))
    }

    /// `x IN (a, b, c)`  →  chained OR of equalities. Empty list collapses
    /// to FALSE (TRUE under NOT IN), matching standard SQL semantics.
    /// v4.11: parse `WITH name AS (SELECT ...) [, ...] SELECT ...`.
    /// Caller already consumed the leading `WITH` ident.
    fn parse_with_cte_then_select(&mut self) -> Result<Statement, ParseError> {
        // v4.22: WITH RECURSIVE — optional keyword right after WITH.
        // Comes through as an identifier; consume it if present and
        // mark every CTE in the clause as recursive (PG semantics —
        // the flag is per-WITH, not per-CTE).
        let mut recursive = false;
        if let Token::Ident(s) | Token::QuotedIdent(s) = self.peek()
            && s.eq_ignore_ascii_case("recursive")
        {
            self.advance();
            recursive = true;
        }
        let mut ctes = Vec::new();
        loop {
            let name = self.expect_ident_like()?;
            // v4.22: optional column-name list — `WITH t(a,b,c) AS ...`.
            // PG uses these to rename the body's output columns; we
            // do the same below by overriding `columns[i].name`.
            let column_overrides: Vec<String> = if matches!(self.peek(), Token::LParen) {
                self.advance();
                let mut names = Vec::new();
                loop {
                    names.push(self.expect_ident_like()?);
                    if matches!(self.peek(), Token::Comma) {
                        self.advance();
                        continue;
                    }
                    break;
                }
                if !matches!(self.peek(), Token::RParen) {
                    return Err(self.err(format!(
                        "expected ')' to close CTE column list, got {:?}",
                        self.peek()
                    )));
                }
                self.advance();
                names
            } else {
                Vec::new()
            };
            // AS is a reserved Token::As (used by SELECT-item / FROM
            // aliasing) — handle it specially rather than as a bare
            // ident.
            if !matches!(self.peek(), Token::As) {
                return Err(self.err(format!(
                    "expected AS after CTE name {name:?}, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            // v7.37.17 (17.6 siblings) — PG 12+ `AS [NOT]
            // MATERIALIZED` optimizer hints. SPG materialises every
            // CTE, so both spellings are accepted and absorbed.
            if matches!(self.peek(), Token::Not) {
                self.advance(); // NOT
                if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                    if s.eq_ignore_ascii_case("materialized"))
                {
                    self.advance();
                } else {
                    return Err(self.err(format!(
                        "expected MATERIALIZED after AS NOT, got {:?}",
                        self.peek()
                    )));
                }
            } else if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s)
                if s.eq_ignore_ascii_case("materialized"))
            {
                self.advance();
            }
            if !matches!(self.peek(), Token::LParen) {
                return Err(self.err(format!(
                    "expected '(' after AS in WITH clause, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            // v7.37.43-T4.4 — accept INSERT / UPDATE / DELETE (with
            // RETURNING) as the CTE body in addition to SELECT.
            // PG writable CTE semantics. UPDATE / DELETE come in as
            // bare Idents (lexer keeps SELECT / INSERT as reserved
            // tokens but treats the rest of DML as case-insensitive
            // idents).
            let is_update_kw = matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("update"));
            let is_delete_kw = matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("delete"));
            let body = match self.peek() {
                Token::Select => {
                    let inner = self.parse_select_stmt()?;
                    let Statement::Select(s) = inner else {
                        unreachable!("parse_select_stmt returns Select");
                    };
                    crate::ast::CteBody::Select(s)
                }
                // v7.37.17 (17.6 siblings) — VALUES as a CTE body:
                // WITH t(a) AS (VALUES (1), (2)) … lowers through
                // the shared rows helper onto a Select body.
                Token::Values => {
                    self.advance(); // VALUES
                    let head = self.parse_values_rows_body()?;
                    crate::ast::CteBody::Select(head)
                }
                Token::Insert => {
                    let inner = self.parse_one_statement()?;
                    let Statement::Insert(s) = inner else {
                        unreachable!("Token::Insert routes to Insert");
                    };
                    crate::ast::CteBody::Insert(alloc::boxed::Box::new(s))
                }
                _ if is_update_kw => {
                    let inner = self.parse_one_statement()?;
                    let Statement::Update(s) = inner else {
                        return Err(
                            self.err(format!("expected UPDATE inside WITH (…), got {inner:?}"))
                        );
                    };
                    crate::ast::CteBody::Update(alloc::boxed::Box::new(s))
                }
                _ if is_delete_kw => {
                    let inner = self.parse_one_statement()?;
                    let Statement::Delete(s) = inner else {
                        return Err(
                            self.err(format!("expected DELETE inside WITH (…), got {inner:?}"))
                        );
                    };
                    crate::ast::CteBody::Delete(alloc::boxed::Box::new(s))
                }
                other => {
                    return Err(self.err(format!(
                        "WITH body must be SELECT / INSERT / UPDATE / DELETE, got {other:?}"
                    )));
                }
            };
            if !matches!(self.peek(), Token::RParen) {
                return Err(self.err(format!(
                    "expected ')' after CTE body, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            ctes.push(crate::ast::Cte {
                name,
                body,
                recursive,
                column_overrides,
            });
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        // v7.37.43-T4.4 — the outer body may be SELECT (classical),
        // or INSERT / UPDATE / DELETE (writable CTE outer). Attach
        // the parsed CTEs to whichever statement the body produces.
        let outer_is_update = matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("update"));
        let outer_is_delete = matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("delete"));
        match self.peek() {
            Token::Select => {
                let body_stmt = self.parse_select_stmt()?;
                let Statement::Select(mut body) = body_stmt else {
                    unreachable!()
                };
                body.ctes = ctes;
                Ok(Statement::Select(body))
            }
            Token::Insert => {
                let body_stmt = self.parse_one_statement()?;
                let Statement::Insert(mut body) = body_stmt else {
                    unreachable!()
                };
                body.ctes = ctes;
                Ok(Statement::Insert(body))
            }
            _ if outer_is_update => {
                let body_stmt = self.parse_one_statement()?;
                let Statement::Update(mut body) = body_stmt else {
                    return Err(self.err(format!("expected UPDATE after WITH clause")));
                };
                body.ctes = ctes;
                Ok(Statement::Update(body))
            }
            _ if outer_is_delete => {
                let body_stmt = self.parse_one_statement()?;
                let Statement::Delete(mut body) = body_stmt else {
                    return Err(self.err(format!("expected DELETE after WITH clause")));
                };
                body.ctes = ctes;
                Ok(Statement::Delete(body))
            }
            other => Err(self.err(format!(
                "expected SELECT / INSERT / UPDATE / DELETE after WITH clause, got {other:?}"
            ))),
        }
    }

    /// v4.10: parse `EXISTS (SELECT ...)`. Caller (`parse_atom`)
    /// already consumed the leading `EXISTS` ident via
    /// `self.advance()`.
    /// v7.13.0 — parse the rest of a `CASE … END` expression after
    /// the leading `CASE` ident has been consumed (mailrs round-5
    /// G9). Supports both the searched form
    /// (`CASE WHEN cond THEN val …`) and the simple form
    /// (`CASE operand WHEN val THEN val …`).
    fn parse_case_atom(&mut self) -> Result<Expr, ParseError> {
        // Disambiguate searched vs simple form: if the next token
        // is `WHEN`, we're in the searched form. Otherwise the
        // intervening expression is the operand.
        let operand = if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("when")) {
            None
        } else {
            Some(Box::new(self.parse_expr(0)?))
        };
        let mut branches: Vec<(Expr, Expr)> = Vec::new();
        loop {
            match self.peek() {
                Token::Ident(s) if s.eq_ignore_ascii_case("when") => {
                    self.advance();
                    let cond = self.parse_expr(0)?;
                    match self.peek() {
                        Token::Ident(t) if t.eq_ignore_ascii_case("then") => {
                            self.advance();
                        }
                        other => {
                            return Err(self.err(alloc::format!(
                                "expected THEN after CASE WHEN <expr>, got {other:?}"
                            )));
                        }
                    }
                    let value = self.parse_expr(0)?;
                    branches.push((cond, value));
                }
                _ => break,
            }
        }
        if branches.is_empty() {
            return Err(self.err("CASE requires at least one WHEN … THEN … branch".into()));
        }
        let else_branch = if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("else"))
        {
            self.advance();
            Some(Box::new(self.parse_expr(0)?))
        } else {
            None
        };
        match self.peek() {
            Token::Ident(s) if s.eq_ignore_ascii_case("end") => {
                self.advance();
            }
            other => {
                return Err(self.err(alloc::format!(
                    "expected END to close CASE expression, got {other:?}"
                )));
            }
        }
        Ok(Expr::Case {
            operand,
            branches,
            else_branch,
        })
    }

    fn parse_exists_atom(&mut self, negated: bool) -> Result<Expr, ParseError> {
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(format!("expected '(' after EXISTS, got {:?}", self.peek())));
        }
        self.advance();
        let inner = self.parse_select_stmt()?;
        if !matches!(self.peek(), Token::RParen) {
            return Err(self.err(format!(
                "expected ')' after EXISTS-subquery, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let Statement::Select(s) = inner else {
            unreachable!("parse_select_stmt returns Select")
        };
        Ok(Expr::Exists {
            subquery: Box::new(s),
            negated,
        })
    }

    fn parse_in_tail(&mut self, expr: Expr, negated: bool) -> Result<Expr, ParseError> {
        self.advance(); // IN
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(format!("expected '(' after IN, got {:?}", self.peek())));
        }
        self.advance();
        // v4.10: `IN (SELECT ...)` — subquery branch.
        if matches!(self.peek(), Token::Select) {
            let inner = self.parse_select_stmt()?;
            if !matches!(self.peek(), Token::RParen) {
                return Err(self.err(format!(
                    "expected ')' after IN-subquery, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            let Statement::Select(s) = inner else {
                unreachable!("parse_select_stmt always returns Statement::Select")
            };
            return Ok(Expr::InSubquery {
                expr: Box::new(expr),
                subquery: Box::new(s),
                negated,
            });
        }
        let mut elements = Vec::new();
        if !matches!(self.peek(), Token::RParen) {
            loop {
                elements.push(self.parse_expr(0)?);
                match self.peek() {
                    Token::Comma => {
                        self.advance();
                    }
                    Token::RParen => break,
                    other => {
                        return Err(
                            self.err(format!("expected ',' or ')' in IN list, got {other:?}"))
                        );
                    }
                }
            }
        }
        self.advance(); // ')'
        // v7.30.2 (mailrs round-25) — flat InList node instead of a
        // left-deep OR-Eq chain: chain depth scaled with the element
        // count and overflowed the stack (eval + drop are recursive).
        if elements.is_empty() {
            return Ok(maybe_not(Expr::Literal(Literal::Bool(false)), negated));
        }
        Ok(Expr::InList {
            expr: Box::new(expr),
            list: elements,
            negated,
        })
    }

    /// Parse a pgvector array literal `[ x1, x2, ... ]`. The opening `[` is
    /// already consumed by the caller. Elements must be numeric literals
    /// (with optional unary `-`); any compound expression is rejected at
    /// parse time so the runtime never needs to evaluate inside a vector.
    /// `EXTRACT(<field> FROM <source>)`. The dispatching `parse_atom`
    /// has already consumed the `EXTRACT` token before calling us —
    /// we pick up at the opening `(`.
    /// v7.17.0 Phase 2.2 — MySQL `MATCH(col [, col ...]) AGAINST
    /// (expr [IN BOOLEAN MODE | IN NATURAL LANGUAGE MODE
    /// [WITH QUERY EXPANSION]])`. Rewritten in-place to a
    /// per-column OR-fold of
    /// `to_tsvector('simple', col) @@ plainto_tsquery('simple',
    /// term)` so the existing FTS evaluator handles semantics.
    ///
    /// The mode modifier is accepted-and-ignored at v7.17 — all
    /// modes map to the same `plainto_tsquery` rewrite. Boolean-
    /// mode operators (`+foo -bar`) would need their own parser
    /// (Phase 2.2c); customers who hit them today already get a
    /// correct lexeme-match against the bare term, only without
    /// the +/- precedence the customer asked for.
    fn parse_match_against_atom(&mut self) -> Result<Expr, ParseError> {
        // Already at `MATCH`-consumed position; the dispatcher
        // confirmed the next token is `(`.
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(alloc::format!(
                "expected '(' after MATCH, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let mut cols: Vec<Expr> = Vec::new();
        loop {
            cols.push(self.parse_expr(0)?);
            match self.peek() {
                Token::Comma => {
                    self.advance();
                }
                Token::RParen => break,
                other => {
                    return Err(self.err(alloc::format!(
                        "expected ',' or ')' in MATCH column list, got {other:?}"
                    )));
                }
            }
        }
        self.advance(); // ')'
        // Expect AGAINST.
        match self.peek() {
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("against") => {
                self.advance();
            }
            other => {
                return Err(self.err(alloc::format!(
                    "expected AGAINST after MATCH column list, got {other:?}"
                )));
            }
        }
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(alloc::format!(
                "expected '(' after AGAINST, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        // Read AGAINST's argument as a single primary token —
        // string literal, placeholder, or column-ref ident. We
        // can't call `parse_expr` / `parse_unary` here because
        // the postfix chain inside `parse_atom` would greedily
        // fold a trailing `IN BOOLEAN MODE` as `expr IN (...)`
        // and fail at "expected '(' after IN". Customers always
        // write a literal or bound parameter in AGAINST, so this
        // restriction is non-blocking; the error path explains
        // the limit if a more complex expression shows up.
        let term = match self.advance() {
            Token::String(s) => Expr::Literal(crate::ast::Literal::String(s)),
            Token::Placeholder(n) => Expr::Placeholder(n),
            Token::Ident(s) | Token::QuotedIdent(s) => Expr::Column(crate::ast::ColumnName {
                qualifier: None,
                name: s,
            }),
            other => {
                return Err(self.err(alloc::format!(
                    "MATCH ... AGAINST(<term>) expects a string literal, \
                     bound parameter, or column ref, got {other:?}"
                )));
            }
        };
        // Optional mode tail — accept-and-ignore at v7.17:
        //   IN NATURAL LANGUAGE MODE [WITH QUERY EXPANSION]
        //   IN BOOLEAN MODE
        //   WITH QUERY EXPANSION
        loop {
            match self.peek() {
                // IN lexes as a reserved Token::In, not an ident,
                // so it gets its own arm.
                Token::In => {
                    self.advance();
                }
                Token::Ident(s) | Token::QuotedIdent(s)
                    if s.eq_ignore_ascii_case("natural")
                        || s.eq_ignore_ascii_case("language")
                        || s.eq_ignore_ascii_case("boolean")
                        || s.eq_ignore_ascii_case("mode")
                        || s.eq_ignore_ascii_case("with")
                        || s.eq_ignore_ascii_case("query")
                        || s.eq_ignore_ascii_case("expansion") =>
                {
                    self.advance();
                }
                _ => break,
            }
        }
        if !matches!(self.peek(), Token::RParen) {
            return Err(self.err(alloc::format!(
                "expected ')' to close AGAINST, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        // Build per-column `to_tsvector('simple', col) @@
        // plainto_tsquery('simple', term)` and OR-fold.
        let simple_lit = || Expr::Literal(crate::ast::Literal::String(String::from("simple")));
        let plainto = Expr::FunctionCall {
            name: String::from("plainto_tsquery"),
            args: alloc::vec![simple_lit(), term.clone()],
        };
        let mut folded: Option<Expr> = None;
        for col in cols {
            let to_tsv = Expr::FunctionCall {
                name: String::from("to_tsvector"),
                args: alloc::vec![simple_lit(), col],
            };
            let leaf = Expr::Binary {
                lhs: Box::new(to_tsv),
                op: crate::ast::BinOp::TsMatch,
                rhs: Box::new(plainto.clone()),
            };
            folded = Some(match folded {
                None => leaf,
                Some(prev) => Expr::Binary {
                    lhs: Box::new(prev),
                    op: crate::ast::BinOp::Or,
                    rhs: Box::new(leaf),
                },
            });
        }
        match folded {
            Some(e) => Ok(e),
            None => Err(self.err(String::from(
                "MATCH(...) AGAINST(...) requires at least one column",
            ))),
        }
    }

    fn parse_extract_atom(&mut self) -> Result<Expr, ParseError> {
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(format!("expected '(' after EXTRACT, got {:?}", self.peek())));
        }
        self.advance();
        let field_name = self.expect_ident_like()?;
        let field = match field_name.to_ascii_lowercase().as_str() {
            "year" => ExtractField::Year,
            "month" => ExtractField::Month,
            "day" => ExtractField::Day,
            "hour" => ExtractField::Hour,
            "minute" => ExtractField::Minute,
            "second" => ExtractField::Second,
            "microsecond" | "microseconds" => ExtractField::Microsecond,
            "epoch" => ExtractField::Epoch,
            other => {
                return Err(self.err(format!(
                    "unknown EXTRACT field {other:?}; \
                     supported: YEAR, MONTH, DAY, HOUR, MINUTE, SECOND, MICROSECOND, EPOCH"
                )));
            }
        };
        if !matches!(self.peek(), Token::From) {
            return Err(self.err(format!(
                "expected FROM after EXTRACT field, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let source = self.parse_expr(0)?;
        if !matches!(self.peek(), Token::RParen) {
            return Err(self.err(format!(
                "expected ')' to close EXTRACT, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        Ok(Expr::Extract {
            field,
            source: Box::new(source),
        })
    }

    /// `INTERVAL '<n> <unit> [<n> <unit> ...]'` — the `INTERVAL` keyword
    /// is already consumed; we expect a single string literal next and
    /// resolve it into `Literal::Interval` at parse time so the engine
    /// never has to re-tokenise inside the string.
    fn parse_interval_atom(&mut self) -> Result<Expr, ParseError> {
        let tok = self.advance();
        let Token::String(text) = tok else {
            return Err(self.err(format!(
                "expected string literal after INTERVAL, got {tok:?}"
            )));
        };
        let (months, days, micros) = parse_interval_text(&text).ok_or_else(|| ParseError {
            message: format!(
                "cannot parse INTERVAL {text:?}; \
                     expected `<n> <unit> [<n> <unit> ...]` with units \
                     microsecond[s], millisecond[s], second[s], minute[s], \
                     hour[s], day[s], week[s], month[s], year[s]"
            ),
            token_pos: self.pos.saturating_sub(1),
        })?;
        Ok(Expr::Literal(Literal::Interval {
            months,
            days,
            micros,
            text,
        }))
    }

    fn parse_vector_literal_body(&mut self) -> Result<Expr, ParseError> {
        let mut elems = Vec::new();
        if matches!(self.peek(), Token::RBracket) {
            self.advance();
            return Ok(Expr::Literal(Literal::Vector(elems)));
        }
        loop {
            let e = self.parse_expr(0)?;
            let x = extract_numeric_literal(&e).ok_or_else(|| ParseError {
                message: format!("vector element must be a numeric literal, got {e:?}"),
                token_pos: self.pos,
            })?;
            elems.push(x);
            match self.peek() {
                Token::Comma => {
                    self.advance();
                }
                Token::RBracket => {
                    self.advance();
                    break;
                }
                other => {
                    return Err(self.err(format!("expected ',' or ']' in vector, got {other:?}")));
                }
            }
        }
        Ok(Expr::Literal(Literal::Vector(elems)))
    }

    /// Atom that started with an identifier: could be `t.col`, `col`, or
    /// `func(arg, ...)`. Detect each shape by looking at the next token.
    /// v4.12: parse `(PARTITION BY expr, ... ORDER BY expr [DESC]
    /// [, ...])`. Caller has already consumed `OVER`. Either clause
    /// is optional; an empty `()` is also legal (PG semantics).
    /// v6.4.2 — consume an optional `IGNORE NULLS` / `RESPECT NULLS`
    /// modifier between `name(args)` and `OVER (...)`. Default is
    /// `Respect`. Unrecognised idents leave the stream unchanged.
    fn parse_null_treatment_modifier(&mut self) -> NullTreatment {
        let Token::Ident(s) = self.peek().clone() else {
            return NullTreatment::Respect;
        };
        let is_ignore = s.eq_ignore_ascii_case("ignore");
        let is_respect = s.eq_ignore_ascii_case("respect");
        if !is_ignore && !is_respect {
            return NullTreatment::Respect;
        }
        // Lookahead for NULLS — only consume both tokens together.
        // pos+1 must hold a "nulls" ident.
        if self.pos + 1 < self.tokens.len()
            && let Token::Ident(s2) = &self.tokens[self.pos + 1]
            && s2.eq_ignore_ascii_case("nulls")
        {
            self.advance();
            self.advance();
            return if is_ignore {
                NullTreatment::Ignore
            } else {
                NullTreatment::Respect
            };
        }
        NullTreatment::Respect
    }

    /// v7.32 (mailrs round-29) — `agg(args) FILTER (WHERE cond)`.
    /// `FILTER` is an unreserved keyword, so it arrives as an `Ident`
    /// (same shape as the `OVER` tail). Consumes the whole clause and
    /// returns the predicate; returns `None` when no `FILTER` follows.
    fn parse_filter_clause(&mut self) -> Result<Option<Box<Expr>>, ParseError> {
        let (Token::Ident(s) | Token::QuotedIdent(s)) = self.peek() else {
            return Ok(None);
        };
        if !s.eq_ignore_ascii_case("filter") {
            return Ok(None);
        }
        self.advance(); // FILTER
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(format!("expected '(' after FILTER, got {:?}", self.peek())));
        }
        self.advance(); // (
        if !matches!(self.peek(), Token::Where) {
            return Err(self.err(format!(
                "expected WHERE inside FILTER (...), got {:?}",
                self.peek()
            )));
        }
        self.advance(); // WHERE
        let cond = self.parse_expr(0)?;
        if !matches!(self.peek(), Token::RParen) {
            return Err(self.err(format!(
                "expected ')' to close FILTER (WHERE ...), got {:?}",
                self.peek()
            )));
        }
        self.advance(); // )
        Ok(Some(Box::new(cond)))
    }

    /// v7.32 (round-29) — `WITHIN GROUP ( ORDER BY <sort_spec> )` tail
    /// for ordered-set aggregates. `WITHIN` is unreserved (arrives as an
    /// `Ident`); `GROUP` and `ORDER`/`BY` are keywords. Returns the sort
    /// keys, or an empty vec when no `WITHIN GROUP` follows.
    fn parse_within_group_clause(&mut self) -> Result<Vec<OrderBy>, ParseError> {
        let (Token::Ident(s) | Token::QuotedIdent(s)) = self.peek() else {
            return Ok(Vec::new());
        };
        if !s.eq_ignore_ascii_case("within") {
            return Ok(Vec::new());
        }
        self.advance(); // WITHIN
        if !matches!(self.peek(), Token::Group) {
            return Err(self.err(format!(
                "expected GROUP after WITHIN, got {:?}",
                self.peek()
            )));
        }
        self.advance(); // GROUP
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(format!(
                "expected '(' after WITHIN GROUP, got {:?}",
                self.peek()
            )));
        }
        self.advance(); // (
        if !matches!(self.peek(), Token::Order) {
            return Err(self.err(format!(
                "expected ORDER BY inside WITHIN GROUP (...), got {:?}",
                self.peek()
            )));
        }
        self.advance(); // ORDER
        if !matches!(self.peek(), Token::By) {
            return Err(self.err(format!("expected BY after ORDER, got {:?}", self.peek())));
        }
        self.advance(); // BY
        let mut keys: Vec<OrderBy> = Vec::new();
        loop {
            let expr = self.parse_expr(0)?;
            let desc = if matches!(self.peek(), Token::Desc) {
                self.advance();
                true
            } else if matches!(self.peek(), Token::Asc) {
                self.advance();
                false
            } else {
                false
            };
            let nulls_first = self.parse_optional_nulls_placement()?;
            keys.push(OrderBy {
                expr,
                desc,
                nulls_first,
            });
            if matches!(self.peek(), Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        if !matches!(self.peek(), Token::RParen) {
            return Err(self.err(format!(
                "expected ')' to close WITHIN GROUP (ORDER BY ...), got {:?}",
                self.peek()
            )));
        }
        self.advance(); // )
        Ok(keys)
    }

    /// No frame clause is supported.
    #[allow(clippy::type_complexity)] // (partitions, ordered-keys-with-desc) is the natural shape
    fn parse_over_clause(
        &mut self,
    ) -> Result<
        (
            Vec<Expr>,
            Vec<(Expr, bool, Option<bool>)>,
            Option<WindowFrame>,
        ),
        ParseError,
    > {
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(format!("expected '(' after OVER, got {:?}", self.peek())));
        }
        self.advance();
        let mut partition_by = Vec::new();
        let mut order_by = Vec::new();
        // PARTITION BY ?
        // v7.37.6-B promoted PARTITION to a reserved keyword
        // (Token::Partition); pre-7.37.6-B catalogs lexed it as
        // `Token::Ident("partition")`. Accept both so older sources
        // and the new lexer surface land on the same path.
        let is_partition_kw = match self.peek() {
            Token::Partition => true,
            Token::Ident(s) | Token::QuotedIdent(s) => s.eq_ignore_ascii_case("partition"),
            _ => false,
        };
        if is_partition_kw {
            self.advance();
            if !matches!(self.peek(), Token::By) {
                return Err(self.err(format!(
                    "expected BY after PARTITION, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            loop {
                partition_by.push(self.parse_expr(0)?);
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                    continue;
                }
                break;
            }
        }
        // ORDER BY ?
        if matches!(self.peek(), Token::Order) {
            self.advance();
            if !matches!(self.peek(), Token::By) {
                return Err(self.err(format!("expected BY after ORDER, got {:?}", self.peek())));
            }
            self.advance();
            loop {
                let e = self.parse_expr(0)?;
                let desc = if matches!(self.peek(), Token::Desc) {
                    self.advance();
                    true
                } else if matches!(self.peek(), Token::Asc) {
                    self.advance();
                    false
                } else {
                    false
                };
                // v7.24.1 — NULLS FIRST/LAST inside OVER (…).
                let nulls_first = self.parse_optional_nulls_placement()?;
                order_by.push((e, desc, nulls_first));
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                    continue;
                }
                break;
            }
        }
        // v4.20: optional explicit frame, `ROWS ...` / `RANGE ...`.
        // Both keywords come through the lexer as identifiers; match
        // case-insensitively.
        let mut frame: Option<WindowFrame> = None;
        if let Token::Ident(s) | Token::QuotedIdent(s) = self.peek() {
            let kind = if s.eq_ignore_ascii_case("rows") {
                Some(FrameKind::Rows)
            } else if s.eq_ignore_ascii_case("range") {
                Some(FrameKind::Range)
            } else if s.eq_ignore_ascii_case("groups") {
                // v7.37.19 (19.11) — PG 11+ GROUPS frame mode.
                Some(FrameKind::Groups)
            } else {
                None
            };
            if let Some(kind) = kind {
                self.advance();
                frame = Some(self.parse_frame_tail(kind)?);
            }
        }
        if !matches!(self.peek(), Token::RParen) {
            return Err(self.err(format!(
                "expected ')' to close OVER clause, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        Ok((partition_by, order_by, frame))
    }

    /// v4.20: parse the tail of an explicit frame, given the `ROWS`
    /// or `RANGE` keyword was just consumed. Accepts both
    /// `BETWEEN <bound> AND <bound>` and the single-bound shorthand
    /// (`ROWS UNBOUNDED PRECEDING`, `ROWS 5 PRECEDING`, etc.) which
    /// PG normalises to `BETWEEN <bound> AND CURRENT ROW`.
    fn parse_frame_tail(&mut self, kind: FrameKind) -> Result<WindowFrame, ParseError> {
        if matches!(self.peek(), Token::Between) {
            self.advance();
            let start = self.parse_frame_bound()?;
            if !matches!(self.peek(), Token::And) {
                return Err(self.err(format!("expected AND in frame spec, got {:?}", self.peek())));
            }
            self.advance();
            let end = self.parse_frame_bound()?;
            Ok(WindowFrame {
                kind,
                start,
                end: Some(end),
            })
        } else {
            let start = self.parse_frame_bound()?;
            Ok(WindowFrame {
                kind,
                start,
                end: None,
            })
        }
    }

    /// Parse one frame bound: `UNBOUNDED PRECEDING`, `<n> PRECEDING`,
    /// `CURRENT ROW`, `<n> FOLLOWING`, `UNBOUNDED FOLLOWING`.
    fn parse_frame_bound(&mut self) -> Result<FrameBound, ParseError> {
        // Number-led: "<n> PRECEDING" / "<n> FOLLOWING".
        if let Token::Integer(n) = *self.peek() {
            self.advance();
            let n: u64 = u64::try_from(n).map_err(|_| {
                self.err(format!(
                    "invalid frame offset {n} — expected non-negative integer"
                ))
            })?;
            let dir = self.expect_ident_like()?;
            return if dir.eq_ignore_ascii_case("preceding") {
                Ok(FrameBound::OffsetPreceding(n))
            } else if dir.eq_ignore_ascii_case("following") {
                Ok(FrameBound::OffsetFollowing(n))
            } else {
                Err(self.err(format!(
                    "expected PRECEDING or FOLLOWING after offset, got {dir:?}"
                )))
            };
        }
        let first = self.expect_ident_like()?;
        if first.eq_ignore_ascii_case("unbounded") {
            let dir = self.expect_ident_like()?;
            return if dir.eq_ignore_ascii_case("preceding") {
                Ok(FrameBound::UnboundedPreceding)
            } else if dir.eq_ignore_ascii_case("following") {
                Ok(FrameBound::UnboundedFollowing)
            } else {
                Err(self.err(format!(
                    "expected PRECEDING or FOLLOWING after UNBOUNDED, got {dir:?}"
                )))
            };
        }
        if first.eq_ignore_ascii_case("current") {
            let row = self.expect_ident_like()?;
            if !row.eq_ignore_ascii_case("row") {
                return Err(self.err(format!("expected ROW after CURRENT, got {row:?}")));
            }
            return Ok(FrameBound::CurrentRow);
        }
        Err(self.err(format!(
            "expected frame bound (UNBOUNDED/CURRENT/<n>), got {first:?}"
        )))
    }

    fn finish_ident_atom(&mut self, first: String) -> Result<Expr, ParseError> {
        if matches!(self.peek(), Token::Dot) {
            self.advance();
            let name = self.expect_ident_like()?;
            // v7.14.0 — schema-qualified function call
            // `<schema>.<fn>(args)`. PG dumps emit
            // `pg_catalog.set_config(...)` in the preamble. SPG
            // is single-namespace: drop the schema prefix and
            // route the dispatch on the bare function name.
            if matches!(self.peek(), Token::LParen) {
                return self.finish_ident_atom(name);
            }
            return Ok(Expr::Column(ColumnName {
                qualifier: Some(first),
                name,
            }));
        }
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            // `COUNT(*)` — special-cased here because `*` isn't a normal
            // expression token. Lower-case match on `first` since the lexer
            // folds identifiers.
            if first.eq_ignore_ascii_case("count") && matches!(self.peek(), Token::Star) {
                self.advance();
                if !matches!(self.peek(), Token::RParen) {
                    return Err(self.err(format!(
                        "expected ')' after COUNT(*), got {:?}",
                        self.peek()
                    )));
                }
                self.advance();
                // v7.32 (round-29) — `COUNT(*) FILTER (WHERE …)`.
                let filter = self.parse_filter_clause()?;
                // v4.12: COUNT(*) OVER (...) — same window tail.
                let null_treatment = self.parse_null_treatment_modifier();
                if let Token::Ident(s) | Token::QuotedIdent(s) = self.peek()
                    && s.eq_ignore_ascii_case("over")
                {
                    if filter.is_some() {
                        return Err(
                            self.err("FILTER on window functions is not supported yet".into())
                        );
                    }
                    self.advance();
                    let (partition_by, order_by, frame) = self.parse_over_clause()?;
                    return Ok(Expr::WindowFunction {
                        name: "count_star".into(),
                        args: Vec::new(),
                        partition_by,
                        order_by,
                        frame,
                        null_treatment,
                    });
                }
                if let Some(filter) = filter {
                    return Ok(Expr::AggregateOrdered {
                        call: Box::new(Expr::FunctionCall {
                            name: "count_star".into(),
                            args: Vec::new(),
                        }),
                        order_by: Vec::new(),
                        distinct: false,
                        filter: Some(filter),
                    });
                }
                return Ok(Expr::FunctionCall {
                    name: "count_star".into(),
                    args: Vec::new(),
                });
            }
            // Function call. PG-style: zero-or-more comma-separated args.
            let mut args = Vec::new();
            let mut agg_order_by: Vec<OrderBy> = Vec::new();
            // v7.25 (round-17) — `COUNT(DISTINCT x)` and friends.
            // v7.32 (round-29) — accept the dual `ALL` quantifier too
            // (the default; ORMs emit `COUNT(ALL x)` / `SUM(ALL x)`).
            let agg_distinct = if matches!(self.peek(), Token::Distinct) {
                self.advance();
                true
            } else if matches!(self.peek(), Token::All) {
                self.advance();
                false
            } else {
                false
            };
            // v7.37.17 (17.6 siblings) — MySQL TIMESTAMPADD /
            // TIMESTAMPDIFF take a bare unit keyword as the first
            // argument (MINUTE, DAY, ...), and GET_FORMAT takes a
            // bare type keyword (DATE / TIME / DATETIME); lower them
            // onto string literals so the evaluator sees plain text.
            if ((first.eq_ignore_ascii_case("timestampadd")
                || first.eq_ignore_ascii_case("timestampdiff"))
                && matches!(self.peek(), Token::Ident(u) if matches!(
                    u.to_ascii_lowercase().as_str(),
                    "microsecond" | "second" | "minute" | "hour" | "day"
                        | "week" | "month" | "quarter" | "year"
                )))
                || (first.eq_ignore_ascii_case("get_format")
                    && matches!(self.peek(), Token::Ident(u) if matches!(
                        u.to_ascii_lowercase().as_str(),
                        "date" | "time" | "datetime" | "timestamp"
                    )))
            {
                if let Token::Ident(u) = self.peek() {
                    args.push(Expr::Literal(Literal::String(u.to_ascii_lowercase())));
                }
                self.advance();
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                }
            }
            if !matches!(self.peek(), Token::RParen) {
                loop {
                    args.push(self.parse_expr(0)?);
                    // v7.25 (round-17) — standard `CAST(expr AS type)`.
                    // The `::` cast already worked; this lowers the
                    // function form onto the same Expr::Cast node.
                    if first.eq_ignore_ascii_case("cast")
                        && args.len() == 1
                        && matches!(self.peek(), Token::As)
                    {
                        self.advance();
                        let target = self.parse_cast_target()?;
                        if !matches!(self.peek(), Token::RParen) {
                            return Err(self.err(format!(
                                "expected ')' to close CAST, got {:?}",
                                self.peek()
                            )));
                        }
                        self.advance();
                        return Ok(Expr::Cast {
                            expr: Box::new(args.pop().expect("one arg")),
                            target,
                        });
                    }
                    // v7.37.7 C.1.8 — PG `substring(str FROM pos FOR len)` syntactic
                    // form. Desugars to the comma-list shape evaluator already
                    // handles. Triggered after the first arg when the function
                    // name is substring / substr and the next token is FROM
                    // (a reserved keyword in PG; SPG also reserves it).
                    if (first.eq_ignore_ascii_case("substring")
                        || first.eq_ignore_ascii_case("substr"))
                        && args.len() == 1
                        && matches!(self.peek(), Token::From)
                    {
                        self.advance();
                        let start = self.parse_expr(0)?;
                        args.push(start);
                        if matches!(self.peek(), Token::For) {
                            self.advance();
                            let length = self.parse_expr(0)?;
                            args.push(length);
                        }
                        if !matches!(self.peek(), Token::RParen) {
                            return Err(self.err(format!(
                                "expected ')' to close substring(... FROM ... [FOR ...]), got {:?}",
                                self.peek()
                            )));
                        }
                        self.advance();
                        return Ok(Expr::FunctionCall {
                            name: first.to_ascii_lowercase(),
                            args,
                        });
                    }
                    // v7.24 (round-16 A) — aggregate-internal
                    // ordering: `array_agg(x ORDER BY y DESC NULLS
                    // LAST)`. Keys close the argument list.
                    if matches!(self.peek(), Token::Order) {
                        self.advance();
                        if !matches!(self.peek(), Token::By) {
                            return Err(self.err(format!(
                                "expected BY after ORDER in aggregate args, got {:?}",
                                self.peek()
                            )));
                        }
                        self.advance();
                        loop {
                            let expr = self.parse_expr(0)?;
                            let desc = if matches!(self.peek(), Token::Desc) {
                                self.advance();
                                true
                            } else if matches!(self.peek(), Token::Asc) {
                                self.advance();
                                false
                            } else {
                                false
                            };
                            let nulls_first = self.parse_optional_nulls_placement()?;
                            agg_order_by.push(OrderBy {
                                expr,
                                desc,
                                nulls_first,
                            });
                            if matches!(self.peek(), Token::Comma) {
                                self.advance();
                            } else {
                                break;
                            }
                        }
                        if !matches!(self.peek(), Token::RParen) {
                            return Err(self.err(format!(
                                "expected ')' after aggregate ORDER BY, got {:?}",
                                self.peek()
                            )));
                        }
                        break;
                    }
                    match self.peek() {
                        Token::Comma => {
                            self.advance();
                        }
                        Token::RParen => break,
                        other => {
                            return Err(self.err(format!(
                                "expected ',' or ')' in function args, got {other:?}"
                            )));
                        }
                    }
                }
            }
            self.advance(); // consume ')'
            // v7.32 (round-29) — ordered-set aggregate tail
            // `name(direct_args) WITHIN GROUP (ORDER BY …)`
            // (percentile_cont / percentile_disc / mode). The sort spec
            // lands in the same `order_by` slot a decorated aggregate
            // uses; the executor dispatches on the function name. WITHIN
            // GROUP and an intra-argument ORDER BY are mutually
            // exclusive (PG rejects both).
            let within_group_order = self.parse_within_group_clause()?;
            if !within_group_order.is_empty() && !agg_order_by.is_empty() {
                return Err(self.err(
                    "an aggregate may not carry both an in-argument ORDER BY and WITHIN GROUP"
                        .into(),
                ));
            }
            let agg_order_by = if within_group_order.is_empty() {
                agg_order_by
            } else {
                within_group_order
            };
            // v7.32 (round-29) — `name(args) FILTER (WHERE …)`.
            let filter = self.parse_filter_clause()?;
            // v4.12: window-function tail — `name(args) OVER (...)`.
            // Promotes the just-parsed FunctionCall into a
            // WindowFunction node carrying partition + order.
            // v6.4.2: also accepts `name(args) IGNORE NULLS OVER (...)`
            // / `RESPECT NULLS OVER (...)` between the closing paren
            // and `OVER`.
            let null_treatment = self.parse_null_treatment_modifier();
            if let Token::Ident(s) | Token::QuotedIdent(s) = self.peek()
                && s.eq_ignore_ascii_case("over")
            {
                if filter.is_some() {
                    return Err(self.err("FILTER on window functions is not supported yet".into()));
                }
                self.advance();
                let (partition_by, order_by, frame) = self.parse_over_clause()?;
                return Ok(Expr::WindowFunction {
                    name: first,
                    args,
                    partition_by,
                    order_by,
                    frame,
                    null_treatment,
                });
            }
            if !agg_order_by.is_empty() || agg_distinct || filter.is_some() {
                return Ok(Expr::AggregateOrdered {
                    call: Box::new(Expr::FunctionCall { name: first, args }),
                    order_by: agg_order_by,
                    distinct: agg_distinct,
                    filter,
                });
            }
            return Ok(Expr::FunctionCall { name: first, args });
        }
        // v7.9.20 — SQL-standard parenless keyword expressions
        // (PG treats these as functions called without parens).
        // Resolve to a synthetic FunctionCall so the engine's
        // eval path reuses the existing function-call routing.
        // mailrs G3.
        let lc = first.to_ascii_lowercase();
        if matches!(
            lc.as_str(),
            "current_date"
                | "current_time"
                | "current_timestamp"
                | "localtimestamp"
                | "localtime"
                // v7.37.17 (17.6 siblings) — session-identity SQL-
                // standard parenless keywords. current_user /
                // session_user / user were already caught by the
                // pgwire canned-response shortcut but bare-select
                // in the embedded engine went through Expr::Column
                // and errored. Adding them here so the parser
                // resolves to a synthetic FunctionCall that reuses
                // the existing eval/functions.rs dispatch.
                | "current_user"
                | "session_user"
                | "current_role"
                | "current_catalog"
                | "current_schema"
                | "current_database"
        ) {
            return Ok(Expr::FunctionCall {
                name: lc,
                args: Vec::new(),
            });
        }
        Ok(Expr::Column(ColumnName {
            qualifier: None,
            name: first,
        }))
    }
}

/// v6.8.2 — walk an expression tree and return the first column
/// reference's bare name. Used by `parse_create_index_stmt_after_create`
/// to derive `CreateIndexStatement.column` from an expression
/// key (so downstream planner code resolving a primary column
/// position keeps working with expression indexes). Returns
/// `None` when the expression has no column ref at all — caller
/// surfaces that as a parse error.
fn extract_first_column(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Column(cn) => Some(cn.name.clone()),
        Expr::FunctionCall { args, .. } => args.iter().find_map(extract_first_column),
        Expr::Binary { lhs, rhs, .. } => {
            extract_first_column(lhs).or_else(|| extract_first_column(rhs))
        }
        Expr::Unary { expr: e, .. } => extract_first_column(e),
        _ => None,
    }
}

fn maybe_not(expr: Expr, negated: bool) -> Expr {
    if negated {
        Expr::Unary {
            op: UnOp::Not,
            expr: Box::new(expr),
        }
    } else {
        expr
    }
}

fn binop_from(tok: &Token) -> Option<(BinOp, u8)> {
    let pair = match tok {
        Token::Or => (BinOp::Or, 1),
        Token::And => (BinOp::And, 2),
        Token::Eq => (BinOp::Eq, 4),
        Token::NotEq => (BinOp::NotEq, 4),
        Token::Lt => (BinOp::Lt, 4),
        Token::LtEq => (BinOp::LtEq, 4),
        Token::Gt => (BinOp::Gt, 4),
        Token::GtEq => (BinOp::GtEq, 4),
        // pgvector distance ops all sit on the same rung — tighter than
        // comparisons (4) so `col <-> v < threshold` parses correctly.
        Token::L2Distance => (BinOp::L2Distance, 5),
        Token::InnerProduct => (BinOp::InnerProduct, 5),
        Token::CosineDistance => (BinOp::CosineDistance, 5),
        Token::Plus => (BinOp::Add, 6),
        Token::Minus => (BinOp::Sub, 6),
        // `||` sits beside `+`/`-` (matches PG conceptually — concat groups
        // by the same level as binary additive arithmetic).
        Token::Concat => (BinOp::Concat, 6),
        // Bitwise `|` / `&` ride the same rung as `||` — PG groups
        // all "other" operators between additive and comparison, so
        // `flags & $1 = 0` parses as `(flags & $1) = 0`.
        //
        // Known divergence (the same one `||` has carried since v1):
        // SPG's rung 6 TIES with `+ -`, while PG binds generic
        // operators LOOSER than additive — `a & b + 1` is
        // `(a & b) + 1` here vs `a & (b + 1)` in PG. Parenthesise
        // mixed bitwise/arithmetic. Keeping every generic operator
        // on one shared rung is deliberate: splitting bitwise off
        // would fix that case but skew `a || b & c`, which PG
        // left-folds at a single level.
        Token::Pipe => (BinOp::BitOr, 6),
        Token::Amp => (BinOp::BitAnd, 6),
        Token::Star => (BinOp::Mul, 7),
        Token::Slash => (BinOp::Div, 7),
        Token::Percent => (BinOp::Mod, 7),
        // v4.14: JSON path ops bind tighter than comparisons (4)
        // and additive (6) so `doc->'k' = 'v'` parses correctly.
        // Same rung as the multiplicative ops.
        Token::JsonGet => (BinOp::JsonGet, 7),
        Token::JsonGetText => (BinOp::JsonGetText, 7),
        Token::JsonGetPath => (BinOp::JsonGetPath, 7),
        Token::JsonGetPathText => (BinOp::JsonGetPathText, 7),
        Token::JsonContains => (BinOp::JsonContains, 7),
        Token::JsonContainedBy => (BinOp::JsonContainedBy, 7),
        Token::JsonKeyExists => (BinOp::JsonKeyExists, 7),
        Token::JsonKeysAny => (BinOp::JsonKeysAny, 7),
        Token::JsonKeysAll => (BinOp::JsonKeysAll, 7),
        // v7.12.2 — `@@` binds at the comparison rung (looser than
        // arithmetic, tighter than AND / OR). PG places `@@` at
        // the same precedence as `=` / `<`, so we follow.
        Token::TsMatch => (BinOp::TsMatch, 4),
        // v7.17.0 Phase 3.P0-47 — PG INET / CIDR containment + overlap.
        // PG places these at the comparison rung (same level as `=`),
        // so we follow.
        Token::InetContainedBy => (BinOp::InetContainedBy, 4),
        Token::InetContainedByEq => (BinOp::InetContainedByEq, 4),
        Token::InetContains => (BinOp::InetContains, 4),
        Token::InetContainsEq => (BinOp::InetContainsEq, 4),
        Token::InetOverlap => (BinOp::InetOverlap, 4),
        _ => return None,
    };
    Some(pair)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
// `as f32` here is intentional: vector elements widen / narrow into f32 on
// purpose. i64 → f32 loses precision past 2^24, f64 → f32 loses precision
// past ~15 decimal digits — both are acceptable for a fixed-precision
// pgvector column.
/// v7.17.0 Phase 1.3 — words that would otherwise be eaten as an
/// implicit table alias and break trailing clauses. WITH lands
/// here so `… FROM t WITH NO DATA` doesn't consume WITH as the
/// alias for `t`; same for ON / WHERE / HAVING / GROUP / ORDER /
/// LIMIT / OFFSET / UNION / EXCEPT / INTERSECT / RETURNING / SET
/// / VALUES / FOR / LATERAL — all of which would otherwise be
/// silently swallowed by `parse_optional_alias`.
fn is_alias_stopword(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "with"
            | "on"
            | "where"
            | "having"
            | "group"
            | "order"
            | "limit"
            | "offset"
            | "union"
            | "except"
            | "intersect"
            | "returning"
            | "set"
            | "values"
            | "for"
            | "lateral"
            | "left"
            | "right"
            | "inner"
            | "outer"
            | "full"
            | "cross"
            | "join"
            | "natural"
            | "using"
            | "fetch"
    )
}

fn extract_numeric_literal(e: &Expr) -> Option<f32> {
    match e {
        Expr::Literal(Literal::Integer(n)) => Some(*n as f32),
        Expr::Literal(Literal::Float(x)) => Some(*x as f32),
        Expr::Unary {
            op: UnOp::Neg,
            expr,
        } => extract_numeric_literal(expr).map(|x| -x),
        _ => None,
    }
}

/// Parse the text inside `INTERVAL '...'` into `(months, micros)`. Accepts
/// one or more `<n> <unit>` pairs separated by whitespace. `<n>` may be
/// negative. Returns `None` if any pair fails to parse or no pair is found.
///
/// Recognised units (case-insensitive, optional trailing `s`):
/// `microsecond`, `millisecond`, `second`, `minute`, `hour`, `day`, `week`,
/// `month`, `year`. `week` widens to 7 days; `year` widens to 12 months.
/// v7.37.5 β — returns `(months, days, micros)`. `days` is preserved
/// as its own dimension so `INTERVAL '1 day'` ≠ `INTERVAL '24 hours'`
/// (PG-canonical: DST and month-boundary semantics depend on this).
/// `week` rolls into `days` (× 7). Sub-day units flow into `micros`.
pub fn parse_interval_text(s: &str) -> Option<(i32, i32, i64)> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.is_empty() || !parts.len().is_multiple_of(2) {
        return None;
    }
    let mut months: i32 = 0;
    let mut days: i32 = 0;
    let mut micros: i64 = 0;
    let mut i = 0;
    while i < parts.len() {
        let n: i64 = parts[i].parse().ok()?;
        let unit = parts[i + 1].to_ascii_lowercase();
        let unit_stripped = unit.strip_suffix('s').unwrap_or(&unit);
        match unit_stripped {
            "microsecond" => micros = micros.checked_add(n)?,
            "millisecond" => micros = micros.checked_add(n.checked_mul(1_000)?)?,
            "second" => micros = micros.checked_add(n.checked_mul(1_000_000)?)?,
            "minute" => micros = micros.checked_add(n.checked_mul(60_000_000)?)?,
            "hour" => micros = micros.checked_add(n.checked_mul(3_600_000_000)?)?,
            "day" => {
                let n32 = i32::try_from(n).ok()?;
                days = days.checked_add(n32)?;
            }
            "week" => {
                let n32 = i32::try_from(n).ok()?;
                days = days.checked_add(n32.checked_mul(7)?)?;
            }
            // v7.37.5 ship triage — accept PG's `format_interval`
            // canonical output (`0 mons 0 days 0 microseconds`) so
            // a round-trip Display → re-parse stays lossless.
            "month" | "mon" => {
                let n32 = i32::try_from(n).ok()?;
                months = months.checked_add(n32)?;
            }
            "year" => {
                let n32 = i32::try_from(n).ok()?;
                months = months.checked_add(n32.checked_mul(12)?)?;
            }
            _ => return None,
        }
        i += 2;
    }
    Some((months, days, micros))
}

/// v7.12.4 — map a bare type-name identifier (the form that
/// appears in a function arg list or RETURNS clause) to a
/// [`ColumnTypeName`]. Returns `None` for unknown / extension
/// types so the caller can preserve them as
/// [`FunctionArgType::Raw`] / [`FunctionReturn::Other`].
///
/// Subset of the full column-type grammar — we deliberately
/// don't parse parameterised forms (`VARCHAR(n)`, `NUMERIC(p,s)`)
/// here because function-arg types in v7.12.4 are mostly the
/// bare form (`text`, `int`, `bytea`, …).
fn map_type_ident_to_column_type_name(ident: &str) -> Option<ColumnTypeName> {
    Some(match ident.to_ascii_lowercase().as_str() {
        "smallint" | "tinyint" => ColumnTypeName::SmallInt,
        "int" | "integer" | "mediumint" => ColumnTypeName::Int,
        "bigint" => ColumnTypeName::BigInt,
        "float" | "double" | "real" => ColumnTypeName::Float,
        "text" => ColumnTypeName::Text,
        "bool" | "boolean" => ColumnTypeName::Bool,
        "date" => ColumnTypeName::Date,
        "timestamp" | "datetime" => ColumnTypeName::Timestamp,
        "timestamptz" => ColumnTypeName::Timestamptz,
        "json" => ColumnTypeName::Json,
        "jsonb" => ColumnTypeName::Jsonb,
        "bytea" | "bytes" => ColumnTypeName::Bytes,
        "tsvector" => ColumnTypeName::TsVector,
        "tsquery" => ColumnTypeName::TsQuery,
        "uuid" => ColumnTypeName::Uuid,
        "interval" => ColumnTypeName::Interval,
        "time" => ColumnTypeName::Time,
        "year" => ColumnTypeName::Year,
        "timetz" => ColumnTypeName::TimeTz,
        "money" => ColumnTypeName::Money,
        _ => return None,
    })
}

/// v7.12.4 — parse a PL/pgSQL function body (the bytes between
/// `$$ ... $$`). Returns the parsed `BEGIN ... END;` block.
///
/// v7.12.4 grammar (strict subset — IF / LOOP / DECLARE / RAISE
/// / embedded SQL land in v7.12.5+):
///
/// ```text
///   body          := [ws] block [ws]
///   block         := BEGIN stmt ( ; stmt )* [ ; ] END [ ; ]
///   stmt          := assign | return
///   assign        := assign_target := expr
///   assign_target := ( NEW | OLD ) . ident | ident
///   return        := RETURN ( NEW | OLD | NULL | expr )
/// ```
///
/// `expr` is parsed by recursing into the regular `Parser` — so a
/// PL/pgSQL `NEW.search_vector := to_tsvector('english',
/// NEW.subject || ' ' || NEW.sender)` body shape works without
/// the body parser knowing what `to_tsvector` is.
///
/// Errors here cause the caller to fall back to
/// `FunctionBody::Raw` — keeping the CREATE FUNCTION DDL itself
/// successful, but the executor will refuse to invoke the
/// function with an "unparseable body" error.
/// v7.12.4 — public alias for [`parse_plpgsql_body`] re-exported
/// from the crate root as `spg_sql::parse_function_body`.
pub fn parse_function_body(body: &str) -> Result<PlPgSqlBlock, ParseError> {
    parse_plpgsql_body(body)
}

fn parse_plpgsql_body(body: &str) -> Result<PlPgSqlBlock, ParseError> {
    // Use the regular lexer on the body text. The trailing
    // `END;` may or may not have a semicolon; the lexer treats
    // both forms identically.
    let tokens = lexer::tokenize(body).map_err(|e| ParseError {
        message: alloc::format!("plpgsql body lex error: {e}"),
        token_pos: 0,
    })?;
    let mut parser = Parser::new(tokens);
    parser.parse_plpgsql_block()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    fn parse(s: &str) -> Statement {
        parse_statement(s).expect("parse ok")
    }

    // v7.37.43-T4 sentori cutover acceptance — `release`, `index`,
    // `tables`, `partition`, etc. are unreserved keywords per PG's
    // `pg_get_keywords()` and MUST be usable as column / table /
    // alias names. Pre-T4 every drop-in user whose schema had one
    // of these as a column name (sentori events.release, mailrs
    // messages.index in some forks) blew the parser up at CREATE
    // TABLE time with "expected identifier, got Release". The
    // generalisation lives in `unreserved_keyword_text` + the
    // `expect_ident_like` and `parse_atom` arms that consult it.
    #[test]
    fn release_usable_as_column_name_in_create_table() {
        let stmt =
            parse("CREATE TABLE events (id INT PRIMARY KEY, release TEXT NOT NULL, payload TEXT)");
        if let Statement::CreateTable(t) = stmt {
            let names: alloc::vec::Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(names, alloc::vec!["id", "release", "payload"]);
        } else {
            panic!("expected CreateTable");
        }
    }

    #[test]
    fn release_usable_as_column_ref_in_select_projection() {
        // The sentori `0003_partition_events.sql` INSERT-SELECT
        // walk references `release` in both column lists; the
        // projection-side use exercises `parse_atom`'s relaxed
        // identifier set.
        parse("SELECT id, release, payload FROM events WHERE id = 1");
    }

    #[test]
    fn release_usable_as_column_ref_in_insert_column_list() {
        // INSERT INTO t (id, release, payload) VALUES (…)
        parse("INSERT INTO events (id, release, payload) VALUES (1, '1.0.0', 'data')");
    }

    #[test]
    fn alter_column_drop_not_null_uses_keyword_drop_token() {
        // Sentori `0013_audit_tombstone.sql` issues
        // `ALTER TABLE … ALTER COLUMN x DROP NOT NULL`. The lexer
        // emits Token::Drop (not Ident("drop")); the parser must
        // accept both in the ALTER COLUMN sub-dispatch.
        parse("ALTER TABLE audit_logs ALTER COLUMN org_id DROP NOT NULL");
    }

    #[test]
    fn create_index_accepts_parenthesised_expression_key() {
        // sentori `0040_events_bundle_idx.sql` shape — JSONB
        // expression index. Pre-T4 the parser bailed at the
        // inner `(` with "expected column ident or expression,
        // got LParen". The Token::LParen arm in CREATE INDEX
        // routes through the expression parser instead.
        parse(
            "CREATE INDEX IF NOT EXISTS events_bundle_id_idx \
             ON events ((payload->'bundle'->>'id'))",
        );
    }

    // v7.30.2 (mailrs round-25 ask 2) — nesting / chain budgets must
    // surface as parse errors, never stack overflows (embed hosts
    // abort on overflow).
    #[test]
    fn nesting_budget_errors_cleanly() {
        let depth = MAX_NEST_DEPTH + 50;
        let sql = format!("SELECT {}1{}", "(".repeat(depth), ")".repeat(depth));
        let err = parse_statement(&sql).expect_err("must reject");
        assert!(err.message.contains("nests deeper"), "{err:?}");
        // Within budget still parses.
        let sql = format!("SELECT {}1{}", "(".repeat(48), ")".repeat(48));
        parse(&sql);
    }

    #[test]
    fn binary_chain_budget_errors_cleanly() {
        let sql = format!("SELECT 1{}", " + 1".repeat(MAX_BINARY_CHAIN + 50));
        let err = parse_statement(&sql).expect_err("must reject");
        assert!(err.message.contains("chained binary"), "{err:?}");
        // Within budget still parses (chain depth ≤ budget is safe
        // for recursive eval/drop on 2 MiB stacks).
        let sql = format!("SELECT 1{}", " + 1".repeat(200));
        parse(&sql);
    }

    #[test]
    fn in_list_unaffected_by_chain_budget() {
        // Flat InList: 20k elements parse fine and stay flat.
        let items: alloc::vec::Vec<String> = (0..20_000).map(|k| k.to_string()).collect();
        let sql = format!("SELECT 1 WHERE 5 IN ({})", items.join(","));
        let Statement::Select(s) = parse(&sql) else {
            panic!("expected select")
        };
        let Some(Expr::InList { list, negated, .. }) = s.where_ else {
            panic!("expected flat InList, got {:?}", s.where_)
        };
        assert_eq!(list.len(), 20_000);
        assert!(!negated);
    }

    fn lit_int(n: i64) -> Expr {
        Expr::Literal(Literal::Integer(n))
    }

    fn col(name: &str) -> Expr {
        Expr::Column(ColumnName {
            qualifier: None,
            name: name.into(),
        })
    }

    #[test]
    fn select_single_integer() {
        let s = parse("SELECT 1");
        let Statement::Select(s) = s else {
            panic!("expected SELECT")
        };
        assert_eq!(s.items.len(), 1);
        assert!(s.from.is_none());
        assert!(s.where_.is_none());
    }

    #[test]
    fn select_multiple_literal_kinds() {
        let s = parse("SELECT 1, 'hi', NULL, TRUE, 1.5");
        let Statement::Select(s) = s else {
            panic!("expected SELECT")
        };
        assert_eq!(s.items.len(), 5);
    }

    #[test]
    fn select_wildcard_from_table() {
        let s = parse("SELECT * FROM users");
        let Statement::Select(s) = s else {
            panic!("expected SELECT")
        };
        assert!(matches!(s.items[..], [SelectItem::Wildcard]));
        assert_eq!(s.from.as_ref().unwrap().primary.name, "users");
    }

    #[test]
    fn select_with_table_alias() {
        let s = parse("SELECT * FROM users AS u");
        let Statement::Select(s) = s else {
            panic!("expected SELECT")
        };
        let t = &s.from.as_ref().unwrap().primary;
        assert_eq!(t.name, "users");
        assert_eq!(t.alias.as_deref(), Some("u"));
    }

    #[test]
    fn select_with_where_eq() {
        let s = parse("SELECT a FROM t WHERE a = 1");
        let Statement::Select(s) = s else {
            panic!("expected SELECT")
        };
        let w = s.where_.unwrap();
        assert_eq!(
            w,
            Expr::Binary {
                lhs: Box::new(col("a")),
                op: BinOp::Eq,
                rhs: Box::new(lit_int(1)),
            }
        );
    }

    #[test]
    fn arithmetic_precedence() {
        let s = parse("SELECT 1 + 2 * 3");
        let Statement::Select(s) = s else {
            panic!("expected SELECT")
        };
        let SelectItem::Expr { expr, .. } = &s.items[0] else {
            panic!("wildcard?")
        };
        assert_eq!(
            expr,
            &Expr::Binary {
                lhs: Box::new(lit_int(1)),
                op: BinOp::Add,
                rhs: Box::new(Expr::Binary {
                    lhs: Box::new(lit_int(2)),
                    op: BinOp::Mul,
                    rhs: Box::new(lit_int(3)),
                }),
            }
        );
    }

    #[test]
    fn parentheses_override_precedence() {
        let s = parse("SELECT (1 + 2) * 3");
        let Statement::Select(s) = s else {
            panic!("expected SELECT")
        };
        let SelectItem::Expr { expr, .. } = &s.items[0] else {
            panic!()
        };
        assert_eq!(
            expr,
            &Expr::Binary {
                lhs: Box::new(Expr::Binary {
                    lhs: Box::new(lit_int(1)),
                    op: BinOp::Add,
                    rhs: Box::new(lit_int(2)),
                }),
                op: BinOp::Mul,
                rhs: Box::new(lit_int(3)),
            }
        );
    }

    #[test]
    fn not_binds_below_comparison() {
        // `NOT a = 1` should parse as `NOT (a = 1)`.
        let s = parse("SELECT NOT a = 1 FROM t");
        let Statement::Select(s) = s else {
            panic!("expected SELECT")
        };
        let SelectItem::Expr { expr, .. } = &s.items[0] else {
            panic!()
        };
        assert_eq!(
            expr,
            &Expr::Unary {
                op: UnOp::Not,
                expr: Box::new(Expr::Binary {
                    lhs: Box::new(col("a")),
                    op: BinOp::Eq,
                    rhs: Box::new(lit_int(1)),
                }),
            }
        );
    }

    #[test]
    fn unary_minus_binds_above_multiplication() {
        // `-a * 2` should be `(-a) * 2`.
        let s = parse("SELECT -a * 2 FROM t");
        let Statement::Select(s) = s else {
            panic!("expected SELECT")
        };
        let SelectItem::Expr { expr, .. } = &s.items[0] else {
            panic!()
        };
        assert_eq!(
            expr,
            &Expr::Binary {
                lhs: Box::new(Expr::Unary {
                    op: UnOp::Neg,
                    expr: Box::new(col("a")),
                }),
                op: BinOp::Mul,
                rhs: Box::new(lit_int(2)),
            }
        );
    }

    #[test]
    fn qualified_column() {
        let s = parse("SELECT t.col FROM t");
        let Statement::Select(s) = s else {
            panic!("expected SELECT")
        };
        let SelectItem::Expr { expr, .. } = &s.items[0] else {
            panic!()
        };
        assert_eq!(
            expr,
            &Expr::Column(ColumnName {
                qualifier: Some("t".into()),
                name: "col".into()
            })
        );
    }

    #[test]
    fn select_item_alias_with_as() {
        let s = parse("SELECT a AS y FROM t");
        let Statement::Select(s) = s else {
            panic!("expected SELECT")
        };
        let SelectItem::Expr { alias, .. } = &s.items[0] else {
            panic!()
        };
        assert_eq!(alias.as_deref(), Some("y"));
    }

    #[test]
    fn trailing_semicolon_accepted() {
        let s = parse("SELECT 1;");
        let Statement::Select(s) = s else {
            panic!("expected SELECT")
        };
        assert_eq!(s.items.len(), 1);
    }

    #[test]
    fn boolean_chain_with_and_or_not() {
        // (NOT a) OR (b AND (NOT c))
        let s = parse("SELECT NOT a OR b AND NOT c FROM t");
        let Statement::Select(s) = s else {
            panic!("expected SELECT")
        };
        let SelectItem::Expr { expr, .. } = &s.items[0] else {
            panic!()
        };
        let expected = Expr::Binary {
            lhs: Box::new(Expr::Unary {
                op: UnOp::Not,
                expr: Box::new(col("a")),
            }),
            op: BinOp::Or,
            rhs: Box::new(Expr::Binary {
                lhs: Box::new(col("b")),
                op: BinOp::And,
                rhs: Box::new(Expr::Unary {
                    op: UnOp::Not,
                    expr: Box::new(col("c")),
                }),
            }),
        };
        assert_eq!(expr, &expected);
    }

    #[test]
    fn empty_input_errors() {
        // v7.14.0 — pg_dump preambles emit several comment-only
        // / blank-line statements that collapse to Statement::
        // Empty rather than a parse error. The old "SELECT in
        // message" assertion is stale; verify the new contract:
        // empty / whitespace / comment-only input parses to
        // Statement::Empty.
        assert!(matches!(parse_statement("").unwrap(), Statement::Empty));
        assert!(matches!(
            parse_statement("  \n\t ").unwrap(),
            Statement::Empty
        ));
        // Sanity: malformed-but-non-empty still errors.
        assert!(parse_statement("SELECT FROM WHERE").is_err());
    }

    #[test]
    fn unmatched_paren_errors() {
        assert!(parse_statement("SELECT (1 + 2").is_err());
    }

    #[test]
    fn display_round_trip_simple_select() {
        let original = parse("SELECT a + 1 FROM t WHERE a > 0");
        let text = original.to_string();
        let again = parse_statement(&text).expect("re-parse");
        assert_eq!(original, again);
    }

    // --- CREATE TABLE & INSERT (v0.3) ---------------------------------------

    #[test]
    fn create_table_single_column() {
        let s = parse("CREATE TABLE foo (a INT)");
        let Statement::CreateTable(c) = s else {
            panic!("expected CreateTable")
        };
        assert_eq!(c.name, "foo");
        assert_eq!(c.columns.len(), 1);
        assert_eq!(c.columns[0].name, "a");
        assert_eq!(c.columns[0].ty, ColumnTypeName::Int);
        assert!(c.columns[0].nullable);
    }

    #[test]
    fn create_table_multi_column_with_not_null_mix() {
        let s = parse("CREATE TABLE u (id INT NOT NULL, name TEXT, score FLOAT NOT NULL, ok BOOL)");
        let Statement::CreateTable(c) = s else {
            panic!()
        };
        assert_eq!(c.columns.len(), 4);
        assert_eq!(c.columns[0].ty, ColumnTypeName::Int);
        assert!(!c.columns[0].nullable);
        assert_eq!(c.columns[1].ty, ColumnTypeName::Text);
        assert!(c.columns[1].nullable);
        assert_eq!(c.columns[2].ty, ColumnTypeName::Float);
        assert!(!c.columns[2].nullable);
        assert_eq!(c.columns[3].ty, ColumnTypeName::Bool);
    }

    #[test]
    fn create_table_bigint_supported() {
        let s = parse("CREATE TABLE accounts (id BIGINT NOT NULL)");
        let Statement::CreateTable(c) = s else {
            panic!()
        };
        assert_eq!(c.columns[0].ty, ColumnTypeName::BigInt);
    }

    #[test]
    fn create_table_vector_default_is_f32() {
        let s = parse("CREATE TABLE t (v VECTOR(128))");
        let Statement::CreateTable(c) = s else {
            panic!()
        };
        assert_eq!(
            c.columns[0].ty,
            ColumnTypeName::Vector {
                dim: 128,
                encoding: VecEncoding::F32,
            },
        );
    }

    #[test]
    fn create_table_vector_using_sq8() {
        // v6.0.1: `USING SQ8` selects scalar-quantised encoding.
        // Case-insensitive on both `USING` and the encoding name.
        for sql in [
            "CREATE TABLE t (v VECTOR(128) USING SQ8)",
            "CREATE TABLE t (v VECTOR(128) using sq8)",
        ] {
            let s = parse(sql);
            let Statement::CreateTable(c) = s else {
                panic!()
            };
            assert_eq!(
                c.columns[0].ty,
                ColumnTypeName::Vector {
                    dim: 128,
                    encoding: VecEncoding::Sq8,
                },
                "{sql}",
            );
        }
    }

    #[test]
    fn create_table_vector_using_unknown_errors() {
        // v7.16.1 — the inline `USING <encoding>` shape on
        // CREATE TABLE column defs was withdrawn before
        // v7.14.0 in favour of `CREATE INDEX … USING hnsw
        // (col vector_<metric>_ops)`; the parser now rejects
        // USING at column-list position with a clearer
        // "expected ',' or ')'" message. Test asserts the
        // current rejection, not the old "unknown vector
        // encoding" string.
        let err = parse_statement("CREATE TABLE t (v VECTOR(8) USING PQ8)").unwrap_err();
        assert!(
            err.message.contains("USING")
                || err.message.contains("using")
                || err.message.contains("')'")
                || err.message.contains("','"),
            "expected USING/column-list rejection, got: {}",
            err.message
        );
    }

    #[test]
    fn vector_using_sq8_display_roundtrips() {
        // The Display impl must produce text that re-parses to the
        // same AST. Guard for the v6.0.1 `USING SQ8` suffix.
        let s = parse("CREATE TABLE t (v VECTOR(64) USING SQ8)");
        let Statement::CreateTable(c) = s else {
            panic!()
        };
        assert_eq!(c.columns[0].ty.to_string(), "VECTOR(64) USING SQ8");
    }

    #[test]
    fn parser_recognises_placeholders() {
        use crate::ast::{Expr, SelectItem, Statement};
        // $N in expression position parses as Expr::Placeholder(N).
        let s = parse("SELECT $1, $2 + 1 FROM t WHERE x = $3");
        let Statement::Select(sel) = s else { panic!() };
        assert!(matches!(
            sel.items[0],
            SelectItem::Expr {
                expr: Expr::Placeholder(1),
                alias: None
            }
        ));
        // $2 + 1
        let SelectItem::Expr {
            expr: Expr::Binary { lhs, rhs, .. },
            ..
        } = &sel.items[1]
        else {
            panic!()
        };
        assert!(matches!(**lhs, Expr::Placeholder(2)));
        assert!(matches!(**rhs, Expr::Literal(Literal::Integer(1))));
        // WHERE x = $3
        let Some(Expr::Binary { rhs, .. }) = sel.where_.as_ref() else {
            panic!()
        };
        assert!(matches!(**rhs, Expr::Placeholder(3)));
    }

    #[test]
    fn parser_rejects_dollar_zero() {
        // $0 is not valid in PG; the lexer rejects it.
        assert!(parse_statement("SELECT $0").is_err());
    }

    #[test]
    fn placeholder_display_roundtrips() {
        // The Display impl must produce text that re-lexes to the
        // same Placeholder token.
        let s = parse("SELECT $42 FROM t");
        let printed = s.to_string();
        assert!(printed.contains("$42"));
        let again = parse(&printed);
        assert_eq!(s, again);
    }

    #[test]
    fn alter_index_rebuild_bare() {
        use crate::ast::{AlterIndexTarget, Statement};
        let s = parse("ALTER INDEX my_idx REBUILD");
        let Statement::AlterIndex(a) = s else {
            panic!("expected AlterIndex, got {s:?}")
        };
        assert_eq!(a.name, "my_idx");
        assert_eq!(a.target, AlterIndexTarget::Rebuild { encoding: None });
    }

    #[test]
    fn alter_index_rebuild_with_encoding() {
        use crate::ast::{AlterIndexTarget, Statement};
        for (sql, want) in [
            (
                "ALTER INDEX my_idx REBUILD WITH (encoding = F32)",
                VecEncoding::F32,
            ),
            (
                "ALTER INDEX my_idx REBUILD WITH (encoding = sq8)",
                VecEncoding::Sq8,
            ),
            (
                "ALTER INDEX my_idx REBUILD WITH (encoding = HALF)",
                VecEncoding::F16,
            ),
        ] {
            let s = parse(sql);
            let Statement::AlterIndex(a) = s else {
                panic!("{sql}: expected AlterIndex")
            };
            assert_eq!(a.name, "my_idx");
            assert_eq!(
                a.target,
                AlterIndexTarget::Rebuild {
                    encoding: Some(want)
                },
                "{sql}"
            );
        }
    }

    #[test]
    fn alter_index_rebuild_unknown_encoding_errors() {
        let err = parse_statement("ALTER INDEX my_idx REBUILD WITH (encoding = PQ8)").unwrap_err();
        assert!(
            err.message.contains("unknown vector encoding"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn alter_index_rebuild_display_roundtrips() {
        for (input, want) in [
            ("ALTER INDEX my_idx REBUILD", "ALTER INDEX my_idx REBUILD"),
            (
                "ALTER INDEX my_idx REBUILD WITH (encoding = SQ8)",
                "ALTER INDEX my_idx REBUILD WITH (encoding = SQ8)",
            ),
            (
                "ALTER INDEX my_idx REBUILD WITH (encoding = HALF)",
                "ALTER INDEX my_idx REBUILD WITH (encoding = HALF)",
            ),
        ] {
            let s = parse(input);
            assert_eq!(s.to_string(), want);
        }
    }

    #[test]
    fn create_table_unknown_type_defers_to_engine() {
        // v4.9 picked XML as a parse-time "unsupported column
        // type" probe. v7.17.0 Phase 1.4 changed the contract:
        // an unknown type ident parses as Text + `user_type_ref`
        // so CREATE TABLE can resolve user-defined enum / domain
        // types — rejection of truly-unknown types moved to the
        // engine's catalog lookup. v7.37.5 ζ-A then promoted XML
        // to a first-class built-in, so this probe switched to a
        // synthetic name nothing in the lexer will ever recognise.
        let stmt = parse_statement("CREATE TABLE x (a my_user_type)").unwrap();
        let Statement::CreateTable(t) = stmt else {
            panic!("expected CreateTable");
        };
        assert_eq!(t.columns[0].user_type_ref.as_deref(), Some("my_user_type"));
    }

    #[test]
    fn create_table_missing_table_keyword_errors() {
        assert!(parse_statement("CREATE x (a INT)").is_err());
    }

    // v7.37.6-B(sentori Epic 2 P0)— `PARTITION BY RANGE` parent +
    // `PARTITION OF parent <bounds>` child parse + Display round-trip.

    #[test]
    fn parse_create_table_partition_by_range() {
        use crate::ast::{PartitionBySpec, PartitionKindAst};
        let stmt = parse_statement(
            "CREATE TABLE events_partitioned (id BIGINT NOT NULL, ts TIMESTAMPTZ NOT NULL, \
             payload JSONB) PARTITION BY RANGE (ts)",
        )
        .unwrap();
        let Statement::CreateTable(t) = stmt else {
            panic!("expected CreateTable");
        };
        assert!(t.partition_of.is_none(), "parent has no partition_of");
        assert_eq!(t.columns.len(), 3);
        let by = t.partition_by.as_ref().expect("expected PARTITION BY");
        assert_eq!(
            by,
            &PartitionBySpec {
                kind: PartitionKindAst::Range,
                key_columns: alloc::vec!["ts".to_string()],
            }
        );
        // Display round-trip preserves the suffix. `quote_ident`
        // only adds double quotes when the ident needs escaping, so
        // a plain `ts` survives bare here.
        assert!(
            t.to_string().contains("PARTITION BY RANGE (ts)"),
            "Display lost PARTITION BY suffix: {t}"
        );
    }

    #[test]
    fn parse_create_table_partition_of_range() {
        use crate::ast::{PartitionOfBoundsAst, PartitionOfSpec};
        let stmt = parse_statement(
            "CREATE TABLE events_2026_06 PARTITION OF events_partitioned \
             FOR VALUES FROM ('2026-06-01 00:00:00+00') TO ('2026-07-01 00:00:00+00')",
        )
        .unwrap();
        let Statement::CreateTable(t) = stmt else {
            panic!("expected CreateTable");
        };
        assert!(t.columns.is_empty(), "child inherits columns from parent");
        assert!(t.partition_by.is_none());
        let of = t.partition_of.as_ref().expect("expected PARTITION OF");
        assert_eq!(of.parent_name, "events_partitioned");
        let PartitionOfSpec { bounds, .. } = of.clone();
        match bounds {
            PartitionOfBoundsAst::Range { lower, upper } => {
                assert!(lower.to_string().contains("2026-06-01"));
                assert!(upper.to_string().contains("2026-07-01"));
            }
            other => panic!("expected Range, got {other:?}"),
        }
        // Display round-trip emits the FOR VALUES tail. `quote_ident`
        // skips quotes when not required, so the parent name appears
        // bare here.
        let s = t.to_string();
        assert!(
            s.contains("PARTITION OF events_partitioned"),
            "Display lost PARTITION OF: {s}"
        );
        assert!(s.contains("FOR VALUES FROM"), "Display lost FROM: {s}");
        assert!(s.contains(") TO ("), "Display lost TO: {s}");
    }

    #[test]
    fn parse_create_table_partition_of_default() {
        use crate::ast::PartitionOfBoundsAst;
        let stmt =
            parse_statement("CREATE TABLE events_default PARTITION OF events_partitioned DEFAULT")
                .unwrap();
        let Statement::CreateTable(t) = stmt else {
            panic!("expected CreateTable");
        };
        let of = t.partition_of.as_ref().expect("expected PARTITION OF");
        assert_eq!(of.parent_name, "events_partitioned");
        assert!(matches!(of.bounds, PartitionOfBoundsAst::Default));
        assert!(
            t.to_string()
                .contains("PARTITION OF events_partitioned DEFAULT"),
            "Display lost DEFAULT: {t}"
        );
    }

    #[test]
    fn parse_create_table_partition_by_list() {
        // v7.37.16 (16.1) — `PARTITION BY LIST (key)` parent + a
        // child with `FOR VALUES IN (lit, lit, …)`.
        use crate::ast::{PartitionBySpec, PartitionKindAst, PartitionOfBoundsAst};
        let parent = parse_statement(
            "CREATE TABLE events_listed (region TEXT) PARTITION BY LIST (region)",
        )
        .unwrap();
        let Statement::CreateTable(t) = parent else {
            panic!("expected CreateTable");
        };
        let Some(PartitionBySpec { kind, ref key_columns }) = t.partition_by else {
            panic!("expected PARTITION BY");
        };
        assert_eq!(kind, PartitionKindAst::List);
        assert_eq!(*key_columns, vec!["region".to_string()]);
        assert!(t.to_string().contains("PARTITION BY LIST (region)"));

        let child = parse_statement(
            "CREATE TABLE events_apac PARTITION OF events_listed \
             FOR VALUES IN ('jp', 'kr', 'tw')",
        )
        .unwrap();
        let Statement::CreateTable(c) = child else {
            panic!("expected CreateTable");
        };
        let of = c.partition_of.as_ref().expect("expected PARTITION OF");
        let PartitionOfBoundsAst::List { values } = &of.bounds else {
            panic!("expected List bounds, got {:?}", of.bounds);
        };
        assert_eq!(values.len(), 3);
        let disp = c.to_string();
        assert!(disp.contains("FOR VALUES IN ("), "Display lost IN: {disp}");
    }

    #[test]
    fn parse_create_table_partition_by_hash() {
        // v7.37.16 (16.2) — `PARTITION BY HASH (key)` parent + a
        // child with `FOR VALUES WITH (MODULUS m, REMAINDER r)`.
        use crate::ast::{PartitionBySpec, PartitionKindAst, PartitionOfBoundsAst};
        let parent =
            parse_statement("CREATE TABLE orders_h (id BIGINT) PARTITION BY HASH (id)").unwrap();
        let Statement::CreateTable(t) = parent else {
            panic!("expected CreateTable");
        };
        let Some(PartitionBySpec { kind, ref key_columns }) = t.partition_by else {
            panic!("expected PARTITION BY");
        };
        assert_eq!(kind, PartitionKindAst::Hash);
        assert_eq!(*key_columns, vec!["id".to_string()]);
        assert!(t.to_string().contains("PARTITION BY HASH (id)"));

        let child = parse_statement(
            "CREATE TABLE orders_h_0 PARTITION OF orders_h \
             FOR VALUES WITH (MODULUS 4, REMAINDER 0)",
        )
        .unwrap();
        let Statement::CreateTable(c) = child else {
            panic!("expected CreateTable");
        };
        let of = c.partition_of.as_ref().expect("expected PARTITION OF");
        let PartitionOfBoundsAst::Hash { modulus, remainder } = of.bounds else {
            panic!("expected Hash bounds");
        };
        assert_eq!(modulus, 4);
        assert_eq!(remainder, 0);
        let disp = c.to_string();
        assert!(
            disp.contains("FOR VALUES WITH (MODULUS 4, REMAINDER 0)"),
            "Display lost HASH bounds: {disp}"
        );

        // Validation: REMAINDER ≥ MODULUS is rejected at parse time.
        let bad = parse_statement(
            "CREATE TABLE orders_h_bad PARTITION OF orders_h \
             FOR VALUES WITH (MODULUS 4, REMAINDER 4)",
        );
        let msg = format!("{}", bad.unwrap_err());
        assert!(
            msg.contains("REMAINDER") && msg.contains("MODULUS"),
            "expected REMAINDER/MODULUS validation error: {msg}"
        );
    }

    #[test]
    fn parse_create_table_partition_of_rejects_columns() {
        // v7.37.6-B contract: PARTITION OF children inherit columns
        // from the parent; an explicit list MUST surface as a parse
        // error rather than getting silently ignored.
        let err = parse_statement(
            "CREATE TABLE events_2026_06 PARTITION OF events_partitioned (id BIGINT) \
             FOR VALUES FROM ('a') TO ('b')",
        );
        assert!(err.is_err(), "expected parse error for explicit columns");
        let msg = format!("{}", err.unwrap_err());
        assert!(
            msg.contains("PARTITION OF") && msg.contains("column"),
            "error should mention PARTITION OF + columns: {msg}"
        );
    }

    #[test]
    fn insert_single_value() {
        let s = parse("INSERT INTO foo VALUES (42)");
        let Statement::Insert(i) = s else {
            panic!("expected Insert")
        };
        assert_eq!(i.table, "foo");
        assert_eq!(i.rows.len(), 1);
        assert_eq!(i.rows[0].len(), 1);
        assert!(matches!(i.rows[0][0], Expr::Literal(Literal::Integer(42))));
    }

    #[test]
    fn insert_multi_value_with_mixed_literals() {
        let s = parse("INSERT INTO foo VALUES (1, 'hi', 3.14, TRUE, NULL)");
        let Statement::Insert(i) = s else { panic!() };
        assert_eq!(i.rows.len(), 1);
        assert_eq!(i.rows[0].len(), 5);
    }

    #[test]
    fn insert_missing_into_errors() {
        assert!(parse_statement("INSERT foo VALUES (1)").is_err());
    }

    #[test]
    fn create_table_round_trip() {
        let original =
            parse("CREATE TABLE foo (id BIGINT NOT NULL, label TEXT, score FLOAT NOT NULL)");
        let text = original.to_string();
        let again = parse_statement(&text).expect("re-parse");
        assert_eq!(original, again);
    }

    #[test]
    fn insert_round_trip_with_negation_and_string() {
        let original = parse("INSERT INTO t VALUES (-1, 'it''s', NULL)");
        let text = original.to_string();
        let again = parse_statement(&text).expect("re-parse");
        assert_eq!(original, again);
    }

    #[test]
    fn unknown_keyword_at_statement_start_errors() {
        // v4.4: UPDATE is real SQL now. Use a fabricated keyword so
        // the top-level dispatch still has no branch to take.
        let err = parse_statement("FROBNICATE foo SET x = 1").unwrap_err();
        assert!(err.message.contains("expected SELECT"));
    }

    // --- v0.8 CREATE INDEX --------------------------------------------------

    #[test]
    fn create_index_basic() {
        let s = parse("CREATE INDEX idx_id ON users (id)");
        let Statement::CreateIndex(c) = s else {
            panic!("expected CreateIndex")
        };
        assert_eq!(c.name, "idx_id");
        assert_eq!(c.table, "users");
        assert_eq!(c.column, "id");
    }

    #[test]
    fn create_index_missing_on_errors() {
        assert!(parse_statement("CREATE INDEX foo users (id)").is_err());
    }

    #[test]
    fn create_index_missing_paren_errors() {
        assert!(parse_statement("CREATE INDEX foo ON users id").is_err());
    }

    #[test]
    fn create_index_round_trip() {
        let original = parse("CREATE INDEX by_name ON users (name)");
        let again = parse_statement(&original.to_string()).unwrap();
        assert_eq!(original, again);
    }

    // --- v7.9.29 CREATE UNIQUE INDEX [WHERE pred] (mailrs K1) -------------

    #[test]
    fn create_unique_index_basic() {
        let s = parse("CREATE UNIQUE INDEX uq_x ON t (a)");
        let Statement::CreateIndex(c) = s else {
            panic!("expected CreateIndex");
        };
        assert!(c.is_unique);
        assert_eq!(c.column, "a");
        assert!(c.partial_predicate.is_none());
    }

    #[test]
    fn create_unique_index_partial() {
        // mailrs's email_templates "one default per user" shape.
        let s = parse(
            "CREATE UNIQUE INDEX idx_email_templates_user_default \
             ON email_templates (user_address) WHERE is_default = true",
        );
        let Statement::CreateIndex(c) = s else {
            panic!("expected CreateIndex");
        };
        assert!(c.is_unique);
        assert_eq!(c.table, "email_templates");
        assert_eq!(c.column, "user_address");
        assert!(c.partial_predicate.is_some());
    }

    #[test]
    fn create_unique_index_composite_with_predicate() {
        // mailrs's calendar_events instance: composite columns.
        let s = parse(
            "CREATE UNIQUE INDEX uq_calendar_events_instance \
             ON calendar_events (calendar_id, uid, recurrence_id) \
             WHERE recurrence_id IS NOT NULL",
        );
        let Statement::CreateIndex(c) = s else {
            panic!("expected CreateIndex");
        };
        assert!(c.is_unique);
        assert_eq!(c.column, "calendar_id");
        assert_eq!(
            c.extra_columns,
            vec!["uid".to_string(), "recurrence_id".to_string()]
        );
        assert!(c.partial_predicate.is_some());
    }

    #[test]
    fn create_unique_index_using_btree_ok() {
        let s = parse("CREATE UNIQUE INDEX uq_x ON t USING btree (a)");
        assert!(matches!(s, Statement::CreateIndex(ref c) if c.is_unique));
    }

    #[test]
    fn create_unique_index_using_hnsw_rejected() {
        let err =
            parse_statement("CREATE UNIQUE INDEX uq_v ON t USING hnsw (embedding)").unwrap_err();
        assert!(err.message.contains("UNIQUE"), "{}", err.message);
    }

    #[test]
    fn create_unique_index_round_trip() {
        let original = parse(
            "CREATE UNIQUE INDEX uq_calendar_events_master \
             ON calendar_events (calendar_id, uid) WHERE recurrence_id IS NULL",
        );
        let again = parse_statement(&original.to_string()).unwrap();
        assert_eq!(original, again);
    }

    #[test]
    fn create_unique_without_index_errors() {
        let err = parse_statement("CREATE UNIQUE TABLE t (a INT)").unwrap_err();
        assert!(err.message.contains("INDEX"), "{}", err.message);
    }

    // --- v7.10.4 BYTES / BYTEA column type (Epic 1) ----------------------

    #[test]
    fn create_table_bytea_column() {
        let s = parse("CREATE TABLE t (id INT NOT NULL, payload BYTEA NOT NULL)");
        let Statement::CreateTable(c) = s else {
            panic!("expected CreateTable");
        };
        assert_eq!(c.columns.len(), 2);
        assert_eq!(c.columns[1].ty, ColumnTypeName::Bytes);
        assert!(!c.columns[1].nullable);
    }

    #[test]
    fn create_table_bytes_alias_column() {
        let s = parse("CREATE TABLE t (blob BYTES)");
        let Statement::CreateTable(c) = s else {
            panic!("expected CreateTable");
        };
        assert_eq!(c.columns[0].ty, ColumnTypeName::Bytes);
    }

    #[test]
    fn bytea_round_trip_display() {
        let original = parse("CREATE TABLE t (a BYTEA NOT NULL)");
        let again = parse_statement(&original.to_string()).unwrap();
        assert_eq!(original, again);
    }

    // --- v0.9 transactions -------------------------------------------------

    #[test]
    fn begin_commit_rollback_parse_as_unit_variants() {
        assert_eq!(parse("BEGIN"), Statement::Begin);
        assert_eq!(parse("COMMIT"), Statement::Commit);
        assert_eq!(parse("ROLLBACK"), Statement::Rollback);
        // Trailing semicolons accepted too.
        assert_eq!(parse("BEGIN;"), Statement::Begin);
    }

    // --- v1.2: pgvector distance ops + ::vector cast --------------------

    #[test]
    fn inner_product_binop_parses() {
        let s = parse("SELECT v <#> [1.0, 2.0] FROM t");
        let Statement::Select(s) = s else { panic!() };
        let SelectItem::Expr { expr, .. } = &s.items[0] else {
            panic!()
        };
        assert!(matches!(
            expr,
            Expr::Binary {
                op: BinOp::InnerProduct,
                ..
            }
        ));
    }

    #[test]
    fn cosine_distance_binop_parses() {
        let s = parse("SELECT v <=> [1.0, 2.0] FROM t");
        let Statement::Select(s) = s else { panic!() };
        let SelectItem::Expr { expr, .. } = &s.items[0] else {
            panic!()
        };
        assert!(matches!(
            expr,
            Expr::Binary {
                op: BinOp::CosineDistance,
                ..
            }
        ));
    }

    #[test]
    fn vector_cast_postfix_wraps_string_literal() {
        let s = parse("SELECT '[1,2,3]'::vector FROM t");
        let Statement::Select(s) = s else { panic!() };
        let SelectItem::Expr { expr, .. } = &s.items[0] else {
            panic!()
        };
        assert!(matches!(
            expr,
            Expr::Cast {
                target: CastTarget::Vector,
                ..
            }
        ));
    }

    #[test]
    fn unsupported_cast_target_errors() {
        // v7.37.5 ship triage promoted the parser to accept every
        // ident as a `CastTarget::Named(canonical)`; the engine
        // surfaces the "unsupported cast target" error at eval
        // time when `type_name_to_data_type` can't resolve it.
        // Parser-side error now requires a NON-ident after `::`
        // (e.g. a punctuation token).
        let err = parse_statement("SELECT 1::, FROM t").unwrap_err();
        assert!(err.message.contains("expected type ident after `::`"));
    }

    #[test]
    fn tx_statements_round_trip() {
        for q in ["BEGIN", "COMMIT", "ROLLBACK"] {
            let original = parse(q);
            let again = parse_statement(&original.to_string()).unwrap();
            assert_eq!(original, again);
        }
    }

    #[test]
    fn interval_text_parsing_units() {
        // v7.37.5 β — three-field shape `(months, days, micros)` so
        // `'1 day'` and `'24 hours'` no longer collide (PG parity).
        // Single unit.
        assert_eq!(parse_interval_text("1 day"), Some((0, 1, 0)));
        assert_eq!(
            parse_interval_text("24 hours"),
            Some((0, 0, 86_400_000_000))
        );
        assert_eq!(parse_interval_text("1 second"), Some((0, 0, 1_000_000)));
        assert_eq!(parse_interval_text("1 month"), Some((1, 0, 0)));
        assert_eq!(parse_interval_text("2 years"), Some((24, 0, 0)));
        assert_eq!(parse_interval_text("1 week"), Some((0, 7, 0)));
        // Compound spans accumulate per-dimension.
        assert_eq!(parse_interval_text("1 year 6 months"), Some((18, 0, 0)));
        assert_eq!(
            parse_interval_text("1 day 2 hours"),
            Some((0, 1, 7_200_000_000))
        );
        // Negative numbers carry through per-dimension.
        assert_eq!(parse_interval_text("-1 day"), Some((0, -1, 0)));
        // Bad shapes return None.
        assert_eq!(parse_interval_text(""), None);
        assert_eq!(parse_interval_text("garbage"), None);
        assert_eq!(parse_interval_text("1 fortnight"), None);
        assert_eq!(parse_interval_text("1"), None);
    }

    #[test]
    fn interval_literal_roundtrips_via_display() {
        let parsed = parse("SELECT INTERVAL '1 day 2 hours'");
        let s = parsed.to_string();
        // Display preserves the original text verbatim.
        assert!(s.contains("INTERVAL '1 day 2 hours'"), "got: {s}");
        // And re-parsing yields a structurally equal statement.
        let again = parse_statement(&s).unwrap();
        assert_eq!(parsed, again);
    }

    // ── v6.1.2: CREATE / DROP PUBLICATION ────────────────────

    #[test]
    fn parser_recognises_create_publication_bare() {
        let s = parse("CREATE PUBLICATION pub_a");
        let Statement::CreatePublication(p) = s else {
            panic!("expected CreatePublication, got {s:?}")
        };
        assert_eq!(p.name, "pub_a");
        assert_eq!(p.scope, PublicationScope::AllTables);
    }

    #[test]
    fn parser_recognises_create_publication_for_all_tables() {
        let s = parse("CREATE PUBLICATION pub_a FOR ALL TABLES");
        let Statement::CreatePublication(p) = s else {
            panic!("expected CreatePublication, got {s:?}")
        };
        assert_eq!(p.name, "pub_a");
        assert_eq!(p.scope, PublicationScope::AllTables);
    }

    #[test]
    fn parser_recognises_drop_publication() {
        let s = parse("DROP PUBLICATION pub_a");
        let Statement::DropPublication(name) = s else {
            panic!("expected DropPublication, got {s:?}")
        };
        assert_eq!(name, "pub_a");
    }

    #[test]
    fn parser_recognises_for_table_list() {
        let s = parse("CREATE PUBLICATION pub_a FOR TABLE t1, t2, t3");
        let Statement::CreatePublication(p) = s else {
            panic!("expected CreatePublication, got {s:?}")
        };
        assert_eq!(p.name, "pub_a");
        let PublicationScope::ForTables(ts) = p.scope else {
            panic!("expected ForTables scope")
        };
        assert_eq!(ts, alloc::vec!["t1", "t2", "t3"]);
    }

    #[test]
    fn parser_recognises_for_tables_plural() {
        // PG 19 accepts both `FOR TABLE` and `FOR TABLES` — match.
        let s = parse("CREATE PUBLICATION pub_a FOR TABLES t1, t2");
        let Statement::CreatePublication(p) = s else {
            panic!("expected CreatePublication, got {s:?}")
        };
        let PublicationScope::ForTables(ts) = p.scope else {
            panic!("expected ForTables")
        };
        assert_eq!(ts, alloc::vec!["t1", "t2"]);
    }

    #[test]
    fn parser_recognises_for_all_tables_except_list() {
        let s = parse("CREATE PUBLICATION p FOR ALL TABLES EXCEPT t1, t2");
        let Statement::CreatePublication(p) = s else {
            panic!()
        };
        let PublicationScope::AllTablesExcept(ts) = p.scope else {
            panic!("expected AllTablesExcept")
        };
        assert_eq!(ts, alloc::vec!["t1", "t2"]);
    }

    #[test]
    fn parser_rejects_for_table_with_empty_list() {
        // `FOR TABLE` with nothing after is a parse error.
        let err = parse_statement("CREATE PUBLICATION p FOR TABLE")
            .expect_err("must error on empty list");
        // No specific message asserted — the call falls through to
        // expect_ident_like which yields "expected identifier, got …".
        assert!(!err.message.is_empty());
    }

    #[test]
    fn parser_recognises_show_publications() {
        // v6.1.3 — SHOW PUBLICATIONS lands here. PUBLICATIONS is a
        // bare ident in this position, NOT a reserved keyword.
        let s = parse("SHOW PUBLICATIONS");
        assert!(matches!(s, Statement::ShowPublications));
    }

    // ── v6.1.4: CREATE / DROP SUBSCRIPTION + SHOW SUBSCRIPTIONS ─

    #[test]
    fn parser_recognises_create_subscription_single_publication() {
        let s = parse(
            "CREATE SUBSCRIPTION sub_a CONNECTION 'host=127.0.0.1 port=20002' PUBLICATION pub_a",
        );
        let Statement::CreateSubscription(c) = s else {
            panic!("expected CreateSubscription, got {s:?}")
        };
        assert_eq!(c.name, "sub_a");
        assert_eq!(c.conn_str, "host=127.0.0.1 port=20002");
        assert_eq!(c.publications, alloc::vec!["pub_a"]);
    }

    #[test]
    fn parser_recognises_create_subscription_multi_publication() {
        let s = parse("CREATE SUBSCRIPTION sub_a CONNECTION 'host=h' PUBLICATION p1, p2, p3");
        let Statement::CreateSubscription(c) = s else {
            panic!()
        };
        assert_eq!(c.publications, alloc::vec!["p1", "p2", "p3"]);
    }

    #[test]
    fn parser_rejects_create_subscription_missing_connection() {
        let err = parse_statement("CREATE SUBSCRIPTION s PUBLICATION p")
            .expect_err("must error on missing CONNECTION");
        assert!(err.message.contains("CONNECTION"), "got: {}", err.message);
    }

    #[test]
    fn parser_rejects_create_subscription_missing_publication() {
        let err = parse_statement("CREATE SUBSCRIPTION s CONNECTION 'host=x'")
            .expect_err("must error on missing PUBLICATION");
        assert!(err.message.contains("PUBLICATION"), "got: {}", err.message);
    }

    #[test]
    fn parser_recognises_drop_subscription() {
        let s = parse("DROP SUBSCRIPTION sub_a");
        let Statement::DropSubscription(name) = s else {
            panic!("expected DropSubscription, got {s:?}")
        };
        assert_eq!(name, "sub_a");
    }

    #[test]
    fn parser_recognises_show_subscriptions() {
        let s = parse("SHOW SUBSCRIPTIONS");
        assert!(matches!(s, Statement::ShowSubscriptions));
    }

    #[test]
    fn parser_recognises_wait_for_wal_position_no_timeout() {
        let s = parse("WAIT FOR WAL POSITION 12345");
        let Statement::WaitForWalPosition { pos, timeout_ms } = s else {
            panic!("expected WaitForWalPosition, got {s:?}")
        };
        assert_eq!(pos, 12345);
        assert!(timeout_ms.is_none());
    }

    #[test]
    fn parser_recognises_wait_for_wal_position_with_timeout() {
        let s = parse("WAIT FOR WAL POSITION 67890 WITH TIMEOUT 5000");
        let Statement::WaitForWalPosition { pos, timeout_ms } = s else {
            panic!()
        };
        assert_eq!(pos, 67890);
        assert_eq!(timeout_ms, Some(5000));
    }

    #[test]
    fn parser_rejects_wait_with_negative_position() {
        // The lexer treats `-` as a token; `expect_u64_literal`
        // only sees the Integer that follows, so the negative
        // arrives as a unary-minus expression at higher levels.
        // Bare `WAIT FOR WAL POSITION -1` thus surfaces as a
        // parse error one way or another.
        let err = parse_statement("WAIT FOR WAL POSITION -1").unwrap_err();
        assert!(!err.message.is_empty());
    }

    #[test]
    fn parser_recognises_bare_analyze() {
        let s = parse("ANALYZE");
        assert!(matches!(s, Statement::Analyze(None)));
    }

    #[test]
    fn parser_recognises_analyze_with_table() {
        let s = parse("ANALYZE users");
        let Statement::Analyze(Some(name)) = s else {
            panic!("expected Analyze, got {s:?}")
        };
        assert_eq!(name, "users");
    }

    #[test]
    fn parser_recognises_analyze_with_quoted_table() {
        let s = parse("ANALYZE \"Mixed Case\"");
        let Statement::Analyze(Some(name)) = s else {
            panic!()
        };
        assert_eq!(name, "Mixed Case");
    }

    #[test]
    fn parser_rejects_analyze_with_garbage_token() {
        let err = parse_statement("ANALYZE 42").expect_err("must error");
        assert!(!err.message.is_empty());
    }

    #[test]
    fn analyze_display_roundtrips() {
        for sql in ["ANALYZE", "ANALYZE users"] {
            let s = parse(sql);
            let printed = s.to_string();
            let again = parse_statement(&printed)
                .unwrap_or_else(|e| panic!("re-parse failed for {printed:?}: {e}"));
            assert_eq!(s, again);
        }
    }

    #[test]
    fn wait_for_display_roundtrips() {
        for sql in [
            "WAIT FOR WAL POSITION 12345",
            "WAIT FOR WAL POSITION 67890 WITH TIMEOUT 5000",
        ] {
            let s = parse(sql);
            let printed = s.to_string();
            let again = parse_statement(&printed)
                .unwrap_or_else(|e| panic!("re-parse failed for {printed:?}: {e}"));
            assert_eq!(s, again, "round-trip mismatch for {sql:?}");
        }
    }

    #[test]
    fn subscription_ddl_display_roundtrips() {
        for sql in [
            "CREATE SUBSCRIPTION sub_a CONNECTION 'host=h port=20002' PUBLICATION pub_a",
            "CREATE SUBSCRIPTION sub_b CONNECTION 'host=h' PUBLICATION p1, p2",
            "DROP SUBSCRIPTION sub_a",
            "SHOW SUBSCRIPTIONS",
        ] {
            let s = parse(sql);
            let printed = s.to_string();
            let again = parse_statement(&printed)
                .unwrap_or_else(|e| panic!("re-parse failed for {printed:?}: {e}"));
            assert_eq!(s, again, "round-trip mismatch for {sql:?}");
        }
    }

    #[test]
    fn parser_drop_dispatches_user_vs_publication() {
        // Pre-v6.1.2 DROP USER took the bare-ident path; v6.1.2
        // tokenises DROP. Both targets must still parse.
        let s = parse("DROP USER 'alice'");
        let Statement::DropUser(name) = s else {
            panic!("expected DropUser, got {s:?}")
        };
        assert_eq!(name, "alice");
        // And DROP PUBLICATION lands the new variant.
        let s = parse("DROP PUBLICATION p1");
        assert!(matches!(s, Statement::DropPublication(_)));
    }

    #[test]
    fn publication_ddl_display_roundtrips() {
        // Every CREATE PUBLICATION variant must Display → parse →
        // same AST. v6.1.3 covers all three scope shapes.
        for sql in [
            "CREATE PUBLICATION pub_a",
            "CREATE PUBLICATION pub_a FOR ALL TABLES",
            "CREATE PUBLICATION pub_a FOR TABLE t1, t2",
            "CREATE PUBLICATION pub_a FOR ALL TABLES EXCEPT t1",
            "DROP PUBLICATION pub_a",
            "SHOW PUBLICATIONS",
        ] {
            let s = parse(sql);
            let printed = s.to_string();
            let again = parse_statement(&printed)
                .unwrap_or_else(|e| panic!("re-parse failed for {printed:?}: {e}"));
            assert_eq!(s, again, "round-trip mismatch for {sql:?}");
        }
    }

    // --- v7.12.4: CREATE FUNCTION + CREATE TRIGGER + PL/pgSQL ---

    #[test]
    fn create_function_returns_trigger_plpgsql_minimal() {
        let sql = "CREATE FUNCTION noop() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END; $$";
        let s = parse(sql);
        let Statement::CreateFunction(f) = s else {
            panic!("expected CreateFunction");
        };
        assert_eq!(f.name, "noop");
        assert!(!f.or_replace);
        assert!(f.args.is_empty());
        assert!(matches!(f.returns, FunctionReturn::Trigger));
        assert_eq!(f.language, "plpgsql");
        let FunctionBody::PlPgSql(block) = f.body else {
            panic!("expected PlPgSql body");
        };
        assert_eq!(block.statements.len(), 1);
        assert!(matches!(
            block.statements[0],
            PlPgSqlStmt::Return(ReturnTarget::New)
        ));
    }

    #[test]
    fn create_function_or_replace_with_assignment() {
        // mailrs-shape trigger function: NEW.col := to_tsvector(...);
        // RETURN NEW.
        let sql = "CREATE OR REPLACE FUNCTION update_sv() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
  NEW.search_vector := to_tsvector('english', NEW.subject);
  RETURN NEW;
END;
$$";
        let s = parse(sql);
        let Statement::CreateFunction(f) = s else {
            panic!("expected CreateFunction");
        };
        assert!(f.or_replace);
        let FunctionBody::PlPgSql(block) = &f.body else {
            panic!("expected PlPgSql body");
        };
        assert_eq!(block.statements.len(), 2);
        // First statement: NEW.search_vector := to_tsvector(...)
        let PlPgSqlStmt::Assign { target, .. } = &block.statements[0] else {
            panic!("expected Assign as first stmt");
        };
        match target {
            AssignTarget::NewColumn(c) => assert_eq!(c, "search_vector"),
            other => panic!("expected NEW.col, got {other:?}"),
        }
        // Second statement: RETURN NEW
        assert!(matches!(
            block.statements[1],
            PlPgSqlStmt::Return(ReturnTarget::New)
        ));
    }

    #[test]
    fn create_trigger_after_insert_or_update() {
        let sql = "CREATE TRIGGER tg AFTER INSERT OR UPDATE ON messages FOR EACH ROW EXECUTE FUNCTION update_sv()";
        let s = parse(sql);
        let Statement::CreateTrigger(t) = s else {
            panic!("expected CreateTrigger");
        };
        assert_eq!(t.name, "tg");
        assert_eq!(t.table, "messages");
        assert_eq!(t.timing, TriggerTiming::After);
        assert_eq!(t.events, vec![TriggerEvent::Insert, TriggerEvent::Update]);
        assert_eq!(t.for_each, TriggerForEach::Row);
        assert_eq!(t.function, "update_sv");
    }

    #[test]
    fn create_trigger_before_delete_execute_procedure_alias() {
        // PG also accepts the legacy `EXECUTE PROCEDURE` spelling.
        let sql =
            "CREATE TRIGGER guard BEFORE DELETE ON t FOR EACH ROW EXECUTE PROCEDURE block_delete()";
        let s = parse(sql);
        let Statement::CreateTrigger(t) = s else {
            panic!("expected CreateTrigger");
        };
        assert_eq!(t.timing, TriggerTiming::Before);
        assert_eq!(t.events, vec![TriggerEvent::Delete]);
    }

    #[test]
    fn drop_trigger_if_exists_round_trips() {
        // No parser support for DROP TRIGGER yet — added in v7.12.5
        // alongside the broader DROP …{IF EXISTS} cleanup. The
        // AST + Display impls are in place so we round-trip via
        // construction:
        let s = Statement::DropTrigger {
            name: "tg".into(),
            table: "messages".into(),
            if_exists: true,
        };
        assert_eq!(s.to_string(), "DROP TRIGGER IF EXISTS tg ON messages");
    }

    #[test]
    fn trigger_ddl_display_roundtrips_through_parser() {
        // CREATE TRIGGER + its referenced CREATE FUNCTION must
        // Display → parse → same AST (modulo PL/pgSQL body
        // formatting which is parser-canonicalised).
        for sql in [
            "CREATE TRIGGER tg AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()",
            "CREATE TRIGGER tg2 BEFORE UPDATE OR DELETE ON t FOR EACH ROW EXECUTE FUNCTION g()",
        ] {
            let s = parse(sql);
            let printed = s.to_string();
            let again = parse_statement(&printed)
                .unwrap_or_else(|e| panic!("re-parse failed for {printed:?}: {e}"));
            assert_eq!(s, again, "round-trip mismatch for {sql:?}");
        }
    }
}
