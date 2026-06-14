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

use alloc::borrow::Cow;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_sql::ast::{BinOp, ColumnName, Expr, Literal, UnOp};
use spg_storage::{ColumnSchema, DataType, Row, Value};

mod cast;
mod compiled;
mod encoding;
mod inet;
mod math;
mod regexp;
mod strings;
mod textsearch;

pub use cast::{cast_to_vector, cast_value, parse_vector_text};
pub(crate) use compiled::{
    CompiledExpr, compile_expr, eval_compiled, eval_compiled_ref, fully_compilable,
};
use encoding::{decode_text, encode_text};
use inet::{inet_host, inet_masklen, inet_network, inet_op_bool_result};
pub(crate) use math::{f64_ceil, f64_floor, f64_sqrt};
use math::{
    f64_exp, f64_ln, f64_powi, f64_round_half_away, f64_trunc, prng_next_f64, prng_next_u64,
};
use regexp::{regexp_matches, regexp_replace, regexp_split_to_array};
use strings::{
    TrimSide, format_string, pg_typeof_name, string_left_right, string_pad, string_trim, to_char,
    value_to_format_text,
};
pub use textsearch::{
    decode_tsquery_external, decode_tsvector_external, format_tsquery, format_tsvector,
};
use textsearch::{
    fts_phraseto_tsquery, fts_plainto_tsquery, fts_setweight, fts_to_tsquery, fts_to_tsvector,
    fts_ts_rank, fts_ts_rank_cd, fts_websearch_to_tsquery, ts_match, tsvector_concat,
};

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
    pub const fn with_sequence_resolver(mut self, resolver: &'a SequenceResolver<'a>) -> Self {
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
        Expr::AggregateOrdered { .. } => Err(EvalError::TypeMismatch {
            detail: "aggregate ORDER BY is only valid inside an aggregating SELECT".into(),
        }),
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
            // v7.32 (P4 borrow channel) — comparison fast path. A pure
            // comparison op only reads its operands and returns Bool,
            // and for non-NUMERIC / non-INTERVAL / non-CI-collation
            // operands `apply_binary` IS just the NULL-3VL check plus
            // the ref-based `compare` (NUMERIC routes through fixed-
            // point `apply_binary_numeric`; INTERVAL through
            // `apply_binary_interval`; CI columns fold). So read the
            // operands borrowed — a column cell is no longer cloned
            // just to compare it (`WHERE thread_id != ''` alone cloned
            // one Text cell per scanned row). Anything that needs the
            // owned path falls through unchanged.
            if matches!(
                op,
                BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq
            ) {
                let lc = eval_expr_cow(lhs, row, ctx)?;
                let rc = eval_expr_cow(rhs, row, ctx)?;
                let owned_path = is_owned_compare_value(lc.as_ref())
                    || is_owned_compare_value(rc.as_ref())
                    || compare_is_case_insensitive(lhs, rhs, ctx);
                if !owned_path {
                    if lc.as_ref().is_null() || rc.as_ref().is_null() {
                        return Ok(Value::Null);
                    }
                    return compare(*op, lc.as_ref(), rc.as_ref());
                }
                let (l, r) = collation_fold_for_compare(
                    *op,
                    lhs,
                    rhs,
                    lc.into_owned(),
                    rc.into_owned(),
                    ctx,
                );
                return apply_binary(*op, l, r);
            }
            let l = eval_expr(lhs, row, ctx)?;
            let r = eval_expr(rhs, row, ctx)?;
            // v7.17.0 Phase 2.5 — collation-aware text comparison.
            // When either operand of a comparison op references a
            // column declared `COLLATE "case_insensitive"` (or any
            // MySQL `_ci` collation), case-fold both sides before
            // the byte-wise compare so `WHERE name = 'foo'` matches
            // stored `'Foo'`. Non-Text values fall straight through
            // — the helper is a no-op outside Text-Text equality
            // and inequality.
            let (l, r) = collation_fold_for_compare(*op, lhs, rhs, l, r, ctx);
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
            // v7.29 (round-22 phase 3) - prefix fast path: LEFT(col, n)
            // on a TEXT column borrows the cell and clones only the
            // prefix. The generic path clones the WHOLE cell first -
            // a LEFT(body, 120) over 24k x 30 KB rows spent 383 ms
            // copying bytes it then threw away (7 ms without LEFT).
            if args.len() == 2
                && name.eq_ignore_ascii_case("left")
                && let Expr::Column(c) = &args[0]
                && let Some(cell) = resolve_column_borrowed(c, row, ctx)?
            {
                {
                    match cell {
                        Value::Null => return Ok(Value::Null),
                        Value::Text(t) => {
                            let n_v = eval_expr(&args[1], row, ctx)?;
                            if let Value::SmallInt(_) | Value::Int(_) | Value::BigInt(_) = n_v {
                                let n = match n_v {
                                    Value::SmallInt(x) => i64::from(x),
                                    Value::Int(x) => i64::from(x),
                                    Value::BigInt(x) => x,
                                    _ => 0,
                                };
                                return Ok(Value::Text(text_prefix_chars(t, n)));
                            }
                        }
                        _ => {}
                    }
                }
            }
            let evaluated: Result<Vec<Value>, _> =
                args.iter().map(|a| eval_expr(a, row, ctx)).collect();
            apply_function(name, &evaluated?, ctx)
        }
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
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
            // v7.25 (round-17) — ILIKE folds both operands (PG
            // lowercases per the default collation).
            let m = if *case_insensitive {
                like_match(&text.to_lowercase(), &pat.to_lowercase())
            } else {
                like_match(&text, &pat)
            };
            Ok(Value::Bool(if *negated { !m } else { m }))
        }
        Expr::Extract { field, source } => {
            let v = eval_expr(source, row, ctx)?;
            extract_field(*field, &v)
        }
        // v4.10: subquery nodes should have been resolved into
        // Literal / InList nodes by Engine::resolve_select_subqueries
        // before the row loop. Anything reaching here is a bug.
        Expr::ScalarSubquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => {
            Err(EvalError::TypeMismatch {
                detail: "subquery reached row eval — engine resolver bug".into(),
            })
        }
        // v7.30.2 (mailrs round-25) — flat `expr [NOT] IN (a, b, …)`.
        // Iterative scan with PG three-valued logic: TRUE on the first
        // Eq match; if nothing matched, NULL when the needle is NULL or
        // any comparison was NULL; FALSE otherwise. Empty list (only
        // reachable via an empty subquery result) is FALSE / TRUE even
        // for a NULL needle — no comparison ever happens.
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let needle = eval_expr(expr, row, ctx)?;
            let needle_null = matches!(needle, Value::Null);
            let mut saw_null = needle_null && !list.is_empty();
            let mut matched = false;
            if !needle_null {
                for item in list {
                    let v = eval_expr(item, row, ctx)?;
                    if matches!(v, Value::Null) {
                        saw_null = true;
                        continue;
                    }
                    match apply_binary(BinOp::Eq, needle.clone(), v)? {
                        Value::Bool(true) => {
                            matched = true;
                            break;
                        }
                        Value::Bool(false) => {}
                        Value::Null => saw_null = true,
                        other => {
                            return Err(EvalError::TypeMismatch {
                                detail: format!(
                                    "IN comparison didn't return Bool: {:?}",
                                    other.data_type()
                                ),
                            });
                        }
                    }
                }
            }
            let inner = if matched {
                Value::Bool(true)
            } else if saw_null {
                Value::Null
            } else {
                Value::Bool(false)
            };
            Ok(match (negated, inner) {
                (true, Value::Bool(b)) => Value::Bool(!b),
                (_, v) => v,
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
            // total seconds in the interval (months count as 30 days,
            // PG's justify_interval convention).
            F::Epoch => i64::from(months) * 30 * 86_400 + secs_total,
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
        // seconds since the unix epoch (truncated; PG returns
        // numeric with fraction — mailrs casts ::BIGINT anyway).
        F::Epoch => i64::from(days) * 86_400 + secs,
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
            let resolver = ctx
                .sequence_resolver
                .ok_or_else(|| EvalError::TypeMismatch {
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
            let resolver = ctx
                .sequence_resolver
                .ok_or_else(|| EvalError::TypeMismatch {
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
            let resolver = ctx
                .sequence_resolver
                .ok_or_else(|| EvalError::TypeMismatch {
                    detail: "setval() requires a sequence resolver (read-only context)".into(),
                })?;
            let v = resolver(SequenceOp::Set {
                name: seq_name,
                value,
                is_called,
            })?;
            Ok(Value::BigInt(v))
        }
        // v7.22 (round-13) — char_length / character_length are the
        // SQL-standard spellings PG accepts everywhere; pg_dump
        // CHECK predicates carry them verbatim.
        "length" | "char_length" | "character_length" => {
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
        "substring" | "substr" => {
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
        // v7.17.0 Phase 3.P0-29 — MySQL time aliases. WordPress,
        // Laravel, mysql-connector-python emit these constantly.
        // `unix_timestamp()` (bare) is folded by clock_replacement_for
        // into a BigInt literal — this arm only handles the 1-arg
        // form (TIMESTAMP / DATE → epoch seconds).
        "date_format" => date_format_mysql(args),
        "unix_timestamp" => unix_timestamp_of(args),
        "from_unixtime" => from_unixtime(args),
        // v7.17.0 Phase 3.8 — PG `format(fmt, args…)` sprintf-style.
        // Conversion specifiers: `%s` (literal string from arg),
        // `%I` (quoted identifier), `%L` (quoted SQL literal),
        // `%%` (literal `%`). `%n$X` argument-position prefix
        // (1-based). NULL arg → empty string for %s; NULL for %I
        // is an error in PG; NULL for %L renders as the SQL
        // literal `NULL`. Args missing for a position → error.
        "format" => format_string(args),
        // PG `concat(args...)` — variadic; coerces every arg to
        // its text representation; NULL arguments are silently
        // skipped (the canonical PG semantic — `concat()` is the
        // NULL-tolerant counterpart to the `||` operator which
        // propagates NULL).
        //
        // Reference:
        //   https://www.postgresql.org/docs/current/functions-string.html
        //   "Concatenates the text representations of all the
        //   arguments. NULL arguments are ignored."
        //
        // Edge cases:
        //   * `concat()` (no args) → ''
        //   * Every arg NULL → '' (NEVER returns NULL — distinct
        //     from `||` and from `array_agg`)
        //   * Bool → PG single-char form 't' / 'f'
        //   * SmallInt / Int / BigInt / Float / Numeric / Date /
        //     Timestamp / Json / Bytes → their canonical text
        //     rendering (shared with `format()`'s %s specifier
        //     via `value_to_format_text`).
        "concat" => {
            let mut out = String::new();
            for v in args {
                if matches!(v, Value::Null) {
                    continue;
                }
                out.push_str(&value_to_format_text(v));
            }
            Ok(Value::Text(out))
        }
        // PG `concat_ws(sep, val1 [, val2 ...])` — like concat but
        // with a separator inserted between each pair of NON-NULL
        // arguments. Critical semantic subtleties:
        //   * NULL separator → NULL result (the sep position is
        //     mandatory and poison-prone; this is the ONLY way
        //     concat_ws can return NULL).
        //   * NULL data args silently SKIPPED — the separator is
        //     NOT inserted around them. `concat_ws(',', 'a', NULL,
        //     'b')` → `'a,b'`, not `'a,,b'`.
        //   * Empty-string data args are KEPT (separator placed
        //     around them). `concat_ws(',', 'a', '', 'b')` →
        //     `'a,,b'`. Distinction with NULL matters for code
        //     like `concat_ws(', ', first_name, middle_name,
        //     last_name)`.
        //   * 0 args → arity error (sep is mandatory).
        //   * Only sep (no data) → '' (NOT NULL — distinct from
        //     the all-NULL data case which also returns '').
        //
        // Reference:
        //   https://www.postgresql.org/docs/current/functions-string.html
        // PG `trim` / `ltrim` / `rtrim` / `btrim`.
        //
        // Semantic anchors (PG-canonical):
        //   * Default chars set is the ASCII SPACE only (NOT the
        //     POSIX whitespace class — tab / newline / form-feed
        //     stay put unless explicitly listed in `chars`).
        //   * `chars` arg is a UTF-8 codepoint SET — any char in
        //     the set is stripped, not the substring.
        //   * `trim(s)` == `btrim(s)` == strip both ends.
        //   * `ltrim(s, c)` / `rtrim(s, c)` strip only the named
        //     side; inner occurrences are preserved.
        //   * NULL on EITHER arg → NULL result.
        //   * Non-text input is coerced via `value_to_format_text`
        //     so trim(42) returns '42'.
        //
        // Reference:
        //   https://www.postgresql.org/docs/current/functions-string.html
        // PG `replace(string, from, to)` — substring substitution
        // for every (non-overlapping, greedy left-to-right)
        // occurrence. Empty `from` passes input through unchanged
        // (PG behavior — avoids infinite loop). Inserted text is
        // NOT re-scanned for new matches (so `replace('a', 'a',
        // 'aa')` terminates at `'aa'`, not blows up). NULL on any
        // arg poisons.
        // PG `split_part(string, delimiter, n)` — split on delim,
        // return the n-th field (1-indexed). Negative n counts
        // from the end (PG 14+). Out-of-range n → '' (NOT NULL).
        // n = 0 → error. Empty delimiter → error. NULL on any
        // arg → NULL.
        // PG `repeat(string, n)` — duplicate the input N times.
        // n=0 → ''; n<0 → '' (PG does NOT error on negative);
        // NULL on any arg → NULL.
        // PG `lpad(string, length [, fill])` / `rpad(...)`.
        // length is the target CODEPOINT count. Truncation when
        // input longer (lpad keeps the LEFT side, rpad keeps
        // LEFT too — both wait truncate from the right side per
        // PG-verified behavior). Padding when shorter, using
        // `fill` (default SPACE) cycling for multi-char fills.
        // length<=0 → ''. Empty fill + needs padding → returns
        // input verbatim (potentially truncated). NULL on any
        // arg → NULL.
        // PG `strpos(string, substring)` — same as position()
        // but with reversed arg order. PG convention is
        // strpos(haystack, needle); position(needle, haystack).
        // Both are 1-indexed; 0 = not found; codepoint-counted.
        // PG `left(string, n)` / `right(string, n)` — head/tail
        // substring helpers. Negative n means "all but last/first
        // |n| chars" — slice from the OPPOSITE side. n=0 → ''.
        // Codepoint-counted. NULL on any arg → NULL.
        // PG `floor(x)` — largest integer <= x.
        //   * Negative floats floor TOWARD -infinity, NOT toward 0.
        //   * Integer types passthrough unchanged.
        //   * NULL → NULL.
        // PG `ceil(x)` / `ceiling(x)` — smallest integer >= x.
        //   * Negative floats round TOWARD zero (toward +inf):
        //     ceil(-1.5) → -1, NOT -2.
        //   * Integer types passthrough unchanged.
        //   * NULL → NULL.
        // PG `round(x)` / `round(x, scale)` — half-away-from-zero
        // rounding (NUMERIC semantic).
        //   * round(0.5) → 1; round(-0.5) → -1; round(2.5) → 3.
        //   * Two-arg form rounds to N decimal places (n>0) or to
        //     nearest 10^|n| (n<0).
        //   * Integer types passthrough unchanged.
        //   * NULL on any arg → NULL.
        // PG `trunc(x)` / `trunc(x, scale)` — truncate TOWARD zero.
        //   * Distinct from floor() which rounds toward -inf:
        //     trunc(-1.7)→-1; floor(-1.7)→-2.
        //   * Distinct from round() which rounds half-away:
        //     trunc(1.5)→1; round(1.5)→2.
        //   * Two-arg form truncates to N decimal places (or 10^|n|
        //     for negative n).
        //   * Integer types passthrough unchanged.
        //   * NULL on any arg → NULL.
        // PG `nullif(a, b)` — returns NULL if a = b, else a.
        // Canonical use cases:
        //   * Divide-by-zero protection: `x / nullif(y, 0)`
        //   * Empty-string normalisation: `nullif(field, '')`
        // Edge: nullif(NULL, NULL) returns NULL. nullif(NULL, x)
        // returns NULL. nullif(x, NULL) returns x (since NULL is
        // not == to anything per IS DISTINCT FROM semantic, x ≠ NULL).
        // PG `greatest(...)` / `least(...)` — variadic max/min.
        // NULL args silently skipped (PG-canonical). All-NULL → NULL.
        // Cross-type widening for numeric comparisons.
        // PG `mod(y, x)` — modulo. Result sign follows dividend.
        //   * mod(7, 3) = 1
        //   * mod(-7, 3) = -1
        //   * mod(7, -3) = 1
        //   * mod(-7, -3) = -1
        // Division by zero → error. NULL on any arg → NULL.
        // PG `power(x, y)` / `pow(x, y)` — x^y.
        // Integer exponent → exact via repeated multiplication
        // (no precision loss). Fractional exponent → exp(y*ln(x))
        // via the no_std exp/ln series helpers.
        // x=0 with negative y → error (1/0). NULL → NULL.
        // PG `sqrt(x)` — square root. Negative input → error.
        // PG `sign(x)` — -1 / 0 / 1.
        // PG `random()` — uniform float in [0, 1). Per-row /
        // per-call: each evaluation returns a different value
        // even within the same statement. Backed by a xorshift64*
        // PRNG with a process-static seed; not cryptographically
        // secure (use a cryptographic source for security tokens).
        "random" => {
            if !args.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("random() takes 0 args, got {}", args.len()),
                });
            }
            Ok(Value::Float(prng_next_f64()))
        }
        // v7.17.0 — PG `gen_random_uuid()` (built-in, no extension)
        // and the historical uuid-ossp `uuid_generate_v4()` alias.
        // Both produce a RFC 4122 v4 (random) UUID. This is the
        // function Django / Rails / Hibernate emit in `id UUID
        // PRIMARY KEY DEFAULT gen_random_uuid()`, the modern
        // default PK pattern.
        "gen_random_uuid" | "uuid_generate_v4" => {
            if !args.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("{name}() takes 0 args, got {}", args.len()),
                });
            }
            Ok(Value::Uuid(gen_random_uuid_bytes()))
        }
        "sign" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("sign() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::SmallInt(n) => Ok(Value::SmallInt(n.signum())),
                Value::Int(n) => Ok(Value::Int(n.signum())),
                Value::BigInt(n) => Ok(Value::BigInt(n.signum())),
                Value::Float(x) => {
                    let s = if *x > 0.0 {
                        1.0
                    } else if *x < 0.0 {
                        -1.0
                    } else {
                        0.0
                    };
                    Ok(Value::Float(s))
                }
                Value::Numeric { scaled, scale } => {
                    let s = scaled.signum();
                    Ok(Value::Numeric {
                        scaled: s * pow10_i128(*scale),
                        scale: *scale,
                    })
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!("sign() needs numeric, got {:?}", other.data_type()),
                }),
            }
        }
        "sqrt" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("sqrt() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                v => {
                    let x = value_to_f64(v).ok_or_else(|| EvalError::TypeMismatch {
                        detail: alloc::format!("sqrt() needs numeric, got {:?}", v.data_type()),
                    })?;
                    if x < 0.0 {
                        return Err(EvalError::TypeMismatch {
                            detail: "sqrt(): negative input outside real domain".into(),
                        });
                    }
                    if x == 0.0 {
                        return Ok(Value::Float(0.0));
                    }
                    Ok(Value::Float(f64_sqrt(x)))
                }
            }
        }
        "power" | "pow" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("power() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let x = value_to_f64(&args[0]).ok_or_else(|| EvalError::TypeMismatch {
                detail: "power() needs numeric x".into(),
            })?;
            let y = value_to_f64(&args[1]).ok_or_else(|| EvalError::TypeMismatch {
                detail: "power() needs numeric y".into(),
            })?;
            // Integer-exponent fast path.
            let y_int = y as i32;
            if (y_int as f64) == y && y.abs() < 1024.0 {
                let result = f64_powi(x, y_int);
                return Ok(Value::Float(result));
            }
            // Fractional exponent — only defined for x >= 0 in real
            // arithmetic. Negative x raised to fractional power is
            // complex; reject cleanly.
            if x < 0.0 {
                return Err(EvalError::TypeMismatch {
                    detail: "power(): negative base with fractional exponent yields complex result"
                        .into(),
                });
            }
            if x == 0.0 && y < 0.0 {
                return Err(EvalError::TypeMismatch {
                    detail: "power(): 0 raised to negative power is undefined".into(),
                });
            }
            if x == 0.0 {
                return Ok(Value::Float(0.0));
            }
            Ok(Value::Float(f64_exp(y * f64_ln(x))))
        }
        "mod" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("mod() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let to_i64 = |v: &Value| -> Result<i64, EvalError> {
                match v {
                    Value::SmallInt(x) => Ok(i64::from(*x)),
                    Value::Int(x) => Ok(i64::from(*x)),
                    Value::BigInt(x) => Ok(*x),
                    other => Err(EvalError::TypeMismatch {
                        detail: alloc::format!("mod() needs integer, got {:?}", other.data_type()),
                    }),
                }
            };
            let y = to_i64(&args[0])?;
            let x = to_i64(&args[1])?;
            if x == 0 {
                return Err(EvalError::TypeMismatch {
                    detail: "mod(): division by zero".into(),
                });
            }
            // Rust's `%` operator on signed integers follows the
            // dividend's sign — same as PG.
            let result = y % x;
            // Pick the narrowest type that holds the result.
            if let Ok(small) = i16::try_from(result) {
                if matches!(args[0], Value::SmallInt(_)) && matches!(args[1], Value::SmallInt(_)) {
                    return Ok(Value::SmallInt(small));
                }
            }
            if let Ok(int_) = i32::try_from(result) {
                if !matches!(args[0], Value::BigInt(_)) && !matches!(args[1], Value::BigInt(_)) {
                    return Ok(Value::Int(int_));
                }
            }
            Ok(Value::BigInt(result))
        }
        "greatest" | "least" => {
            if args.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "{lc}() takes at least 1 arg",
                        lc = if name.eq_ignore_ascii_case("greatest") {
                            "greatest"
                        } else {
                            "least"
                        }
                    ),
                });
            }
            let non_null: alloc::vec::Vec<&Value> =
                args.iter().filter(|v| !matches!(v, Value::Null)).collect();
            if non_null.is_empty() {
                return Ok(Value::Null);
            }
            let is_greatest = name.eq_ignore_ascii_case("greatest");
            let mut best = non_null[0].clone();
            for v in &non_null[1..] {
                let ord = value_cmp_for_min_max(&best, v);
                let take = if is_greatest {
                    ord == core::cmp::Ordering::Less
                } else {
                    ord == core::cmp::Ordering::Greater
                };
                if take {
                    best = (*v).clone();
                }
            }
            Ok(best)
        }
        // MySQL `ifnull(a, b)` — alias for coalesce(a, b).
        // Used by every ORM with a MySQL target (Hibernate /
        // Laravel / Sequelize).
        "ifnull" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("ifnull() takes 2 args, got {}", args.len()),
                });
            }
            for v in args {
                if !matches!(v, Value::Null) {
                    return Ok(v.clone());
                }
            }
            Ok(Value::Null)
        }
        // MySQL `if(cond, then, else)` — alias for CASE WHEN.
        // NULL condition → else branch (MySQL semantic).
        // Integer condition: nonzero is true.
        "if" => {
            if args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "if() takes 3 args (cond, then, else), got {}",
                        args.len()
                    ),
                });
            }
            let truthy = match &args[0] {
                Value::Null => false,
                Value::Bool(b) => *b,
                Value::SmallInt(n) => *n != 0,
                Value::Int(n) => *n != 0,
                Value::BigInt(n) => *n != 0,
                Value::Float(x) => *x != 0.0,
                Value::Text(s) => !s.is_empty() && s != "0",
                _ => true,
            };
            if truthy {
                Ok(args[1].clone())
            } else {
                Ok(args[2].clone())
            }
        }
        "nullif" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("nullif() takes 2 args, got {}", args.len()),
                });
            }
            match (&args[0], &args[1]) {
                (Value::Null, _) => Ok(Value::Null),
                (a, Value::Null) => Ok(a.clone()),
                (a, b) => {
                    // Use value_cmp (already defined as Ord-like
                    // function in lib.rs) — but it's not accessible
                    // here. Fall back to direct equality.
                    if values_equal_for_nullif(a, b) {
                        Ok(Value::Null)
                    } else {
                        Ok(a.clone())
                    }
                }
            }
        }
        "trunc" => {
            match args.len() {
                1 => match &args[0] {
                    Value::Null => Ok(Value::Null),
                    Value::SmallInt(_) | Value::Int(_) | Value::BigInt(_) => Ok(args[0].clone()),
                    Value::Float(x) => Ok(Value::Float(f64_trunc(*x))),
                    Value::Numeric { scaled, scale } => {
                        let factor = pow10_i128(*scale);
                        // Truncate toward zero — sign-preserving division.
                        let q = scaled / factor;
                        Ok(Value::Numeric {
                            scaled: q * factor,
                            scale: *scale,
                        })
                    }
                    other => Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "trunc() needs numeric, got {:?}",
                            other.data_type()
                        ),
                    }),
                },
                2 => {
                    if args.iter().any(|v| matches!(v, Value::Null)) {
                        return Ok(Value::Null);
                    }
                    let n = match &args[1] {
                        Value::SmallInt(x) => i32::from(*x),
                        Value::Int(x) => *x,
                        Value::BigInt(x) => {
                            i32::try_from(*x).map_err(|_| EvalError::TypeMismatch {
                                detail: "trunc(): scale must fit in i32".into(),
                            })?
                        }
                        other => {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!(
                                    "trunc(): scale must be integer, got {:?}",
                                    other.data_type()
                                ),
                            });
                        }
                    };
                    let x = match &args[0] {
                        Value::SmallInt(v) => f64::from(*v),
                        Value::Int(v) => f64::from(*v),
                        Value::BigInt(v) => *v as f64,
                        Value::Float(v) => *v,
                        Value::Numeric { scaled, scale } => {
                            (*scaled as f64) / f64_powi(10.0, i32::from(*scale))
                        }
                        other => {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!(
                                    "trunc() needs numeric x, got {:?}",
                                    other.data_type()
                                ),
                            });
                        }
                    };
                    let result = if n >= 0 {
                        let factor = f64_powi(10.0, n);
                        f64_trunc(x * factor) / factor
                    } else {
                        let factor = f64_powi(10.0, -n);
                        f64_trunc(x / factor) * factor
                    };
                    Ok(Value::Float(result))
                }
                _ => Err(EvalError::TypeMismatch {
                    detail: alloc::format!("trunc() takes 1 or 2 args, got {}", args.len()),
                }),
            }
        }
        "round" => {
            match args.len() {
                1 => match &args[0] {
                    Value::Null => Ok(Value::Null),
                    Value::SmallInt(_) | Value::Int(_) | Value::BigInt(_) => Ok(args[0].clone()),
                    Value::Float(x) => Ok(Value::Float(f64_round_half_away(*x))),
                    Value::Numeric { scaled, scale } => {
                        let factor = pow10_i128(*scale);
                        let q = scaled.div_euclid(factor);
                        let r = scaled.rem_euclid(factor);
                        // Half-away-from-zero: if 2*r >= factor → round up.
                        let result = if 2 * r >= factor { q + 1 } else { q };
                        Ok(Value::Numeric {
                            scaled: result * factor,
                            scale: *scale,
                        })
                    }
                    other => Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "round() needs numeric, got {:?}",
                            other.data_type()
                        ),
                    }),
                },
                2 => {
                    if args.iter().any(|v| matches!(v, Value::Null)) {
                        return Ok(Value::Null);
                    }
                    let n = match &args[1] {
                        Value::SmallInt(x) => i32::from(*x),
                        Value::Int(x) => *x,
                        Value::BigInt(x) => {
                            i32::try_from(*x).map_err(|_| EvalError::TypeMismatch {
                                detail: "round(): scale must fit in i32".into(),
                            })?
                        }
                        other => {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!(
                                    "round(): scale must be integer, got {:?}",
                                    other.data_type()
                                ),
                            });
                        }
                    };
                    // Convert input to f64 for arithmetic
                    // simplicity (PG does NUMERIC math here but
                    // SPG's f64 path matches the dominant
                    // customer expectation for round(N, scale)
                    // patterns).
                    let x = match &args[0] {
                        Value::SmallInt(v) => f64::from(*v),
                        Value::Int(v) => f64::from(*v),
                        Value::BigInt(v) => *v as f64,
                        Value::Float(v) => *v,
                        Value::Numeric { scaled, scale } => {
                            (*scaled as f64) / f64_powi(10.0, i32::from(*scale))
                        }
                        other => {
                            return Err(EvalError::TypeMismatch {
                                detail: alloc::format!(
                                    "round() needs numeric x, got {:?}",
                                    other.data_type()
                                ),
                            });
                        }
                    };
                    // Avoid float precision drift from the
                    // 10^(-k) reciprocal — for n<0 work with the
                    // positive-exponent form: round(x / 10^|n|) *
                    // 10^|n|.
                    let result = if n >= 0 {
                        let factor = f64_powi(10.0, n);
                        f64_round_half_away(x * factor) / factor
                    } else {
                        let factor = f64_powi(10.0, -n);
                        f64_round_half_away(x / factor) * factor
                    };
                    Ok(Value::Float(result))
                }
                _ => Err(EvalError::TypeMismatch {
                    detail: alloc::format!("round() takes 1 or 2 args, got {}", args.len()),
                }),
            }
        }
        "ceil" | "ceiling" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("ceil() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::SmallInt(_) | Value::Int(_) | Value::BigInt(_) => Ok(args[0].clone()),
                Value::Float(x) => Ok(Value::Float(f64_ceil(*x))),
                Value::Numeric { scaled, scale } => {
                    let factor = pow10_i128(*scale);
                    let q = scaled.div_euclid(factor);
                    let r = scaled.rem_euclid(factor);
                    let result = if r == 0 { q } else { q + 1 };
                    Ok(Value::Numeric {
                        scaled: result * factor,
                        scale: *scale,
                    })
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!("ceil() needs numeric, got {:?}", other.data_type()),
                }),
            }
        }
        "floor" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("floor() takes 1 arg, got {}", args.len()),
                });
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::SmallInt(_) | Value::Int(_) | Value::BigInt(_) => Ok(args[0].clone()),
                Value::Float(x) => Ok(Value::Float(f64_floor(*x))),
                Value::Numeric { scaled, scale } => {
                    let factor = pow10_i128(*scale);
                    let q = scaled.div_euclid(factor);
                    // div_euclid rounds toward -infinity which is
                    // exactly the floor semantic — perfect for
                    // negative values.
                    Ok(Value::Numeric {
                        scaled: q * factor,
                        scale: *scale,
                    })
                }
                other => Err(EvalError::TypeMismatch {
                    detail: alloc::format!("floor() needs numeric, got {:?}", other.data_type()),
                }),
            }
        }
        "left" => string_left_right(args, true, "left"),
        "right" => string_left_right(args, false, "right"),
        "strpos" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "strpos() takes 2 args (haystack, needle), got {}",
                        args.len()
                    ),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let haystack = value_to_format_text(&args[0]);
            let needle = value_to_format_text(&args[1]);
            if needle.is_empty() {
                return Ok(Value::Int(1));
            }
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
        "lpad" => string_pad(args, true, "lpad"),
        "rpad" => string_pad(args, false, "rpad"),
        "repeat" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("repeat() takes 2 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let s = value_to_format_text(&args[0]);
            let n = match &args[1] {
                Value::SmallInt(x) => i64::from(*x),
                Value::Int(x) => i64::from(*x),
                Value::BigInt(x) => *x,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "repeat(): n must be integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            if n <= 0 {
                return Ok(Value::Text(String::new()));
            }
            // Safety cap so a runaway argument doesn't allocate
            // terabytes. PG itself enforces a similar cap via
            // work_mem; SPG inherits a defensive 64MiB cap.
            const MAX_REPEAT_BYTES: usize = 64 * 1024 * 1024;
            let needed =
                s.len()
                    .checked_mul(n as usize)
                    .ok_or_else(|| EvalError::TypeMismatch {
                        detail: "repeat(): result size overflows usize".into(),
                    })?;
            if needed > MAX_REPEAT_BYTES {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "repeat(): result would exceed {MAX_REPEAT_BYTES} bytes"
                    ),
                });
            }
            Ok(Value::Text(s.repeat(n as usize)))
        }
        "split_part" => {
            if args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "split_part() takes 3 args (string, delim, n), got {}",
                        args.len()
                    ),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let s = value_to_format_text(&args[0]);
            let delim = value_to_format_text(&args[1]);
            if delim.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: "split_part(): delimiter cannot be empty".into(),
                });
            }
            let n = match &args[2] {
                Value::SmallInt(x) => i64::from(*x),
                Value::Int(x) => i64::from(*x),
                Value::BigInt(x) => *x,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!(
                            "split_part(): n must be integer, got {:?}",
                            other.data_type()
                        ),
                    });
                }
            };
            if n == 0 {
                return Err(EvalError::TypeMismatch {
                    detail: "split_part(): n must be nonzero (PG: 1-indexed)".into(),
                });
            }
            let parts: alloc::vec::Vec<&str> = s.split(&delim[..]).collect();
            let total = parts.len() as i64;
            let idx = if n > 0 {
                n - 1
            } else {
                // n=-1 → last (idx = total - 1)
                total + n
            };
            if idx < 0 || idx >= total {
                return Ok(Value::Text(String::new()));
            }
            Ok(Value::Text(parts[idx as usize].to_string()))
        }
        // PG `translate(s, from, to)` — char-by-char positional
        // mapping. Each codepoint in `from` is replaced by the
        // codepoint at the same index in `to`. When `from` is
        // longer than `to`, the extra `from` codepoints are
        // DELETED (not replaced). When `from` has duplicates,
        // the FIRST occurrence's mapping wins. NULL → NULL.
        "translate" => {
            if args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("translate() takes 3 args, got {}", args.len()),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let s = value_to_format_text(&args[0]);
            let from = value_to_format_text(&args[1]);
            let to = value_to_format_text(&args[2]);
            let from_chars: Vec<char> = from.chars().collect();
            let to_chars: Vec<char> = to.chars().collect();
            // Build the codepoint map. First occurrence wins.
            let mut map: alloc::collections::BTreeMap<char, Option<char>> =
                alloc::collections::BTreeMap::new();
            for (i, &fc) in from_chars.iter().enumerate() {
                if map.contains_key(&fc) {
                    continue;
                }
                let replacement = to_chars.get(i).copied();
                map.insert(fc, replacement);
            }
            let mut out = String::with_capacity(s.len());
            for c in s.chars() {
                match map.get(&c) {
                    Some(Some(r)) => out.push(*r),
                    Some(None) => {} // mapped to "deleted"
                    None => out.push(c),
                }
            }
            Ok(Value::Text(out))
        }
        "replace" => {
            if args.len() != 3 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "replace() takes 3 args (string, from, to), got {}",
                        args.len()
                    ),
                });
            }
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let s = value_to_format_text(&args[0]);
            let from = value_to_format_text(&args[1]);
            let to = value_to_format_text(&args[2]);
            if from.is_empty() {
                return Ok(Value::Text(s));
            }
            // std `String::replace` matches PG semantics exactly:
            // non-overlapping, left-to-right, no re-scan of
            // inserted text. Sealed test surface verifies the
            // edge cases independently.
            Ok(Value::Text(s.replace(&from[..], &to)))
        }
        "trim" | "btrim" => string_trim(args, TrimSide::Both, "trim"),
        "ltrim" => string_trim(args, TrimSide::Left, "ltrim"),
        "rtrim" => string_trim(args, TrimSide::Right, "rtrim"),
        "concat_ws" => {
            if args.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: "concat_ws() requires at least 1 arg (the separator)".into(),
                });
            }
            // NULL separator poisons the result.
            let sep = match &args[0] {
                Value::Null => return Ok(Value::Null),
                v => value_to_format_text(v),
            };
            let mut out = String::new();
            let mut first = true;
            for v in &args[1..] {
                if matches!(v, Value::Null) {
                    continue;
                }
                if first {
                    first = false;
                } else {
                    out.push_str(&sep);
                }
                out.push_str(&value_to_format_text(v));
            }
            Ok(Value::Text(out))
        }
        // v7.17.0 Phase 3.7 — PG regex function family.
        "regexp_matches" => regexp_matches(args),
        "regexp_replace" => regexp_replace(args),
        "regexp_split_to_array" => regexp_split_to_array(args),
        // v7.17.0 Phase 3.P0-28 — PG JSON builder family.
        // to_json / to_jsonb coerce any value to JSON text (NULL
        // becomes the JSON literal 'null', not SQL NULL).
        "to_json" | "to_jsonb" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("to_json() takes 1 arg, got {}", args.len()),
                });
            }
            // Json input passes through verbatim — PG identity.
            if let Value::Json(s) = &args[0] {
                return Ok(Value::Json(s.clone()));
            }
            Ok(Value::Json(crate::json::value_to_json_text(&args[0])))
        }
        "json_build_object" | "jsonb_build_object" => crate::json::build_object(args),
        "json_build_array" | "jsonb_build_array" => crate::json::build_array(args),
        "jsonb_set" | "json_set" => crate::json::set(args),
        "jsonb_insert" | "json_insert" => crate::json::insert(args),
        // v7.17.0 Phase 3.9 — PG `jsonb_path_query` family.
        "jsonb_path_query" | "json_path_query" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("jsonb_path_query() takes 2 args, got {}", args.len()),
                });
            }
            crate::json::path_query(&args[0], &args[1])
        }
        "jsonb_path_query_first" | "json_path_query_first" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "jsonb_path_query_first() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            crate::json::path_query_first(&args[0], &args[1])
        }
        "jsonb_path_query_array" | "json_path_query_array" => {
            if args.len() != 2 {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!(
                        "jsonb_path_query_array() takes 2 args, got {}",
                        args.len()
                    ),
                });
            }
            crate::json::path_query_array(&args[0], &args[1])
        }
        // v7.17.0 Phase 7 — INET / CIDR network helpers.
        "host" => inet_host(args),
        "network" => inet_network(args),
        "masklen" => inet_masklen(args),
        // v6.4.3 — encode/decode + error_on_null SQL function bundle.
        "encode" => encode_text(args),
        "decode" => decode_text(args),
        "error_on_null" => error_on_null(args),
        // v7.12.1 — PG full-text search lexer / tsquery builders.
        // mailrs G-CRIT-3 acceptance path: `to_tsvector('english',
        // … || ' ' || … || …)` runs end-to-end against a tsvector
        // column with Porter stemming + standard english stopwords.
        "to_tsvector" => fts_to_tsvector(args, ctx),
        // v7.24 (round-16 C) — setweight(tsvector, 'A'..'D'): label
        // every lexeme. mailrs's migrate-016 search trigger builds
        // its vector as setweight(to_tsvector(…),'A') || ….
        "setweight" => fts_setweight(args),
        // v7.24 (round-15) — string_to_array(text, delim): inverse
        // of array_to_string. PG semantics: NULL text → NULL,
        // '' → empty array, NULL delim → one element per char.
        "string_to_array" => fn_string_to_array(args),
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
        // v7.17.0 Phase 3.P0-30 — session / introspection functions.
        // Engine-level dispatch so these compose inside expressions
        // (`WHERE schemaname = current_schema()`, `SELECT *,
        // database() AS db FROM t`) — the pgwire layer's canned
        // shortcuts only catch the bare top-level SELECT shape.
        // SPG is single-database + single-schema; the values
        // mirror the wire-layer canned defaults.
        "current_database" | "database" => Ok(Value::Text("spg".into())),
        "current_schema" => Ok(Value::Text("public".into())),
        "current_user" | "session_user" | "user" => Ok(Value::Text("admin".into())),
        // v7.17.0 Phase 3.P0-31 — `pg_typeof(any)` returns the
        // canonical PG lowercase type name. sqlx / SQLAlchemy /
        // Diesel emit this during describe; generic ORMs may
        // branch on it (`CASE WHEN pg_typeof(x) = 'jsonb' ...`).
        // NULL has no resolved value-level type → 'unknown' per
        // PG semantics.
        "pg_typeof" => {
            if args.len() != 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!("pg_typeof() takes 1 arg, got {}", args.len()),
                });
            }
            Ok(Value::Text(pg_typeof_name(&args[0]).into()))
        }
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

/// v7.24 (round-15) — `string_to_array(text, delimiter)`.
fn fn_string_to_array(args: &[Value]) -> Result<Value, EvalError> {
    let [text_arg, delim_arg] = args else {
        return Err(EvalError::TypeMismatch {
            detail: alloc::format!("string_to_array expects 2 arguments, got {}", args.len()),
        });
    };
    let text = match text_arg {
        Value::Null => return Ok(Value::Null),
        Value::Text(t) => t,
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!("string_to_array expects text, got {:?}", other.data_type()),
            });
        }
    };
    // PG (9.1+): empty input → empty array, regardless of delimiter.
    if text.is_empty() {
        return Ok(Value::TextArray(Vec::new()));
    }
    let parts: Vec<Option<String>> = match delim_arg {
        // NULL delimiter → one element per character.
        Value::Null => text.chars().map(|c| Some(c.to_string())).collect(),
        Value::Text(d) if d.is_empty() => alloc::vec![Some(text.clone())],
        Value::Text(d) => text
            .split(d.as_str())
            .map(|p| Some(p.to_string()))
            .collect(),
        other => {
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "string_to_array delimiter must be text, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    Ok(Value::TextArray(parts))
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
        "epoch" => F::Epoch,
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

// `to_char(value, format)` — render a DATE / TIMESTAMP through a PG
// format template. Supports the high-traffic placeholders:
//   YYYY YY MM Mon Month DD HH24 HH12 MI SS MS US AM PM
// Unrecognised characters pass through literally so the template's
// punctuation ('-', ':', ' ', '/') needs no escape mechanism.

/// Helper: coerce a Value to an Option<String> for regex args. NULL
/// propagates as None (caller short-circuits to Value::Null).
fn text_arg(v: &Value) -> Result<Option<String>, EvalError> {
    match v {
        Value::Text(s) => Ok(Some(s.clone())),
        Value::Null => Ok(None),
        other => Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "regex function expects TEXT arg, got {:?}",
                other.data_type()
            ),
        }),
    }
}

/// Compare two values for min/max selection. Returns Equal when
/// values are equal (including cross-numeric-width), Less when
/// a < b, Greater when a > b. NULL handling is upstream.
fn value_cmp_for_min_max(a: &Value, b: &Value) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    // Integer-widen first (covers SmallInt vs Int vs BigInt).
    let a_int = match a {
        Value::SmallInt(x) => Some(i64::from(*x)),
        Value::Int(x) => Some(i64::from(*x)),
        Value::BigInt(x) => Some(*x),
        _ => None,
    };
    let b_int = match b {
        Value::SmallInt(x) => Some(i64::from(*x)),
        Value::Int(x) => Some(i64::from(*x)),
        Value::BigInt(x) => Some(*x),
        _ => None,
    };
    if let (Some(av), Some(bv)) = (a_int, b_int) {
        return av.cmp(&bv);
    }
    // Float-widen.
    let a_f = value_to_f64(a);
    let b_f = value_to_f64(b);
    if let (Some(av), Some(bv)) = (a_f, b_f) {
        return av.partial_cmp(&bv).unwrap_or(Ordering::Equal);
    }
    // Text/Text.
    match (a, b) {
        (Value::Text(av), Value::Text(bv)) => av.cmp(bv),
        (Value::Bytes(av), Value::Bytes(bv)) => av.cmp(bv),
        _ => Ordering::Equal,
    }
}

fn value_to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Float(x) => Some(*x),
        Value::SmallInt(x) => Some(f64::from(*x)),
        Value::Int(x) => Some(f64::from(*x)),
        Value::BigInt(x) => Some(*x as f64),
        Value::Numeric { scaled, scale } => {
            Some((*scaled as f64) / f64_powi(10.0, i32::from(*scale)))
        }
        _ => None,
    }
}

/// PG-style equality for nullif. Handles cross-numeric-width
/// comparison (Int vs BigInt vs SmallInt vs Float vs Numeric);
/// text matches text exactly; everything else uses derived
/// PartialEq.
fn values_equal_for_nullif(a: &Value, b: &Value) -> bool {
    // Same-type fast path.
    if a == b {
        return true;
    }
    // Cross-int widening: SmallInt / Int / BigInt all comparable.
    let a_int = match a {
        Value::SmallInt(x) => Some(i64::from(*x)),
        Value::Int(x) => Some(i64::from(*x)),
        Value::BigInt(x) => Some(*x),
        _ => None,
    };
    let b_int = match b {
        Value::SmallInt(x) => Some(i64::from(*x)),
        Value::Int(x) => Some(i64::from(*x)),
        Value::BigInt(x) => Some(*x),
        _ => None,
    };
    if let (Some(a), Some(b)) = (a_int, b_int) {
        return a == b;
    }
    // Float / Numeric: widen to f64.
    let a_f = match a {
        Value::Float(x) => Some(*x),
        Value::SmallInt(x) => Some(f64::from(*x)),
        Value::Int(x) => Some(f64::from(*x)),
        Value::BigInt(x) => Some(*x as f64),
        Value::Numeric { scaled, scale } => {
            Some((*scaled as f64) / f64_powi(10.0, i32::from(*scale)))
        }
        _ => None,
    };
    let b_f = match b {
        Value::Float(x) => Some(*x),
        Value::SmallInt(x) => Some(f64::from(*x)),
        Value::Int(x) => Some(f64::from(*x)),
        Value::BigInt(x) => Some(*x as f64),
        Value::Numeric { scaled, scale } => {
            Some((*scaled as f64) / f64_powi(10.0, i32::from(*scale)))
        }
        _ => None,
    };
    if let (Some(a), Some(b)) = (a_f, b_f) {
        return a == b;
    }
    false
}

/// v7.17.0 — generate a RFC 4122 v4 (random) UUID. Layout: 16
/// random bytes with the version nibble (high nibble of byte 6)
/// pinned to `0100` (= 4) and the variant top bits (high two bits
/// of byte 8) pinned to `10` — exactly what PG's
/// `gen_random_uuid()` and the historical uuid-ossp
/// `uuid_generate_v4()` produce.
pub fn gen_random_uuid_bytes() -> [u8; 16] {
    let mut out = [0u8; 16];
    let hi = prng_next_u64().to_be_bytes();
    let lo = prng_next_u64().to_be_bytes();
    out[..8].copy_from_slice(&hi);
    out[8..].copy_from_slice(&lo);
    // Version 4: top nibble of byte 6 must be 0100.
    out[6] = (out[6] & 0x0f) | 0x40;
    // Variant 1 (RFC 4122): top two bits of byte 8 must be 10.
    out[8] = (out[8] & 0x3f) | 0x80;
    out
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

/// v7.17.0 Phase 3.P0-29 — MySQL `DATE_FORMAT(t, fmt)`.
///
/// Format tokens (MySQL 8.0 surface):
///   * `%Y` — 4-digit year  `%y` — 2-digit year
///   * `%m` — 01-12 month   `%c` — 1-12 month (no zero pad)
///   * `%d` — 01-31 day     `%e` — 1-31 day (no zero pad)
///   * `%H` — 00-23 hour    `%h` / `%I` — 01-12 hour
///   * `%i` — 00-59 MINUTE (NB: `%M` is month name in MySQL — easy
///     footgun if we mirror PG's `to_char` tokens by accident)
///   * `%s` / `%S` — 00-59 second
///   * `%f` — 000000-999999 microseconds (always 6 digits)
///   * `%p` — AM / PM
///   * `%M` — January-December (full month name)
///   * `%b` — Jan-Dec (abbreviated month name)
///   * `%%` — literal `%`
///
/// Unknown `%X` tokens pass through verbatim (MySQL emits the `%`
/// then the unknown letter).
fn date_format_mysql(args: &[Value]) -> Result<Value, EvalError> {
    use core::fmt::Write as _;
    if args.len() != 2 {
        return Err(EvalError::TypeMismatch {
            detail: format!("date_format() takes 2 args, got {}", args.len()),
        });
    }
    if matches!(&args[0], Value::Null) || matches!(&args[1], Value::Null) {
        return Ok(Value::Null);
    }
    let Value::Text(fmt) = &args[1] else {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "date_format() needs a text format, got {:?}",
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
                    "date_format() needs DATE or TIMESTAMP, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let (y, mo, d) = civil_from_days(days);
    let secs = day_micros / 1_000_000;
    let frac = day_micros % 1_000_000;
    let hh24 = u32::try_from(secs / 3600).unwrap_or(0);
    let mi = u32::try_from((secs / 60) % 60).unwrap_or(0);
    let ss = u32::try_from(secs % 60).unwrap_or(0);
    let hh12 = match hh24 % 12 {
        0 => 12,
        x => x,
    };
    let ampm = if hh24 < 12 { "AM" } else { "PM" };
    let us = u32::try_from(frac).unwrap_or(0);

    let mut out = String::with_capacity(fmt.len() + 8);
    let bytes = fmt.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        if i + 1 >= bytes.len() {
            // Trailing `%` with no specifier — emit verbatim.
            out.push('%');
            i += 1;
            continue;
        }
        let token = bytes[i + 1];
        match token {
            b'Y' => {
                let _ = write!(out, "{y:04}");
            }
            b'y' => {
                #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                let yy = (y.rem_euclid(100)) as u32;
                let _ = write!(out, "{yy:02}");
            }
            b'm' => {
                let _ = write!(out, "{mo:02}");
            }
            b'c' => {
                let _ = write!(out, "{mo}");
            }
            b'd' => {
                let _ = write!(out, "{d:02}");
            }
            b'e' => {
                let _ = write!(out, "{d}");
            }
            b'H' => {
                let _ = write!(out, "{hh24:02}");
            }
            b'h' | b'I' => {
                let _ = write!(out, "{hh12:02}");
            }
            b'i' => {
                // MINUTE — distinct from PG's `MI` and from MySQL's
                // own `%M` (month name).
                let _ = write!(out, "{mi:02}");
            }
            b's' | b'S' => {
                let _ = write!(out, "{ss:02}");
            }
            b'f' => {
                let _ = write!(out, "{us:06}");
            }
            b'p' => {
                out.push_str(ampm);
            }
            b'M' => {
                out.push_str(MONTH_FULL[(mo - 1) as usize]);
            }
            b'b' => {
                out.push_str(MONTH_ABBR[(mo - 1) as usize]);
            }
            b'%' => {
                out.push('%');
            }
            other => {
                // Unknown specifier — MySQL emits the letter
                // verbatim (without the `%`).
                out.push(other as char);
            }
        }
        i += 2;
    }
    Ok(Value::Text(out))
}

/// v7.17.0 Phase 3.P0-29 — `UNIX_TIMESTAMP(t)` returns epoch
/// seconds (BIGINT) for a TIMESTAMP / DATE.
///
/// Bare `UNIX_TIMESTAMP()` (no args) is folded to a BigInt literal
/// by clock_replacement_for at the rewrite layer — never reaches
/// this arm.
fn unix_timestamp_of(args: &[Value]) -> Result<Value, EvalError> {
    if args.len() != 1 {
        return Err(EvalError::TypeMismatch {
            detail: format!("unix_timestamp() takes 0 or 1 arg, got {}", args.len()),
        });
    }
    match &args[0] {
        Value::Null => Ok(Value::Null),
        Value::Timestamp(t) => Ok(Value::BigInt(t.div_euclid(1_000_000))),
        Value::Date(d) => Ok(Value::BigInt(i64::from(*d) * 86_400)),
        other => Err(EvalError::TypeMismatch {
            detail: format!(
                "unix_timestamp() needs DATE or TIMESTAMP, got {:?}",
                other.data_type()
            ),
        }),
    }
}

/// v7.17.0 Phase 3.P0-29 — `FROM_UNIXTIME(n)` returns a TIMESTAMP
/// at `n` seconds past the Unix epoch. `FROM_UNIXTIME(n, fmt)`
/// applies MySQL date_format on top, returning TEXT.
fn from_unixtime(args: &[Value]) -> Result<Value, EvalError> {
    if !(1..=2).contains(&args.len()) {
        return Err(EvalError::TypeMismatch {
            detail: format!("from_unixtime() takes 1 or 2 args, got {}", args.len()),
        });
    }
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    let secs: i64 = match &args[0] {
        Value::SmallInt(n) => i64::from(*n),
        Value::Int(n) => i64::from(*n),
        Value::BigInt(n) => *n,
        Value::Float(x) => *x as i64,
        Value::Numeric { scaled, scale } => {
            let denom = 10_i128.pow(u32::from(*scale));
            i64::try_from(scaled.div_euclid(denom)).unwrap_or(i64::MAX)
        }
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "from_unixtime() needs a numeric epoch second count, got {:?}",
                    other.data_type()
                ),
            });
        }
    };
    let ts = Value::Timestamp(secs.saturating_mul(1_000_000));
    if args.len() == 1 {
        Ok(ts)
    } else {
        date_format_mysql(&[ts, args[1].clone()])
    }
}

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
        // v7.17.0 — UUID renders canonical lowercase 8-4-4-4-12
        // hyphenated form (PG `uuid_out`).
        Value::Uuid(b) => spg_storage::format_uuid(b),
        // v7.17.0 Phase 3.P0-32 — TIME canonical text.
        Value::Time(us) => format_time(*us),
        // v7.17.0 Phase 3.P0-34 — TIMETZ canonical text.
        Value::TimeTz { us, offset_secs } => format_timetz(*us, *offset_secs),
        // v7.17.0 Phase 3.P0-33 — YEAR 4-digit zero-padded.
        Value::Year(y) => format!("{y:04}"),
        // v7.17.0 Phase 3.P0-35 — MONEY en_US locale.
        Value::Money(c) => format_money(*c),
        // v7.17.0 Phase 3.P0-38 — Range canonical form. Routes
        // through the engine's format_range_text to share the
        // single renderer with pgwire / sqllogictest.
        Value::Range { .. } => crate::conversions::format_range_text(v),
        // v7.17.0 Phase 3.P0-39 — Hstore canonical PG text form.
        Value::Hstore(pairs) => crate::conversions::format_hstore_text(pairs),
        // v7.17.0 Phase 3.P0-40 — 2D array canonical PG text form.
        Value::IntArray2D(rows) => crate::conversions::format_int_2d_text_pub(rows),
        Value::BigIntArray2D(rows) => crate::conversions::format_bigint_2d_text_pub(rows),
        Value::TextArray2D(rows) => crate::conversions::format_text_2d_text_pub(rows),
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

/// v7.17.0 Phase 3.P0-35 — PG `money` canonical text form, en_US
/// locale: `$N,NNN.CC`, negative → `-$1.23`. Mirrors PG's
/// `cash_out` for `lc_monetary = 'en_US.UTF-8'`.
pub fn format_money(cents: i64) -> String {
    let neg = cents < 0;
    let abs = cents.unsigned_abs();
    let dollars = abs / 100;
    let cc = abs % 100;
    // Insert comma thousands separators in the integer portion.
    let dollar_str = dollars.to_string();
    let bytes = dollar_str.as_bytes();
    let mut int_part = String::with_capacity(dollar_str.len() + dollar_str.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        // Position from the right: insert ',' before every 3rd
        // digit (except the first).
        let from_right = bytes.len() - i;
        if i > 0 && from_right % 3 == 0 {
            int_part.push(',');
        }
        int_part.push(*b as char);
    }
    let sign = if neg { "-" } else { "" };
    format!("{sign}${int_part}.{cc:02}")
}

/// v7.17.0 Phase 3.P0-34 — PG `TIMETZ` canonical text form
/// `HH:MM:SS[.ffffff]±HH[:MM]`. Mirrors PG `timetz_out`. The
/// offset uses `±HH` for whole-hour offsets and `±HH:MM` for
/// sub-hour offsets (matching PG's "minimal display" rule).
pub fn format_timetz(us: i64, offset_secs: i32) -> String {
    let time = format_time(us);
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let abs = offset_secs.unsigned_abs();
    let oh = abs / 3600;
    let om = (abs % 3600) / 60;
    if om == 0 {
        format!("{time}{sign}{oh:02}")
    } else {
        format!("{time}{sign}{oh:02}:{om:02}")
    }
}

/// v7.17.0 Phase 3.P0-32 — PG `TIME` canonical text form
/// `HH:MM:SS[.ffffff]`. Mirrors PG `time_out`. Trailing zeros in
/// the fractional component are stripped — `12:00:00.500000`
/// renders as `12:00:00.5` to match PG's text output.
pub fn format_time(us: i64) -> String {
    let total_secs = us.div_euclid(1_000_000);
    let frac = us.rem_euclid(1_000_000);
    let hh = total_secs / 3600;
    let mm = (total_secs / 60) % 60;
    let ss = total_secs % 60;
    if frac == 0 {
        format!("{hh:02}:{mm:02}:{ss:02}")
    } else {
        let raw = format!("{frac:06}");
        let trimmed = raw.trim_end_matches('0');
        format!("{hh:02}:{mm:02}:{ss:02}.{trimmed}")
    }
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

pub(crate) fn literal_to_value(l: &Literal) -> Value {
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
        Literal::TextArray(items) => Value::TextArray(items.clone()),
        Literal::IntArray(items) => Value::IntArray(items.clone()),
        Literal::BigIntArray(items) => Value::BigIntArray(items.clone()),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Null => Value::Null,
        Literal::Interval { months, micros, .. } => Value::Interval {
            months: *months,
            micros: *micros,
        },
    }
}

/// v7.17.0 Phase 2.5 — look up the collation of a column reference
/// in the current evaluation context. Returns `None` when the
/// expression is not a column reference (e.g. literal / function
/// call) or the column can't be resolved (caller falls back to
/// `Collation::Binary` semantics).
pub(crate) fn column_collation(e: &Expr, ctx: &EvalContext<'_>) -> Option<spg_storage::Collation> {
    let Expr::Column(c) = e else {
        return None;
    };
    // v7.31 (perf 3e) — zero-allocation segment matching (the
    // composite_eq pattern). This runs once per comparison eval —
    // 24k × per-row format! calls showed up as an allocator line
    // item in the inbox profile for a value that never changes
    // across rows.
    let matches_composite = |s: &str| {
        c.qualifier.as_deref().is_some_and(|q| {
            s.len() == q.len() + 1 + c.name.len()
                && s.as_bytes()[q.len()] == b'.'
                && s.starts_with(q)
                && s.ends_with(c.name.as_str())
        })
    };
    if c.qualifier.is_some()
        && let Some(s) = ctx.columns.iter().find(|s| matches_composite(&s.name))
    {
        return Some(s.collation);
    }
    if let Some(s) = ctx.columns.iter().find(|s| s.name == c.name) {
        return Some(s.collation);
    }
    // Bare-name fallback for joined schemas (same shape as
    // resolve_column): match a single composite ending in
    // ".<name>".
    let ends_with_dot_name = |s: &str| {
        // usize: `len > name.len()` ≡ `len >= name.len() + 1`
        // (rust 1.96 clippy::int_plus_one sweep).
        s.len() > c.name.len()
            && s.ends_with(c.name.as_str())
            && s.as_bytes()[s.len() - c.name.len() - 1] == b'.'
    };
    let mut matches = ctx.columns.iter().filter(|s| ends_with_dot_name(&s.name));
    let first = matches.next();
    let extra = matches.next();
    match (first, extra) {
        (Some(s), None) => Some(s.collation),
        _ => None,
    }
}

/// v7.17.0 Phase 2.5 — if the comparison op is text-equality and
/// either operand references a CaseInsensitive column, return
/// ASCII-folded copies of both Text values; otherwise pass
/// through. Only Eq / NotEq / Lt / LtEq / Gt / GtEq trigger the
/// fold — relational operators on text still honour collation
/// the same way (PG semantics). Non-Text values pass through.
fn collation_fold_for_compare(
    op: BinOp,
    lhs: &Expr,
    rhs: &Expr,
    l: Value,
    r: Value,
    ctx: &EvalContext<'_>,
) -> (Value, Value) {
    if !matches!(
        op,
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq
    ) {
        return (l, r);
    }
    let lhs_col = column_collation(lhs, ctx);
    let rhs_col = column_collation(rhs, ctx);
    let ci = matches!(lhs_col, Some(spg_storage::Collation::CaseInsensitive))
        || matches!(rhs_col, Some(spg_storage::Collation::CaseInsensitive));
    if !ci {
        return (l, r);
    }
    let fold = |v: Value| match v {
        Value::Text(s) => Value::Text(s.to_ascii_lowercase()),
        other => other,
    };
    (fold(l), fold(r))
}

/// v7.32 (P4 borrow channel) — borrowed-or-owned evaluation. A bare
/// column read borrows its cell (no clone); literals and computed
/// sub-expressions stay owned. Used by the comparison fast path in
/// `eval_expr` so a predicate like `col != ''` reads the cell by
/// reference instead of cloning it per scanned row. Semantically
/// identical to `eval_expr` — a borrowed cell compares equal to its
/// clone — and the fallback to owned `resolve_column` preserves the
/// detailed not-found / unknown-qualifier errors.
fn eval_expr_cow<'r>(
    expr: &Expr,
    row: &'r Row,
    ctx: &EvalContext<'_>,
) -> Result<Cow<'r, Value>, EvalError> {
    match expr {
        Expr::Column(c) => match resolve_column_borrowed(c, row, ctx)? {
            Some(v) => Ok(Cow::Borrowed(v)),
            None => resolve_column(c, row, ctx).map(Cow::Owned),
        },
        _ => eval_expr(expr, row, ctx).map(Cow::Owned),
    }
}

/// v7.32 (P4 borrow channel) — operands whose comparison `apply_binary`
/// does NOT route through the plain ref-based `compare`: NUMERIC goes
/// through fixed-point `apply_binary_numeric` and INTERVAL through
/// `apply_binary_interval`. The borrowed comparison fast path falls
/// back to the owned path for these so their semantics are untouched.
#[inline]
fn is_owned_compare_value(v: &Value) -> bool {
    matches!(v, Value::Numeric { .. } | Value::Interval { .. })
}

/// v7.32 (P4 borrow channel) — does a comparison need case-insensitive
/// folding? Mirrors the trigger in `collation_fold_for_compare`; when
/// true the fast path defers to the owned path so the fold still runs.
#[inline]
fn compare_is_case_insensitive(lhs: &Expr, rhs: &Expr, ctx: &EvalContext<'_>) -> bool {
    matches!(
        column_collation(lhs, ctx),
        Some(spg_storage::Collation::CaseInsensitive)
    ) || matches!(
        column_collation(rhs, ctx),
        Some(spg_storage::Collation::CaseInsensitive)
    )
}

/// v7.29 - borrow a column cell without cloning (the prefix fast
/// path for LEFT). Mirrors resolve_column's lookup; returns Ok(None)
/// when the reference can't be attributed (caller falls back to the
/// generic owned path, which will surface the proper error).
/// v7.30 (perf campaign) - zero-allocation composite-name match:
/// does `schema_name` equal `qualifier ++ '.' ++ name`? The old path
/// FORMATTED a fresh String per column reference per row (~290k
/// format+compare pairs per 24k-row aggregate query) - the single
/// hottest residue on the inbox profile.
#[inline]
fn composite_eq(schema_name: &str, qualifier: &str, name: &str) -> bool {
    schema_name.len() == qualifier.len() + 1 + name.len()
        && schema_name.as_bytes()[qualifier.len()] == b'.'
        && schema_name[..qualifier.len()] == *qualifier
        && schema_name[qualifier.len() + 1..] == *name
}

/// v7.30 (perf campaign) - position-only resolution for bind-once
/// fast paths (aggregate row loop). Same lookup order as
/// resolve_column's happy paths: composite "alias.col", then the
/// bare name.
pub(crate) fn find_column_pos(c: &ColumnName, ctx: &EvalContext<'_>) -> Option<usize> {
    if let Some(q) = &c.qualifier {
        if let Some(pos) = ctx
            .columns
            .iter()
            .position(|s| composite_eq(&s.name, q, &c.name))
        {
            return Some(pos);
        }
    }
    ctx.columns.iter().position(|s| s.name == c.name)
}

fn resolve_column_borrowed<'r>(
    c: &ColumnName,
    row: &'r Row,
    ctx: &EvalContext<'_>,
) -> Result<Option<&'r Value>, EvalError> {
    if let Some(q) = &c.qualifier {
        if let Some(pos) = ctx
            .columns
            .iter()
            .position(|s| composite_eq(&s.name, q, &c.name))
        {
            return Ok(row.values.get(pos));
        }
    }
    if let Some(pos) = ctx.columns.iter().position(|s| s.name == c.name) {
        return Ok(row.values.get(pos));
    }
    Ok(None)
}

/// First `n` CHARACTERS of `t` (PG LEFT semantics; negative n means
/// all but the last |n|), cloning only the prefix bytes.
fn text_prefix_chars(t: &str, n: i64) -> String {
    if n >= 0 {
        let n = usize::try_from(n).unwrap_or(usize::MAX);
        match t.char_indices().nth(n) {
            Some((byte_idx, _)) => t[..byte_idx].into(),
            None => t.into(),
        }
    } else {
        let drop_tail = usize::try_from(-n).unwrap_or(usize::MAX);
        let total = t.chars().count();
        let keep = total.saturating_sub(drop_tail);
        match t.char_indices().nth(keep) {
            Some((byte_idx, _)) => t[..byte_idx].into(),
            None => t.into(),
        }
    }
}

fn resolve_column(c: &ColumnName, row: &Row, ctx: &EvalContext<'_>) -> Result<Value, EvalError> {
    if let Some(q) = &c.qualifier {
        // Multi-table evaluation (joins): the synthesised schema uses
        // composite column names "alias.column" so we look that up
        // directly. Falls back to the single-table case below if the
        // composite isn't present.
        // v7.30 - zero-alloc composite match (was a String format
        // per column reference per row).
        if let Some(pos) = ctx
            .columns
            .iter()
            .position(|s| composite_eq(&s.name, q, &c.name))
        {
            return Ok(row.values[pos].clone());
        }
        // v7.26 (round-20 B) — when the qualifier IS a known table
        // alias in a joined schema (composite "alias.x" columns
        // exist) but THIS column isn't among them, the honest error
        // is "column does not exist", not "unknown table
        // qualifier". The misleading message sent mailrs hunting a
        // resolver bug when their fixture was missing a column.
        let prefix = alloc::format!("{q}.");
        if ctx.columns.iter().any(|sc| sc.name.starts_with(&prefix)) {
            return Err(EvalError::ColumnNotFound {
                name: alloc::format!("{q}.{name}", name = c.name),
            });
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
        (UnOp::BitNot, Value::SmallInt(n)) => Ok(Value::Int(!i32::from(n))),
        (UnOp::BitNot, Value::Int(n)) => Ok(Value::Int(!n)),
        (UnOp::BitNot, Value::BigInt(n)) => Ok(Value::BigInt(!n)),
        (UnOp::BitNot, other) => Err(EvalError::TypeMismatch {
            detail: format!("cannot apply ~ to {other:?}"),
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
        BinOp::BitOr => bitop(l, r, |a, b| a | b, "|"),
        BinOp::BitAnd => bitop(l, r, |a, b| a & b, "&"),
        BinOp::JsonGet => crate::json::path_get(&l, &r, false),
        BinOp::JsonGetText => crate::json::path_get(&l, &r, true),
        BinOp::JsonGetPath => crate::json::path_walk(&l, &r, false),
        BinOp::JsonGetPathText => crate::json::path_walk(&l, &r, true),
        BinOp::JsonContains => crate::json::contains(&l, &r),
        // v7.12.2 — `@@` match. NULL on either side → NULL; PG
        // accepts both orderings so we normalise.
        BinOp::TsMatch => ts_match(l, r),
        // v7.17.0 Phase 3.P0-47 — PG INET / CIDR containment + overlap.
        BinOp::InetContainedBy
        | BinOp::InetContainedByEq
        | BinOp::InetContains
        | BinOp::InetContainsEq
        | BinOp::InetOverlap => inet_op_bool_result(op, &l, &r),
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
pub(crate) fn apply_binary_interval(
    op: BinOp,
    l: &Value,
    r: &Value,
) -> Result<Option<Value>, EvalError> {
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
/// v7.24 (round-16 C) — `tsvector || tsvector`. PG semantics: the
/// right side's positions shift by the left side's max position;
/// lexemes present on both sides merge (positions concatenated,
/// the higher weight wins — SPG models weight per lexeme, PG per
/// position, so the stronger label is the faithful collapse).
fn text_concat(l: &Value, r: &Value) -> Value {
    if let (Value::TsVector(a), Value::TsVector(b)) = (l, r) {
        return tsvector_concat(a, b);
    }
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
/// Bitwise integer op (`|` / `&`). PG defines these for integer
/// types only — SmallInt widens to Int, Int x BigInt widens to
/// BigInt, anything else is a type error (mailrs embed round-12).
fn bitop(
    l: Value,
    r: Value,
    f: impl Fn(i64, i64) -> i64,
    op_name: &str,
) -> Result<Value, EvalError> {
    let widen = |v: Value| -> Value {
        match v {
            Value::SmallInt(n) => Value::Int(i32::from(n)),
            other => other,
        }
    };
    match (widen(l), widen(r)) {
        (Value::Int(a), Value::Int(b)) => {
            let result = f(i64::from(a), i64::from(b));
            // Two i32 inputs can't overflow i32 under | / &.
            Ok(Value::Int(result as i32))
        }
        (Value::Int(a), Value::BigInt(b)) | (Value::BigInt(b), Value::Int(a)) => {
            Ok(Value::BigInt(f(i64::from(a), b)))
        }
        (Value::BigInt(a), Value::BigInt(b)) => Ok(Value::BigInt(f(a, b))),
        (a, b) => Err(EvalError::TypeMismatch {
            detail: format!("cannot apply {op_name} to {a:?} and {b:?}"),
        }),
    }
}

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
        // v7.17.0 — UUID byte-wise comparison; both sides UUID.
        (Value::Uuid(a), Value::Uuid(b)) => a.cmp(b),
        // v7.17.0 — PG promotes a `text` literal compared against a
        // `uuid` column into uuid (unknown-type literal inference).
        // Without this, `WHERE id = '550e...'` falls through to the
        // generic TypeMismatch — the application's literal becomes
        // an error rather than a comparison.
        (Value::Uuid(a), Value::Text(b)) => {
            let bu = spg_storage::parse_uuid_str(b).ok_or_else(|| EvalError::TypeMismatch {
                detail: format!("invalid input syntax for type uuid: {b:?}"),
            })?;
            a.cmp(&bu)
        }
        (Value::Text(a), Value::Uuid(b)) => {
            let au = spg_storage::parse_uuid_str(a).ok_or_else(|| EvalError::TypeMismatch {
                detail: format!("invalid input syntax for type uuid: {a:?}"),
            })?;
            au.cmp(b)
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
        | BinOp::BitOr
        | BinOp::BitAnd
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
        | BinOp::IsNotDistinctFrom
        | BinOp::InetContainedBy
        | BinOp::InetContainedByEq
        | BinOp::InetContains
        | BinOp::InetContainsEq
        | BinOp::InetOverlap => {
            unreachable!("compare() only called with comparison ops")
        }
    };
    Ok(Value::Bool(result))
}

// SQL three-valued AND / OR.
pub(crate) fn and_3vl(l: Value, r: Value) -> Result<Value, EvalError> {
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

    /// v7.32 (P4 borrow channel) differential: the borrowed comparison
    /// fast path in `eval_expr`'s Binary arm must be byte-for-byte the
    /// pre-P4 owned path (`apply_binary` on cloned operands) across a
    /// cross-type value matrix and every comparison operator — covering
    /// the fast-path types (Text/Int/Float/Date/Timestamp/Bool/Null) and
    /// the owned-fallback types (Numeric/Interval).
    #[test]
    fn borrowed_compare_equals_owned_apply_binary() {
        let vals = vec![
            Value::Null,
            Value::Bool(true),
            Value::Bool(false),
            Value::SmallInt(3),
            Value::Int(3),
            Value::Int(-1),
            Value::BigInt(3),
            Value::BigInt(100),
            Value::Float(3.0),
            Value::Float(2.5),
            Value::Text(String::new()),
            Value::Text("a".into()),
            Value::Text("b".into()),
            Value::Date(10),
            Value::Timestamp(1000),
            Value::Numeric {
                scaled: 30,
                scale: 1,
            },
            Value::Interval {
                months: 0,
                micros: 5,
            },
        ];
        let ops = [
            BinOp::Eq,
            BinOp::NotEq,
            BinOp::Lt,
            BinOp::LtEq,
            BinOp::Gt,
            BinOp::GtEq,
        ];
        let cs = vec![col("x", DataType::Int), col("y", DataType::Int)];
        let c = ctx(&cs, None);
        let lhs = Expr::Column(ColumnName {
            qualifier: None,
            name: "x".into(),
        });
        let rhs = Expr::Column(ColumnName {
            qualifier: None,
            name: "y".into(),
        });
        for l in &vals {
            for r in &vals {
                let row = Row::new(vec![l.clone(), r.clone()]);
                for op in ops {
                    let got = eval_expr(
                        &Expr::Binary {
                            lhs: alloc::boxed::Box::new(lhs.clone()),
                            op,
                            rhs: alloc::boxed::Box::new(rhs.clone()),
                        },
                        &row,
                        &c,
                    );
                    // Pre-P4 reference: owned operands through apply_binary
                    // (collation fold is a no-op for non-CI columns).
                    let want = apply_binary(op, l.clone(), r.clone());
                    assert_eq!(
                        format!("{got:?}"),
                        format!("{want:?}"),
                        "op={op:?} l={l:?} r={r:?}"
                    );
                }
            }
        }
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
