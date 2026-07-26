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
    like_match_str, literal_to_value,
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
    /// v7.39 (perf — like_filter tied 1.04×) — unanchored substring
    /// LIKE: `%[k×_]literal[m×_]%`. Instead of the generic matcher's
    /// try-every-suffix backtracking (per-position `_`+literal walk),
    /// scan with `str::find` (two-way, sublinear) over the literal and
    /// verify the `k` leading / `m` trailing wildcard chars have room.
    LikeSubstring {
        needle: alloc::string::String,
        k_before: usize,
        m_after: usize,
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
    /// v7.38 (read01) — widen the top-of-stack value to a statically
    /// resolved PG common type (e.g. a `CASE` whose branches mix integer and
    /// numeric resolves to numeric). Resolved once at compile time from the
    /// branch expressions' types, so the per-row cost is a single
    /// scale-preserving coercion, not a describe. See
    /// [`crate::eval::widen_value_to`].
    CoerceCommon(spg_storage::DataType),
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
///
/// v7.37.16 — pub(crate): the aggregate bind-once fast path
/// (aggregate.rs `col_pos`) uses this as its resolver so bare-name
/// group/arg columns bind exactly like compiled-WHERE columns do.
pub(crate) fn compile_column_pos(c: &ColumnName, ctx: &EvalContext<'_>) -> Option<usize> {
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
            // v7.39 (read01 round 56) — a COMPOSITE column must not compile to
            // a raw `Step::Column`: that loads the stored JSON straight off the
            // row and skips the rehydration into `Value::Composite` that
            // `resolve_column` does. `p = ROW(2,'b')::pt` in a WHERE then
            // compared Json against Composite and errored, while the same
            // predicate in a projection worked. Route it through eval instead.
            // The check is COMPILE-time, so the hot column path pays nothing.
            Some(pos)
                if ctx
                    .columns
                    .get(pos)
                    .is_some_and(|sc| sc.user_composite_type.is_some()) =>
            {
                steps.push(Step::Subtree(e.clone()));
            }
            Some(pos) => steps.push(Step::Column(pos)),
            None => steps.push(Step::Subtree(e.clone())),
        },
        Expr::Binary { lhs, op, rhs } => {
            // v7.39 (round 383) — the MySQL bitwise operators are UNSIGNED
            // 64-bit (`~ & | ^ << >>`); the VM's Step::Binary calls the
            // dialect-blind apply_binary, so route them to the interpreter,
            // which has the dialect (eval.rs `mysql_bitwise`). `<< >>` share
            // the inet-containment BinOps — the interpreter still keeps the
            // inet meaning for non-numeric operands.
            if ctx.mysql_dialect
                && matches!(
                    op,
                    BinOp::BitAnd
                        | BinOp::BitOr
                        | BinOp::BitXor
                        | BinOp::InetContainedBy
                        | BinOp::InetContains
                )
            {
                steps.push(Step::Subtree(e.clone()));
                return;
            }
            // v7.39 (round 407) — MySQL's logical `XOR` reads both sides as
            // truth values, which the VM's dialect-blind apply_binary (no
            // LogicalXor arm) cannot do. Route to the interpreter, whose
            // eval_expr arm handles the connective (eval.rs
            // `eval_mysql_connective`).
            if ctx.mysql_dialect && matches!(op, BinOp::LogicalXor) {
                steps.push(Step::Subtree(e.clone()));
                return;
            }
            // v7.39 (round 402) — an arithmetic op on a SET / inline-ENUM
            // column reads the column numerically (bitmask / 1-based
            // ordinal), which the VM's value-level Add cannot see (it has the
            // text). Route to the interpreter, which folds it (eval.rs
            // resolve `collation_fold_for_compare`).
            if ctx.mysql_dialect
                && matches!(
                    op,
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod
                )
                && (crate::eval::expr_set_variants(lhs, ctx.columns).is_some()
                    || crate::eval::expr_set_variants(rhs, ctx.columns).is_some()
                    || crate::eval::expr_inline_enum_variants(lhs, ctx.columns).is_some()
                    || crate::eval::expr_inline_enum_variants(rhs, ctx.columns).is_some())
            {
                steps.push(Step::Subtree(e.clone()));
                return;
            }
            let cmp = matches!(
                op,
                BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq
            );
            // v7.39 (enum order knife) — an enum-witnessed comparison must
            // order by catalog member order; the VM's value-level compare
            // cannot. Fall back to the tree evaluator for this subtree
            // (compile-time check, zero cost when the catalog has no
            // enum types).
            if cmp
                && ctx.catalog.is_some_and(|cat| !cat.enum_types().is_empty())
                && (crate::eval::expr_enum_labels(lhs, ctx.columns, ctx.catalog).is_some()
                    || crate::eval::expr_enum_labels(rhs, ctx.columns, ctx.catalog).is_some())
            {
                steps.push(Step::Subtree(e.clone()));
                return;
            }
            compile_into(lhs, ctx, steps);
            compile_into(rhs, ctx, steps);
            let ci = cmp
                && (matches!(
                    column_collation(lhs, ctx),
                    Some(spg_storage::Collation::CaseInsensitive)
                ) || matches!(
                    column_collation(rhs, ctx),
                    Some(spg_storage::Collation::CaseInsensitive)
                ));
            // v7.39 (round 364, M4 P2) — a MySQL session folds every text
            // comparison, so it needs the CI step too (the step chooses
            // the accent-aware fold at run time).
            let ci = ci || (cmp && super::resolve::mysql_text_fold_applies(lhs, rhs, ctx));
            steps.push(if ci {
                Step::BinaryCi(*op)
            } else {
                Step::Binary(*op)
            });
        }
        Expr::Unary { op, expr } => {
            // v7.39 (round 383) — MySQL `~x` is the UNSIGNED 64-bit
            // complement; route to the interpreter (eval.rs `mysql_bit_not`)
            // since Step::Unary calls the dialect-blind apply_unary.
            if ctx.mysql_dialect && matches!(op, UnOp::BitNot) {
                steps.push(Step::Subtree(e.clone()));
                return;
            }
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
            // v7.39 (round 364, M4 P2) — a MySQL session folds text before
            // the membership test; the set-based compiled path compares
            // raw. Route it to the interpreter, which folds (eval.rs
            // `eval_in_list_arm`). The perf-critical InSet path is PG-only.
            if ctx.mysql_dialect {
                steps.push(Step::Subtree(e.clone()));
                return;
            }
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
        } => {
            // v7.39 (round 364, M4 P2) — LIKE folds accents + case on a
            // MySQL session (eval.rs `eval_like_arm`); the compiled
            // pattern walk does not. Route to the interpreter.
            if ctx.mysql_dialect {
                steps.push(Step::Subtree(e.clone()));
                return;
            }
            match literal_text_pattern(pattern) {
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
                // v7.39 — `%[k×_]lit[m×_]%` runs on the substring fast
                // path (see Step::LikeSubstring).
                if let Some((k, needle, m)) = like_substring_shape(&chars) {
                    steps.push(Step::LikeSubstring {
                        needle,
                        k_before: k,
                        m_after: m,
                        negated: *negated,
                        case_insensitive: *case_insensitive,
                    });
                    return;
                }
                steps.push(Step::Like {
                    pattern: chars,
                    negated: *negated,
                    case_insensitive: *case_insensitive,
                });
            }
            _ => steps.push(Step::Subtree(e.clone())),
            }
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
            // v7.39 (read01 ruleutils.c) — catalog-dependent casts run
            // through eval's pre-hook (regclass dual-shape, domain/enum/
            // composite named types).
            if matches!(
                target,
                spg_sql::ast::CastTarget::RegClass | spg_sql::ast::CastTarget::Named(_)
            ) {
                steps.push(Step::Subtree(e.clone()));
                return;
            }
            // v7.39 (read01 round 76) — `<timestamptz>::text` renders the
            // `+00` offset, and tz-ness lives in the *static* type, not in
            // the runtime `Value::Timestamp`. `Step::Cast` calls the pure
            // `cast_value(value, target)`, which cannot see the expression
            // it came from — so a cast the interpreter renders with an
            // offset came out without one whenever the compiled VM drove
            // it (every cast inside an aggregate argument, and every cast
            // over an aggregate result: `string_agg(x::text, ',')`,
            // `min(x)::text`). Keep this one shape on Subtree.
            if matches!(target, spg_sql::ast::CastTarget::Text)
                && crate::describe::describe_expr(expr, ctx.columns)
                    .is_some_and(|s| matches!(s.ty, spg_storage::DataType::Timestamptz))
            {
                steps.push(Step::Subtree(e.clone()));
                return;
            }
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
            let op_c = operand.as_deref().map(|o| compile_expr(o, ctx));
            let branches_c: alloc::vec::Vec<(CompiledExpr, CompiledExpr)> = branches
                .iter()
                .map(|(w, t)| (compile_expr(w, ctx), compile_expr(t, ctx)))
                .collect();
            let else_c = else_branch.as_deref().map(|el| compile_expr(el, ctx));
            steps.push(Step::Case {
                operand: op_c,
                branches: branches_c,
                else_branch: else_c,
            });
            // v7.38 (read01) — resolve the CASE result to PG's common type of
            // every THEN/ELSE branch once, here, and append a scale-preserving
            // coercion so a taken integer branch is widened to numeric (and
            // `pg_typeof` / downstream division match PG). Costs nothing when
            // the branches already share a type (common_type → None).
            let branch_types: Vec<spg_storage::DataType> = branches
                .iter()
                .map(|(_, t)| t)
                .chain(else_branch.iter().map(|b| b.as_ref()))
                .filter_map(|e| crate::describe::describe_expr(e, ctx.columns).map(|s| s.ty))
                .collect();
            if let Some(common) = crate::describe::common_type(&branch_types) {
                steps.push(Step::CoerceCommon(common));
            }
        }
        other => steps.push(Step::Subtree(other.clone())),
    }
}

/// Literal text pattern behind a LIKE/ILIKE, if any.
/// v7.39 — recognise `%[k×_]literal[m×_]%` (any number of leading /
/// trailing `%`; literal free of `%` / `_` / `\`). Returns
/// `(k, literal, m)` when the pattern fits the substring fast path.
fn like_substring_shape(pat: &[char]) -> Option<(usize, alloc::string::String, usize)> {
    let mut lo = 0;
    while lo < pat.len() && pat[lo] == '%' {
        lo += 1;
    }
    if lo == 0 {
        return None; // not %-anchored at the front
    }
    let mut hi = pat.len();
    while hi > lo && pat[hi - 1] == '%' {
        hi -= 1;
    }
    if hi == pat.len() {
        return None; // not %-anchored at the back
    }
    let inner = &pat[lo..hi];
    let mut i = 0;
    while i < inner.len() && inner[i] == '_' {
        i += 1;
    }
    let mut j = inner.len();
    while j > i && inner[j - 1] == '_' {
        j -= 1;
    }
    let lit = &inner[i..j];
    if lit.is_empty() || lit.iter().any(|&c| c == '%' || c == '_' || c == '\\') {
        return None;
    }
    Some((i, lit.iter().collect(), inner.len() - j))
}

/// v7.39 — `%[k×_]needle[m×_]%` matcher: walk `str::find` hits of the
/// literal and accept one with ≥k chars before it and ≥m chars after.
fn like_substring_match(hay: &str, needle: &str, k: usize, m: usize) -> bool {
    let mut start = 0;
    while let Some(rel) = hay[start..].find(needle) {
        let off = start + rel;
        let before_ok = k == 0 || hay[..off].chars().take(k).count() == k;
        let after_ok = m == 0 || hay[off + needle.len()..].chars().take(m).count() == m;
        if before_ok && after_ok {
            return true;
        }
        // Advance one char past this hit's start and retry.
        match hay[off..].chars().next() {
            Some(c) => start = off + c.len_utf8(),
            None => return false,
        }
    }
    false
}

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
        // v7.39 (read01 ruleutils.c) — regclass / user-named casts
        // need the catalog (dual-shape resolve, domain/enum/composite
        // hooks); they stay Subtree so eval's pre-hook runs.
        Expr::Cast { expr, target } => {
            !matches!(
                target,
                spg_sql::ast::CastTarget::RegClass | spg_sql::ast::CastTarget::Named(_)
            ) && fully_compilable(expr)
        }
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
    // v7.37.16 — reuse the caller's stack allocation across rows.
    // v7.37.9 T3 S2 had severed this: `eval_compiled_ref` pushes
    // `Value<'val>` where `'val` is the per-call RowRef borrow, and
    // `Vec<Value<'val>>` is invariant in `'val`, so the caller's
    // `Vec<Value<'static>>` could not be lent in-place and every call
    // allocated a fresh local Vec. That was sized for the ~50×/query
    // post-group projection path, but the aggregate/scan WHERE filter
    // loops (select.rs) call this once PER ROW — 50 k allocs/query on
    // a 50 k-row filter (the heavy.rs filter_agg 1.5×-vs-PG18 loss).
    // Instead: MOVE the caller's Vec in (covariant shrink 'static →
    // 'val, safe), run, then hand the emptied allocation back via
    // `recycle_stack`. Zero per-row alloc; the borrowed-push (S2/S3)
    // zero-clone Text path is untouched.
    let rowref = crate::join::RowRef::Owned(row);
    let mut local_stack: Vec<Value<'_>> = core::mem::take(stack);
    let result = eval_compiled_ref(c, &rowref, ctx, &mut local_stack);
    let owned = result.map(Value::into_owned);
    *stack = recycle_stack(local_stack);
    owned
}

/// v7.39 (round 479) — evaluate a compiled WHERE and answer the bool,
/// without ever materialising an owned `Value`.
///
/// `eval_compiled` ends in `result.map(Value::into_owned)` because its
/// contract is to hand back a `Value<'static>`. A predicate does not want
/// a value at all — it wants one bool — and round 478's profile put
/// `Value::into_owned` at 5.8 % of self time and `drop_glue<Value>` at
/// 15.1 %, against 5.5 % for the comparison the predicate exists to
/// perform. The `into_owned` and the owned value's drop are both pure
/// overhead on this path.
///
/// Everything else is `eval_compiled`'s bridge unchanged: the caller's
/// stack is moved in (covariant shrink), run, and handed back emptied.
pub(crate) fn eval_compiled_pred(
    c: &CompiledExpr,
    row: &Row<'static>,
    ctx: &EvalContext<'_>,
    stack: &mut Vec<Value<'static>>,
    mysql: bool,
) -> Result<bool, EvalError> {
    let rowref = crate::join::RowRef::Owned(row);
    let mut local_stack: Vec<Value<'_>> = core::mem::take(stack);
    let verdict = eval_compiled_ref(c, &rowref, ctx, &mut local_stack)
        .and_then(|v| crate::eval::predicate_is_true(&v, "WHERE", mysql));
    *stack = recycle_stack(local_stack);
    verdict
}

/// Return an emptied stack's allocation with its value lifetime reset.
/// This is the standard "recycle" pattern (cf. the `recycle_vec` crate):
/// an EMPTY `Vec<Value<'a>>` holds no values, only a raw allocation, so
/// re-labelling its element lifetime cannot dangle.
#[allow(unsafe_code)] // empty-Vec lifetime relabel; isolated (see SAFETY).
fn recycle_stack(mut v: Vec<Value<'_>>) -> Vec<Value<'static>> {
    v.clear();
    debug_assert!(v.is_empty());
    // SAFETY: `v` is empty (cleared above) — there are no `Value<'_>`s
    // whose lifetime could be unsoundly extended; `Vec<Value<'a>>` and
    // `Vec<Value<'static>>` are the same type constructor differing only
    // in a lifetime parameter, so they have identical size/align/layout
    // (lifetimes are erased before layout is computed).
    unsafe { core::mem::transmute::<Vec<Value<'_>>, Vec<Value<'static>>>(v) }
}

/// v7.32 (P4 borrow channel, increment 2) — the RowRef-borrowing form of
/// `eval_compiled`. `Step::Column` borrows its cell straight from the
/// RowRef (a join tuple resolves it via `tuple_value`, never
/// materialising a combined Row); only the rare Subtree / InSet
/// cross-family fallback materialises the row once. Bit-for-bit
/// equivalent to the Owned path — `eval_compiled` above is now a thin
/// `RowRef::Owned` wrapper, so there is a single interpreter (invariant
/// I3); a differential test pins the equivalence.
// v7.37.9 T3 S1 — row-lifetime stack plumbing. Two lifetimes:
// `'row` = the RowRef's data lifetime; `'val` = stack value lifetime
// (must outlive function return). Constraint `'row: 'val` allows the
// step body to push `Value::Text(Cow::Borrowed(row_cell))` (S2+) while
// the caller's stack stays at whatever lifetime it declared (often
// `'static` for Vec<Value<'static>>). S1 keeps every step body forcing
// `.into_owned()` so behaviour is bit-identical; later stages
// (S2 Column, S3 Lit, S4 Binary, S6 Function, S7 Case) progressively
// switch to borrowed push to eliminate per-row String allocs.
pub(crate) fn eval_compiled_ref<'row, 'val>(
    c: &'val CompiledExpr,
    row: &'val crate::join::RowRef<'row>,
    ctx: &EvalContext<'_>,
    stack: &mut Vec<Value<'val>>,
) -> Result<Value<'val>, EvalError>
where
    'row: 'val,
{
    stack.clear();
    run_compiled_steps(&c.steps, row, ctx, stack)?;
    Ok(stack.pop().unwrap_or(Value::Null))
}

/// v7.37.5-A2b — append-mode entry point for nested sub-programs (the
/// `Step::Case` executor's per-branch evaluations). Does NOT clear the
/// stack; pushes the program's result on top of whatever was already
/// there. Caller uses the `mark` to know where to truncate / pop. Kept
/// out of public surface — only the Case opcode reaches for it.
fn eval_compiled_ref_into<'row, 'val>(
    c: &'val CompiledExpr,
    row: &'val crate::join::RowRef<'row>,
    ctx: &EvalContext<'_>,
    stack: &mut Vec<Value<'val>>,
    _mark: usize,
) -> Result<(), EvalError>
where
    'row: 'val,
{
    run_compiled_steps(&c.steps, row, ctx, stack)
}

#[inline]
fn run_compiled_steps<'row, 'val>(
    steps: &'val [Step],
    row: &'val crate::join::RowRef<'row>,
    ctx: &EvalContext<'_>,
    stack: &mut Vec<Value<'val>>,
) -> Result<(), EvalError>
where
    'row: 'val,
{
    // v7.37.9 Phase 1A-ext-2 T1 — counter per call into the Step VM
    // interpreter. Tells us "how many steps does the average compiled
    // arg run per row" → narrows the attack target (subtree CSE vs
    // column-ref-push vs multi-spec combine). Read-only.
    crate::bump_counter!(STEP_VM_CALL_COUNT);
    crate::bump_counter!(STEP_VM_STEPS_TOTAL, steps.len() as u64);
    for step in steps {
        match step {
            Step::Column(pos) => {
                crate::bump_counter!(STEP_VM_COLUMN_FIRE);
                // v7.37.9 T3 S2 — catalog rows hold `Cow::Owned(String)`
                // for Text-class variants (per `spg-storage/src/lib.rs:539`
                // — "Persistent / catalog Values use Value<'static> with
                // Cow::Owned(...)"). Plain `.clone()` would therefore
                // still trigger `String::clone()` per cell read. Instead
                // manually wrap the existing storage into a borrowed Cow
                // pointing at the same bytes — zero-alloc push.
                let cell: Value<'val> = match row.get(*pos) {
                    Some(spg_storage::Value::Text(s)) => {
                        spg_storage::Value::Text(alloc::borrow::Cow::Borrowed(s.as_ref()))
                    }
                    Some(spg_storage::Value::Bytes(b)) => {
                        spg_storage::Value::Bytes(alloc::borrow::Cow::Borrowed(b.as_ref()))
                    }
                    Some(spg_storage::Value::Json(s)) => {
                        spg_storage::Value::Json(alloc::borrow::Cow::Borrowed(s.as_ref()))
                    }
                    Some(spg_storage::Value::Vector(v)) => {
                        spg_storage::Value::Vector(alloc::borrow::Cow::Borrowed(v.as_ref()))
                    }
                    // Copy-light variants: clone is free (just enum copy).
                    Some(v) => v.clone(),
                    None => Value::Null,
                };
                // Classification counter unchanged (still counts cells
                // that WERE heap-bearing in the baseline).
                if matches!(
                    &cell,
                    spg_storage::Value::Text(_)
                        | spg_storage::Value::Bytes(_)
                        | spg_storage::Value::Json(_)
                        | spg_storage::Value::Vector(_)
                ) {
                    crate::bump_counter!(STEP_VM_COLUMN_HEAP_ALLOC);
                }
                stack.push(cell);
            }
            Step::Lit(v) => {
                crate::bump_counter!(STEP_VM_LIT_FIRE);
                if matches!(
                    v,
                    spg_storage::Value::Text(_)
                        | spg_storage::Value::Bytes(_)
                        | spg_storage::Value::Json(_)
                        | spg_storage::Value::Vector(_)
                ) {
                    crate::bump_counter!(STEP_VM_LIT_HEAP_ALLOC);
                }
                // v7.37.9 T3 S3 — borrow literal storage instead of
                // String::clone'ing it. Step variants own their
                // literal (`Value<'static>` enum payload), so we can
                // safely construct a `Cow::Borrowed(&'static …)` view.
                // Same pattern as S2's Column path.
                let pushed: Value<'val> = match v {
                    spg_storage::Value::Text(s) => {
                        spg_storage::Value::Text(alloc::borrow::Cow::Borrowed(s.as_ref()))
                    }
                    spg_storage::Value::Bytes(b) => {
                        spg_storage::Value::Bytes(alloc::borrow::Cow::Borrowed(b.as_ref()))
                    }
                    spg_storage::Value::Json(s) => {
                        spg_storage::Value::Json(alloc::borrow::Cow::Borrowed(s.as_ref()))
                    }
                    spg_storage::Value::Vector(vec) => {
                        spg_storage::Value::Vector(alloc::borrow::Cow::Borrowed(vec.as_ref()))
                    }
                    other => other.clone(),
                };
                stack.push(pushed);
            }
            Step::Binary(op) => {
                crate::bump_counter!(STEP_VM_BINARY_FIRE);
                // v7.37.9 T3 S4 — try the by-ref fast path first
                // (comparison + 3VL ops). For those, operand bytes are
                // read but never stored in the result; we avoid the
                // .into_owned() that would clone every Cow::Borrowed
                // Text/Bytes/Json/Vector pushed by S2/S3. For ops that
                // build owned results (arithmetic, concat, json get,
                // etc.) apply_binary_by_ref returns None and we fall
                // through to the owning path.
                // v7.39 (round 346, M1) — the MySQL reading of AND / OR
                // has to be here TOO: a compiled predicate never passes
                // through `eval_expr`'s arm, so `WHERE a AND 1` still
                // errored on a MySQL session while the interpreted form
                // answered. (The pin found this, not the reading.)
                if ctx.mysql_dialect && matches!(op, BinOp::And | BinOp::Or) {
                    let r = super::as_mysql_truth(stack.pop().unwrap_or(Value::Null).into_owned())?;
                    let l = super::as_mysql_truth(stack.pop().unwrap_or(Value::Null).into_owned())?;
                    stack.push(apply_binary(*op, l, r)?);
                    continue;
                }
                let n = stack.len();
                if n >= 2 {
                    if let Some(result) =
                        super::apply_binary_by_ref(*op, &stack[n - 2], &stack[n - 1])?
                    {
                        stack.truncate(n - 2);
                        stack.push(result);
                        continue;
                    }
                }
                let r = stack.pop().unwrap_or(Value::Null).into_owned();
                let l = stack.pop().unwrap_or(Value::Null).into_owned();
                stack.push(apply_binary(*op, l, r)?);
            }
            Step::BinaryCi(op) => {
                // v7.39 (round 364, M4 P2) — the MySQL session uses the
                // accent-aware fold; a PG `case_insensitive` column keeps
                // its ASCII-only contract.
                let fold = |v: Value<'static>| match v {
                    Value::Text(s) if ctx.mysql_dialect => {
                        Value::text(spg_storage::mysql_compare_fold(&s))
                    }
                    Value::Text(s) => Value::text(s.to_ascii_lowercase()),
                    other => other,
                };
                let r = fold(stack.pop().unwrap_or(Value::Null).into_owned());
                let l = fold(stack.pop().unwrap_or(Value::Null).into_owned());
                stack.push(apply_binary(*op, l, r)?);
            }
            Step::Unary(op) => {
                let v = stack.pop().unwrap_or(Value::Null).into_owned();
                if ctx.mysql_dialect
                    && matches!(op, UnOp::Not)
                    && !matches!(v, Value::Bool(_) | Value::Null)
                {
                    stack.push(Value::Bool(!super::predicate_is_true(&v, "NOT", true)?));
                    continue;
                }
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
                // v7.37.16 — borrow the popped operand and run the
                // zero-alloc &str matcher. The old body paid
                // `.into_owned()` (a String clone of the S2 borrowed
                // push) plus a `Vec<char>` collect PER ROW — ~90 ns/row
                // of allocator traffic on a LIKE table scan (heavy.rs
                // like_filter 2.8× loss vs PG18). ILIKE still lowercases
                // (Unicode fold needs an owned buffer); plain LIKE is
                // allocation-free.
                let v = stack.pop().unwrap_or(Value::Null);
                match v {
                    Value::Null => stack.push(Value::Null),
                    // v7.39 (bpchar epic) — LIKE matches bpchar on its
                    // PADDED stored form ('ab'::char(5) LIKE 'ab' is
                    // false, LIKE 'ab   ' is true), per PG's bpchar
                    // pattern operators.
                    Value::Text(t) | Value::BpChar(t) => {
                        let m = if *case_insensitive {
                            like_match_str(&t.to_lowercase(), pattern, 0)?
                        } else {
                            like_match_str(t.as_ref(), pattern, 0)?
                        };
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
            Step::LikeSubstring {
                needle,
                k_before,
                m_after,
                negated,
                case_insensitive,
            } => {
                // v7.39 — `%[k×_]lit[m×_]%`: two-way substring search
                // over the literal, then verify the wildcard chars fit.
                let v = stack.pop().unwrap_or(Value::Null);
                match v {
                    Value::Null => stack.push(Value::Null),
                    Value::Text(t) | Value::BpChar(t) => {
                        let lowered;
                        let hay: &str = if *case_insensitive {
                            lowered = t.to_lowercase();
                            &lowered
                        } else {
                            t.as_ref()
                        };
                        let m = like_substring_match(hay, needle, *k_before, *m_after);
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
                    // v7.39 (bpchar epic) — length(bpchar) counts with the
                    // trailing blanks stripped (length('ab'::char(5)) = 2).
                    Value::BpChar(s) => {
                        let t = s.trim_end_matches(' ');
                        let n = if t.is_ascii() {
                            i32::try_from(t.len()).unwrap_or(i32::MAX)
                        } else {
                            i32::try_from(t.chars().count()).unwrap_or(i32::MAX)
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
                    // v7.39 (bpchar epic) — octet_length(bpchar) counts the
                    // PADDED stored form.
                    Value::Text(s) | Value::BpChar(s) => {
                        Value::Int(i32::try_from(s.len()).unwrap_or(i32::MAX))
                    }
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
                crate::bump_counter!(STEP_VM_FUNCTION_FIRE);
                let start = stack.len().saturating_sub(*n_args);
                // `apply_function` borrows the trailing `n_args`
                // values off the stack; we then truncate + push the
                // result. `name_lower` is pre-lowercased at compile
                // time, so dispatch skips the per-row
                // `to_ascii_lowercase()` allocation.
                // v7.37.9 T3 S6 — apply_function_lower signature relaxed
                // to `&[Value<'_>]`; pass the borrowed stack slice
                // directly. Eliminates the Vec materialise + per-arg
                // String::clone that S1 introduced as a placeholder.
                let result =
                    super::functions::apply_function_lower(name_lower, &stack[start..], ctx)?;
                stack.truncate(start);
                stack.push(result);
            }
            Step::Cast { target } => {
                crate::bump_counter!(STEP_VM_CAST_FIRE);
                let v = stack.pop().unwrap_or(Value::Null).into_owned();
                stack.push(super::cast::cast_value(v, target.clone())?);
            }
            Step::Case {
                operand,
                branches,
                else_branch,
            } => {
                crate::bump_counter!(STEP_VM_CASE_FIRE);
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
                // v7.37.9 T3 S7 — Case sub-program lifetime threads
                // through naturally via S1's `'row: 'val`. Operand /
                // when / matched / else results are pushed by sub-progs
                // into our same stack; we pop them as `Value<'val>` and
                // keep them at that lifetime instead of forcing
                // into_owned. The simple-form operand match (Eq) uses
                // apply_binary_by_ref to avoid the operand clone +
                // pop-side into_owned the S1 placeholder was paying.
                let operand_value: Option<Value<'val>> = if let Some(op) = operand {
                    eval_compiled_ref_into(op, row, ctx, stack, mark)?;
                    Some(stack.pop().unwrap_or(Value::Null))
                } else {
                    None
                };
                stack.truncate(mark);
                let mut matched_value: Option<Value<'val>> = None;
                for (when_c, then_c) in branches {
                    eval_compiled_ref_into(when_c, row, ctx, stack, mark)?;
                    let when_v = stack.pop().unwrap_or(Value::Null);
                    stack.truncate(mark);
                    let matched = match &operand_value {
                        None => matches!(when_v, Value::Bool(true)),
                        Some(op_v) => {
                            // Try the by-ref comparison fast path; fall
                            // back to owning apply_binary only if the
                            // by-ref path returns None (non-comparison
                            // op, which Eq never is).
                            let eq_result =
                                match super::apply_binary_by_ref(BinOp::Eq, op_v, &when_v)? {
                                    Some(v) => v,
                                    None => apply_binary(
                                        BinOp::Eq,
                                        op_v.clone().into_owned(),
                                        when_v.clone().into_owned(),
                                    )?,
                                };
                            matches!(eq_result, Value::Bool(true))
                        }
                    };
                    if matched {
                        eval_compiled_ref_into(then_c, row, ctx, stack, mark)?;
                        matched_value = Some(stack.pop().unwrap_or(Value::Null));
                        stack.truncate(mark);
                        break;
                    }
                }
                let v: Value<'val> = match matched_value {
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
            Step::CoerceCommon(target) => {
                let v = stack.pop().unwrap_or(Value::Null).into_owned();
                stack.push(super::widen_value_to(v, *target));
            }
            Step::Subtree(e) => stack.push(eval_expr(e, &row.as_row(), ctx)?),
        }
    }
    Ok(())
}

/// v7.37.9 Phase 1A-ext-2 T1 — Step VM internal step-type counters.
/// Read-only diagnostic; gates no behaviour. Used by counter_dump.rs
/// to ground-truth subtree CSE / column-ref-push / multi-spec-combine
/// attack ROI estimates.
pub static STEP_VM_CALL_COUNT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static STEP_VM_STEPS_TOTAL: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static STEP_VM_COLUMN_FIRE: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static STEP_VM_LIT_FIRE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static STEP_VM_BINARY_FIRE: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static STEP_VM_FUNCTION_FIRE: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static STEP_VM_CAST_FIRE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static STEP_VM_CASE_FIRE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// v7.37.9 Round 3 — heap-alloc counters specifically for the T3
/// structural attack's ROI estimate. Step::Column / Step::Lit hits
/// pay a String alloc when the cell variant is heap-bearing
/// (Text/Bytes/Json/Vector). T3 stack-lifetime push-by-borrow
/// would eliminate these for the bulk of per-row work.
pub static STEP_VM_COLUMN_HEAP_ALLOC: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static STEP_VM_LIT_HEAP_ALLOC: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
mod like_substring_tests {
    use super::{like_substring_match, like_substring_shape};

    fn shape(p: &str) -> Option<(usize, alloc::string::String, usize)> {
        let chars: alloc::vec::Vec<char> = p.chars().collect();
        like_substring_shape(&chars)
    }

    #[test]
    fn shape_recognition() {
        assert_eq!(shape("%_05%"), Some((1, "05".into(), 0)));
        assert_eq!(shape("%abc%"), Some((0, "abc".into(), 0)));
        assert_eq!(shape("%ab_%"), Some((0, "ab".into(), 1)));
        assert_eq!(shape("%%x%%"), Some((0, "x".into(), 0)));
        assert_eq!(shape("%__a__%"), Some((2, "a".into(), 2)));
        // Not eligible: missing anchors, inner %, escapes, empty literal.
        assert_eq!(shape("ab%"), None);
        assert_eq!(shape("%ab"), None);
        assert_eq!(shape("%a%b%"), None);
        assert_eq!(shape("%___%"), None);
        assert_eq!(shape("%a\\%b%"), None);
        assert_eq!(shape("%"), None);
    }

    #[test]
    fn matcher_semantics() {
        // %_05% — needs one char before "05".
        assert!(like_substring_match("x05", "05", 1, 0));
        assert!(!like_substring_match("05", "05", 1, 0));
        assert!(like_substring_match("ab05cd", "05", 1, 0));
        // Overlapping / repeated hits: first hit fails the k-check,
        // a later one passes.
        assert!(like_substring_match("05x05", "05", 1, 0));
        // Trailing underscore needs one char after.
        assert!(like_substring_match("abz", "ab", 0, 1));
        assert!(!like_substring_match("ab", "ab", 0, 1));
        // Plain substring.
        assert!(like_substring_match("hello", "ell", 0, 0));
        assert!(!like_substring_match("hello", "xyz", 0, 0));
        // Multi-byte chars count as single wildcard chars.
        assert!(like_substring_match("é05", "05", 1, 0));
        assert!(!like_substring_match("é5", "05", 1, 0));
    }
}
