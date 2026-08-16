//! r1042 — evaluate the constant parts of a predicate once, at prepare
//! time, instead of once per row.
//!
//! The cost this removes is not subtle. On a 400,000-row table, measured
//! through the release sweep's own harness:
//!
//! ```text
//! WHERE id = 7                        Index Scan     0.08 ms
//! WHERE id = 7::int                   Seq Scan       1.86 ms   <- 23x
//! WHERE b = '\x…'                     Index Scan     0.08 ms
//! WHERE b = '\x…'::bytea              Seq Scan       2.24 ms   <- 28x
//! WHERE n = 1.23::numeric             Seq Scan
//! ```
//!
//! A cast to the column's OWN type — a no-op as far as the value goes —
//! turned every one of those seeks into a full scan, because the seek
//! resolver reads an `Expr::Literal` and a `Cast` node is not one. That
//! is the shape an ORM writes (`$1::int`), the shape `pg_dump` writes,
//! and the shape anyone writes when they are being explicit. It had been
//! true for every type since long before the types that made it visible.
//!
//! PostgreSQL does this in `eval_const_expressions` before planning, and
//! prints the folded form in `EXPLAIN`, which is why its plans show
//! `Index Cond: (id = 7)` for both spellings.
//!
//! ## What is folded, and why it is an allowlist
//!
//! The SHAPE test is round 597's `constant_expr`, shared with round 605
//! rather than written again: two answers to "is this expression
//! constant" is how they come to disagree. On top of it this pass adds a
//! CONTEXT test of its own, described below.
//!
//! Literals, casts to a CONTEXT-FREE type, unary operators, and
//! arithmetic between them. No function calls — not because they could
//! not be folded, but because folding one requires knowing its
//! VOLATILITY, and a function whose volatility this engine cannot look
//! up would be folded silently and wrongly (`random()` collapsing to one
//! draw). Rounds 590 and 596 drew the same line for the same reason, in
//! the correlated-subquery key and the computed JOIN key. When there is
//! a volatility table to consult, the allowlist is where the immutable
//! ones get added.
//!
//! "Context-free" is the second half of that rule, and the first version
//! of this pass did not have it. `'u'::regclass` means whatever the
//! CATALOG says it means; folded against the empty context this pass
//! evaluates in, it came back as the text `u`, and twenty-six catalog
//! tests went red comparing an oid column to the string `u`. The failure
//! was not that the fold refused — it is that it SUCCEEDED and was
//! wrong. So the cast targets are listed one by one, and a target this
//! pass has not been shown to be catalog-independent is not folded.
//!
//! An expression that RAISES while being folded is left exactly as it
//! was. `WHERE x = 1/0` keeps raising from where it raised before, so
//! this pass cannot move an error to a new place.

use spg_sql::ast::{BinOp, CastTarget, Expr, SelectStatement, Statement, UnOp};
use spg_storage::Row;

use crate::eval::{self, EvalContext};

/// Fold the constant parts of every predicate in `stmt`.
///
/// WHERE and JOIN ON only: that is where a folded constant changes what
/// the executor DOES (an index seek instead of a scan) rather than only
/// what it costs, and a narrow pass is one whose effects can be stated.
pub(crate) fn fold_statement(stmt: &mut Statement) {
    match stmt {
        Statement::Select(s) => fold_select(s),
        Statement::Update(u) => {
            if let Some(w) = u.where_.as_mut() {
                fold_expr(w);
            }
        }
        Statement::Delete(d) => {
            if let Some(w) = d.where_.as_mut() {
                fold_expr(w);
            }
        }
        // EXPLAIN wraps the statement it explains. Folding through it
        // is not cosmetic: the plan a reader is shown has to be the plan
        // that runs, and without this arm `EXPLAIN` printed `Seq Scan`
        // for a query that seeks.
        Statement::Explain(x) => fold_statement(&mut x.inner),
        _ => {}
    }
}

fn fold_select(s: &mut SelectStatement) {
    if let Some(w) = s.where_.as_mut() {
        fold_expr(w);
    }
    if let Some(from) = s.from.as_mut() {
        for j in &mut from.joins {
            if let Some(on) = j.on.as_mut() {
                fold_expr(on);
            }
        }
    }
}

/// Replace every constant subtree of `e` with the value it evaluates to.
///
/// Top-down: a node that folds whole is replaced and not descended into,
/// so `(1 + 2)::text` becomes one literal rather than a cast over one.
fn fold_expr(e: &mut Expr) {
    if is_foldable(e) && !matches!(e, Expr::Literal(_)) {
        if let Some(folded) = evaluate(e) {
            *e = folded;
        }
        return;
    }
    for child in children_mut(e) {
        fold_expr(child);
    }
}

/// Evaluate a constant expression, or `None` if it raises.
///
/// `None` leaves the node alone, so an expression that errors keeps
/// erroring from wherever it did before this pass existed.
fn evaluate(e: &Expr) -> Option<Expr> {
    let cols = [];
    let ctx = EvalContext::new(&cols, None);
    let row = Row::new(alloc::vec::Vec::new());
    let v = eval::eval_expr(e, &row, &ctx).ok()?;
    Some(Expr::Literal(crate::clock::value_to_literal(v)))
}

/// Whether this node is a constant this pass is willing to evaluate.
///
/// Deliberately a list of node kinds rather than "does it mention a
/// column": a node this walk does not know about would otherwise be
/// admitted by default, and the ones it does not know about include
/// every function call.
fn is_foldable(e: &Expr) -> bool {
    crate::eval::compiled::constant_expr(e) && is_context_free(e)
}

/// Whether every part of this expression means the same thing without a
/// catalog and without a session.
///
/// Separate from the SHAPE test above on purpose. Round 605 folds these
/// same expressions inside the row loop, where the real `EvalContext` is
/// in hand, so `'u'::regclass` folds there to the oid it means. This pass
/// runs at prepare time against an empty context, and folded that to the
/// text `u` — twenty-six catalog tests went red comparing an oid column
/// to a string. Same question, two different safe answers, because the
/// two folders can supply different amounts of context.
fn is_context_free(e: &Expr) -> bool {
    match e {
        Expr::Literal(_) => true,
        Expr::Cast { expr, target } => target_is_context_free(target) && is_context_free(expr),
        Expr::Unary { op, expr } => matches!(op, UnOp::Neg | UnOp::Plus) && is_context_free(expr),
        Expr::Binary { lhs, op, rhs } => {
            matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::IntDiv | BinOp::Mod
            ) && is_context_free(lhs)
                && is_context_free(rhs)
        }
        // Not "not constant" — the shape test already answered that.
        // False because this pass has not been shown what the node
        // depends on.
        _ => false,
    }
}

/// Whether a cast to this target means the same thing without a catalog
/// and without a session.
///
/// Listed one by one on purpose. `RegClass` / `RegType` resolve a NAME
/// against the catalog; `Named` is a user type, which may be an enum or
/// a domain with a constraint; both would be folded against nothing at
/// all here. Everything below parses its input by fixed rules that no
/// DDL can change.
fn target_is_context_free(t: &CastTarget) -> bool {
    matches!(
        t,
        CastTarget::Int
            | CastTarget::BigInt
            | CastTarget::Float
            | CastTarget::Text
            | CastTarget::Bool
            | CastTarget::Date
            | CastTarget::Timestamp
            | CastTarget::Uuid
            | CastTarget::Bytea
    )
}

/// The sub-expressions to recurse into for a node this pass will not
/// fold whole. Only the positions a predicate can hold — anything not
/// listed simply is not descended into, which costs a missed fold and
/// never costs correctness.
fn children_mut(e: &mut Expr) -> alloc::vec::Vec<&mut Expr> {
    match e {
        Expr::Binary { lhs, rhs, .. } => alloc::vec![&mut **lhs, &mut **rhs],
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            alloc::vec![&mut **expr]
        }
        Expr::InList { expr, list, .. } => {
            let mut out = alloc::vec![&mut **expr];
            out.extend(list.iter_mut());
            out
        }
        _ => alloc::vec::Vec::new(),
    }
}
