//! Expression evaluator. Given a parsed `Expr`, a `Row`, and the row's column
//! schema, produce a `Value`. v0.4 implements:
//!
//! - literals
//! - column lookups (bare and qualified `t.col`)
//! - unary minus / NOT
//! - binary arithmetic, comparison, AND, OR
//! - numeric widening (`Int → BigInt → Float`) at evaluation time
//! - SQL three-valued logic for NULL:
//!     * any arithmetic / comparison op with a NULL operand → NULL
//!     * `TRUE OR NULL` → TRUE, `FALSE OR NULL` → NULL,
//!     * `FALSE AND NULL` → FALSE, `TRUE AND NULL` → NULL,
//!     * `NOT NULL` → NULL
//!
//! v0.4 deliberately does *not* implement: function calls, string
//! concatenation, IS NULL / IS NOT NULL, BETWEEN, IN, etc. Those come later.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use spg_sql::ast::{BinOp, ColumnName, Expr, Literal, UnOp};
use spg_storage::{ColumnSchema, DataType, Row, Value};

/// Resolution context for evaluating a single row. `table_alias` is the alias
/// (or table name) callers should accept as the qualifier on a column ref —
/// e.g. `FROM users AS u` makes `u.name` valid and rejects `other.name`.
#[derive(Debug, Clone)]
pub struct EvalContext<'a> {
    pub columns: &'a [ColumnSchema],
    pub table_alias: Option<&'a str>,
}

impl<'a> EvalContext<'a> {
    pub const fn new(columns: &'a [ColumnSchema], table_alias: Option<&'a str>) -> Self {
        Self {
            columns,
            table_alias,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum EvalError {
    ColumnNotFound { name: String },
    UnknownQualifier { qualifier: String },
    DivisionByZero,
    TypeMismatch { detail: String },
}

impl core::fmt::Display for EvalError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ColumnNotFound { name } => write!(f, "column not found: {name}"),
            Self::UnknownQualifier { qualifier } => {
                write!(f, "unknown table qualifier: {qualifier}")
            }
            Self::DivisionByZero => f.write_str("division by zero"),
            Self::TypeMismatch { detail } => write!(f, "type mismatch: {detail}"),
        }
    }
}

pub fn eval_expr(expr: &Expr, row: &Row, ctx: &EvalContext<'_>) -> Result<Value, EvalError> {
    match expr {
        Expr::Literal(l) => Ok(literal_to_value(l)),
        Expr::Column(c) => resolve_column(c, row, ctx),
        Expr::Unary { op, expr } => {
            let v = eval_expr(expr, row, ctx)?;
            apply_unary(*op, v)
        }
        Expr::Binary { lhs, op, rhs } => {
            let l = eval_expr(lhs, row, ctx)?;
            let r = eval_expr(rhs, row, ctx)?;
            apply_binary(*op, l, r)
        }
        Expr::VectorCast(inner) => {
            let v = eval_expr(inner, row, ctx)?;
            cast_to_vector(v)
        }
    }
}

/// Parse a `Value::Text("[1.0, 2.0, 3.0]")` into a `Value::Vector(..)`. Mirrors
/// pgvector's `'[..]'::vector` cast. NULL casts as NULL.
pub fn cast_to_vector(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::Null => Ok(Value::Null),
        Value::Vector(v) => Ok(Value::Vector(v)),
        Value::Text(s) => parse_vector_text(&s)
            .map(Value::Vector)
            .ok_or(EvalError::TypeMismatch {
                detail: format!("cannot parse {s:?} as a vector literal"),
            }),
        other => Err(EvalError::TypeMismatch {
            detail: format!("::vector requires text input, got {:?}", other.data_type()),
        }),
    }
}

/// Parse `"[1.0, 2.0, -3]"` into `Vec<f32>`. Returns `None` on malformed input.
fn parse_vector_text(s: &str) -> Option<Vec<f32>> {
    let trimmed = s.trim();
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    let trimmed_inner = inner.trim();
    if trimmed_inner.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for part in trimmed_inner.split(',') {
        let f: f32 = part.trim().parse().ok()?;
        out.push(f);
    }
    Some(out)
}

fn literal_to_value(l: &Literal) -> Value {
    match l {
        Literal::Integer(n) => {
            if let Ok(small) = i32::try_from(*n) {
                Value::Int(small)
            } else {
                Value::BigInt(*n)
            }
        }
        Literal::Float(x) => Value::Float(*x),
        Literal::String(s) => Value::Text(s.clone()),
        Literal::Vector(v) => Value::Vector(v.clone()),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Null => Value::Null,
    }
}

fn resolve_column(c: &ColumnName, row: &Row, ctx: &EvalContext<'_>) -> Result<Value, EvalError> {
    if let Some(q) = &c.qualifier {
        let expected = ctx.table_alias.ok_or_else(|| EvalError::UnknownQualifier {
            qualifier: q.clone(),
        })?;
        if q != expected {
            return Err(EvalError::UnknownQualifier {
                qualifier: q.clone(),
            });
        }
    }
    let pos = ctx
        .columns
        .iter()
        .position(|s| s.name == c.name)
        .ok_or_else(|| EvalError::ColumnNotFound {
            name: c.name.clone(),
        })?;
    Ok(row.values[pos].clone())
}

fn apply_unary(op: UnOp, v: Value) -> Result<Value, EvalError> {
    match (op, v) {
        (_, Value::Null) => Ok(Value::Null),
        (UnOp::Neg, Value::Int(n)) => {
            n.checked_neg()
                .map(Value::Int)
                .ok_or(EvalError::TypeMismatch {
                    detail: "integer overflow on unary -".into(),
                })
        }
        (UnOp::Neg, Value::BigInt(n)) => {
            n.checked_neg()
                .map(Value::BigInt)
                .ok_or(EvalError::TypeMismatch {
                    detail: "bigint overflow on unary -".into(),
                })
        }
        (UnOp::Neg, Value::Float(x)) => Ok(Value::Float(-x)),
        (UnOp::Neg, other) => Err(EvalError::TypeMismatch {
            detail: format!("unary - applied to {:?}", other.data_type()),
        }),
        (UnOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
        (UnOp::Not, other) => Err(EvalError::TypeMismatch {
            detail: format!("NOT applied to {:?}", other.data_type()),
        }),
    }
}

fn apply_binary(op: BinOp, l: Value, r: Value) -> Result<Value, EvalError> {
    // SQL three-valued logic for AND / OR with NULL is special — handle before
    // the general NULL-propagation rule.
    if let BinOp::And = op {
        return and_3vl(l, r);
    }
    if let BinOp::Or = op {
        return or_3vl(l, r);
    }
    // Everything else: any NULL operand → NULL.
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    match op {
        BinOp::Add => arith(l, r, i64::checked_add, |a, b| a + b, "+"),
        BinOp::Sub => arith(l, r, i64::checked_sub, |a, b| a - b, "-"),
        BinOp::Mul => arith(l, r, i64::checked_mul, |a, b| a * b, "*"),
        BinOp::Div => div_op(l, r),
        BinOp::L2Distance => l2_distance(l, r),
        BinOp::InnerProduct => inner_product(l, r),
        BinOp::CosineDistance => cosine_distance(l, r),
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => {
            compare(op, &l, &r)
        }
        BinOp::And | BinOp::Or => unreachable!("handled above"),
    }
}

/// pgvector inner-product `<#>`. Returns the *negative* dot product so
/// smaller still means more similar — same convention as pgvector.
fn inner_product(l: Value, r: Value) -> Result<Value, EvalError> {
    let (a, b) = unwrap_vec_pair(l, r, "<#>")?;
    let mut dot: f64 = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += f64::from(*x) * f64::from(*y);
    }
    Ok(Value::Float(-dot))
}

/// pgvector cosine distance `<=>` — `1 - (a·b) / (‖a‖ ‖b‖)`. A zero-norm
/// operand produces NaN (matches pgvector).
fn cosine_distance(l: Value, r: Value) -> Result<Value, EvalError> {
    let (a, b) = unwrap_vec_pair(l, r, "<=>")?;
    let mut dot: f64 = 0.0;
    let mut na: f64 = 0.0;
    let mut nb: f64 = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        let xf = f64::from(*x);
        let yf = f64::from(*y);
        dot += xf * yf;
        na += xf * xf;
        nb += yf * yf;
    }
    let denom = sqrt_newton(na) * sqrt_newton(nb);
    if denom == 0.0 {
        return Ok(Value::Float(f64::NAN));
    }
    Ok(Value::Float(1.0 - dot / denom))
}

fn unwrap_vec_pair(l: Value, r: Value, op: &str) -> Result<(Vec<f32>, Vec<f32>), EvalError> {
    match (l, r) {
        (Value::Vector(a), Value::Vector(b)) => {
            if a.len() != b.len() {
                return Err(EvalError::TypeMismatch {
                    detail: format!("vector dim mismatch in {op}: {} vs {}", a.len(), b.len()),
                });
            }
            Ok((a, b))
        }
        (a, b) => Err(EvalError::TypeMismatch {
            detail: format!(
                "{op} requires two vectors, got {:?} and {:?}",
                a.data_type(),
                b.data_type()
            ),
        }),
    }
}

/// Numeric arithmetic with widening.
/// - both `Int` → `Int` (with overflow check)
/// - `Int` op `BigInt` (either side) → `BigInt`
/// - any `Float` involved → `Float`
fn arith(
    l: Value,
    r: Value,
    int_op: impl Fn(i64, i64) -> Option<i64>,
    float_op: impl Fn(f64, f64) -> f64,
    op_name: &str,
) -> Result<Value, EvalError> {
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => {
            let result = int_op(i64::from(a), i64::from(b)).ok_or(EvalError::TypeMismatch {
                detail: format!("integer overflow on {op_name}"),
            })?;
            if let Ok(small) = i32::try_from(result) {
                Ok(Value::Int(small))
            } else {
                Ok(Value::BigInt(result))
            }
        }
        (Value::Int(a), Value::BigInt(b)) | (Value::BigInt(b), Value::Int(a)) => {
            let result = int_op(i64::from(a), b).ok_or(EvalError::TypeMismatch {
                detail: format!("bigint overflow on {op_name}"),
            })?;
            Ok(Value::BigInt(result))
        }
        (Value::BigInt(a), Value::BigInt(b)) => {
            let result = int_op(a, b).ok_or(EvalError::TypeMismatch {
                detail: format!("bigint overflow on {op_name}"),
            })?;
            Ok(Value::BigInt(result))
        }
        (a, b)
            if a.data_type() == Some(DataType::Float) || b.data_type() == Some(DataType::Float) =>
        {
            let af = as_f64(&a)?;
            let bf = as_f64(&b)?;
            Ok(Value::Float(float_op(af, bf)))
        }
        (a, b) => Err(EvalError::TypeMismatch {
            detail: format!(
                "{op_name} applied to non-numeric: {:?} vs {:?}",
                a.data_type(),
                b.data_type()
            ),
        }),
    }
}

/// L2 (Euclidean) distance between two vectors of equal dimension.
/// Returned as `Value::Float(d)` so it composes with the existing
/// comparison / sort plumbing. Mismatched dims or non-vector operands
/// raise `TypeMismatch`.
#[allow(clippy::many_single_char_names)] // l, r, a, b, d are the natural names
fn l2_distance(l: Value, r: Value) -> Result<Value, EvalError> {
    match (l, r) {
        (Value::Vector(a), Value::Vector(b)) => {
            if a.len() != b.len() {
                return Err(EvalError::TypeMismatch {
                    detail: format!("vector dim mismatch in <->: {} vs {}", a.len(), b.len()),
                });
            }
            let mut sum: f64 = 0.0;
            for (x, y) in a.iter().zip(b.iter()) {
                let d = f64::from(*x) - f64::from(*y);
                sum += d * d;
            }
            Ok(Value::Float(sqrt_newton(sum)))
        }
        (a, b) => Err(EvalError::TypeMismatch {
            detail: format!(
                "<-> requires two vectors, got {:?} and {:?}",
                a.data_type(),
                b.data_type()
            ),
        }),
    }
}

/// Self-built `sqrt` for `f64` — `std::f64::sqrt` lives in `std`, which the
/// engine's `no_std` constraint disallows. Newton-Raphson with a few rounds
/// reaches IEEE-754 precision for the inputs we'll see (sum of squares of
/// f32-derived distances, always non-negative, never NaN).
fn sqrt_newton(x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    let mut g = x;
    // 10 iterations is conservative; 6 already converges to ulp for typical
    // distances.
    for _ in 0..10 {
        g = 0.5 * (g + x / g);
    }
    g
}

fn div_op(l: Value, r: Value) -> Result<Value, EvalError> {
    let any_float = matches!(l.data_type(), Some(DataType::Float))
        || matches!(r.data_type(), Some(DataType::Float));
    if any_float {
        let a = as_f64(&l)?;
        let b = as_f64(&r)?;
        if b == 0.0 {
            return Err(EvalError::DivisionByZero);
        }
        return Ok(Value::Float(a / b));
    }
    arith(
        l,
        r,
        |a, b| {
            if b == 0 { None } else { Some(a / b) }
        },
        |a, b| a / b,
        "/",
    )
    .map_err(|e| match e {
        // The closure returns None on b == 0; translate that into the dedicated
        // DivisionByZero variant instead of "integer overflow on /".
        EvalError::TypeMismatch { detail } if detail.contains('/') => EvalError::DivisionByZero,
        other => other,
    })
}

fn as_f64(v: &Value) -> Result<f64, EvalError> {
    match v {
        Value::Int(n) => Ok(f64::from(*n)),
        #[allow(clippy::cast_precision_loss)]
        Value::BigInt(n) => Ok(*n as f64),
        Value::Float(x) => Ok(*x),
        other => Err(EvalError::TypeMismatch {
            detail: format!("cannot convert {:?} to FLOAT", other.data_type()),
        }),
    }
}

fn compare(op: BinOp, l: &Value, r: &Value) -> Result<Value, EvalError> {
    let ord = match (l, r) {
        (Value::Int(a), Value::Int(b)) => i64::from(*a).cmp(&i64::from(*b)),
        (Value::Int(a), Value::BigInt(b)) => i64::from(*a).cmp(b),
        (Value::BigInt(a), Value::Int(b)) => a.cmp(&i64::from(*b)),
        (Value::BigInt(a), Value::BigInt(b)) => a.cmp(b),
        (a, b)
            if matches!(a.data_type(), Some(DataType::Float))
                || matches!(b.data_type(), Some(DataType::Float)) =>
        {
            let af = as_f64(a)?;
            let bf = as_f64(b)?;
            af.partial_cmp(&bf).ok_or(EvalError::TypeMismatch {
                detail: "NaN in comparison".into(),
            })?
        }
        (Value::Text(a), Value::Text(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (a, b) => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "comparison between {:?} and {:?}",
                    a.data_type(),
                    b.data_type()
                ),
            });
        }
    };
    let result = match op {
        BinOp::Eq => ord.is_eq(),
        BinOp::NotEq => !ord.is_eq(),
        BinOp::Lt => ord.is_lt(),
        BinOp::LtEq => ord.is_le(),
        BinOp::Gt => ord.is_gt(),
        BinOp::GtEq => ord.is_ge(),
        BinOp::And
        | BinOp::Or
        | BinOp::Add
        | BinOp::Sub
        | BinOp::Mul
        | BinOp::Div
        | BinOp::L2Distance
        | BinOp::InnerProduct
        | BinOp::CosineDistance => {
            unreachable!("compare() only called with comparison ops")
        }
    };
    Ok(Value::Bool(result))
}

// SQL three-valued AND / OR.
fn and_3vl(l: Value, r: Value) -> Result<Value, EvalError> {
    match (l, r) {
        (Value::Bool(false), _) | (_, Value::Bool(false)) => Ok(Value::Bool(false)),
        (Value::Bool(true), Value::Bool(true)) => Ok(Value::Bool(true)),
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (a, b) => Err(EvalError::TypeMismatch {
            detail: format!(
                "AND on non-boolean: {:?} and {:?}",
                a.data_type(),
                b.data_type()
            ),
        }),
    }
}

fn or_3vl(l: Value, r: Value) -> Result<Value, EvalError> {
    match (l, r) {
        (Value::Bool(true), _) | (_, Value::Bool(true)) => Ok(Value::Bool(true)),
        (Value::Bool(false), Value::Bool(false)) => Ok(Value::Bool(false)),
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (a, b) => Err(EvalError::TypeMismatch {
            detail: format!(
                "OR on non-boolean: {:?} and {:?}",
                a.data_type(),
                b.data_type()
            ),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use spg_storage::{ColumnSchema, Row};

    fn col(name: &str, ty: DataType) -> ColumnSchema {
        ColumnSchema::new(name, ty, true)
    }

    fn ctx<'a>(cols: &'a [ColumnSchema], alias: Option<&'a str>) -> EvalContext<'a> {
        EvalContext::new(cols, alias)
    }

    fn lit(n: i64) -> Expr {
        Expr::Literal(Literal::Integer(n))
    }

    fn null() -> Expr {
        Expr::Literal(Literal::Null)
    }

    fn col_ref(name: &str) -> Expr {
        Expr::Column(ColumnName {
            qualifier: None,
            name: name.into(),
        })
    }

    #[test]
    fn literal_evaluates_to_value() {
        let r = Row::new(vec![]);
        let cs: [ColumnSchema; 0] = [];
        let c = ctx(&cs, None);
        assert_eq!(eval_expr(&lit(42), &r, &c).unwrap(), Value::Int(42));
        assert_eq!(
            eval_expr(&Expr::Literal(Literal::Float(1.5)), &r, &c).unwrap(),
            Value::Float(1.5)
        );
        assert_eq!(eval_expr(&null(), &r, &c).unwrap(), Value::Null);
    }

    #[test]
    fn column_lookup_unqualified() {
        let cs = vec![col("a", DataType::Int), col("b", DataType::Text)];
        let r = Row::new(vec![Value::Int(7), Value::Text("hi".into())]);
        let c = ctx(&cs, None);
        assert_eq!(eval_expr(&col_ref("a"), &r, &c).unwrap(), Value::Int(7));
        assert_eq!(
            eval_expr(&col_ref("b"), &r, &c).unwrap(),
            Value::Text("hi".into())
        );
    }

    #[test]
    fn column_not_found_errors() {
        let cs = vec![col("a", DataType::Int)];
        let r = Row::new(vec![Value::Int(0)]);
        let c = ctx(&cs, None);
        let err = eval_expr(&col_ref("ghost"), &r, &c).unwrap_err();
        assert!(matches!(err, EvalError::ColumnNotFound { ref name } if name == "ghost"));
    }

    #[test]
    fn qualified_column_matches_alias() {
        let cs = vec![col("a", DataType::Int)];
        let r = Row::new(vec![Value::Int(5)]);
        let c = ctx(&cs, Some("u"));
        let qualified = Expr::Column(ColumnName {
            qualifier: Some("u".into()),
            name: "a".into(),
        });
        assert_eq!(eval_expr(&qualified, &r, &c).unwrap(), Value::Int(5));
    }

    #[test]
    fn qualified_column_unknown_alias_errors() {
        let cs = vec![col("a", DataType::Int)];
        let r = Row::new(vec![Value::Int(5)]);
        let c = ctx(&cs, Some("u"));
        let wrong = Expr::Column(ColumnName {
            qualifier: Some("x".into()),
            name: "a".into(),
        });
        assert!(matches!(
            eval_expr(&wrong, &r, &c).unwrap_err(),
            EvalError::UnknownQualifier { .. }
        ));
    }

    #[test]
    fn arithmetic_with_widening() {
        let r = Row::new(vec![]);
        let cs: [ColumnSchema; 0] = [];
        let c = ctx(&cs, None);
        let e = Expr::Binary {
            lhs: alloc::boxed::Box::new(lit(2)),
            op: BinOp::Add,
            rhs: alloc::boxed::Box::new(Expr::Literal(Literal::Float(0.5))),
        };
        assert_eq!(eval_expr(&e, &r, &c).unwrap(), Value::Float(2.5));
    }

    #[test]
    fn division_by_zero_errors() {
        let r = Row::new(vec![]);
        let cs: [ColumnSchema; 0] = [];
        let c = ctx(&cs, None);
        let e = Expr::Binary {
            lhs: alloc::boxed::Box::new(lit(1)),
            op: BinOp::Div,
            rhs: alloc::boxed::Box::new(lit(0)),
        };
        assert_eq!(
            eval_expr(&e, &r, &c).unwrap_err(),
            EvalError::DivisionByZero
        );
    }

    #[test]
    fn comparison_returns_bool() {
        let r = Row::new(vec![]);
        let cs: [ColumnSchema; 0] = [];
        let c = ctx(&cs, None);
        let e = Expr::Binary {
            lhs: alloc::boxed::Box::new(lit(1)),
            op: BinOp::Lt,
            rhs: alloc::boxed::Box::new(lit(2)),
        };
        assert_eq!(eval_expr(&e, &r, &c).unwrap(), Value::Bool(true));
    }

    #[test]
    fn null_propagates_through_arithmetic() {
        let r = Row::new(vec![]);
        let cs: [ColumnSchema; 0] = [];
        let c = ctx(&cs, None);
        let e = Expr::Binary {
            lhs: alloc::boxed::Box::new(lit(1)),
            op: BinOp::Add,
            rhs: alloc::boxed::Box::new(null()),
        };
        assert_eq!(eval_expr(&e, &r, &c).unwrap(), Value::Null);
    }

    #[test]
    fn and_three_valued_logic() {
        let r = Row::new(vec![]);
        let cs: [ColumnSchema; 0] = [];
        let c = ctx(&cs, None);
        let tt = |a: bool, b_null: bool| Expr::Binary {
            lhs: alloc::boxed::Box::new(Expr::Literal(Literal::Bool(a))),
            op: BinOp::And,
            rhs: alloc::boxed::Box::new(if b_null {
                null()
            } else {
                Expr::Literal(Literal::Bool(true))
            }),
        };
        // FALSE AND NULL → FALSE
        assert_eq!(
            eval_expr(&tt(false, true), &r, &c).unwrap(),
            Value::Bool(false)
        );
        // TRUE AND NULL → NULL
        assert_eq!(eval_expr(&tt(true, true), &r, &c).unwrap(), Value::Null);
        // TRUE AND TRUE → TRUE
        assert_eq!(
            eval_expr(&tt(true, false), &r, &c).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn or_three_valued_logic() {
        let r = Row::new(vec![]);
        let cs: [ColumnSchema; 0] = [];
        let c = ctx(&cs, None);
        let or_with_null = |a: bool| Expr::Binary {
            lhs: alloc::boxed::Box::new(Expr::Literal(Literal::Bool(a))),
            op: BinOp::Or,
            rhs: alloc::boxed::Box::new(null()),
        };
        // TRUE OR NULL → TRUE
        assert_eq!(
            eval_expr(&or_with_null(true), &r, &c).unwrap(),
            Value::Bool(true)
        );
        // FALSE OR NULL → NULL
        assert_eq!(
            eval_expr(&or_with_null(false), &r, &c).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn not_on_null_is_null() {
        let r = Row::new(vec![]);
        let cs: [ColumnSchema; 0] = [];
        let c = ctx(&cs, None);
        let e = Expr::Unary {
            op: UnOp::Not,
            expr: alloc::boxed::Box::new(null()),
        };
        assert_eq!(eval_expr(&e, &r, &c).unwrap(), Value::Null);
    }

    #[test]
    fn text_comparison_lexicographic() {
        let r = Row::new(vec![]);
        let cs: [ColumnSchema; 0] = [];
        let c = ctx(&cs, None);
        let e = Expr::Binary {
            lhs: alloc::boxed::Box::new(Expr::Literal(Literal::String("apple".into()))),
            op: BinOp::Lt,
            rhs: alloc::boxed::Box::new(Expr::Literal(Literal::String("banana".into()))),
        };
        assert_eq!(eval_expr(&e, &r, &c).unwrap(), Value::Bool(true));
    }
}
