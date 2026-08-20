//! v7.38.11 — read a range predicate's bounds so a BRIN index can say
//! which slots the scan may skip.
//!
//! Lives in its own module, and is `#[cold]` `#[inline(never)]`, for a
//! measured reason: v7.38.8's conjunct reorder cost two shapes it did
//! not even touch 17 % and 20 % purely by being compiled into the file
//! that holds the row loop. This runs once per plan; it has no business
//! near that loop's code layout.

use alloc::vec::Vec;
use core::sync::atomic::AtomicU64;

/// Diagnostic only: how often the reader ran, and how often it produced
/// a slot list. Cheap enough to leave in — one relaxed add per plan.
pub static PROBE_ENTERED: AtomicU64 = AtomicU64::new(0);
/// See [`PROBE_ENTERED`].
pub static PROBE_PRUNED: AtomicU64 = AtomicU64::new(0);
use core::ops::Range;

use spg_sql::ast::{BinOp, Expr};
use spg_storage::{Table, Value};

/// The slots a BRIN index cannot rule out for `where_`, or `None` when
/// nothing about this query and table lets it rule anything out.
///
/// `None` means "no opinion" and the caller scans as before. It is
/// returned for a table with no BRIN index, a predicate with no bound
/// on a BRIN column, and — deliberately — for a predicate this reader
/// does not fully understand. Declining is always safe; the danger is
/// only ever in claiming a range can be skipped.
#[cold]
#[inline(never)]
pub(crate) fn candidate_slots(where_: &Expr, table: &Table) -> Option<Vec<Range<usize>>> {
    PROBE_ENTERED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    for col_pos in table.brin_columns() {
        let name = table.schema().columns.get(col_pos)?.name.as_str();
        let (lo, hi) = bounds_on(where_, name);
        if lo.is_none() && hi.is_none() {
            continue;
        }
        if let Some(slots) = table.brin_candidate_slots(col_pos, lo, hi) {
            PROBE_PRUNED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            return Some(slots);
        }
    }
    None
}

/// Walk the AND-chain and collect the tightest lower and upper bound on
/// `col_pos`.
///
/// Only conjuncts joined by AND count. An OR anywhere above a bound
/// makes it unusable — `a < 5 OR b > 9` does not restrict `a` — so this
/// simply does not descend into one.
fn bounds_on(e: &Expr, col: &str) -> (Option<i64>, Option<i64>) {
    let mut lo: Option<i64> = None;
    let mut hi: Option<i64> = None;
    let mut stack = alloc::vec![e];
    while let Some(cur) = stack.pop() {
        match cur {
            Expr::Binary {
                lhs,
                op: BinOp::And,
                rhs,
            } => {
                stack.push(lhs);
                stack.push(rhs);
            }
            Expr::Binary { lhs, op, rhs } => {
                let Some((op, lit)) = normalise(lhs, *op, rhs, col) else {
                    continue;
                };
                let Some(k) = spg_storage::brin_scalar(&lit) else {
                    continue;
                };
                match op {
                    // `x > k` cannot be tightened to `k + 1` here: the
                    // summary comparison is `>=`-shaped and a range
                    // whose max IS k still has to be visited so the
                    // predicate itself can reject it.
                    BinOp::Gt | BinOp::GtEq => lo = Some(lo.map_or(k, |p: i64| p.max(k))),
                    BinOp::Lt | BinOp::LtEq => hi = Some(hi.map_or(k, |p: i64| p.min(k))),
                    BinOp::Eq => {
                        lo = Some(lo.map_or(k, |p: i64| p.max(k)));
                        hi = Some(hi.map_or(k, |p: i64| p.min(k)));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    (lo, hi)
}

/// `(op, literal)` with the column on the left, or `None` if this is
/// not a comparison between THIS column and a literal.
fn normalise(lhs: &Expr, op: BinOp, rhs: &Expr, col: &str) -> Option<(BinOp, Value<'static>)> {
    let lit_of = |e: &Expr| match e {
        Expr::Literal(l) => Some(crate::eval::literal_to_value(l)),
        _ => None,
    };
    let is_col = |e: &Expr| matches!(e, Expr::Column(c) if c.name == col);
    if is_col(lhs) {
        return lit_of(rhs).map(|v| (op, v));
    }
    if is_col(rhs) {
        // `5 < x` is `x > 5`.
        let flipped = match op {
            BinOp::Lt => BinOp::Gt,
            BinOp::LtEq => BinOp::GtEq,
            BinOp::Gt => BinOp::Lt,
            BinOp::GtEq => BinOp::LtEq,
            other => other,
        };
        return lit_of(lhs).map(|v| (flipped, v));
    }
    None
}
