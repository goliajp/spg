//! 8.0.3 — the type a computed expression's VALUE will have, asked of the
//! evaluator that will produce it.
//!
//! Describe inferred a column's type by its own rules, and for a computed
//! expression those rules were a sketch: a binary operator took the type
//! of its LEFT operand, and a function the static table did not know fell
//! back to `text`. The value, meanwhile, came from the evaluator. A driver
//! decodes by the type Describe announced, so every disagreement was a
//! wrong answer on the wire — silently, in binary format:
//!
//! ```text
//!   psycopg 3, binary           PG 18.6          SPG 8.0.2
//!   SELECT 1 + 1.5              Decimal 2.5      int 131072
//!   SELECT date - date          int 1            date 2000-01-02
//!   SELECT 1 IS DISTINCT FROM 2 bool True        int 16777217
//!   SELECT sqrt(2)              float 1.414…     int 1073127582
//! ```
//!
//! and as a decode error in text format. A differential of 189 common
//! expressions against PG 18.6 found 57 whose Describe type differed.
//!
//! The fix does not add a second copy of the evaluator's typing rules.
//! It evaluates the expression once, on a row of representative values of
//! the columns' declared types, and takes the type of the result — so the
//! type Describe announces is, by construction, the type of the value the
//! wire will carry. A fresh evaluation context carries no sequence,
//! session or engine resolver, so nothing with a side effect can run
//! here: such a call errors and Describe keeps its previous answer.

extern crate alloc;

use alloc::vec::Vec;

use spg_sql::ast::Expr;
use spg_storage::{ColumnSchema, DataType, Row, Value};

use crate::eval;

/// How an output column gets its type.
#[derive(Clone, Copy)]
pub(crate) enum Typing {
    /// The typing rules alone. Every caller inside evaluation takes this:
    /// they run per row, and an evaluation from there would evaluate
    /// again for every row it types.
    Static,
    /// The rules, corrected by evaluating the expression once — only where
    /// a result's columns are decided, in the dialect that will run it.
    Evaluated(Dialect),
}

/// The dialect an expression is evaluated in. The parser admits some
/// constructs only in one of them (MySQL's `XOR`), and the evaluator
/// reads others differently (`1 AND 2`), so sampling in the wrong one is
/// not a smaller answer but a wrong one, or none.
#[derive(Clone, Copy)]
pub(crate) enum Dialect {
    Postgres,
    MySql,
}

impl Dialect {
    pub(crate) fn of_engine(speaks_mysql: bool) -> Self {
        if speaks_mysql {
            Self::MySql
        } else {
            Self::Postgres
        }
    }
}

/// The type `e` evaluates to, or `None` when it cannot be asked without
/// running something that is not an expression over one row.
pub(crate) fn sampled_type(
    e: &Expr,
    schema_cols: &[ColumnSchema],
    dialect: Dialect,
) -> Option<DataType> {
    if !sampleable(e) {
        return None;
    }
    let values: Vec<Value<'static>> = schema_cols
        .iter()
        .enumerate()
        .map(|(i, c)| sample_value(&c.ty, &c.name, i))
        .collect();
    let mut ctx = eval::EvalContext::new(schema_cols, None);
    ctx.mysql_dialect = matches!(dialect, Dialect::MySql);
    let v = eval::eval_expr(e, &Row::new(values), &ctx).ok()?;
    if v.is_null() {
        return None;
    }
    // A value whose type has no PostgreSQL OID tells a driver nothing it
    // can decode by; the static answer is better than `???`.
    v.data_type()
        .filter(|ty| crate::system_catalog::pg_type_oid(*ty) != 0)
}

/// Whether two declared types are carried by the same value encoding, so
/// that a value of one decodes correctly under the other's OID family.
pub(crate) fn same_encoding(a: &DataType, b: &DataType) -> bool {
    use DataType as D;
    let family = |t: &DataType| -> u8 {
        match t {
            D::Timestamp | D::Timestamptz => 1,
            D::Json | D::Jsonb => 2,
            D::Bit(_) | D::BitVarying(_) => 3,
            D::Text | D::Varchar(_) | D::Char(_) | D::Name => 4,
            _ => 0,
        }
    };
    a == b || (family(a) != 0 && family(a) == family(b))
}

/// Only an expression over the current row. An aggregate is excluded
/// because a single-row evaluation of it can SUCCEED with the wrong answer;
/// a subquery because it needs the engine. A window call or a bound
/// parameter simply fails to evaluate here, and Describe falls back.
fn sampleable(e: &Expr) -> bool {
    !crate::subquery::expr_has_subquery(e) && !crate::aggregate::contains_aggregate(e)
}

/// A representative non-NULL value of a declared column type, made through
/// the same coercion an INSERT uses. `Null` when the type has no sample
/// here, which makes an expression reading that column fall back.
fn sample_value(ty: &DataType, name: &str, position: usize) -> Value<'static> {
    let text = match ty {
        DataType::SmallInt | DataType::Int | DataType::BigInt | DataType::Oid => "1",
        DataType::Float | DataType::Real | DataType::Numeric { .. } => "1.5",
        DataType::Bool => "true",
        DataType::Text | DataType::Varchar(_) | DataType::Char(_) | DataType::Name => "a",
        DataType::Date => "2026-01-01",
        DataType::Timestamp => "2026-01-01 00:00:00",
        DataType::Timestamptz => "2026-01-01 00:00:00+00",
        DataType::Interval => "1 day",
        DataType::Json | DataType::Jsonb => "{\"a\": 1}",
        DataType::Bytes => "\\x61",
        _ => return Value::Null,
    };
    crate::conversions::coerce_value(Value::text(text), *ty, name, position).unwrap_or(Value::Null)
}
