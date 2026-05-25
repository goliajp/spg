//! AST for the PG-dialect subset SPG accepts in v0.2.
//!
//! `Display` is implemented so that for any AST `a` produced by [`crate::parser`],
//! re-parsing `format!("{a}")` yields a structurally equal AST. Binary and
//! unary operators always emit parentheses to remove any precedence
//! ambiguity — round-trip safety wins over prettiness.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Select(SelectStatement),
    CreateTable(CreateTableStatement),
    CreateIndex(CreateIndexStatement),
    Insert(InsertStatement),
    Begin,
    Commit,
    Rollback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateIndexStatement {
    pub name: String,
    pub table: String,
    pub column: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTableStatement {
    pub name: String,
    pub columns: Vec<ColumnDef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: ColumnTypeName,
    pub nullable: bool,
}

/// SQL-level type names. The mapping to the storage runtime's `DataType`
/// happens in `spg-engine` — keeping `spg-sql` free of storage deps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnTypeName {
    Int,
    BigInt,
    Float,
    Text,
    Bool,
    /// pgvector fixed-dimension `VECTOR(N)`.
    Vector(u32),
}

impl fmt::Display for ColumnTypeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int => f.write_str("INT"),
            Self::BigInt => f.write_str("BIGINT"),
            Self::Float => f.write_str("FLOAT"),
            Self::Text => f.write_str("TEXT"),
            Self::Bool => f.write_str("BOOL"),
            Self::Vector(n) => write!(f, "VECTOR({n})"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct InsertStatement {
    pub table: String,
    /// One or more `(expr, expr, ...)` tuples — the multi-row VALUES form.
    /// v1.3+ accepts `INSERT INTO t VALUES (a), (b)`.
    pub rows: Vec<Vec<Expr>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectStatement {
    pub items: Vec<SelectItem>,
    pub from: Option<TableRef>,
    pub where_: Option<Expr>,
    pub order_by: Option<Expr>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Wildcard,
    Expr { expr: Expr, alias: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableRef {
    pub name: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Literal),
    Column(ColumnName),
    Binary {
        lhs: Box<Expr>,
        op: BinOp,
        rhs: Box<Expr>,
    },
    Unary {
        op: UnOp,
        expr: Box<Expr>,
    },
    /// PG-style `expr::TYPE` cast. v1.3 supports VECTOR, INT, BIGINT, FLOAT,
    /// TEXT, BOOL targets; engine coerces at evaluation time.
    Cast {
        expr: Box<Expr>,
        target: CastTarget,
    },
    /// Postfix `IS NULL` / `IS NOT NULL`. Returns BOOL.
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    /// Function call `name(args...)`. v1.4 supports a small built-in set
    /// (length, upper, lower, abs, coalesce); unknown names error at eval
    /// time so the parser stays open for v1.5 aggregates.
    FunctionCall {
        name: String,
        args: Vec<Expr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastTarget {
    Int,
    BigInt,
    Float,
    Text,
    Bool,
    Vector,
}

impl fmt::Display for CastTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Int => "int",
            Self::BigInt => "bigint",
            Self::Float => "float",
            Self::Text => "text",
            Self::Bool => "bool",
            Self::Vector => "vector",
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Integer(i64),
    Float(f64),
    String(String),
    Bool(bool),
    Null,
    /// pgvector-style array literal, e.g. `[1, 2.5, -3]`.
    Vector(Vec<f32>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnName {
    pub qualifier: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Or,
    And,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Add,
    Sub,
    Mul,
    Div,
    /// pgvector L2 (Euclidean) distance `<->`. Defined for two vector
    /// operands of equal dimension; engine returns `Value::Float(d)`.
    L2Distance,
    /// pgvector inner-product `<#>` — returns `-Σ aᵢ bᵢ` so "smaller =
    /// more similar" remains true (matches pgvector's published convention).
    InnerProduct,
    /// pgvector cosine distance `<=>` — `1 - (a·b)/(|a| |b|)`.
    CosineDistance,
    /// SQL string concatenation `||`. NULL propagates.
    Concat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Not,
    Neg,
}

// --- Display impls (round-trip-safe) --------------------------------------

impl fmt::Display for Statement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Select(s) => s.fmt(f),
            Self::CreateTable(s) => s.fmt(f),
            Self::CreateIndex(s) => s.fmt(f),
            Self::Insert(s) => s.fmt(f),
            Self::Begin => f.write_str("BEGIN"),
            Self::Commit => f.write_str("COMMIT"),
            Self::Rollback => f.write_str("ROLLBACK"),
        }
    }
}

impl fmt::Display for CreateIndexStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CREATE INDEX {} ON {} ({})",
            quote_ident(&self.name),
            quote_ident(&self.table),
            quote_ident(&self.column),
        )
    }
}

impl fmt::Display for CreateTableStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CREATE TABLE {} (", quote_ident(&self.name))?;
        for (i, col) in self.columns.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{col}")?;
        }
        f.write_str(")")
    }
}

impl fmt::Display for ColumnDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", quote_ident(&self.name), self.ty)?;
        if !self.nullable {
            f.write_str(" NOT NULL")?;
        }
        Ok(())
    }
}

impl fmt::Display for InsertStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "INSERT INTO {} VALUES ", quote_ident(&self.table))?;
        for (ri, row) in self.rows.iter().enumerate() {
            if ri > 0 {
                f.write_str(", ")?;
            }
            f.write_str("(")?;
            for (i, v) in row.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{v}")?;
            }
            f.write_str(")")?;
        }
        Ok(())
    }
}

impl fmt::Display for SelectStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SELECT ")?;
        for (i, item) in self.items.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{item}")?;
        }
        if let Some(t) = &self.from {
            write!(f, " FROM {t}")?;
        }
        if let Some(e) = &self.where_ {
            write!(f, " WHERE {e}")?;
        }
        if let Some(e) = &self.order_by {
            write!(f, " ORDER BY {e}")?;
        }
        if let Some(n) = &self.limit {
            write!(f, " LIMIT {n}")?;
        }
        Ok(())
    }
}

impl fmt::Display for SelectItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wildcard => f.write_str("*"),
            Self::Expr { expr, alias } => {
                write!(f, "{expr}")?;
                if let Some(a) = alias {
                    write!(f, " AS {}", quote_ident(a))?;
                }
                Ok(())
            }
        }
    }
}

impl fmt::Display for TableRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", quote_ident(&self.name))?;
        if let Some(a) = &self.alias {
            write!(f, " AS {}", quote_ident(a))?;
        }
        Ok(())
    }
}

impl fmt::Display for ColumnName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(q) = &self.qualifier {
            write!(f, "{}.{}", quote_ident(q), quote_ident(&self.name))
        } else {
            write!(f, "{}", quote_ident(&self.name))
        }
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(l) => write!(f, "{l}"),
            Self::Column(c) => write!(f, "{c}"),
            Self::Binary { lhs, op, rhs } => write!(f, "({lhs} {op} {rhs})"),
            Self::Unary { op, expr } => match op {
                UnOp::Not => write!(f, "(NOT {expr})"),
                UnOp::Neg => write!(f, "(-{expr})"),
            },
            Self::Cast { expr, target } => write!(f, "({expr}::{target})"),
            Self::IsNull { expr, negated } => {
                if *negated {
                    write!(f, "({expr} IS NOT NULL)")
                } else {
                    write!(f, "({expr} IS NULL)")
                }
            }
            Self::FunctionCall { name, args } => {
                write!(f, "{name}(")?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{a}")?;
                }
                f.write_str(")")
            }
        }
    }
}

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer(n) => write!(f, "{n}"),
            Self::Float(x) => {
                let s = format!("{x}");
                // Default Display for an integral f64 (e.g. 1.0) emits "1",
                // which would round-trip back to Integer. Force a dot.
                if s.contains('.') || s.contains('e') || s.contains('E') {
                    f.write_str(&s)
                } else {
                    write!(f, "{s}.0")
                }
            }
            Self::String(s) => {
                f.write_str("'")?;
                for c in s.chars() {
                    if c == '\'' {
                        f.write_str("''")?;
                    } else {
                        write!(f, "{c}")?;
                    }
                }
                f.write_str("'")
            }
            Self::Bool(b) => f.write_str(if *b { "TRUE" } else { "FALSE" }),
            Self::Null => f.write_str("NULL"),
            Self::Vector(v) => {
                f.write_str("[")?;
                for (i, x) in v.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    let s = format!("{x}");
                    // Mirror Float Display: force a dot so re-parse stays
                    // numerically literal.
                    if s.contains('.') || s.contains('e') || s.contains('E') {
                        f.write_str(&s)?;
                    } else {
                        write!(f, "{s}.0")?;
                    }
                }
                f.write_str("]")
            }
        }
    }
}

impl fmt::Display for BinOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Or => "OR",
            Self::And => "AND",
            Self::Eq => "=",
            Self::NotEq => "<>",
            Self::Lt => "<",
            Self::LtEq => "<=",
            Self::Gt => ">",
            Self::GtEq => ">=",
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::L2Distance => "<->",
            Self::InnerProduct => "<#>",
            Self::CosineDistance => "<=>",
            Self::Concat => "||",
        })
    }
}

/// Quote `s` as a PG double-quoted identifier when required (keyword,
/// non-folded case, leading digit, embedded non-`[A-Za-z0-9_]`, empty).
/// Otherwise return it as-is. Returns an owned `String` to keep the call site
/// uniform.
fn quote_ident(s: &str) -> String {
    let needs_quote = match s.chars().next() {
        None => true,
        Some(c) if !c.is_ascii_alphabetic() && c != '_' => true,
        _ => {
            s.chars().any(|c| !(c.is_ascii_alphanumeric() || c == '_'))
                || s.chars().any(|c| c.is_ascii_uppercase())
                || is_keyword(s)
        }
    };
    if !needs_quote {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' {
            out.push_str("\"\"");
        } else {
            out.push(c);
        }
    }
    out.push('"');
    out
}

fn is_keyword(s: &str) -> bool {
    matches!(
        &*s.to_ascii_lowercase(),
        "select"
            | "from"
            | "where"
            | "as"
            | "null"
            | "true"
            | "false"
            | "and"
            | "or"
            | "not"
            | "create"
            | "table"
            | "insert"
            | "into"
            | "values"
            | "index"
            | "on"
            | "begin"
            | "commit"
            | "rollback"
            | "is"
            | "between"
            | "in"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn integer_literal_renders_without_dot() {
        assert_eq!(Literal::Integer(42).to_string(), "42");
    }

    #[test]
    fn integral_float_keeps_dot() {
        assert_eq!(Literal::Float(1.0).to_string(), "1.0");
        assert_eq!(Literal::Float(1.5).to_string(), "1.5");
        assert_eq!(Literal::Float(2.5e-3).to_string(), "0.0025");
    }

    #[test]
    fn string_literal_doubles_quote() {
        assert_eq!(Literal::String("it's".into()).to_string(), "'it''s'");
    }

    #[test]
    fn bool_and_null_render_uppercase() {
        assert_eq!(Literal::Bool(true).to_string(), "TRUE");
        assert_eq!(Literal::Bool(false).to_string(), "FALSE");
        assert_eq!(Literal::Null.to_string(), "NULL");
    }

    #[test]
    fn binary_op_always_parenthesised() {
        let e = Expr::Binary {
            lhs: Box::new(Expr::Literal(Literal::Integer(1))),
            op: BinOp::Add,
            rhs: Box::new(Expr::Literal(Literal::Integer(2))),
        };
        assert_eq!(e.to_string(), "(1 + 2)");
    }

    #[test]
    fn select_star_from_table() {
        let s = SelectStatement {
            items: vec![SelectItem::Wildcard],
            from: Some(TableRef {
                name: "users".into(),
                alias: None,
            }),
            where_: None,
            order_by: None,
            limit: None,
        };
        assert_eq!(s.to_string(), "SELECT * FROM users");
    }

    #[test]
    fn quote_ident_for_uppercase_and_keyword() {
        assert_eq!(quote_ident("foo"), "foo");
        assert_eq!(quote_ident("Foo"), "\"Foo\"");
        assert_eq!(quote_ident("select"), "\"select\"");
        assert_eq!(quote_ident(""), "\"\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }
}
