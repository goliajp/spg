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

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_sql::ast::{BinOp, CastTarget, ColumnName, Expr, Literal, UnOp};
use spg_storage::{ColumnSchema, DataType, Row, TsLexeme, TsQueryAst, Value};

/// Resolution context for evaluating a single row. `table_alias` is the alias
/// (or table name) callers should accept as the qualifier on a column ref —
/// e.g. `FROM users AS u` makes `u.name` valid and rejects `other.name`.
#[derive(Clone)]
#[allow(missing_debug_implementations)] // sequence_resolver is a dyn Fn — no Debug
pub struct EvalContext<'a> {
    pub columns: &'a [ColumnSchema],
    pub table_alias: Option<&'a str>,
    /// v6.1.1 — bound parameters for `$N` placeholders inside the
    /// expression tree. Empty for simple queries; populated by the
    /// prepared-statement Execute path with Bind values converted
    /// to `Value`. Index N (1-based per PG) hits `params[N-1]`.
    pub params: &'a [Value],
    /// v7.12.1 — session text-search config (from `SET
    /// default_text_search_config = '<name>'`). Resolved when the
    /// engine builds an `EvalContext` and consumed by the FTS
    /// function dispatcher when `to_tsvector(text)` /
    /// `plainto_tsquery(text)` etc are called without an explicit
    /// config arg. `None` falls through to `simple`.
    pub default_text_search_config: Option<&'a str>,
    /// v7.17.0 Phase 1.1 — `nextval` / `currval` / `setval`
    /// resolver. The engine builds this around a `&mut Catalog`
    /// so apply_function can mutate sequence state without
    /// eval owning a catalog reference. When `None`, sequence
    /// functions return an error (read-only contexts).
    pub sequence_resolver: Option<&'a SequenceResolver<'a>>,
}

/// v7.17.0 — sequence-mutating callback used by `apply_function`
/// for `nextval` / `currval` / `setval`. Implemented by the
/// engine to thread `&mut Catalog` access through an immutable
/// `&EvalContext`.
pub type SequenceResolver<'a> = dyn Fn(SequenceOp) -> Result<i64, EvalError> + 'a;

/// v7.17.0 — sequence operation requested by an Expr eval.
#[derive(Debug, Clone)]
pub enum SequenceOp {
    Next(String),
    Curr(String),
    Set {
        name: String,
        value: i64,
        is_called: bool,
    },
}

impl<'a> EvalContext<'a> {
    pub const fn new(columns: &'a [ColumnSchema], table_alias: Option<&'a str>) -> Self {
        Self {
            columns,
            table_alias,
            params: &[],
            default_text_search_config: None,
            sequence_resolver: None,
        }
    }

    /// v7.17.0 — attach a sequence resolver. The engine wraps a
    /// `&mut Catalog` in a closure that performs the requested
    /// SequenceOp.
    #[must_use]
    pub const fn with_sequence_resolver(
        mut self,
        resolver: &'a SequenceResolver<'a>,
    ) -> Self {
        self.sequence_resolver = Some(resolver);
        self
    }

    /// v6.1.1 — attach a parameter buffer for `$N` placeholder
    /// resolution. The slice must outlive the context; callers
    /// construct it from the prepared statement's Bind values.
    #[must_use]
    pub const fn with_params(mut self, params: &'a [Value]) -> Self {
        self.params = params;
        self
    }

    /// v7.12.1 — attach the session's
    /// `default_text_search_config`. Used by the FTS function
    /// dispatcher when no explicit config arg is given.
    #[must_use]
    pub const fn with_default_text_search_config(mut self, cfg: Option<&'a str>) -> Self {
        self.default_text_search_config = cfg;
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum EvalError {
    ColumnNotFound {
        name: String,
    },
    UnknownQualifier {
        qualifier: String,
    },
    DivisionByZero,
    TypeMismatch {
        detail: String,
    },
    /// v6.1.1 — `$N` reference past the number of bound parameters.
    /// Either the client sent too few in Bind, or the SQL has a
    /// placeholder the prepared statement didn't account for.
    PlaceholderOutOfRange {
        n: u16,
        bound: u16,
    },
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
            Self::PlaceholderOutOfRange { n, bound } => write!(
                f,
                "parameter ${n} referenced but only {bound} bound by client"
            ),
        }
    }
}

pub fn eval_expr(expr: &Expr, row: &Row, ctx: &EvalContext<'_>) -> Result<Value, EvalError> {
    match expr {
        Expr::Literal(l) => Ok(literal_to_value(l)),
        Expr::Column(c) => resolve_column(c, row, ctx),
        Expr::Placeholder(n) => {
            let idx = usize::from(*n).saturating_sub(1);
            ctx.params
                .get(idx)
                .cloned()
                .ok_or_else(|| EvalError::PlaceholderOutOfRange {
                    n: *n,
                    bound: u16::try_from(ctx.params.len()).unwrap_or(u16::MAX),
                })
        }
        Expr::Unary { op, expr } => {
            let v = eval_expr(expr, row, ctx)?;
            apply_unary(*op, v)
        }
        Expr::Binary { lhs, op, rhs } => {
            let l = eval_expr(lhs, row, ctx)?;
            let r = eval_expr(rhs, row, ctx)?;
            apply_binary(*op, l, r)
        }
        Expr::Cast { expr, target } => {
            let v = eval_expr(expr, row, ctx)?;
            cast_value(v, *target)
        }
        Expr::IsNull { expr, negated } => {
            let v = eval_expr(expr, row, ctx)?;
            let is_null = matches!(v, Value::Null);
            Ok(Value::Bool(if *negated { !is_null } else { is_null }))
        }
        Expr::FunctionCall { name, args } => {
            let evaluated: Result<Vec<Value>, _> =
                args.iter().map(|a| eval_expr(a, row, ctx)).collect();
            apply_function(name, &evaluated?, ctx)
        }
        Expr::Like {
            expr,
            pattern,
            negated,
        } => {
            let v = eval_expr(expr, row, ctx)?;
            let p = eval_expr(pattern, row, ctx)?;
            // NULL on either side propagates to NULL — same as PG.
            let (text, pat) = match (v, p) {
                (Value::Null, _) | (_, Value::Null) => return Ok(Value::Null),
                (Value::Text(a), Value::Text(b)) => (a, b),
                (Value::Text(_), other) | (other, _) => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!("LIKE requires text operands, got {:?}", other.data_type()),
                    });
                }
            };
            let m = like_match(&text, &pat);
            Ok(Value::Bool(if *negated { !m } else { m }))
        }
        Expr::Extract { field, source } => {
            let v = eval_expr(source, row, ctx)?;
            extract_field(*field, &v)
        }
        // v4.10: subquery nodes should have been resolved into
        // Literal / Binary-Eq-OR chains by Engine::resolve_select_subqueries
        // before the row loop. Anything reaching here is a bug.
        Expr::ScalarSubquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => {
            Err(EvalError::TypeMismatch {
                detail: "subquery reached row eval — engine resolver bug".into(),
            })
        }
        // v4.12: window functions should have been rewritten into
        // synthetic __win_N column references by
        // exec_select_with_window before row eval. Anything
        // reaching here is similarly a bug.
        Expr::WindowFunction { .. } => Err(EvalError::TypeMismatch {
            detail: "window function reached row eval — engine rewrite bug".into(),
        }),
        // v7.10.10 — `ARRAY[expr, expr, …]` constructor.
        // v7.11.13 — element-type detection: all integers →
        // IntArray (or BigIntArray when widening), any Text →
        // TextArray. Non-TEXT non-integer elements (Bool, Float)
        // stringify into TextArray as the safe default.
        Expr::Array(items) => {
            let mut materialised: Vec<Value> = Vec::with_capacity(items.len());
            for elem in items {
                materialised.push(eval_expr(elem, row, ctx)?);
            }
            let mut has_text = false;
            let mut has_bigint = false;
            let mut has_int = false;
            for v in &materialised {
                match v {
                    Value::Null => {}
                    Value::Int(_) | Value::SmallInt(_) => has_int = true,
                    Value::BigInt(_) => has_bigint = true,
                    Value::Text(_) | Value::Json(_) => has_text = true,
                    _ => has_text = true,
                }
            }
            if has_text || (!has_int && !has_bigint) {
                let out: Vec<Option<String>> = materialised
                    .into_iter()
                    .map(|v| match v {
                        Value::Null => None,
                        Value::Text(s) | Value::Json(s) => Some(s),
                        other => Some(value_to_text_for_array(&other)),
                    })
                    .collect();
                return Ok(Value::TextArray(out));
            }
            if has_bigint {
                let out: Vec<Option<i64>> = materialised
                    .into_iter()
                    .map(|v| match v {
                        Value::Null => None,
                        Value::Int(n) => Some(i64::from(n)),
                        Value::SmallInt(n) => Some(i64::from(n)),
                        Value::BigInt(n) => Some(n),
                        _ => unreachable!(),
                    })
                    .collect();
                return Ok(Value::BigIntArray(out));
            }
            let out: Vec<Option<i32>> = materialised
                .into_iter()
                .map(|v| match v {
                    Value::Null => None,
                    Value::Int(n) => Some(n),
                    Value::SmallInt(n) => Some(i32::from(n)),
                    _ => unreachable!(),
                })
                .collect();
            Ok(Value::IntArray(out))
        }
        // v7.10.12 — `arr[i]` PG-style 1-based indexing.
        // Out-of-range indices (including i ≤ 0) return NULL.
        Expr::ArraySubscript { target, index } => {
            let target_v = eval_expr(target, row, ctx)?;
            let idx_v = eval_expr(index, row, ctx)?;
            if matches!(target_v, Value::Null) || matches!(idx_v, Value::Null) {
                return Ok(Value::Null);
            }
            let i: i64 = match idx_v {
                Value::Int(n) => i64::from(n),
                Value::BigInt(n) => n,
                Value::SmallInt(n) => i64::from(n),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "array subscript must be integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            if i < 1 {
                return Ok(Value::Null);
            }
            let pos = (i - 1) as usize;
            match target_v {
                Value::TextArray(items) => match items.get(pos) {
                    Some(Some(s)) => Ok(Value::Text(s.clone())),
                    Some(None) | None => Ok(Value::Null),
                },
                Value::IntArray(items) => match items.get(pos) {
                    Some(Some(n)) => Ok(Value::Int(*n)),
                    Some(None) | None => Ok(Value::Null),
                },
                Value::BigIntArray(items) => match items.get(pos) {
                    Some(Some(n)) => Ok(Value::BigInt(*n)),
                    Some(None) | None => Ok(Value::Null),
                },
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "subscript target must be an array, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.10.12 — `x op ANY(arr)` / `x op ALL(arr)`. PG
        // 3VL: ANY → true if any element compares-true; NULL if
        // no true but some NULL; false otherwise. ALL: false if
        // any compares-false; NULL if no false but some NULL;
        // true otherwise.
        Expr::AnyAll {
            expr,
            op,
            array,
            is_any,
        } => {
            let lhs = eval_expr(expr, row, ctx)?;
            let arr = eval_expr(array, row, ctx)?;
            if matches!(arr, Value::Null) {
                return Ok(Value::Null);
            }
            let elems: Vec<Option<Value>> = match arr {
                Value::TextArray(items) => items.into_iter().map(|o| o.map(Value::Text)).collect(),
                Value::IntArray(items) => items.into_iter().map(|o| o.map(Value::Int)).collect(),
                Value::BigIntArray(items) => {
                    items.into_iter().map(|o| o.map(Value::BigInt)).collect()
                }
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "ANY/ALL right-hand side must be an array, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let mut saw_null = matches!(lhs, Value::Null);
            let mut saw_match = false;
            let mut saw_mismatch = false;
            for elem in elems {
                let elem_v = match elem {
                    Some(v) => v,
                    None => {
                        saw_null = true;
                        continue;
                    }
                };
                if matches!(lhs, Value::Null) {
                    saw_null = true;
                    continue;
                }
                match apply_binary(*op, lhs.clone(), elem_v) {
                    Ok(Value::Bool(true)) => saw_match = true,
                    Ok(Value::Bool(false)) => saw_mismatch = true,
                    Ok(Value::Null) => saw_null = true,
                    Ok(other) => {
                        return Err(EvalError::TypeMismatch {
                            detail: format!(
                                "ANY/ALL comparison didn't return Bool: {:?}",
                                other.data_type()
                            ),
                        });
                    }
                    Err(e) => return Err(e),
                }
            }
            let result = if *is_any {
                if saw_match {
                    Value::Bool(true)
                } else if saw_null {
                    Value::Null
                } else {
                    Value::Bool(false)
                }
            } else if saw_mismatch {
                Value::Bool(false)
            } else if saw_null {
                Value::Null
            } else {
                Value::Bool(true)
            };
            Ok(result)
        }
        // v7.13.0 — CASE WHEN … END (mailrs round-5 G9).
        // Short-circuit on the first matching branch. Searched form
        // (operand=None) treats each branch's WHEN as a Bool
        // predicate. Simple form (operand=Some) compares with =.
        // ELSE on no match; NULL if no ELSE.
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            let operand_value = match operand {
                Some(o) => Some(eval_expr(o, row, ctx)?),
                None => None,
            };
            for (when_expr, then_expr) in branches {
                let when_value = eval_expr(when_expr, row, ctx)?;
                let matched = match &operand_value {
                    None => matches!(when_value, Value::Bool(true)),
                    Some(op_v) => matches!(
                        apply_binary(spg_sql::ast::BinOp::Eq, op_v.clone(), when_value)?,
                        Value::Bool(true)
                    ),
                };
                if matched {
                    return eval_expr(then_expr, row, ctx);
                }
            }
            match else_branch {
                Some(e) => eval_expr(e, row, ctx),
                None => Ok(Value::Null),
            }
        }
    }
}

/// v7.10.10 — best-effort text rendering for non-TEXT array
/// elements (numbers, bools, etc.). The PG rule is that
/// `ARRAY[1, 2]` is `int[]`, but SPG's v7.10 only models TEXT[],
/// so we widen by stringifying. NUMERIC formatting goes through
/// the existing canonical helpers to stay consistent with
/// `format_numeric` / `format_date` etc.
fn value_to_text_for_array(v: &Value) -> String {
    match v {
        Value::Text(s) | Value::Json(s) => s.clone(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::SmallInt(n) => n.to_string(),
        Value::Bool(b) => {
            if *b {
                "true".into()
            } else {
                "false".into()
            }
        }
        Value::Float(x) => format!("{x}"),
        Value::Date(d) => format_date(*d),
        Value::Timestamp(t) => format_timestamp(*t),
        Value::Numeric { scaled, scale } => format_numeric(*scaled, *scale),
        _ => format!("{v:?}"),
    }
}

/// Pull an integer component (year / month / ... / microsecond) out
/// of a `DATE` or `TIMESTAMP`. Returns NULL on a NULL source, errors
/// when the source isn't a calendar type.
fn extract_field(field: spg_sql::ast::ExtractField, v: &Value) -> Result<Value, EvalError> {
    use spg_sql::ast::ExtractField as F;
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }
    // INTERVAL has its own decomposition — `YEAR` / `MONTH` come from
    // the months part, the rest from the microseconds part. PG matches
    // this convention (months is normalised modulo 12 for MONTH).
    if let Value::Interval { months, micros } = *v {
        let years = months / 12;
        let mons = months % 12;
        let secs_total = micros / 1_000_000;
        let frac = micros % 1_000_000;
        let result = match field {
            F::Year => i64::from(years),
            F::Month => i64::from(mons),
            F::Day => micros / 86_400_000_000,
            F::Hour => (secs_total / 3600) % 24,
            F::Minute => (secs_total / 60) % 60,
            F::Second => secs_total % 60,
            F::Microsecond => (secs_total % 60) * 1_000_000 + frac,
        };
        return Ok(Value::BigInt(result));
    }
    let (days, day_micros) = match *v {
        Value::Date(d) => (d, 0_i64),
        Value::Timestamp(t) => {
            let days = t.div_euclid(86_400_000_000);
            let day_micros = t.rem_euclid(86_400_000_000);
            (i32::try_from(days).unwrap_or(i32::MAX), day_micros)
        }
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "EXTRACT requires DATE / TIMESTAMP / INTERVAL, got {:?}",
                    v.data_type()
                ),
            });
        }
    };
    let (y, m, d) = civil_components(days);
    let secs = day_micros / 1_000_000;
    let hh = secs / 3600;
    let mm = (secs / 60) % 60;
    let ss = secs % 60;
    let frac = day_micros % 1_000_000;
    let result = match field {
        F::Year => i64::from(y),
        F::Month => i64::from(m),
        F::Day => i64::from(d),
        F::Hour => hh,
        F::Minute => mm,
        F::Second => ss,
        F::Microsecond => ss * 1_000_000 + frac,
    };
    Ok(Value::BigInt(result))
}

/// Internal wrapper around the file-private `civil_from_days` so the
/// public surface area doesn't change. Returns `(year, month, day)`.
fn civil_components(days: i32) -> (i32, u32, u32) {
    civil_from_days(days)
}

/// SQL `LIKE` matcher. Wildcards are `%` (any run, possibly empty) and `_`
/// (exactly one char). `\` escapes the next pattern char so `\%` matches a
/// literal `%`. Matches the whole input — no implicit anchoring needed
/// since SQL `LIKE` is always full-string.
fn like_match(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pat: Vec<char> = pattern.chars().collect();
    like_match_inner(&text, 0, &pat, 0)
}

fn like_match_inner(text: &[char], mut ti: usize, pat: &[char], mut pi: usize) -> bool {
    while pi < pat.len() {
        match pat[pi] {
            '%' => {
                // Collapse consecutive `%` and try every possible split.
                while pi < pat.len() && pat[pi] == '%' {
                    pi += 1;
                }
                if pi == pat.len() {
                    return true;
                }
                for k in ti..=text.len() {
                    if like_match_inner(text, k, pat, pi) {
                        return true;
                    }
                }
                return false;
            }
            '_' => {
                if ti >= text.len() {
                    return false;
                }
                ti += 1;
                pi += 1;
            }
            '\\' if pi + 1 < pat.len() => {
                let want = pat[pi + 1];
                if ti >= text.len() || text[ti] != want {
                    return false;
                }
                ti += 1;
                pi += 2;
            }
            c => {
                if ti >= text.len() || text[ti] != c {
                    return false;
                }
                ti += 1;
                pi += 1;
            }
        }
    }
    ti == text.len()
}

/// Dispatch on lowercased function name. v1.4 implements only a handful of
/// scalar functions; aggregates land in v1.5 alongside GROUP BY.
fn apply_function(name: &str, args: &[Value], ctx: &EvalContext<'_>) -> Result<Value, EvalError> {
    match name.to_ascii_lowercase().as_str() {
        // v7.17.0 Phase 1.1 — SEQUENCE accessor functions.
        "nextval" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("nextval() takes 1 arg, got {}", args.len()),
                });
            }
            let seq_name = match &args[0] {
                Value::Text(s) => s.clone(),
                Value::Null => return Ok(Value::Null),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "nextval() argument must be TEXT, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let resolver = ctx.sequence_resolver.ok_or_else(|| EvalError::TypeMismatch {
                detail: "nextval() requires a sequence resolver (read-only context)".into(),
            })?;
            let v = resolver(SequenceOp::Next(seq_name))?;
            Ok(Value::BigInt(v))
        }
        "currval" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("currval() takes 1 arg, got {}", args.len()),
                });
            }
            let seq_name = match &args[0] {
                Value::Text(s) => s.clone(),
                Value::Null => return Ok(Value::Null),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "currval() argument must be TEXT, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let resolver = ctx.sequence_resolver.ok_or_else(|| EvalError::TypeMismatch {
                detail: "currval() requires a sequence resolver (read-only context)".into(),
            })?;
            let v = resolver(SequenceOp::Curr(seq_name))?;
            Ok(Value::BigInt(v))
        }
        "setval" => {
            if args.len() != 2 && args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("setval() takes 2 or 3 args, got {}", args.len()),
                });
            }
            let seq_name = match &args[0] {
                Value::Text(s) => s.clone(),
                Value::Null => return Ok(Value::Null),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "setval() name argument must be TEXT, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let value = match &args[1] {
                Value::SmallInt(n) => i64::from(*n),
                Value::Int(n) => i64::from(*n),
                Value::BigInt(n) => *n,
                Value::Null => return Ok(Value::Null),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "setval() value argument must be integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            let is_called = if args.len() == 3 {
                match &args[2] {
                    Value::Bool(b) => *b,
                    Value::Null => return Ok(Value::Null),
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: format!(
                                "setval() is_called argument must be BOOL, got {:?}",
                                other.data_type()
                            ),
                        });
                    }
                }
            } else {
                true
            };
            let resolver = ctx.sequence_resolver.ok_or_else(|| EvalError::TypeMismatch {
                detail: "setval() requires a sequence resolver (read-only context)".into(),
            })?;
            let v = resolver(SequenceOp::Set {
                name: seq_name,
                value,
                is_called,
            })?;
            Ok(Value::BigInt(v))
        }
        "length" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("length() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    let n = i32::try_from(s.chars().count()).unwrap_or(i32::MAX);
                    Ok(Value::Int(n))
                }
                // v7.10.4 — PG semantics: length(bytea) returns
                // byte count (= octet_length). Without this branch
                // mailrs's INSERT … SELECT length(body) … against a
                // BYTEA column would type-mismatch.
                Value::Bytes(b) => {
                    let n = i32::try_from(b.len()).unwrap_or(i32::MAX);
                    Ok(Value::Int(n))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!("length() needs text or bytea, got {:?}", other.data_type()),
                }),
            }
        }
        // v7.10.4 — `OCTET_LENGTH(x)` returns byte count for both
        // TEXT (UTF-8 byte length) and BYTEA. PG-spec name; aliases
        // to length() for bytea by design.
        "octet_length" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("octet_length() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    let n = i32::try_from(s.len()).unwrap_or(i32::MAX);
                    Ok(Value::Int(n))
                }
                Value::Bytes(b) => {
                    let n = i32::try_from(b.len()).unwrap_or(i32::MAX);
                    Ok(Value::Int(n))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "octet_length() needs text or bytea, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.11.6 — `array_length(arr, dim)` returns the element
        // count of `arr` along dimension `dim`. v7.11 only models
        // single-dimension arrays so dim must be 1 (otherwise NULL,
        // matching PG semantics for unsupported dimensions). NULL
        // array → NULL. v7.11 TEXT[] only; non-array operand is
        // a type mismatch.
        "array_length" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("array_length() takes 2 args, got {}", args.len()),
                });
            }
            if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
                return Ok(Value::Null);
            }
            let len = match &args[0] {
                Value::TextArray(items) => items.len(),
                Value::IntArray(items) => items.len(),
                Value::BigIntArray(items) => items.len(),
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "array_length() first arg must be an array, got {:?}",
                            args[0].data_type()
                        ),
                    });
                }
            };
            let dim: i64 = match args[1] {
                Value::Int(n) => i64::from(n),
                Value::BigInt(n) => n,
                Value::SmallInt(n) => i64::from(n),
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "array_length() second arg must be integer, got {:?}",
                            args[1].data_type()
                        ),
                    });
                }
            };
            if dim != 1 {
                return Ok(Value::Null);
            }
            let n = i32::try_from(len).unwrap_or(i32::MAX);
            Ok(Value::Int(n))
        }
        // v7.11.6 — `array_position(arr, val)` returns 1-based
        // index of the first element of `arr` equal to `val`, or
        // NULL if not found. PG NULL semantics: NULL array → NULL;
        // NULL val never matches (returns NULL if absent).
        "array_position" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("array_position() takes 2 args, got {}", args.len()),
                });
            }
            if matches!(args[0], Value::Null) {
                return Ok(Value::Null);
            }
            if matches!(args[1], Value::Null) {
                return Ok(Value::Null);
            }
            match (&args[0], &args[1]) {
                (Value::TextArray(items), Value::Text(needle)) => {
                    for (idx, item) in items.iter().enumerate() {
                        if let Some(s) = item
                            && s == needle
                        {
                            return Ok(Value::Int(i32::try_from(idx + 1).unwrap_or(i32::MAX)));
                        }
                    }
                    Ok(Value::Null)
                }
                (Value::IntArray(items), needle_v)
                    if matches!(
                        needle_v,
                        Value::Int(_) | Value::SmallInt(_) | Value::BigInt(_)
                    ) =>
                {
                    let needle: i64 = match *needle_v {
                        Value::Int(n) => i64::from(n),
                        Value::SmallInt(n) => i64::from(n),
                        Value::BigInt(n) => n,
                        _ => unreachable!(),
                    };
                    for (idx, item) in items.iter().enumerate() {
                        if let Some(n) = item
                            && i64::from(*n) == needle
                        {
                            return Ok(Value::Int(i32::try_from(idx + 1).unwrap_or(i32::MAX)));
                        }
                    }
                    Ok(Value::Null)
                }
                (Value::BigIntArray(items), needle_v)
                    if matches!(
                        needle_v,
                        Value::Int(_) | Value::SmallInt(_) | Value::BigInt(_)
                    ) =>
                {
                    let needle: i64 = match *needle_v {
                        Value::Int(n) => i64::from(n),
                        Value::SmallInt(n) => i64::from(n),
                        Value::BigInt(n) => n,
                        _ => unreachable!(),
                    };
                    for (idx, item) in items.iter().enumerate() {
                        if let Some(n) = item
                            && *n == needle
                        {
                            return Ok(Value::Int(i32::try_from(idx + 1).unwrap_or(i32::MAX)));
                        }
                    }
                    Ok(Value::Null)
                }
                (
                    arr @ (Value::TextArray(_) | Value::IntArray(_) | Value::BigIntArray(_)),
                    other,
                ) => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "array_position() needle type {:?} doesn't match array {:?}",
                        other.data_type(),
                        arr.data_type()
                    ),
                }),
                (other, _) => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "array_position() first arg must be an array, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.11.15 — `substring(s, start)` / `substring(s, start, length)`
        // for both TEXT and BYTEA. PG semantics: `start` is 1-based;
        // values ≤ 0 clamp into the string (i.e. effective start is
        // adjusted so the window still begins at index 1 — but
        // `length` is reduced by the clipped prefix). A NULL arg
        // makes the result NULL. Out-of-range windows return an
        // empty value, not NULL.
        "substring" => {
            if !matches!(args.len(), 2 | 3) {
                return Err(EvalError::TypeMismatch {
                    detail: format!("substring() takes 2 or 3 args, got {}", args.len()),
                });
            }
            if args.iter().any(|a| matches!(a, Value::Null)) {
                return Ok(Value::Null);
            }
            let start: i64 = match args[1] {
                Value::Int(n) => i64::from(n),
                Value::BigInt(n) => n,
                Value::SmallInt(n) => i64::from(n),
                _ => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!(
                            "substring() start must be integer, got {:?}",
                            args[1].data_type()
                        ),
                    });
                }
            };
            let length: Option<i64> = if args.len() == 3 {
                match args[2] {
                    Value::Int(n) => Some(i64::from(n)),
                    Value::BigInt(n) => Some(n),
                    Value::SmallInt(n) => Some(i64::from(n)),
                    _ => {
                        return Err(EvalError::TypeMismatch {
                            detail: format!(
                                "substring() length must be integer, got {:?}",
                                args[2].data_type()
                            ),
                        });
                    }
                }
            } else {
                None
            };
            // PG: when length is given, end = start + length; if
            // end < start the result is empty. Clip start to 1.
            let (effective_start, effective_length): (i64, Option<i64>) = match length {
                Some(len) => {
                    let end = start.saturating_add(len);
                    if end <= 1 || len < 0 {
                        return Ok(match &args[0] {
                            Value::Text(_) => Value::Text(String::new()),
                            Value::Bytes(_) => Value::Bytes(Vec::new()),
                            other => {
                                return Err(EvalError::TypeMismatch {
                                    detail: format!(
                                        "substring() needs text or bytea, got {:?}",
                                        other.data_type()
                                    ),
                                });
                            }
                        });
                    }
                    let eff_start = start.max(1);
                    let eff_len = end - eff_start;
                    (eff_start, Some(eff_len.max(0)))
                }
                None => (start.max(1), None),
            };
            match &args[0] {
                Value::Text(s) => {
                    // PG counts in characters (codepoints) for TEXT.
                    let chars: Vec<char> = s.chars().collect();
                    let skip = (effective_start - 1) as usize;
                    if skip >= chars.len() {
                        return Ok(Value::Text(String::new()));
                    }
                    let take = match effective_length {
                        Some(n) => (n as usize).min(chars.len() - skip),
                        None => chars.len() - skip,
                    };
                    Ok(Value::Text(chars[skip..skip + take].iter().collect()))
                }
                Value::Bytes(b) => {
                    let skip = (effective_start - 1) as usize;
                    if skip >= b.len() {
                        return Ok(Value::Bytes(Vec::new()));
                    }
                    let take = match effective_length {
                        Some(n) => (n as usize).min(b.len() - skip),
                        None => b.len() - skip,
                    };
                    Ok(Value::Bytes(b[skip..skip + take].to_vec()))
                }
                other => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "substring() needs text or bytea, got {:?}",
                        other.data_type()
                    ),
                }),
            }
        }
        // v7.11.15 — `position(needle, haystack)`. PG semantics:
        // 1-based byte/char index of first occurrence, or 0 if
        // absent. NULL on either operand → NULL. Empty needle
        // returns 1 (PG convention). Works on TEXT (char positions)
        // and BYTEA (byte positions). (The PG-spec syntax `position(
        // needle IN haystack)` is not parsed in v7.11; clients must
        // call the function-call form.)
        "position" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("position() takes 2 args, got {}", args.len()),
                });
            }
            if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
                return Ok(Value::Null);
            }
            match (&args[0], &args[1]) {
                (Value::Text(needle), Value::Text(haystack)) => {
                    if needle.is_empty() {
                        return Ok(Value::Int(1));
                    }
                    // Char-based position (PG uses character count).
                    let h_chars: Vec<char> = haystack.chars().collect();
                    let n_chars: Vec<char> = needle.chars().collect();
                    if n_chars.len() > h_chars.len() {
                        return Ok(Value::Int(0));
                    }
                    for i in 0..=h_chars.len() - n_chars.len() {
                        if h_chars[i..i + n_chars.len()] == n_chars[..] {
                            return Ok(Value::Int(i32::try_from(i + 1).unwrap_or(i32::MAX)));
                        }
                    }
                    Ok(Value::Int(0))
                }
                (Value::Bytes(needle), Value::Bytes(haystack)) => {
                    if needle.is_empty() {
                        return Ok(Value::Int(1));
                    }
                    if needle.len() > haystack.len() {
                        return Ok(Value::Int(0));
                    }
                    for i in 0..=haystack.len() - needle.len() {
                        if &haystack[i..i + needle.len()] == needle.as_slice() {
                            return Ok(Value::Int(i32::try_from(i + 1).unwrap_or(i32::MAX)));
                        }
                    }
                    Ok(Value::Int(0))
                }
                (a, b) => Err(EvalError::TypeMismatch {
                    detail: format!(
                        "position() operands must both be text or both bytea, got {:?} and {:?}",
                        a.data_type(),
                        b.data_type()
                    ),
                }),
            }
        }
        "upper" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("upper() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Text(s.to_uppercase())),
                other => Err(EvalError::TypeMismatch {
                    detail: format!("upper() needs text, got {:?}", other.data_type()),
                }),
            }
        }
        "lower" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("lower() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Text(s.to_lowercase())),
                other => Err(EvalError::TypeMismatch {
                    detail: format!("lower() needs text, got {:?}", other.data_type()),
                }),
            }
        }
        "abs" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("abs() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Int(n.wrapping_abs())),
                Value::BigInt(n) => Ok(Value::BigInt(n.wrapping_abs())),
                Value::Float(x) => Ok(Value::Float(x.abs())),
                other => Err(EvalError::TypeMismatch {
                    detail: format!("abs() needs numeric, got {:?}", other.data_type()),
                }),
            }
        }
        "coalesce" => {
            for a in args {
                if !matches!(a, Value::Null) {
                    return Ok(a.clone());
                }
            }
            Ok(Value::Null)
        }
        "date_trunc" => date_trunc(args),
        "date_part" => date_part(args),
        "age" => age(args),
        "to_char" => to_char(args),
        // v6.4.3 — encode/decode + error_on_null SQL function bundle.
        "encode" => encode_text(args),
        "decode" => decode_text(args),
        "error_on_null" => error_on_null(args),
        // v7.12.1 — PG full-text search lexer / tsquery builders.
        // mailrs G-CRIT-3 acceptance path: `to_tsvector('english',
        // … || ' ' || … || …)` runs end-to-end against a tsvector
        // column with Porter stemming + standard english stopwords.
        "to_tsvector" => fts_to_tsvector(args, ctx),
        "plainto_tsquery" => fts_plainto_tsquery(args, ctx),
        "phraseto_tsquery" => fts_phraseto_tsquery(args, ctx),
        "websearch_to_tsquery" => fts_websearch_to_tsquery(args, ctx),
        "to_tsquery" => fts_to_tsquery(args, ctx),
        // v7.12.2 — ranking functions. mailrs's fallback search
        // query ORDERs BY ts_rank(search_vector, q) DESC.
        "ts_rank" => fts_ts_rank(args),
        "ts_rank_cd" => fts_ts_rank_cd(args),
        // v7.14.0 — PG dump preamble emits
        // `SELECT pg_catalog.set_config('search_path', '', false);`
        // and friends. SPG is single-schema; accept-as-no-op
        // returning either the new value or NULL.
        "set_config" => Ok(args.get(1).cloned().unwrap_or(Value::Null)),
        "current_setting" => Ok(Value::Text(String::new())),
        // PG `pg_catalog.*` discovery / cast helpers commonly
        // emitted by ORMs probing the server. Accept-as-no-op
        // with sensible defaults so the dump preamble doesn't
        // fail. `pg_get_serial_sequence` returns NULL (no
        // sequence — SPG has AUTO_INCREMENT instead).
        "pg_get_serial_sequence" | "pg_get_constraintdef" | "pg_get_indexdef" => Ok(Value::Null),
        "version" => Ok(Value::Text("PostgreSQL 16 (SPG-compat)".into())),
        // v7.17.0 — `nextval` / `currval` / `setval` are handled
        // at the top of this match against the SequenceResolver.
        // `lastval()` (no-arg session memory) still degrades to
        // NULL pending a Phase 1.1b session tracker.
        "lastval" => Ok(Value::Null),
        // v7.15.0 — pg_trgm: similarity, show_trgm. Match PG
        // semantics: similarity returns Jaccard of trigram sets;
        // show_trgm returns the trigram set as TEXT[]. NULL on
        // any NULL arg.
        "similarity" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("similarity() takes 2 args, got {}", args.len()),
                });
            }
            if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
                return Ok(Value::Null);
            }
            let a = match &args[0] {
                Value::Text(s) => s.as_str(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!("similarity() needs text, got {:?}", other.data_type()),
                    });
                }
            };
            let b = match &args[1] {
                Value::Text(s) => s.as_str(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!("similarity() needs text, got {:?}", other.data_type()),
                    });
                }
            };
            // PG returns REAL (f32) — we use Float (f64) and let
            // coerce_value narrow on assignment to a REAL column.
            Ok(Value::Float(spg_storage::trgm::similarity(a, b)))
        }
        "show_trgm" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("show_trgm() takes 1 arg, got {}", args.len()),
                });
            }
            if matches!(args[0], Value::Null) {
                return Ok(Value::Null);
            }
            let s = match &args[0] {
                Value::Text(s) => s.as_str(),
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!("show_trgm() needs text, got {:?}", other.data_type()),
                    });
                }
            };
            // PG returns the trigram set sorted lexicographically.
            // `extract_trigrams` already returns a BTreeSet so the
            // order is canonical.
            let trigrams: Vec<Option<String>> = spg_storage::trgm::extract_trigrams(s)
                .into_iter()
                .map(Some)
                .collect();
            Ok(Value::TextArray(trigrams))
        }
        other => Err(EvalError::TypeMismatch {
            detail: format!("unknown function `{other}`"),
        }),
    }
}

/// v7.12.2 — `ts_rank([weights,] vec, query [, norm])`. v7.12.2
/// supports the canonical `(vec, query)` two-arg form mailrs uses;
/// optional weight-array / normalisation arguments error with an
/// "unsupported" message rather than silently changing semantics.
fn fts_ts_rank(args: &[Value]) -> Result<Value, EvalError> {
    let (vec, query) = parse_rank_args("ts_rank", args)?;
    match (vec, query) {
        (None, _) | (_, None) => Ok(Value::Null),
        (Some(v), Some(q)) => Ok(Value::Float(f64::from(crate::fts::ts_rank(&v, &q)))),
    }
}

fn fts_ts_rank_cd(args: &[Value]) -> Result<Value, EvalError> {
    let (vec, query) = parse_rank_args("ts_rank_cd", args)?;
    match (vec, query) {
        (None, _) | (_, None) => Ok(Value::Null),
        (Some(v), Some(q)) => Ok(Value::Float(f64::from(crate::fts::ts_rank_cd(&v, &q)))),
    }
}

fn parse_rank_args(
    name: &str,
    args: &[Value],
) -> Result<
    (
        Option<Vec<spg_storage::TsLexeme>>,
        Option<spg_storage::TsQueryAst>,
    ),
    EvalError,
> {
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "{name}() takes 2 args in v7.12.2 (weights array + normalisation flag are v7.12.x carve-out), got {}",
                args.len()
            ),
        });
    }
    let vec = match &args[0] {
        Value::Null => None,
        Value::TsVector(v) => Some(v.clone()),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "{name}() first arg must be tsvector, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let query = match &args[1] {
        Value::Null => None,
        Value::TsQuery(q) => Some(q.clone()),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "{name}() second arg must be tsquery, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    Ok((vec, query))
}

/// v7.12.2 — `tsvector @@ tsquery` match operator. Either
/// ordering accepted (PG semantics). NULL on either side → NULL.
/// Anything that isn't tsvector/tsquery on either side is a type
/// mismatch. Returns BOOL.
fn ts_match(l: Value, r: Value) -> Result<Value, EvalError> {
    let (vec, query) = match (l, r) {
        (Value::Null, _) | (_, Value::Null) => return Ok(Value::Null),
        (Value::TsVector(v), Value::TsQuery(q)) => (v, q),
        (Value::TsQuery(q), Value::TsVector(v)) => (v, q),
        (l, r) => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "@@ requires (tsvector, tsquery), got ({:?}, {:?})",
                    l.data_type(),
                    r.data_type()
                ),
            });
        }
    };
    Ok(Value::Bool(crate::fts::ts_query_matches(&vec, &query)))
}

/// v7.12.1 — `to_tsvector([config,] text)`. With one arg the
/// session-resolved `default_text_search_config` is used (defaults
/// to `simple` when unset); with two args the first picks the
/// config. NULL text → NULL.
fn fts_to_tsvector(args: &[Value], ctx: &EvalContext<'_>) -> Result<Value, EvalError> {
    let (config, text) = parse_fts_args("to_tsvector", args, ctx)?;
    match text {
        None => Ok(Value::Null),
        Some(t) => Ok(Value::TsVector(crate::fts::to_tsvector(config, &t))),
    }
}

fn fts_plainto_tsquery(args: &[Value], ctx: &EvalContext<'_>) -> Result<Value, EvalError> {
    let (config, text) = parse_fts_args("plainto_tsquery", args, ctx)?;
    match text {
        None => Ok(Value::Null),
        Some(t) => Ok(Value::TsQuery(crate::fts::plainto_tsquery(config, &t))),
    }
}

fn fts_phraseto_tsquery(args: &[Value], ctx: &EvalContext<'_>) -> Result<Value, EvalError> {
    let (config, text) = parse_fts_args("phraseto_tsquery", args, ctx)?;
    match text {
        None => Ok(Value::Null),
        Some(t) => Ok(Value::TsQuery(crate::fts::phraseto_tsquery(config, &t))),
    }
}

fn fts_websearch_to_tsquery(args: &[Value], ctx: &EvalContext<'_>) -> Result<Value, EvalError> {
    let (config, text) = parse_fts_args("websearch_to_tsquery", args, ctx)?;
    match text {
        None => Ok(Value::Null),
        Some(t) => Ok(Value::TsQuery(crate::fts::websearch_to_tsquery(config, &t))),
    }
}

fn fts_to_tsquery(args: &[Value], ctx: &EvalContext<'_>) -> Result<Value, EvalError> {
    let (config, text) = parse_fts_args("to_tsquery", args, ctx)?;
    match text {
        None => Ok(Value::Null),
        Some(t) => Ok(Value::TsQuery(crate::fts::to_tsquery(config, &t)?)),
    }
}

/// Parse the `(config, text)` / `(text)` argument pair shared by
/// all FTS builders. Returns the resolved config + the text
/// payload (None when text is NULL). The one-arg form pulls the
/// config from the session's `default_text_search_config`.
fn parse_fts_args(
    name: &str,
    args: &[Value],
    ctx: &EvalContext<'_>,
) -> Result<(crate::fts::TsConfig, Option<String>), EvalError> {
    let (config_arg, text_arg) = match args {
        [t] => (None, t),
        [c, t] => (Some(c), t),
        _ => {
            return Err(EvalError::TypeMismatch {
                detail: format!("{name}() takes 1 or 2 args, got {}", args.len()),
            });
        }
    };
    let config = match config_arg {
        None => match ctx.default_text_search_config {
            Some(name_str) => crate::fts::TsConfig::from_name(name_str).ok_or_else(|| {
                EvalError::TypeMismatch {
                    detail: format!(
                        "text search config not implemented: {name_str:?} (supported: simple, english)"
                    ),
                }
            })?,
            None => crate::fts::TsConfig::Simple,
        },
        Some(Value::Null) => return Ok((crate::fts::TsConfig::Simple, None)),
        Some(Value::Text(name_str)) => crate::fts::TsConfig::from_name(name_str).ok_or_else(|| {
            EvalError::TypeMismatch {
                detail: format!(
                    "text search config not implemented: {name_str:?} (supported: simple, english)"
                ),
            }
        })?,
        Some(other) => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "{name}() config arg must be text, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let text = match text_arg {
        Value::Null => None,
        Value::Text(s) => Some(s.clone()),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "{name}() text arg must be text, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    Ok((config, text))
}

/// v6.4.3 — `encode(bytes_as_text, format)`. PG works on bytea
/// arguments; SPG's value space treats Text as the byte container
/// (raw UTF-8 bytes). Supported formats: base64 (PG default),
/// base64url (RFC 4648 §5), base32hex (RFC 4648 §7 extended-hex),
/// hex.
fn encode_text(args: &[Value]) -> Result<Value, EvalError> {
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!("encode() takes 2 args, got {}", args.len()),
        });
    }
    if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
        return Ok(Value::Null);
    }
    let bytes: &[u8] = match &args[0] {
        Value::Text(s) => s.as_bytes(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!("encode() expects text bytes, got {:?}", other.data_type()),
            });
        }
    };
    let fmt = match &args[1] {
        Value::Text(s) => s.to_ascii_lowercase(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!("encode() format must be text, got {:?}", other.data_type()),
            });
        }
    };
    let out = match fmt.as_str() {
        "base64" => b64_encode(bytes, B64_STD),
        "base64url" => b64_encode(bytes, B64_URL),
        "base32hex" => b32hex_encode(bytes),
        "hex" => hex_encode(bytes),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!("encode(): unknown format `{other}`"),
            });
        }
    };
    Ok(Value::Text(out))
}

/// v6.4.3 — `decode(text, format)`. Inverse of `encode`; returns
/// Text containing the raw decoded bytes (caller may CAST to bytea
/// equivalent if SPG adds bytea later).
fn decode_text(args: &[Value]) -> Result<Value, EvalError> {
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!("decode() takes 2 args, got {}", args.len()),
        });
    }
    if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
        return Ok(Value::Null);
    }
    let text = match &args[0] {
        Value::Text(s) => s.as_str(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!("decode() expects text, got {:?}", other.data_type()),
            });
        }
    };
    let fmt = match &args[1] {
        Value::Text(s) => s.to_ascii_lowercase(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!("decode() format must be text, got {:?}", other.data_type()),
            });
        }
    };
    let bytes = match fmt.as_str() {
        "base64" => b64_decode(text, B64_STD)?,
        "base64url" => b64_decode(text, B64_URL)?,
        "base32hex" => b32hex_decode(text)?,
        "hex" => hex_decode(text)?,
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!("decode(): unknown format `{other}`"),
            });
        }
    };
    let s = String::from_utf8(bytes).map_err(|_| EvalError::TypeMismatch {
        detail: "decode(): result bytes are not valid UTF-8 (SPG stores raw bytes as Text)".into(),
    })?;
    Ok(Value::Text(s))
}

/// v6.4.3 — `error_on_null(v)`. Returns `v` unchanged if non-NULL;
/// errors otherwise. Convenience to assert NOT NULL inside an
/// expression without wrapping it in COALESCE + raise hacks.
fn error_on_null(args: &[Value]) -> Result<Value, EvalError> {
    if args.len() != 1 {
        return Err(EvalError::TypeMismatch {
            detail: format!("error_on_null() takes 1 arg, got {}", args.len()),
        });
    }
    if matches!(args[0], Value::Null) {
        return Err(EvalError::TypeMismatch {
            detail: "error_on_null(): argument is NULL".into(),
        });
    }
    Ok(args[0].clone())
}

// ── byte-level encoders ───────────────────────────────────────────

const B64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
const B32HEX_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHIJKLMNOPQRSTUV";

fn b64_encode(bytes: &[u8], alpha: &[u8; 64]) -> String {
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | (bytes[i + 2] as u32);
        out.push(alpha[((n >> 18) & 0x3f) as usize] as char);
        out.push(alpha[((n >> 12) & 0x3f) as usize] as char);
        out.push(alpha[((n >> 6) & 0x3f) as usize] as char);
        out.push(alpha[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = (bytes[i] as u32) << 16;
        out.push(alpha[((n >> 18) & 0x3f) as usize] as char);
        out.push(alpha[((n >> 12) & 0x3f) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
        out.push(alpha[((n >> 18) & 0x3f) as usize] as char);
        out.push(alpha[((n >> 12) & 0x3f) as usize] as char);
        out.push(alpha[((n >> 6) & 0x3f) as usize] as char);
        out.push('=');
    }
    out
}

fn b64_decode(text: &str, alpha: &[u8; 64]) -> Result<Vec<u8>, EvalError> {
    let mut lookup = [255u8; 256];
    for (i, &c) in alpha.iter().enumerate() {
        lookup[c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for c in text.bytes() {
        if c == b'=' {
            break;
        }
        if c == b'\n' || c == b'\r' || c == b' ' {
            continue;
        }
        let v = lookup[c as usize];
        if v == 255 {
            return Err(EvalError::TypeMismatch {
                detail: format!("decode(base64): invalid char {:?}", c as char),
            });
        }
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

fn b32hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() * 8 + 4) / 5);
    let mut buf: u64 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {
        buf = (buf << 8) | b as u64;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(B32HEX_ALPHABET[((buf >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(B32HEX_ALPHABET[((buf << (5 - bits)) & 0x1f) as usize] as char);
    }
    // Pad to multiple of 8.
    while out.len() % 8 != 0 {
        out.push('=');
    }
    out
}

fn b32hex_decode(text: &str) -> Result<Vec<u8>, EvalError> {
    let mut lookup = [255u8; 256];
    for (i, &c) in B32HEX_ALPHABET.iter().enumerate() {
        lookup[c as usize] = i as u8;
        // base32hex is case-insensitive — also map lowercase.
        let lower = (c as char).to_ascii_lowercase() as u8;
        lookup[lower as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    let mut buf: u64 = 0;
    let mut bits: u32 = 0;
    for c in text.bytes() {
        if c == b'=' {
            break;
        }
        if c == b'\n' || c == b'\r' || c == b' ' {
            continue;
        }
        let v = lookup[c as usize];
        if v == 255 {
            return Err(EvalError::TypeMismatch {
                detail: format!("decode(base32hex): invalid char {:?}", c as char),
            });
        }
        buf = (buf << 5) | v as u64;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

fn hex_decode(text: &str) -> Result<Vec<u8>, EvalError> {
    let trimmed = text.trim();
    if trimmed.len() % 2 != 0 {
        return Err(EvalError::TypeMismatch {
            detail: "decode(hex): input length must be even".into(),
        });
    }
    let mut out = Vec::with_capacity(trimmed.len() / 2);
    let mut hi: u8 = 0;
    for (i, c) in trimmed.bytes().enumerate() {
        let v = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => {
                return Err(EvalError::TypeMismatch {
                    detail: format!("decode(hex): invalid char {:?}", c as char),
                });
            }
        };
        if i % 2 == 0 {
            hi = v;
        } else {
            out.push((hi << 4) | v);
        }
    }
    Ok(out)
}

/// `date_part(field_text, source)` — function form of `EXTRACT(field FROM
/// source)`. Same component dispatch (DATE / TIMESTAMP / INTERVAL) and
/// same `BigInt` return shape; PG returns double precision but we keep the
/// integer convention so the runner's `query I` shape works unchanged.
fn date_part(args: &[Value]) -> Result<Value, EvalError> {
    use spg_sql::ast::ExtractField as F;
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!("date_part() takes 2 args, got {}", args.len()),
        });
    }
    if matches!(&args[0], Value::Null) || matches!(&args[1], Value::Null) {
        return Ok(Value::Null);
    }
    let Value::Text(field_name) = &args[0] else {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "date_part() needs a text field, got {:?}",
                args[0].data_type()
            ),
        });
    };
    let field = match field_name.to_ascii_lowercase().as_str() {
        "year" => F::Year,
        "month" => F::Month,
        "day" => F::Day,
        "hour" => F::Hour,
        "minute" => F::Minute,
        "second" => F::Second,
        "microsecond" | "microseconds" => F::Microsecond,
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "unknown date_part field {other:?}; \
                     supported: year, month, day, hour, minute, second, microsecond"
                ),
            });
        }
    };
    extract_field(field, &args[1])
}

/// `age(t1, t2)` — return `t1 - t2` as an INTERVAL. v2.12 produces a
/// micros-only interval (no months normalisation) because PG's
/// month-justification rule is sensitive to the day-of-month walk and
/// adds material complexity for marginal corpus value.
///
/// `age(t)` (single-arg form) is intentionally unsupported in v2.12:
/// the dispatcher errors instead of guessing a clock source. Callers
/// who want PG's `age(t)` semantics should write `age(CURRENT_DATE, t)`
/// explicitly so the clock reference is visible at the SQL layer.
fn age(args: &[Value]) -> Result<Value, EvalError> {
    if args.is_empty() || args.len() > 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!("age() takes 1 or 2 args, got {}", args.len()),
        });
    }
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    // Coerce to TIMESTAMP micros — DATE lifts to midnight; TIMESTAMP
    // stays as-is; anything else errors.
    let to_micros = |v: &Value| -> Result<i64, EvalError> {
        match v {
            Value::Timestamp(t) => Ok(*t),
            Value::Date(d) => Ok(i64::from(*d) * 86_400_000_000),
            other => Err(EvalError::TypeMismatch {
                detail: format!("age() needs DATE or TIMESTAMP, got {:?}", other.data_type()),
            }),
        }
    };
    if args.len() == 1 {
        return Err(EvalError::TypeMismatch {
            detail: "single-arg age() is unsupported in v2.12 \
                     (use age(CURRENT_DATE, t) explicitly)"
                .into(),
        });
    }
    let a = to_micros(&args[0])?;
    let b = to_micros(&args[1])?;
    let delta = a.checked_sub(b).ok_or(EvalError::TypeMismatch {
        detail: "age() subtraction overflows i64 microseconds".into(),
    })?;
    Ok(Value::Interval {
        months: 0,
        micros: delta,
    })
}

/// `to_char(value, format)` — render a DATE / TIMESTAMP through a PG
/// format template. Supports the high-traffic placeholders:
///   YYYY YY MM Mon Month DD HH24 HH12 MI SS MS US AM PM
/// Unrecognised characters pass through literally so the template's
/// punctuation ('-', ':', ' ', '/') needs no escape mechanism.
fn to_char(args: &[Value]) -> Result<Value, EvalError> {
    use core::fmt::Write as _;
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!("to_char() takes 2 args, got {}", args.len()),
        });
    }
    if matches!(&args[0], Value::Null) || matches!(&args[1], Value::Null) {
        return Ok(Value::Null);
    }
    let Value::Text(fmt) = &args[1] else {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "to_char() needs a text format, got {:?}",
                args[1].data_type()
            ),
        });
    };
    let (days, day_micros) = match &args[0] {
        Value::Date(d) => (*d, 0_i64),
        Value::Timestamp(t) => {
            let days = t.div_euclid(86_400_000_000);
            (
                i32::try_from(days).unwrap_or(i32::MAX),
                t.rem_euclid(86_400_000_000),
            )
        }
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "to_char() needs DATE or TIMESTAMP, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let (y, mo, d) = civil_from_days(days);
    let secs = day_micros / 1_000_000;
    let frac = day_micros % 1_000_000;
    // div_euclid keeps every value non-negative — the casts below are
    // sign-safe by construction. `secs ∈ [0, 86400)`, `frac ∈ [0,
    // 1_000_000)`, so all three quantities fit in u32.
    let hh24 = u32::try_from(secs / 3600).unwrap_or(0);
    let mi = u32::try_from((secs / 60) % 60).unwrap_or(0);
    let ss = u32::try_from(secs % 60).unwrap_or(0);
    let hh12 = match hh24 % 12 {
        0 => 12,
        x => x,
    };
    let ampm = if hh24 < 12 { "AM" } else { "PM" };
    let ms = u32::try_from(frac / 1_000).unwrap_or(0); // millisecond
    let us = u32::try_from(frac).unwrap_or(0); // microsecond (0..1_000_000)

    let mut out = String::with_capacity(fmt.len() + 8);
    let bytes = fmt.as_bytes();
    let mut i = 0;
    // write! against a String never fails — discard the Result.
    while i < bytes.len() {
        // Try the longest prefixes first so "YYYY" wins over "YY".
        let rest = &bytes[i..];
        if rest.starts_with(b"YYYY") {
            let _ = write!(out, "{y:04}");
            i += 4;
        } else if rest.starts_with(b"YY") {
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            let yy = (y.rem_euclid(100)) as u32;
            let _ = write!(out, "{yy:02}");
            i += 2;
        } else if rest.starts_with(b"Month") {
            out.push_str(MONTH_FULL[(mo - 1) as usize]);
            i += 5;
        } else if rest.starts_with(b"Mon") {
            out.push_str(MONTH_ABBR[(mo - 1) as usize]);
            i += 3;
        } else if rest.starts_with(b"MM") {
            let _ = write!(out, "{mo:02}");
            i += 2;
        } else if rest.starts_with(b"DD") {
            let _ = write!(out, "{d:02}");
            i += 2;
        } else if rest.starts_with(b"HH24") {
            let _ = write!(out, "{hh24:02}");
            i += 4;
        } else if rest.starts_with(b"HH12") {
            let _ = write!(out, "{hh12:02}");
            i += 4;
        } else if rest.starts_with(b"MI") {
            let _ = write!(out, "{mi:02}");
            i += 2;
        } else if rest.starts_with(b"SS") {
            let _ = write!(out, "{ss:02}");
            i += 2;
        } else if rest.starts_with(b"MS") {
            let _ = write!(out, "{ms:03}");
            i += 2;
        } else if rest.starts_with(b"US") {
            let _ = write!(out, "{us:06}");
            i += 2;
        } else if rest.starts_with(b"AM") || rest.starts_with(b"PM") {
            out.push_str(ampm);
            i += 2;
        } else {
            // Pass any non-placeholder byte through verbatim.
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    Ok(Value::Text(out))
}

const MONTH_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const MONTH_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `date_trunc(unit, timestamp)` — round a `TIMESTAMP` down to the
/// requested calendar boundary (year / month / day / hour / minute /
/// second). Returns the truncated `TIMESTAMP`. NULL on either side
/// propagates to NULL.
fn date_trunc(args: &[Value]) -> Result<Value, EvalError> {
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!("date_trunc() takes 2 args, got {}", args.len()),
        });
    }
    if matches!(&args[0], Value::Null) || matches!(&args[1], Value::Null) {
        return Ok(Value::Null);
    }
    let Value::Text(unit) = &args[0] else {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "date_trunc() needs a text unit, got {:?}",
                args[0].data_type()
            ),
        });
    };
    // Both DATE and TIMESTAMP sources are accepted. DATE lifts to
    // midnight first; the result is always TIMESTAMP.
    let micros = match &args[1] {
        Value::Timestamp(t) => *t,
        Value::Date(d) => i64::from(*d) * 86_400_000_000,
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "date_trunc() needs DATE or TIMESTAMP, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let unit_lc = unit.to_ascii_lowercase();
    let days = micros.div_euclid(86_400_000_000);
    let day_micros = micros.rem_euclid(86_400_000_000);
    let day_i32 = i32::try_from(days).unwrap_or(i32::MAX);
    let (y, m, _) = civil_from_days(day_i32);
    let truncated = match unit_lc.as_str() {
        "year" => i64::from(days_from_civil(y, 1, 1)) * 86_400_000_000,
        "month" => i64::from(days_from_civil(y, m, 1)) * 86_400_000_000,
        "day" => days * 86_400_000_000,
        "hour" => days * 86_400_000_000 + (day_micros / 3_600_000_000) * 3_600_000_000,
        "minute" => days * 86_400_000_000 + (day_micros / 60_000_000) * 60_000_000,
        "second" => days * 86_400_000_000 + (day_micros / 1_000_000) * 1_000_000,
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "unknown date_trunc unit {other:?}; \
                     supported: year, month, day, hour, minute, second"
                ),
            });
        }
    };
    Ok(Value::Timestamp(truncated))
}

/// PG-style `expr::TYPE` coercion. NULL always casts as NULL.
pub fn cast_value(v: Value, target: CastTarget) -> Result<Value, EvalError> {
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }
    match target {
        CastTarget::Vector => cast_to_vector(v),
        CastTarget::Text => Ok(Value::Text(value_to_text(&v))),
        CastTarget::Int => cast_numeric_to_int(v),
        CastTarget::BigInt => cast_numeric_to_bigint(v),
        CastTarget::Float => cast_numeric_to_float(v),
        CastTarget::Bool => cast_to_bool(v),
        CastTarget::Date => cast_to_date(v),
        // TIMESTAMP and TIMESTAMPTZ have identical runtime
        // representation (i64 microseconds UTC).
        CastTarget::Timestamp | CastTarget::Timestamptz => cast_to_timestamp(v),
        // v7.9.25 — `expr::INTERVAL`. Currently only TEXT → Interval
        // is supported (the mailrs idiom: `$1::INTERVAL` where the
        // bound param is a string like `'7 days'`).
        CastTarget::Interval => cast_to_interval(v),
        // v7.9.25 — `::json` / `::jsonb`. Routes Text → Json
        // (validation is the producer's responsibility, same as
        // the column-INSERT path).
        CastTarget::Json | CastTarget::Jsonb => match v {
            Value::Json(s) => Ok(Value::Json(s)),
            Value::Text(s) => Ok(Value::Json(s)),
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "::json / ::jsonb only accepts TEXT-shape inputs, got {:?}",
                    other.data_type()
                ),
            }),
        },
        // v7.9.26 — `::regtype` / `::regclass`. SPG has no
        // pg_catalog; surface a clear error.
        CastTarget::RegType | CastTarget::RegClass => Err(EvalError::TypeMismatch {
            detail: "::regtype / ::regclass not supported on SPG \
                 (no pg_catalog); use SHOW TABLES / spg_table_ddl instead"
                .into(),
        }),
        // v7.10.11 — `::TEXT[]`. Decode PG external array form
        // when input is Text; pass through unchanged when it is
        // already TextArray. Anything else is a type mismatch.
        CastTarget::TextArray => match v {
            Value::TextArray(items) => Ok(Value::TextArray(items)),
            Value::Text(s) => decode_text_array_external(&s).map(Value::TextArray),
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "::TEXT[] only accepts TEXT / TEXT[] inputs, got {:?}",
                    other.data_type()
                ),
            }),
        },
        // v7.11.13 — `::INT[]` / `::BIGINT[]`. Decode PG external
        // form `{1,2,3}` when input is Text; widen TextArray /
        // IntArray as appropriate.
        CastTarget::IntArray => cast_to_int_array(v),
        CastTarget::BigIntArray => cast_to_bigint_array(v),
        // v7.12.0 — `::tsvector` / `::tsquery`. Decodes PG external
        // form when input is Text; passes through unchanged when the
        // input is already the target type. Other inputs are a type
        // mismatch. Lexer / Porter stemmer arrive in v7.12.1; the
        // external-form cast at v7.12.0 is the path pg_dump and
        // direct-literal callers use.
        CastTarget::TsVector => match v {
            Value::TsVector(items) => Ok(Value::TsVector(items)),
            Value::Text(s) => decode_tsvector_external(&s).map(Value::TsVector),
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "::tsvector only accepts TEXT / tsvector inputs, got {:?}",
                    other.data_type()
                ),
            }),
        },
        CastTarget::TsQuery => match v {
            Value::TsQuery(ast) => Ok(Value::TsQuery(ast)),
            Value::Text(s) => decode_tsquery_external(&s).map(Value::TsQuery),
            other => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "::tsquery only accepts TEXT / tsquery inputs, got {:?}",
                    other.data_type()
                ),
            }),
        },
    }
}

fn cast_to_int_array(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::IntArray(items) => Ok(Value::IntArray(items)),
        Value::BigIntArray(items) => {
            let mut out: Vec<Option<i32>> = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    None => out.push(None),
                    Some(n) => match i32::try_from(n) {
                        Ok(x) => out.push(Some(x)),
                        Err(_) => {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!("::INT[] element {n} overflows i32"),
                            });
                        }
                    },
                }
            }
            Ok(Value::IntArray(out))
        }
        Value::Text(s) => decode_int_array_external(&s).map(Value::IntArray),
        Value::TextArray(items) => {
            let mut out: Vec<Option<i32>> = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    None => out.push(None),
                    Some(s) => match s.parse::<i32>() {
                        Ok(n) => out.push(Some(n)),
                        Err(_) => {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!("::INT[] cannot parse {s:?}"),
                            });
                        }
                    },
                }
            }
            Ok(Value::IntArray(out))
        }
        other => Err(EvalError::TypeMismatch {
            detail: alloc::format!("::INT[] does not accept {:?}", other.data_type()),
        }),
    }
}

fn cast_to_bigint_array(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::BigIntArray(items) => Ok(Value::BigIntArray(items)),
        Value::IntArray(items) => Ok(Value::BigIntArray(
            items.into_iter().map(|x| x.map(i64::from)).collect(),
        )),
        Value::Text(s) => decode_bigint_array_external(&s).map(Value::BigIntArray),
        Value::TextArray(items) => {
            let mut out: Vec<Option<i64>> = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    None => out.push(None),
                    Some(s) => match s.parse::<i64>() {
                        Ok(n) => out.push(Some(n)),
                        Err(_) => {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!("::BIGINT[] cannot parse {s:?}"),
                            });
                        }
                    },
                }
            }
            Ok(Value::BigIntArray(out))
        }
        other => Err(EvalError::TypeMismatch {
            detail: alloc::format!("::BIGINT[] does not accept {:?}", other.data_type()),
        }),
    }
}

fn decode_int_array_external(s: &str) -> Result<Vec<Option<i32>>, EvalError> {
    let trimmed = s.trim();
    let inner = trimmed
        .strip_prefix('{')
        .and_then(|x| x.strip_suffix('}'))
        .ok_or_else(|| EvalError::TypeMismatch {
            detail: alloc::format!("INT[] literal {s:?} must be enclosed in '{{...}}'"),
        })?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|part| {
            let p = part.trim();
            if p.eq_ignore_ascii_case("NULL") {
                Ok(None)
            } else {
                p.parse::<i32>()
                    .map(Some)
                    .map_err(|_| EvalError::TypeMismatch {
                        detail: alloc::format!("INT[] element {p:?} is not an i32"),
                    })
            }
        })
        .collect()
}

fn decode_bigint_array_external(s: &str) -> Result<Vec<Option<i64>>, EvalError> {
    let trimmed = s.trim();
    let inner = trimmed
        .strip_prefix('{')
        .and_then(|x| x.strip_suffix('}'))
        .ok_or_else(|| EvalError::TypeMismatch {
            detail: alloc::format!("BIGINT[] literal {s:?} must be enclosed in '{{...}}'"),
        })?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|part| {
            let p = part.trim();
            if p.eq_ignore_ascii_case("NULL") {
                Ok(None)
            } else {
                p.parse::<i64>()
                    .map(Some)
                    .map_err(|_| EvalError::TypeMismatch {
                        detail: alloc::format!("BIGINT[] element {p:?} is not an i64"),
                    })
            }
        })
        .collect()
}

/// v7.10.11 — same decoder as `decode_text_array_literal` in
/// `lib.rs`, but lives here so the eval-time cast path stays
/// inside `spg-engine::eval`. Kept in lock-step with the engine
/// `coerce_value` decoder by tests.
fn decode_text_array_external(s: &str) -> Result<Vec<Option<String>>, EvalError> {
    let trimmed = s.trim();
    let inner = trimmed
        .strip_prefix('{')
        .and_then(|x| x.strip_suffix('}'))
        .ok_or_else(|| EvalError::TypeMismatch {
            detail: alloc::format!("TEXT[] literal {s:?} must be enclosed in '{{...}}'"),
        })?;
    let mut out: Vec<Option<String>> = Vec::new();
    if inner.trim().is_empty() {
        return Ok(out);
    }
    let bytes = inner.as_bytes();
    let mut i = 0;
    while i <= bytes.len() {
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'"' {
            i += 1;
            let mut buf = String::new();
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    buf.push(bytes[i + 1] as char);
                    i += 2;
                } else {
                    buf.push(bytes[i] as char);
                    i += 1;
                }
            }
            if i >= bytes.len() {
                return Err(EvalError::TypeMismatch {
                    detail: "unterminated quoted element in TEXT[] literal".into(),
                });
            }
            i += 1;
            out.push(Some(buf));
        } else {
            let start = i;
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            let raw = inner[start..i].trim();
            if raw.eq_ignore_ascii_case("NULL") {
                out.push(None);
            } else {
                out.push(Some(raw.to_string()));
            }
        }
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] != b',' {
            return Err(EvalError::TypeMismatch {
                detail: "expected ',' between TEXT[] elements".into(),
            });
        }
        i += 1;
    }
    Ok(out)
}

fn cast_to_interval(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::Interval { months, micros } => Ok(Value::Interval { months, micros }),
        Value::Text(s) => {
            let (months, micros) = spg_sql::parser::parse_interval_text(&s).ok_or_else(|| {
                EvalError::TypeMismatch {
                    detail: alloc::format!("cannot parse {s:?} as INTERVAL"),
                }
            })?;
            Ok(Value::Interval { months, micros })
        }
        other => Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "::INTERVAL only accepts TEXT-shape inputs, got {:?}",
                other.data_type()
            ),
        }),
    }
}

fn cast_to_date(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::Date(d) => Ok(Value::Date(d)),
        // Integer literals carry days since the Unix epoch — used by
        // the `CURRENT_DATE` AST rewrite to inject the wall clock.
        Value::Int(n) => Ok(Value::Date(n)),
        Value::BigInt(n) => {
            i32::try_from(n)
                .map(Value::Date)
                .map_err(|_| EvalError::TypeMismatch {
                    detail: "bigint days-since-epoch out of DATE range".into(),
                })
        }
        // Timestamp truncates to its day boundary.
        Value::Timestamp(t) => {
            let days = t.div_euclid(86_400_000_000);
            i32::try_from(days)
                .map(Value::Date)
                .map_err(|_| EvalError::TypeMismatch {
                    detail: "timestamp out of DATE range".into(),
                })
        }
        Value::Text(s) => parse_date_literal(&s)
            .map(Value::Date)
            .ok_or(EvalError::TypeMismatch {
                detail: format!("cannot parse {s:?} as DATE (expected YYYY-MM-DD)"),
            }),
        other => Err(EvalError::TypeMismatch {
            detail: format!("cannot cast {:?} to DATE", other.data_type()),
        }),
    }
}

fn cast_to_timestamp(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::Timestamp(t) => Ok(Value::Timestamp(t)),
        // Int / BigInt carry microseconds since the Unix epoch — used
        // by the `NOW()` / `CURRENT_TIMESTAMP` AST rewrite to inject
        // the wall clock as a plain integer literal.
        Value::Int(n) => Ok(Value::Timestamp(i64::from(n))),
        Value::BigInt(n) => Ok(Value::Timestamp(n)),
        // DATE → TIMESTAMP picks midnight on the date.
        Value::Date(d) => Ok(Value::Timestamp(i64::from(d) * 86_400_000_000)),
        Value::Text(s) => {
            parse_timestamp_literal(&s)
                .map(Value::Timestamp)
                .ok_or(EvalError::TypeMismatch {
                    detail: format!(
                        "cannot parse {s:?} as TIMESTAMP \
                     (expected YYYY-MM-DD[ HH:MM:SS[.ffffff]])"
                    ),
                })
        }
        other => Err(EvalError::TypeMismatch {
            detail: format!("cannot cast {:?} to TIMESTAMP", other.data_type()),
        }),
    }
}

fn value_to_text(v: &Value) -> String {
    match v {
        // v7.5.0 — Value is #[non_exhaustive]; any future variant
        // without explicit text rendering hits the Debug fallback
        // at the end.
        Value::SmallInt(n) => format!("{n}"),
        Value::Int(n) => format!("{n}"),
        Value::BigInt(n) => format!("{n}"),
        Value::Float(x) => format!("{x}"),
        // v4.9: JSON renders identically to Text — both are raw UTF-8.
        Value::Text(s) | Value::Json(s) => s.clone(),
        Value::Bool(b) => (if *b { "true" } else { "false" }).into(),
        Value::Vector(v) => {
            let cells: Vec<String> = v.iter().map(|x| format!("{x}")).collect();
            format!("[{}]", cells.join(", "))
        }
        // v6.0.1: render SQ8 cells dequantised, so SELECT output
        // matches the pgvector wire shape clients expect. The
        // recall envelope already absorbs the ≤ (max-min)/255/2
        // dequantisation error.
        Value::Sq8Vector(q) => {
            let cells: Vec<String> = spg_storage::quantize::dequantize(q)
                .iter()
                .map(|x| format!("{x}"))
                .collect();
            format!("[{}]", cells.join(", "))
        }
        // v6.0.3: HalfVector cells dequantise bit-exactly to f32
        // for SELECT output.
        Value::HalfVector(h) => {
            let cells: Vec<String> = h.to_f32_vec().iter().map(|x| format!("{x}")).collect();
            format!("[{}]", cells.join(", "))
        }
        Value::Numeric { scaled, scale } => format_numeric(*scaled, *scale),
        Value::Date(d) => format_date(*d),
        Value::Timestamp(t) => format_timestamp(*t),
        Value::Interval { months, micros } => format_interval(*months, *micros),
        Value::Null => "NULL".into(),
        // v7.10.4 — BYTEA renders as PG hex form.
        Value::Bytes(b) => format_bytea_hex(b),
        // v7.10.9 — TEXT[] / INT[] / BIGINT[] render PG external form.
        Value::TextArray(items) => format_text_array(items),
        Value::IntArray(items) => format_int_array(items),
        Value::BigIntArray(items) => format_bigint_array(items),
        // v7.12.0 — tsvector / tsquery render PG external form.
        Value::TsVector(lexs) => format_tsvector(lexs),
        Value::TsQuery(ast) => format_tsquery(ast),
        // v7.5.0 — #[non_exhaustive] fallback for future Value variants.
        _ => format!("{v:?}"),
    }
}

/// Render a `Date` (days since epoch) as `YYYY-MM-DD`. Negative values
/// for pre-1970 dates render with a leading `-` on the year.
pub fn format_date(days: i32) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Render a `Timestamp` (microseconds since epoch) as
/// `YYYY-MM-DD HH:MM:SS[.fff...]`. Trailing-zero fractional digits are
/// dropped; a whole-second value has no fractional part.
/// v7.15.0 — PG-canonical TIMESTAMPTZ wire format. Storage is
/// the same i64 microseconds UTC as TIMESTAMP, but the canonical
/// PG text output appends the session's UTC-offset suffix (`+00`
/// for the default UTC session, the form pg_dump emits). Mailrs
/// round-8 acceptance criterion: `SELECT col FROM tstz` should
/// round-trip to a literal that re-INSERTs without semantic
/// drift.
pub fn format_timestamptz(micros: i64) -> String {
    let base = format_timestamp(micros);
    let mut s = String::with_capacity(base.len() + 3);
    s.push_str(&base);
    s.push_str("+00");
    s
}

pub fn format_timestamp(micros: i64) -> String {
    const MICROS_PER_DAY: i64 = 86_400_000_000;
    // Split into day + intra-day part with proper floor division so
    // negative timestamps render right too.
    let days = micros.div_euclid(MICROS_PER_DAY);
    let day_micros = micros.rem_euclid(MICROS_PER_DAY);
    let day_i32 = i32::try_from(days).unwrap_or(i32::MAX);
    let (y, m, d) = civil_from_days(day_i32);
    let secs = day_micros / 1_000_000;
    let frac = day_micros % 1_000_000;
    let hh = secs / 3600;
    let mm = (secs / 60) % 60;
    let ss = secs % 60;
    if frac == 0 {
        format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
    } else {
        // Strip trailing zeros from the 6-digit fractional component.
        let raw = format!("{frac:06}");
        let trimmed = raw.trim_end_matches('0');
        format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}.{trimmed}")
    }
}

/// Howard Hinnant's `civil_from_days` — converts days since the Unix
/// epoch back to a proleptic-Gregorian (year, month, day) triple. Both
/// directions of this calendar conversion live in `eval.rs` so the
/// engine never reaches for `std` time facilities.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn civil_from_days(days: i32) -> (i32, u32, u32) {
    let z = i64::from(days) + 719_468;
    let era = z.div_euclid(146_097);
    // doe ∈ [0, 146_097); fits in u32 with room to spare. Same for
    // every other quantity below — `as u32` truncations are safe by
    // construction.
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe.saturating_sub(doe / 1460) + doe / 36524 - doe / 146_096) / 365;
    let y_base = i64::from(yoe) + era * 400;
    let doy = doe.saturating_sub(365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy.saturating_sub((153 * mp + 2) / 5) + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y_base + 1 } else { y_base };
    (y as i32, m, d)
}

/// Inverse of `civil_from_days` — converts (year, month, day) to days
/// since 1970-01-01. Out-of-range months / days saturate.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i32 {
    let y_adj = if m <= 2 {
        i64::from(y) - 1
    } else {
        i64::from(y)
    };
    let era = y_adj.div_euclid(400);
    let yoe = (y_adj - era * 400) as u32;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d.saturating_sub(1);
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let total = era * 146_097 + i64::from(doe) - 719_468;
    i32::try_from(total).unwrap_or(i32::MAX)
}

/// Parse `YYYY-MM-DD` into a `Date` (days since Unix epoch). Returns
/// `None` on shape / numeric failure; the engine surfaces that as a
/// `TypeMismatch` with the original text included.
pub fn parse_date_literal(s: &str) -> Option<i32> {
    let bytes = s.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let y: i32 = s[0..4].parse().ok()?;
    let m: u32 = s[5..7].parse().ok()?;
    let d: u32 = s[8..10].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// Parse `YYYY-MM-DD[ HH:MM:SS[.ffffff]]` into a `Timestamp`
/// (microseconds since Unix epoch). The time portion is optional;
/// missing → midnight. The fractional portion accepts 1–6 digits and
/// pads with zeros to microseconds.
pub fn parse_timestamp_literal(s: &str) -> Option<i64> {
    let trimmed = s.trim();
    let (date_part, time_part) = match trimmed.find([' ', 'T']) {
        Some(i) => (&trimmed[..i], Some(&trimmed[i + 1..])),
        None => (trimmed, None),
    };
    let days = parse_date_literal(date_part)?;
    let (day_micros, tz_offset_micros) = match time_part {
        None => (0, 0),
        Some(t) => parse_time_of_day_micros(t)?,
    };
    // PG semantics: a TIMESTAMPTZ literal with an explicit offset
    // is normalised to UTC for storage. `'12:00:00+09'` means
    // 12:00:00 in a UTC+09 zone → 03:00:00 UTC → subtract the
    // positive offset (or add the negative one). Storage is i64
    // microseconds UTC for both TIMESTAMP and TIMESTAMPTZ (see
    // spg-storage::DataType::Timestamptz docs); the wire-level
    // round-trip then re-applies the session timezone on the
    // SELECT side when format_timestamp is asked for a TZ-aware
    // render.
    Some(i64::from(days) * 86_400_000_000 + day_micros - tz_offset_micros)
}

/// v7.15.0 — Parse `HH:MM:SS[.frac][<tz>]` and return
/// `(day_micros, tz_offset_micros)` where `day_micros` is the
/// local-clock seconds-of-day in microseconds and
/// `tz_offset_micros` is the UTC offset (positive = east of
/// UTC, negative = west). Caller subtracts the offset to
/// normalise to UTC. PG's recognised TZ shapes after the
/// seconds (or frac) part:
///   * `+OO[:MM]` / `-OO[:MM]` — numeric offset
///   * `+OOMM` / `-OOMM` (no colon, less common but legal)
///   * ` UTC` / `UTC` / `Z` — explicit zero offset
/// Anything else after the seconds = parse failure (the caller
/// surfaces as "cannot parse … as TIMESTAMP").
fn parse_time_of_day_micros(t: &str) -> Option<(i64, i64)> {
    let t = t.trim();
    // Detect & strip optional TZ suffix. Anchor on the first
    // `+` / `-` AFTER position 8 (so the leading sign on a
    // negative offset can't be mistaken for an `HH:MM:SS-OO`
    // boundary if the time itself is somehow malformed).
    // ` UTC` and trailing `Z` also count as zero-offset TZ tags.
    let (core, tz_micros) = if let Some(rest) = t.strip_suffix('Z') {
        (rest, 0i64)
    } else if let Some(rest) = t.strip_suffix(" UTC").or_else(|| t.strip_suffix("UTC")) {
        (rest, 0i64)
    } else if let Some((idx, sign_byte)) = find_offset_sign(t) {
        let suffix = &t[idx..];
        let micros = parse_tz_offset_suffix(suffix, sign_byte == b'+')?;
        (&t[..idx], micros)
    } else {
        (t, 0i64)
    };
    let (time, frac_str) = match core.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (core, None),
    };
    let bytes = time.as_bytes();
    if bytes.len() != 8 || bytes[2] != b':' || bytes[5] != b':' {
        return None;
    }
    let hh: i64 = time[0..2].parse().ok()?;
    let mm: i64 = time[3..5].parse().ok()?;
    let ss: i64 = time[6..8].parse().ok()?;
    if !(0..24).contains(&hh) || !(0..60).contains(&mm) || !(0..60).contains(&ss) {
        return None;
    }
    let frac_micros: i64 = match frac_str {
        None => 0,
        Some(f) => {
            // Pad right with zeros to 6 digits, then truncate extras.
            if f.is_empty() || f.len() > 9 {
                return None;
            }
            let mut padded = String::with_capacity(6);
            padded.push_str(&f[..f.len().min(6)]);
            while padded.len() < 6 {
                padded.push('0');
            }
            padded.parse().ok()?
        }
    };
    Some((
        ((hh * 3600 + mm * 60 + ss) * 1_000_000) + frac_micros,
        tz_micros,
    ))
}

/// Find the index of the TZ-offset sign byte (`+` or `-`) that
/// terminates an `HH:MM:SS[.fff]` time string, or `None` when
/// the time carries no numeric TZ suffix. Anchors past the first
/// 8 bytes (`HH:MM:SS`) so the seconds/minutes colons don't
/// confuse the scan.
fn find_offset_sign(t: &str) -> Option<(usize, u8)> {
    let bytes = t.as_bytes();
    // Start past `HH:MM:SS` (8 bytes).
    if bytes.len() < 9 {
        return None;
    }
    for i in 8..bytes.len() {
        match bytes[i] {
            b'+' | b'-' => return Some((i, bytes[i])),
            _ => {}
        }
    }
    None
}

/// Parse `+OO`, `+OO:MM`, `+OOMM`, `-OO`, `-OO:MM`, `-OOMM` into
/// a UTC-offset microsecond delta. `is_positive` reflects the
/// already-stripped sign.
fn parse_tz_offset_suffix(suffix: &str, is_positive: bool) -> Option<i64> {
    // suffix starts with `+` or `-`; strip it.
    let body = &suffix[1..];
    let (hh, mm): (i64, i64) = if let Some((h, m)) = body.split_once(':') {
        (h.parse().ok()?, m.parse().ok()?)
    } else {
        match body.len() {
            2 => (body.parse().ok()?, 0),
            3 => {
                // PG's "+0530" form lacks the colon; but a 3-char
                // body is `OOM` which is ambiguous (`+053` ?). PG
                // doesn't emit that; reject.
                return None;
            }
            4 => {
                let h: i64 = body[0..2].parse().ok()?;
                let m: i64 = body[2..4].parse().ok()?;
                (h, m)
            }
            _ => return None,
        }
    };
    if !(0..=18).contains(&hh) || !(0..60).contains(&mm) {
        return None;
    }
    let abs = (hh * 3600 + mm * 60) * 1_000_000;
    Some(if is_positive { abs } else { -abs })
}

/// Render an `Interval { months, micros }` in a PG-ish shape. The output
/// mirrors `psql`'s text format: years/months from the months part,
/// days/HH:MM:SS[.frac] from the microsecond part. Empty parts are
/// omitted; an all-zero interval renders as `0`.
pub fn format_interval(months: i32, micros: i64) -> String {
    const MICROS_PER_DAY: i64 = 86_400_000_000;
    let mut parts: Vec<String> = Vec::new();
    let years = months / 12;
    let mons = months % 12;
    // PG renders the unit in the singular only for `+1`; `-1` and any
    // other value pluralise. Helper closes over that rule.
    let unit = |n: i64, singular: &'static str, plural: &'static str| -> &'static str {
        if n == 1 { singular } else { plural }
    };
    if years != 0 {
        parts.push(format!(
            "{years} {}",
            unit(i64::from(years), "year", "years")
        ));
    }
    if mons != 0 {
        parts.push(format!("{mons} {}", unit(i64::from(mons), "mon", "mons")));
    }
    let days = micros / MICROS_PER_DAY;
    let mut rem = micros % MICROS_PER_DAY;
    if days != 0 {
        parts.push(format!("{days} {}", unit(days, "day", "days")));
    }
    if rem != 0 {
        let neg = rem < 0;
        if neg {
            rem = -rem;
        }
        let secs = rem / 1_000_000;
        let frac = rem % 1_000_000;
        let hh = secs / 3600;
        let mm = (secs / 60) % 60;
        let ss = secs % 60;
        let sign = if neg { "-" } else { "" };
        if frac == 0 {
            parts.push(format!("{sign}{hh:02}:{mm:02}:{ss:02}"));
        } else {
            let raw = format!("{frac:06}");
            let trimmed = raw.trim_end_matches('0');
            parts.push(format!("{sign}{hh:02}:{mm:02}:{ss:02}.{trimmed}"));
        }
    }
    if parts.is_empty() {
        "0".into()
    } else {
        parts.join(" ")
    }
}

/// Add `months` (signed) to a `(year, month, day)` triple using PG's
/// clamp-to-last-day rule (so `'2024-01-31' + 1 month` → `'2024-02-29'`).
fn add_months_to_civil(y: i32, m: u32, d: u32, months: i32) -> (i32, u32, u32) {
    let total_months = i64::from(y) * 12 + i64::from(m) - 1 + i64::from(months);
    let new_year = i32::try_from(total_months.div_euclid(12)).unwrap_or(i32::MAX);
    let new_month_zero = total_months.rem_euclid(12);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let new_month = (new_month_zero as u32) + 1;
    let max_day = days_in_month(new_year, new_month);
    (new_year, new_month, d.min(max_day))
}

const fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        2 => {
            // Proleptic Gregorian leap rule.
            if y.rem_euclid(4) == 0 && (y.rem_euclid(100) != 0 || y.rem_euclid(400) == 0) {
                29
            } else {
                28
            }
        }
        // 4 / 6 / 9 / 11 plus any out-of-range month (callers normalise
        // first, but be defensive) get the 30-day fallback.
        _ => 30,
    }
}

/// v7.10.9 — render a TEXT[] in PG's external array form
/// (`{a,b,NULL}`). Elements containing whitespace, commas,
/// quotes, or braces get double-quoted with `\\` / `\"` escapes.
/// NULL elements use the literal token `NULL`. Public so the
/// wire layer can produce the canonical text-mode encoding.
pub fn format_text_array(items: &[Option<String>]) -> String {
    let mut out = String::with_capacity(2 + items.len() * 8);
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            Some(s) => {
                let needs_quote = s.is_empty()
                    || s.eq_ignore_ascii_case("NULL")
                    || s.chars()
                        .any(|c| matches!(c, ',' | '{' | '}' | '"' | '\\' | ' ' | '\t'));
                if needs_quote {
                    out.push('"');
                    for c in s.chars() {
                        if c == '"' || c == '\\' {
                            out.push('\\');
                        }
                        out.push(c);
                    }
                    out.push('"');
                } else {
                    out.push_str(s);
                }
            }
        }
    }
    out.push('}');
    out
}

/// v7.11.14 — render an INT[] in PG's external array form
/// (`{1,2,NULL}`). Integer payloads never need quoting. NULL
/// elements use the literal token `NULL`.
pub fn format_int_array(items: &[Option<i32>]) -> String {
    let mut out = String::with_capacity(2 + items.len() * 4);
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            Some(n) => out.push_str(&n.to_string()),
        }
    }
    out.push('}');
    out
}

/// v7.11.14 — render a BIGINT[] in PG's external array form
/// (`{1,2,NULL}`).
pub fn format_bigint_array(items: &[Option<i64>]) -> String {
    let mut out = String::with_capacity(2 + items.len() * 6);
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            Some(n) => out.push_str(&n.to_string()),
        }
    }
    out.push('}');
    out
}

/// v7.12.0 — render a `tsvector` in PG's external form:
/// `'lex':1,2A 'word':3` (single-quoted lexemes, optional
/// `:positions`, optional weight letter `A/B/C/D` per position).
/// Lexemes already arrive sorted + deduped from the engine. Used
/// by the wire layer (OID 3614) and by SELECT-text output.
pub fn format_tsvector(lexs: &[TsLexeme]) -> String {
    let mut out = String::with_capacity(lexs.len() * 12);
    for (i, l) in lexs.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push('\'');
        for c in l.word.chars() {
            if c == '\'' {
                out.push('\'');
            }
            out.push(c);
        }
        out.push('\'');
        if !l.positions.is_empty() {
            for (pi, p) in l.positions.iter().enumerate() {
                out.push(if pi == 0 { ':' } else { ',' });
                out.push_str(&p.to_string());
            }
            // v7.12.0 — weight is per-lexeme (the v7.12 design
            // collapses PG's per-position weight into one letter).
            // Emit once after the last position; default `D`
            // (weight=0) stays implicit.
            match l.weight {
                3 => out.push('A'),
                2 => out.push('B'),
                1 => out.push('C'),
                _ => {}
            }
        }
    }
    out
}

/// v7.12.0 — render a `tsquery` in PG's external form. Operator
/// precedence: `!` > `&` > `|`. Phrase distance shown as `<N>`.
pub fn format_tsquery(ast: &TsQueryAst) -> String {
    fn go(ast: &TsQueryAst, parent_prec: u8, out: &mut String) {
        // 0 = top, 1 = OR, 2 = AND, 3 = NOT/Phrase, 4 = atom.
        let (own_prec, write_self): (u8, &dyn Fn(&mut String)) = match ast {
            TsQueryAst::Or(_, _) => (1, &|_| {}),
            TsQueryAst::And(_, _) | TsQueryAst::Phrase { .. } => (2, &|_| {}),
            TsQueryAst::Not(_) => (3, &|_| {}),
            TsQueryAst::Term { .. } => (4, &|_| {}),
        };
        let need_parens = own_prec < parent_prec;
        if need_parens {
            out.push('(');
        }
        match ast {
            TsQueryAst::Term { word, .. } => {
                out.push('\'');
                for c in word.chars() {
                    if c == '\'' {
                        out.push('\'');
                    }
                    out.push(c);
                }
                out.push('\'');
            }
            TsQueryAst::And(a, b) => {
                go(a, own_prec, out);
                out.push_str(" & ");
                go(b, own_prec, out);
            }
            TsQueryAst::Or(a, b) => {
                go(a, own_prec, out);
                out.push_str(" | ");
                go(b, own_prec, out);
            }
            TsQueryAst::Not(x) => {
                out.push('!');
                go(x, own_prec, out);
            }
            TsQueryAst::Phrase {
                left,
                right,
                distance,
            } => {
                go(left, own_prec, out);
                out.push_str(&alloc::format!(" <{distance}> "));
                go(right, own_prec, out);
            }
        }
        write_self(out);
        if need_parens {
            out.push(')');
        }
    }
    let mut out = String::new();
    go(ast, 0, &mut out);
    out
}

/// v7.12.0 — decode PG external form `'word':1,2A 'other':3` into
/// a `Vec<TsLexeme>`. Lexemes are sorted ascending by `word` (with
/// duplicates merged on positions) so the output matches the
/// engine invariant. Empty input yields an empty vector.
///
/// v7.12.0 only ships the cast-literal entry. Full `to_tsvector`
/// (Unicode word-split + Porter stemming + stopwords) lands in
/// v7.12.1.
pub fn decode_tsvector_external(s: &str) -> Result<Vec<TsLexeme>, EvalError> {
    let mut out: Vec<TsLexeme> = Vec::new();
    let mut i = 0;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        // Quoted form `'word'` (with embedded `''` for a literal
        // single quote, mirroring PG).
        let word = if bytes[i] == b'\'' {
            i += 1;
            let mut w = String::new();
            loop {
                if i >= bytes.len() {
                    return Err(EvalError::TypeMismatch {
                        detail: "tsvector literal: unterminated quoted lexeme".into(),
                    });
                }
                let b = bytes[i];
                if b == b'\'' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        w.push('\'');
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    w.push(b as char);
                    i += 1;
                }
            }
            w
        } else {
            // Bare form — read until whitespace, ':' or end.
            let start = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b':' {
                i += 1;
            }
            core::str::from_utf8(&bytes[start..i])
                .map_err(|_| EvalError::TypeMismatch {
                    detail: "tsvector literal: non-UTF-8 lexeme".into(),
                })?
                .to_string()
        };
        if word.is_empty() {
            return Err(EvalError::TypeMismatch {
                detail: "tsvector literal: empty lexeme".into(),
            });
        }
        // Optional `:pos[,pos][,pos]`. Each position is u16; each
        // may carry a trailing weight letter A/B/C/D.
        let mut positions: Vec<u16> = Vec::new();
        let mut weight: u8 = 0;
        if i < bytes.len() && bytes[i] == b':' {
            i += 1;
            loop {
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if start == i {
                    return Err(EvalError::TypeMismatch {
                        detail: "tsvector literal: expected digit after ':'".into(),
                    });
                }
                let num: u16 = core::str::from_utf8(&bytes[start..i])
                    .expect("ascii digits")
                    .parse()
                    .map_err(|_| EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "tsvector literal: position {} overflows u16",
                            core::str::from_utf8(&bytes[start..i]).unwrap_or("?")
                        ),
                    })?;
                positions.push(num);
                if i < bytes.len() {
                    let w = bytes[i];
                    if matches!(w, b'A' | b'B' | b'C' | b'D') {
                        weight = match w {
                            b'A' => 3,
                            b'B' => 2,
                            b'C' => 1,
                            _ => 0,
                        };
                        i += 1;
                    }
                }
                if i < bytes.len() && bytes[i] == b',' {
                    i += 1;
                    continue;
                }
                break;
            }
        }
        positions.sort_unstable();
        positions.dedup();
        // Merge into the output vector — sorted insert by word,
        // duplicate words merge positions.
        match out.binary_search_by(|l| l.word.as_str().cmp(word.as_str())) {
            Ok(idx) => {
                for p in positions {
                    if !out[idx].positions.contains(&p) {
                        out[idx].positions.push(p);
                    }
                }
                out[idx].positions.sort_unstable();
                if weight != 0 {
                    out[idx].weight = weight;
                }
            }
            Err(idx) => {
                out.insert(
                    idx,
                    TsLexeme {
                        word,
                        positions,
                        weight,
                    },
                );
            }
        }
    }
    Ok(out)
}

/// v7.12.0 — decode PG external form `'foo' & 'bar' | !'baz'`
/// into a `TsQueryAst`. v7.12.0 supports the canonical
/// `to_tsquery` surface: single-quoted lexemes, `&` / `|` / `!`,
/// parens, and phrase `<N>`. Bare lexemes are accepted too. Full
/// `plainto_tsquery` / `websearch_to_tsquery` arrive in v7.12.1.
pub fn decode_tsquery_external(s: &str) -> Result<TsQueryAst, EvalError> {
    let mut p = TsQueryParser {
        bytes: s.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    if p.pos >= p.bytes.len() {
        return Err(EvalError::TypeMismatch {
            detail: "tsquery literal: empty".into(),
        });
    }
    let ast = p.parse_or()?;
    p.skip_ws();
    if p.pos < p.bytes.len() {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("tsquery literal: trailing garbage at offset {}", p.pos),
        });
    }
    Ok(ast)
}

struct TsQueryParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> TsQueryParser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }
    fn parse_or(&mut self) -> Result<TsQueryAst, EvalError> {
        let mut lhs = self.parse_and()?;
        loop {
            self.skip_ws();
            if self.peek() != Some(b'|') {
                return Ok(lhs);
            }
            self.pos += 1;
            let rhs = self.parse_and()?;
            lhs = TsQueryAst::Or(Box::new(lhs), Box::new(rhs));
        }
    }
    fn parse_and(&mut self) -> Result<TsQueryAst, EvalError> {
        let mut lhs = self.parse_unary()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'&') => {
                    self.pos += 1;
                    let rhs = self.parse_unary()?;
                    lhs = TsQueryAst::And(Box::new(lhs), Box::new(rhs));
                }
                Some(b'<') => {
                    // Phrase distance `<N>`.
                    self.pos += 1;
                    let start = self.pos;
                    while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_digit() {
                        self.pos += 1;
                    }
                    if start == self.pos || self.peek() != Some(b'>') {
                        return Err(EvalError::TypeMismatch {
                            detail: "tsquery literal: malformed <N> phrase operator".into(),
                        });
                    }
                    let n: u16 = core::str::from_utf8(&self.bytes[start..self.pos])
                        .expect("ascii digits")
                        .parse()
                        .map_err(|_| EvalError::TypeMismatch {
                            detail: "tsquery literal: phrase distance overflows u16".into(),
                        })?;
                    self.pos += 1; // consume '>'
                    let rhs = self.parse_unary()?;
                    lhs = TsQueryAst::Phrase {
                        left: Box::new(lhs),
                        right: Box::new(rhs),
                        distance: n,
                    };
                }
                _ => return Ok(lhs),
            }
        }
    }
    fn parse_unary(&mut self) -> Result<TsQueryAst, EvalError> {
        self.skip_ws();
        if self.peek() == Some(b'!') {
            self.pos += 1;
            let inner = self.parse_unary()?;
            return Ok(TsQueryAst::Not(Box::new(inner)));
        }
        self.parse_atom()
    }
    fn parse_atom(&mut self) -> Result<TsQueryAst, EvalError> {
        self.skip_ws();
        match self.peek() {
            Some(b'(') => {
                self.pos += 1;
                let inner = self.parse_or()?;
                self.skip_ws();
                if self.peek() != Some(b')') {
                    return Err(EvalError::TypeMismatch {
                        detail: "tsquery literal: missing ')'".into(),
                    });
                }
                self.pos += 1;
                Ok(inner)
            }
            Some(b'\'') => {
                self.pos += 1;
                let mut w = String::new();
                loop {
                    match self.peek() {
                        None => {
                            return Err(EvalError::TypeMismatch {
                                detail: "tsquery literal: unterminated quoted lexeme".into(),
                            });
                        }
                        Some(b'\'') => {
                            if self.bytes.get(self.pos + 1) == Some(&b'\'') {
                                w.push('\'');
                                self.pos += 2;
                            } else {
                                self.pos += 1;
                                break;
                            }
                        }
                        Some(b) => {
                            w.push(b as char);
                            self.pos += 1;
                        }
                    }
                }
                // Optional `:WEIGHT_MASK` (digit-mask) — v7.12.0
                // accepts but always stores 0 (any).
                self.skip_weight_suffix();
                Ok(TsQueryAst::Term {
                    word: w,
                    weight_mask: 0,
                })
            }
            Some(b) if b.is_ascii_alphanumeric() || b == b'_' => {
                let start = self.pos;
                while self.pos < self.bytes.len() {
                    let c = self.bytes[self.pos];
                    if c.is_ascii_alphanumeric() || c == b'_' {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                let w = core::str::from_utf8(&self.bytes[start..self.pos])
                    .map_err(|_| EvalError::TypeMismatch {
                        detail: "tsquery literal: non-UTF-8 lexeme".into(),
                    })?
                    .to_string();
                self.skip_weight_suffix();
                Ok(TsQueryAst::Term {
                    word: w,
                    weight_mask: 0,
                })
            }
            Some(b) => Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "tsquery literal: unexpected byte {:?} at offset {}",
                    b as char,
                    self.pos
                ),
            }),
            None => Err(EvalError::TypeMismatch {
                detail: "tsquery literal: expected term".into(),
            }),
        }
    }
    fn skip_weight_suffix(&mut self) {
        if self.peek() != Some(b':') {
            return;
        }
        self.pos += 1;
        while let Some(b) = self.peek() {
            if matches!(
                b,
                b'A' | b'B' | b'C' | b'D' | b'a' | b'b' | b'c' | b'd' | b'*'
            ) || b.is_ascii_digit()
            {
                self.pos += 1;
            } else {
                break;
            }
        }
    }
}

/// v7.10.4 — render a BYTEA payload in PG's hex output format
/// (`\x` prefix, lowercase hex pairs). Public so the wire layer
/// can emit the canonical bytea-as-text representation.
pub fn format_bytea_hex(b: &[u8]) -> String {
    let mut out = String::with_capacity(2 + 2 * b.len());
    out.push_str("\\x");
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in b {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0F) as usize] as char);
    }
    out
}

/// Render a `Numeric { scaled, scale }` as its decimal text form.
/// Negative `scaled` prepends `-` to the absolute value's digits; the
/// integer / fractional split is by character count, padding the
/// fractional side with leading zeros to exactly `scale` chars.
pub fn format_numeric(scaled: i128, scale: u8) -> String {
    if scale == 0 {
        return format!("{scaled}");
    }
    let negative = scaled < 0;
    let mag_str = scaled.unsigned_abs().to_string();
    let mag_bytes = mag_str.as_bytes();
    let scale_u = scale as usize;
    let mut out = String::with_capacity(mag_str.len() + 3);
    if negative {
        out.push('-');
    }
    if mag_bytes.len() <= scale_u {
        out.push('0');
        out.push('.');
        for _ in mag_bytes.len()..scale_u {
            out.push('0');
        }
        out.push_str(&mag_str);
    } else {
        let split = mag_bytes.len() - scale_u;
        out.push_str(&mag_str[..split]);
        out.push('.');
        out.push_str(&mag_str[split..]);
    }
    out
}

fn cast_numeric_to_int(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::Int(n) => Ok(Value::Int(n)),
        Value::BigInt(n) => i32::try_from(n)
            .map(Value::Int)
            .map_err(|_| EvalError::TypeMismatch {
                detail: format!("bigint {n} does not fit in int"),
            }),
        #[allow(clippy::cast_possible_truncation)]
        Value::Float(x) => Ok(Value::Int(x as i32)),
        Value::Text(s) => {
            s.trim()
                .parse::<i32>()
                .map(Value::Int)
                .map_err(|_| EvalError::TypeMismatch {
                    detail: format!("cannot parse {s:?} as int"),
                })
        }
        Value::Bool(b) => Ok(Value::Int(i32::from(b))),
        other => Err(EvalError::TypeMismatch {
            detail: format!("cannot cast {:?} to int", other.data_type()),
        }),
    }
}

fn cast_numeric_to_bigint(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::Int(n) => Ok(Value::BigInt(i64::from(n))),
        Value::BigInt(n) => Ok(Value::BigInt(n)),
        #[allow(clippy::cast_possible_truncation)]
        Value::Float(x) => Ok(Value::BigInt(x as i64)),
        Value::Text(s) => {
            s.trim()
                .parse::<i64>()
                .map(Value::BigInt)
                .map_err(|_| EvalError::TypeMismatch {
                    detail: format!("cannot parse {s:?} as bigint"),
                })
        }
        Value::Bool(b) => Ok(Value::BigInt(i64::from(b))),
        other => Err(EvalError::TypeMismatch {
            detail: format!("cannot cast {:?} to bigint", other.data_type()),
        }),
    }
}

fn cast_numeric_to_float(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::Int(n) => Ok(Value::Float(f64::from(n))),
        #[allow(clippy::cast_precision_loss)]
        Value::BigInt(n) => Ok(Value::Float(n as f64)),
        Value::Float(x) => Ok(Value::Float(x)),
        Value::Text(s) => {
            s.trim()
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| EvalError::TypeMismatch {
                    detail: format!("cannot parse {s:?} as float"),
                })
        }
        other => Err(EvalError::TypeMismatch {
            detail: format!("cannot cast {:?} to float", other.data_type()),
        }),
    }
}

fn cast_to_bool(v: Value) -> Result<Value, EvalError> {
    match v {
        Value::Bool(b) => Ok(Value::Bool(b)),
        Value::Int(n) => Ok(Value::Bool(n != 0)),
        Value::BigInt(n) => Ok(Value::Bool(n != 0)),
        Value::Text(s) => {
            let lo = s.trim().to_ascii_lowercase();
            match lo.as_str() {
                "true" | "t" | "yes" | "y" | "1" | "on" => Ok(Value::Bool(true)),
                "false" | "f" | "no" | "n" | "0" | "off" => Ok(Value::Bool(false)),
                _ => Err(EvalError::TypeMismatch {
                    detail: format!("cannot parse {s:?} as bool"),
                }),
            }
        }
        other => Err(EvalError::TypeMismatch {
            detail: format!("cannot cast {:?} to bool", other.data_type()),
        }),
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
        Literal::Interval { months, micros, .. } => Value::Interval {
            months: *months,
            micros: *micros,
        },
    }
}

fn resolve_column(c: &ColumnName, row: &Row, ctx: &EvalContext<'_>) -> Result<Value, EvalError> {
    if let Some(q) = &c.qualifier {
        // Multi-table evaluation (joins): the synthesised schema uses
        // composite column names "alias.column" so we look that up
        // directly. Falls back to the single-table case below if the
        // composite isn't present.
        let composite = alloc::format!("{q}.{name}", name = c.name);
        if let Some(pos) = ctx.columns.iter().position(|s| s.name == composite) {
            return Ok(row.values[pos].clone());
        }
        let expected = ctx.table_alias.ok_or_else(|| EvalError::UnknownQualifier {
            qualifier: q.clone(),
        })?;
        if q != expected {
            return Err(EvalError::UnknownQualifier {
                qualifier: q.clone(),
            });
        }
    }
    if let Some(pos) = ctx.columns.iter().position(|s| s.name == c.name) {
        return Ok(row.values[pos].clone());
    }
    // Bare-name fallback for joined schemas: match any single composite
    // column ending in ".<name>"; ambiguity is an error.
    let suffix = alloc::format!(".{name}", name = c.name);
    let mut matches = ctx
        .columns
        .iter()
        .enumerate()
        .filter(|(_, s)| s.name.ends_with(&suffix));
    let first = matches.next();
    let extra = matches.next();
    match (first, extra) {
        (Some((pos, _)), None) => Ok(row.values[pos].clone()),
        (Some(_), Some(_)) => Err(EvalError::TypeMismatch {
            detail: alloc::format!("ambiguous column reference: {}", c.name),
        }),
        _ => Err(EvalError::ColumnNotFound {
            name: c.name.clone(),
        }),
    }
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

/// v7.9.27b — true when two values are "not distinct" per PG:
/// both NULL counts as equal; otherwise reduces to regular Eq.
fn values_not_distinct(l: &Value, r: &Value) -> bool {
    match (l, r) {
        (Value::Null, Value::Null) => true,
        (Value::Null, _) | (_, Value::Null) => false,
        _ => l == r,
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
    // v7.9.27b — IS [NOT] DISTINCT FROM. NULL-safe equality:
    // `NULL IS NOT DISTINCT FROM NULL` → true. mailrs pg_dump.
    if let BinOp::IsNotDistinctFrom = op {
        return Ok(Value::Bool(values_not_distinct(&l, &r)));
    }
    if let BinOp::IsDistinctFrom = op {
        return Ok(Value::Bool(!values_not_distinct(&l, &r)));
    }
    // Everything else: any NULL operand → NULL.
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    // NUMERIC arithmetic and comparisons run in fixed-point; promote
    // integers to a common NUMERIC scale and stay in i128 throughout.
    if matches!(l, Value::Numeric { .. }) || matches!(r, Value::Numeric { .. }) {
        return apply_binary_numeric(op, l, r);
    }
    // Date / Timestamp arithmetic. PG semantics:
    //   * date + int      → date  (int is days)
    //   * int + date      → date
    //   * date - int      → date
    //   * date - date     → int   (days, signed)
    //   * timestamp - timestamp → bigint (microseconds, signed)
    // Other date/time math (`timestamp + int`, INTERVAL) lands later.
    if let Some(result) = apply_binary_calendar(op, &l, &r)? {
        return Ok(result);
    }
    match op {
        BinOp::Add => arith(l, r, i64::checked_add, |a, b| a + b, "+"),
        BinOp::Sub => arith(l, r, i64::checked_sub, |a, b| a - b, "-"),
        BinOp::Mul => arith(l, r, i64::checked_mul, |a, b| a * b, "*"),
        BinOp::Div => div_op(l, r),
        BinOp::L2Distance => l2_distance(l, r),
        BinOp::InnerProduct => inner_product(l, r),
        BinOp::CosineDistance => cosine_distance(l, r),
        BinOp::Concat => Ok(text_concat(&l, &r)),
        BinOp::JsonGet => crate::json::path_get(&l, &r, false),
        BinOp::JsonGetText => crate::json::path_get(&l, &r, true),
        BinOp::JsonGetPath => crate::json::path_walk(&l, &r, false),
        BinOp::JsonGetPathText => crate::json::path_walk(&l, &r, true),
        BinOp::JsonContains => crate::json::contains(&l, &r),
        // v7.12.2 — `@@` match. NULL on either side → NULL; PG
        // accepts both orderings so we normalise.
        BinOp::TsMatch => ts_match(l, r),
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => {
            compare(op, &l, &r)
        }
        BinOp::And | BinOp::Or | BinOp::IsDistinctFrom | BinOp::IsNotDistinctFrom => {
            unreachable!("handled above")
        }
    }
}

/// Calendar arithmetic. Returns `Some(value)` when the operand pair
/// is a date/time combo this function understands, `None` to let the
/// caller fall through to the regular numeric / text paths.
fn apply_binary_calendar(op: BinOp, l: &Value, r: &Value) -> Result<Option<Value>, EvalError> {
    let int_value = |v: &Value| -> Option<i64> {
        match v {
            Value::SmallInt(n) => Some(i64::from(*n)),
            Value::Int(n) => Some(i64::from(*n)),
            Value::BigInt(n) => Some(*n),
            _ => None,
        }
    };
    // Most-specific cases first — DATE-DATE / TS-TS subtraction before
    // DATE-integer subtraction, otherwise the latter swallows the
    // former with an `int_value(Date) = None` no-op fall-through.
    match (l, r) {
        (Value::Date(a), Value::Date(b)) if op == BinOp::Sub => {
            return Ok(Some(Value::BigInt(i64::from(*a) - i64::from(*b))));
        }
        (Value::Timestamp(a), Value::Timestamp(b)) if op == BinOp::Sub => {
            let delta = a.checked_sub(*b).ok_or(EvalError::TypeMismatch {
                detail: "TIMESTAMP - TIMESTAMP overflows i64 microseconds".into(),
            })?;
            return Ok(Some(Value::BigInt(delta)));
        }
        _ => {}
    }
    // INTERVAL arithmetic. PG: timestamp ± interval → timestamp,
    // date ± interval → date (if interval is pure days/months with no
    // sub-day component) else timestamp, interval ± interval → interval.
    if let Some(out) = apply_binary_interval(op, l, r)? {
        return Ok(Some(out));
    }
    match (l, r) {
        (Value::Date(d), other) if op == BinOp::Add => {
            if let Some(n) = int_value(other) {
                let days = i64::from(*d).saturating_add(n);
                let days32 = i32::try_from(days).map_err(|_| EvalError::TypeMismatch {
                    detail: "DATE + integer overflows DATE range".into(),
                })?;
                return Ok(Some(Value::Date(days32)));
            }
        }
        (other, Value::Date(d)) if op == BinOp::Add => {
            if let Some(n) = int_value(other) {
                let days = i64::from(*d).saturating_add(n);
                let days32 = i32::try_from(days).map_err(|_| EvalError::TypeMismatch {
                    detail: "integer + DATE overflows DATE range".into(),
                })?;
                return Ok(Some(Value::Date(days32)));
            }
        }
        (Value::Date(d), other) if op == BinOp::Sub => {
            if let Some(n) = int_value(other) {
                let days = i64::from(*d).saturating_sub(n);
                let days32 = i32::try_from(days).map_err(|_| EvalError::TypeMismatch {
                    detail: "DATE - integer overflows DATE range".into(),
                })?;
                return Ok(Some(Value::Date(days32)));
            }
        }
        _ => {}
    }
    Ok(None)
}

/// INTERVAL-aware binary ops. Recognises:
///   timestamp ± interval → timestamp
///   date ± interval      → date (if interval is integral days/months only)
///                       → timestamp (if interval has sub-day micros)
///   interval ± interval  → interval
/// Commutative for `+`. Returns `None` for unrecognised operand pairs so
/// the caller can fall through.
fn apply_binary_interval(op: BinOp, l: &Value, r: &Value) -> Result<Option<Value>, EvalError> {
    // Normalise so the interval (if any) is always on the right for Add;
    // Sub stays left-handed because it isn't commutative.
    let (lhs, rhs, sign): (&Value, &Value, i64) = match (l, r, op) {
        (Value::Interval { .. }, _, BinOp::Add) => (r, l, 1),
        (_, Value::Interval { .. }, BinOp::Add) => (l, r, 1),
        (_, Value::Interval { .. }, BinOp::Sub) => (l, r, -1),
        _ => return Ok(None),
    };
    let Value::Interval {
        months: rhs_months,
        micros: rhs_us,
    } = rhs
    else {
        unreachable!("rhs guaranteed to be Interval by the match above");
    };
    let signed_months = i64::from(*rhs_months) * sign;
    let signed_micros = rhs_us.checked_mul(sign).ok_or(EvalError::TypeMismatch {
        detail: "INTERVAL micros overflows on negation".into(),
    })?;
    match lhs {
        Value::Timestamp(t) => Ok(Some(Value::Timestamp(add_interval_to_micros(
            *t,
            signed_months,
            signed_micros,
        )?))),
        Value::Date(d) => {
            // Date + interval stays a date when the interval has zero
            // sub-day microseconds; otherwise promote to TIMESTAMP at
            // midnight of the (months-shifted) date first.
            let day_aligned = signed_micros.rem_euclid(86_400_000_000) == 0;
            if day_aligned {
                let micros_per_day = 86_400_000_000_i64;
                let days_delta = signed_micros / micros_per_day;
                let shifted = shift_date_by_months(*d, signed_months)?;
                let new_days =
                    i64::from(shifted)
                        .checked_add(days_delta)
                        .ok_or(EvalError::TypeMismatch {
                            detail: "DATE ± INTERVAL overflows DATE range".into(),
                        })?;
                let days32 = i32::try_from(new_days).map_err(|_| EvalError::TypeMismatch {
                    detail: "DATE ± INTERVAL overflows DATE range".into(),
                })?;
                Ok(Some(Value::Date(days32)))
            } else {
                let base =
                    i64::from(*d)
                        .checked_mul(86_400_000_000)
                        .ok_or(EvalError::TypeMismatch {
                            detail: "DATE → TIMESTAMP lift overflows for INTERVAL math".into(),
                        })?;
                Ok(Some(Value::Timestamp(add_interval_to_micros(
                    base,
                    signed_months,
                    signed_micros,
                )?)))
            }
        }
        Value::Interval {
            months: lhs_months,
            micros: lhs_us,
        } => {
            let new_months = i64::from(*lhs_months)
                .checked_add(signed_months)
                .and_then(|n| i32::try_from(n).ok())
                .ok_or(EvalError::TypeMismatch {
                    detail: "INTERVAL ± INTERVAL months overflows i32".into(),
                })?;
            let new_micros = lhs_us
                .checked_add(signed_micros)
                .ok_or(EvalError::TypeMismatch {
                    detail: "INTERVAL ± INTERVAL micros overflows i64".into(),
                })?;
            Ok(Some(Value::Interval {
                months: new_months,
                micros: new_micros,
            }))
        }
        _ => Err(EvalError::TypeMismatch {
            detail: format!(
                "operator {op:?} not defined for {:?} and INTERVAL",
                lhs.data_type()
            ),
        }),
    }
}

/// Shift a `Date` by a signed number of months using the PG clamp rule.
fn shift_date_by_months(d: i32, months: i64) -> Result<i32, EvalError> {
    let (y, m, day) = civil_from_days(d);
    let months_i32 = i32::try_from(months).map_err(|_| EvalError::TypeMismatch {
        detail: "INTERVAL months delta out of i32 range".into(),
    })?;
    let (ny, nm, nd) = add_months_to_civil(y, m, day, months_i32);
    Ok(days_from_civil(ny, nm, nd))
}

/// Add (months, micros) to a `Timestamp` (microseconds since epoch).
/// Months part is applied through civil calendar with clamp-to-last-day;
/// micros part is plain i64 addition with overflow guard.
fn add_interval_to_micros(t: i64, months: i64, micros: i64) -> Result<i64, EvalError> {
    let mut out = t;
    if months != 0 {
        const MICROS_PER_DAY: i64 = 86_400_000_000;
        let days = out.div_euclid(MICROS_PER_DAY);
        let day_micros = out.rem_euclid(MICROS_PER_DAY);
        let day_i32 = i32::try_from(days).map_err(|_| EvalError::TypeMismatch {
            detail: "TIMESTAMP day component out of i32 range for INTERVAL months math".into(),
        })?;
        let shifted_days = shift_date_by_months(day_i32, months)?;
        out = i64::from(shifted_days)
            .checked_mul(MICROS_PER_DAY)
            .and_then(|n| n.checked_add(day_micros))
            .ok_or(EvalError::TypeMismatch {
                detail: "TIMESTAMP ± INTERVAL months overflows i64 microseconds".into(),
            })?;
    }
    out.checked_add(micros).ok_or(EvalError::TypeMismatch {
        detail: "TIMESTAMP ± INTERVAL micros overflows i64".into(),
    })
}

/// Dispatch for any binary op when at least one operand is NUMERIC.
/// Other-side integers / floats are promoted to a NUMERIC at a common
/// scale; all add / sub / mul / div / compare paths stay in i128.
#[allow(clippy::needless_pass_by_value)] // mirrors `apply_binary`'s by-value calling convention
fn apply_binary_numeric(op: BinOp, l: Value, r: Value) -> Result<Value, EvalError> {
    // Float still wins — Numeric + Float coerces both to f64 and runs
    // through the float path. PG demotes Numeric to float in this mix
    // too (the documented behaviour for `numeric + double precision`).
    let float_path = matches!(l, Value::Float(_)) || matches!(r, Value::Float(_));
    if float_path {
        let af = as_f64(&l)?;
        let bf = as_f64(&r)?;
        return match op {
            BinOp::Add => Ok(Value::Float(af + bf)),
            BinOp::Sub => Ok(Value::Float(af - bf)),
            BinOp::Mul => Ok(Value::Float(af * bf)),
            BinOp::Div => {
                if bf == 0.0 {
                    Err(EvalError::DivisionByZero)
                } else {
                    Ok(Value::Float(af / bf))
                }
            }
            BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => {
                let ord = af.partial_cmp(&bf).ok_or(EvalError::TypeMismatch {
                    detail: "NaN in NUMERIC/Float comparison".into(),
                })?;
                Ok(Value::Bool(cmp_to_bool(op, ord)))
            }
            BinOp::Concat => Ok(text_concat(&l, &r)),
            other => Err(EvalError::TypeMismatch {
                detail: format!("operator {other:?} not defined for NUMERIC and Float"),
            }),
        };
    }
    // Promote integer ↔ numeric to a shared scale (max of both sides).
    let (a, sa) = numeric_or_widen(&l).ok_or_else(|| EvalError::TypeMismatch {
        detail: format!("NUMERIC op against non-numeric {:?}", l.data_type()),
    })?;
    let (b, sb) = numeric_or_widen(&r).ok_or_else(|| EvalError::TypeMismatch {
        detail: format!("NUMERIC op against non-numeric {:?}", r.data_type()),
    })?;
    match op {
        BinOp::Add | BinOp::Sub => {
            let target_scale = sa.max(sb);
            let lhs = rescale(a, sa, target_scale).ok_or(EvalError::TypeMismatch {
                detail: "NUMERIC overflow on rescale".into(),
            })?;
            let rhs = rescale(b, sb, target_scale).ok_or(EvalError::TypeMismatch {
                detail: "NUMERIC overflow on rescale".into(),
            })?;
            let r = match op {
                BinOp::Add => lhs.checked_add(rhs),
                BinOp::Sub => lhs.checked_sub(rhs),
                _ => unreachable!(),
            }
            .ok_or(EvalError::TypeMismatch {
                detail: "NUMERIC overflow on +/-".into(),
            })?;
            Ok(Value::Numeric {
                scaled: r,
                scale: target_scale,
            })
        }
        BinOp::Mul => {
            let scaled = a.checked_mul(b).ok_or(EvalError::TypeMismatch {
                detail: "NUMERIC overflow on *".into(),
            })?;
            Ok(Value::Numeric {
                scaled,
                scale: sa.saturating_add(sb),
            })
        }
        BinOp::Div => {
            if b == 0 {
                return Err(EvalError::DivisionByZero);
            }
            // Result scale: keep the wider operand's scale. Pre-scale
            // the numerator so the integer division retains that many
            // fractional digits. Round half-away-from-zero.
            let target_scale = sa.max(sb);
            // Numerator effective scale becomes sa + target_scale; we
            // bring it up to (target_scale + sb) so the divisor's scale
            // cancels cleanly.
            let bump = pow10_i128(target_scale.saturating_add(sb).saturating_sub(sa));
            let num = a.checked_mul(bump).ok_or(EvalError::TypeMismatch {
                detail: "NUMERIC overflow on / scaling".into(),
            })?;
            let half = if b >= 0 { b / 2 } else { -(b / 2) };
            let adj = if (num >= 0) == (b >= 0) {
                num + half
            } else {
                num - half
            };
            Ok(Value::Numeric {
                scaled: adj / b,
                scale: target_scale,
            })
        }
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => {
            let target_scale = sa.max(sb);
            let lhs = rescale(a, sa, target_scale).ok_or(EvalError::TypeMismatch {
                detail: "NUMERIC overflow on rescale".into(),
            })?;
            let rhs = rescale(b, sb, target_scale).ok_or(EvalError::TypeMismatch {
                detail: "NUMERIC overflow on rescale".into(),
            })?;
            Ok(Value::Bool(cmp_to_bool(op, lhs.cmp(&rhs))))
        }
        BinOp::Concat => Ok(text_concat(&l, &r)),
        other => Err(EvalError::TypeMismatch {
            detail: format!("operator {other:?} not defined for NUMERIC"),
        }),
    }
}

/// Express `v` as a `(scaled_i128, scale)` pair. Plain integers come
/// back with `scale=0`; NUMERIC keeps its own scale. Anything else
/// returns `None` and the caller raises a type error.
fn numeric_or_widen(v: &Value) -> Option<(i128, u8)> {
    match v {
        Value::Numeric { scaled, scale } => Some((*scaled, *scale)),
        Value::Int(n) => Some((i128::from(*n), 0)),
        Value::SmallInt(n) => Some((i128::from(*n), 0)),
        Value::BigInt(n) => Some((i128::from(*n), 0)),
        _ => None,
    }
}

fn rescale(scaled: i128, src: u8, dst: u8) -> Option<i128> {
    if src == dst {
        return Some(scaled);
    }
    if dst > src {
        scaled.checked_mul(pow10_i128(dst - src))
    } else {
        let drop = pow10_i128(src - dst);
        let half = drop / 2;
        let r = if scaled >= 0 {
            scaled + half
        } else {
            scaled - half
        };
        Some(r / drop)
    }
}

const fn pow10_i128(p: u8) -> i128 {
    let mut acc: i128 = 1;
    let mut i = 0;
    while i < p {
        acc *= 10;
        i += 1;
    }
    acc
}

const fn cmp_to_bool(op: BinOp, ord: core::cmp::Ordering) -> bool {
    use core::cmp::Ordering::{Equal, Greater, Less};
    match op {
        BinOp::Eq => matches!(ord, Equal),
        BinOp::NotEq => !matches!(ord, Equal),
        BinOp::Lt => matches!(ord, Less),
        BinOp::LtEq => matches!(ord, Less | Equal),
        BinOp::Gt => matches!(ord, Greater),
        BinOp::GtEq => matches!(ord, Greater | Equal),
        _ => false,
    }
}

/// SQL `||` string concatenation. Operands are coerced to text via the same
/// rule as `::text` cast. NULL propagates (handled above; this function only
/// runs with non-NULL operands).
fn text_concat(l: &Value, r: &Value) -> Value {
    // v7.11.8 — PG `||` overloads: TEXT[] || TEXT[] = concatenated array;
    // TEXT[] || TEXT (or TEXT || TEXT[]) prepends/appends the single
    // element. NULL || anything = NULL (PG semantics for arrays;
    // text concat treats NULL the same way after value_to_text).
    match (l, r) {
        (Value::Null, _) | (_, Value::Null) => {
            // PG text concat: NULL || x = NULL. Array concat: NULL || x = NULL.
            // Keep the legacy text path (value_to_text handles Null as ""),
            // but for arrays we surface real NULL to match PG.
            if matches!(
                l,
                Value::TextArray(_) | Value::IntArray(_) | Value::BigIntArray(_) | Value::Bytes(_)
            ) || matches!(
                r,
                Value::TextArray(_) | Value::IntArray(_) | Value::BigIntArray(_) | Value::Bytes(_)
            ) {
                return Value::Null;
            }
        }
        (Value::TextArray(a), Value::TextArray(b)) => {
            let mut out = a.clone();
            out.extend(b.iter().cloned());
            return Value::TextArray(out);
        }
        (Value::TextArray(a), Value::Text(s)) => {
            let mut out = a.clone();
            out.push(Some(s.clone()));
            return Value::TextArray(out);
        }
        (Value::Text(s), Value::TextArray(b)) => {
            let mut out: alloc::vec::Vec<Option<alloc::string::String>> =
                alloc::vec::Vec::with_capacity(1 + b.len());
            out.push(Some(s.clone()));
            out.extend(b.iter().cloned());
            return Value::TextArray(out);
        }
        // v7.11.13 — IntArray / BigIntArray `||` overloads. Same
        // PG semantics as TEXT[]: array||array concatenates, and
        // array||scalar appends/prepends. Mixed Int/BigInt widens
        // to BigIntArray.
        (Value::IntArray(a), Value::IntArray(b)) => {
            let mut out = a.clone();
            out.extend(b.iter().copied());
            return Value::IntArray(out);
        }
        (Value::IntArray(a), Value::Int(n)) => {
            let mut out = a.clone();
            out.push(Some(*n));
            return Value::IntArray(out);
        }
        (Value::IntArray(a), Value::SmallInt(n)) => {
            let mut out = a.clone();
            out.push(Some(i32::from(*n)));
            return Value::IntArray(out);
        }
        (Value::Int(n), Value::IntArray(b)) => {
            let mut out: alloc::vec::Vec<Option<i32>> = alloc::vec::Vec::with_capacity(1 + b.len());
            out.push(Some(*n));
            out.extend(b.iter().copied());
            return Value::IntArray(out);
        }
        (Value::SmallInt(n), Value::IntArray(b)) => {
            let mut out: alloc::vec::Vec<Option<i32>> = alloc::vec::Vec::with_capacity(1 + b.len());
            out.push(Some(i32::from(*n)));
            out.extend(b.iter().copied());
            return Value::IntArray(out);
        }
        (Value::BigIntArray(a), Value::BigIntArray(b)) => {
            let mut out = a.clone();
            out.extend(b.iter().copied());
            return Value::BigIntArray(out);
        }
        (Value::BigIntArray(a), Value::IntArray(b)) => {
            let mut out = a.clone();
            out.extend(b.iter().map(|o| o.map(i64::from)));
            return Value::BigIntArray(out);
        }
        (Value::IntArray(a), Value::BigIntArray(b)) => {
            let mut out: alloc::vec::Vec<Option<i64>> =
                a.iter().map(|o| o.map(i64::from)).collect();
            out.extend(b.iter().copied());
            return Value::BigIntArray(out);
        }
        (Value::BigIntArray(a), Value::BigInt(n)) => {
            let mut out = a.clone();
            out.push(Some(*n));
            return Value::BigIntArray(out);
        }
        (Value::BigIntArray(a), Value::Int(n)) => {
            let mut out = a.clone();
            out.push(Some(i64::from(*n)));
            return Value::BigIntArray(out);
        }
        (Value::BigIntArray(a), Value::SmallInt(n)) => {
            let mut out = a.clone();
            out.push(Some(i64::from(*n)));
            return Value::BigIntArray(out);
        }
        (Value::BigInt(n), Value::BigIntArray(b)) => {
            let mut out: alloc::vec::Vec<Option<i64>> = alloc::vec::Vec::with_capacity(1 + b.len());
            out.push(Some(*n));
            out.extend(b.iter().copied());
            return Value::BigIntArray(out);
        }
        (Value::Int(n), Value::BigIntArray(b)) => {
            let mut out: alloc::vec::Vec<Option<i64>> = alloc::vec::Vec::with_capacity(1 + b.len());
            out.push(Some(i64::from(*n)));
            out.extend(b.iter().copied());
            return Value::BigIntArray(out);
        }
        (Value::SmallInt(n), Value::BigIntArray(b)) => {
            let mut out: alloc::vec::Vec<Option<i64>> = alloc::vec::Vec::with_capacity(1 + b.len());
            out.push(Some(i64::from(*n)));
            out.extend(b.iter().copied());
            return Value::BigIntArray(out);
        }
        // v7.11.15 — BYTEA `||` is byte concatenation.
        (Value::Bytes(a), Value::Bytes(b)) => {
            let mut out = a.clone();
            out.extend_from_slice(b);
            return Value::Bytes(out);
        }
        _ => {}
    }
    let a = value_to_text(l);
    let b = value_to_text(r);
    Value::Text(a + &b)
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
    // v6.0.1: SQ8 cells coming through the SQL evaluator are
    // dequantised to f32 here so the existing scalar distance
    // arithmetic stays intact. HNSW kNN search continues to use
    // the asymmetric ADC variant inside `cell_to_query_metric_
    // distance` — this path only runs when a vector expression
    // lands in the evaluator (full-scan ORDER BY, SELECT
    // projection of `v <-> $1`, etc.).
    let to_f32 = |v: Value| -> Option<Vec<f32>> {
        match v {
            Value::Vector(a) => Some(a),
            Value::Sq8Vector(q) => Some(spg_storage::quantize::dequantize(&q)),
            // v6.0.3: bit-exact dequant for halfvec cells.
            Value::HalfVector(h) => Some(h.to_f32_vec()),
            _ => None,
        }
    };
    let l_ty = l.data_type();
    let r_ty = r.data_type();
    match (to_f32(l), to_f32(r)) {
        (Some(a), Some(b)) => {
            if a.len() != b.len() {
                return Err(EvalError::TypeMismatch {
                    detail: format!("vector dim mismatch in {op}: {} vs {}", a.len(), b.len()),
                });
            }
            Ok((a, b))
        }
        _ => Err(EvalError::TypeMismatch {
            detail: format!("{op} requires two vectors, got {l_ty:?} and {r_ty:?}"),
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
    // Widen SmallInt to Int up front so the rest of the arithmetic
    // table only deals with Int / BigInt / Float pairs.
    let widen = |v: Value| -> Value {
        match v {
            Value::SmallInt(n) => Value::Int(i32::from(n)),
            other => other,
        }
    };
    let l = widen(l);
    let r = widen(r);
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
    // v6.0.1: route both operands through `unwrap_vec_pair` so SQ8
    // cells dequantise on the way in. Sub-f64 precision loss is
    // negligible vs the dequantisation noise the SQ8 path already
    // ships with.
    let (a, b) = unwrap_vec_pair(l, r, "<->")?;
    let mut sum: f64 = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        let d = f64::from(*x) - f64::from(*y);
        sum += d * d;
    }
    Ok(Value::Float(sqrt_newton(sum)))
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
        Value::SmallInt(n) => Ok(f64::from(*n)),
        Value::Int(n) => Ok(f64::from(*n)),
        #[allow(clippy::cast_precision_loss)]
        Value::BigInt(n) => Ok(*n as f64),
        Value::Float(x) => Ok(*x),
        #[allow(clippy::cast_precision_loss)]
        Value::Numeric { scaled, scale } => {
            let mut div = 1.0_f64;
            for _ in 0..*scale {
                div *= 10.0;
            }
            Ok((*scaled as f64) / div)
        }
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
        // Date / Timestamp compare on their integer storage repr.
        // Cross-domain (Date vs Timestamp) lifts the Date to the
        // matching midnight TIMESTAMP first.
        (Value::Date(a), Value::Date(b)) => a.cmp(b),
        (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
        (Value::Date(a), Value::Timestamp(b)) => (i64::from(*a) * 86_400_000_000).cmp(b),
        (Value::Timestamp(a), Value::Date(b)) => a.cmp(&(i64::from(*b) * 86_400_000_000)),
        // PG-style implicit coercion: comparing a DATE / TIMESTAMP
        // column against a text literal lifts the literal into the
        // matching domain (e.g. `day >= '2024-01-01'`).
        (Value::Date(a), Value::Text(b)) => {
            let bd = parse_date_literal(b).ok_or_else(|| EvalError::TypeMismatch {
                detail: format!("cannot parse {b:?} as DATE for comparison"),
            })?;
            a.cmp(&bd)
        }
        (Value::Text(a), Value::Date(b)) => {
            let ad = parse_date_literal(a).ok_or_else(|| EvalError::TypeMismatch {
                detail: format!("cannot parse {a:?} as DATE for comparison"),
            })?;
            ad.cmp(b)
        }
        (Value::Timestamp(a), Value::Text(b)) => {
            let bt = parse_timestamp_literal(b).ok_or_else(|| EvalError::TypeMismatch {
                detail: format!("cannot parse {b:?} as TIMESTAMP for comparison"),
            })?;
            a.cmp(&bt)
        }
        (Value::Text(a), Value::Timestamp(b)) => {
            let at = parse_timestamp_literal(a).ok_or_else(|| EvalError::TypeMismatch {
                detail: format!("cannot parse {a:?} as TIMESTAMP for comparison"),
            })?;
            at.cmp(b)
        }
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
        | BinOp::CosineDistance
        | BinOp::Concat
        | BinOp::JsonGet
        | BinOp::JsonGetText
        | BinOp::JsonGetPath
        | BinOp::JsonGetPathText
        | BinOp::JsonContains
        | BinOp::TsMatch
        | BinOp::IsDistinctFrom
        | BinOp::IsNotDistinctFrom => {
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

    #[test]
    fn interval_format_basics() {
        assert_eq!(format_interval(0, 0), "0");
        assert_eq!(format_interval(0, 86_400_000_000), "1 day");
        assert_eq!(format_interval(0, -86_400_000_000), "-1 days");
        assert_eq!(format_interval(0, 3_600_000_000), "01:00:00");
        assert_eq!(
            format_interval(0, 86_400_000_000 + 9_000_000),
            "1 day 00:00:09"
        );
        assert_eq!(format_interval(14, 0), "1 year 2 mons");
        assert_eq!(format_interval(-1, 0), "-1 mons");
    }

    #[test]
    fn interval_add_to_timestamp_micros_part() {
        // 2024-01-01 00:00:00 + INTERVAL '1 hour' = 2024-01-01 01:00:00
        let ts = i64::from(days_from_civil(2024, 1, 1)) * 86_400_000_000;
        let r = add_interval_to_micros(ts, 0, 3_600_000_000).unwrap();
        let expected = ts + 3_600_000_000;
        assert_eq!(r, expected);
    }

    #[test]
    fn interval_clamp_month_end() {
        // 2024-01-31 + 1 month = 2024-02-29 (leap year).
        let d = days_from_civil(2024, 1, 31);
        let shifted = shift_date_by_months(d, 1).unwrap();
        let (y, m, day) = civil_from_days(shifted);
        assert_eq!((y, m, day), (2024, 2, 29));
        // 2023-01-31 + 1 month = 2023-02-28 (non-leap).
        let d = days_from_civil(2023, 1, 31);
        let shifted = shift_date_by_months(d, 1).unwrap();
        let (y, m, day) = civil_from_days(shifted);
        assert_eq!((y, m, day), (2023, 2, 28));
        // 2024-03-31 - 1 month = 2024-02-29.
        let d = days_from_civil(2024, 3, 31);
        let shifted = shift_date_by_months(d, -1).unwrap();
        let (y, m, day) = civil_from_days(shifted);
        assert_eq!((y, m, day), (2024, 2, 29));
    }

    #[test]
    fn interval_date_plus_pure_days_stays_date() {
        // DATE + INTERVAL '7 days' must stay DATE.
        let d = days_from_civil(2024, 6, 1);
        let lhs = Value::Date(d);
        let rhs = Value::Interval {
            months: 0,
            micros: 7 * 86_400_000_000,
        };
        let v = apply_binary_interval(BinOp::Add, &lhs, &rhs)
            .unwrap()
            .unwrap();
        let expected = days_from_civil(2024, 6, 8);
        assert_eq!(v, Value::Date(expected));
    }

    #[test]
    fn interval_date_plus_sub_day_lifts_to_timestamp() {
        // DATE + INTERVAL '1 hour' must lift to TIMESTAMP.
        let d = days_from_civil(2024, 6, 1);
        let lhs = Value::Date(d);
        let rhs = Value::Interval {
            months: 0,
            micros: 3_600_000_000,
        };
        let v = apply_binary_interval(BinOp::Add, &lhs, &rhs)
            .unwrap()
            .unwrap();
        let expected = i64::from(d) * 86_400_000_000 + 3_600_000_000;
        assert_eq!(v, Value::Timestamp(expected));
    }
}
