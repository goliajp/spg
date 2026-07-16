//! v7.37 — INNER JOIN to filter-pushdown fold.
//!
//! For shapes like
//!     FROM A JOIN B ON A.fk = B.pk WHERE B.cond AND <A-only preds>
//! where:
//!   * the join is INNER
//!   * the ON clause is a single equality on a UNIQUE/PK column of B
//!   * B's alias is referenced ONLY inside the ON eq and inner-only
//!     WHERE conjuncts (no projection / ORDER BY / GROUP BY / HAVING
//!     references B)
//! we execute the inner query `SELECT B.pk FROM B WHERE B.cond` once,
//! collect the key values, then rewrite the outer to
//!     FROM A WHERE A.fk IN (<keys>) AND <A-only preds>
//! and drop the JOIN entirely.
//!
//! Cuts the JOIN executor's per-row hash probe + tuple bookkeeping —
//! the mailrs Phase 1 `count_messages` (2.46 → 1.0 ms = PG-beating)
//! and `user_storage_usage` (6.48 → 1.07 ms = 2.3× faster than PG)
//! probes proved fold-vs-JOIN is the structural lever.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_sql::ast::{
    BinOp, ColumnName, Expr, FromClause, FromJoin, JoinKind, Literal, SelectItem, SelectStatement,
};
use spg_storage::Value;

use crate::cancel::CancelToken;
use crate::reorder::split_and_conjunctions;
use crate::{Engine, EngineError, QueryResult};

impl Engine {
    /// Try to fold INNER JOINs in `stmt` into IN-list predicates on
    /// the primary table. Returns `Some(rewritten)` when at least one
    /// join was folded; `None` when no join is eligible (caller
    /// continues with the regular JOIN executor).
    pub(crate) fn try_fold_inner_joins(
        &self,
        stmt: &SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<Option<SelectStatement>, EngineError> {
        let Some(from) = &stmt.from else {
            return Ok(None);
        };
        if from.joins.is_empty() {
            return Ok(None);
        }
        // Gate: all joins must be INNER. LEFT preserves NULL rows on
        // an outer-side miss; folding to `fk IN (...)` would silently
        // drop those rows. CROSS has no ON clause to read.
        for j in &from.joins {
            if !matches!(j.kind, JoinKind::Inner) {
                return Ok(None);
            }
        }
        // Gate: bail if any expression anywhere in the statement
        // touches an AST shape we don't model. Conservative: a
        // pull-up-produced `Expr::InList` over 6 k literal IDs from
        // `NOT EXISTS → NOT IN` materialisation triggered a 22 ms
        // regression on `not_exists_filter` despite the fold itself
        // producing a clean single-table SCAN; the post-fold WHERE
        // shape goes through the ORDER BY + LIMIT path with a much
        // heavier compiled-InSet than the JOIN executor needed. Hold
        // back on these cases until we have a proper cost model.
        // Concretely: only fold when the statement is "boring" —
        // SELECT / WHERE / aggregate over plain tables, no ORDER BY,
        // no LIMIT, no HAVING, no DISTINCT, no UNION.
        if !stmt.order_by.is_empty()
            || stmt.limit.is_some()
            || stmt.offset.is_some()
            || stmt.having.is_some()
            || stmt.distinct
            || !stmt.unions.is_empty()
            || stmt.limit_with_ties
        {
            return Ok(None);
        }
        if statement_has_unresolved_subquery(stmt) {
            return Ok(None);
        }
        // Gate: every joined table must be a real catalog table —
        // not a LATERAL subquery, UNNEST, generate_series, or
        // AS-OF-SEGMENT scan. The fold path runs a `SELECT col FROM
        // <name>` against the live engine; the FROM primary already
        // dispatches the special forms separately.
        for j in &from.joins {
            if j.table.lateral_subquery.is_some()
                || j.table.unnest_expr.is_some()
                || j.table.generate_series_args.is_some()
                || j.table.as_of_segment.is_some()
            {
                return Ok(None);
            }
        }
        if from.primary.lateral_subquery.is_some()
            || from.primary.unnest_expr.is_some()
            || from.primary.generate_series_args.is_some()
            || from.primary.as_of_segment.is_some()
        {
            return Ok(None);
        }
        // Build alias -> join-index map. 0 = primary, 1..n = joins[0..n-1].
        let primary_alias = table_alias(&from.primary).to_string();
        let mut alias_to_idx: BTreeMap<String, usize> = BTreeMap::new();
        alias_to_idx.insert(primary_alias.clone(), 0);
        for (i, j) in from.joins.iter().enumerate() {
            let a = table_alias(&j.table).to_string();
            // Duplicate alias = ambiguous WHERE binding; bail.
            if alias_to_idx.insert(a, i + 1).is_some() {
                return Ok(None);
            }
        }
        // Identify which joins are foldable. A join is foldable when:
        //  * ON = `outer_alias.X = inner_alias.Y` (or swapped); the
        //    column qualified by the inner alias is single-column
        //    UNIQUE / PK on the inner table.
        //  * The inner alias does NOT appear in: SELECT items,
        //    ORDER BY, GROUP BY, HAVING, the ON clauses of joins
        //    OTHER than this one, or any WHERE conjunct that also
        //    references a non-inner alias.
        let mut planned: Vec<FoldPlan> = Vec::new();
        let mut folded_join_indices: Vec<usize> = Vec::new();
        for (i, j) in from.joins.iter().enumerate() {
            let Some(on) = j.on.as_ref() else {
                continue;
            };
            let inner_alias = table_alias(&j.table);
            let Some((outer_expr, inner_col_name)) = analyse_on_eq(on, inner_alias, &alias_to_idx)
            else {
                continue;
            };
            if !self.column_is_single_unique_public(&j.table.name, &inner_col_name) {
                continue;
            }
            // Reject if the inner alias is referenced outside the
            // foldable surface (ON eq + inner-only WHERE conjuncts).
            if alias_referenced_elsewhere(stmt, &from.joins, i, inner_alias) {
                continue;
            }
            // Inner-only WHERE conjuncts get rerooted inside the
            // inner query.
            let inner_preds: Vec<Expr> = if let Some(w) = &stmt.where_ {
                split_and_conjunctions(w)
                    .into_iter()
                    .filter(|p| {
                        let refs = referenced_aliases(p, &alias_to_idx);
                        refs.len() == 1 && refs.iter().next().copied() == Some(i + 1)
                    })
                    .cloned()
                    .collect()
            } else {
                Vec::new()
            };
            // Gate: only fold when there's at least one inner-only
            // WHERE predicate. Without one, the inner query returns
            // every row of the inner table — and the rewritten
            // `outer.fk IN (every_pk)` is just as expensive as the
            // JOIN walk it replaced (often more — building a
            // 25 k-element BTreeSet then probing it per outer row
            // beats a 1-row hash probe by a lot). The pull-up paths
            // (EXISTS sublinks, scalar subquery → LEFT JOIN) produce
            // exactly this shape (semi-joins on FK-PK pairs with no
            // intrinsic filter on the inner side), so the gate cuts
            // them off cleanly.
            if inner_preds.is_empty() {
                continue;
            }
            // Build & run: SELECT B.<pk> FROM B [WHERE inner_preds].
            // The eligible inner_preds reference inner_alias only;
            // re-qualify into B's intrinsic column names so the
            // probe scans against the table directly (the alias is
            // dropped from the inner FROM).
            let pk_values = match self.collect_unique_keys(
                &j.table.name,
                inner_alias,
                &inner_col_name,
                &inner_preds,
                cancel,
            ) {
                Ok(v) => v,
                // Eval miss / unsupported predicate shape: skip
                // this fold, keep the JOIN.
                Err(_) => continue,
            };
            // Eligible. Record the rewrite.
            planned.push(FoldPlan {
                join_idx: i,
                outer_expr,
                pk_values,
            });
            folded_join_indices.push(i);
        }
        if planned.is_empty() {
            return Ok(None);
        }
        // Gate: only fold when EVERY join in the statement is being
        // folded away. A partial fold (some joins folded, others
        // left) ends up running the JOIN executor over the leftover
        // joins WITH an `outer.fk IN (...)` predicate appended — and
        // the JOIN executor's per-row filter is the slow tree-walker
        // (subquery-bearing or post-pullup shapes always trip the
        // gate), so the leftover joins get worse, not better.
        // Better to defer to the regular JOIN executor entirely when
        // we can't fold everything.
        if folded_join_indices.len() != from.joins.len() {
            return Ok(None);
        }
        // Rewrite the statement.
        let mut new_stmt = stmt.clone();
        let new_from = build_folded_from(from, &folded_join_indices);
        new_stmt.from = Some(new_from);
        new_stmt.where_ = rewrite_where(stmt.where_.as_ref(), &alias_to_idx, &planned);
        Ok(Some(new_stmt))
    }
}

struct FoldPlan {
    join_idx: usize,
    outer_expr: Expr,
    pk_values: Vec<Value<'static>>,
}

impl Engine {
    /// v7.37.x (docker-fair LEFTJOIN 71 % attack) — eliminate LEFT
    /// JOINs whose right side is unreferenced everywhere except the
    /// ON equality, and whose ON equality is on a UNIQUE/PK column of
    /// the right table. Such a join preserves outer cardinality
    /// exactly and contributes no value used downstream, so it can
    /// be dropped wholesale. PG's planner applies the same rewrite
    /// for canonical shapes like
    ///     SELECT COUNT(*) FROM A LEFT JOIN B ON B.pk = A.fk
    /// — A's row count is the result; B never has to be touched.
    ///
    /// Conservative: requires no aliases to be duplicated, no
    /// LATERAL/UNNEST/AS-OF-SEGMENT special FROM shapes, and the
    /// right side's join key resolved against the live catalog.
    /// Returns `Some(rewritten)` if at least one join was eliminated;
    /// `None` otherwise.
    pub(crate) fn try_eliminate_redundant_left_joins(
        &self,
        stmt: &SelectStatement,
    ) -> Option<SelectStatement> {
        let from = stmt.from.as_ref()?;
        if from.joins.is_empty() {
            return None;
        }
        // Quick gate: at least one LEFT JOIN must exist.
        if !from.joins.iter().any(|j| matches!(j.kind, JoinKind::Left)) {
            return None;
        }
        // Reject special-shape FROM primaries / joined tables — the
        // alias/column resolution below doesn't model these.
        if from.primary.lateral_subquery.is_some()
            || from.primary.unnest_expr.is_some()
            || from.primary.generate_series_args.is_some()
            || from.primary.as_of_segment.is_some()
        {
            return None;
        }
        for j in &from.joins {
            if j.table.lateral_subquery.is_some()
                || j.table.unnest_expr.is_some()
                || j.table.generate_series_args.is_some()
                || j.table.as_of_segment.is_some()
            {
                return None;
            }
        }
        let primary_alias = table_alias(&from.primary).to_string();
        let mut alias_to_idx: BTreeMap<String, usize> = BTreeMap::new();
        alias_to_idx.insert(primary_alias.clone(), 0);
        for (i, j) in from.joins.iter().enumerate() {
            let a = table_alias(&j.table).to_string();
            if alias_to_idx.insert(a, i + 1).is_some() {
                return None;
            }
        }
        // Identify eligible LEFT joins to eliminate.
        let mut drop_indices: Vec<usize> = Vec::new();
        for (i, j) in from.joins.iter().enumerate() {
            if !matches!(j.kind, JoinKind::Left) {
                continue;
            }
            let on = j.on.as_ref()?;
            let inner_alias = table_alias(&j.table);
            // Reuse the INNER-fold's ON-equality analyser. It rejects
            // anything but a single equality with one column qualified
            // by the inner alias.
            let (_outer_expr, inner_col_name) = analyse_on_eq(on, inner_alias, &alias_to_idx)?;
            // The inner join key must be UNIQUE/PK on the inner table;
            // otherwise LEFT JOIN can multiply outer rows.
            if !self.column_is_single_unique_public(&j.table.name, &inner_col_name) {
                continue;
            }
            // The inner alias must be referenced nowhere except this
            // join's ON clause. Stricter than the INNER-fold variant:
            // inner-only WHERE conjuncts CAN'T be re-applied after a
            // LEFT JOIN elimination (no IN-list seed sub-SELECT exists
            // here), and any IS-NULL anti-join pattern
            // (`__exsj_0.message_id IS NULL` from v7.37.27 NOT EXISTS
            // pull-up) MUST veto elimination.
            if alias_referenced_strictly_elsewhere(stmt, &from.joins, i, inner_alias) {
                continue;
            }
            drop_indices.push(i);
        }
        if drop_indices.is_empty() {
            return None;
        }
        let new_from = build_folded_from(from, &drop_indices);
        let mut new_stmt = stmt.clone();
        new_stmt.from = Some(new_from);
        Some(new_stmt)
    }
}

/// True when any expression anywhere in the statement still carries
/// an unresolved subquery node. We bail in this case because the fold
/// recurses into `exec_bare_select_cancel`, which does NOT re-run
/// `resolve_select_subqueries` — leaving the subquery to be walked
/// per row.
fn statement_has_unresolved_subquery(stmt: &SelectStatement) -> bool {
    let mut hit = false;
    let mut visit = |e: &Expr| {
        walk_expr(e, &mut |item| match item {
            WalkItem::Subquery(_) | WalkItem::Unknown => hit = true,
            WalkItem::Column(_) => {}
        });
    };
    if let Some(w) = &stmt.where_ {
        visit(w);
    }
    for it in &stmt.items {
        if let SelectItem::Expr { expr, .. } = it {
            visit(expr);
        }
    }
    for o in &stmt.order_by {
        visit(&o.expr);
    }
    if let Some(g) = &stmt.group_by {
        for e in g {
            visit(e);
        }
    }
    if let Some(h) = &stmt.having {
        visit(h);
    }
    if let Some(from) = &stmt.from {
        for j in &from.joins {
            if let Some(on) = &j.on {
                visit(on);
            }
        }
    }
    hit
}

fn table_alias(t: &spg_sql::ast::TableRef) -> &str {
    t.alias.as_deref().unwrap_or(&t.name)
}

/// `outer.X = inner_alias.Y` (or swapped) → `(outer_expr, "Y")`.
fn analyse_on_eq(
    on: &Expr,
    inner_alias: &str,
    alias_to_idx: &BTreeMap<String, usize>,
) -> Option<(Expr, String)> {
    let Expr::Binary {
        op: BinOp::Eq,
        lhs,
        rhs,
    } = on
    else {
        return None;
    };
    let (outer_side, inner_side) = if is_qualified_with(lhs, inner_alias) {
        (rhs.as_ref(), lhs.as_ref())
    } else if is_qualified_with(rhs, inner_alias) {
        (lhs.as_ref(), rhs.as_ref())
    } else {
        return None;
    };
    let Expr::Column(ColumnName {
        qualifier: _,
        name: inner_col,
    }) = inner_side
    else {
        return None;
    };
    // Outer side must reference some known alias OTHER than the inner.
    let refs = referenced_aliases(outer_side, alias_to_idx);
    let inner_idx = *alias_to_idx.get(inner_alias)?;
    if refs.contains(&inner_idx) {
        return None;
    }
    if refs.is_empty() {
        return None;
    }
    Some((outer_side.clone(), inner_col.clone()))
}

fn is_qualified_with(e: &Expr, alias: &str) -> bool {
    matches!(
        e,
        Expr::Column(ColumnName {
            qualifier: Some(q),
            ..
        }) if q == alias
    )
}

/// Collect the join-index of every aliased column reference in `e`.
/// Subqueries / unqualified columns make the set "unknown" — we
/// return `{0..n}` (worst-case) so eligibility gates reject the
/// expression conservatively.
fn referenced_aliases(
    e: &Expr,
    alias_to_idx: &BTreeMap<String, usize>,
) -> alloc::collections::BTreeSet<usize> {
    let mut state = WalkState {
        out: alloc::collections::BTreeSet::new(),
        unknown: false,
    };
    walk_expr(e, &mut |kind| match kind {
        WalkItem::Column(c) => match &c.qualifier {
            Some(q) => match alias_to_idx.get(q) {
                Some(&i) => {
                    state.out.insert(i);
                }
                None => state.unknown = true,
            },
            None => state.unknown = true,
        },
        WalkItem::Subquery(_) | WalkItem::Unknown => state.unknown = true,
    });
    if state.unknown {
        state.out.clear();
        for i in 0..alias_to_idx.len() {
            state.out.insert(i);
        }
    }
    state.out
}

struct WalkState {
    out: alloc::collections::BTreeSet<usize>,
    unknown: bool,
}

enum WalkItem<'a> {
    Column(&'a ColumnName),
    Subquery(&'a SelectStatement),
    /// An AST variant the walker doesn't model — caller treats as
    /// "could touch anything".
    Unknown,
}

fn walk_expr<F>(e: &Expr, f: &mut F)
where
    F: FnMut(WalkItem<'_>),
{
    match e {
        Expr::Column(c) => f(WalkItem::Column(c)),
        Expr::Literal(_) | Expr::Placeholder(_) => {}
        Expr::Binary { lhs, rhs, .. } => {
            walk_expr(lhs, f);
            walk_expr(rhs, f);
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => walk_expr(expr, f),
        Expr::FunctionCall { args, .. } => {
            for a in args {
                walk_expr(a, f);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            walk_expr(expr, f);
            walk_expr(pattern, f);
        }
        Expr::InList { expr: e2, list, .. } => {
            walk_expr(e2, f);
            for li in list {
                walk_expr(li, f);
            }
        }
        Expr::ScalarSubquery(s) => f(WalkItem::Subquery(s)),
        Expr::Exists { subquery, .. } => f(WalkItem::Subquery(subquery)),
        Expr::InSubquery {
            expr: e2, subquery, ..
        } => {
            walk_expr(e2, f);
            f(WalkItem::Subquery(subquery));
        }
        // Anything else we don't model — conservatively unknown.
        _ => f(WalkItem::Unknown),
    }
}

/// True when `alias` appears anywhere outside `joins[skip_idx].on` and
/// inner-only WHERE conjuncts. Conservative: if any expr we can't
/// walk references something, we say yes.
fn alias_referenced_elsewhere(
    stmt: &SelectStatement,
    joins: &[FromJoin],
    skip_idx: usize,
    alias: &str,
) -> bool {
    // SELECT items.
    for it in &stmt.items {
        match it {
            SelectItem::Wildcard => continue,
            // `q.*` references alias `q`; a wildcard for another table does not.
            SelectItem::QualifiedWildcard(q) => {
                if q.eq_ignore_ascii_case(alias) {
                    return true;
                }
            }
            SelectItem::Expr { expr, .. } => {
                if expr_references_alias(expr, alias) {
                    return true;
                }
            }
        }
    }
    // ORDER BY / GROUP BY / HAVING.
    for o in &stmt.order_by {
        if expr_references_alias(&o.expr, alias) {
            return true;
        }
    }
    if let Some(g) = &stmt.group_by {
        for e in g {
            if expr_references_alias(e, alias) {
                return true;
            }
        }
    }
    if let Some(h) = &stmt.having
        && expr_references_alias(h, alias)
    {
        return true;
    }
    // Other joins' ON clauses.
    for (i, j) in joins.iter().enumerate() {
        if i == skip_idx {
            continue;
        }
        if let Some(on) = &j.on
            && expr_references_alias(on, alias)
        {
            return true;
        }
    }
    // WHERE: every conjunct that touches `alias` must touch ONLY
    // `alias`. Mixed conjuncts (e.g. `mb.x = m.y`) make the fold
    // unsound — those are real join-correlated preds.
    if let Some(w) = &stmt.where_ {
        for p in split_and_conjunctions(w) {
            let touches_alias = expr_references_alias(p, alias);
            if !touches_alias {
                continue;
            }
            // Touches alias — must touch ONLY alias.
            if expr_references_any_other_alias(p, alias) {
                return true;
            }
        }
    }
    false
}

/// Stricter variant of `alias_referenced_elsewhere` for LEFT-JOIN
/// elimination: WHERE conjuncts that touch the inner alias AT ALL
/// veto elimination (e.g. anti-join `inner.k IS NULL` patterns), not
/// just mixed-alias ones. Inner-only WHERE conjuncts can be re-applied
/// after INNER-fold via the IN-list seed sub-SELECT; LEFT-JOIN
/// elimination has no equivalent place to put them.
pub(crate) fn alias_referenced_strictly_elsewhere(
    stmt: &SelectStatement,
    joins: &[FromJoin],
    skip_idx: usize,
    alias: &str,
) -> bool {
    if alias_referenced_elsewhere(stmt, joins, skip_idx, alias) {
        return true;
    }
    if let Some(w) = &stmt.where_ {
        for p in split_and_conjunctions(w) {
            if expr_references_alias(p, alias) {
                return true;
            }
        }
    }
    false
}

/// True if `e` mentions `alias.col` anywhere or contains a Subquery/
/// Unknown node we conservatively treat as referencing everything.
///
/// Visibility: `pub(crate)` since v7.37.6 — `join.rs::
/// try_streamed_inner_join_walk_topn` reuses this to classify outer-only
/// vs cross-alias WHERE conjuncts and short-circuit before materialising
/// the combined join row.
pub(crate) fn expr_references_alias(e: &Expr, alias: &str) -> bool {
    let mut hit = false;
    walk_expr(e, &mut |item| match item {
        WalkItem::Column(c) => {
            if c.qualifier.as_deref() == Some(alias) {
                hit = true;
            }
        }
        // Subquery / unknown: conservative.
        WalkItem::Subquery(_) | WalkItem::Unknown => hit = true,
    });
    hit
}

/// True if `e` mentions an alias other than `alias`, OR contains a
/// Subquery/Unknown node we conservatively treat as referencing
/// everything. v7.37.6: promoted to `pub(crate)` for join.rs reuse.
pub(crate) fn expr_references_any_other_alias(e: &Expr, alias: &str) -> bool {
    let mut other = false;
    walk_expr(e, &mut |item| match item {
        WalkItem::Column(c) => {
            if let Some(q) = &c.qualifier
                && q != alias
            {
                other = true;
            }
        }
        WalkItem::Subquery(_) | WalkItem::Unknown => other = true,
    });
    other
}

/// Build the new FROM with all `drop_indices` joins removed.
fn build_folded_from(from: &FromClause, drop_indices: &[usize]) -> FromClause {
    let drop: alloc::collections::BTreeSet<usize> = drop_indices.iter().copied().collect();
    let kept: Vec<FromJoin> = from
        .joins
        .iter()
        .enumerate()
        .filter_map(|(i, j)| {
            if drop.contains(&i) {
                None
            } else {
                Some(j.clone())
            }
        })
        .collect();
    FromClause {
        primary: from.primary.clone(),
        joins: kept,
    }
}

/// Rebuild WHERE: drop conjuncts referencing any folded inner alias
/// (they've been consumed by the eager inner query); keep the rest;
/// append `outer_expr IN (pk_values)` for each fold.
fn rewrite_where(
    original: Option<&Expr>,
    alias_to_idx: &BTreeMap<String, usize>,
    plans: &[FoldPlan],
) -> Option<Expr> {
    let folded_idx: alloc::collections::BTreeSet<usize> =
        plans.iter().map(|p| p.join_idx + 1).collect();
    let mut kept: Vec<Expr> = Vec::new();
    if let Some(w) = original {
        for p in split_and_conjunctions(w) {
            let refs = referenced_aliases(p, alias_to_idx);
            if refs.iter().any(|i| folded_idx.contains(i)) {
                continue;
            }
            kept.push(p.clone());
        }
    }
    for p in plans {
        // `outer_expr IN (literals)` — empty list folds to FALSE
        // (i.e. `outer IS NOT NULL AND FALSE`); use a sentinel
        // Literal::Bool(false) so the downstream optimiser short-
        // circuits the whole query without an empty-IN special case.
        if p.pk_values.is_empty() {
            kept.push(Expr::Literal(Literal::Bool(false)));
            continue;
        }
        let list: Vec<Expr> = p
            .pk_values
            .iter()
            .map(|v| Expr::Literal(value_to_literal(v)))
            .collect();
        kept.push(Expr::InList {
            expr: alloc::boxed::Box::new(p.outer_expr.clone()),
            list,
            negated: false,
        });
    }
    if kept.is_empty() {
        return None;
    }
    let mut iter = kept.into_iter();
    let mut acc = iter.next().expect("kept non-empty");
    for e in iter {
        acc = Expr::Binary {
            lhs: alloc::boxed::Box::new(acc),
            op: BinOp::And,
            rhs: alloc::boxed::Box::new(e),
        };
    }
    Some(acc)
}

fn value_to_literal(v: &Value) -> Literal {
    match v {
        Value::SmallInt(n) => Literal::Integer(i64::from(*n)),
        Value::Int(n) => Literal::Integer(i64::from(*n)),
        Value::BigInt(n) => Literal::Integer(*n),
        Value::Bool(b) => Literal::Bool(*b),
        Value::Float(x) => Literal::Float(*x),
        Value::Text(s) => Literal::String(s.to_string()),
        _ => Literal::Null,
    }
}

impl Engine {
    /// Public surface for `crate::joinfold` over the private
    /// `column_is_single_unique` (which lives on a different impl).
    fn column_is_single_unique_public(&self, table: &str, col: &str) -> bool {
        let Some(t) = self.active_catalog().get(table) else {
            return false;
        };
        let sch = t.schema();
        let Some(pos) = sch
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(col))
        else {
            return false;
        };
        if sch
            .uniqueness_constraints
            .iter()
            .any(|u| u.columns.as_slice() == [pos])
        {
            return true;
        }
        t.index_on(pos).is_some_and(|idx| idx.is_unique)
    }

    /// Run `SELECT alias.pk FROM table_name [AS alias] WHERE
    /// inner_preds` and return the materialised pk values. `alias`
    /// is the alias used in `inner_preds`; we use it as the FROM
    /// alias so the predicate's qualified columns resolve.
    fn collect_unique_keys(
        &self,
        table_name: &str,
        alias: &str,
        pk_col: &str,
        inner_preds: &[Expr],
        cancel: CancelToken<'_>,
    ) -> Result<Vec<Value<'static>>, EngineError> {
        let primary = spg_sql::ast::TableRef {
            name: table_name.to_string(),
            alias: Some(alias.to_string()),
            as_of_segment: None,
            unnest_expr: None,
            unnest_column_aliases: Vec::new(),
            with_ordinality: false,
            generate_series_args: None,
            lateral_subquery: None,
            jsonb_each_text_arg: None,
            table_fn_call: None,
            scalar_fn_item: false,
        rows_from: None,
        };
        let where_ = combine_with_and_owned(inner_preds);
        let pk_col_name = ColumnName {
            qualifier: Some(alias.to_string()),
            name: pk_col.to_string(),
        };
        let probe = SelectStatement {
            ctes: Vec::new(),
            distinct: false,
            distinct_on: Vec::new(),
            items: alloc::vec![SelectItem::Expr {
                expr: Expr::Column(pk_col_name),
                alias: None,
            }],
            from: Some(FromClause {
                primary,
                joins: Vec::new(),
            }),
            where_,
            group_by: None,
            having: None,
            unions: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            limit_with_ties: false,
            group_by_all: false,
        };
        let result = self.exec_bare_select_cancel(&probe, cancel)?;
        let QueryResult::Rows { rows, .. } = result else {
            return Err(EngineError::Unsupported(format!(
                "joinfold: inner probe returned non-Rows for {table_name}"
            )));
        };
        let mut out: Vec<Value<'static>> = Vec::with_capacity(rows.len());
        for r in rows {
            if let Some(v) = r.values.into_iter().next() {
                out.push(v);
            }
        }
        Ok(out)
    }
}

fn combine_with_and_owned(preds: &[Expr]) -> Option<Expr> {
    let mut iter = preds.iter();
    let mut acc = iter.next()?.clone();
    for e in iter {
        acc = Expr::Binary {
            lhs: alloc::boxed::Box::new(acc),
            op: BinOp::And,
            rhs: alloc::boxed::Box::new(e.clone()),
        };
    }
    Some(acc)
}
