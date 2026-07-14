//! SELECT/expression structural analysis — does a query refer to a name,
//! collect qualified column references, walk columns and subqueries. Split
//! out of `lib.rs` (v7.32 engine modularisation). Pure AST walks over
//! `spg_sql::ast`, no Engine state.

use alloc::string::String;
use alloc::vec::Vec;

use spg_sql::ast::{Expr, FromClause, SelectItem, SelectStatement};

/// v4.22: cheap structural scan for `FROM <name>` (qualified or
/// not) inside a SELECT — used to verify the anchor of a WITH
/// RECURSIVE CTE doesn't recurse into itself. Conservative: walks
/// FROM joins, subqueries, and unions.
pub(crate) fn select_refers_to(stmt: &SelectStatement, target: &str) -> bool {
    if let Some(from) = &stmt.from
        && from_refers_to(from, target)
    {
        return true;
    }
    for (_, peer) in &stmt.unions {
        if select_refers_to(peer, target) {
            return true;
        }
    }
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item
            && expr_refers_to(expr, target)
        {
            return true;
        }
    }
    if let Some(w) = &stmt.where_
        && expr_refers_to(w, target)
    {
        return true;
    }
    false
}

pub(crate) fn from_refers_to(from: &FromClause, target: &str) -> bool {
    if from.primary.name.eq_ignore_ascii_case(target) {
        return true;
    }
    from.joins
        .iter()
        .any(|j| j.table.name.eq_ignore_ascii_case(target))
}

/// v7.28 (round-22) — collect every QUALIFIED column referenced
/// anywhere in a SELECT (subquery bodies included). Returns None
/// when a wildcard or a bare column name makes static attribution
/// unsafe — callers then keep every column.
pub(crate) fn collect_qualified_refs(
    stmt: &SelectStatement,
    out: &mut alloc::collections::BTreeSet<(String, String)>,
) -> Option<()> {
    for item in &stmt.items {
        match item {
            SelectItem::Wildcard => return None,
            SelectItem::Expr { expr, .. } => collect_qualified_refs_expr(expr, out)?,
        }
    }
    if let Some(w) = &stmt.where_ {
        collect_qualified_refs_expr(w, out)?;
    }
    if let Some(from) = &stmt.from {
        for j in &from.joins {
            if let Some(on) = &j.on {
                collect_qualified_refs_expr(on, out)?;
            }
            if j.table.lateral_subquery.is_some() {
                return None;
            }
        }
    }
    if let Some(gs) = &stmt.group_by {
        for g in gs {
            collect_qualified_refs_expr(g, out)?;
        }
    }
    if let Some(h) = &stmt.having {
        collect_qualified_refs_expr(h, out)?;
    }
    for o in &stmt.order_by {
        collect_qualified_refs_expr(&o.expr, out)?;
    }
    for (_, peer) in &stmt.unions {
        collect_qualified_refs(peer, out)?;
    }
    for cte in &stmt.ctes {
        // v7.37.43-T4.4 — only Select-bodied CTEs participate in
        // qualified-ref analysis here; modifying CTE bodies
        // (INSERT/UPDATE/DELETE) are independently planned and
        // their inner SELECTs (e.g. INSERT…SELECT) get scanned by
        // dml-side passes.
        if let Some(s) = cte.body.as_select() {
            collect_qualified_refs(s, out)?;
        }
    }
    Some(())
}

pub(crate) fn collect_qualified_refs_expr(
    e: &Expr,
    out: &mut alloc::collections::BTreeSet<(String, String)>,
) -> Option<()> {
    // Two passes so the column and subquery visitors don't both
    // capture `out` mutably.
    let mut cols: Vec<spg_sql::ast::ColumnName> = Vec::new();
    let mut subs: Vec<&SelectStatement> = Vec::new();
    visit_expr_columns_and_subqueries(
        e,
        &mut |c: &spg_sql::ast::ColumnName| cols.push(c.clone()),
        &mut |sub| subs.push(sub),
    );
    for c in cols {
        {
            let q = c.qualifier?;
            out.insert((q, c.name));
        }
    }
    for sub in subs {
        collect_qualified_refs(sub, out)?;
    }
    Some(())
}

/// Immutable walk over an Expr visiting every Column and every
/// nested SelectStatement (v7.28).
pub(crate) fn visit_expr_columns_and_subqueries<'a>(
    e: &'a Expr,
    on_col: &mut impl FnMut(&'a spg_sql::ast::ColumnName),
    on_sub: &mut impl FnMut(&'a SelectStatement),
) {
    match e {
        Expr::Column(c) => on_col(c),
        Expr::ScalarSubquery(s) => on_sub(s),
        Expr::Exists { subquery, .. } => on_sub(subquery),
        Expr::InSubquery { expr, subquery, .. } => {
            visit_expr_columns_and_subqueries(expr, on_col, on_sub);
            on_sub(subquery);
        }
        Expr::Binary { lhs, rhs, .. } => {
            visit_expr_columns_and_subqueries(lhs, on_col, on_sub);
            visit_expr_columns_and_subqueries(rhs, on_col, on_sub);
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => {
            visit_expr_columns_and_subqueries(expr, on_col, on_sub);
        }
        Expr::Like { expr, pattern, .. } => {
            visit_expr_columns_and_subqueries(expr, on_col, on_sub);
            visit_expr_columns_and_subqueries(pattern, on_col, on_sub);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                visit_expr_columns_and_subqueries(a, on_col, on_sub);
            }
        }
        Expr::AggregateOrdered { call, order_by, .. } => {
            visit_expr_columns_and_subqueries(call, on_col, on_sub);
            for o in order_by {
                visit_expr_columns_and_subqueries(&o.expr, on_col, on_sub);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(op) = operand {
                visit_expr_columns_and_subqueries(op, on_col, on_sub);
            }
            for (w, t) in branches {
                visit_expr_columns_and_subqueries(w, on_col, on_sub);
                visit_expr_columns_and_subqueries(t, on_col, on_sub);
            }
            if let Some(eb) = else_branch {
                visit_expr_columns_and_subqueries(eb, on_col, on_sub);
            }
        }
        Expr::ArraySubscript { target, index } => {
            visit_expr_columns_and_subqueries(target, on_col, on_sub);
            visit_expr_columns_and_subqueries(index, on_col, on_sub);
        }
        Expr::ArraySlice { target, lo, hi } => {
            visit_expr_columns_and_subqueries(target, on_col, on_sub);
            if let Some(l) = lo {
                visit_expr_columns_and_subqueries(l, on_col, on_sub);
            }
            if let Some(h) = hi {
                visit_expr_columns_and_subqueries(h, on_col, on_sub);
            }
        }
        // v7.37.7 K02 root-cause fix (mailrs cascade 4th recurrence):
        // missing arm caused `Expr::InList` to fall through to the
        // exotic "_" arm which emits a BAIL ColumnName with no
        // qualifier — `expr_is_all_inner` then returned false for
        // ANY conjunct of the form `inner.col IN (literals)`, so the
        // `pull_up_exists_sublinks` pass refused to rewrite Class B's
        // `WHERE ea.message_id = m.id AND ea.category IN ('spam','scam')`
        // and the per-outer-row inner SELECT path stayed live (~10k
        // MemoizeCache lifecycles per query, ~1.9 GB allocator churn,
        // cascaded under 20-conn concurrency to ~8× amplification).
        // Visiting the `expr` and each list item is the same shape
        // every other binary/n-ary node uses; semantics are preserved
        // for callers that only count columns (no `BAIL` injected).
        Expr::InList { expr, list, .. } => {
            visit_expr_columns_and_subqueries(expr, on_col, on_sub);
            for item in list {
                visit_expr_columns_and_subqueries(item, on_col, on_sub);
            }
        }
        Expr::Literal(_) | Expr::Placeholder(_) => {}
        // Exotic nodes (window etc.) — visit nothing extra; their
        // columns are caught when the caller bails on bare names
        // elsewhere, and window queries skip pruning entirely at
        // the call sites.
        _ => {
            // Exotic node (window function etc.): report an
            // unattributable marker so callers disable pruning.
            static BAIL: spg_sql::ast::ColumnName = spg_sql::ast::ColumnName {
                qualifier: None,
                name: String::new(),
            };
            on_col(&BAIL);
        }
    }
}

/// v7.28 (round-22) — collect every Column qualifier in an expr;
/// `all_qualified` flips false on any bare column (those can't be
/// attributed to one table safely, so the pushdown skips them).
pub(crate) fn collect_column_qualifiers<'e>(
    e: &'e Expr,
    out: &mut Vec<&'e str>,
    all_qualified: &mut bool,
) {
    if let Expr::Column(c) = e {
        match &c.qualifier {
            Some(q) => out.push(q.as_str()),
            None => *all_qualified = false,
        }
        return;
    }
    // Reuse the canonical immutable walk via describe's walker shape:
    // recurse the common containers.
    match e {
        Expr::Binary { lhs, rhs, .. } => {
            collect_column_qualifiers(lhs, out, all_qualified);
            collect_column_qualifiers(rhs, out, all_qualified);
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => {
            collect_column_qualifiers(expr, out, all_qualified);
        }
        Expr::Like { expr, pattern, .. } => {
            collect_column_qualifiers(expr, out, all_qualified);
            collect_column_qualifiers(pattern, out, all_qualified);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_column_qualifiers(a, out, all_qualified);
            }
        }
        // v7.33 (mailrs 7.33.0) — `col IN (…)` is attributable to its
        // operands' tables (a literal list adds none), so analyze_join_
        // pushdown can push a single-table `indexed_col IN (lits)` to the
        // primary filter for an index seed instead of leaving it in the
        // residual WHERE as a full scan.
        Expr::InList { expr, list, .. } => {
            collect_column_qualifiers(expr, out, all_qualified);
            for e in list {
                collect_column_qualifiers(e, out, all_qualified);
            }
        }
        Expr::Literal(_) | Expr::Placeholder(_) => {}
        // Anything exotic (CASE, subquery, window, arrays…):
        // conservatively mark unattributable.
        _ => *all_qualified = false,
    }
}

pub(crate) fn expr_refers_to(e: &Expr, target: &str) -> bool {
    match e {
        Expr::NamedArg { expr, .. } => expr_refers_to(expr, target),
        Expr::AggregateOrdered { call, order_by, .. } => {
            expr_refers_to(call, target) || order_by.iter().any(|o| expr_refers_to(&o.expr, target))
        }
        Expr::ScalarSubquery(s) => select_refers_to(s, target),
        Expr::Exists { subquery, .. } | Expr::InSubquery { subquery, .. } => {
            select_refers_to(subquery, target)
        }
        Expr::RowInSubquery { row, subquery, .. } => {
            row.iter().any(|el| expr_refers_to(el, target)) || select_refers_to(subquery, target)
        }
        Expr::RowCmpSubquery { row, subquery, .. } => {
            row.iter().any(|el| expr_refers_to(el, target)) || select_refers_to(subquery, target)
        }
        Expr::Binary { lhs, rhs, .. } => expr_refers_to(lhs, target) || expr_refers_to(rhs, target),
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => expr_refers_to(expr, target),
        Expr::Like { expr, pattern, .. } => {
            expr_refers_to(expr, target) || expr_refers_to(pattern, target)
        }
        Expr::FunctionCall { args, .. } => args.iter().any(|a| expr_refers_to(a, target)),
        Expr::Extract { source, .. } => expr_refers_to(source, target),
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            args.iter().any(|a| expr_refers_to(a, target))
                || partition_by.iter().any(|p| expr_refers_to(p, target))
                || order_by.iter().any(|(o, _, _)| expr_refers_to(o, target))
        }
        Expr::Literal(_) | Expr::Placeholder(_) | Expr::Column(_) => false,
        Expr::Array(items) => items.iter().any(|e| expr_refers_to(e, target)),
        Expr::InList { expr, list, .. } => {
            expr_refers_to(expr, target) || list.iter().any(|e| expr_refers_to(e, target))
        }
        Expr::ArraySubscript { target: t, index } => {
            expr_refers_to(t, target) || expr_refers_to(index, target)
        }
        Expr::ArraySlice { target: t, lo, hi } => {
            expr_refers_to(t, target)
                || lo.as_deref().is_some_and(|l| expr_refers_to(l, target))
                || hi.as_deref().is_some_and(|h| expr_refers_to(h, target))
        }
        Expr::AnyAll { expr, array, .. } => {
            expr_refers_to(expr, target) || expr_refers_to(array, target)
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            operand
                .as_deref()
                .is_some_and(|o| expr_refers_to(o, target))
                || branches
                    .iter()
                    .any(|(w, t)| expr_refers_to(w, target) || expr_refers_to(t, target))
                || else_branch
                    .as_deref()
                    .is_some_and(|e| expr_refers_to(e, target))
        }
    }
}

/// v7.39 (read01 round 78) — the one exhaustive top-down rewrite over an
/// expression tree. `f` sees each node before its children and returns `true`
/// when it has REPLACED that node, in which case the walk does not descend
/// into it.
///
/// Every mutable rewrite in the engine (clock folding, parameter substitution,
/// USING-column rewriting, trigger locals…) had spelled out all ~25 `Expr`
/// variants by hand, so each of them is a place a new variant can be forgotten.
/// This one is written once and the compiler checks it.
///
/// Subquery nodes are LEAVES here on purpose: an inner SELECT has its own scope,
/// and a rewrite that is right in the outer one is rarely right inside it. A
/// caller that wants to descend does so itself, deliberately.
pub(crate) fn rewrite_nodes_mut(e: &mut Expr, f: &mut impl FnMut(&mut Expr) -> bool) {
    if f(e) {
        return;
    }
    match e {
        Expr::Literal(_)
        | Expr::Placeholder(_)
        | Expr::Column(_)
        | Expr::ScalarSubquery(_)
        | Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::RowInSubquery { .. }
        | Expr::RowCmpSubquery { .. } => {}
        Expr::NamedArg { expr, .. }
        | Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::FieldAccess { base: expr, .. }
        | Expr::Extract { source: expr, .. } => rewrite_nodes_mut(expr, f),
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_nodes_mut(lhs, f);
            rewrite_nodes_mut(rhs, f);
        }
        Expr::FunctionCall { args, .. } | Expr::Array(args) => {
            for a in args {
                rewrite_nodes_mut(a, f);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            rewrite_nodes_mut(expr, f);
            rewrite_nodes_mut(pattern, f);
        }
        Expr::AggregateOrdered { call, order_by, .. } => {
            rewrite_nodes_mut(call, f);
            for o in order_by.iter_mut() {
                rewrite_nodes_mut(&mut o.expr, f);
            }
        }
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                rewrite_nodes_mut(a, f);
            }
            for p in partition_by {
                rewrite_nodes_mut(p, f);
            }
            for (oe, _, _) in order_by {
                rewrite_nodes_mut(oe, f);
            }
        }
        Expr::ArraySubscript { target, index } => {
            rewrite_nodes_mut(target, f);
            rewrite_nodes_mut(index, f);
        }
        Expr::ArraySlice { target, lo, hi } => {
            rewrite_nodes_mut(target, f);
            if let Some(l) = lo {
                rewrite_nodes_mut(l, f);
            }
            if let Some(h) = hi {
                rewrite_nodes_mut(h, f);
            }
        }
        Expr::AnyAll { expr, array, .. } => {
            rewrite_nodes_mut(expr, f);
            rewrite_nodes_mut(array, f);
        }
        Expr::InList { expr, list, .. } => {
            rewrite_nodes_mut(expr, f);
            for item in list {
                rewrite_nodes_mut(item, f);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                rewrite_nodes_mut(o, f);
            }
            for (w, t) in branches {
                rewrite_nodes_mut(w, f);
                rewrite_nodes_mut(t, f);
            }
            if let Some(eb) = else_branch {
                rewrite_nodes_mut(eb, f);
            }
        }
    }
}
