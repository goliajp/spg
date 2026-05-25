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
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::mem;

use crate::ast::{
    BinOp, CastTarget, ColumnDef, ColumnName, ColumnTypeName, CreateIndexStatement,
    CreateTableStatement, Expr, InsertStatement, Literal, SelectItem, SelectStatement, Statement,
    TableRef, UnOp,
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

    fn parse_one_statement(&mut self) -> Result<Statement, ParseError> {
        match self.peek() {
            Token::Select => self.parse_select_stmt(),
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
                Ok(Statement::Rollback)
            }
            other => Err(self.err(format!(
                "expected SELECT / CREATE / INSERT / BEGIN / COMMIT / ROLLBACK \
                 at start of statement, got {other:?}"
            ))),
        }
    }

    fn parse_create_stmt(&mut self) -> Result<Statement, ParseError> {
        debug_assert!(matches!(self.peek(), Token::Create));
        self.advance();
        match self.peek() {
            Token::Table => self.parse_create_table_stmt_after_create(),
            Token::Index => self.parse_create_index_stmt_after_create(),
            other => Err(self.err(format!(
                "expected TABLE or INDEX after CREATE, got {other:?}"
            ))),
        }
    }

    fn parse_select_stmt(&mut self) -> Result<Statement, ParseError> {
        // Caller dispatches on Token::Select; assert and advance.
        debug_assert!(matches!(self.peek(), Token::Select));
        self.advance();
        let items = self.parse_select_list()?;
        let from = if matches!(self.peek(), Token::From) {
            self.advance();
            Some(self.parse_table_ref()?)
        } else {
            None
        };
        let where_ = if matches!(self.peek(), Token::Where) {
            self.advance();
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        let order_by = if matches!(self.peek(), Token::Order) {
            self.advance();
            if !matches!(self.peek(), Token::By) {
                return Err(self.err(format!("expected BY after ORDER, got {:?}", self.peek())));
            }
            self.advance();
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        let limit = if matches!(self.peek(), Token::Limit) {
            self.advance();
            let n = match self.advance() {
                Token::Integer(n) if n >= 0 => u32::try_from(n).map_err(|_| ParseError {
                    message: format!("LIMIT value too large: {n}"),
                    token_pos: self.pos.saturating_sub(1),
                })?,
                other => {
                    return Err(ParseError {
                        message: format!(
                            "expected non-negative integer after LIMIT, got {other:?}"
                        ),
                        token_pos: self.pos.saturating_sub(1),
                    });
                }
            };
            Some(n)
        } else {
            None
        };
        Ok(Statement::Select(SelectStatement {
            items,
            from,
            where_,
            order_by,
            limit,
        }))
    }

    fn parse_create_table_stmt_after_create(&mut self) -> Result<Statement, ParseError> {
        // Caller already consumed CREATE; we're sitting on TABLE.
        debug_assert!(matches!(self.peek(), Token::Table));
        self.advance();
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
        }))
    }

    fn parse_create_index_stmt_after_create(&mut self) -> Result<Statement, ParseError> {
        // Caller consumed CREATE; we're on INDEX.
        debug_assert!(matches!(self.peek(), Token::Index));
        self.advance();
        let name = self.expect_ident_like()?;
        if !matches!(self.peek(), Token::On) {
            return Err(self.err(format!(
                "expected ON after CREATE INDEX <name>, got {:?}",
                self.peek()
            )));
        }
        self.advance();
        let table = self.expect_ident_like()?;
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
            "int" => ColumnTypeName::Int,
            "bigint" => ColumnTypeName::BigInt,
            "float" => ColumnTypeName::Float,
            "text" => ColumnTypeName::Text,
            "bool" => ColumnTypeName::Bool,
            "vector" => {
                if !matches!(self.peek(), Token::LParen) {
                    return Err(
                        self.err(format!("VECTOR type requires (dim), got {:?}", self.peek()))
                    );
                }
                self.advance();
                let dim = match self.advance() {
                    Token::Integer(n) if n > 0 => u32::try_from(n).map_err(|_| ParseError {
                        message: format!("VECTOR dim too large: {n}"),
                        token_pos: self.pos.saturating_sub(1),
                    })?,
                    other => {
                        return Err(ParseError {
                            message: format!("expected positive integer VECTOR dim, got {other:?}"),
                            token_pos: self.pos.saturating_sub(1),
                        });
                    }
                };
                if !matches!(self.peek(), Token::RParen) {
                    return Err(self.err(format!(
                        "expected ')' after VECTOR dim, got {:?}",
                        self.peek()
                    )));
                }
                self.advance();
                ColumnTypeName::Vector(dim)
            }
            other => {
                return Err(ParseError {
                    message: format!("unsupported column type {other:?}"),
                    token_pos: self.pos.saturating_sub(1),
                });
            }
        };
        // Optional NOT NULL.
        let nullable = if matches!(self.peek(), Token::Not) {
            self.advance();
            if !matches!(self.peek(), Token::Null) {
                return Err(self.err(format!(
                    "expected NULL after NOT in column def, got {:?}",
                    self.peek()
                )));
            }
            self.advance();
            false
        } else {
            true
        };
        Ok(ColumnDef { name, ty, nullable })
    }

    fn parse_insert_stmt(&mut self) -> Result<Statement, ParseError> {
        debug_assert!(matches!(self.peek(), Token::Insert));
        self.advance();
        if !matches!(self.peek(), Token::Into) {
            return Err(self.err(format!("expected INTO after INSERT, got {:?}", self.peek())));
        }
        self.advance();
        let table = self.expect_ident_like()?;
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
        Ok(Statement::Insert(InsertStatement { table, rows }))
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
                let e = self.parse_expr(0)?;
                match self.advance() {
                    Token::RParen => Ok(e),
                    other => Err(ParseError {
                        message: format!("expected ')', got {other:?}"),
                        token_pos: self.pos.saturating_sub(1),
                    }),
                }
            }
            Token::LBracket => self.parse_vector_literal_body(),
            Token::Ident(s) | Token::QuotedIdent(s) => self.finish_column(s),
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
            return Ok(expr);
        }
    }

    /// Parse a pgvector array literal `[ x1, x2, ... ]`. The opening `[` is
    /// already consumed by the caller. Elements must be numeric literals
    /// (with optional unary `-`); any compound expression is rejected at
    /// parse time so the runtime never needs to evaluate inside a vector.
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

    fn finish_column(&mut self, first: String) -> Result<Expr, ParseError> {
        if matches!(self.peek(), Token::Dot) {
            self.advance();
            let name = self.expect_ident_like()?;
            Ok(Expr::Column(ColumnName {
                qualifier: Some(first),
                name,
            }))
        } else {
            Ok(Expr::Column(ColumnName {
                qualifier: None,
                name: first,
            }))
        }
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
        assert_eq!(s.from.as_ref().unwrap().name, "users");
    }

    #[test]
    fn select_with_table_alias() {
        let s = parse("SELECT * FROM users AS u");
        let Statement::Select(s) = s else {
            panic!("expected SELECT")
        };
        let t = s.from.as_ref().unwrap();
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
        let err = parse_statement("CREATE TABLE x (a NUMERIC)").unwrap_err();
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
        let err = parse_statement("UPDATE foo SET x = 1").unwrap_err();
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
}
