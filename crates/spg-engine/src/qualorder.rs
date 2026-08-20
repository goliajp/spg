//! v7.38.8 — run the cheap half of a conjunction first, whichever half
//! the query happened to write first.
//!
//! Measured on a customer profile, same data, same box, 200,000 rows:
//!
//! ```text
//! WHERE traits @> '{"plan":"pro"}' AND received_at >= … AND received_at < …   39.7 ms
//! WHERE received_at >= … AND received_at < … AND traits @> '{"plan":"pro"}'    8.4 ms
//! ```
//!
//! The same query, in a different written order, 4.7 times apart. PG
//! answers both in 5.3 and 5.4 ms because `order_qual_clauses` sorts
//! quals by estimated cost before the executor sees them; we evaluated
//! them in the order they were typed, and which one a person types
//! first is habit.
//!
//! ## Where this runs, and why not earlier
//!
//! In the compiled predicate, not in the AST. The first version of
//! this pass rewrote the statement in `preprocess`, next to the
//! constant fold, and it won 4.3x on the shape above while making
//! every INDEXED shape slower — 5.20 ms to 6.65 on one, 5.99 to 8.24
//! on the same query written the other way, against 1.86 for the seek
//! alone. Two controls placed it: the same build with the pass
//! early-returning matched the baseline, so it was not code layout;
//! and on an unindexed copy of the same table the pass was FASTER, so
//! it was not a fixed per-query cost.
//!
//! Reordering the tree before the seek matcher reads it changes which
//! seek it finds. PG has the same pass and does not make that mistake:
//! `order_qual_clauses` runs in the executor, over the quals that are
//! LEFT once index conditions have been extracted. So this runs from
//! `compile_expr`, once per plan, after every planner decision is
//! already made.
//!
//! ## Why a partition and not a sort
//!
//! `AND` is commutative under three-valued logic, so the ANSWER does
//! not depend on the order. What does depend on it is which conjuncts
//! get evaluated at all — the machine short-circuits — and therefore
//! which errors a row can raise. Moving an expression that can raise
//! to the FRONT can make an error appear where the query used to
//! short-circuit past it, and that is a change no perf win pays for.
//!
//! So this pass never moves anything that can raise. It stably
//! partitions the conjuncts into "cannot raise, and is cheap" and
//! "everything else", each half keeping the order it was written in.
//! The safe half runs first. A conjunct the classifier does not
//! recognise stays where it is, which is why the classifier is an
//! allowlist and not a deny-list: an expression nobody has thought
//! about is left alone rather than assumed harmless.
//!
//! The effect on errors is one-directional and matches PG's: an error
//! that used to be raised may now be short-circuited past. It cannot
//! introduce one.

use alloc::boxed::Box;
use alloc::vec::Vec;

use spg_sql::ast::{BinOp, Expr};

/// The same conjunction with its cheap, total conjuncts first, or
/// `None` when there is nothing to move.
///
/// `None` rather than a copy that happens to be equal: the caller
/// compiles the original in that case, so a predicate this pass has no
/// opinion about is not cloned at all.
/// `#[cold]` and never inlined, and that is measured rather than
/// decorative. Compiled inline, this function's presence cost two
/// shapes it does not even reorder 17 % and 20 % — a tax that scaled
/// with ROW COUNT (nothing at 5,000 rows, +1.36 ms at 200,000) on a
/// function that runs once per plan. That is the shape of a layout
/// tax on the row loop, not of work being done: the same build with
/// the body replaced by `return None` — which lets the compiler
/// delete it — matched the baseline exactly.
#[cold]
#[inline(never)]
pub(crate) fn reordered(e: &Expr) -> Option<Expr> {
    // Flattened BY REFERENCE, and cloned only if the answer is `Some`.
    // The first version collected owned clones before deciding, and the
    // decision is usually "nothing to move": on the customer profile
    // that cost two shapes 17 % and 20 % — shapes this pass does not
    // even reorder — which is a per-call cost showing up on a path that
    // is entered far more often than once per query.
    let mut parts: Vec<&Expr> = Vec::new();
    flatten_and(e, &mut parts);
    if parts.len() < 2 {
        return None;
    }
    let cheap_count = parts.iter().take_while(|p| cheap_and_total(p)).count();
    let (cheap, rest): (Vec<&Expr>, Vec<&Expr>) =
        parts.into_iter().partition(|p| cheap_and_total(p));
    // Already in this order, or nothing on one side of the partition.
    if cheap.is_empty() || rest.is_empty() || cheap_count == cheap.len() {
        return None;
    }
    let mut it = cheap.into_iter().chain(rest);
    let first = it.next().expect("at least two parts").clone();
    Some(it.fold(first, |acc, p| Expr::Binary {
        lhs: Box::new(acc),
        op: BinOp::And,
        rhs: Box::new(p.clone()),
    }))
}

#[inline(never)]
fn flatten_and<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
    if let Expr::Binary {
        lhs,
        op: BinOp::And,
        rhs,
    } = e
    {
        flatten_and(lhs, out);
        flatten_and(rhs, out);
    } else {
        out.push(e);
    }
}

/// Is this conjunct both cheap to evaluate and incapable of raising?
///
/// An allowlist. A comparison between a column and a literal reads one
/// cell and compares it; `IS [NOT] NULL` reads one cell; a bare boolean
/// column reads one cell. None of them can fail on a row that the scan
/// produced. Everything else — a function call, an operator that
/// interprets a document, arithmetic that can overflow or divide, a
/// subquery, a cast — is left exactly where it was written.
#[inline(never)]
fn cheap_and_total(e: &Expr) -> bool {
    match e {
        Expr::Column(_) => true,
        Expr::IsNull { expr, .. } => matches!(**expr, Expr::Column(_)),
        // Equality is deliberately NOT here, and the omission is
        // measured. `col = <literal>` is the shape an index seek
        // consumes: after the seek every row satisfies it, so hoisting
        // it to the front spends a comparison per row on a question
        // already answered AND demotes the predicate that actually
        // filters. On the customer profile that cost 5.1 ms to 6.8 on
        // one shape while the ranges below were winning 39.4 to 9.4 on
        // another. A range is not a seek key in the same way — the
        // window predicate that motivated this pass is exactly the
        // case worth moving.
        Expr::Binary { lhs, op, rhs } => {
            matches!(op, BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq)
                && ((matches!(**lhs, Expr::Column(_)) && matches!(**rhs, Expr::Literal(_)))
                    || (matches!(**lhs, Expr::Literal(_)) && matches!(**rhs, Expr::Column(_))))
        }
        _ => false,
    }
}
