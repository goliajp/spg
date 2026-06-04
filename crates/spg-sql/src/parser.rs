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
    BinOp, CastTarget, ColumnDef, ColumnName, ColumnTypeName, CreateIndexStatement,
    CreatePublicationStatement, CreateSubscriptionStatement, CreateTableStatement, Expr,
    ExtractField, FkAction, ForeignKeyConstraint, FrameBound, FrameKind, FromClause, FromJoin,
    IndexMethod, InsertStatement, JoinKind, Literal, NullTreatment, OrderBy, PublicationScope,
    SelectItem, SelectStatement, Statement, TableRef, UnOp, UnionKind, VecEncoding, WindowFrame,
};
use crate::lexer::{self, LexError, Token};

/// v7.9.22 — recognise pgvector / SPG vector-index opclass names
/// in CREATE INDEX. SPG's HNSW already routes by query operator;
/// the opclass is accepted for `pg_dump` compatibility (mailrs
/// migration follow-up G5).
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
/// the token stream to end there.
pub fn parse_statement(input: &str) -> Result<Statement, ParseError> {
    let tokens = lexer::tokenize(input)?;
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
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
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

    fn expect_ident_like(&mut self) -> Result<String, ParseError> {
        match self.advance() {
            Token::Ident(s) | Token::QuotedIdent(s) => Ok(s),
            other => Err(ParseError {
                message: format!("expected identifier, got {other:?}"),
                token_pos: self.pos.saturating_sub(1),
            }),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn parse_one_statement(&mut self) -> Result<Statement, ParseError> {
        match self.peek() {
            Token::Select => self.parse_select_stmt(),
            // v7.9.27 — `DO $$ … $$ [LANGUAGE plpgsql]`. PG-only;
            // SPG has no PL/pgSQL so the body is consumed (lexer
            // already turned it into a Token::String) and the whole
            // DO statement returns CommandOk no-op. mailrs H1 +
            // pg_dump compat.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("do") => {
                self.advance();
                // Body — single string token (dollar-quoted or
                // ordinary).
                match self.advance() {
                    Token::String(_) => {}
                    other => {
                        return Err(self.err(alloc::format!(
                            "expected dollar-quoted body after DO, got {other:?}"
                        )));
                    }
                }
                // Optional `LANGUAGE <name>` trailer (idents only).
                if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("language")) {
                    self.advance();
                    let _ = self.expect_ident_like()?;
                }
                Ok(Statement::DoBlock)
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
                // v6.8.3 — `EXPLAIN (SUGGEST)` opt-in.
                if matches!(self.peek(), Token::LParen) {
                    self.advance();
                    let opt = match self.peek().clone() {
                        Token::Ident(s) | Token::QuotedIdent(s) => s,
                        other => {
                            return Err(self.err(format!(
                                "expected option keyword inside EXPLAIN (…), got {other:?}"
                            )));
                        }
                    };
                    if !opt.eq_ignore_ascii_case("suggest") {
                        return Err(self.err(format!(
                            "unknown EXPLAIN option {opt:?}; v6.8.3 supports SUGGEST"
                        )));
                    }
                    self.advance();
                    if !matches!(self.peek(), Token::RParen) {
                        return Err(self.err(format!(
                            "expected ')' after EXPLAIN option, got {:?}",
                            self.peek()
                        )));
                    }
                    self.advance();
                    suggest = true;
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
                }))
            }
            Token::Create => self.parse_create_stmt(),
            Token::Insert => self.parse_insert_stmt(),
            Token::Begin => {
                self.advance();
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
                    other => Err(self.err(format!(
                        "unknown SHOW target {other:?}; supported: TABLES, COLUMNS, USERS, PUBLICATIONS"
                    ))),
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
                    other => Err(self.err(format!(
                        "expected USER / PUBLICATION / SUBSCRIPTION after DROP, got {other:?}"
                    ))),
                }
            }
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("update") => {
                self.advance();
                self.parse_update_after_keyword()
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
                // — accept and ignore.
                if matches!(self.peek(), Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("local") || s.eq_ignore_ascii_case("session"))
                {
                    self.advance();
                }
                let name = self.parse_set_param_name()?;
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
                            "expected `=` or TO after SET {name}, got {other:?}"
                        )));
                    }
                }
                let value = self.parse_set_value()?;
                Ok(Statement::SetParameter { name, value })
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
            other => Err(self.err(format!(
                "expected TABLE / INDEX / USER / EXTENSION / PUBLICATION / SUBSCRIPTION after CREATE, got {other:?}"
            ))),
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
    fn parse_publication_table_list(&mut self) -> Result<Vec<String>, ParseError> {
        let first = self.expect_ident_like()?;
        let mut out = alloc::vec![first];
        while matches!(self.peek(), Token::Comma) {
            self.advance();
            out.push(self.expect_ident_like()?);
        }
        Ok(out)
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
            other => Err(self.err(format!(
                "expected literal, identifier, or DEFAULT after `=` in SET, got {other:?}"
            ))),
        }
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
            table,
            where_,
            returning,
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
        match self.advance() {
            Token::Index => {}
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("index") => {}
            // v6.7.2 — ALTER TABLE t SET hot_tier_bytes = X
            Token::Table => return self.parse_alter_table_after_keyword(),
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("table") => {
                return self.parse_alter_table_after_keyword();
            }
            other => {
                return Err(self.err(format!(
                    "expected INDEX or TABLE after ALTER, got {other:?}"
                )));
            }
        }
        let name = self.expect_ident_like()?;
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
    fn parse_alter_table_after_keyword(&mut self) -> Result<Statement, ParseError> {
        let table_name = self.expect_ident_like()?;
        // v7.6.8 — dispatch on the next keyword: SET / ADD / DROP.
        // SET kept identical to v6.7.x. ADD / DROP CONSTRAINT routes
        // to FK installation / removal.
        match self.peek() {
            Token::Ident(s) if s.eq_ignore_ascii_case("set") => {
                self.advance();
                let setting = self.expect_ident_like()?;
                if !setting.eq_ignore_ascii_case("hot_tier_bytes") {
                    return Err(self.err(alloc::format!(
                        "ALTER TABLE SET: unknown setting {setting:?}; supported: hot_tier_bytes"
                    )));
                }
                if !matches!(self.peek(), Token::Eq) {
                    return Err(self.err(alloc::format!(
                        "expected '=' after hot_tier_bytes, got {:?}",
                        self.peek()
                    )));
                }
                self.advance();
                let n = self.expect_u64_literal()?;
                Ok(Statement::AlterTable(crate::ast::AlterTableStatement {
                    name: table_name,
                    target: crate::ast::AlterTableTarget::SetHotTierBytes(n),
                }))
            }
            Token::Ident(s) if s.eq_ignore_ascii_case("add") => {
                self.advance();
                // Optional `CONSTRAINT <name>` prefix, then the same
                // FK clause shape as table-level CREATE TABLE FK.
                let fk = self.parse_table_level_fk()?;
                Ok(Statement::AlterTable(crate::ast::AlterTableStatement {
                    name: table_name,
                    target: crate::ast::AlterTableTarget::AddForeignKey(fk),
                }))
            }
            Token::Drop => {
                self.advance();
                match self.advance() {
                    Token::Ident(s) if s.eq_ignore_ascii_case("constraint") => {}
                    other => {
                        return Err(self.err(alloc::format!(
                            "expected CONSTRAINT after DROP in ALTER TABLE, got {other:?}"
                        )));
                    }
                }
                let cname = self.expect_ident_like()?;
                Ok(Statement::AlterTable(crate::ast::AlterTableStatement {
                    name: table_name,
                    target: crate::ast::AlterTableTarget::DropForeignKey(cname),
                }))
            }
            other => Err(self.err(alloc::format!(
                "expected SET / ADD / DROP in ALTER TABLE, got {other:?}"
            ))),
        }
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
        // Caller dispatches on Token::Select; the inner helper handles
        // the rest. ORDER BY / LIMIT bind at this top level; UNION peers
        // get a fresh bare-select parse and may not have their own ORDER
        // BY / LIMIT.
        let mut head = self.parse_bare_select()?;
        while matches!(self.peek(), Token::Union) {
            self.advance();
            let kind = if matches!(self.peek(), Token::All) {
                self.advance();
                UnionKind::All
            } else {
                UnionKind::Distinct
            };
            let peer = self.parse_bare_select()?;
            head.unions.push((kind, peer));
        }
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
                keys.push(OrderBy { expr, desc });
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
            Some(self.parse_limit_expr("LIMIT")?)
        } else {
            None
        };
        head.offset = if matches!(self.peek(), Token::Offset) {
            self.advance();
            Some(self.parse_limit_expr("OFFSET")?)
        } else {
            None
        };
        Ok(Statement::Select(head))
    }

    /// v7.9.24 — accept `LIMIT <int>` or `LIMIT $N`. mailrs H2.
    /// Bind value gets resolved during prepared-statement Execute;
    /// the Pratt expression parser would over-accept here (e.g.
    /// `LIMIT 5 + 5`), so we narrowly accept only the two PG forms.
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
    fn parse_bare_select(&mut self) -> Result<SelectStatement, ParseError> {
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
        Ok(SelectStatement {
            ctes: Vec::new(),
            distinct,
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
        })
    }

    fn parse_create_table_stmt_after_create(&mut self) -> Result<Statement, ParseError> {
        // Caller already consumed CREATE; we're sitting on TABLE.
        debug_assert!(matches!(self.peek(), Token::Table));
        self.advance();
        let if_not_exists = self.consume_if_not_exists();
        let name = self.expect_ident_like()?;
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
            } else if self.peek_constraint_or_fk_start() {
                foreign_keys.push(self.parse_table_level_fk()?);
            } else {
                let (col, col_level_fk) = self.parse_column_def_with_fk()?;
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
        Ok(Statement::CreateTable(CreateTableStatement {
            name,
            columns,
            if_not_exists,
            foreign_keys,
            table_constraints,
        }))
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
    fn peek_table_level_unique_start(&self) -> bool {
        let cur = self.peek();
        let nxt = self.tokens.get(self.pos + 1);
        let is_unique = matches!(cur, Token::Ident(s) if s.eq_ignore_ascii_case("unique"));
        let is_lparen = matches!(nxt, Some(Token::LParen));
        is_unique && is_lparen
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
        let columns = self.parse_paren_ident_list("UNIQUE")?;
        Ok(crate::ast::TableConstraint::Unique {
            name: None,
            columns,
        })
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
        // v7.6.7 — accept and reject `[NOT] DEFERRABLE [INITIALLY
        // {DEFERRED | IMMEDIATE}]` so existing PG dumps don't fail
        // at parse time. SPG's single-writer model has no deferred
        // constraint window, so we surface this as a clean
        // unsupported-feature error rather than a syntax error.
        loop {
            if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("deferrable")) {
                return Err(self.err(
                    "DEFERRABLE constraints are not supported (SPG is single-writer; \
                     constraints are always evaluated immediately at commit)"
                        .into(),
                ));
            }
            if matches!(self.peek(), Token::Not) {
                let look = self.tokens.get(self.pos + 1);
                if matches!(look, Some(Token::Ident(s)) if s.eq_ignore_ascii_case("deferrable")) {
                    // NOT DEFERRABLE — accept as the SPG default
                    // and consume both tokens silently.
                    self.advance();
                    self.advance();
                    // Optional `INITIALLY IMMEDIATE` clause.
                    if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("initially"))
                    {
                        self.advance();
                        match self.advance() {
                            Token::Ident(s) if s.eq_ignore_ascii_case("immediate") => {}
                            other => {
                                return Err(self.err(format!(
                                    "expected IMMEDIATE after INITIALLY for NOT DEFERRABLE, \
                                     got {other:?}"
                                )));
                            }
                        }
                    }
                    continue;
                }
                break;
            }
            break;
        }
        // Optional `ON DELETE <action>` and `ON UPDATE <action>` in
        // either order, each at most once.
        let mut on_delete = FkAction::Restrict;
        let mut on_update = FkAction::Restrict;
        let mut seen_on_delete = false;
        let mut seen_on_update = false;
        loop {
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

    /// v7.9.14 — consume `ASC | DESC | NULLS FIRST | NULLS LAST`
    /// qualifiers after an index column ref. ASC / DESC are
    /// reserved tokens; NULLS / FIRST / LAST are bare idents.
    /// We accept and discard them since single-column BTree
    /// stores rows in natural key order today.
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
                // v7.9.26b — PG `pg_dump` emits `USING gin` /
                // `USING gist` / `USING spgist` / `USING hash` for
                // their built-in index AMs. SPG doesn't have a
                // matching implementation; degrade to BTree on the
                // leading column so the schema loads + the index
                // catalogue stays consistent. Operator pays the
                // planner cost only for the queries that would have
                // used the specialised AM.
                "gin" | "gist" | "spgist" | "hash" => IndexMethod::BTree,
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
                        "unknown index method {other:?}; supported: hnsw, btree, brin (gin/gist/spgist/hash accepted as BTree fallback)"
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
            // SPG's HNSW currently picks its distance metric from
            // the query's operator (`<->` / `<#>` / `<=>`), so the
            // opclass is informational — accepted and discarded.
            // Recognised opclasses: vector_cosine_ops, vector_l2_ops,
            // vector_ip_ops, halfvec_*_ops, sq8_*_ops.
            Token::Ident(s) | Token::QuotedIdent(s)
                if matches!(
                    self.tokens.get(self.pos + 1),
                    Some(Token::Ident(op) | Token::QuotedIdent(op))
                        if is_vector_opclass_name(op)
                ) =>
            {
                self.advance(); // column name
                self.advance(); // opclass ident — drop
                (s, None)
            }
            Token::Ident(_) | Token::QuotedIdent(_) => {
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

    fn parse_column_def(&mut self) -> Result<ColumnDef, ParseError> {
        let name = self.expect_ident_like()?;
        // Type keyword arrives as a bare Ident (we did not promote type names
        // to keyword tokens — see lexer rationale).
        let ty_ident = match self.advance() {
            Token::Ident(s) => s,
            other => {
                return Err(ParseError {
                    message: format!("expected column type, got {other:?}"),
                    token_pos: self.pos.saturating_sub(1),
                });
            }
        };
        // v7.9.6 — PG `SERIAL` / `BIGSERIAL` shorthand for
        // `INT/BIGINT NOT NULL AUTO_INCREMENT`. PG also defines
        // SMALLSERIAL → SMALLINT; we accept that too. The implicit
        // NOT NULL + AUTO_INCREMENT flags get baked in after the
        // type tag so the rest of the constraint-loop parser sees
        // them as if user-supplied (rejecting duplicates).
        let mut implied_auto_increment = false;
        let mut implied_not_null = false;
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
            "smallint" | "tinyint" => ColumnTypeName::SmallInt,
            // INTEGER is MySQL's spelling for INT; MEDIUMINT widens up.
            "int" | "integer" | "mediumint" => ColumnTypeName::Int,
            "bigint" => ColumnTypeName::BigInt,
            // DOUBLE / REAL are 64-bit IEEE — same as our FLOAT.
            "float" | "double" | "real" => ColumnTypeName::Float,
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
            "timestamp" | "datetime" => ColumnTypeName::Timestamp,
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
            // v7.12.0 — PG full-text search types. mailrs G-CRIT-3.
            // The actual `to_tsvector` / `@@` / `ts_rank` surface
            // arrives in v7.12.1+; the type itself loads here so
            // mailrs's `scripts/init-schema.sql` runs unmodified.
            "tsvector" => ColumnTypeName::TsVector,
            "tsquery" => ColumnTypeName::TsQuery,
            other => {
                return Err(ParseError {
                    message: format!("unsupported column type {other:?}"),
                    token_pos: self.pos.saturating_sub(1),
                });
            }
        };
        // MySQL's `UNSIGNED` modifier sits right after the type
        // keyword. SPG doesn't carry a separate unsigned variant —
        // accepting the keyword keeps existing schemas compatible
        // without changing semantics. Drop it silently.
        if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("unsigned")) {
            self.advance();
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
                other => {
                    return Err(self.err(alloc::format!(
                        "v7.11 supports TEXT[] / INT[] / BIGINT[] only; got {other:?}[]"
                    )));
                }
            };
        }
        // Column constraints: `DEFAULT <expr>`, `NOT NULL`, and the
        // MySQL-flavoured `AUTO_INCREMENT` may appear in any order;
        // each at most once.
        let mut default: Option<Expr> = None;
        let mut nullable = !implied_not_null;
        let mut nullability_seen = implied_not_null;
        let mut auto_increment = implied_auto_increment;
        let mut is_primary_key = false;
        loop {
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
            break;
        }
        Ok(ColumnDef {
            name,
            ty,
            nullable,
            default,
            auto_increment,
            is_primary_key,
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
        if !matches!(self.peek(), Token::Values) {
            return Err(self.err(format!(
                "expected VALUES after table name, got {:?}",
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
            table,
            columns,
            rows,
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

    fn parse_table_ref(&mut self) -> Result<TableRef, ParseError> {
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
            let alias_ident = self.parse_optional_alias();
            let name = alias_ident.clone().unwrap_or_else(|| "unnest".to_string());
            return Ok(TableRef {
                name,
                alias: alias_ident,
                as_of_segment: None,
                unnest_expr: Some(Box::new(expr)),
            });
        }
        let name = self.expect_ident_like()?;
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
        })
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
            let on = if matches!(self.peek(), Token::On) {
                self.advance();
                Some(self.parse_expr(0)?)
            } else if kind == JoinKind::Cross {
                None
            } else {
                return Err(self.err(format!(
                    "expected ON after {:?} JOIN, got {:?}",
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
        if let Token::Ident(_) | Token::QuotedIdent(_) = self.peek() {
            return self.expect_ident_like().ok();
        }
        None
    }

    /// Pratt loop. `min_prec` is the minimum binary-op precedence we'll accept.
    fn parse_expr(&mut self, min_prec: u8) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_unary()?;
        while let Some((op, prec)) = binop_from(self.peek()) {
            if prec < min_prec {
                break;
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
            // v4.10: EXISTS / NOT EXISTS. EXISTS isn't a reserved
            // token; we match on the bare ident. NOT is a token
            // (consumed in the comparison rung), but `EXISTS (...)`
            // at the top of an expression starts here.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("exists") => {
                self.parse_exists_atom(false)
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
            Token::Ident(s) | Token::QuotedIdent(s) => self.finish_ident_atom(s),
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
    fn finish_postfix_casts(&mut self, mut expr: Expr) -> Result<Expr, ParseError> {
        loop {
            if matches!(self.peek(), Token::DoubleColon) {
                self.advance();
                // v7.9.25 / v7.9.26 — broaden the postfix `::` cast
                // target set to include INTERVAL (reserved Token),
                // TIMESTAMPTZ, and PG catalog regtype / regclass.
                // mailrs follow-up H3a + H3b.
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
                        other => {
                            return Err(ParseError {
                                message: format!("unsupported cast target `::{other}`"),
                                token_pos: self.pos.saturating_sub(1),
                            });
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
                if !matches!(self.peek(), Token::Null) {
                    return Err(self.err(format!(
                        "expected NULL or DISTINCT after IS{}, got {:?}",
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
            if !matches!(self.peek(), Token::LParen) {
                return Err(self.err(format!(
                    "expected '(' after AS in WITH clause, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            if !matches!(self.peek(), Token::Select) {
                return Err(self.err(format!("WITH body must be a SELECT, got {:?}", self.peek())));
            }
            let inner = self.parse_select_stmt()?;
            if !matches!(self.peek(), Token::RParen) {
                return Err(self.err(format!(
                    "expected ')' after CTE body, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            let Statement::Select(body) = inner else {
                unreachable!("parse_select_stmt returns Select")
            };
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
        // The body SELECT follows. Must start with SELECT.
        if !matches!(self.peek(), Token::Select) {
            return Err(self.err(format!(
                "expected SELECT after WITH clause, got {:?}",
                self.peek()
            )));
        }
        let body_stmt = self.parse_select_stmt()?;
        let Statement::Select(mut body) = body_stmt else {
            unreachable!()
        };
        body.ctes = ctes;
        Ok(Statement::Select(body))
    }

    /// v4.10: parse `EXISTS (SELECT ...)`. Caller (`parse_atom`)
    /// already consumed the leading `EXISTS` ident via
    /// `self.advance()`.
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
        let target = Box::new(expr);
        let combined = if elements.is_empty() {
            Expr::Literal(Literal::Bool(false))
        } else {
            let mut iter = elements.into_iter();
            let first = iter.next().unwrap();
            let mut acc = Expr::Binary {
                lhs: target.clone(),
                op: BinOp::Eq,
                rhs: Box::new(first),
            };
            for elt in iter {
                acc = Expr::Binary {
                    lhs: Box::new(acc),
                    op: BinOp::Or,
                    rhs: Box::new(Expr::Binary {
                        lhs: target.clone(),
                        op: BinOp::Eq,
                        rhs: Box::new(elt),
                    }),
                };
            }
            acc
        };
        Ok(maybe_not(combined, negated))
    }

    /// Parse a pgvector array literal `[ x1, x2, ... ]`. The opening `[` is
    /// already consumed by the caller. Elements must be numeric literals
    /// (with optional unary `-`); any compound expression is rejected at
    /// parse time so the runtime never needs to evaluate inside a vector.
    /// `EXTRACT(<field> FROM <source>)`. The dispatching `parse_atom`
    /// has already consumed the `EXTRACT` token before calling us —
    /// we pick up at the opening `(`.
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
            other => {
                return Err(self.err(format!(
                    "unknown EXTRACT field {other:?}; \
                     supported: YEAR, MONTH, DAY, HOUR, MINUTE, SECOND, MICROSECOND"
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
        let (months, micros) = parse_interval_text(&text).ok_or_else(|| ParseError {
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

    /// No frame clause is supported.
    #[allow(clippy::type_complexity)] // (partitions, ordered-keys-with-desc) is the natural shape
    fn parse_over_clause(
        &mut self,
    ) -> Result<(Vec<Expr>, Vec<(Expr, bool)>, Option<WindowFrame>), ParseError> {
        if !matches!(self.peek(), Token::LParen) {
            return Err(self.err(format!("expected '(' after OVER, got {:?}", self.peek())));
        }
        self.advance();
        let mut partition_by = Vec::new();
        let mut order_by = Vec::new();
        // PARTITION BY ?
        if let Token::Ident(s) | Token::QuotedIdent(s) = self.peek()
            && s.eq_ignore_ascii_case("partition")
        {
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
                order_by.push((e, desc));
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
                // v4.12: COUNT(*) OVER (...) — same window tail.
                let null_treatment = self.parse_null_treatment_modifier();
                if let Token::Ident(s) | Token::QuotedIdent(s) = self.peek()
                    && s.eq_ignore_ascii_case("over")
                {
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
                return Ok(Expr::FunctionCall {
                    name: "count_star".into(),
                    args: Vec::new(),
                });
            }
            // Function call. PG-style: zero-or-more comma-separated args.
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
                            return Err(self.err(format!(
                                "expected ',' or ')' in function args, got {other:?}"
                            )));
                        }
                    }
                }
            }
            self.advance(); // consume ')'
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
            "current_date" | "current_time" | "current_timestamp" | "localtimestamp" | "localtime"
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
        Token::Star => (BinOp::Mul, 7),
        Token::Slash => (BinOp::Div, 7),
        // v4.14: JSON path ops bind tighter than comparisons (4)
        // and additive (6) so `doc->'k' = 'v'` parses correctly.
        // Same rung as the multiplicative ops.
        Token::JsonGet => (BinOp::JsonGet, 7),
        Token::JsonGetText => (BinOp::JsonGetText, 7),
        Token::JsonGetPath => (BinOp::JsonGetPath, 7),
        Token::JsonGetPathText => (BinOp::JsonGetPathText, 7),
        Token::JsonContains => (BinOp::JsonContains, 7),
        // v7.12.2 — `@@` binds at the comparison rung (looser than
        // arithmetic, tighter than AND / OR). PG places `@@` at
        // the same precedence as `=` / `<`, so we follow.
        Token::TsMatch => (BinOp::TsMatch, 4),
        _ => return None,
    };
    Some(pair)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
// `as f32` here is intentional: vector elements widen / narrow into f32 on
// purpose. i64 → f32 loses precision past 2^24, f64 → f32 loses precision
// past ~15 decimal digits — both are acceptable for a fixed-precision
// pgvector column.
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
pub fn parse_interval_text(s: &str) -> Option<(i32, i64)> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.is_empty() || !parts.len().is_multiple_of(2) {
        return None;
    }
    let mut months: i32 = 0;
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
            "day" => micros = micros.checked_add(n.checked_mul(86_400_000_000)?)?,
            "week" => micros = micros.checked_add(n.checked_mul(604_800_000_000)?)?,
            "month" => {
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
    Some((months, micros))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    fn parse(s: &str) -> Statement {
        parse_statement(s).expect("parse ok")
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
        let err = parse_statement("").unwrap_err();
        assert!(err.message.contains("SELECT"));
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
        let err = parse_statement("CREATE TABLE t (v VECTOR(8) USING PQ8)").unwrap_err();
        assert!(
            err.message.contains("unknown vector encoding"),
            "got: {}",
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
    fn create_table_unknown_type_errors() {
        // v4.9: JSON is now real; pick an actually unsupported keyword
        // (XML never landed and isn't planned).
        let err = parse_statement("CREATE TABLE x (a xml)").unwrap_err();
        assert!(err.message.contains("unsupported column type"));
    }

    #[test]
    fn create_table_missing_table_keyword_errors() {
        assert!(parse_statement("CREATE x (a INT)").is_err());
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
        // `::numeric` isn't in the v1.3 cast target set.
        let err = parse_statement("SELECT 1::numeric FROM t").unwrap_err();
        assert!(err.message.contains("unsupported cast target"));
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
        // Single unit.
        assert_eq!(parse_interval_text("1 day"), Some((0, 86_400_000_000)));
        assert_eq!(parse_interval_text("1 second"), Some((0, 1_000_000)));
        assert_eq!(parse_interval_text("1 month"), Some((1, 0)));
        assert_eq!(parse_interval_text("2 years"), Some((24, 0)));
        // Compound spans accumulate.
        assert_eq!(parse_interval_text("1 year 6 months"), Some((18, 0)));
        assert_eq!(
            parse_interval_text("1 day 2 hours"),
            Some((0, 86_400_000_000 + 7_200_000_000))
        );
        // Negative numbers carry through.
        assert_eq!(parse_interval_text("-1 day"), Some((0, -86_400_000_000)));
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
}
