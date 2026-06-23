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

use spg_sql::ast::{BinOp, ColumnName, Expr, Literal};
use spg_storage::{ColumnSchema, Row, Value};

mod binop;
mod cast;
pub mod compiled;
mod datetime;
mod encoding;
mod format;
mod functions;
mod inet;
mod math;
mod regexp;
mod resolve;
mod strings;
mod textsearch;
mod values;

pub use crate::conversions::format_money_array;
pub(crate) use binop::{and_3vl, apply_binary_interval, apply_binary_by_ref};
use binop::{apply_binary, apply_unary, compare, pow10_i128};
pub use cast::{cast_to_vector, cast_value, parse_vector_text};
pub(crate) use compiled::{
    CompiledExpr, compile_expr, eval_compiled, eval_compiled_ref, fully_compilable,
};
use datetime::{
    age, date_format_mysql, date_part, date_trunc, extract_field, from_unixtime, unix_timestamp_of,
};
use encoding::{decode_text, encode_text};
pub use format::{
    days_from_civil, format_bigint_array, format_bool_array, format_bytea_array, format_bytea_hex,
    format_date, format_date_array, format_float_array, format_int_array, format_interval,
    format_interval_array, format_money, format_numeric, format_numeric_array,
    format_smallint_array, format_text_array, format_time, format_timestamp,
    format_timestamp_array, format_timestamptz, format_timetz, format_uuid_array,
    parse_date_literal, parse_timestamp_literal,
};
use functions::apply_function;
use inet::{inet_host, inet_masklen, inet_network, inet_op_bool_result};
pub(crate) use math::{f64_ceil, f64_floor, f64_sqrt};
use math::{
    f64_exp, f64_ln, f64_powi, f64_round_half_away, f64_trunc, prng_next_f64, prng_next_u64,
};
use regexp::{regexp_matches, regexp_replace, regexp_split_to_array};
use resolve::{
    collation_fold_for_compare, compare_is_case_insensitive, composite_eq, eval_expr_cow,
    is_owned_compare_value, resolve_column, resolve_column_borrowed, text_prefix_chars,
};
pub(crate) use resolve::{column_collation, find_column_pos};
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
pub use values::gen_random_uuid_bytes;
use values::{value_cmp_for_min_max, value_to_f64, value_to_text, values_equal_for_nullif};

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
    pub params: &'a [Value<'static>],
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
    pub const fn with_params(mut self, params: &'a [Value<'static>]) -> Self {
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

pub fn eval_expr(
    expr: &Expr,
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
) -> Result<Value<'static>, EvalError> {
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
            cast_value(v, target.clone())
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
                                return Ok(Value::text(text_prefix_chars(t, n)));
                            }
                        }
                        _ => {}
                    }
                }
            }
            let evaluated: Result<Vec<Value<'static>>, _> =
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
            let mut materialised: Vec<Value<'static>> = Vec::with_capacity(items.len());
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
                        Value::Text(s) | Value::Json(s) => Some(s.into_owned()),
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
                    Some(Some(s)) => Ok(Value::text(s.clone())),
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
                Value::TextArray(items) => items.into_iter().map(|o| o.map(Value::text)).collect(),
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
        Value::Text(s) | Value::Json(s) => s.to_string(),
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

/// v7.24 (round-15) — `string_to_array(text, delimiter)`.
fn fn_string_to_array(args: &[Value<'static>]) -> Result<Value<'static>, EvalError> {
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
        Value::Text(d) if d.is_empty() => alloc::vec![Some(text.to_string())],
        Value::Text(d) => text
            .split(d.as_ref())
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
fn error_on_null(args: &[Value<'static>]) -> Result<Value<'static>, EvalError> {
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

/// Helper: coerce a Value to an Option<String> for regex args. NULL
/// propagates as None (caller short-circuits to Value::Null).
fn text_arg(v: &Value) -> Result<Option<String>, EvalError> {
    match v {
        Value::Text(s) => Ok(Some(s.to_string())),
        Value::Null => Ok(None),
        other => Err(EvalError::TypeMismatch {
            detail: alloc::format!(
                "regex function expects TEXT arg, got {:?}",
                other.data_type()
            ),
        }),
    }
}

// Month-name tables shared by the date formatters in `eval::strings`
// (`date_format_mysql`) and `eval::datetime` via `use super::`. Kept in
// `eval.rs` alongside `civil_from_days` so the calendar primitives live
// in one place.
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

/// Howard Hinnant's `civil_from_days` — converts days since the Unix
/// epoch back to a proleptic-Gregorian (year, month, day) triple. Stays
/// in `eval.rs` (shared with the date SQL functions here and with
/// `eval::strings`); the inverse `days_from_civil` lives in
/// `eval::format`. Both keep the engine off `std` time facilities.
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

pub(crate) fn literal_to_value(l: &Literal) -> Value<'static> {
    match l {
        Literal::Integer(n) => {
            if let Ok(small) = i32::try_from(*n) {
                Value::Int(small)
            } else {
                Value::BigInt(*n)
            }
        }
        Literal::Float(x) => Value::Float(*x),
        Literal::String(s) => Value::text(s.clone()),
        Literal::Vector(v) => Value::vector(v.clone()),
        Literal::TextArray(items) => Value::TextArray(items.clone()),
        Literal::IntArray(items) => Value::IntArray(items.clone()),
        Literal::BigIntArray(items) => Value::BigIntArray(items.clone()),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Null => Value::Null,
        Literal::Interval {
            months,
            days,
            micros,
            ..
        } => Value::Interval {
            months: *months,
            days: *days,
            micros: *micros,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use spg_sql::ast::UnOp;
    use spg_storage::{ColumnSchema, DataType, Row};

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
            Value::text(String::new()),
            Value::text("a"),
            Value::text("b"),
            Value::Date(10),
            Value::Timestamp(1000),
            Value::Numeric {
                scaled: 30,
                scale: 1,
            },
            Value::Interval {
                months: 0,
                days: 0,
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
        let r = Row::new(vec![Value::Int(7), Value::text("hi")]);
        let c = ctx(&cs, None);
        assert_eq!(eval_expr(&col_ref("a"), &r, &c).unwrap(), Value::Int(7));
        assert_eq!(eval_expr(&col_ref("b"), &r, &c).unwrap(), Value::text("hi"));
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
        // v7.37.5 β — three-arg signature. PG byte-equal:
        // `'1 day'` ≠ `'24 hours'` now, the format reflects it.
        assert_eq!(format_interval(0, 0, 0), "0");
        assert_eq!(format_interval(0, 1, 0), "1 day");
        assert_eq!(format_interval(0, -1, 0), "-1 days");
        assert_eq!(format_interval(0, 0, 86_400_000_000), "24:00:00");
        assert_eq!(format_interval(0, 0, 3_600_000_000), "01:00:00");
        assert_eq!(format_interval(0, 1, 9_000_000), "1 day 00:00:09");
        assert_eq!(format_interval(14, 0, 0), "1 year 2 mons");
        assert_eq!(format_interval(-1, 0, 0), "-1 mons");
    }

    #[test]
    fn interval_format_pg_byte_equal_day_vs_24h() {
        // v7.37.5 β — the PG-canonical distinction `'1 day'` ≠ `'24 hours'`
        // is preserved in the formatter, not just the parser.
        assert_eq!(format_interval(0, 1, 0), "1 day");
        assert_eq!(format_interval(0, 0, 86_400_000_000), "24:00:00");
        assert_ne!(
            format_interval(0, 1, 0),
            format_interval(0, 0, 86_400_000_000),
        );
    }
}
