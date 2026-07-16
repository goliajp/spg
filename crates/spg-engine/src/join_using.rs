//! v7.37.16 — `JOIN … USING (cols)` column-merge + `NATURAL JOIN`.
//!
//! PostgreSQL merges the join columns of a `USING` / `NATURAL` join into a
//! single unqualified output column:
//!   * INNER / LEFT   → the value comes from the LEFT input (`t1.c`)
//!   * RIGHT          → from the RIGHT input (`t2.c`)
//!   * FULL           → `COALESCE(t1.c, t2.c)`
//! The merged column is unqualified `c`, resolves unambiguously (a bare
//! `SELECT c` is legal), and appears FIRST in `SELECT *` (before the two
//! tables' remaining columns). The two physical columns `t1.c` / `t2.c`
//! stay individually addressable through their qualifier.
//!
//! Rather than teach the generic column resolver / projection about
//! merged columns (which would touch every eval + projection site), this
//! pass runs ONCE at the top of the SELECT executor and rewrites the
//! statement into an equivalent one the existing executor already
//! handles:
//!   * unqualified references to a merged column (in the SELECT list,
//!     WHERE, GROUP BY, HAVING, ORDER BY) become the concrete pick
//!     expression (`t1.c` / `t2.c` / `COALESCE(...)`);
//!   * `SELECT *` expands to the merged columns first, then each table's
//!     non-merged columns, all as explicit qualified items with bare
//!     output names — matching PG's shape;
//!   * `NATURAL` joins have their common columns resolved from the table
//!     schemas, an equivalent `ON` predicate synthesised, and the merge
//!     applied exactly like an explicit `USING`.
//!
//! The join FILTER / count path is untouched: an explicit `USING` already
//! carries an equivalent `ON` from the parser, and `NATURAL` gets its
//! `ON` synthesised here. So join cardinality is decided by the same
//! nested-loop / hash stages as before — only the OUTPUT shape changes.
//!
//! Scope note (documented limitation): for a chain of joins whose USING
//! sets name DIFFERENT columns (`a JOIN b USING(x) JOIN c USING(y)`), the
//! `SELECT *` column ORDER emits merged columns in first-seen order
//! rather than PG's strictly-nested order. Unqualified reference merge is
//! exact for any depth; only wildcard ordering of mixed-column chains
//! differs. Single-join (the common case, and every promoted differential)
//! is byte-exact.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_sql::ast::{
    BinOp, ColumnName, Expr, JoinKind, Literal, SelectItem, SelectStatement, TableRef,
};

use crate::{Engine, EngineError};

impl Engine {
    /// Resolve `NATURAL` / `USING` joins in `stmt`'s FROM clause into an
    /// equivalent statement with the column-merge applied. Returns
    /// `Ok(None)` when no join needs it (the fast path — no allocation),
    /// or a fully-rewritten owned statement the regular executor runs.
    pub(crate) fn desugar_using_natural(
        &self,
        stmt: &SelectStatement,
    ) -> Result<Option<SelectStatement>, EngineError> {
        let Some(from) = &stmt.from else {
            return Ok(None);
        };
        if !from
            .joins
            .iter()
            .any(|j| j.natural || j.using_cols.is_some())
        {
            return Ok(None);
        }

        // Column-name lists for [primary, peer0, peer1, …]. `None` for a
        // non-catalog source (derived table / unnest / generate_series /
        // jsonb_each) whose schema is not resolvable here.
        let mut quals: Vec<String> = Vec::with_capacity(from.joins.len() + 1);
        let mut col_names: Vec<Option<Vec<String>>> = Vec::with_capacity(from.joins.len() + 1);
        {
            let (q, c) = self.table_qual_and_columns(&from.primary);
            quals.push(q);
            col_names.push(c);
        }
        for j in &from.joins {
            let (q, c) = self.table_qual_and_columns(&j.table);
            quals.push(q);
            col_names.push(c);
        }

        // A NATURAL join needs both sides' schemas to find the common
        // columns; a non-catalog source there is unsupported.
        for (i, j) in from.joins.iter().enumerate() {
            if j.natural
                && (col_names[0..=i].iter().any(Option::is_none) || col_names[i + 1].is_none())
            {
                return Err(EngineError::Unsupported(
                    "NATURAL JOIN requires both inputs to be tables with a known schema"
                        .to_string(),
                ));
            }
        }

        // Per-merged-column pick expression, and the merged-name order.
        let mut pick: BTreeMap<String, Expr> = BTreeMap::new();
        let mut merged_order: Vec<String> = Vec::new();
        let mut merged_set: BTreeSet<String> = BTreeSet::new();
        // Leftmost qualifier that owns a given (unqualified) column name.
        let mut left_first_qual: BTreeMap<String, String> = BTreeMap::new();
        // Visible left column names in first-seen order (for NATURAL).
        let mut left_ordered: Vec<String> = Vec::new();

        let register_left = |name: &str,
                             qual: &str,
                             left_first_qual: &mut BTreeMap<String, String>,
                             left_ordered: &mut Vec<String>| {
            if !left_first_qual.contains_key(name) {
                left_first_qual.insert(name.to_string(), qual.to_string());
                left_ordered.push(name.to_string());
            }
        };

        // Seed with the primary's columns.
        if let Some(cols) = &col_names[0] {
            for c in cols {
                register_left(c, &quals[0], &mut left_first_qual, &mut left_ordered);
            }
        }

        // Build the rewritten FROM in lock-step.
        let mut new_from = from.clone();
        for (i, join) in new_from.joins.iter_mut().enumerate() {
            let peer_qual = quals[i + 1].clone();
            let is_merge = join.natural || join.using_cols.is_some();

            // Effective merge column set for this join.
            let effective: Vec<String> = if let Some(uc) = &join.using_cols {
                uc.clone()
            } else if join.natural {
                // Common column names, in the LEFT relation's order.
                let peer_cols = col_names[i + 1].as_ref().expect("checked above");
                let peer_set: BTreeSet<&String> = peer_cols.iter().collect();
                left_ordered
                    .iter()
                    .filter(|n| peer_set.contains(*n))
                    .cloned()
                    .collect()
            } else {
                Vec::new()
            };

            if is_merge {
                // Synthesise the pick expression + ON predicate.
                let mut on_acc: Option<Expr> = None;
                for c in &effective {
                    let left_qual = left_first_qual
                        .get(c)
                        .cloned()
                        .unwrap_or_else(|| quals[0].clone());
                    let left_ref = col_ref(&left_qual, c);
                    let right_ref = col_ref(&peer_qual, c);
                    let merged_expr = match join.kind {
                        JoinKind::Right => right_ref.clone(),
                        JoinKind::FullOuter => Expr::FunctionCall {
                            name: "coalesce".to_string(),
                            args: alloc::vec![left_ref.clone(), right_ref.clone()],
                        },
                        // Inner / Left / Cross(unreachable for merge)
                        _ => left_ref.clone(),
                    };
                    if !merged_set.contains(c) {
                        merged_set.insert(c.clone());
                        merged_order.push(c.clone());
                        pick.insert(c.clone(), merged_expr);
                    }
                    let eq = Expr::Binary {
                        lhs: alloc::boxed::Box::new(left_ref),
                        op: BinOp::Eq,
                        rhs: alloc::boxed::Box::new(right_ref),
                    };
                    on_acc = Some(match on_acc {
                        None => eq,
                        Some(acc) => Expr::Binary {
                            lhs: alloc::boxed::Box::new(acc),
                            op: BinOp::And,
                            rhs: alloc::boxed::Box::new(eq),
                        },
                    });
                }

                // A NATURAL join needs its ON built here (explicit USING
                // already has an equivalent ON from the parser — keep it).
                if join.natural {
                    if effective.is_empty() {
                        // No common columns → PG treats it as a CROSS join.
                        if join.kind == JoinKind::Inner {
                            join.kind = JoinKind::Cross;
                            join.on = None;
                        } else {
                            join.on = Some(Expr::Literal(Literal::Bool(true)));
                        }
                    } else {
                        join.on = on_acc;
                    }
                }
            }

            // Register this peer's columns as now-visible on the left, so a
            // later join can find them (merged names already registered
            // keep their leftmost qualifier).
            if let Some(cols) = &col_names[i + 1] {
                for c in cols {
                    register_left(c, &peer_qual, &mut left_first_qual, &mut left_ordered);
                }
            }

            // Clear the flags so a re-entrant call is a no-op.
            join.natural = false;
            join.using_cols = None;
        }

        // Nothing actually merged (e.g. every NATURAL had no common cols).
        // Still return the rewritten FROM so the synthesised ON / Cross
        // downgrade takes effect.
        let mut new_stmt = stmt.clone();
        new_stmt.from = Some(new_from);

        if !pick.is_empty() {
            // Rewrite unqualified merged-column references everywhere the
            // outer query binds against the join schema.
            for item in &mut new_stmt.items {
                if let SelectItem::Expr { expr, .. } = item {
                    rewrite_unqualified(expr, &pick);
                }
            }
            if let Some(w) = &mut new_stmt.where_ {
                rewrite_unqualified(w, &pick);
            }
            if let Some(g) = &mut new_stmt.group_by {
                for e in g {
                    rewrite_unqualified(e, &pick);
                }
            }
            if let Some(h) = &mut new_stmt.having {
                rewrite_unqualified(h, &pick);
            }
            for ob in &mut new_stmt.order_by {
                rewrite_unqualified(&mut ob.expr, &pick);
            }
            for e in &mut new_stmt.distinct_on {
                rewrite_unqualified(e, &pick);
            }

            // Expand `SELECT *` with the PG column-merge shape.
            expand_wildcards(
                &mut new_stmt.items,
                &merged_order,
                &pick,
                &merged_set,
                &quals,
                &col_names,
            )?;
        }

        Ok(Some(new_stmt))
    }

    /// The qualifier a column reference would use for this table (alias if
    /// present, else the table name) plus its ordered column-name list
    /// (`None` for a non-catalog source).
    fn table_qual_and_columns(&self, tref: &TableRef) -> (String, Option<Vec<String>>) {
        let qual = tref.alias.clone().unwrap_or_else(|| tref.name.clone());
        let is_non_catalog = tref.unnest_expr.is_some()
            || tref.lateral_subquery.is_some()
            || tref.generate_series_args.is_some()
            || tref.jsonb_each_text_arg.is_some();
        if is_non_catalog {
            return (qual, None);
        }
        let cols = self
            .active_catalog()
            .get(&tref.name)
            .map(|t| t.schema().columns.iter().map(|c| c.name.clone()).collect());
        (qual, cols)
    }
}

/// Build a qualified column reference `qual.name`.
fn col_ref(qual: &str, name: &str) -> Expr {
    Expr::Column(ColumnName {
        qualifier: Some(qual.to_string()),
        name: name.to_string(),
    })
}

/// Replace every UNQUALIFIED column reference whose name is a merged
/// column with its pick expression. Qualified references (`t.c`) are left
/// alone — the physical columns stay addressable. Does NOT descend into
/// subqueries: an unqualified name inside a subquery binds to the
/// subquery's own scope first (PG scoping), so rewriting it here would be
/// wrong.
fn rewrite_unqualified(e: &mut Expr, pick: &BTreeMap<String, Expr>) {
    match e {
        Expr::NamedArg { expr, .. } => rewrite_unqualified(expr, pick),
        Expr::Variadic(expr) => rewrite_unqualified(expr, pick),
        Expr::Column(c) => {
            if c.qualifier.is_none()
                && let Some(replacement) = pick.get(&c.name)
            {
                *e = replacement.clone();
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_unqualified(lhs, pick);
            rewrite_unqualified(rhs, pick);
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::FieldAccess { base: expr, .. }
        | Expr::Extract { source: expr, .. } => rewrite_unqualified(expr, pick),
        Expr::FunctionCall { args, .. } => {
            for a in args {
                rewrite_unqualified(a, pick);
            }
        }
        Expr::AggregateOrdered {
            call,
            order_by,
            filter,
            ..
        } => {
            rewrite_unqualified(call, pick);
            for ob in order_by {
                rewrite_unqualified(&mut ob.expr, pick);
            }
            if let Some(f) = filter {
                rewrite_unqualified(f, pick);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            rewrite_unqualified(expr, pick);
            rewrite_unqualified(pattern, pick);
        }
        Expr::Array(items) => {
            for it in items {
                rewrite_unqualified(it, pick);
            }
        }
        Expr::ArraySubscript { target, index } => {
            rewrite_unqualified(target, pick);
            rewrite_unqualified(index, pick);
        }
        Expr::ArraySlice { target, lo, hi } => {
            rewrite_unqualified(target, pick);
            if let Some(l) = lo {
                rewrite_unqualified(l, pick);
            }
            if let Some(h) = hi {
                rewrite_unqualified(h, pick);
            }
        }
        Expr::AnyAll { expr, array, .. } => {
            rewrite_unqualified(expr, pick);
            rewrite_unqualified(array, pick);
        }
        Expr::InList { expr, list, .. } => {
            rewrite_unqualified(expr, pick);
            for it in list {
                rewrite_unqualified(it, pick);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                rewrite_unqualified(o, pick);
            }
            for (w, t) in branches {
                rewrite_unqualified(w, pick);
                rewrite_unqualified(t, pick);
            }
            if let Some(el) = else_branch {
                rewrite_unqualified(el, pick);
            }
        }
        // WindowFunction: rewrite args + partition/order keys (still bind
        // against the join schema). Frame bounds are literals.
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                rewrite_unqualified(a, pick);
            }
            for p in partition_by {
                rewrite_unqualified(p, pick);
            }
            for (oe, _, _) in order_by {
                rewrite_unqualified(oe, pick);
            }
        }
        // Subquery boundaries — do not descend (inner scope wins).
        Expr::ScalarSubquery(_)
        | Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::RowInSubquery { .. }
        | Expr::RowCmpSubquery { .. }
        | Expr::Literal(_)
        | Expr::Placeholder(_) => {}
    }
}

/// Expand any `SELECT *` item into the PG column-merge shape: the merged
/// columns first (as their pick expressions, bare-named), then each
/// table's non-merged columns as qualified items with bare output names.
fn expand_wildcards(
    items: &mut Vec<SelectItem>,
    merged_order: &[String],
    pick: &BTreeMap<String, Expr>,
    merged_set: &BTreeSet<String>,
    quals: &[String],
    col_names: &[Option<Vec<String>>],
) -> Result<(), EngineError> {
    if !items.iter().any(|i| matches!(i, SelectItem::Wildcard)) {
        return Ok(());
    }
    // All involved tables must have known schemas to expand `*`.
    if col_names.iter().any(Option::is_none) {
        return Err(EngineError::Unsupported(
            "SELECT * over a USING/NATURAL join requires table sources with known schemas"
                .to_string(),
        ));
    }
    let mut expanded: Vec<SelectItem> = Vec::new();
    // Merged columns first.
    for name in merged_order {
        expanded.push(SelectItem::Expr {
            expr: pick.get(name).expect("merged pick").clone(),
            alias: Some(name.clone()),
        });
    }
    // Then each table's non-merged columns, left to right.
    for (t, cols) in col_names.iter().enumerate() {
        let cols = cols.as_ref().expect("checked above");
        for c in cols {
            if merged_set.contains(c) {
                continue;
            }
            expanded.push(SelectItem::Expr {
                expr: col_ref(&quals[t], c),
                alias: Some(c.clone()),
            });
        }
    }
    // Splice: replace the FIRST wildcard with the expansion, drop any
    // further bare wildcards (a join `SELECT *, *` is degenerate; PG would
    // repeat, but the merge shape makes a second `*` meaningless here).
    let mut out: Vec<SelectItem> = Vec::with_capacity(items.len() + expanded.len());
    let mut spliced = false;
    for it in items.drain(..) {
        match it {
            SelectItem::Wildcard if !spliced => {
                out.extend(expanded.iter().cloned());
                spliced = true;
            }
            SelectItem::Wildcard => {}
            other @ (SelectItem::Expr { .. } | SelectItem::QualifiedWildcard(_)) => {
                out.push(other)
            }
        }
    }
    *items = out;
    Ok(())
}
