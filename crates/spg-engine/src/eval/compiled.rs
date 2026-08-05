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
    /// evaluation — same as the interpreter for every op EXCEPT the two
    /// boolean connectives, which take `Connective` below.
    Binary(BinOp),
    /// v7.39 (round 621) — COALESCE and NULLIF as steps on the borrowed
    /// stack, because they are control flow wearing a function's name.
    ///
    /// Through `Step::Function` each had to return `Value<'static>`, which
    /// forces a clone of a borrowed text argument; and the coalesce arm also
    /// built a `Vec<DataType>` EVERY row for the numeric widening that
    /// `COALESCE(1, 2.5)` needs. Measured: `count(coalesce(s,'z'))` at 3.00
    /// allocations a row, `count(nullif(s,'row1'))` at 2.00, their chain at
    /// 5.00 — all of it for values that end up borrowed from the row anyway.
    ///
    /// On the stack, the chosen argument is handed back AS IS. The widening
    /// survives by inspection: only when the non-null arguments carry MIXED
    /// numeric-family types does the step fall to the owned function arm,
    /// which still does what it always did — same answers, paid only by the
    /// mixed shapes that need it.
    Coalesce {
        n_args: usize,
    },
    NullIf,
    /// v7.39 (round 717) — GREATEST / LEAST. Through `Step::Function`
    /// every row re-ran `apply_function_lower`'s name dispatch, and
    /// "least" lives in the crowded five-letter probe chain — measured
    /// +6 ms over "greatest" on the same 500k scan REGARDLESS of which
    /// argument wins (the take-always and take-never shapes cost the
    /// same, so the branch was never the tax; the name was). Uniform
    /// same-type arguments compare in place off the stack; the mixed /
    /// coercing / xid / MySQL-NULL shapes fall to the function arm,
    /// which still does what it always did.
    Extremum {
        n_args: usize,
        max: bool,
    },
    /// v7.39 (round 621) — `AND` / `OR`, short-circuiting.
    ///
    /// The VM is a stack machine, so both operands were pushed before the
    /// `Binary` step could look at either: `WHERE x <> 0 AND 1/x > 0` divided
    /// by zero on exactly the rows the guard exists to exclude. The
    /// interpreter's arm was fixed first and this path still failed, which is
    /// the second time a connective has been fixed in one evaluator and not
    /// the other (round 346's MySQL reading was the first — its comment is
    /// three screens down).
    ///
    /// Rather than turn the hottest loop in the engine into an indexed one
    /// with jumps, the right operand is its OWN program, run only when the
    /// left does not decide. Nesting depth is the AND-nesting depth of the
    /// predicate.
    Connective {
        op: BinOp,
        rhs: Vec<Step>,
    },
    /// Comparison whose operands referenced a CaseInsensitive
    /// column: ASCII-fold Text operands first (decided at compile
    /// time; the interpreter re-decides per row).
    BinaryCi(BinOp),
    Unary(UnOp),
    IsNull {
        negated: bool,
    },
    /// v7.39 (round 488) — the verdict of an all-`%` LIKE pattern:
    /// matches every non-NULL operand, and is NULL for a NULL one.
    ///
    /// v7.36 collapsed this shape into `IsNull { negated: !negated }`,
    /// which answers a three-valued question two-valued. `NULL NOT LIKE
    /// '%'` came out TRUE where PG18 says NULL, so `WHERE s NOT LIKE '%'`
    /// SELECTED the NULL row (PG selects nothing), and `SELECT s LIKE '%'`
    /// printed `false` where PG prints NULL. Same collapse, three-valued.
    AnyTextMatch {
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
    /// v7.39 (round 594) — `text ~ '<literal pattern>'` and its `~*` /
    /// `regexp_like(...)` spellings. `regexp_like` parsed the pattern into a
    /// tree for EVERY row: 500k rows cost 350 ms against PG18's 34.5, the
    /// same 10x whichever way the match was spelled. The pattern is a
    /// compile product here, exactly as `Step::Like`'s is — PG solves the
    /// same problem with a cache; a compile product cannot be forgotten.
    /// v7.39 (round 597) — `<expr> <op> ANY/ALL (<constant array>)`. The
    /// array is a compile PRODUCT: it used to be rebuilt for every row, and
    /// `WHERE id = ANY (ARRAY[1..10])` cost 268 ms over 500k rows against
    /// PG18's 8.3 — 494 at twenty elements — where the equivalent
    /// `id IN (1..10)` took 2.3. A non-constant right-hand side keeps the
    /// interpreter, which has to rebuild it: there it really can differ.
    AnyAll {
        op: spg_sql::ast::BinOp,
        is_any: bool,
        arr: Value<'static>,
    },
    /// v7.39 (round 595) — `EXTRACT(<field> FROM <expr>)`. The field is a
    /// keyword, not a value, so it rides in the step; the source is the
    /// preceding sub-program and this pops it. `fallback` carries the whole
    /// node because the extraction's error wording names the source's
    /// declared type, which only the node knows.
    Extract {
        field: spg_sql::ast::ExtractField,
        fallback: Expr,
    },
    Regex {
        re: crate::eval::CompiledRe,
        /// The whole call, for an operand that is not text: the interpreter
        /// owns whatever coercion or error that is, and this step must not
        /// invent one. Same escape `Step::InSet` takes, evaluated cold.
        fallback: Expr,
    },
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
    /// v7.39 (round 722) — a NAMED cast whose name resolved at COMPILE
    /// time (`::NUMERIC`, `::REAL`, `numeric(10,2)` — the
    /// `plain_named_target` table). The blanket Named -> Subtree rule
    /// sent these to the interpreter — worse, it made the whole
    /// aggregate argument non-compilable, so `count(id::NUMERIC)` fell
    /// off the round-716 fused parallel lane entirely. The name rides
    /// along for error wording only.
    CastPlain {
        dt: spg_storage::DataType,
        name: alloc::string::String,
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
    /// Which fast predicate shape this program is — settled once, here,
    /// instead of re-derived per row. See [`PredShape`].
    pred_shape: PredShape,
}

/// v7.39 (round 486) — the shape of a compiled predicate, decided at
/// compile time.
///
/// Round 482 added a `<column> <cmp> <literal>` fast path and this round
/// added `<column> [NOT] IN (<literals>)`. Both were slice pattern-matches
/// run PER ROW, so a program that is neither paid for every probe in the
/// list: adding the second one cost `like_filter` — a shape with no `IN`
/// anywhere in it — 4.5 %, measured against the previous commit on the same
/// machine minutes apart. A program's shape does not change between its
/// rows, so it is settled once and the row loop reads one discriminant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PredShape {
    Other,
    ColumnCmpLit,
    ColumnInSet,
    ColumnLike,
}

impl CompiledExpr {
    /// v7.36 (perf — mailrs Phase 1, user_storage_usage hot loop) —
    /// shape inspector for the aggregate's tight inner. Returns
    /// `Some(pos)` iff this compiled expression is exactly the
    /// single step `ColumnLength { pos }` — i.e. `LENGTH(<column>)`
    /// on a bound text column with no surrounding work.
    /// v7.39 (round 482) — is this exactly `<column> <cmp> <literal>`?
    ///
    /// Rounds 478-481 traced the per-row predicate cost to `Value` churn:
    /// three steps a row (Column, Lit, Binary) means three `Value`s built
    /// and destroyed, and `drop_glue<Value>` is an out-of-line call that
    /// switches on the discriminant even when the value carries no heap.
    /// Round 481's counter ruled out leftovers on the stack — the churn is
    /// the VM's ordinary operands.
    ///
    /// This shape needs none of them: both operands can be read by
    /// reference. It covers `g = 5` and `s = '…'`; `LIKE` is its own AST
    /// node rather than a `BinOp`, so it compiles to a different step and
    /// is NOT covered here — measured, not assumed.
    ///
    /// `BinaryCi` is deliberately not matched: it folds its operands
    /// first, which is a different comparison. Nor is the mirrored
    /// `<literal> <cmp> <column>` — flipping the operator is a separate
    /// judgement and this returns None so it takes the general path.
    pub(crate) fn as_column_cmp_literal(&self) -> Option<(usize, BinOp, &Value<'static>)> {
        let [Step::Column(pos), Step::Lit(lit), Step::Binary(op)] = &self.steps[..] else {
            return None;
        };
        if !matches!(
            op,
            BinOp::Eq
                | BinOp::NotEq
                | BinOp::Lt
                | BinOp::LtEq
                | BinOp::Gt
                | BinOp::GtEq
        ) {
            return None;
        }
        Some((*pos, *op, lit))
    }

    /// v7.39 (round 486) — the sibling shape `<column> [NOT] IN (<literals>)`.
    ///
    /// `big_in` is the read panel's worst shape and compiles to exactly two
    /// steps, `Column` then `InSet`. The round-482 fast path does not cover
    /// it (three steps, a `Binary`), so it runs the general VM: a `Value`
    /// built from the cell, popped, and a `Value::Bool` built and popped
    /// again. Its profile put `drop_glue<Value>` at 20 % and the VM loop at
    /// 27 %. The set lookup itself wants nothing but a reference to the
    /// cell.
    pub(crate) fn as_column_in_set(
        &self,
    ) -> Option<(usize, &crate::memoize::InListSet, bool, bool)> {
        let [
            Step::Column(pos),
            Step::InSet {
                set,
                has_null,
                negated,
                ..
            },
        ] = &self.steps[..]
        else {
            return None;
        };
        Some((*pos, set, *has_null, *negated))
    }

    /// v7.39 (round 488) — the third two-step shape: `<column> [NOT]
    /// [I]LIKE '<literal>'`, in either the general matcher's form or the
    /// unanchored-substring form round 484 added.
    ///
    /// `like_filter` is the read panel's worst shape. Rounds 482 and 486
    /// covered its two siblings; this one still ran the general VM, which
    /// pushes the cell as a `Value` and pops it again for a matcher that
    /// only ever wanted a `&str`.
    pub(crate) fn as_column_like(&self) -> Option<(usize, &Step)> {
        let [
            Step::Column(pos),
            step @ (Step::Like { .. } | Step::LikeSubstring { .. }),
        ] = &self.steps[..]
        else {
            return None;
        };
        Some((*pos, step))
    }

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
/// v7.39 (round 693) — does this comparison operand carry a collation the
/// VM cannot perform?
///
/// Deliberately a COMPILE-time question. The answer is the same for every
/// row of the scan, and the alternative — asking per row inside `compare` —
/// puts a lookup on the hottest path in the engine.
/// v7.39 (round 704) — does this comparison pair an unknown string literal
/// with a numeric-family operand whose type the literal will not parse as?
/// Compile-time twin of the eval Binary arm's error rewrite; see the bail
/// site for why the shape cannot stay on the VM.
fn unparseable_numeric_literal_cmp(lhs: &Expr, rhs: &Expr, ctx: &EvalContext<'_>) -> bool {
    let check = |lit: &Expr, other: &Expr| -> bool {
        let Expr::Literal(spg_sql::ast::Literal::String(text)) = lit else {
            return false;
        };
        let Some(desc) = crate::describe::describe_expr(other, ctx.columns) else {
            return false;
        };
        if !matches!(
            desc.ty,
            spg_storage::DataType::SmallInt
                | spg_storage::DataType::Int
                | spg_storage::DataType::BigInt
                | spg_storage::DataType::Float
                | spg_storage::DataType::Real
                | spg_storage::DataType::Numeric { .. }
        ) {
            return false;
        }
        crate::conversions::coerce_value(
            spg_storage::Value::text(text.as_str()),
            desc.ty,
            "",
            0,
        )
        .is_err()
    };
    check(lhs, rhs) || check(rhs, lhs)
}

fn operand_declares_a_collation(e: &Expr, ctx: &EvalContext<'_>) -> bool {
    let derived = crate::collate_derive::derive(e, &|c: &ColumnName| {
        let pos = crate::eval::find_column_pos(c, ctx)?;
        ctx.columns.get(pos)?.collation_name.clone()
    });
    // A conflict has to leave the VM too — the tree evaluator is where the
    // error is raised, with PG's own sentence.
    derived.conflict().is_some()
        || derived
            .name()
            .is_some_and(|n| crate::collate::is_supported(n))
}

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

/// v7.39 (round 621) — can evaluating this raise at RUN time?
///
/// The errors a short circuit spares are the run-time ones: a division, an
/// overflow, a cast that will not parse, a function that refuses its input.
/// A type mismatch is not among them — PG raises those while ANALYSING, so it
/// raises them whether or not the operand would have been evaluated, and so
/// does SPG. That is why a predicate built only from columns, literals,
/// comparisons and the boolean shapes over them needs no short circuit: there
/// is nothing for it to spare.
///
/// Unrecognised shapes answer `true`, so a new kind of expression short
/// circuits (correct, slightly slower) rather than silently not.
fn can_raise_at_run_time(e: &Expr) -> bool {
    match e {
        Expr::Literal(_) | Expr::Column(_) => false,
        Expr::Binary { op, lhs, rhs } => {
            !matches!(
                op,
                BinOp::Eq
                    | BinOp::NotEq
                    | BinOp::Lt
                    | BinOp::LtEq
                    | BinOp::Gt
                    | BinOp::GtEq
                    | BinOp::And
                    | BinOp::Or
            ) || can_raise_at_run_time(lhs)
                || can_raise_at_run_time(rhs)
        }
        Expr::Unary { op, expr } => {
            !matches!(op, UnOp::Not) || can_raise_at_run_time(expr)
        }
        Expr::IsNull { expr, .. } | Expr::BoolTest { expr, .. } => can_raise_at_run_time(expr),
        Expr::Like {
            expr,
            pattern,
            ..
        } => can_raise_at_run_time(expr) || can_raise_at_run_time(pattern),
        Expr::InList { expr, list, .. } => {
            can_raise_at_run_time(expr) || list.iter().any(can_raise_at_run_time)
        }
        _ => true,
    }
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
            // v7.39 (round 621) — the boolean connectives short-circuit, so
            // the right operand compiles to its own program. The shapes whose
            // right operand is a literal go to the interpreter instead: those
            // carry PG's analysis-time half (a non-boolean literal is refused
            // even when the short circuit would not reach it, and an unknown
            // string literal is resolved), which is decided there and is not
            // worth a second implementation for how rare they are in a
            // compiled predicate.
            if matches!(op, BinOp::And | BinOp::Or) {
                if matches!(rhs.as_ref(), Expr::Literal(_)) {
                    steps.push(Step::Subtree(e.clone()));
                    return;
                }
                // A right operand that cannot fail has nothing to be spared,
                // so it keeps the eager step and its inline cost. `WHERE g
                // BETWEEN 10 AND 20` is `g >= 10 AND g <= 20`, the commonest
                // conjunctive predicate there is, and paying a nested program
                // per row for it measured +42% to +60% on the panel — a real
                // regression, reproduced, for a short circuit that can never
                // change an answer.
                if !can_raise_at_run_time(rhs) {
                    compile_into(lhs, ctx, steps);
                    compile_into(rhs, ctx, steps);
                    steps.push(Step::Binary(*op));
                    return;
                }
                compile_into(lhs, ctx, steps);
                let mut rhs_steps = Vec::new();
                compile_into(rhs, ctx, &mut rhs_steps);
                steps.push(Step::Connective {
                    op: *op,
                    rhs: rhs_steps,
                });
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
            // v7.39 (round 693) — and the same move for a declared
            // collation, which is the shape F36 had left: `loc BETWEEN 'a'
            // AND 'd'` returns a different ROW SET under en_US.utf8 than
            // under byte order.
            //
            // Compile-time, like its enum neighbour, and for the better of
            // the two reasons. `binop::compare` is the dominant cost of a
            // scan — its own comment measures 35.6 % of self time on
            // `g = 5` — so a per-row collation lookup there would have to
            // earn its place against a bench. Deciding once, while the
            // predicate compiles, costs the scan nothing at all: a column
            // that declares nothing never leaves the VM.
            //
            // Only the ORDERING operators. Measured on PG18, `=`, `<>`,
            // LIKE, IN and count(DISTINCT …) all give byte-equality's
            // answer under a deterministic collation.
            if matches!(op, BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq)
                && operand_declares_a_collation(lhs, ctx)
                | operand_declares_a_collation(rhs, ctx)
            {
                steps.push(Step::Subtree(e.clone()));
                return;
            }
            // v7.39 (round 704) — an UNKNOWN string literal against a
            // numeric-family operand that will NOT parse as its type. PG's
            // error for `WHERE i = 'abc'` is the input function's
            // (`invalid input syntax for type integer: "abc"`); the VM's
            // value-level compare can only say "operator does not exist",
            // so this shape leaves for the tree evaluator, whose Binary
            // arm has the Exprs and rewrites the error. Compile-time and
            // failure-only: a literal that parses stays on the VM path
            // and costs nothing.
            if cmp && unparseable_numeric_literal_cmp(lhs, rhs, ctx) {
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
                    steps.push(Step::AnyTextMatch { negated: *negated });
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
        // v7.39 (round 594) — a literal-pattern regex compiles here instead
        // of once per row. `s ~ 'p'` and `s ~* 'p'` both lower to
        // `regexp_like`, so this one shape covers the operators too. A
        // pattern that is not a literal (or flags that are not) stays on the
        // interpreter, which still has to compile per row: the pattern can
        // differ row to row.
        Expr::FunctionCall { name, args }
            if name.eq_ignore_ascii_case("regexp_like")
                && matches!(args.len(), 2 | 3)
                && regex_literal_parts(args.as_slice()).is_some()
                && fully_compilable(&args[0]) =>
        {
            let (pat, ci) = regex_literal_parts(args.as_slice()).expect("checked above");
            match crate::eval::compile_re(pat, ci) {
                Ok(re) => {
                    compile_into(&args[0], ctx, steps);
                    steps.push(Step::Regex {
                        re,
                        fallback: e.clone(),
                    });
                }
                // An invalid pattern is an error the interpreter words; let
                // it keep raising it, in its own wording.
                Err(_) => steps.push(Step::Subtree(e.clone())),
            }
        }
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
            // v7.39 (round 621) — COALESCE / NULLIF compile to their own
            // steps (see the variants) so the chosen argument stays borrowed.
            // The arguments are already on the stack from the loop above — a
            // first cut recompiled them here and doubled them.
            if name.eq_ignore_ascii_case("coalesce") && !args.is_empty() {
                steps.push(Step::Coalesce { n_args: args.len() });
                return;
            }
            if name.eq_ignore_ascii_case("nullif") && args.len() == 2 {
                steps.push(Step::NullIf);
                return;
            }
            // v7.39 (round 717) — GREATEST / LEAST get their own step;
            // see the variant.
            if (lower == "greatest" || lower == "least") && !args.is_empty() {
                steps.push(Step::Extremum {
                    n_args: args.len(),
                    max: lower == "greatest",
                });
                return;
            }
            steps.push(Step::Function {
                name_lower: lower,
                n_args: args.len(),
            });
        }
        // v7.39 (round 605) — a CONSTANT subexpression is evaluated once here
        // rather than for every row. `WHERE id < ('500')::INT` cost two
        // allocations a row against none for `WHERE id < 500`, and the same
        // gap is much wider in a projection. A literal is already a `Lit`
        // step, so this is only about the shapes built OUT of literals.
        //
        // An error stays where it was: if the fold does not evaluate, the
        // expression compiles as before and raises per row, in the
        // interpreter's own wording.
        e if !matches!(e, Expr::Literal(_)) && constant_expr(e) => {
            match eval_expr(e, &Row::new(alloc::vec::Vec::new()), ctx) {
                Ok(v) => steps.push(Step::Lit(v)),
                Err(_) => steps.push(Step::Subtree(e.clone())),
            }
        }
        // v7.39 (round 597) — `x = ANY (ARRAY[literals])` is `x IN (…)` and
        // `x <> ALL (…)` is `x NOT IN (…)`, down to the three-valued
        // treatment of a NULL element, so they take the membership set the
        // IN list already builds at compile time: 40.9 ms for a ten-element
        // array against 2.1 for the IN spelling of the same question. Folding
        // the array (below) alone left the per-row cost growing with the
        // array's length; a set does not.
        Expr::AnyAll {
            expr,
            op,
            array,
            is_any,
        } if !ctx.mysql_dialect
            && ((matches!(op, spg_sql::ast::BinOp::Eq) && *is_any)
                || (matches!(op, spg_sql::ast::BinOp::NotEq) && !*is_any))
            && array_literal_items(array).is_some_and(|it| {
                !it.is_empty() && crate::build_in_list_set(it).is_some()
            })
            && fully_compilable(expr) =>
        {
            let items = array_literal_items(array).expect("checked above");
            let entry = crate::build_in_list_set(items).expect("checked above");
            compile_into(expr, ctx, steps);
            steps.push(Step::InSet {
                set: entry.set,
                has_null: entry.has_null,
                negated: !*is_any,
                fallback: e.clone(),
            });
        }
        // v7.39 (round 597) — any other ANY/ALL whose right-hand array is
        // constant: build it once here rather than per row.
        Expr::AnyAll {
            expr,
            op,
            array,
            is_any,
        } if constant_expr(array) => {
            match eval_expr(array, &Row::new(alloc::vec::Vec::new()), ctx) {
                Ok(arr) => {
                    compile_into(expr, ctx, steps);
                    // v7.39 (round 604) — with the array in hand, an equality
                    // ANY / inequality ALL is a membership test whatever the
                    // spelling: `'{1,2,3}'::int[]` keeps its elements inside a
                    // string, so round 597's literal-list route could not see
                    // them, but they are values now.
                    if !ctx.mysql_dialect
                        && ((matches!(op, spg_sql::ast::BinOp::Eq) && *is_any)
                            || (matches!(op, spg_sql::ast::BinOp::NotEq) && !*is_any))
                        && let Some(entry) = value_array_in_list_set(&arr)
                    {
                        steps.push(Step::InSet {
                            set: entry.set,
                            has_null: entry.has_null,
                            negated: !*is_any,
                            fallback: e.clone(),
                        });
                        return;
                    }
                    steps.push(Step::AnyAll {
                        op: *op,
                        is_any: *is_any,
                        arr,
                    });
                }
                // A constant that does not evaluate is the interpreter's
                // error to raise, per row, in its own wording.
                Err(_) => steps.push(Step::Subtree(e.clone())),
            }
        }
        // v7.39 (round 595) — EXTRACT over a compilable source. One
        // non-compilable node used to disqualify the WHOLE predicate, so
        // `WHERE extract(year FROM t) = 2020` interpreted the column read
        // and the comparison as well: 81.7 ms on 500k rows against PG18's
        // 14.5, where a compiled comparison on the same column is 13.1.
        Expr::Extract { field, source } => {
            compile_into(source, ctx, steps);
            steps.push(Step::Extract {
                field: field.clone(),
                fallback: e.clone(),
            });
        }
        Expr::Cast { expr, target } => {
            // v7.39 (read01 ruleutils.c) — catalog-dependent casts run
            // through eval's pre-hook (regclass dual-shape, domain/enum/
            // composite named types).
            // v7.39 (round 621) — the varchar/char FAMILY is catalog-free, so
            // it stays on the compiled path; the blanket Named -> Subtree rule
            // sent `s::VARCHAR(20)` to the interpreter, which pays two
            // allocations a row. Everything else Named (domains, enums,
            // composites, regtypes) still needs the interpreter's catalog.
            let named_text_family = match target {
                spg_sql::ast::CastTarget::Named(n) => named_varchar_family(n),
                _ => false,
            };
            // v7.39 (round 722) — a plain scalar spelling resolves NOW, not
            // per row; see `Step::CastPlain`. The text family keeps its
            // dedicated route (the timestamptz::text Subtree guard below
            // must still see it).
            if let spg_sql::ast::CastTarget::Named(n) = target
                && !named_text_family
                && let Some(dt) = super::cast::plain_named_target(n)
            {
                compile_into(expr, ctx, steps);
                steps.push(Step::CastPlain {
                    dt,
                    name: n.clone(),
                });
                return;
            }
            if matches!(target, spg_sql::ast::CastTarget::RegClass)
                || (matches!(target, spg_sql::ast::CastTarget::Named(_)) && !named_text_family)
            {
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
            if (matches!(target, spg_sql::ast::CastTarget::Text) || named_text_family)
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
/// v7.39 (round 484) — find `needle` in `hay` at or after `start`.
///
/// `str::find(&str)` runs the two-way algorithm, and its SETUP is the cost:
/// round 484's profile of `s LIKE '%_05%'` put `StrSearcher::new` at 14.6 %
/// of self time — rebuilt for every row against a needle that is a compile
/// -time constant, and only two bytes long here.
///
/// An ASCII needle can be scanned as bytes instead: a UTF-8 continuation
/// byte is always >= 0x80, so an ASCII byte match can never land inside a
/// multi-byte character and every hit is on a char boundary. A non-ASCII
/// needle keeps `find`, where that reasoning does not hold.
fn like_find_from(hay: &str, needle: &str, start: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(start);
    }
    if !needle.is_ascii() {
        return hay[start..].find(needle).map(|rel| start + rel);
    }
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if h.len() < n.len() {
        return None;
    }
    let last = h.len() - n.len();
    let mut i = start;
    while i <= last {
        let off = h[i..=last].iter().position(|&b| b == n[0])?;
        let at = i + off;
        if &h[at..at + n.len()] == n {
            return Some(at);
        }
        i = at + 1;
    }
    None
}

fn like_substring_match(hay: &str, needle: &str, k: usize, m: usize) -> bool {
    let mut start = 0;
    while let Some(off) = like_find_from(hay, needle, start) {
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
/// v7.39 (round 621) — is this cast an identity on THIS value?
///
/// `s::TEXT` over a text cell changes nothing, and neither does an unbounded
/// `::VARCHAR`; the compiled path used to clone the cell anyway. Only the
/// pairs that provably change nothing are listed — a bounded VARCHAR(n) must
/// still check its length, numerics their range — so an unlisted pair merely
/// keeps the owned path, never a wrong answer.
/// The catalog-free varchar/char family, in the canonical `name(p)` spelling
/// the parser produces. Only these Named targets stay on the compiled path.
fn named_varchar_family(n: &str) -> bool {
    let base = n.split('(').next().unwrap_or(n);
    base.eq_ignore_ascii_case("varchar")
        || base.eq_ignore_ascii_case("text")
        || base.eq_ignore_ascii_case("char")
        || base.eq_ignore_ascii_case("bpchar")
        || base.eq_ignore_ascii_case("character")
}

/// `varchar(k)`'s k, when the name carries one.
fn varchar_limit(n: &str) -> Option<usize> {
    let base = n.split('(').next().unwrap_or(n);
    if !base.eq_ignore_ascii_case("varchar") {
        return None;
    }
    let inner = n.split('(').nth(1)?.strip_suffix(')')?;
    inner.trim().parse().ok()
}

fn cast_is_identity_for(v: &Value<'_>, target: &spg_sql::ast::CastTarget) -> bool {
    match (v, target) {
        (Value::Text(_), spg_sql::ast::CastTarget::Text) => true,
        (Value::Text(t), spg_sql::ast::CastTarget::Named(n)) => {
            // Unbounded text and varchar change nothing. A BOUNDED varchar is
            // an identity exactly when the text is within its limit — VARCHAR
            // truncates and never pads. CHAR(n) pads, so it is never one.
            n.eq_ignore_ascii_case("text")
                || n.eq_ignore_ascii_case("varchar")
                || varchar_limit(n).is_some_and(|k| t.chars().take(k + 1).count() <= k)
        }
        (Value::Int(_), spg_sql::ast::CastTarget::Int) => true,
        (Value::BigInt(_), spg_sql::ast::CastTarget::BigInt) => true,
        (Value::Float(_), spg_sql::ast::CastTarget::Float) => true,
        (Value::Bool(_), spg_sql::ast::CastTarget::Bool) => true,
        _ => false,
    }
}

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
        // v7.39 (round 594) — a `regexp_like` with a LITERAL pattern is
        // compilable even though the function is not on the pure list: the
        // pattern becomes a compile product (`Step::Regex`) rather than an
        // argument the step would have to re-parse per row. A non-literal
        // pattern stays off, because then it really can differ row to row.
        Expr::FunctionCall { name, args }
            if name.eq_ignore_ascii_case("regexp_like")
                && matches!(args.len(), 2 | 3)
                && regex_literal_parts(args.as_slice()).is_some() =>
        {
            fully_compilable(&args[0])
        }
        Expr::FunctionCall { name, args } => {
            is_pure_scalar_function(name) && args.iter().all(fully_compilable)
        }
        // v7.36 — CAST over a compilable expression. `cast_value`
        // is pure / context-free for the scalar targets we care
        // about (text, ints, floats, bool, dates).
        // v7.39 (read01 ruleutils.c) — regclass / user-named casts
        // need the catalog (dual-shape resolve, domain/enum/composite
        // hooks); they stay Subtree so eval's pre-hook runs.
        Expr::AnyAll { expr, array, .. } if constant_expr(array) => fully_compilable(expr),
        Expr::Extract { source, .. } => fully_compilable(source),
        Expr::Cast { expr, target } => {
            // v7.39 (round 621) — the varchar/char family is catalog-free and
            // compiles (the compile arm gates it the same way); other Named
            // targets still need eval's catalog pre-hooks.
            let target_ok = match target {
                spg_sql::ast::CastTarget::RegClass => false,
                // v7.39 (round 722) — a compile-time-resolvable plain name
                // is as compilable as the dedicated variants; see
                // `Step::CastPlain`.
                spg_sql::ast::CastTarget::Named(n) => {
                    named_varchar_family(n) || super::cast::plain_named_target(n).is_some()
                }
                _ => true,
            };
            target_ok && fully_compilable(expr)
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

/// v7.39 (round 595) — functions that are NOT context-free but ARE fixed for
/// the whole statement: they read the session's time zone, DateStyle or
/// lc_time out of the `EvalContext`, and `Step::Function` hands that context
/// to `apply_function_lower` exactly as the interpreter would.
///
/// Keeping them off the compiled path cost the whole predicate, not just the
/// call: one non-compilable node disqualifies the entire WHERE, so
/// `WHERE date_trunc('day', t) = TIMESTAMP '…'` interpreted the column read
/// and the comparison too — 153.8 ms over 500k rows against PG18's 9.7,
/// where a compiled comparison on the same column is 13.1.
///
/// `now` / `random` / sequence accessors stay off: they are not fixed for
/// the statement in the way these are.
fn is_session_deterministic_function(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        // v7.39 (round 717) — `format` belongs here, not on the pure
        // list: it renders arguments through the SESSION's RenderStyle
        // (datestyle / extra_float_digits / bytea_output), exactly the
        // dependency class to_char carries. Its absence from BOTH lists
        // was the round-716 panel's 4.89× cell — the only remaining
        // text-shape loss that was pure fallback tax.
        "date_trunc" | "date_part" | "to_char" | "age" | "format"
    )
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
    is_session_deterministic_function(name)
        || matches!(
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
                // v7.39 (round 728) — the JSON constructors: pure over
                // their arguments (JSON's number/text rendering is fixed
                // by the format, not the session's RenderStyle — probed
                // against the ::JSONB cast lane, already whitelisted).
                // v7.39 (round 730) — the digest family: pure bytes-in,
                // hex/bytea-out. count(md5(s)) was the panel's last
                // serial-lane text cell (2.37×): the hash itself is
                // ~40% faster than PG's per call here, and ALL of the
                // loss was the missing parallel lane.
                | "md5"
                | "sha224"
                | "sha256"
                | "sha384"
                | "sha512"
                | "to_json"
                | "to_jsonb"
                | "jsonb_build_object"
                | "json_build_object"
                | "jsonb_build_array"
                | "json_build_array"
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
    let mut c = CompiledExpr {
        steps,
        pred_shape: PredShape::Other,
    };
    // Classified through the very matchers the row loop will use, so the
    // label and the destructuring cannot disagree.
    c.pred_shape = if c.as_column_cmp_literal().is_some() {
        PredShape::ColumnCmpLit
    } else if c.as_column_in_set().is_some() {
        PredShape::ColumnInSet
    } else if c.as_column_like().is_some() {
        PredShape::ColumnLike
    } else {
        PredShape::Other
    };
    c
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
    let result = eval_compiled_ref(c, rowref, ctx, &mut local_stack);
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
    // The shape was settled at compile time; the row loop reads one
    // discriminant instead of re-matching the step list per row.
    match c.pred_shape {
        // v7.39 (round 482) — `<column> <cmp> <literal>` compares in place.
        //
        // The general path builds three `Value`s a row and drops them;
        // this one reads both operands by reference and builds only the
        // comparison result. `apply_binary_by_ref` is the SAME function
        // `Step::Binary` reaches for first, so the answer is identical by
        // construction rather than by a second reading of the semantics.
        PredShape::ColumnCmpLit => {
            if let Some((pos, op, lit)) = c.as_column_cmp_literal() {
                crate::bump_counter!(STEP_VM_FASTPRED_FIRE);
                let cell = row.values.get(pos).unwrap_or(&Value::Null);
                if let Some(res) = super::apply_binary_by_ref(op, cell, lit)? {
                    return crate::eval::predicate_is_true(&res, "WHERE", mysql);
                }
                // The by-ref form declined (an op that builds an owned
                // result); fall through rather than answer differently
                // from the VM.
            }
        }
        // v7.39 (round 486) — `<column> [NOT] IN (<literals>)` looks the
        // cell up in place. Same `in_set_verdict` the `InSet` step calls,
        // so the answer is identical by construction; a family mismatch
        // returns None and falls through to the general path, which takes
        // the step's interpreter fallback.
        PredShape::ColumnInSet => {
            if let Some((pos, set, has_null, negated)) = c.as_column_in_set() {
                let cell = row.values.get(pos).unwrap_or(&Value::Null);
                if let Some(v) = in_set_verdict(cell, set, has_null, negated) {
                    crate::bump_counter!(STEP_VM_FASTPRED_FIRE);
                    return crate::eval::predicate_is_true(&v, "WHERE", mysql);
                }
            }
        }
        // v7.39 (round 488) — `<column> [NOT] [I]LIKE '<literal>'` matches
        // straight off the cell. The matcher wanted a `&str` all along;
        // the VM was pushing a `Value` and popping it for no other reason.
        PredShape::ColumnLike => {
            if let Some((pos, step)) = c.as_column_like() {
                let cell = row.values.get(pos).unwrap_or(&Value::Null);
                if let Some(v) = like_verdict(cell, step) {
                    crate::bump_counter!(STEP_VM_FASTPRED_FIRE);
                    return crate::eval::predicate_is_true(&v?, "WHERE", mysql);
                }
            }
        }
        PredShape::Other => {}
    }
    let rowref = crate::join::RowRef::Owned(row);
    let mut local_stack: Vec<Value<'_>> = core::mem::take(stack);
    let verdict = eval_compiled_ref(c, rowref, ctx, &mut local_stack)
        .and_then(|v| crate::eval::predicate_is_true(&v, "WHERE", mysql));
    *stack = recycle_stack(local_stack);
    verdict
}

/// v7.39 (round 486) — the membership decision, shared by `Step::InSet`
/// and by the fast predicate below so the two cannot drift. `None` means
/// the needle's family does not match the set's, which is the caller's
/// cue to take the interpreter's coercion path on the whole node.
///
/// v7.39 (round 489) — `#[inline(always)]` is load-bearing, and the
/// measurement behind it is worth stating because round 486 got it wrong.
/// Round 486 saw the shared-helper form cost `like_filter` 4.5 % and
/// concluded "editing this loop is expensive"; it then duplicated the
/// body into the arm to avoid touching it. Re-measured with the shape
/// ISOLATED (round 488 found the panel's shapes contaminate each other),
/// `like_filter` shows no such cost — that reading was its neighbours.
/// What IS real is `big_in`: +4.6 % with a plain call, separated spreads,
/// on a shape that takes the fast path and never executes this arm.
/// `#[inline(always)]` returns it to parity (-0.1 %, overlapping), so the
/// duplicate bought nothing and is gone.
///
/// `e2e_in_set_fast_path_round486` still runs every needle × set ×
/// negated × has-NULL combination down BOTH entry points.
#[allow(clippy::inline_always)] // measured: see the note above
#[inline(always)]
fn in_set_verdict(
    needle: &Value<'_>,
    set: &crate::memoize::InListSet,
    has_null: bool,
    negated: bool,
) -> Option<Value<'static>> {
    let contained = match (needle, set) {
        // Non-empty list + NULL needle → NULL (NOT NULL is still NULL) —
        // matches the interpreter and eval_with_in_sets.
        (Value::Null, _) => return Some(Value::Null),
        (Value::SmallInt(n), crate::memoize::InListSet::Int(s)) => s.contains(&i64::from(*n)),
        (Value::Int(n), crate::memoize::InListSet::Int(s)) => s.contains(&i64::from(*n)),
        (Value::BigInt(n), crate::memoize::InListSet::Int(s)) => s.contains(n),
        (Value::Text(t), crate::memoize::InListSet::Text(s)) => s.contains(t.as_ref()),
        _ => return None,
    };
    let inner = if contained {
        Value::Bool(true)
    } else if has_null {
        Value::Null
    } else {
        Value::Bool(false)
    };
    Some(match (negated, inner) {
        (true, Value::Bool(b)) => Value::Bool(!b),
        (_, v) => v,
    })
}

/// v7.39 (round 604) — the membership set of an ALREADY-EVALUATED constant
/// array.
///
/// Round 597 gave `x = ANY (ARRAY[1,2,3])` the same set an IN list builds,
/// which took it from 268 ms over 500k rows to 1.93. It could not do the
/// same for `x = ANY ('{1,2,3}'::int[])`, because it built the set from AST
/// literals and that spelling keeps its elements inside a string: the array
/// was folded once but every row still walked it, and the shape stayed at
/// 43.49 ms against PG18's 9.37. The array has been evaluated by the time
/// this is asked, so the elements are right there.
///
/// The families are the ones `build_in_list_set` accepts, for the same
/// reason: an integer set answers `Int = BigInt` correctly across widths,
/// and a text set compares verbatim. Anything else — a mixed array, floats,
/// NUMERIC, dates — returns `None` and keeps the folded-array walk.
fn value_array_in_list_set(arr: &Value<'_>) -> Option<crate::memoize::InListSetEntry> {
    let len = crate::eval::values::array_len(arr)?;
    if len == 0 {
        return None;
    }
    let mut ints: hashbrown::HashSet<i64> = hashbrown::HashSet::with_capacity(len);
    let mut texts: hashbrown::HashSet<alloc::string::String> =
        hashbrown::HashSet::with_capacity(len);
    let mut has_null = false;
    for i in 0..len {
        match crate::eval::values::array_element_at(arr, i) {
            None | Some(Value::Null) => has_null = true,
            Some(Value::SmallInt(n)) => {
                ints.insert(i64::from(n));
            }
            Some(Value::Int(n)) => {
                ints.insert(i64::from(n));
            }
            Some(Value::BigInt(n)) => {
                ints.insert(n);
            }
            Some(Value::Text(s) | Value::BpChar(s)) => {
                texts.insert(s.into_owned());
            }
            _ => return None,
        }
        if !ints.is_empty() && !texts.is_empty() {
            return None;
        }
    }
    let set = if !ints.is_empty() {
        crate::memoize::InListSet::Int(ints)
    } else if !texts.is_empty() {
        crate::memoize::InListSet::Text(texts)
    } else {
        return None;
    };
    Some(crate::memoize::InListSetEntry { set, has_null })
}

/// v7.39 (round 597) — the literal elements of an `ARRAY[…]` constructor.
/// `None` for any other right-hand side, including the `'{1,2}'::int[]`
/// spelling, whose elements live inside a string rather than the tree.
fn array_literal_items(e: &Expr) -> Option<&[Expr]> {
    match e {
        Expr::Array(items) if items.iter().all(constant_expr) => Some(items.as_slice()),
        _ => None,
    }
}

/// v7.39 (round 605) — the value of a projection item that cannot depend on
/// the row, evaluated once. `None` for anything that depends on a row, or
/// that fails to evaluate — the latter so its error still comes from the row
/// loop, in the interpreter's own wording, rather than from planning.
pub(crate) fn constant_projection_value(
    e: &Expr,
    ctx: &EvalContext<'_>,
) -> Option<Value<'static>> {
    if matches!(e, Expr::Literal(_)) || !constant_expr(e) {
        return None;
    }
    eval_expr(e, &Row::new(alloc::vec::Vec::new()), ctx).ok()
}

/// v7.39 (round 597) — an expression whose value cannot depend on the row.
/// An allowlist of node kinds, for the reason rounds 590 and 596 recorded:
/// asking "does it mention a column" would admit a node the walk did not
/// know about, and a function whose volatility SPG cannot look up.
fn constant_expr(e: &Expr) -> bool {
    match e {
        Expr::Literal(_) => true,
        Expr::Array(items) => items.iter().all(constant_expr),
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } => constant_expr(expr),
        Expr::Binary { lhs, rhs, .. } => constant_expr(lhs) && constant_expr(rhs),
        _ => false,
    }
}

/// v7.39 (round 595) — the source sub-expression of the EXTRACT node a
/// `Step::Extract` was compiled from. Only its declared TYPE is read, for
/// the error wording; the value came off the stack.
fn source_of_extract(node: &Expr) -> &Expr {
    match node {
        Expr::Extract { source, .. } => source,
        other => other,
    }
}

/// v7.39 (round 594) — the literal pattern and case flag of a `regexp_like`
/// call, when both are literals. `None` keeps the call on the interpreter.
fn regex_literal_parts(args: &[Expr]) -> Option<(&str, bool)> {
    let Expr::Literal(spg_sql::ast::Literal::String(pat)) = &args[1] else {
        return None;
    };
    let ci = match args.get(2) {
        None => false,
        Some(Expr::Literal(spg_sql::ast::Literal::String(f))) => f.contains('i'),
        Some(_) => return None,
    };
    Some((pat.as_str(), ci))
}

/// The verdict `Step::Regex` produces. `None` means the operand is not text,
/// which is the caller's cue to fall through to the interpreter for its own
/// coercion and wording.
fn regex_verdict(cell: &Value<'_>, re: &crate::eval::CompiledRe) -> Option<Result<Value<'static>, EvalError>> {
    let text = match cell {
        Value::Null => return Some(Ok(Value::Null)),
        Value::Text(t) | Value::BpChar(t) => t.as_ref(),
        _ => return None,
    };
    Some(crate::eval::compiled_is_match(re, text).map(Value::Bool))
}

/// v7.39 (round 488) — the verdict `Step::Like` / `Step::LikeSubstring`
/// produce, restated for the fast predicate. `None` means the operand is
/// not text, which is the caller's cue to fall through to the VM and let
/// it raise the type error in its own wording.
///
/// v7.39 (round 489) — the VM arm calls this too, so there is one body
/// rather than two that can drift. Round 488 kept them separate on round
/// 486's belief that editing that loop costs unrelated shapes; round 489
/// re-measured that belief with the shapes isolated and force-inlined the
/// helper, and the cost is gone (see `in_set_verdict`).
/// `e2e_like_fast_path_round488` runs both entry points over the same
/// matrix.
#[allow(clippy::inline_always)] // measured: see `in_set_verdict`
#[inline(always)]
fn like_verdict(cell: &Value<'_>, step: &Step) -> Option<Result<Value<'static>, EvalError>> {
    let (text, negated) = match (cell, step) {
        (Value::Null, _) => return Some(Ok(Value::Null)),
        (
            Value::Text(t) | Value::BpChar(t),
            Step::Like { negated, .. } | Step::LikeSubstring { negated, .. },
        ) => (t.as_ref(), *negated),
        _ => return None,
    };
    let matched = match step {
        Step::Like {
            pattern,
            case_insensitive,
            ..
        } => {
            let r = if *case_insensitive {
                like_match_str(&text.to_lowercase(), pattern, 0)
            } else {
                like_match_str(text, pattern, 0)
            };
            match r {
                Ok(m) => m,
                Err(e) => return Some(Err(e)),
            }
        }
        Step::LikeSubstring {
            needle,
            k_before,
            m_after,
            case_insensitive,
            ..
        } => {
            if *case_insensitive {
                like_substring_match(&text.to_lowercase(), needle, *k_before, *m_after)
            } else {
                like_substring_match(text, needle, *k_before, *m_after)
            }
        }
        _ => return None,
    };
    Some(Ok(Value::Bool(if negated { !matched } else { matched })))
}

/// Return an emptied stack's allocation with its value lifetime reset.
/// This is the standard "recycle" pattern (cf. the `recycle_vec` crate):
/// an EMPTY `Vec<Value<'a>>` holds no values, only a raw allocation, so
/// re-labelling its element lifetime cannot dangle.
#[allow(unsafe_code)] // empty-Vec lifetime relabel; isolated (see SAFETY).
fn recycle_stack(mut v: Vec<Value<'_>>) -> Vec<Value<'static>> {
    // v7.39 (round 481) — read before the clear: this is exactly the set of
    // values the clear is about to drop.
    crate::bump_counter!(STEP_VM_STACK_LEFTOVER, v.len() as u64);
    #[cfg(feature = "perf-counters")]
    {
        let heap = v
            .iter()
            .filter(|x| {
                matches!(
                    x,
                    Value::Text(_) | Value::Bytes(_) | Value::Json(_) | Value::Vector(_)
                )
            })
            .count();
        crate::bump_counter!(STEP_VM_STACK_LEFTOVER_HEAP, heap as u64);
    }
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
    // v7.39 (round 656) — BY VALUE. `RowRef` is `Copy` and its `get`
    // borrows the row data, not the wrapper, so taking a reference here
    // only served to tie the result's lifetime to a caller local — which
    // is what stopped the aggregate loop from holding its `RowRef` by
    // value and forced a materialised `Vec<RowRef>` per scan.
    row: crate::join::RowRef<'row>,
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
    // v7.39 (round 656) — BY VALUE. `RowRef` is `Copy` and its `get`
    // borrows the row data, not the wrapper, so taking a reference here
    // only served to tie the result's lifetime to a caller local — which
    // is what stopped the aggregate loop from holding its `RowRef` by
    // value and forced a materialised `Vec<RowRef>` per scan.
    row: crate::join::RowRef<'row>,
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
    // v7.39 (round 656) — BY VALUE. `RowRef` is `Copy` and its `get`
    // borrows the row data, not the wrapper, so taking a reference here
    // only served to tie the result's lifetime to a caller local — which
    // is what stopped the aggregate loop from holding its `RowRef` by
    // value and forced a materialised `Vec<RowRef>` per scan.
    row: crate::join::RowRef<'row>,
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
            Step::Connective { op, rhs } => {
                crate::bump_counter!(STEP_VM_BINARY_FIRE);
                let l = stack.pop().unwrap_or(Value::Null).into_owned();
                // The left decides, or it does not. A NULL decides nothing:
                // NULL AND false is false, so the right side is still needed.
                match (op, &l) {
                    (BinOp::And, Value::Bool(false)) => {
                        stack.push(Value::Bool(false));
                        continue;
                    }
                    (BinOp::Or, Value::Bool(true)) => {
                        stack.push(Value::Bool(true));
                        continue;
                    }
                    _ => {}
                }
                run_compiled_steps(rhs, row, ctx, stack)?;
                let r = stack.pop().unwrap_or(Value::Null).into_owned();
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
            Step::AnyTextMatch { negated } => {
                let v = stack.pop().unwrap_or(Value::Null);
                stack.push(match v {
                    Value::Null => Value::Null,
                    _ => Value::Bool(!*negated),
                });
            }
            Step::InSet {
                set,
                has_null,
                negated,
                fallback,
            } => {
                let needle = stack.pop().unwrap_or(Value::Null);
                match in_set_verdict(&needle, set, *has_null, *negated) {
                    Some(v) => stack.push(v),
                    // Cross-family needle: take the interpreter's
                    // exact coercion / error path on the whole node.
                    None => stack.push(eval_expr(fallback, &row.as_row(), ctx)?),
                }
            }
            Step::AnyAll { op, is_any, arr } => {
                let lhs = stack.pop().unwrap_or(Value::Null).into_owned();
                stack.push(crate::eval::any_all_over(lhs, arr.clone(), op, *is_any)?);
            }
            Step::Extract { field, fallback } => {
                let v = stack.pop().unwrap_or(Value::Null).into_owned();
                stack.push(crate::eval::extract_from_value(
                    field,
                    v,
                    source_of_extract(fallback),
                    ctx,
                )?);
            }
            Step::Regex { re, fallback } => {
                let v = stack.pop().unwrap_or(Value::Null);
                match regex_verdict(&v, re) {
                    Some(r) => stack.push(r?),
                    // Not text: the interpreter's coercion and wording, on
                    // the whole node, exactly as `Step::InSet` does.
                    None => stack.push(eval_expr(fallback, &row.as_row(), ctx)?),
                }
            }
            step @ (Step::Like { .. } | Step::LikeSubstring { .. }) => {
                // v7.39 (round 489) — one arm for both pattern steps,
                // sharing `like_verdict` with the fast predicate.
                //
                // The matching itself was already out of line: v7.37.16
                // borrowed the operand instead of paying `.into_owned()`
                // plus a per-row `Vec<char>` collect (~90 ns/row of
                // allocator traffic on a LIKE table scan), and round 484
                // replaced `str::find`'s two-way searcher — whose SETUP
                // was 14.6 % of self time, rebuilt every row for a
                // two-byte constant needle — with an ASCII byte scan.
                // ILIKE still lowercases; plain LIKE allocates nothing.
                let v = stack.pop().unwrap_or(Value::Null);
                match like_verdict(&v, step) {
                    Some(r) => stack.push(r?),
                    None => {
                        return Err(EvalError::TypeMismatch {
                            detail: format!(
                                "LIKE requires text operands, got {}",
                                crate::conversions::pg_type_name_for_error_opt(v.data_type())
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
                                "length() needs text or bytea, got {}",
                                crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
                                "octet_length() needs text or bytea, got {}",
                                crate::conversions::pg_type_name_for_error_opt(other.data_type())
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
            Step::Coalesce { n_args } => {
                let start = stack.len().saturating_sub(*n_args);
                // The widening `COALESCE(1, 2.5)` needs only exists when the
                // non-null arguments carry MIXED types; inspected by ref, and
                // the mixed shapes fall to the owned arm that always did it.
                let mut mixed = false;
                let mut seen: Option<spg_storage::DataType> = None;
                for v in &stack[start..] {
                    if let Some(t) = v.data_type() {
                        match seen {
                            None => seen = Some(t),
                            Some(prev) if prev != t => {
                                mixed = true;
                                break;
                            }
                            Some(_) => {}
                        }
                    }
                }
                if mixed {
                    let result =
                        super::functions::apply_function_lower("coalesce", &stack[start..], ctx)?;
                    stack.truncate(start);
                    stack.push(result);
                } else {
                    let chosen = stack[start..]
                        .iter()
                        .position(|v| !matches!(v, Value::Null));
                    match chosen {
                        Some(k) => {
                            let v = stack.swap_remove(start + k);
                            stack.truncate(start);
                            stack.push(v);
                        }
                        None => {
                            stack.truncate(start);
                            stack.push(Value::Null);
                        }
                    }
                }
            }
            Step::Extremum { n_args, max } => {
                let start = stack.len().saturating_sub(*n_args);
                // Fast path: every non-NULL argument carries the SAME
                // concrete type — the comparison is the type's own and
                // the widen-to-common finish is the identity. Everything
                // else (mixed types, unknown-type text beside a typed
                // sibling, xid's refusal, MySQL's NULL-poisoning) falls
                // to the function arm unchanged.
                let mut uniform: Option<spg_storage::DataType> = None;
                let mut any_null = false;
                let mut fall_back = false;
                for v in &stack[start..] {
                    if matches!(v, Value::Null) {
                        any_null = true;
                        continue;
                    }
                    if matches!(v, Value::Xid(_)) {
                        fall_back = true;
                        break;
                    }
                    match (v.data_type(), uniform) {
                        (Some(t), None) => uniform = Some(t),
                        (Some(t), Some(prev)) if t != prev => {
                            fall_back = true;
                            break;
                        }
                        (Some(_), Some(_)) => {}
                        (None, _) => {
                            fall_back = true;
                            break;
                        }
                    }
                }
                if fall_back || (ctx.mysql_dialect && any_null) {
                    let name = if *max { "greatest" } else { "least" };
                    let result =
                        super::functions::apply_function_lower(name, &stack[start..], ctx)?;
                    stack.truncate(start);
                    stack.push(result);
                } else {
                    let mut best: Option<usize> = None;
                    for k in start..stack.len() {
                        if matches!(&stack[k], Value::Null) {
                            continue;
                        }
                        match best {
                            None => best = Some(k),
                            Some(b) => {
                                let ord = super::values::value_cmp_for_min_max(
                                    &stack[b],
                                    &stack[k],
                                    ctx.mysql_dialect,
                                );
                                let take = if *max {
                                    ord == core::cmp::Ordering::Less
                                } else {
                                    ord == core::cmp::Ordering::Greater
                                };
                                if take {
                                    best = Some(k);
                                }
                            }
                        }
                    }
                    match best {
                        Some(k) => {
                            let v = stack.swap_remove(k);
                            stack.truncate(start);
                            stack.push(v);
                        }
                        None => {
                            stack.truncate(start);
                            stack.push(Value::Null);
                        }
                    }
                }
            }
            Step::NullIf => {
                let n = stack.len();
                // NULLIF is `=` under the hood and keeps round 238's refusal
                // of incomparable operands; both reads are by reference.
                let verdict = match (&stack[n - 2], &stack[n - 1]) {
                    (Value::Null, _) => Some(true),
                    (_, Value::Null) => Some(false),
                    (a, b) => {
                        super::binop::require_comparable(spg_sql::ast::BinOp::Eq, a, b)?;
                        match super::apply_binary_by_ref(spg_sql::ast::BinOp::Eq, a, b)? {
                            Some(Value::Bool(eq)) => Some(eq),
                            _ => None,
                        }
                    }
                };
                match verdict {
                    Some(true) => {
                        stack.truncate(n - 2);
                        stack.push(Value::Null);
                    }
                    Some(false) => {
                        let a = stack.swap_remove(n - 2);
                        stack.truncate(n - 2);
                        stack.push(a);
                    }
                    // The by-ref compare could not decide — the owned arm can.
                    None => {
                        let result =
                            super::functions::apply_function_lower("nullif", &stack[n - 2..], ctx)?;
                        stack.truncate(n - 2);
                        stack.push(result);
                    }
                }
            }
            Step::Cast { target } => {
                crate::bump_counter!(STEP_VM_CAST_FIRE);
                // v7.39 (round 621) — two allocations a row lived on this one
                // line: `into_owned()` cloned a borrowed text cell just to
                // hand it to the cast, and `target.clone()` re-built the
                // target (a String, for the Named form) EVERY row even though
                // it is a compile product. `count(s::TEXT)` — a cast that
                // changes nothing — measured 2.00 allocs/row and 12 ms where
                // `count(s)` measures 0.00 and 2.8 ms.
                //
                // A cast that is an identity on the value it was given hands
                // the borrowed value straight back; everything else takes the
                // owned path, with the target passed by reference.
                let v = stack.pop().unwrap_or(Value::Null);
                if cast_is_identity_for(&v, target) {
                    stack.push(v);
                } else {
                    stack.push(super::cast::cast_value_ref_in(
                        v.into_owned(),
                        target,
                        ctx.mysql_dialect,
                    )?);
                }
            }
            Step::CastPlain { dt, name } => {
                let v = stack.pop().unwrap_or(Value::Null);
                // The name is pre-validated (it came off the plain table),
                // so NULL keeps its short-circuit; a same-type value passes
                // through untouched, exactly the identity the Cast step
                // recognises.
                let identity = matches!(
                    (&v, dt),
                    (Value::Null, _)
                        | (Value::Int(_), spg_storage::DataType::Int)
                        | (Value::BigInt(_), spg_storage::DataType::BigInt)
                        | (Value::SmallInt(_), spg_storage::DataType::SmallInt)
                        | (Value::Real(_), spg_storage::DataType::Real)
                        | (Value::Float(_), spg_storage::DataType::Float)
                        | (Value::Bool(_), spg_storage::DataType::Bool)
                        | (Value::Date(_), spg_storage::DataType::Date)
                        | (Value::Uuid(_), spg_storage::DataType::Uuid)
                );
                if identity {
                    stack.push(v);
                } else {
                    stack.push(super::cast::finish_named_cast_plain(
                        v.into_owned(),
                        *dt,
                        name,
                        ctx.mysql_dialect,
                    )?);
                }
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

/// v7.39 (round 481) — how many values the stack still holds when a call
/// finishes, and how many are heap-bearing.
///
/// Round 480 left `drop_glue<Value>` at 16 % of self time with the drops
/// attributed to the predicate closure, i.e. to the stack rather than to
/// the returned value (round 479 removed that one). Whether the ops leave
/// operands behind for the next call's `clear()` to drop is a question
/// with a number, so this counts it rather than reasoning about it — the
/// previous round was spent acting on an inference that turned out to name
/// an unreachable branch.
/// v7.39 (round 482) — how often the `<column> <cmp> <literal>` fast
/// predicate fires, so "is it even reached" is a number and not a guess
/// (round 480 was spent on a branch that turned out to be unreachable).
pub static STEP_VM_FASTPRED_FIRE: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

pub static STEP_VM_STACK_LEFTOVER: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static STEP_VM_STACK_LEFTOVER_HEAP: core::sync::atomic::AtomicU64 =
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
