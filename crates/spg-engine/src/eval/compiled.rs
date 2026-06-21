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
    Lit(Value<'static>),
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
    /// v7.36 (perf — mailrs Ask 1) — pure scalar function call
    /// (LENGTH, COALESCE, UPPER, etc.) on already-pushed args.
    /// Pops `n_args` values, calls `apply_function(name, args, ctx)`,
    /// pushes the result. Replaces the Subtree fallback for the
    /// "function over bound columns" shape that aggregate arg paths
    /// like `SUM(LENGTH(text_body))` and `MAX(COALESCE(col, ''))`
    /// otherwise force the row-materialise eval path. Only the
    /// `fully_compilable` whitelist (PURE scalars — no NOW / RANDOM
    /// / sequence accessors) is emitted; everything else stays on
    /// `Step::Subtree`.
    /// `name_lower` is pre-lowercased at compile time so the per-
    /// row dispatch in `apply_function` skips an allocation on
    /// every input row.
    Function {
        name_lower: alloc::string::String,
        n_args: usize,
    },
    /// v7.36 (perf — mailrs Ask 1 SUM(LENGTH(text_body)) zero-copy)
    /// — `LENGTH(<column>)` / `CHAR_LENGTH(<column>)` /
    /// `CHARACTER_LENGTH(<column>)` over a bound column. Reads the
    /// cell by reference, computes the char length WITHOUT cloning
    /// the underlying `String` — the 1 KB text bodies in
    /// `user_storage_usage` otherwise pay 25 k × 1 KB heap allocs
    /// per query just to push a `Value::Text` onto the stack so the
    /// next Step pops it and asks `s.len()`.
    ColumnLength {
        pos: usize,
    },
    /// v7.36 — `OCTET_LENGTH(<column>)` — byte count, regardless of
    /// encoding. Even simpler than `ColumnLength` (no ASCII probe).
    ColumnOctetLength {
        pos: usize,
    },
    /// v7.36 — `CAST(<expr> AS <ty>)` over an already-pushed value.
    /// Pure / context-free conversion goes through the same
    /// `cast_value` dispatcher the interpreter uses.
    Cast {
        target: spg_sql::ast::CastTarget,
    },
    /// v7.37.5-A2b — `CASE [operand] WHEN x THEN y … ELSE z END`.
    /// Each `(when, then)` branch and the optional `else` is a
    /// pre-compiled sub-program; the executor short-circuits on the
    /// first matching WHEN. Compiles only when **every** sub-program
    /// is itself `fully_compilable` (so the Case never falls back to
    /// a Subtree that would force a row materialise — profile-guided
    /// fix for Track A `COUNT(DISTINCT CASE WHEN ...)` aggregates).
    /// Searched form has `operand=None` and treats each WHEN as a
    /// Bool predicate; simple form has `operand=Some(prog)` and
    /// compares the operand value with each WHEN via `BinOp::Eq`.
    Case {
        operand: Option<CompiledExpr>,
        branches: alloc::vec::Vec<(CompiledExpr, CompiledExpr)>,
        else_branch: Option<CompiledExpr>,
    },
    /// Fallback: interpret this subtree with eval_expr.
    Subtree(Expr),
}

pub(crate) struct CompiledExpr {
    steps: Vec<Step>,
}

impl CompiledExpr {
    /// v7.36 (perf — mailrs Phase 1, user_storage_usage hot loop) —
    /// shape inspector for the aggregate's tight inner. Returns
    /// `Some(pos)` iff this compiled expression is exactly the
    /// single step `ColumnLength { pos }` — i.e. `LENGTH(<column>)`
    /// on a bound text column with no surrounding work.
    pub(crate) fn as_single_column_length(&self) -> Option<usize> {
        if self.steps.len() == 1
            && let Step::ColumnLength { pos } = &self.steps[0]
        {
            Some(*pos)
        } else {
            None
        }
    }
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
                // v7.36 (perf — mailrs Phase 1, get_contacts hot
                // inner) — trivial all-`%` pattern (`%`, `%%`, …)
                // matches every non-NULL text. Collapse the LIKE
                // into a `lhs IS NOT NULL` check: emit the operand
                // then `IsNull { negated: !*negated }`. For ILIKE
                // `%%` on 25 k rows the per-row `like_match_inner`
                // → 2-char walk (~30 ns each) becomes a tag check
                // (~3 ns); the operand still gets evaluated for the
                // NULL semantics that SQL `LIKE` requires.
                if !pat.is_empty() && pat.chars().all(|c| c == '%') {
                    compile_into(expr, ctx, steps);
                    steps.push(Step::IsNull { negated: !*negated });
                    return;
                }
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
        // v7.36 — PURE scalar function call: emit args then a
        // single Function step that pops them. `fully_compilable`
        // gates the whitelist + recurses into args, so this branch
        // only fires when the entire subtree is compilable.
        Expr::FunctionCall { name, args } if is_pure_scalar_function(name) => {
            // v7.36 — specialise `LENGTH(<column>)` /
            // `OCTET_LENGTH(<column>)` so the column's `Value::Text`
            // isn't cloned just to read its length. The general
            // `Step::Function` path goes through `apply_function`,
            // which can't borrow off the stack — it copies.
            let lower = name.to_ascii_lowercase();
            if args.len() == 1 {
                if let Expr::Column(c) = &args[0]
                    && let Some(pos) = compile_column_pos(c, ctx)
                {
                    match lower.as_str() {
                        "length" | "char_length" | "character_length" => {
                            steps.push(Step::ColumnLength { pos });
                            return;
                        }
                        "octet_length" => {
                            steps.push(Step::ColumnOctetLength { pos });
                            return;
                        }
                        _ => {}
                    }
                }
            }
            for a in args {
                compile_into(a, ctx, steps);
            }
            steps.push(Step::Function {
                name_lower: lower,
                n_args: args.len(),
            });
        }
        Expr::Cast { expr, target } => {
            compile_into(expr, ctx, steps);
            steps.push(Step::Cast {
                target: target.clone(),
            });
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            // Gate by `fully_compilable` at the leaf: if any sub-expr
            // can't compile natively, the whole Case stays Subtree so
            // a single Case never escapes to a row-materialise eval.
            let all_ok = operand.as_deref().is_none_or(fully_compilable)
                && branches
                    .iter()
                    .all(|(w, t)| fully_compilable(w) && fully_compilable(t))
                && else_branch.as_deref().is_none_or(fully_compilable);
            if !all_ok {
                steps.push(Step::Subtree(e.clone()));
                return;
            }
            let op_c = operand
                .as_deref()
                .map(|o| compile_expr(o, ctx));
            let branches_c: alloc::vec::Vec<(CompiledExpr, CompiledExpr)> = branches
                .iter()
                .map(|(w, t)| (compile_expr(w, ctx), compile_expr(t, ctx)))
                .collect();
            let else_c = else_branch
                .as_deref()
                .map(|el| compile_expr(el, ctx));
            steps.push(Step::Case {
                operand: op_c,
                branches: branches_c,
                else_branch: else_c,
            });
        }
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
        // v7.36 (perf — mailrs Ask 1) — PURE scalar functions over
        // compilable args go to `Step::Function`. The whitelist
        // covers the high-traffic / non-volatile cases; anything
        // outside (NOW, RANDOM, sequence accessors, EXTRACT-with-
        // context-dependent fields, etc.) stays on Subtree where
        // the interpreter has the full ctx.
        Expr::FunctionCall { name, args } => {
            is_pure_scalar_function(name) && args.iter().all(fully_compilable)
        }
        // v7.36 — CAST over a compilable expression. `cast_value`
        // is pure / context-free for the scalar targets we care
        // about (text, ints, floats, bool, dates).
        Expr::Cast { expr, .. } => fully_compilable(expr),
        // v7.37.5-A2b — `CASE [operand] WHEN x THEN y … ELSE z END`
        // when every sub-expression is itself fully-compilable. Hot
        // shape: Track A's 14 aggregates over
        // `COUNT(DISTINCT CASE WHEN m.message_id != '' THEN
        //                          m.message_id
        //                     ELSE CAST(m.id AS TEXT) END)` — without
        // this, every Case fell to `arg_compiled = None`, forced
        // `needs_mat = true` per-row, and triggered a full combined-
        // row `Vec<Value>` clone for the eval path.
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            operand.as_deref().is_none_or(fully_compilable)
                && branches
                    .iter()
                    .all(|(w, t)| fully_compilable(w) && fully_compilable(t))
                && else_branch.as_deref().is_none_or(fully_compilable)
        }
        _ => false,
    }
}

/// v7.36 — PURE scalar function whitelist for `Step::Function`.
/// "Pure" means: deterministic, context-independent, no side
/// effects. Aggregate names (sum / count / max / …) are filtered
/// upstream by the caller — they never reach the compiler. NOW /
/// RANDOM / sequence accessors are excluded because they need the
/// `EvalContext`'s clock / sequence resolver and aren't
/// deterministic. EXTRACT is excluded because the field kind is
/// parsed off the Expr tree, not an arg.
fn is_pure_scalar_function(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        // string length + slicing
        "length"
            | "char_length"
            | "character_length"
            | "octet_length"
            | "upper"
            | "lower"
            | "trim"
            | "ltrim"
            | "rtrim"
            | "btrim"
            | "left"
            | "right"
            | "substring"
            | "substr"
            | "replace"
            | "position"
            | "strpos"
            | "concat"
            | "concat_ws"
            | "reverse"
            | "repeat"
            | "lpad"
            | "rpad"
            | "split_part"
            // null/conditional
            | "coalesce"
            | "nullif"
            | "greatest"
            | "least"
            | "ifnull"
            | "isnull"
            | "nvl"
            // numeric
            | "abs"
            | "ceil"
            | "ceiling"
            | "floor"
            | "round"
            | "trunc"
            | "sqrt"
            | "power"
            | "pow"
            | "mod"
            | "sign"
            | "log"
            | "log10"
            | "exp"
            | "ln"
            // boolean / cast helpers
            | "cast"
    )
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
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
    stack: &mut Vec<Value<'static>>,
) -> Result<Value<'static>, EvalError> {
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
    stack: &mut Vec<Value<'static>>,
) -> Result<Value<'static>, EvalError> {
    stack.clear();
    run_compiled_steps(&c.steps, row, ctx, stack)?;
    Ok(stack.pop().unwrap_or(Value::Null))
}

/// v7.37.5-A2b — append-mode entry point for nested sub-programs (the
/// `Step::Case` executor's per-branch evaluations). Does NOT clear the
/// stack; pushes the program's result on top of whatever was already
/// there. Caller uses the `mark` to know where to truncate / pop. Kept
/// out of public surface — only the Case opcode reaches for it.
fn eval_compiled_ref_into(
    c: &CompiledExpr,
    row: &crate::join::RowRef<'_>,
    ctx: &EvalContext<'_>,
    stack: &mut Vec<Value<'static>>,
    _mark: usize,
) -> Result<(), EvalError> {
    run_compiled_steps(&c.steps, row, ctx, stack)
}

#[inline]
fn run_compiled_steps(
    steps: &[Step],
    row: &crate::join::RowRef<'_>,
    ctx: &EvalContext<'_>,
    stack: &mut Vec<Value<'static>>,
) -> Result<(), EvalError> {
    for step in steps {
        match step {
            Step::Column(pos) => {
                stack.push(
                    row.get(*pos)
                        .cloned()
                        .map(Value::into_owned)
                        .unwrap_or(Value::Null),
                );
            }
            Step::Lit(v) => stack.push(v.clone()),
            Step::Binary(op) => {
                let r = stack.pop().unwrap_or(Value::Null);
                let l = stack.pop().unwrap_or(Value::Null);
                stack.push(apply_binary(*op, l, r)?);
            }
            Step::BinaryCi(op) => {
                let fold = |v: Value<'static>| match v {
                    Value::Text(s) => Value::text(s.to_ascii_lowercase()),
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
                    (Value::Text(t), crate::memoize::InListSet::Text(s)) => s.contains(t.as_ref()),
                    // Cross-family needle: take the interpreter's
                    // exact coercion / error path on the whole node.
                    _ => {
                        stack.push(eval_expr(fallback, &row.as_row(), ctx)?);
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
            Step::ColumnLength { pos } => {
                // v7.36 — zero-copy LENGTH on a column. Read the
                // cell by reference; compute char count without
                // cloning the underlying `String`. Saves 25 k ×
                // ~1 KB heap clones on the user_storage_usage shape.
                let v = row.get(*pos).unwrap_or(&Value::Null);
                let pushed = match v {
                    Value::Null => Value::Null,
                    Value::Text(s) => {
                        let n = if s.is_ascii() {
                            i32::try_from(s.len()).unwrap_or(i32::MAX)
                        } else {
                            i32::try_from(s.chars().count()).unwrap_or(i32::MAX)
                        };
                        Value::Int(n)
                    }
                    Value::Bytes(b) => Value::Int(i32::try_from(b.len()).unwrap_or(i32::MAX)),
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: format!(
                                "length() needs text or bytea, got {:?}",
                                other.data_type()
                            ),
                        });
                    }
                };
                stack.push(pushed);
            }
            Step::ColumnOctetLength { pos } => {
                let v = row.get(*pos).unwrap_or(&Value::Null);
                let pushed = match v {
                    Value::Null => Value::Null,
                    Value::Text(s) => Value::Int(i32::try_from(s.len()).unwrap_or(i32::MAX)),
                    Value::Bytes(b) => Value::Int(i32::try_from(b.len()).unwrap_or(i32::MAX)),
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: format!(
                                "octet_length() needs text or bytea, got {:?}",
                                other.data_type()
                            ),
                        });
                    }
                };
                stack.push(pushed);
            }
            Step::Function { name_lower, n_args } => {
                let start = stack.len().saturating_sub(*n_args);
                // `apply_function` borrows the trailing `n_args`
                // values off the stack; we then truncate + push the
                // result. `name_lower` is pre-lowercased at compile
                // time, so dispatch skips the per-row
                // `to_ascii_lowercase()` allocation.
                let result =
                    super::functions::apply_function_lower(name_lower, &stack[start..], ctx)?;
                stack.truncate(start);
                stack.push(result);
            }
            Step::Cast { target } => {
                let v = stack.pop().unwrap_or(Value::Null);
                stack.push(super::cast::cast_value(v, target.clone())?);
            }
            Step::Case {
                operand,
                branches,
                else_branch,
            } => {
                // v7.37.5-A2b — short-circuit Case executor. Mirrors
                // `Expr::Case` interpreter semantics bit-for-bit (each
                // WHEN evaluates with its own scratch stack; first
                // match wins; ELSE = NULL when absent). The outer
                // `stack` is reused (truncated back to its pre-Case
                // mark after each sub-program); allocator-free per
                // branch — the prior version allocated a fresh
                // `Vec<Value>` per sub-program which showed up as
                // ~3 % `drop_in_place<Vec<Value>>` self time.
                let mark = stack.len();
                let operand_value = if let Some(op) = operand {
                    // Recursive call reuses the same stack: it
                    // `stack.clear()`s at entry so we hand it a fresh
                    // logical view; pop the sub-program's result
                    // afterwards and restore our prefix manually.
                    eval_compiled_ref_into(op, row, ctx, stack, mark)?;
                    Some(stack.pop().unwrap_or(Value::Null))
                } else {
                    None
                };
                stack.truncate(mark);
                let mut matched_value: Option<Value<'static>> = None;
                for (when_c, then_c) in branches {
                    eval_compiled_ref_into(when_c, row, ctx, stack, mark)?;
                    let when_v = stack.pop().unwrap_or(Value::Null);
                    stack.truncate(mark);
                    let matched = match &operand_value {
                        None => matches!(when_v, Value::Bool(true)),
                        Some(op_v) => matches!(
                            apply_binary(BinOp::Eq, op_v.clone(), when_v)?,
                            Value::Bool(true)
                        ),
                    };
                    if matched {
                        eval_compiled_ref_into(then_c, row, ctx, stack, mark)?;
                        matched_value = Some(stack.pop().unwrap_or(Value::Null));
                        stack.truncate(mark);
                        break;
                    }
                }
                let v = match matched_value {
                    Some(v) => v,
                    None => match else_branch {
                        Some(el) => {
                            eval_compiled_ref_into(el, row, ctx, stack, mark)?;
                            let v = stack.pop().unwrap_or(Value::Null);
                            stack.truncate(mark);
                            v
                        }
                        None => Value::Null,
                    },
                };
                stack.push(v);
            }
            Step::Subtree(e) => stack.push(eval_expr(e, &row.as_row(), ctx)?),
        }
    }
    Ok(())
}
