//! v7.38.13 — a bare `GROUP BY` is a `DISTINCT`, and costs 63 % more.
//!
//! `SELECT k FROM t GROUP BY k ORDER BY k` and
//! `SELECT DISTINCT k FROM t ORDER BY k` return byte-identical output —
//! verified against PostgreSQL 18 on both spellings, all four md5s
//! equal. SPG answered the first in 155.7 ms and the second in 95.2,
//! against PG's 111.3 for either: `uses_aggregate` returns true for any
//! statement carrying a GROUP BY, so a query with no aggregate in it
//! still went through the aggregate executor and paid for the
//! machinery it never used.
//!
//! No sweep cell covers the GROUP BY spelling, which is why a 40 % loss
//! sat there while a neighbouring cell that flickers with the machine
//! cost three releases a round each.
//!
//! The rewrite re-enters the ordinary path, the way
//! `desugar_using_natural` does; clearing `group_by` makes the second
//! pass a no-op.
//!
//! ## What the gate has to rule out
//!
//! The equivalence is NOT general. `SELECT a FROM t GROUP BY a, b`
//! yields one row per (a, b) pair and so may repeat `a`, which
//! `SELECT DISTINCT a` would collapse. So the projected expressions and
//! the group keys must be the SAME SET, not merely overlap. HAVING
//! filters groups and has no DISTINCT spelling. A window function is
//! evaluated after grouping and would see a different input. An
//! ordinal group key (`GROUP BY 1`) refers to a select-list position
//! and is left alone rather than reasoned about.

use alloc::vec::Vec;
use spg_sql::ast::{Expr, SelectItem, SelectStatement};

/// The statement rewritten as a `DISTINCT`, or `None` when it is not
/// exactly one.
pub(crate) fn as_distinct(stmt: &SelectStatement) -> Option<SelectStatement> {
    let keys = stmt.group_by.as_ref()?;
    if keys.is_empty()
        || stmt.group_by_all
        || stmt.having.is_some()
        || stmt.distinct
        || !stmt.distinct_on.is_empty()
    {
        return None;
    }
    // An ordinal key names a select-list position, not a value.
    if keys.iter().any(is_ordinal) {
        return None;
    }
    // Aggregates anywhere put this back on the aggregate path, which is
    // where it belongs.
    if crate::aggregate::uses_aggregate_ignoring_group_by(stmt) {
        return None;
    }
    let mut projected: Vec<&Expr> = Vec::with_capacity(stmt.items.len());
    for item in &stmt.items {
        match item {
            // A wildcard's expansion is not known here, so it is not a
            // set this can compare against the keys.
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => return None,
            SelectItem::Expr { expr, .. } => {
                if crate::window::expr_has_window_pub(expr) {
                    return None;
                }
                projected.push(expr);
            }
        }
    }
    // Same SET both ways — see the module note on `GROUP BY a, b`.
    if !projected.iter().all(|p| keys.contains(p)) {
        return None;
    }
    if !keys.iter().all(|k| projected.contains(&k)) {
        return None;
    }
    // ORDER BY may only name a group key (or a position); anything else
    // is not a column this query can order by at all.
    if !stmt
        .order_by
        .iter()
        .all(|o| is_ordinal(&o.expr) || keys.contains(&o.expr))
    {
        return None;
    }
    let mut out = stmt.clone();
    out.group_by = None;
    out.distinct = true;
    Some(out)
}

/// `GROUP BY 1` / `ORDER BY 2` — a select-list position.
fn is_ordinal(e: &Expr) -> bool {
    matches!(e, Expr::Literal(spg_sql::ast::Literal::Integer(_)))
}
