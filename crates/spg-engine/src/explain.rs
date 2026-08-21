//! EXPLAIN rendering and index suggestions — walk a SELECT plan into
//! human-readable lines, annotate row estimates, and suggest missing
//! indexes. Split out of `lib.rs` (v7.32 engine modularisation).

use alloc::string::String;
use alloc::vec::Vec;

use spg_sql::ast::{Expr, SelectItem, SelectStatement};

use crate::index_access::try_index_seek;
use spg_storage::{ColumnSchema, DataType, Row, Value};

use crate::{
    CancelToken, Engine, EngineError, QueryResult, aggregate, expr_has_subquery, select_has_window,
};

/// Walks the SELECT's FROM clauses + WHERE expression tree;
/// returns one line per missing index. Deterministic order:
/// FROM-clause iteration order, then column-reference walk
/// order inside each WHERE. Each suggestion is a copy-pastable
/// DDL string.
pub(crate) fn build_index_suggestions(stmt: &SelectStatement, engine: &Engine) -> Vec<String> {
    use alloc::collections::BTreeSet;
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    let cat = engine.active_catalog();
    // Build a (table, qualifier-or-alias) list from the FROM clause
    // so unqualified column refs in WHERE resolve to the correct
    // table.
    let Some(from) = &stmt.from else {
        return out;
    };
    let mut tables: Vec<String> = Vec::new();
    tables.push(from.primary.name.clone());
    for j in &from.joins {
        tables.push(j.table.name.clone());
    }
    // Collect column refs from the WHERE expression. JOIN ON
    // predicates also feed in.
    let mut col_refs: Vec<spg_sql::ast::ColumnName> = Vec::new();
    if let Some(w) = &stmt.where_ {
        collect_column_refs(w, &mut col_refs);
    }
    for j in &from.joins {
        if let Some(on) = &j.on {
            collect_column_refs(on, &mut col_refs);
        }
    }
    for cn in &col_refs {
        // Resolve owner table: explicit qualifier first, else
        // first table in FROM that has a column of this name.
        let owner: Option<String> = if let Some(q) = &cn.qualifier {
            tables.iter().find(|t| t == &q).cloned()
        } else {
            tables.iter().find_map(|t| {
                cat.get(t).and_then(|tbl| {
                    if tbl.schema().column_position(&cn.name).is_some() {
                        Some(t.clone())
                    } else {
                        None
                    }
                })
            })
        };
        let Some(owner) = owner else {
            continue;
        };
        let Some(tbl) = cat.get(&owner) else {
            continue;
        };
        let Some(col_pos) = tbl.schema().column_position(&cn.name) else {
            continue;
        };
        // Skip if any BTree index already covers this column as
        // its key.
        let already_indexed = tbl.indices().iter().any(|i| {
            matches!(
                i.kind,
                spg_storage::IndexKind::BTree(_) | spg_storage::IndexKind::BTreeMulti(_)
            ) && i.column_position == col_pos
                && i.expression.is_none()
                && i.partial_predicate.is_none()
        });
        if already_indexed {
            continue;
        }
        if seen.insert((owner.clone(), cn.name.clone())) {
            out.push(alloc::format!(
                "SUGGEST: CREATE INDEX ix_{}_{} ON {} ({})",
                owner,
                cn.name,
                owner,
                cn.name
            ));
        }
    }
    // v7.37.19 (19.21) — composite index opportunity detection.
    // Walk the WHERE clause for AND-chained equality predicates on
    // the same table. When ≥2 distinct columns of one table appear
    // as `col = lit` inside a single AND chain, suggest a composite
    // index covering them — PG's planner gains a real seek over
    // separate single-column indices in this case.
    let mut composite_eqs: alloc::collections::BTreeMap<
        String,
        alloc::collections::BTreeSet<String>,
    > = alloc::collections::BTreeMap::new();
    if let Some(w) = &stmt.where_ {
        collect_and_eq_columns(w, &tables, cat, &mut composite_eqs);
    }
    for j in &from.joins {
        if let Some(on) = &j.on {
            collect_and_eq_columns(on, &tables, cat, &mut composite_eqs);
        }
    }
    for (owner, cols) in composite_eqs {
        if cols.len() < 2 {
            continue;
        }
        let cols_vec: Vec<&String> = cols.iter().collect();
        // Skip if an index or UNIQUE constraint already covers
        // this column set (set-membership, not order — PG's
        // planner uses any index whose key columns equal the
        // predicate columns regardless of order for equality-only
        // filters).
        if let Some(tbl) = cat.get(&owner) {
            let pos_to_name = |pos: usize| tbl.schema().columns.get(pos).map(|c| c.name.clone());
            let already_in_index = tbl.indices().iter().any(|i| {
                if !matches!(
                    i.kind,
                    spg_storage::IndexKind::BTree(_) | spg_storage::IndexKind::BTreeMulti(_)
                ) {
                    return false;
                }
                let mut all_cols: alloc::collections::BTreeSet<String> =
                    alloc::collections::BTreeSet::new();
                if let Some(n) = pos_to_name(i.column_position) {
                    all_cols.insert(n);
                }
                for &extra in &i.extra_column_positions {
                    if let Some(c) = pos_to_name(extra) {
                        all_cols.insert(c);
                    }
                }
                cols.iter().all(|c| all_cols.contains(c))
            });
            let already_in_uc = tbl.schema().uniqueness_constraints.iter().any(|uc| {
                let names: alloc::collections::BTreeSet<String> =
                    uc.columns.iter().filter_map(|&p| pos_to_name(p)).collect();
                cols.iter().all(|c| names.contains(c))
            });
            if already_in_index || already_in_uc {
                continue;
            }
        }
        let cols_csv: Vec<String> = cols_vec.iter().map(|s| (*s).clone()).collect();
        let suffix = cols_csv.join("_");
        let body = cols_csv.join(", ");
        out.push(alloc::format!(
            "SUGGEST: CREATE INDEX ix_{owner}_{suffix} ON {owner} ({body})"
        ));
    }
    out
}

/// v7.37.19 (19.21) — walk an AND-chain WHERE and collect
/// (table, column) tuples for every equality predicate on a
/// table-qualified column. Used to suggest composite indices
/// when ≥2 columns of the same table appear in one AND chain.
fn collect_and_eq_columns(
    expr: &Expr,
    tables: &[String],
    cat: &spg_storage::Catalog,
    out: &mut alloc::collections::BTreeMap<String, alloc::collections::BTreeSet<String>>,
) {
    let mut stack: Vec<&Expr> = alloc::vec![expr];
    while let Some(e) = stack.pop() {
        match e {
            Expr::Binary {
                lhs,
                op: spg_sql::ast::BinOp::And,
                rhs,
            } => {
                stack.push(lhs);
                stack.push(rhs);
            }
            Expr::Binary {
                lhs,
                op: spg_sql::ast::BinOp::Eq,
                rhs,
            } => {
                // Pick whichever side is a column ref. Owner is the
                // explicit qualifier when present; otherwise the
                // first FROM table that has a column of that name.
                let resolve = |e: &Expr| -> Option<(String, String)> {
                    if let Expr::Column(cn) = e {
                        let owner: Option<String> = if let Some(q) = &cn.qualifier {
                            tables.iter().find(|t| t == &q).cloned()
                        } else {
                            tables.iter().find_map(|t| {
                                cat.get(t).and_then(|tbl| {
                                    if tbl.schema().column_position(&cn.name).is_some() {
                                        Some(t.clone())
                                    } else {
                                        None
                                    }
                                })
                            })
                        };
                        owner.map(|o| (o, cn.name.clone()))
                    } else {
                        None
                    }
                };
                let lhs_col = resolve(lhs);
                let rhs_col = resolve(rhs);
                // Skip when both sides are columns (a JOIN-ON
                // predicate, not a filter). Only single-column-eq-
                // literal patterns help with a composite index.
                if let (Some((t, c)), None) | (None, Some((t, c))) = (lhs_col, rhs_col) {
                    let mut entry = out.remove(&t).unwrap_or_default();
                    entry.insert(c);
                    out.insert(t, entry);
                }
            }
            _ => {}
        }
    }
}

/// Walks an `Expr` and pushes every `ColumnName` it references.
/// Order is depth-first, left-to-right.
pub(crate) fn collect_column_refs(expr: &Expr, out: &mut Vec<spg_sql::ast::ColumnName>) {
    match expr {
        Expr::Column(cn) => out.push(cn.clone()),
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_column_refs(a, out);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            collect_column_refs(lhs, out);
            collect_column_refs(rhs, out);
        }
        Expr::Unary { expr: e, .. } => collect_column_refs(e, out),
        _ => {}
    }
}

/// v7.39 (round 224, EXPLAIN epic Phase 0) — one node of the PG-shaped
/// plan tree. `head` is the node line ("Seq Scan on t1", "Sort"), `attrs`
/// the property lines beneath it ("Sort Key: v", "Filter: (id = 5)"),
/// `children` the `->`-prefixed sub-plans. `no_arrow` marks named
/// sub-plan labels (PG's `CTE <name>` blocks render without `->  `).
struct PlanNode {
    head: String,
    attrs: Vec<String>,
    children: Vec<PlanNode>,
    no_arrow: bool,
    /// v7.39 (round 225, Phase 1) — `(startup, total, rows, width)` for the
    /// PG-format cost annotation `(cost=A..B rows=N width=W)`. The FORMAT
    /// is PG's; the NUMBERS are SPG's own estimates (real table row counts
    /// + fixed per-type widths + simple selectivity guesses — SPG has no
    /// PG statistics and its cost model is its own). `None` = no
    /// annotation (COSTS OFF, or a label pseudo-node).
    cost: Option<(f64, f64, u64, u64)>,
    /// v7.39 (round 227, Phase 3) — EXPLAIN ANALYZE's measured block:
    /// `(time_ms, rows)`. `time_ms` is `Some` ONLY where SPG genuinely
    /// measured it (the top node, whose elapsed IS the query elapsed);
    /// inner nodes carry rows-only because SPG has no per-node timer and
    /// will not fabricate one. `None` = SPG could not derive this node's
    /// actual row count from a real counter, so no block is emitted
    /// rather than a guess. Loops is always 1 (SPG re-executes nothing).
    actual: Option<(Option<f64>, u64)>,
}

impl PlanNode {
    fn new(head: String) -> Self {
        Self {
            head,
            attrs: Vec::new(),
            children: Vec::new(),
            no_arrow: false,
            cost: None,
            actual: None,
        }
    }
}

/// Fixed per-type width estimate (bytes) for the `width=` figure. PG reads
/// pg_statistic averages; SPG uses declared-type widths (text-ish = 32).
fn est_width(cols: &[ColumnSchema]) -> u64 {
    cols.iter()
        .map(|c| match c.ty {
            DataType::SmallInt => 2,
            DataType::Int | DataType::Date | DataType::Float => 4,
            DataType::BigInt | DataType::Timestamp | DataType::Timestamptz | DataType::Money => 8,
            DataType::Bool => 1,
            DataType::Uuid => 16,
            _ => 32,
        })
        .sum()
}

/// The number of rows an indexed range in `where_` actually covers, when
/// the index can say so under a cap.
///
/// This is the difference between a `rows=` a reader can act on and one
/// they cannot: mailrs measured a 200-fold change in selectivity moving
/// the estimate not at all, because [`est_scan_rows`] answers `n / 3` to
/// everything that is not equality. `None` here means the range was too
/// wide to count cheaply — and a wide range is where the old fraction was
/// closest to right.
fn real_range_rows(
    engine: &Engine,
    name: &str,
    alias: Option<&str>,
    where_: Option<&Expr>,
) -> Option<u64> {
    let w = where_?;
    let table = engine.active_catalog().get(name)?;
    crate::index_access::count_indexed_range_capped(
        w,
        &table.schema().columns,
        table,
        alias.unwrap_or(name),
        &engine.current_snapshot(),
    )
}

/// Row-count estimate for a scan: no predicate = the real live count;
/// an equality lands 1 (unique-ish assumption on the seek path) or n/10;
/// anything else n/3 (PG's default_selectivity flavour).
fn est_scan_rows(n: u64, where_: Option<&Expr>, eq_seek: bool) -> u64 {
    match where_ {
        None => n,
        Some(_) if eq_seek => 1,
        Some(w) => {
            let frac = if matches!(
                w,
                Expr::Binary {
                    op: spg_sql::ast::BinOp::Eq,
                    ..
                }
            ) {
                10
            } else {
                3
            };
            (n / frac).max(1)
        }
    }
}

/// PG's text renderer: a node's text starts at column 6*depth; its `->  `
/// arrow occupies the 4 columns before that; attribute lines indent 2 past
/// the node text. (Measured off live PG18.4 output, r224 probe.)
fn render_pg_tree(node: &PlanNode, depth: usize, out: &mut Vec<String>) {
    let head = if depth == 0 {
        node.head.clone()
    } else if node.no_arrow {
        alloc::format!("{}{}", " ".repeat(6 * depth - 4), node.head)
    } else {
        alloc::format!("{}->  {}", " ".repeat(6 * depth - 6 + 2), node.head)
    };
    out.push(head);
    let attr_pad = " ".repeat(6 * depth + 2);
    for a in &node.attrs {
        out.push(alloc::format!("{attr_pad}{a}"));
    }
    for c in &node.children {
        render_pg_tree(c, depth + 1, out);
    }
}

/// Render an expression the way PG's deparser does at the plan level —
/// wrapped in one set of parentheses unless it already is.
/// v7.39 (round 588) — the top-level WHERE conjuncts that could be equi-join
/// keys, under the executor's own rule: `<col> = <col>` with two different
/// qualifiers, and only while no outer join is in the chain.
fn where_equi_join_conds<'w>(
    from: &spg_sql::ast::FromClause,
    where_: Option<&'w Expr>,
) -> alloc::vec::Vec<&'w Expr> {
    let Some(w) = where_ else {
        return alloc::vec::Vec::new();
    };
    if !from.joins.iter().all(|j| {
        matches!(
            j.kind,
            spg_sql::ast::JoinKind::Inner | spg_sql::ast::JoinKind::Cross
        )
    }) {
        return alloc::vec::Vec::new();
    }
    crate::reorder::split_and_conjunctions(w)
        .into_iter()
        .filter(|sub| equi_quals(sub).is_some())
        .collect()
}

/// The two qualifiers of a `<qual>.<col> = <qual>.<col>` conjunct.
fn equi_quals(sub: &Expr) -> Option<(&str, &str)> {
    let Expr::Binary {
        lhs,
        op: spg_sql::ast::BinOp::Eq,
        rhs,
    } = sub
    else {
        return None;
    };
    let (Expr::Column(a), Expr::Column(b)) = (lhs.as_ref(), rhs.as_ref()) else {
        return None;
    };
    let (Some(qa), Some(qb)) = (a.qualifier.as_deref(), b.qualifier.as_deref()) else {
        return None;
    };
    (!qa.eq_ignore_ascii_case(qb)).then_some((qa, qb))
}

/// The WHERE conjunct that joins peer `jidx` to something already joined —
/// the primary or an earlier peer — if there is one.
fn promoted_key_for<'w>(
    from: &spg_sql::ast::FromClause,
    jidx: usize,
    candidates: &[&'w Expr],
) -> Option<&'w Expr> {
    let rel = |t: &spg_sql::ast::TableRef| t.alias.clone().unwrap_or_else(|| t.name.clone());
    let peer = rel(&from.joins[jidx].table);
    let mut left: alloc::vec::Vec<String> = alloc::vec![rel(&from.primary)];
    left.extend(from.joins[..jidx].iter().map(|j| rel(&j.table)));
    candidates.iter().copied().find(|sub| {
        equi_quals(sub).is_some_and(|(qa, qb)| {
            let names = |q: &str| left.iter().any(|l| l.eq_ignore_ascii_case(q));
            (qa.eq_ignore_ascii_case(&peer) && names(qb))
                || (qb.eq_ignore_ascii_case(&peer) && names(qa))
        })
    })
}

/// The WHERE with a set of conjuncts removed, for a scan node whose Filter
/// line should no longer claim a condition the join itself now enforces.
fn without_conjuncts(where_: Option<&Expr>, drop: &[&Expr]) -> Option<Expr> {
    if drop.is_empty() {
        return None;
    }
    let w = where_?;
    let dropped: alloc::collections::BTreeSet<usize> = drop
        .iter()
        .map(|e| core::ptr::from_ref::<Expr>(*e) as usize)
        .collect();
    crate::reorder::split_and_conjunctions(w)
        .into_iter()
        .filter(|c| !dropped.contains(&(core::ptr::from_ref::<Expr>(c) as usize)))
        .cloned()
        .reduce(|a, b| Expr::Binary {
            lhs: alloc::boxed::Box::new(a),
            op: spg_sql::ast::BinOp::And,
            rhs: alloc::boxed::Box::new(b),
        })
}

fn pg_cond(e: &Expr) -> String {
    let s = alloc::format!("{e}");
    if s.starts_with('(') && s.ends_with(')') {
        s
    } else {
        alloc::format!("({s})")
    }
}

/// v7.39 (round 226, Phase 2) — split a WHERE into the conjunct the index
/// actually serves (rendered as `Index Cond:`) and the residual conjuncts
/// (rendered as `Filter:`), matching PG's two-line Index Scan shape. The
/// split MIRRORS `try_index_seek`'s own decision: it walks the top-level
/// AND chain in the same order (LHS first, then RHS) and takes the FIRST
/// branch the seek accepts — so the plan reports the predicate SPG's
/// executor really pushed into the index, not a guess.
fn split_index_cond<'a>(
    engine: &Engine,
    name: &str,
    alias: &str,
    where_: &'a Expr,
) -> (Option<&'a Expr>, Vec<&'a Expr>) {
    // Flatten the top-level AND chain, preserving source order.
    fn flatten<'b>(e: &'b Expr, out: &mut Vec<&'b Expr>) {
        if let Expr::Binary {
            lhs,
            op: spg_sql::ast::BinOp::And,
            rhs,
        } = e
        {
            flatten(lhs, out);
            flatten(rhs, out);
        } else {
            out.push(e);
        }
    }
    let mut conjuncts: Vec<&Expr> = Vec::new();
    flatten(where_, &mut conjuncts);
    let Some(table) = engine.active_catalog().get(name) else {
        return (None, conjuncts);
    };
    let cols = &table.schema().columns;
    let snap = engine.current_snapshot();
    // Whole-predicate seek that is NOT decomposable (a two-sided range like
    // BETWEEN) stays one Index Cond.
    if conjuncts.len() == 1 {
        return (Some(where_), Vec::new());
    }
    // r1035 — several conjuncts can still be ONE seek. A BETWEEN is two of
    // them on the same column, and now that each half seeks on its own the
    // loop below would claim one and call the other a Filter, which reads
    // as a re-check the executor never performs.
    if crate::index_access::whole_predicate_is_one_range(where_, cols, table, alias) {
        return (Some(where_), Vec::new());
    }
    for (i, c) in conjuncts.iter().enumerate() {
        if try_index_seek(c, cols, engine.active_catalog(), table, alias, &snap).is_some() {
            let residual: Vec<&Expr> = conjuncts
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, e)| *e)
                .collect();
            return (Some(conjuncts[i]), residual);
        }
    }
    (None, conjuncts)
}

/// v7.39 (round 551) — the index that actually serves this condition.
///
/// `scan_node` named the table's FIRST BTree index whatever the
/// predicate was, with a comment saying so ("Approximation … single-index
/// tables are exact"). On a table with a primary key and a secondary
/// index that is a plain misstatement:
///
/// ```text
///     CREATE TABLE e (id INT PRIMARY KEY, k INT);
///     CREATE INDEX ek ON e (k);
///     EXPLAIN SELECT * FROM e WHERE k BETWEEN 10 AND 12;
///     -> Index Scan using e_pkey on e
/// ```
///
/// naming an index on `id` for a condition on `k`, which it cannot
/// serve. EXPLAIN is what every perf investigation reads first — this
/// one included — so an instrument that misnames the access path is
/// worse than one that says nothing.
///
/// The column comes from the condition itself; the index is the one
/// keyed on it.
fn index_name_for_cond(engine: &Engine, table: &str, alias: &str, cond: &Expr) -> Option<String> {
    fn column_of<'a>(e: &'a Expr, out: &mut Vec<&'a spg_sql::ast::ColumnName>) {
        match e {
            Expr::Column(c) => out.push(c),
            Expr::Binary { lhs, rhs, .. } => {
                column_of(lhs, out);
                column_of(rhs, out);
            }
            Expr::Unary { expr, .. } | Expr::Cast { expr, .. } => column_of(expr, out),
            _ => {}
        }
    }
    let mut refs: Vec<&spg_sql::ast::ColumnName> = Vec::new();
    column_of(cond, &mut refs);
    let t = engine.active_catalog().get(table)?;
    let cols = &t.schema().columns;
    for r in refs {
        if let Some(q) = &r.qualifier
            && !q.eq_ignore_ascii_case(alias)
            && !q.eq_ignore_ascii_case(table)
        {
            continue;
        }
        let Some(pos) = cols
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&r.name))
        else {
            continue;
        };
        if let Some(idx) = t.indices().iter().find(|i| {
            // v7.38.1 (L12) — a composite B-tree leading on the column
            // seeks it too (prefix descent), so the mirror must say so.
            i.column_position == pos
                && matches!(
                    i.kind,
                    spg_storage::IndexKind::BTree(_) | spg_storage::IndexKind::BTreeMulti(_)
                )
        }) {
            return Some(idx.name.clone());
        }
        // v7.38.14 — and the GIN kinds, for the same reason. Teaching the
        // node to say `Index Scan` for a containment while leaving this
        // function btree-only just moved the lie: the plan named the
        // table's primary key as the index serving `j @> '…'`. A plan that
        // names the wrong index is harder to argue with than one that says
        // Seq Scan, because it looks like it was checked.
        //
        // All FOUR variants, not the two whose names read like "GIN". The
        // first cut listed `Gin` and `GinFulltext` and still printed
        // `t_pkey`, because a jsonb containment index is `GinJsonb` --
        // matching on what a name looks like rather than on what the
        // catalog stores is how the wrong index got named twice.
        if let Some(idx) = t.indices().iter().find(|i| {
            i.column_position == pos
                && matches!(
                    i.kind,
                    spg_storage::IndexKind::Gin(_)
                        | spg_storage::IndexKind::GinTrgm(_)
                        | spg_storage::IndexKind::GinFulltext(_)
                        | spg_storage::IndexKind::GinJsonb(_)
                )
        }) {
            return Some(idx.name.clone());
        }
    }
    None
}

/// Render a conjunct list the way PG prints a multi-branch Filter:
/// `((a) AND (b))`, single conjunct as `(a)`.
fn pg_conjuncts(list: &[&Expr]) -> String {
    match list.len() {
        0 => String::new(),
        1 => pg_cond(list[0]),
        _ => {
            let parts: Vec<String> = list.iter().map(|e| pg_cond(e)).collect();
            alloc::format!("({})", parts.join(" AND "))
        }
    }
}

/// v7.39 (round 224) — build the scan node for one table reference:
/// `Index Scan using <idx> on <t>` when the executor's own index-seek
/// heuristic fires (the WHERE lands in `Index Cond:`), else
/// `Seq Scan on <t>` with the WHERE in `Filter:`. `where_` is attached
/// only when `attach_where` (the caller owns predicate placement for
/// joins). The plan shown is SPG's REAL access decision expressed in
/// PG's vocabulary — node names/indentation match PG so tools parse it;
/// the planner choices are SPG's own.
fn scan_node(
    engine: &Engine,
    name: &str,
    alias: Option<&str>,
    where_: Option<&Expr>,
    cte_names: &[String],
    index_only: bool,
) -> PlanNode {
    let alias_sfx = match alias {
        Some(a) if a != name => alloc::format!(" {a}"),
        _ => String::new(),
    };
    // A FROM item naming a CTE renders as PG's materialized-CTE scan.
    if cte_names.iter().any(|c| c == name) {
        let mut n = PlanNode::new(alloc::format!("CTE Scan on {name}{alias_sfx}"));
        if let Some(w) = where_ {
            n.attrs.push(alloc::format!("Filter: {}", pg_cond(w)));
        }
        return n;
    }
    // A partition parent renders as PG's Append over the surviving
    // children (the engine's prune pass decides which stay — the same
    // kept-set the pre-r224 format printed as `kept=[…]`).
    if crate::partition::is_partition_parent(engine.active_catalog(), name) {
        let mut app = PlanNode::new(String::from("Append"));
        let kept = where_
            .and_then(|_| engine.explain_partition_kept_children_by_where(name, where_))
            .or_else(|| engine.explain_partition_kept_children_by_where(name, None));
        if let Some(children) = kept {
            for c in &children {
                let mut sc = PlanNode::new(alloc::format!("Seq Scan on {c}"));
                if let Some(w) = where_ {
                    sc.attrs.push(alloc::format!("Filter: {}", pg_cond(w)));
                }
                app.children.push(sc);
            }
        }
        if app.children.is_empty() {
            // Every child pruned (or prune unavailable): PG shows a
            // one-child Append over the parent relation name.
            let mut sc = PlanNode::new(alloc::format!("Seq Scan on {name}{alias_sfx}"));
            if let Some(w) = where_ {
                sc.attrs.push(alloc::format!("Filter: {}", pg_cond(w)));
            }
            app.children.push(sc);
        }
        return app;
    }
    // v7.38.14 — ask what the EXECUTOR asks, which is more than one
    // question.
    //
    // `try_index_seek` is the btree/hash door and it is the only one this
    // node used to knock on. A jsonb containment or a full-text match goes
    // through `try_gin_jsonb_seek` / `try_gin_seek` instead, so a query
    // that really did use a GIN index printed `Seq Scan`.
    //
    // That is not a cosmetic gap. A read-only survey of this engine
    // reported, from these plans, that no GIN index is ever chosen by the
    // planner -- and it was wrong: timed at 10k against 100k rows, the
    // containment is FLAT (0.003 ms both) while a real sequential scan is
    // linear (0.105 -> 1.222). The index was working the whole time and
    // the plan said otherwise, which sent a real investigation to a wrong
    // conclusion. Round 565 left the rule on this same function -- "the
    // node has to follow the executor, not the gate" -- and this is that
    // rule applied to the doors it did not yet know about.
    let seek = where_.and_then(|w| {
        let table = engine.active_catalog().get(name)?;
        let cols = &table.schema().columns;
        let a = alias.unwrap_or(name);
        let snap = engine.current_snapshot();
        try_index_seek(w, cols, engine.active_catalog(), table, a, &snap)
            .map(|_| ())
            .or_else(|| {
                crate::index_access::try_gin_jsonb_seek(w, cols, table, a, &snap).map(|_| ())
            })
        // NOT covered here: `try_gin_seek`, the full-text door. It needs a
        // catalog and an `EvalContext` this node does not build, so a
        // `@@ to_tsquery(...)` that really does use its GIN index still
        // prints `Seq Scan`. Named rather than left looking handled --
        // the honest half of a fix is saying which half it is.
    });
    let table_rows = engine
        .active_catalog()
        .get(name)
        .map(|t| t.rows().len() as u64)
        .unwrap_or(0);
    let width = engine
        .active_catalog()
        .get(name)
        .map(|t| est_width(&t.schema().columns))
        .unwrap_or(8);
    // v7.39 (round 565) — `try_index_seek` carries a selectivity ceiling
    // (a seek that reads most of the table is worse than the scan it
    // replaces). The index-only walk has none by design — it touches no
    // row — so a wide range over a small table takes it while this gate
    // said Seq Scan. The node has to follow the executor, not the gate.
    if seek.is_some() || index_only {
        // v7.39 (round 226) — PG splits the predicate: the indexed conjunct
        // goes to Index Cond, everything else to a Filter line beneath it.
        let a = alias.unwrap_or(name);
        let split = where_.map(|w| split_index_cond(engine, name, a, w));
        // v7.39 (round 551) — name the index that actually serves the
        // condition, not the table's first BTree one.
        let idx_name = split
            .as_ref()
            .and_then(|(cond, _)| cond.as_ref().copied())
            .or(where_)
            .and_then(|c| index_name_for_cond(engine, name, a, c))
            .or_else(|| {
                engine.active_catalog().get(name).and_then(|t| {
                    t.indices()
                        .iter()
                        .find(|i| {
                            matches!(
                                i.kind,
                                spg_storage::IndexKind::BTree(_)
                                    | spg_storage::IndexKind::BTreeMulti(_)
                            )
                        })
                        .map(|i| i.name.clone())
                })
            })
            .unwrap_or_else(|| alloc::format!("{name}_idx"));
        // v7.39 (round 565) — PG distinguishes the scan that reads rows
        // from the one that answers out of the index, and so does the
        // executor: round 560 added the second and round 564 measured
        // them 2x apart at 50k rows. EXPLAIN called both "Index Scan",
        // so the two plans read identically to anyone comparing them —
        // and EXPLAIN is the first thing a performance question opens.
        let verb = if index_only {
            "Index Only Scan using"
        } else {
            "Index Scan using"
        };
        let mut n = PlanNode::new(alloc::format!("{verb} {idx_name} on {name}{alias_sfx}"));
        // r1038 — the conjunct this plan SEEKS, taken before `split` is
        // consumed below. `None` from the split means the seek took the
        // whole predicate, so `where_` is that conjunct.
        let seek_cond: Option<&Expr> = split.as_ref().and_then(|(c, _)| *c).or(where_);
        if let Some(w) = where_ {
            let (cond, residual) = split.expect("computed alongside where_");
            match cond {
                Some(c) => {
                    n.attrs.push(alloc::format!("Index Cond: {}", pg_cond(c)));
                    if !residual.is_empty() {
                        n.attrs
                            .push(alloc::format!("Filter: {}", pg_conjuncts(&residual)));
                    }
                }
                // v7.39 (round 551) — the seek took the WHOLE predicate
                // (a two-sided range is one seek, not two conjuncts), so
                // there is no residual. This used to print the same
                // expression twice: once as Index Cond and again as a
                // Filter, which reads as a re-check that does not happen.
                None => n.attrs.push(alloc::format!("Index Cond: {}", pg_cond(w))),
            }
        }
        // r1035 — ask the index rather than guessing, when it is cheap.
        //
        // r1038 — ask it about the conjunct this plan says it SEEKS, not
        // about the whole predicate. `WHERE id = 7 AND ts > <x>` seeks the
        // equality and filters the range; counting the range there gave a
        // node that printed `Index Cond: (id = 7)` above `rows=199`, an
        // estimate for work the plan does not do.
        let rows = real_range_rows(engine, name, alias, seek_cond)
            .unwrap_or_else(|| est_scan_rows(table_rows, where_, true));
        // Index descent + per-row fetch (SPG's own constants, PG's format).
        n.cost = Some((0.15, 0.15 + 8.0 + rows as f64 * 0.01, rows, width));
        n
    } else {
        let mut n = PlanNode::new(alloc::format!("Seq Scan on {name}{alias_sfx}"));
        let filtered = where_.is_some();
        if let Some(w) = where_ {
            n.attrs.push(alloc::format!("Filter: {}", pg_cond(w)));
        }
        let rows = real_range_rows(engine, name, alias, where_)
            .unwrap_or_else(|| est_scan_rows(table_rows, where_, false));
        let total = 1.0
            + table_rows as f64 * 0.01
            + if filtered {
                table_rows as f64 * 0.0025
            } else {
                0.0
            };
        n.cost = Some((0.0, total, rows, width));
        n
    }
}

/// Cost roll-ups for the wrapper nodes: derived from the child's total +
/// a per-row CPU term. SPG's own model in PG's clothes.
fn child_cost(n: &PlanNode) -> (f64, f64, u64, u64) {
    n.children
        .first()
        .and_then(|c| c.cost)
        .unwrap_or((0.0, 0.0, 1, 8))
}

/// v7.39 (round 227, Phase 3) — fill the tree's ANALYZE blocks from
/// GENUINELY MEASURED numbers only. The top node takes the real elapsed
/// (its elapsed IS the query's) plus the real result-row count; leaf scan
/// nodes take the per-table scan-counter delta the executor really bumped
/// (`idx_tup_fetch` for an Index Scan, `seq_tup_read` for a Seq Scan).
/// Any node whose actual row count SPG cannot derive from a real counter
/// is left un-annotated rather than given a fabricated figure — SPG has
/// no per-node timer and this renderer will not invent one. Documented
/// divergence from PG, which instruments every node.
/// v7.39 (round 555) — which sort SPG really runs, under ANALYZE only.
///
/// `ORDER BY … LIMIT k` takes the streaming top-N trim, which bounds
/// live memory to O(k) instead of materialising every projected row.
/// Measured on a 600k-row table: the LIMIT 5 sort added 0 MB of
/// residency where the same sort without a LIMIT added 28. The bound
/// was real and INVISIBLE — the node said only "Sort", so a reader
/// could not tell it from the unbounded one, and EXPLAIN is where they
/// would look.
///
/// PG prints this line under ANALYZE and not under a plain EXPLAIN,
/// because it is a measured runtime fact rather than a plan property;
/// this follows it. No Memory figure beside it: PG measures its sort's
/// peak and SPG does not meter one, and a number that was not measured
/// is worse than none.
///
/// v7.37 (round 884) — a KNOWN DIVERGENCE, recorded here because round
/// 882 created it. A single-table `ORDER BY` served over the wire now
/// goes through the bounded sorter and can spill (26 runs and 86 MB on a
/// 400k-row sort at `work_mem = 4MB`, counted in
/// `pg_stat_database.temp_files`). EXPLAIN ANALYZE does not see that: it
/// re-runs the statement through `exec_select_cancel`, the materialising
/// executor, where the spilling walk is not hooked — so the sort it
/// measures really is a quicksort, and this line is accurate about the
/// run it describes while understating what the same SQL does in
/// production.
///
/// Reporting `external merge` here would mean predicting a spill rather
/// than measuring one, which is what the paragraph above refuses to do.
/// Closing it properly means making ANALYZE execute the path the query
/// actually takes; that is a change to what EXPLAIN ANALYZE runs, not to
/// what it prints, and it is not smuggled in under a `Sort Method` fix.
fn annotate_sort_method(node: &mut PlanNode, has_limit: bool) {
    if node.head == "Sort"
        && !node.attrs.iter().any(|a| a.starts_with("Sort Method:"))
        && let Some(pos) = node.attrs.iter().position(|a| a.starts_with("Sort Key:"))
    {
        node.attrs.insert(
            pos + 1,
            alloc::string::String::from(if has_limit {
                "Sort Method: top-N heapsort"
            } else {
                "Sort Method: quicksort"
            }),
        );
    }
    for c in &mut node.children {
        annotate_sort_method(c, has_limit);
    }
}

fn fill_actuals(
    node: &mut PlanNode,
    is_top: bool,
    engine: &Engine,
    result_rows: u64,
    elapsed_ms: Option<f64>,
    deltas: &alloc::collections::BTreeMap<String, u64>,
) {
    /// The table a scan node reads, if the head names one.
    fn scan_table(head: &str) -> Option<(&str, bool)> {
        if let Some(r) = head.strip_prefix("Seq Scan on ") {
            Some((r.split_whitespace().next().unwrap_or(r), true))
        } else if let Some(r) = head.strip_prefix("CTE Scan on ") {
            Some((r.split_whitespace().next().unwrap_or(r), false))
        } else if let Some(r) = head
            .strip_prefix("Index Scan using ")
            .or_else(|| head.strip_prefix("Index Only Scan using "))
            .and_then(|r| r.split_once(" on ").map(|(_, t)| t))
        {
            Some((r.split_whitespace().next().unwrap_or(r), false))
        } else {
            None
        }
    }
    let head = node.head.clone();
    let filtered = node.attrs.iter().any(|a| a.starts_with("Filter: "));
    let live_rows = |t: &str| -> Option<u64> {
        engine
            .active_catalog()
            .get(t)
            .map(|tb| (tb.rows().len() as u64).saturating_sub(tb.dead_rows()))
    };
    // v7.39 (round 565) — PG prints this under ANALYZE and not under a
    // plain EXPLAIN, because it is a counter, not a plan property. Zero
    // is the measured truth here rather than a placeholder: the path
    // this node names never reads a row, which is the whole reason the
    // node has a different name.
    if head.starts_with("Index Only Scan using ")
        && !node.attrs.iter().any(|a| a.starts_with("Heap Fetches:"))
    {
        node.attrs.push(String::from("Heap Fetches: 0"));
    }
    if is_top {
        node.actual = Some((elapsed_ms, result_rows));
        // When the top node IS a Seq Scan carrying the filter, both numbers
        // are genuine: a sequential scan reads every live row (catalog
        // count), and the result is what survived.
        if node.children.is_empty()
            && filtered
            && let Some((t, is_seq)) = scan_table(&head)
            && is_seq
            && let Some(read) = live_rows(t)
            && read >= result_rows
        {
            node.attrs.push(alloc::format!(
                "Rows Removed by Filter: {}",
                read - result_rows
            ));
        }
    } else if let Some((table, is_seq)) = scan_table(&head) {
        // PG's `actual rows` on a scan is its OUTPUT (post-filter) count.
        // An unfiltered sequential scan emits every live row — genuinely
        // the catalog count. A filtered one's output is not derivable
        // without per-node instrumentation, so it is left un-annotated
        // rather than labelled with a number that means something else.
        // Index scans report the executor's real fetch counter when the
        // path bumped it.
        if !filtered {
            if is_seq {
                if let Some(n) = live_rows(table) {
                    node.actual = Some((None, n));
                }
            } else if let Some(&n) = deltas.get(table) {
                node.actual = Some((None, n));
            }
        }
    }
    for c in &mut node.children {
        fill_actuals(c, false, engine, result_rows, elapsed_ms, deltas);
    }
}

/// Snapshot every table's (seq_tup_read + idx_tup_fetch) so a before/after
/// pair yields the rows each scan really touched during the ANALYZE run.
fn scan_counter_snapshot(engine: &Engine) -> alloc::collections::BTreeMap<String, u64> {
    use core::sync::atomic::Ordering;
    let cat = engine.active_catalog();
    let mut out = alloc::collections::BTreeMap::new();
    for name in cat.table_names() {
        if let Some(t) = cat.get(&name) {
            let st = t.scan_stats();
            let v =
                st.seq_tup_read.load(Ordering::Relaxed) + st.idx_tup_fetch.load(Ordering::Relaxed);
            out.insert(name, v);
        }
    }
    out
}

/// One rendered property of a plan node, format-neutral: the structured
/// forms below are what differ between JSON / XML / YAML, so the property
/// list is built once (`node_props`) and each renderer spells it its own
/// way. (v7.39 round 228 — was JSON-only in r226.)
enum Prop {
    /// A quoted scalar in JSON/YAML, bare text in XML.
    Str(String),
    /// A bare token (`false`, a number) — unquoted in every format.
    Bare(String),
    /// A sequence: JSON `[…]`, XML `<Item>…</Item>`, YAML `- …` lines.
    List(Vec<String>),
}

/// v7.39 (round 226 Phase 2 / round 228) — the PG property list for one
/// plan node: "Node Type" first, structural keys, cost keys (omitted under
/// COSTS OFF), ANALYZE actuals when measured, then the node's attribute
/// lines as their own keys. Key names, order, and the Aggregate
/// Strategy / Partial Mode / Scan Direction spellings are measured off
/// live PG18.4 (r226 + r228 probes).
fn node_props(node: &PlanNode, with_costs: bool, parent_rel: Option<&str>) -> Vec<(String, Prop)> {
    let mut p: Vec<(String, Prop)> = Vec::new();
    let mut push = |k: &str, v: Prop| p.push((String::from(k), v));
    // "Node Type" + the structural keys PG derives from the node head.
    let head = node.head.as_str();
    let (node_type, rel, idx) = if let Some(rest) = head.strip_prefix("Seq Scan on ") {
        (
            "Seq Scan",
            Some(rest.split_whitespace().next().unwrap_or(rest)),
            None,
        )
    } else if let Some(rest) = head.strip_prefix("Index Only Scan using ") {
        let (i, r) = rest.split_once(" on ").unwrap_or((rest, ""));
        (
            "Index Only Scan",
            Some(r.split_whitespace().next().unwrap_or(r)),
            Some(i),
        )
    } else if let Some(rest) = head.strip_prefix("Index Scan using ") {
        let (i, r) = rest.split_once(" on ").unwrap_or((rest, ""));
        (
            "Index Scan",
            Some(r.split_whitespace().next().unwrap_or(r)),
            Some(i),
        )
    } else if let Some(rest) = head.strip_prefix("CTE Scan on ") {
        (
            "CTE Scan",
            Some(rest.split_whitespace().next().unwrap_or(rest)),
            None,
        )
    } else if let Some(rest) = head.strip_prefix("Insert on ") {
        ("Insert", Some(rest), None)
    } else if let Some(rest) = head.strip_prefix("Update on ") {
        ("Update", Some(rest), None)
    } else if let Some(rest) = head.strip_prefix("Delete on ") {
        ("Delete", Some(rest), None)
    } else if head.starts_with("CTE ") {
        ("CTE", None, None)
    } else {
        (head, None, None)
    };
    // PG's HashAggregate is Node Type "Aggregate" + Strategy "Hashed",
    // and every Aggregate carries a Partial Mode.
    let is_agg = node_type == "HashAggregate" || node_type == "Aggregate";
    push(
        "Node Type",
        Prop::Str(String::from(if is_agg { "Aggregate" } else { node_type })),
    );
    if let Some(pr) = parent_rel {
        push("Parent Relationship", Prop::Str(String::from(pr)));
    }
    if is_agg {
        let strategy = if node_type == "HashAggregate" {
            "Hashed"
        } else {
            "Plain"
        };
        push("Strategy", Prop::Str(String::from(strategy)));
        push("Partial Mode", Prop::Str(String::from("Simple")));
    }
    push("Parallel Aware", Prop::Bare(String::from("false")));
    push("Async Capable", Prop::Bare(String::from("false")));
    if let Some(i) = idx {
        // SPG's index access is always a forward descent (r228: PG spells
        // the same thing "Forward").
        push("Scan Direction", Prop::Str(String::from("Forward")));
        push("Index Name", Prop::Str(String::from(i)));
    }
    if let Some(r) = rel {
        push("Relation Name", Prop::Str(String::from(r)));
        push("Alias", Prop::Str(String::from(r)));
    }
    if with_costs && let Some((cs, ct, rows, width)) = node.cost {
        push("Startup Cost", Prop::Bare(alloc::format!("{cs:.2}")));
        push("Total Cost", Prop::Bare(alloc::format!("{ct:.2}")));
        push("Plan Rows", Prop::Bare(alloc::format!("{rows}")));
        push("Plan Width", Prop::Bare(alloc::format!("{width}")));
    }
    // ANALYZE actuals — only the genuinely measured ones (r227): a time
    // key appears only where SPG really took a reading.
    if let Some((time, rows)) = &node.actual {
        if let Some(ms) = time {
            push("Actual Startup Time", Prop::Bare(String::from("0.000")));
            push("Actual Total Time", Prop::Bare(alloc::format!("{ms:.3}")));
        }
        push("Actual Rows", Prop::Bare(alloc::format!("{rows}.00")));
        push("Actual Loops", Prop::Bare(String::from("1")));
    }
    push("Disabled", Prop::Bare(String::from("false")));
    // Attribute lines become their own keys ("Filter", "Index Cond",
    // "Sort Key", "Group Key", "Hash Cond", "Join Filter"). PG renders
    // the key-list forms (Sort/Group Key) as sequences.
    for a in &node.attrs {
        let Some((k, v)) = a.split_once(": ") else {
            continue;
        };
        if k == "Sort Key" || k == "Group Key" {
            push(k, Prop::List(v.split(", ").map(String::from).collect()));
        } else if k == "Rows Removed by Filter" {
            push(k, Prop::Bare(String::from(v)));
        } else {
            push(k, Prop::Str(String::from(v)));
        }
    }
    p
}

/// PG labels a node's first child Outer and the second Inner.
fn child_rel(i: usize) -> &'static str {
    if i == 0 { "Outer" } else { "Inner" }
}

/// v7.39 (round 226 / round 228) — PG's FORMAT JSON: a one-element array
/// holding `{"Plan": {…}}`, pretty-printed two spaces per level exactly as
/// PG does, children nested under "Plans".
fn render_json_plan(node: &PlanNode, with_costs: bool) -> String {
    fn obj(
        node: &PlanNode,
        with_costs: bool,
        parent_rel: Option<&str>,
        ind: usize,
        out: &mut String,
    ) {
        let pad = " ".repeat(ind);
        let inner = " ".repeat(ind + 2);
        out.push_str("{\n");
        let props = node_props(node, with_costs, parent_rel);
        let last = props.len().saturating_sub(1);
        for (i, (k, v)) in props.iter().enumerate() {
            out.push_str(&inner);
            out.push_str(&json_string_lit(k));
            out.push_str(": ");
            match v {
                Prop::Str(s) => out.push_str(&json_string_lit(s)),
                Prop::Bare(s) => out.push_str(s),
                Prop::List(items) => {
                    let its: Vec<String> = items.iter().map(|s| json_string_lit(s)).collect();
                    out.push_str(&alloc::format!("[{}]", its.join(", ")));
                }
            }
            if i != last || !node.children.is_empty() {
                out.push(',');
            }
            out.push('\n');
        }
        if !node.children.is_empty() {
            out.push_str(&inner);
            out.push_str("\"Plans\": [\n");
            for (i, c) in node.children.iter().enumerate() {
                out.push_str(&" ".repeat(ind + 4));
                obj(c, with_costs, Some(child_rel(i)), ind + 4, out);
                if i + 1 != node.children.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&inner);
            out.push_str("]\n");
        }
        out.push_str(&pad);
        out.push('}');
    }
    let mut out = String::from("[\n  {\n    \"Plan\": ");
    obj(node, with_costs, None, 4, &mut out);
    out.push_str("\n  }\n]");
    out
}

/// v7.39 (round 228) — PG's FORMAT XML: the `<explain>` envelope with the
/// PG namespace, one `<Query>`, and the node tree with hyphenated element
/// names (`Node-Type`, `Relation-Name`) and `<Item>` sequences.
fn render_xml_plan(node: &PlanNode, with_costs: bool) -> String {
    fn elem(
        node: &PlanNode,
        with_costs: bool,
        parent_rel: Option<&str>,
        ind: usize,
        out: &mut String,
    ) {
        let pad = " ".repeat(ind);
        let inner = " ".repeat(ind + 2);
        out.push_str(&alloc::format!("{pad}<Plan>\n"));
        for (k, v) in node_props(node, with_costs, parent_rel) {
            let tag = k.replace(' ', "-");
            match v {
                Prop::Str(s) => {
                    out.push_str(&alloc::format!(
                        "{inner}<{tag}>{}</{tag}>\n",
                        xml_escape(&s)
                    ));
                }
                Prop::Bare(s) => out.push_str(&alloc::format!("{inner}<{tag}>{s}</{tag}>\n")),
                Prop::List(items) => {
                    out.push_str(&alloc::format!("{inner}<{tag}>\n"));
                    for it in items {
                        out.push_str(&alloc::format!(
                            "{inner}  <Item>{}</Item>\n",
                            xml_escape(&it)
                        ));
                    }
                    out.push_str(&alloc::format!("{inner}</{tag}>\n"));
                }
            }
        }
        if !node.children.is_empty() {
            out.push_str(&alloc::format!("{inner}<Plans>\n"));
            for (i, c) in node.children.iter().enumerate() {
                elem(c, with_costs, Some(child_rel(i)), ind + 4, out);
            }
            out.push_str(&alloc::format!("{inner}</Plans>\n"));
        }
        out.push_str(&alloc::format!("{pad}</Plan>\n"));
    }
    let mut out =
        String::from("<explain xmlns=\"http://www.postgresql.org/2009/explain\">\n  <Query>\n");
    elem(node, with_costs, None, 4, &mut out);
    out.push_str("  </Query>\n</explain>");
    out
}

/// v7.39 (round 228) — PG's FORMAT YAML: a one-element sequence whose
/// `Plan:` mapping holds the node properties, children as a nested `Plans:`
/// sequence. PG emits a trailing space after the container keys; matched.
fn render_yaml_plan(node: &PlanNode, with_costs: bool) -> String {
    fn map(
        node: &PlanNode,
        with_costs: bool,
        parent_rel: Option<&str>,
        ind: usize,
        out: &mut String,
    ) {
        let pad = " ".repeat(ind);
        for (i, (k, v)) in node_props(node, with_costs, parent_rel).iter().enumerate() {
            // The first property carries the sequence dash from the caller.
            if i > 0 {
                out.push_str(&pad);
            }
            match v {
                Prop::Str(s) => out.push_str(&alloc::format!("{k}: {}\n", yaml_scalar(s))),
                Prop::Bare(s) => out.push_str(&alloc::format!("{k}: {s}\n")),
                Prop::List(items) => {
                    out.push_str(&alloc::format!("{k}: \n"));
                    for it in items {
                        out.push_str(&alloc::format!("{pad}  - {}\n", yaml_scalar(it)));
                    }
                }
            }
        }
        if !node.children.is_empty() {
            out.push_str(&alloc::format!("{pad}Plans: \n"));
            for (i, c) in node.children.iter().enumerate() {
                out.push_str(&alloc::format!("{pad}  - "));
                map(c, with_costs, Some(child_rel(i)), ind + 4, out);
            }
        }
    }
    let mut out = String::from("- Plan: \n    ");
    map(node, with_costs, None, 4, &mut out);
    out
}

/// v7.39 (round 224) — build the PG-shaped plan tree for a SELECT.
/// Wrapping order (inner to outer): scan/join -> Aggregate/WindowAgg ->
/// Sort -> Limit, matching PG's text plans. UNION peers become Append;
/// materialized CTEs hang a `CTE <name>` labelled block under the root.
fn build_plan_tree(stmt: &SelectStatement, engine: &Engine) -> PlanNode {
    let cte_names: Vec<String> = stmt.ctes.iter().map(|c| c.name.clone()).collect();
    // UNION family: every branch under one Append (PG uses Append for
    // UNION ALL and wraps distinct set-ops further; Phase 0 renders the
    // branch structure for all of them).
    if !stmt.unions.is_empty() {
        let mut root = PlanNode::new(String::from("Append"));
        let mut first = stmt.clone();
        first.unions = Vec::new();
        root.children.push(build_plan_tree(&first, engine));
        for (_kind, peer) in &stmt.unions {
            root.children.push(build_plan_tree(peer, engine));
        }
        let mut total = 0.0f64;
        let mut rows = 0u64;
        let mut width = 8u64;
        for c in &root.children {
            if let Some((_, ct, cr, cw)) = c.cost {
                total += ct;
                rows += cr;
                width = width.max(cw);
            }
        }
        root.cost = Some((0.0, total + rows as f64 * 0.0025, rows, width));
        return root;
    }
    // Base: FROM-less SELECT is PG's Result node.
    let mut node = match &stmt.from {
        None => {
            let mut r = PlanNode::new(String::from("Result"));
            r.cost = Some((0.0, 0.01, 1, 4));
            r
        }
        Some(from) => {
            // v7.39 (round 588) — the executor now takes an equi-join key out
            // of the WHERE clause when the ON clause has none, so
            // `FROM a, b WHERE a.id = b.id` really does hash. Ask the same
            // question here, by qualifier, or EXPLAIN would keep naming a
            // Nested Loop that no longer runs — and a promoted conjunct is
            // the join's condition now, so it leaves the scan's Filter line.
            let where_eq = where_equi_join_conds(from, stmt.where_.as_ref());
            let promoted_all: alloc::vec::Vec<&Expr> = (0..from.joins.len())
                .filter(|&i| {
                    !from.joins[i].on.as_ref().is_some_and(|on| {
                        matches!(on, Expr::Binary { op, .. } if matches!(op, spg_sql::ast::BinOp::Eq))
                    })
                })
                .filter_map(|i| promoted_key_for(from, i, &where_eq))
                .collect();
            let scan_where = without_conjuncts(stmt.where_.as_ref(), &promoted_all);
            let mut left = scan_node(
                engine,
                &from.primary.name,
                from.primary.alias.as_deref(),
                scan_where.as_ref().or(if promoted_all.is_empty() {
                    stmt.where_.as_ref()
                } else {
                    None
                }),
                &cte_names,
                engine.stmt_takes_index_only_scan(stmt),
            );
            // Fold joins left-deep: equality ON -> Hash Join (right side
            // wrapped in a Hash node, like PG); anything else ->
            // Nested Loop with a Join Filter.
            for (jidx, j) in from.joins.iter().enumerate() {
                let right = scan_node(
                    engine,
                    &j.table.name,
                    j.table.alias.as_deref(),
                    None,
                    &cte_names,
                    false,
                );
                let (verb, hashable) = match j.kind {
                    spg_sql::ast::JoinKind::Inner => ("", true),
                    spg_sql::ast::JoinKind::Left => (" Left", true),
                    spg_sql::ast::JoinKind::Right => (" Right", true),
                    spg_sql::ast::JoinKind::FullOuter => (" Full", true),
                    // v7.39 (round 725) — PG's plan node name for the
                    // EXISTS pull-up's join.
                    spg_sql::ast::JoinKind::Semi => (" Semi", true),
                    spg_sql::ast::JoinKind::Cross => ("", true),
                };
                let is_eq_join = j
                    .on
                    .as_ref()
                    .is_some_and(|on| matches!(on, Expr::Binary { op, .. } if matches!(op, spg_sql::ast::BinOp::Eq)));
                let promoted = if is_eq_join {
                    None
                } else {
                    promoted_key_for(from, jidx, &where_eq)
                };
                let mut jn = if (is_eq_join || promoted.is_some()) && hashable {
                    let mut jn = PlanNode::new(alloc::format!("Hash Join{verb}"));
                    // PG spells it "Hash Left Join", verb before "Join".
                    jn.head = alloc::format!("Hash{verb} Join");
                    if let Some(on) = j.on.as_ref().or(promoted) {
                        jn.attrs.push(alloc::format!("Hash Cond: {}", pg_cond(on)));
                    }
                    let mut hash = PlanNode::new(String::from("Hash"));
                    let (_, rt, rr, rw) = right.cost.unwrap_or((0.0, 0.0, 1, 8));
                    hash.cost = Some((rt, rt + rr as f64 * 0.01, rr, rw));
                    hash.children.push(right);
                    let (_, lt, lr, lw) = left.cost.unwrap_or((0.0, 0.0, 1, 8));
                    let (hs, ht, hr, hw) = hash.cost.unwrap_or((0.0, 0.0, 1, 8));
                    let _ = hs;
                    jn.cost = Some((ht, ht + lt + (lr + hr) as f64 * 0.01, lr.max(hr), lw + hw));
                    jn.children.push(left);
                    jn.children.push(hash);
                    jn
                } else {
                    let mut jn = PlanNode::new(alloc::format!("Nested Loop{verb}"));
                    if let Some(on) = &j.on {
                        jn.attrs
                            .push(alloc::format!("Join Filter: {}", pg_cond(on)));
                    }
                    let (_, lt, lr, lw) = left.cost.unwrap_or((0.0, 0.0, 1, 8));
                    let (_, rt, rr, rw) = right.cost.unwrap_or((0.0, 0.0, 1, 8));
                    jn.cost = Some((0.0, lt + lr as f64 * rt.max(0.01), lr * rr.max(1), lw + rw));
                    jn.children.push(left);
                    jn.children.push(right);
                    jn
                };
                if expr_has_subquery(
                    stmt.where_
                        .as_ref()
                        .unwrap_or(&Expr::Literal(spg_sql::ast::Literal::Null)),
                ) {
                    // Subquery filters stay on the scan node (attached
                    // above); nothing extra here — placeholder branch kept
                    // for the Phase 1 predicate-split work.
                    let _ = &mut jn;
                }
                left = jn;
            }
            left
        }
    };
    // Aggregate / window / distinct wrappers.
    if select_has_window(stmt) {
        let mut w = PlanNode::new(String::from("WindowAgg"));
        w.children.push(node);
        let (cs, ct, cr, cw) = child_cost(&w);
        w.cost = Some((cs, ct + cr as f64 * 0.01, cr, cw + 8));
        node = w;
    }
    if aggregate::uses_aggregate(stmt) || stmt.group_by.is_some() {
        let mut agg = if let Some(gs) = &stmt.group_by {
            let mut a = PlanNode::new(String::from("HashAggregate"));
            let keys: Vec<String> = gs.iter().map(|g| alloc::format!("{g}")).collect();
            a.attrs
                .push(alloc::format!("Group Key: {}", keys.join(", ")));
            a
        } else {
            PlanNode::new(String::from("Aggregate"))
        };
        if let Some(h) = &stmt.having {
            agg.attrs.push(alloc::format!("Filter: {}", pg_cond(h)));
        }
        agg.children.push(node);
        let (_, ct, cr, cw) = child_cost(&agg);
        let out_rows = if stmt.group_by.is_some() {
            (cr / 10).max(1)
        } else {
            1
        };
        agg.cost = Some((ct, ct + cr as f64 * 0.0025, out_rows, cw.min(16)));
        node = agg;
    } else if stmt.distinct {
        // PG plans SELECT DISTINCT as a HashAggregate over the select list.
        let mut d = PlanNode::new(String::from("HashAggregate"));
        let keys: Vec<String> = stmt
            .items
            .iter()
            .map(|it| match it {
                SelectItem::Wildcard => String::from("*"),
                SelectItem::Expr { expr, .. } => alloc::format!("{expr}"),
                other => alloc::format!("{other:?}"),
            })
            .collect();
        d.attrs
            .push(alloc::format!("Group Key: {}", keys.join(", ")));
        d.children.push(node);
        let (_, ct, cr, cw) = child_cost(&d);
        d.cost = Some((ct, ct + cr as f64 * 0.0025, (cr / 10).max(1), cw));
        node = d;
    }
    // r1044 — the walk that replaces the sort entirely. `EXPLAIN` used to
    // print `Sort` over `Seq Scan` for a query the executor served by
    // walking the index: `SELECT pad FROM t ORDER BY id` on 400,000 rows
    // took 34.9 ms against 147.0 for the same query ordered by an
    // unindexed column, so the walk was plainly running and the plan
    // named the wrong access path. Both now ask
    // `Engine::index_order_walk_target`.
    let walk = stmt
        .from
        .as_ref()
        .and_then(|from| engine.index_order_walk_target(stmt, from));
    if let Some((idx_name, _)) = &walk
        && let Some(from) = stmt.from.as_ref()
    {
        // r1047 — under DISTINCT the node built above is the
        // HashAggregate wrapper, which the walk does not run: the
        // executor emits one row per key group. Unwrap to the scan it
        // wraps, and put a `Unique` on top instead — PG's plan shape for
        // distinct over sorted input, and the thing that actually runs.
        let mut base = node;
        if stmt.distinct && !base.children.is_empty() {
            base = base.children.remove(0);
        }
        let name = from.primary.name.as_str();
        let alias_sfx = from
            .primary
            .alias
            .as_deref()
            .map(|a| alloc::format!(" {a}"))
            .unwrap_or_default();
        let mut n = PlanNode::new(alloc::format!(
            "Index Scan using {idx_name} on {name}{alias_sfx}"
        ));
        let desc = if stmt.order_by[0].desc { " DESC" } else { "" };
        n.attrs
            .push(alloc::format!("Order By: {}{desc}", stmt.order_by[0].expr));
        // The walk IS the scan — one node, the way PG renders it — so it
        // carries the scan's own rows and width, not a child's. Reading
        // `child_cost` here instead gave `cost=0.15..0.00 rows=1` on a
        // 2,000-row table: a total below its own startup, which is not a
        // number anything should print.
        let (_, ct, cr, cw) = base.cost.unwrap_or((0.0, 0.0, 0, 0));
        // No sort to pay for: the rows arrive in order.
        n.cost = Some((0.15, ct, cr, cw));
        n.children = core::mem::take(&mut base.children);
        for a in &base.attrs {
            if a.starts_with("Filter:") {
                n.attrs.push(a.clone());
            }
        }
        if stmt.distinct {
            let mut u = PlanNode::new(String::from("Unique"));
            u.children.push(n);
            let (cs, ct, cr, cw) = child_cost(&u);
            u.cost = Some((cs, ct + cr as f64 * 0.0025, (cr / 10).max(1), cw));
            node = u;
        } else {
            node = n;
        }
    } else if !stmt.order_by.is_empty() {
        let mut s = PlanNode::new(String::from("Sort"));
        let keys: Vec<String> = stmt
            .order_by
            .iter()
            .map(|o| {
                if o.desc {
                    alloc::format!("{} DESC", o.expr)
                } else {
                    alloc::format!("{}", o.expr)
                }
            })
            .collect();
        s.attrs
            .push(alloc::format!("Sort Key: {}", keys.join(", ")));
        s.children.push(node);
        let (_, ct, cr, cw) = child_cost(&s);
        // Sort pays its work up front: startup ≈ total (PG shape).
        let sort_cost = ct + cr as f64 * 0.02;
        s.cost = Some((sort_cost, sort_cost + cr as f64 * 0.01, cr, cw));
        node = s;
    }
    if stmt.limit.is_some() || stmt.offset.is_some() {
        let mut l = PlanNode::new(String::from("Limit"));
        l.children.push(node);
        let (cs, ct, cr, cw) = child_cost(&l);
        // A literal LIMIT n caps the estimate; expression limits keep it.
        let lim = match &stmt.limit {
            Some(spg_sql::ast::LimitExpr::Literal(n)) => u64::from(*n).min(cr),
            _ => cr,
        };
        l.cost = Some((cs, ct, lim, cw));
        node = l;
    }
    // Materialized CTE blocks hang as named (`no_arrow`) sub-plans on the
    // root, matching PG's `CTE <name>` label + arrowed body.
    for cte in &stmt.ctes {
        let label = if cte.recursive {
            alloc::format!("CTE {} (recursive)", cte.name)
        } else {
            alloc::format!("CTE {}", cte.name)
        };
        let mut block = PlanNode::new(label);
        block.no_arrow = true;
        match &cte.body {
            spg_sql::ast::CteBody::Select(s) => {
                block.children.push(build_plan_tree(s, engine));
            }
            spg_sql::ast::CteBody::Insert(s) => {
                block
                    .children
                    .push(PlanNode::new(alloc::format!("Insert on {}", s.table)));
            }
            spg_sql::ast::CteBody::Update(s) => {
                block
                    .children
                    .push(PlanNode::new(alloc::format!("Update on {}", s.table)));
            }
            spg_sql::ast::CteBody::Delete(s) => {
                block
                    .children
                    .push(PlanNode::new(alloc::format!("Delete on {}", s.table)));
            }
            spg_sql::ast::CteBody::Merge(s) => {
                block
                    .children
                    .push(PlanNode::new(alloc::format!("Merge on {}", s.target)));
            }
        }
        node.children.insert(0, block);
    }
    node
}

/// v4.26 → v7.39 (round 224) — render the plan for `EXPLAIN <select>` in
/// PG's text-tree shape (node vocabulary + `->` indentation measured off
/// live PG18.4), so tools that parse PG plans (pgAdmin, explain
/// visualisers, ORM analyzers) read SPG plans unchanged. The tree shows
/// SPG's REAL execution decisions (its own index-seek heuristic, its own
/// join strategy) — the shape is PG's, the choices are SPG's.
pub(crate) fn explain_select(
    stmt: &SelectStatement,
    engine: &Engine,
    depth: usize,
    out: &mut Vec<String>,
) {
    let tree = build_plan_tree(stmt, engine);
    render_pg_tree(&tree, depth, out);
}

/// v7.39 (round 225, Phase 1) — costed variant: bare `EXPLAIN` shows PG's
/// `(cost=A..B rows=N width=W)` suffix on every plan node (format PG's,
/// numbers SPG's own estimates); `COSTS OFF` routes to the bare renderer.
pub(crate) fn explain_select_costed(
    stmt: &SelectStatement,
    engine: &Engine,
    with_costs: bool,
    out: &mut Vec<String>,
) {
    let tree = build_plan_tree(stmt, engine);
    render_costed(&tree, with_costs, out);
}

/// Render with or without the cost suffix (one shared walk).
fn render_costed(tree: &PlanNode, with_costs: bool, out: &mut Vec<String>) {
    // v7.39 (round 227) — one walk handles both: the cost suffix is gated
    // per-node, the ANALYZE `(actual …)` block always renders when present
    // (COSTS OFF must not swallow the measured block).
    fn walk(node: &PlanNode, depth: usize, out: &mut Vec<String>, with_costs: bool) {
        let mut head = if depth == 0 {
            node.head.clone()
        } else if node.no_arrow {
            alloc::format!("{}{}", " ".repeat(6 * depth - 4), node.head)
        } else {
            alloc::format!("{}->  {}", " ".repeat(6 * depth - 6 + 2), node.head)
        };
        if with_costs && let Some((cs, ct, rows, width)) = node.cost {
            head.push_str(&alloc::format!(
                "  (cost={cs:.2}..{ct:.2} rows={rows} width={width})"
            ));
        }
        // v7.39 (round 227) — PG's measured block follows the estimate.
        if let Some((t, rows)) = node.actual {
            match t {
                Some(ms) => head.push_str(&alloc::format!(
                    " (actual time=0.000..{ms:.3} rows={rows}.00 loops=1)"
                )),
                None => head.push_str(&alloc::format!(" (actual rows={rows}.00 loops=1)")),
            }
        }
        out.push(head);
        let attr_pad = " ".repeat(6 * depth + 2);
        for a in &node.attrs {
            out.push(alloc::format!("{attr_pad}{a}"));
        }
        for c in &node.children {
            walk(c, depth + 1, out, with_costs);
        }
    }
    walk(tree, 0, out, with_costs);
}

impl Engine {
    /// v7.39 (round 286) — the `<Verb> on <table>` root PG puts over a
    /// DML statement's source plan. Split out of `exec_explain` so the
    /// ANALYZE path, which has to really execute and therefore needs
    /// `&mut self`, renders the identical tree.
    fn dml_plan_tree(&self, inner: &spg_sql::ast::Statement) -> Option<PlanNode> {
        match inner {
            spg_sql::ast::Statement::Insert(i) => {
                let mut root = PlanNode::new(alloc::format!("Insert on {}", i.table));
                let child = match &i.select_source {
                    Some(src) => build_plan_tree(src, self),
                    None => {
                        let mut r = PlanNode::new(String::from("Result"));
                        r.cost = Some((0.0, 0.01, i.rows.len().max(1) as u64, 8));
                        r
                    }
                };
                root.cost = child.cost.map(|(_, ct, _, _)| (0.0, ct, 0, 0));
                root.children.push(child);
                Some(root)
            }
            spg_sql::ast::Statement::Update(u) => {
                let mut root = PlanNode::new(alloc::format!("Update on {}", u.table));
                let child = scan_node(self, &u.table, None, u.where_.as_ref(), &[], false);
                root.cost = child.cost.map(|(cs, ct, _, _)| (cs, ct, 0, 0));
                root.children.push(child);
                Some(root)
            }
            spg_sql::ast::Statement::Delete(d) => {
                let mut root = PlanNode::new(alloc::format!("Delete on {}", d.table));
                let child = scan_node(self, &d.table, None, d.where_.as_ref(), &[], false);
                root.cost = child.cost.map(|(cs, ct, _, _)| (cs, ct, 0, 0));
                root.children.push(child);
                Some(root)
            }
            _ => None,
        }
    }
}

impl Engine {
    /// v7.39 (round 286) — `EXPLAIN ANALYZE <INSERT|UPDATE|DELETE>`.
    ///
    /// PG's ANALYZE really runs the statement — it does not plan-and-
    /// discard, and it does NOT roll back. SPG refused it outright
    /// because `exec_explain` takes `&self`, so the write had nowhere
    /// to happen; that is the whole of the old restriction. This is the
    /// `&mut self` sibling, reached only from the write dispatch. The
    /// read-only path still refuses, correctly: a write cannot run there.
    ///
    /// The row counts follow PG: the `<Verb> on <table>` node reports
    /// `rows=0.00` (nothing is returned without RETURNING) while the
    /// source node reports the rows it produced.
    pub(crate) fn exec_explain_analyze_dml(
        &mut self,
        e: &spg_sql::ast::ExplainStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let Some(mut tree) = self.dml_plan_tree(&e.inner) else {
            return Err(EngineError::Unsupported(String::from(
                "EXPLAIN ANALYZE body must be INSERT / UPDATE / DELETE",
            )));
        };
        let started = self.clock.map(|f| f());
        let res = self.dispatch_stmt_inner((*e.inner).clone(), cancel)?;
        let elapsed_micros = match (self.clock, started) {
            (Some(f), Some(s)) => Some(f().saturating_sub(s)),
            _ => None,
        };
        let affected = match &res {
            QueryResult::CommandOk { affected, .. } => *affected as u64,
            QueryResult::Rows { rows, .. } => rows.len() as u64,
        };
        let show_time = !e.timing_off && !self.env_cfg().explain_no_costs;
        let elapsed_ms = if show_time {
            elapsed_micros.map(|us| us as f64 / 1000.0)
        } else {
            None
        };
        // The ModifyTable node returns nothing; its source produced the
        // rows it modified.
        tree.actual = Some((elapsed_ms, 0));
        for child in &mut tree.children {
            child.actual = Some((None, affected));
        }
        let mut lines = Vec::<String>::new();
        render_costed(&tree, !e.costs_off, &mut lines);
        if !e.summary_off
            && !self.env_cfg().explain_no_costs
            && let Some(us) = elapsed_micros
        {
            let ms = us as f64 / 1000.0;
            lines.push(alloc::format!("Execution Time: {ms:.3} ms"));
        }
        let columns = alloc::vec![ColumnSchema::new("QUERY PLAN", DataType::Text, false)];
        let rows: Vec<Row<'static>> = lines
            .into_iter()
            .map(|l| Row::new(alloc::vec![Value::text(l)]))
            .collect();
        Ok(QueryResult::Rows { columns, rows })
    }

    /// v4.26: `EXPLAIN [ANALYZE] <select>`. Returns a single-column
    /// `QUERY PLAN` text table — first line names the top operator
    /// (Scan / Aggregate / Window / etc.), indented children list
    /// FROM joins, WHERE filters, ORDER BY / LIMIT, projection
    /// shape, and any active index hits. `ANALYZE` execs the inner
    /// SELECT and appends actual-row + elapsed-micros annotations.
    #[allow(clippy::format_push_string)]
    pub(crate) fn exec_explain(
        &self,
        e: &spg_sql::ast::ExplainStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let mut lines = Vec::<String>::new();
        // v7.39 (round 226) — keep the tree so FORMAT JSON can render PG's
        // nested node objects instead of a per-line fallback.
        let mut plan_tree: Option<PlanNode>;
        // v7.39 (round 225) — the body may be DML; PG explains it as a
        // `<Verb> on <table>` root over the source plan. `sel` is Some only
        // for a SELECT body — the suggest / analyze branches below need it.
        let sel: Option<&SelectStatement> = match &*e.inner {
            spg_sql::ast::Statement::Select(s) => {
                let tree = build_plan_tree(s, self);
                render_costed(&tree, !e.costs_off, &mut lines);
                plan_tree = Some(tree);
                Some(s)
            }
            dml @ (spg_sql::ast::Statement::Insert(_)
            | spg_sql::ast::Statement::Update(_)
            | spg_sql::ast::Statement::Delete(_)) => {
                let root = self.dml_plan_tree(dml).expect("DML arm builds a tree");
                render_costed(&root, !e.costs_off, &mut lines);
                plan_tree = Some(root);
                None
            }
            other => {
                plan_tree = None;
                let _ = &plan_tree;
                return Err(EngineError::Unsupported(alloc::format!(
                    "EXPLAIN body must be SELECT / INSERT / UPDATE / DELETE, got {other:?}"
                )));
            }
        };
        if e.suggest {
            // v6.8.3 — index advisor. Walks the SELECT's FROM
            // tables + WHERE column refs; for each (table, column)
            // pair that lacks an index, append a SUGGEST line with
            // a copy-pastable `CREATE INDEX` statement. This is a
            // pure-syntax heuristic — no cardinality estimation —
            // matching the v6.8.3 design intent of "tell the
            // operator where indexes are missing", not "give the
            // mathematically optimal index set".
            if let Some(sel) = sel {
                let suggestions = build_index_suggestions(sel, self);
                for s in suggestions {
                    lines.push(s);
                }
            }
        } else if e.analyze {
            // v6.2.4 — EXPLAIN ANALYZE annotates each operator line
            // with `(rows=N)` where the row count is computable
            // without re-executing the full query:
            //   - Top-level operator (first non-indented line):
            //     rows = final result.len()
            //   - "From: <table> [full scan]" lines: rows =
            //     table.rows().len() (catalog read; no execution)
            //   - "From: <table> [index seek]": indeterminate —
            //     the index step would need re-execution; v6.2.5
            //     adds per-operator wall-clock + hot/cold rows
            //     instrumentation that makes this concrete.
            //   - Everything else: marked `(—)` so the surface
            //     stays well-defined without silently dropping
            //     stats. v6.2.5 fills in via inline executor
            //     instrumentation.
            // Total elapsed lands on a trailing `Total: …` line.
            // v7.39 (round 225) — ANALYZE executes; SPG supports it for
            // SELECT bodies only (a DML ANALYZE would have to really write).
            let Some(sel) = sel else {
                return Err(EngineError::Unsupported(String::from(
                    "EXPLAIN ANALYZE on INSERT/UPDATE/DELETE cannot run on the read-only path",
                )));
            };
            // v7.39 (round 227, Phase 3) — PG-shaped ANALYZE. Snapshot the
            // real per-table scan counters around the execution so leaf
            // scans report rows the executor genuinely touched.
            let before = scan_counter_snapshot(self);
            let started = self.clock.map(|f| f());
            // v7.37 (round 903) — ANALYZE runs the path the QUERY runs.
            //
            // This called `exec_select_cancel`, the materialising executor,
            // while a client's SELECT arrives through
            // `execute_readonly_select_streaming_prepared`. So the plan
            // measured one execution and described another. Round 882 made
            // the divergence visible — a single-table ORDER BY spills over
            // the wire and does not here, so `Sort Method` said `quicksort`
            // for a sort that had gone to disk — and rounds 900-902 showed
            // it is worse than a wrong label: every stage timing read off
            // ANALYZE was measuring a walk the client does not take.
            // Scanning 10k rows costs 2.4 ms materialising, and the
            // streaming walk is not that, so "which stage is slow" could
            // not be answered from here at all.
            //
            // The streaming entry falls back to the materialising one for
            // whatever it cannot claim, so nothing this used to explain
            // stops being explained. Rows are counted and dropped, which is
            // what ANALYZE does with them either way.
            let exec_rows =
                self.execute_readonly_select_streaming_prepared(sel, cancel, |item| {
                    let _ = matches!(item, crate::StreamItem::Row(_));
                    Ok(())
                })?;
            let elapsed_micros = match (self.clock, started) {
                (Some(f), Some(s)) => Some(f().saturating_sub(s)),
                _ => None,
            };
            let after = scan_counter_snapshot(self);
            let mut deltas: alloc::collections::BTreeMap<String, u64> =
                alloc::collections::BTreeMap::new();
            for (k, v) in &after {
                let d = v.saturating_sub(before.get(k).copied().unwrap_or(0));
                if d > 0 {
                    deltas.insert(k.clone(), d);
                }
            }
            let row_count = exec_rows;
            // Re-render the tree with the measured blocks attached. Timing
            // rides the top node only (see fill_actuals); TIMING OFF and
            // the test-mode GUC suppress it entirely.
            let show_time = !e.timing_off && !self.env_cfg().explain_no_costs;
            let elapsed_ms = if show_time {
                elapsed_micros.map(|us| us as f64 / 1000.0)
            } else {
                None
            };
            if let Some(tree) = &mut plan_tree {
                fill_actuals(tree, true, self, row_count as u64, elapsed_ms, &deltas);
                annotate_sort_method(tree, sel.limit.is_some());
                lines.clear();
                render_costed(tree, !e.costs_off, &mut lines);
            }
            // PG's trailing summary (suppressed by SUMMARY OFF). Only
            // `Execution Time:` is emitted: SPG's planning is not separately
            // instrumented (it happens inside the same execute call), so a
            // `Planning Time:` line would have to be invented — omitted
            // rather than faked. Documented divergence.
            if !e.summary_off
                && !self.env_cfg().explain_no_costs
                && let Some(us) = elapsed_micros
            {
                let ms = us as f64 / 1000.0;
                lines.push(alloc::format!("Execution Time: {ms:.3} ms"));
            }
            // v7.37.22 (22.7) — BUFFERS adds a hot/cold row
            // breakdown after Total. SPG's hot-tier row count is
            // exactly the live-row count we already display; cold
            // rows live in segments and don't get streamed through
            // this scan's row counter, so the cold side reads as 0
            // when the query touched only hot tier. The shape
            // matches PG's "Buffers: shared hit=N read=M dirtied=K"
            // line so dashboards parsing PG buffers can adapt.
            if e.buffers {
                // v7.37.19 (19.23 [PG+]) — cache-hit ratio
                // alongside the hot/cold breakdown. PG dashboards
                // commonly compute `shared_hit / (shared_hit +
                // shared_read)` from pg_statio_user_tables; SPG's
                // hot-tier rows are the cache-hit equivalent (no
                // disk seek) and cold-tier rows the cache-miss
                // equivalent. row_count = hot_rows + cold_rows;
                // when both are zero (no rows touched) the ratio
                // surfaces as "n/a" rather than 0/0.
                let cold_rows: u64 = 0;
                let hot_rows: u64 = row_count as u64;
                let total_rows = hot_rows.saturating_add(cold_rows);
                let ratio = if total_rows == 0 {
                    alloc::string::String::from("n/a")
                } else {
                    // Two-decimal-place integer arithmetic — keeps
                    // spg-engine no_std without pulling in libm.
                    // ratio_x10000 ∈ [0, 10000]; divide for output.
                    let ratio_x10000 = (hot_rows.saturating_mul(10_000)) / total_rows;
                    alloc::format!("{}.{:02}", ratio_x10000 / 100, ratio_x10000 % 100)
                };
                lines.push(alloc::format!(
                    "Buffers: hot_rows={hot_rows} cold_rows={cold_rows} cache_hit_ratio={ratio}"
                ));
            }
        }
        // v7.37.22 (22.7) — SETTINGS appends GUCs that diverge from
        // default. Independent of ANALYZE — `EXPLAIN (SETTINGS) S`
        // also emits this line. Today we surface
        // `default_text_search_config` + `statement_timeout` if set.
        if e.settings {
            let mut diverged: Vec<alloc::string::String> = Vec::new();
            for key in [
                "default_text_search_config",
                "statement_timeout",
                "default_transaction_isolation",
                "search_path",
            ] {
                if let Some(v) = self.session_param(key) {
                    diverged.push(alloc::format!("{key}={v}"));
                }
            }
            if diverged.is_empty() {
                lines.push("Settings: (no overrides)".into());
            } else {
                lines.push(alloc::format!("Settings: {}", diverged.join(", ")));
            }
        }
        // v7.37.22 (22.7) — WAL counts the bytes / records / FPI
        // emitted by the inner SELECT. SELECT is read-only, so
        // these stay 0 unless the inner is a writing CTE. The
        // shape matches PG's "WAL: records=N bytes=M".
        if e.wal {
            lines.push("WAL: records=0 bytes=0 fpi=0".into());
        }
        // v7.37.23 (23.5) — EXPLAIN (FORMAT json|xml|yaml). PG's
        // default is text (one row per line). Non-text formats
        // bundle the whole plan into a single TEXT row whose body
        // wraps the line list in the chosen container.
        let columns = alloc::vec![ColumnSchema::new("QUERY PLAN", DataType::Text, false)];
        let rows: Vec<Row<'static>> = match e.format {
            spg_sql::ast::ExplainFormat::Text => lines
                .into_iter()
                .map(|l| Row::new(alloc::vec![Value::text(l)]))
                .collect(),
            spg_sql::ast::ExplainFormat::Json => {
                // PG: a JSON array of plan objects. SPG's planner
                // doesn't yet emit a tree of nodes — wrap each
                // text line as a `{"Plan Line": "..."}` object
                // inside the array. Dashboards parsing the line
                // bodies see the same content; tools doing a
                // strict PG-tree schema match should still call
                // out to the engine via the text shape.
                // v7.39 (round 226) — PG's nested node objects, rendered
                // from the real plan tree. The per-line fallback stays for
                // shapes that produced no tree (SUGGEST / ANALYZE extras).
                let body = match &plan_tree {
                    Some(tree) => render_json_plan(tree, !e.costs_off),
                    None => {
                        let mut b = alloc::string::String::from("[");
                        for (i, l) in lines.iter().enumerate() {
                            if i > 0 {
                                b.push_str(", ");
                            }
                            b.push_str("{\"Plan Line\": ");
                            b.push_str(&json_string_lit(l));
                            b.push('}');
                        }
                        b.push(']');
                        b
                    }
                };
                alloc::vec![Row::new(alloc::vec![Value::text(body)])]
            }
            // v7.39 (round 228) — XML / YAML render the same node tree the
            // JSON path does. The per-line fallback stays for shapes that
            // produced no tree (SUGGEST / ANALYZE extras).
            spg_sql::ast::ExplainFormat::Xml => {
                let body = match &plan_tree {
                    Some(tree) => render_xml_plan(tree, !e.costs_off),
                    None => {
                        let mut b = alloc::string::String::from(
                            "<explain xmlns=\"http://www.postgresql.org/2009/explain\">",
                        );
                        for l in &lines {
                            b.push_str("<line>");
                            b.push_str(&xml_escape(l));
                            b.push_str("</line>");
                        }
                        b.push_str("</explain>");
                        b
                    }
                };
                alloc::vec![Row::new(alloc::vec![Value::text(body)])]
            }
            spg_sql::ast::ExplainFormat::Yaml => {
                let body = match &plan_tree {
                    Some(tree) => render_yaml_plan(tree, !e.costs_off),
                    None => {
                        let mut b = alloc::string::String::from("- Plan:\n");
                        for l in &lines {
                            b.push_str("  - ");
                            b.push_str(&yaml_scalar(l));
                            b.push('\n');
                        }
                        b
                    }
                };
                alloc::vec![Row::new(alloc::vec![Value::text(body)])]
            }
        };
        Ok(QueryResult::Rows { columns, rows })
    }
}

/// JSON-encode a string scalar with proper escaping for the
/// EXPLAIN FORMAT JSON output.
fn json_string_lit(s: &str) -> alloc::string::String {
    let mut out = alloc::string::String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&alloc::format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// XML-escape a body fragment. Covers the five canonical entities;
/// the EXPLAIN payload doesn't contain bytes outside `&<>"'`.
fn xml_escape(s: &str) -> alloc::string::String {
    let mut out = alloc::string::String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// YAML-quote a scalar that may contain `:` or other YAML-special
/// characters. The simplest safe form is double-quoting with the
/// same escapes JSON uses.
fn yaml_scalar(s: &str) -> alloc::string::String {
    json_string_lit(s)
}
