//! Compiled expressions — PG's ExprState idea (cut 30, extracted
//! from `eval.rs`; v7.32 perf knife D / architecture v2 P1).
//!
//! Walk the tree ONCE per query, pre-resolve column positions and
//! collation-fold decisions (both row-invariant), emit a flat
//! post-order step program; per-row evaluation is a linear loop —
//! no tree dispatch, no name resolution, no collation lookups.
//! Anything the compiler doesn't model becomes a `Step::Subtree`
//! that calls the interpreter for that node, so values AND error
//! behaviour stay bit-for-bit with `eval_expr` (invariant I3).

use alloc::format;
use alloc::vec::Vec;

use spg_sql::ast::{BinOp, ColumnName, Expr, Literal, UnOp};
use spg_storage::{Row, Value};

use super::{
    EvalContext, EvalError, apply_binary, apply_unary, column_collation, composite_eq, eval_expr,
    like_match_inner, literal_to_value,
};

pub(crate) enum Step {
    /// Pre-resolved column read (position into the row).
    Column(usize),
    /// Pre-converted literal.
    Lit(Value),
    /// Pops rhs then lhs, pushes the op result. Eager both-sides
    /// evaluation — same as the interpreter (no short-circuit).
    Binary(BinOp),
    /// Comparison whose operands referenced a CaseInsensitive
    /// column: ASCII-fold Text operands first (decided at compile
    /// time; the interpreter re-decides per row).
    BinaryCi(BinOp),
    Unary(UnOp),
    IsNull {
        negated: bool,
    },
    /// v7.32 (architecture v2, P1) — `needle [NOT] IN (literals…)`.
    /// The membership SET is a COMPILE PRODUCT, not a runtime cache:
    /// it lives in the step, so there is no "forgot to pass the
    /// memo" failure mode (the round-25 18.7 s accident is now
    /// unconstructable — see v7.32-executor-architecture-design.md
    /// invariant I2). The needle is the preceding sub-program; this
    /// step pops it. `fallback` is the whole InList node, used only
    /// when the runtime needle family doesn't match the set
    /// (e.g. Float needle vs Int set) — same escape the interpreter
    /// takes, evaluated cold.
    InSet {
        set: crate::memoize::InListSet,
        has_null: bool,
        negated: bool,
        fallback: Expr,
    },
    /// v7.32 (P1) — `text [NOT] [I]LIKE '<literal pattern>'`. The
    /// pattern (and its lowercased form for ILIKE) is compiled once;
    /// the step pops the text operand.
    Like {
        pattern: alloc::vec::Vec<char>,
        negated: bool,
        case_insensitive: bool,
    },
    /// Fallback: interpret this subtree with eval_expr.
    Subtree(Expr),
}

pub(crate) struct CompiledExpr {
    steps: Vec<Step>,
}

/// Column-position resolution at compile time. Mirrors the happy
/// layers of `resolve_column`; ANY case that would reach an error
/// path, an ambiguity, or a miss returns None so the node falls
/// back to the interpreter (identical runtime error / NULL
/// semantics).
fn compile_column_pos(c: &ColumnName, ctx: &EvalContext<'_>) -> Option<usize> {
    if let Some(q) = &c.qualifier {
        if let Some(pos) = ctx
            .columns
            .iter()
            .position(|s| composite_eq(&s.name, q, &c.name))
        {
            return Some(pos);
        }
        // resolve_column's error layers live behind this point:
        // composites under the qualifier exist (ColumnNotFound) or
        // the qualifier is unknown (UnknownQualifier) — interpret.
        let prefix_exists = ctx.columns.iter().any(|s| {
            s.name.starts_with(q.as_str()) && s.name.as_bytes().get(q.len()) == Some(&b'.')
        });
        if prefix_exists {
            return None;
        }
        match ctx.table_alias {
            // Alias-accepted single-table reference: fall through
            // to the bare layers (the inner-subquery hot shape).
            Some(a) if a == q => {}
            _ => return None,
        }
    }
    if let Some(pos) = ctx.columns.iter().position(|s| s.name == c.name) {
        return Some(pos);
    }
    let mut matches = ctx.columns.iter().enumerate().filter(|(_, s)| {
        s.name.len() > c.name.len()
            && s.name.ends_with(c.name.as_str())
            && s.name.as_bytes()[s.name.len() - c.name.len() - 1] == b'.'
    });
    let first = matches.next();
    if matches.next().is_some() {
        return None; // ambiguous — interpreter owns the error text
    }
    first.map(|(i, _)| i)
}

fn compile_into(e: &Expr, ctx: &EvalContext<'_>, steps: &mut Vec<Step>) {
    match e {
        Expr::Literal(l) => steps.push(Step::Lit(literal_to_value(l))),
        Expr::Column(c) => match compile_column_pos(c, ctx) {
            Some(pos) => steps.push(Step::Column(pos)),
            None => steps.push(Step::Subtree(e.clone())),
        },
        Expr::Binary { lhs, op, rhs } => {
            compile_into(lhs, ctx, steps);
            compile_into(rhs, ctx, steps);
            let cmp = matches!(
                op,
                BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq
            );
            let ci = cmp
                && (matches!(
                    column_collation(lhs, ctx),
                    Some(spg_storage::Collation::CaseInsensitive)
                ) || matches!(
                    column_collation(rhs, ctx),
                    Some(spg_storage::Collation::CaseInsensitive)
                ));
            steps.push(if ci {
                Step::BinaryCi(*op)
            } else {
                Step::Binary(*op)
            });
        }
        Expr::Unary { op, expr } => {
            compile_into(expr, ctx, steps);
            steps.push(Step::Unary(*op));
        }
        Expr::IsNull { expr, negated } => {
            compile_into(expr, ctx, steps);
            steps.push(Step::IsNull { negated: *negated });
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            // I2: the set is built at compile time. The gate
            // (`fully_compilable`) guarantees we only reach here
            // when the list builds a set and the needle compiles —
            // but keep the Subtree fallback for defence in depth.
            match crate::build_in_list_set(list) {
                Some(entry) if fully_compilable(expr) => {
                    compile_into(expr, ctx, steps);
                    steps.push(Step::InSet {
                        set: entry.set,
                        has_null: entry.has_null,
                        negated: *negated,
                        fallback: e.clone(),
                    });
                }
                _ => steps.push(Step::Subtree(e.clone())),
            }
        }
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => match literal_text_pattern(pattern) {
            Some(pat) if fully_compilable(expr) => {
                compile_into(expr, ctx, steps);
                let chars: alloc::vec::Vec<char> = if *case_insensitive {
                    pat.to_lowercase().chars().collect()
                } else {
                    pat.chars().collect()
                };
                steps.push(Step::Like {
                    pattern: chars,
                    negated: *negated,
                    case_insensitive: *case_insensitive,
                });
            }
            _ => steps.push(Step::Subtree(e.clone())),
        },
        other => steps.push(Step::Subtree(other.clone())),
    }
}

/// Literal text pattern behind a LIKE/ILIKE, if any.
fn literal_text_pattern(pattern: &Expr) -> Option<&str> {
    match pattern {
        Expr::Literal(Literal::String(s)) => Some(s.as_str()),
        _ => None,
    }
}

/// True when the whole tree consists of nodes the compiler models
/// natively. Mixed trees stay on the interpreted path: a Subtree
/// fallback would run WITHOUT the per-query MemoizeCache, and
/// memo-dependent nodes (InList set fast path — round-25) rebuild
/// per row there. Measured: compiling a search WHERE with an
/// InList subtree regressed 634 ms → 18.7 s.
pub(crate) fn fully_compilable(e: &Expr) -> bool {
    match e {
        Expr::Literal(_) | Expr::Column(_) => true,
        Expr::Binary { lhs, rhs, .. } => fully_compilable(lhs) && fully_compilable(rhs),
        Expr::Unary { expr, .. } | Expr::IsNull { expr, .. } => fully_compilable(expr),
        // I2: an InList is compilable ONLY when it becomes a real
        // InSet (all-literal list + compilable needle). A
        // non-set-able InList must keep the whole tree off the
        // compiled path so it never degrades to a memo-less,
        // O(list) per-row Subtree (the round-25 18.7 s trap).
        Expr::InList { expr, list, .. } => {
            fully_compilable(expr) && crate::build_in_list_set(list).is_some()
        }
        Expr::Like { expr, pattern, .. } => {
            fully_compilable(expr) && literal_text_pattern(pattern).is_some()
        }
        _ => false,
    }
}

pub(crate) fn compile_expr(e: &Expr, ctx: &EvalContext<'_>) -> CompiledExpr {
    let mut steps = Vec::new();
    compile_into(e, ctx, &mut steps);
    CompiledExpr { steps }
}

/// Run a compiled program. `stack` is caller-owned scratch
/// (cleared here) so tight row loops never touch the allocator
/// for the machine itself.
pub(crate) fn eval_compiled(
    c: &CompiledExpr,
    row: &Row,
    ctx: &EvalContext<'_>,
    stack: &mut Vec<Value>,
) -> Result<Value, EvalError> {
    eval_compiled_ref(c, &crate::join::RowRef::Owned(row), ctx, stack)
}

/// v7.32 (P4 borrow channel, increment 2) — the RowRef-borrowing form of
/// `eval_compiled`. `Step::Column` borrows its cell straight from the
/// RowRef (a join tuple resolves it via `tuple_value`, never
/// materialising a combined Row); only the rare Subtree / InSet
/// cross-family fallback materialises the row once. Bit-for-bit
/// equivalent to the Owned path — `eval_compiled` above is now a thin
/// `RowRef::Owned` wrapper, so there is a single interpreter (invariant
/// I3); a differential test pins the equivalence.
pub(crate) fn eval_compiled_ref(
    c: &CompiledExpr,
    row: &crate::join::RowRef<'_>,
    ctx: &EvalContext<'_>,
    stack: &mut Vec<Value>,
) -> Result<Value, EvalError> {
    stack.clear();
    for step in &c.steps {
        match step {
            Step::Column(pos) => {
                stack.push(row.get(*pos).cloned().unwrap_or(Value::Null));
            }
            Step::Lit(v) => stack.push(v.clone()),
            Step::Binary(op) => {
                let r = stack.pop().unwrap_or(Value::Null);
                let l = stack.pop().unwrap_or(Value::Null);
                stack.push(apply_binary(*op, l, r)?);
            }
            Step::BinaryCi(op) => {
                let fold = |v: Value| match v {
                    Value::Text(s) => Value::Text(s.to_ascii_lowercase()),
                    other => other,
                };
                let r = fold(stack.pop().unwrap_or(Value::Null));
                let l = fold(stack.pop().unwrap_or(Value::Null));
                stack.push(apply_binary(*op, l, r)?);
            }
            Step::Unary(op) => {
                let v = stack.pop().unwrap_or(Value::Null);
                stack.push(apply_unary(*op, v)?);
            }
            Step::IsNull { negated } => {
                let v = stack.pop().unwrap_or(Value::Null);
                let is_null = matches!(v, Value::Null);
                stack.push(Value::Bool(if *negated { !is_null } else { is_null }));
            }
            Step::InSet {
                set,
                has_null,
                negated,
                fallback,
            } => {
                let needle = stack.pop().unwrap_or(Value::Null);
                let contained = match (&needle, set) {
                    // Non-empty list + NULL needle → NULL (NOT NULL
                    // is still NULL) — matches the interpreter and
                    // eval_with_in_sets.
                    (Value::Null, _) => {
                        stack.push(Value::Null);
                        continue;
                    }
                    (Value::SmallInt(n), crate::memoize::InListSet::Int(s)) => {
                        s.contains(&i64::from(*n))
                    }
                    (Value::Int(n), crate::memoize::InListSet::Int(s)) => {
                        s.contains(&i64::from(*n))
                    }
                    (Value::BigInt(n), crate::memoize::InListSet::Int(s)) => s.contains(n),
                    (Value::Text(t), crate::memoize::InListSet::Text(s)) => s.contains(t.as_str()),
                    // Cross-family needle: take the interpreter's
                    // exact coercion / error path on the whole node.
                    _ => {
                        stack.push(eval_expr(fallback, &*row.as_row(), ctx)?);
                        continue;
                    }
                };
                let inner = if contained {
                    Value::Bool(true)
                } else if *has_null {
                    Value::Null
                } else {
                    Value::Bool(false)
                };
                stack.push(match (negated, inner) {
                    (true, Value::Bool(b)) => Value::Bool(!b),
                    (_, v) => v,
                });
            }
            Step::Like {
                pattern,
                negated,
                case_insensitive,
            } => {
                let v = stack.pop().unwrap_or(Value::Null);
                match v {
                    Value::Null => stack.push(Value::Null),
                    Value::Text(t) => {
                        let text: Vec<char> = if *case_insensitive {
                            t.to_lowercase().chars().collect()
                        } else {
                            t.chars().collect()
                        };
                        let m = like_match_inner(&text, 0, pattern, 0);
                        stack.push(Value::Bool(if *negated { !m } else { m }));
                    }
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: format!(
                                "LIKE requires text operands, got {:?}",
                                other.data_type()
                            ),
                        });
                    }
                }
            }
            Step::Subtree(e) => stack.push(eval_expr(e, &*row.as_row(), ctx)?),
        }
    }
    Ok(stack.pop().unwrap_or(Value::Null))
}
