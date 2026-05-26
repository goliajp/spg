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
use alloc::vec::Vec;
use core::fmt;
use core::mem;

use crate::ast::{
    BinOp, CastTarget, ColumnDef, ColumnName, ColumnTypeName, CreateIndexStatement,
    CreateTableStatement, Expr, ExtractField, FrameBound, FrameKind, FromClause, FromJoin,
    IndexMethod, InsertStatement, JoinKind, Literal, OrderBy, SelectItem, SelectStatement,
    Statement, TableRef, UnOp, UnionKind, WindowFrame,
};
use crate::lexer::{self, LexError, Token};

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
                if let Token::Ident(s) | Token::QuotedIdent(s) = self.peek()
                    && (s.eq_ignore_ascii_case("analyze") || s.eq_ignore_ascii_case("analyse"))
                {
                    self.advance();
                    analyze = true;
                }
                let inner = self.parse_select_stmt()?;
                let Statement::Select(s) = inner else {
                    return Err(self.err(format!(
                        "EXPLAIN body must be a SELECT, got {inner:?}"
                    )));
                };
                Ok(Statement::Explain(crate::ast::ExplainStatement {
                    analyze,
                    inner: Box::new(s),
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
                // `SHOW TABLES` and `SHOW COLUMNS FROM <table>`. Both
                // keywords (TABLES / COLUMNS) arrive as bare idents.
                let what = self.expect_ident_like()?;
                match what.to_ascii_lowercase().as_str() {
                    "tables" => Ok(Statement::ShowTables),
                    "users" => Ok(Statement::ShowUsers),
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
                        "unknown SHOW target {other:?}; supported: TABLES, COLUMNS, USERS"
                    ))),
                }
            }
            // v4.1: DROP USER 'name' / v4.4: DELETE FROM table / UPDATE
            // table SET. None of these leading keywords are reserved in
            // our lexer, so dispatch on the bare ident.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("drop") => {
                self.advance();
                self.expect_keyword_ident("user")?;
                let name = self.expect_ident_or_string()?;
                Ok(Statement::DropUser(name))
            }
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("update") => {
                self.advance();
                self.parse_update_after_keyword()
            }
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("delete") => {
                self.advance();
                self.parse_delete_after_keyword()
            }
            other => Err(self.err(format!(
                "expected SELECT / CREATE / DROP / INSERT / UPDATE / DELETE / BEGIN / COMMIT / \
                 ROLLBACK / SAVEPOINT / RELEASE / SHOW at start of statement, got {other:?}"
            ))),
        }
    }

    fn parse_create_stmt(&mut self) -> Result<Statement, ParseError> {
        debug_assert!(matches!(self.peek(), Token::Create));
        self.advance();
        match self.peek() {
            Token::Table => self.parse_create_table_stmt_after_create(),
            Token::Index => self.parse_create_index_stmt_after_create(),
            // v4.1: CREATE USER 'name' WITH PASSWORD 'pw' [ROLE 'role'].
            // USER isn't a reserved keyword — we look for the bare
            // identifier so the lexer doesn't have to grow a token.
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("user") => {
                self.advance();
                self.parse_create_user_after_keyword()
            }
            other => Err(self.err(format!(
                "expected TABLE / INDEX / USER after CREATE, got {other:?}"
            ))),
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
        Ok(Statement::Update(crate::ast::UpdateStatement {
            table,
            assignments,
            where_,
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
        Ok(Statement::Delete(crate::ast::DeleteStatement {
            table,
            where_,
        }))
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
            let expr = self.parse_expr(0)?;
            // ASC is the default; either keyword may follow the
            // order_by expression.
            let desc = if matches!(self.peek(), Token::Desc) {
                self.advance();
                true
            } else if matches!(self.peek(), Token::Asc) {
                self.advance();
                false
            } else {
                false
            };
            Some(OrderBy { expr, desc })
        } else {
            None
        };
        head.limit = if matches!(self.peek(), Token::Limit) {
            self.advance();
            let n = self.expect_u32_literal("LIMIT")?;
            Some(n)
        } else {
            None
        };
        head.offset = if matches!(self.peek(), Token::Offset) {
            self.advance();
            let n = self.expect_u32_literal("OFFSET")?;
            Some(n)
        } else {
            None
        };
        Ok(Statement::Select(head))
    }

    fn expect_u32_literal(&mut self, label: &str) -> Result<u32, ParseError> {
        match self.advance() {
            Token::Integer(n) if n >= 0 => u32::try_from(n).map_err(|_| ParseError {
                message: format!("{label} value too large: {n}"),
                token_pos: self.pos.saturating_sub(1),
            }),
            other => Err(ParseError {
                message: format!("expected non-negative integer after {label}, got {other:?}"),
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
        let group_by = if matches!(self.peek(), Token::Group) {
            self.advance();
            if !matches!(self.peek(), Token::By) {
                return Err(self.err(format!("expected BY after GROUP, got {:?}", self.peek())));
            }
            self.advance();
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
            having,
            unions: Vec::new(),
            order_by: None,
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
        loop {
            columns.push(self.parse_column_def()?);
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
        }))
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

    fn parse_create_index_stmt_after_create(&mut self) -> Result<Statement, ParseError> {
        // Caller consumed CREATE; we're on INDEX.
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
                other => {
                    return Err(self.err(alloc::format!(
                        "unknown index method {other:?}; supported: hnsw, btree"
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
        let column = self.expect_ident_like()?;
        if !matches!(self.peek(), Token::RParen) {
            return Err(self.err(format!(
                "expected ')' after indexed column, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        Ok(Statement::CreateIndex(CreateIndexStatement {
            name,
            table,
            column,
            method,
            if_not_exists,
        }))
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
        let ty = match ty_ident.as_str() {
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
            "vector" => ColumnTypeName::Vector(self.parse_paren_size("VECTOR")?),
            "numeric" => {
                let (precision, scale) = self.parse_optional_numeric_params()?;
                ColumnTypeName::Numeric(precision, scale)
            }
            "date" => ColumnTypeName::Date,
            // MySQL's `DATETIME` is the same domain as standard
            // `TIMESTAMP` — accept both spellings.
            "timestamp" | "datetime" => ColumnTypeName::Timestamp,
            // v4.9: JSON / JSONB. Stored as raw text — no parse-time
            // validation. We accept the JSONB spelling too because
            // most PG clients default to it; SPG doesn't distinguish
            // the two (no path-operator perf advantage to model).
            "json" | "jsonb" => ColumnTypeName::Json,
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
        // Column constraints: `DEFAULT <expr>`, `NOT NULL`, and the
        // MySQL-flavoured `AUTO_INCREMENT` may appear in any order;
        // each at most once.
        let mut default: Option<Expr> = None;
        let mut nullable = true;
        let mut nullability_seen = false;
        let mut auto_increment = false;
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
            break;
        }
        Ok(ColumnDef {
            name,
            ty,
            nullable,
            default,
            auto_increment,
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
        Ok(Statement::Insert(InsertStatement {
            table,
            columns,
            rows,
        }))
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
        let name = self.expect_ident_like()?;
        let alias = self.parse_optional_alias();
        Ok(TableRef { name, alias })
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
                let target = match self.advance() {
                    Token::Ident(s) => match s.as_str() {
                        "int" => CastTarget::Int,
                        "bigint" => CastTarget::BigInt,
                        "float" => CastTarget::Float,
                        "text" => CastTarget::Text,
                        "bool" => CastTarget::Bool,
                        "vector" => CastTarget::Vector,
                        "date" => CastTarget::Date,
                        "timestamp" | "datetime" => CastTarget::Timestamp,
                        other => {
                            return Err(ParseError {
                                message: format!("unsupported cast target `::{other}`"),
                                token_pos: self.pos.saturating_sub(1),
                            });
                        }
                    },
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
                if !matches!(self.peek(), Token::Null) {
                    return Err(self.err(format!(
                        "expected NULL after IS{}, got {:?}",
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
                return Err(self.err(format!(
                    "expected AND in frame spec, got {:?}",
                    self.peek()
                )));
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
                });
            }
            return Ok(Expr::FunctionCall { name: first, args });
        }
        Ok(Expr::Column(ColumnName {
            qualifier: None,
            name: first,
        }))
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
}
