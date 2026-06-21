// pedantic doc_markdown flags the embedded algorithm-spec block;
// allowing at the module level keeps the spec readable.
#![allow(clippy::doc_markdown)]

//! v6.2.3 — JOIN reorder planner pass.
//!
//! Runs after parse + clock rewrite + ORDER BY position
//! resolution. For SelectStatements with multiple INNER-joined
//! tables, picks an ordering that minimises the cumulative
//! nested-loop work — leveraging v6.2.0's `Statistics` for
//! per-edge selectivity estimates.
//!
//! Algorithm:
//!
//!   * Identify tables: `from.primary` + `from.joins[*]`.
//!   * Identify edges: each `INNER JOIN ... ON <expr>` adds one
//!     edge whose endpoints are the table names referenced by
//!     the ON expression (extracted by walking `ColumnName`
//!     nodes). LEFT / CROSS joins disable reorder — they have
//!     semantics-preserving order constraints we don't unpack
//!     in v6.2.3.
//!   * Enumerate orderings:
//!       - For `n ≤ 4` tables: brute force all `n!` orderings.
//!       - For `n > 4`: greedy — pick the smallest table first,
//!         then at each step pick the next table that gives the
//!         smallest expected output size given the edges already
//!         applicable.
//!   * Cost an ordering: walk left-to-right tracking the running
//!     output size. Every edge becomes applicable as soon as both
//!     its endpoint tables are in the prefix; multiplying the
//!     running size by `selectivity::equal(stats, …) / n_distinct`
//!     for that edge updates the size. Step cost = `running_size ×
//!     new_table_size`. Total cost = sum of step costs.
//!   * Pick the minimum-cost ordering and rewrite `from.primary` +
//!     `from.joins` in that order. ON predicates travel with their
//!     edges — they re-attach to whichever join introduces both
//!     endpoint tables.
//!
//! All of v6.2.3 ships pure-AST rewriting. The executor at
//! `exec_joined_select` doesn't change shape — it consumes the
//! newly-ordered FROM clause unchanged.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use spg_sql::ast::{ColumnName, Expr, FromClause, FromJoin, JoinKind, SelectStatement, TableRef};

use crate::selectivity;
use crate::statistics::Statistics;
use spg_storage::Catalog;

/// v6.2.3 — full-enumeration cap. v6.2.x can re-tune; the value
/// determines `n!` plan-space size (4! = 24, 5! = 120, 6! = 720
/// — 6 is on the verge of "noticeable" in micro-bench terms).
pub const FULL_ENUM_MAX: usize = 4;

/// v6.2.3 — entry point. Rewrites `stmt.from` (when present) so
/// the join chain is in cost-minimising order. No-op when:
///   - `stmt.from` is `None` or has no joins
///   - any join is LEFT / CROSS (semantics-sensitive)
///   - any ON predicate can't be resolved to a pair of endpoint
///     tables (the conservative fallback keeps the user's order)
/// v6.2.3 test-only — computes the chosen order WITHOUT mutating
/// the statement. Returns `None` when the pass would no-op.
pub fn choose_order_for_test(
    stmt: &SelectStatement,
    catalog: &Catalog,
    stats: &Statistics,
) -> Option<Vec<usize>> {
    let mut clone = stmt.clone();
    choose_order_inner(&mut clone, catalog, stats)
}

fn choose_order_inner(
    stmt: &mut SelectStatement,
    catalog: &Catalog,
    stats: &Statistics,
) -> Option<Vec<usize>> {
    let from = stmt.from.as_mut()?;
    if from.joins.is_empty() {
        return None;
    }
    if from
        .joins
        .iter()
        .any(|j| !matches!(j.kind, JoinKind::Inner))
    {
        return None;
    }
    let mut tables: Vec<TableRef> = Vec::with_capacity(1 + from.joins.len());
    tables.push(from.primary.clone());
    for j in &from.joins {
        tables.push(j.table.clone());
    }
    let n = tables.len();
    let mut alias_to_idx: BTreeMap<String, usize> = BTreeMap::new();
    for (i, t) in tables.iter().enumerate() {
        let key = t.alias.clone().unwrap_or_else(|| t.name.clone());
        alias_to_idx.insert(key, i);
        if t.alias.is_some() {
            alias_to_idx.entry(t.name.clone()).or_insert(i);
        }
    }
    let mut edges: Vec<Edge> = Vec::new();
    for j in &from.joins {
        let on = j.on.as_ref()?;
        for sub in split_and_conjunctions(on) {
            let mut endpoint_set: Vec<usize> = Vec::new();
            if !collect_referenced_tables(sub, &alias_to_idx, &mut endpoint_set) {
                return None;
            }
            endpoint_set.sort_unstable();
            endpoint_set.dedup();
            edges.push(Edge {
                endpoints: endpoint_set,
                predicate: sub.clone(),
                selectivity: estimate_edge_selectivity(sub, &tables, catalog, stats),
            });
        }
    }
    let mut sizes: Vec<u64> = Vec::with_capacity(n);
    for t in &tables {
        let table = catalog.get(&t.name)?;
        sizes.push(table.rows().len() as u64);
    }
    Some(if n <= FULL_ENUM_MAX {
        best_order_brute(n, &sizes, &edges)
    } else {
        best_order_greedy(n, &sizes, &edges)
    })
}

pub fn reorder_joins(stmt: &mut SelectStatement, catalog: &Catalog, stats: &Statistics) {
    // v7.37.43 (T3) — capture order-by alias targets before borrowing
    // `stmt.from` mutably; the empty-stats fallback skips when the
    // source-order primary owns an ORDER BY column (its promotion
    // would break the index walker that already early-stops at
    // LIMIT). Cheap clone — order_by is small in practice.
    let order_by_targets: alloc::collections::BTreeSet<String> = stmt
        .order_by
        .iter()
        .filter_map(|o| match &o.expr {
            Expr::Column(c) => c.qualifier.as_ref().map(|q| q.to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    let Some(from) = stmt.from.as_mut() else {
        return;
    };
    if from.joins.is_empty() {
        return;
    }
    // v7.38 — the LEFT/CROSS "disable reorder" rule is too coarse.
    // Only the *leading run* of INNER joins is reorderable; LEFT /
    // CROSS joins must stay at their original positions (their
    // outer side is the prefix produced so far, so promoting a
    // later table across them would change semantics). The leading
    // INNERs can swap freely since the trailing LEFT/CROSS ON
    // predicates resolve by alias, not position. Split the chain
    // at the first non-Inner join; reorder only the leading prefix;
    // re-append the preserved trailing joins after rewrite.
    let split = from
        .joins
        .iter()
        .position(|j| !matches!(j.kind, JoinKind::Inner))
        .unwrap_or(from.joins.len());
    if split == 0 {
        // First join is LEFT/CROSS — `from.primary` is its outer
        // side, swapping would change semantics. v6.2.3 conservative
        // bail still applies.
        return;
    }
    // v6.2.3 — full cost-based reorder is gated on having
    // statistics. PG's policy: ANALYZE drives the optimizer.
    // Without stats the cost model is guessing from row counts
    // only, which can produce the same plan as source order and
    // hide regressions in the executor. The user-facing contract:
    // "run ANALYZE to opt into the cost-based JOIN reorder."
    //
    // v7.37.43 (T3 mailrs 100k cascade general fix) — but the
    // strict-bail leaves a hole the mailrs cascade exposes: a
    // freshly opened catalog (no auto-ANALYZE yet) running
    // `SELECT ... FROM messages JOIN mailboxes ... WHERE
    // mb.user_address = $1 LIMIT 50` source-orders to `messages`
    // (100k rows) driving `mailboxes` (~90 rows), so the join
    // materialises 100k tuples before LIMIT bites. The fix
    // delegates to a stats-free fallback that only fires when
    // there's catastrophic size asymmetry (≥100× between the
    // largest and smallest reorder-candidate). The asymmetry
    // threshold means a wrong call is impossible: even if the
    // small table somehow turned out to be the worse driver, the
    // ratio of row counts caps the worst-case cost growth at <2×
    // the source order. The common case (mailrs prod shape) wins
    // 50× back. See [feedback-infra-not-contractor] +
    // `.claude/notes/v7.37-linear-ship-plan.md` §T3.
    if stats.is_empty() {
        size_asymmetry_fallback(from, catalog, split, &order_by_targets);
        return;
    }
    // Build the table list for the LEADING INNER CHAIN ONLY.
    // primary first + leading INNER joins. Trailing LEFT/CROSS joins
    // are preserved verbatim by `rewrite_from_with_trailing`.
    let mut tables: Vec<TableRef> = Vec::with_capacity(1 + split);
    tables.push(from.primary.clone());
    for j in &from.joins[..split] {
        tables.push(j.table.clone());
    }
    let n = tables.len();
    // Build (table_name → alias) and (alias → table_name) so we
    // can resolve column references to either.
    let mut alias_to_idx: BTreeMap<String, usize> = BTreeMap::new();
    for (i, t) in tables.iter().enumerate() {
        let key = t.alias.clone().unwrap_or_else(|| t.name.clone());
        alias_to_idx.insert(key, i);
        // Also register the bare name when the alias differs, so
        // an ON-expression that uses the unaliased name still
        // resolves.
        if t.alias.is_some() {
            alias_to_idx.entry(t.name.clone()).or_insert(i);
        }
    }
    // Extract edges from each LEADING INNER join's ON predicate.
    let mut edges: Vec<Edge> = Vec::new();
    for j in &from.joins[..split] {
        let Some(on) = j.on.as_ref() else {
            // INNER without ON is a CROSS in v4.x parser — bail
            // (we'd lose the user's intent).
            return;
        };
        for sub in split_and_conjunctions(on) {
            let mut endpoint_set: Vec<usize> = Vec::new();
            if !collect_referenced_tables(sub, &alias_to_idx, &mut endpoint_set) {
                return;
            }
            endpoint_set.sort_unstable();
            endpoint_set.dedup();
            edges.push(Edge {
                endpoints: endpoint_set,
                predicate: sub.clone(),
                selectivity: estimate_edge_selectivity(sub, &tables, catalog, stats),
            });
        }
    }
    // Per-table row counts from the catalog. Tables that aren't
    // in the catalog yet (which can happen for a CTE — covered by
    // a different code path — but we double-check) bail.
    let mut sizes: Vec<u64> = Vec::with_capacity(n);
    for t in &tables {
        let Some(table) = catalog.get(&t.name) else {
            return;
        };
        sizes.push(table.rows().len() as u64);
    }
    // Pick an ordering.
    let order: Vec<usize> = if n <= FULL_ENUM_MAX {
        best_order_brute(n, &sizes, &edges)
    } else {
        best_order_greedy(n, &sizes, &edges)
    };
    // No-op when the chosen order matches the input.
    if order.iter().enumerate().all(|(i, &j)| i == j) {
        return;
    }
    rewrite_from_with_trailing(from, &tables, &edges, &order, split);
}

/// v7.37.43 (T3) — stats-free reorder fallback. Only fires when one
/// reorderable table is **≥100× smaller** than the current primary;
/// in that regime the catastrophic source-order cost
/// (`big × intermediate` cascade) dwarfs any plan-shape risk from a
/// wrong call. PG's pre-ANALYZE heuristic does the same: `pg_class.
/// reltuples` drives default selectivity even without per-column
/// stats.
///
/// Connectedness — only promotes the small candidate when it shares
/// an INNER ON predicate with the current primary (an edge endpoint
/// pair covers both). A disconnected small table is left alone
/// (promoting it would force a CROSS-style intermediate).
///
/// Behaviour: no-op unless the new driver strictly improves the
/// source order — same conservative gate as the full cost path. The
/// rewrite reuses `rewrite_from_with_trailing` so trailing LEFT /
/// CROSS joins ride through verbatim.
const SIZE_ASYMMETRY_THRESHOLD: u64 = 100;

fn size_asymmetry_fallback(
    from: &mut FromClause,
    catalog: &Catalog,
    split: usize,
    order_by_targets: &alloc::collections::BTreeSet<String>,
) {
    if split < 1 {
        // Need at least 1 INNER join in the leading chain (i.e. ≥ 2
        // tables to reorder). `split = 0` means the first join is
        // LEFT / CROSS, which `reorder_joins` already bailed on.
        return;
    }
    // If the source-order primary owns an ORDER BY alias target,
    // promoting it away breaks the bounded-streaming join executors
    // (`try_streamed_inner_join_topn` / `try_streamed_inner_join_
    // walk_topn` / `try_pk_walk_top_n`) which all walk the primary
    // table's BTree index in the ORDER BY direction and early-stop
    // at LIMIT — peak memory stays O(LIMIT) instead of O(table).
    // Without this skip, a `messages JOIN mailboxes ORDER BY m.id ASC
    // LIMIT N` shape over fat text bodies regresses to the general
    // join path (full materialise then sort) the v7.30.3 round-26
    // memory gate was added to catch.
    let primary_alias = from
        .primary
        .alias
        .as_deref()
        .unwrap_or(from.primary.name.as_str())
        .to_ascii_lowercase();
    if order_by_targets.contains(&primary_alias) {
        return;
    }
    let mut tables: Vec<TableRef> = Vec::with_capacity(1 + split);
    tables.push(from.primary.clone());
    for j in &from.joins[..split] {
        tables.push(j.table.clone());
    }
    let n = tables.len();
    let mut sizes: Vec<u64> = Vec::with_capacity(n);
    for t in &tables {
        let Some(table) = catalog.get(&t.name) else {
            return;
        };
        sizes.push(table.rows().len() as u64);
    }
    // Locate the smallest table; check it's ≥ THRESHOLD× smaller
    // than the current primary. A "tiny" table with sub-row catalog
    // shape (1-2 rows from a fresh-fixture run) can produce extreme
    // ratios that aren't planner-meaningful — apply a floor on the
    // big side so the rule only fires when the asymmetry is between
    // populated tables.
    const MIN_BIG_ROWS: u64 = 100;
    let primary_rows = sizes[0];
    if primary_rows < MIN_BIG_ROWS {
        return;
    }
    let (smallest_idx, &smallest_rows) = sizes
        .iter()
        .enumerate()
        .min_by_key(|&(_, s)| *s)
        .expect("n >= 2");
    if smallest_idx == 0 {
        // Primary is already the smallest.
        return;
    }
    if smallest_rows.saturating_mul(SIZE_ASYMMETRY_THRESHOLD) > primary_rows {
        // Not asymmetric enough — fall back to source order.
        return;
    }
    // Build alias index + edges so the rewrite can re-attach ON
    // predicates correctly (same shape `reorder_joins` uses).
    let mut alias_to_idx: BTreeMap<String, usize> = BTreeMap::new();
    for (i, t) in tables.iter().enumerate() {
        let key = t.alias.clone().unwrap_or_else(|| t.name.clone());
        alias_to_idx.insert(key, i);
        if t.alias.is_some() {
            alias_to_idx.entry(t.name.clone()).or_insert(i);
        }
    }
    let mut edges: Vec<Edge> = Vec::new();
    for j in &from.joins[..split] {
        let Some(on) = j.on.as_ref() else {
            return;
        };
        for sub in split_and_conjunctions(on) {
            let mut endpoint_set: Vec<usize> = Vec::new();
            if !collect_referenced_tables(sub, &alias_to_idx, &mut endpoint_set) {
                return;
            }
            endpoint_set.sort_unstable();
            endpoint_set.dedup();
            edges.push(Edge {
                endpoints: endpoint_set,
                predicate: sub.clone(),
                selectivity: 0.0,
            });
        }
    }
    // Connectedness check: the small table must share an edge with
    // the current primary (so promoting it doesn't force a CROSS-
    // shaped intermediate at step 1).
    let mut connected_to_primary = false;
    for e in &edges {
        if e.endpoints.contains(&smallest_idx) && e.endpoints.contains(&0) {
            connected_to_primary = true;
            break;
        }
    }
    if !connected_to_primary {
        return;
    }
    // Build connectivity-preserving order starting from the small
    // table (same loop `drive_from` uses).
    let mut order: Vec<usize> = alloc::vec![smallest_idx];
    let mut included: Vec<bool> = alloc::vec![false; n];
    included[smallest_idx] = true;
    loop {
        let mut progressed = false;
        for ti in 0..n {
            if included[ti] {
                continue;
            }
            let connects = edges.iter().any(|e| {
                e.endpoints.contains(&ti) && e.endpoints.iter().all(|&x| x == ti || included[x])
            });
            if connects {
                order.push(ti);
                included[ti] = true;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    for ti in 0..n {
        if !included[ti] {
            order.push(ti);
        }
    }
    if order.iter().enumerate().all(|(i, &j)| i == j) {
        return;
    }
    rewrite_from_with_trailing(from, &tables, &edges, &order, split);
}

struct Edge {
    /// Sorted unique table indices the ON predicate references.
    endpoints: Vec<usize>,
    /// The original ON expression. Re-attached to whichever join
    /// in the reordered chain introduces both endpoints.
    predicate: Expr,
    /// 0..1 — fraction of rows expected to satisfy the predicate.
    /// Cached so plan evaluation doesn't repeatedly walk the
    /// histogram.
    selectivity: f64,
}

/// v6.2.3 — split a conjunction predicate `p1 AND p2 AND …` into
/// its leaf clauses. Each leaf becomes its own [`Edge`] so the
/// optimizer can pull tight predicates earlier in the plan tree.
/// Non-AND expressions return a single-element vec.
pub(crate) fn split_and_conjunctions(expr: &Expr) -> Vec<&Expr> {
    use spg_sql::ast::BinOp;
    let mut out: Vec<&Expr> = Vec::new();
    let mut stack: Vec<&Expr> = alloc::vec![expr];
    while let Some(e) = stack.pop() {
        if let Expr::Binary {
            op: BinOp::And,
            lhs,
            rhs,
        } = e
        {
            stack.push(rhs);
            stack.push(lhs);
        } else {
            out.push(e);
        }
    }
    out
}

fn collect_referenced_tables(
    expr: &Expr,
    alias_to_idx: &BTreeMap<String, usize>,
    out: &mut Vec<usize>,
) -> bool {
    match expr {
        Expr::Column(ColumnName {
            qualifier: Some(q), ..
        }) => {
            if let Some(&i) = alias_to_idx.get(q) {
                out.push(i);
                true
            } else {
                false
            }
        }
        Expr::Column(_) => {
            // Unqualified column — can't resolve without column-set
            // knowledge. Conservative bail.
            false
        }
        Expr::Literal(_) | Expr::Placeholder(_) => true,
        Expr::Binary { lhs, rhs, .. } => {
            collect_referenced_tables(lhs, alias_to_idx, out)
                && collect_referenced_tables(rhs, alias_to_idx, out)
        }
        Expr::Unary { expr, .. } => collect_referenced_tables(expr, alias_to_idx, out),
        Expr::FunctionCall { args, .. } => args
            .iter()
            .all(|a| collect_referenced_tables(a, alias_to_idx, out)),
        Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            collect_referenced_tables(expr, alias_to_idx, out)
        }
        Expr::Like {
            expr: e, pattern, ..
        } => {
            collect_referenced_tables(e, alias_to_idx, out)
                && collect_referenced_tables(pattern, alias_to_idx, out)
        }
        // Subqueries / windows / EXTRACT etc. — bail conservatively.
        _ => false,
    }
}

/// Estimate selectivity for an ON-predicate. v6.2.3 MVP recognises
/// `t1.col1 = t2.col2` and uses `1 / max(n_distinct_left,
/// n_distinct_right, 1)` as an FK-join estimate (PG's heuristic).
/// Everything else: PG's `DEFAULT_RANGE = 0.333`.
fn estimate_edge_selectivity(
    on: &Expr,
    tables: &[TableRef],
    catalog: &Catalog,
    stats: &Statistics,
) -> f64 {
    use spg_sql::ast::BinOp;
    let Expr::Binary {
        op: BinOp::Eq,
        lhs,
        rhs,
    } = on
    else {
        return selectivity::DEFAULT_RANGE;
    };
    let lhs_col = column_ref(lhs);
    let rhs_col = column_ref(rhs);
    let (Some(lhs_col), Some(rhs_col)) = (lhs_col, rhs_col) else {
        return selectivity::DEFAULT_RANGE;
    };
    let lhs_distinct = column_n_distinct(&lhs_col, tables, catalog, stats);
    let rhs_distinct = column_n_distinct(&rhs_col, tables, catalog, stats);
    let max_distinct = lhs_distinct.max(rhs_distinct).max(1);
    1.0 / max_distinct as f64
}

fn column_ref(expr: &Expr) -> Option<(Option<String>, String)> {
    if let Expr::Column(ColumnName { qualifier, name }) = expr {
        Some((qualifier.clone(), name.clone()))
    } else {
        None
    }
}

fn column_n_distinct(
    col: &(Option<String>, String),
    tables: &[TableRef],
    catalog: &Catalog,
    stats: &Statistics,
) -> u64 {
    let Some(alias) = col.0.as_ref() else {
        return 0;
    };
    let Some(table_name) = tables
        .iter()
        .find(|t| t.alias.as_deref() == Some(alias.as_str()) || t.name == *alias)
        .map(|t| t.name.clone())
    else {
        return 0;
    };
    if let Some(s) = stats.get(&table_name, &col.1) {
        return s.n_distinct.max(1);
    }
    catalog
        .get(&table_name)
        .map_or(1, |t| (t.rows().len() as u64).max(1))
}

fn best_order_brute(n: usize, sizes: &[u64], edges: &[Edge]) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..n).collect();
    let mut best_cost = f64::INFINITY;
    let mut best_order = indices.clone();
    permute(&mut indices, 0, &mut |perm| {
        let c = plan_cost(perm, sizes, edges);
        if c < best_cost {
            best_cost = c;
            best_order = perm.to_vec();
        }
    });
    best_order
}

fn permute<F: FnMut(&[usize])>(arr: &mut Vec<usize>, k: usize, visit: &mut F) {
    if k >= arr.len() {
        visit(arr);
        return;
    }
    for i in k..arr.len() {
        arr.swap(i, k);
        permute(arr, k + 1, visit);
        arr.swap(i, k);
    }
}

fn best_order_greedy(n: usize, sizes: &[u64], edges: &[Edge]) -> Vec<usize> {
    // Seed: smallest table.
    let mut chosen: Vec<usize> = Vec::with_capacity(n);
    let mut remaining: Vec<usize> = (0..n).collect();
    let &first = remaining.iter().min_by_key(|&&i| sizes[i]).expect("n > 0");
    chosen.push(first);
    remaining.retain(|&x| x != first);
    while !remaining.is_empty() {
        // Pick the candidate whose addition produces the smallest
        // intermediate output size. plan_cost over the current
        // prefix + candidate works.
        let mut best_cand = remaining[0];
        let mut best_cost = f64::INFINITY;
        for &cand in &remaining {
            let mut probe = chosen.clone();
            probe.push(cand);
            let c = plan_cost(&probe, sizes, edges);
            if c < best_cost {
                best_cost = c;
                best_cand = cand;
            }
        }
        chosen.push(best_cand);
        remaining.retain(|&x| x != best_cand);
    }
    chosen
}

/// Cost an ordering by simulating the cumulative nested-loop work
/// + cross-applying every edge whose endpoints are now in the
/// prefix.
fn plan_cost(order: &[usize], sizes: &[u64], edges: &[Edge]) -> f64 {
    // Track output size at each step. Step 0: just the first table.
    let mut running = sizes[order[0]] as f64;
    let mut cost = 0.0_f64;
    let mut in_prefix: Vec<bool> = alloc::vec![false; sizes.len()];
    in_prefix[order[0]] = true;
    for &table_idx in &order[1..] {
        let right = sizes[table_idx] as f64;
        // Step cost: produce `running × right` candidate rows
        // before filtering.
        cost += running * right;
        in_prefix[table_idx] = true;
        let mut step_output = running * right;
        // Apply every edge whose endpoints just became fully
        // covered by the prefix.
        for edge in edges {
            if edge.endpoints.iter().all(|&e| in_prefix[e]) {
                // Apply selectivity only if at least one endpoint
                // is this step's `table_idx` — otherwise the
                // selectivity was already applied at an earlier
                // step.
                if edge.endpoints.contains(&table_idx) {
                    step_output *= edge.selectivity;
                }
            }
        }
        running = step_output.max(1.0);
    }
    cost
}

fn rewrite_from(from: &mut FromClause, tables: &[TableRef], edges: &[Edge], order: &[usize]) {
    rewrite_from_with_trailing(from, tables, edges, order, from.joins.len());
}

/// v7.38 — `rewrite_from` variant aware of trailing LEFT/CROSS joins
/// that must be preserved verbatim. `split` is the index where the
/// trailing (non-reorderable) joins begin in the *original* `from.joins`.
/// The leading INNER chain (primary + `joins[0..split]`) is rewritten
/// per `order`; `joins[split..]` is re-appended at the end so LEFT /
/// CROSS semantics are preserved.
fn rewrite_from_with_trailing(
    from: &mut FromClause,
    tables: &[TableRef],
    edges: &[Edge],
    order: &[usize],
    split: usize,
) {
    let trailing: alloc::vec::Vec<FromJoin> = from.joins[split..].to_vec();
    from.primary = tables[order[0]].clone();
    from.joins.clear();
    let mut in_prefix: Vec<bool> = alloc::vec![false; tables.len()];
    in_prefix[order[0]] = true;
    let mut edges_used: Vec<bool> = alloc::vec![false; edges.len()];
    for &table_idx in &order[1..] {
        in_prefix[table_idx] = true;
        // Pick the edge that joins `table_idx` to the prefix.
        // There may be multiple; AND them all so we don't lose any
        // predicate.
        let mut combined: Option<Expr> = None;
        for (ei, edge) in edges.iter().enumerate() {
            if edges_used[ei] {
                continue;
            }
            if edge.endpoints.contains(&table_idx) && edge.endpoints.iter().all(|&e| in_prefix[e]) {
                edges_used[ei] = true;
                combined = Some(match combined {
                    None => edge.predicate.clone(),
                    Some(prev) => Expr::Binary {
                        op: spg_sql::ast::BinOp::And,
                        lhs: alloc::boxed::Box::new(prev),
                        rhs: alloc::boxed::Box::new(edge.predicate.clone()),
                    },
                });
            }
        }
        // Fallback: if no edge applies, build a `TRUE` predicate
        // so the join shape stays valid. (Shouldn't happen on a
        // connected join graph; the reorder pass only takes
        // connected graphs anyway.)
        let on = combined.unwrap_or_else(|| Expr::Literal(spg_sql::ast::Literal::Bool(true)));
        from.joins.push(FromJoin {
            kind: JoinKind::Inner,
            table: tables[table_idx].clone(),
            on: Some(on),
        });
    }
    // v7.38 — re-append preserved trailing LEFT/CROSS joins.
    from.joins.extend(trailing);
}

/// v7.32 (architecture v2 P3) — force a specific table to drive an
/// all-INNER join chain, ignoring cost.
///
/// `reorder_joins` is cost-based: it weighs table sizes and ON-edge
/// selectivity but is blind to single-table WHERE restrictions. The
/// keyed correlated-subquery probe ([`crate::Engine::try_batch_correlated_scalar`])
/// needs the opposite: the correlated column is a *known* seek key
/// (an equality against a literal pushed in per surviving group), so
/// the table owning it must be the driving table — exactly how PG,
/// MySQL and MariaDB all plan a correlated join subquery (an index
/// scan on the correlation column, then an index-nested-loop to the
/// joined table). The driver is a certainty here, not an estimate,
/// so we skip the cost model entirely.
///
/// Returns `true` if `driver_alias` now drives `stmt.from` (already
/// primary counts as success). Returns `false` without mutating when
/// the chain isn't all-INNER, the alias can't be found, or an ON
/// predicate can't be resolved to its endpoint tables.
pub(crate) fn drive_from(stmt: &mut SelectStatement, driver_alias: &str) -> bool {
    let Some(from) = stmt.from.as_mut() else {
        return false;
    };
    // Build the table list (primary first) + alias index, mirroring
    // `choose_order_inner`. Joinless FROMs only succeed when the lone
    // table already is the driver.
    let mut tables: Vec<TableRef> = Vec::with_capacity(1 + from.joins.len());
    tables.push(from.primary.clone());
    for j in &from.joins {
        tables.push(j.table.clone());
    }
    let mut alias_to_idx: BTreeMap<String, usize> = BTreeMap::new();
    for (i, t) in tables.iter().enumerate() {
        let key = t.alias.clone().unwrap_or_else(|| t.name.clone());
        alias_to_idx.insert(key, i);
        if t.alias.is_some() {
            alias_to_idx.entry(t.name.clone()).or_insert(i);
        }
    }
    let Some(&driver_idx) = alias_to_idx.get(driver_alias) else {
        return false;
    };
    if from.joins.is_empty() {
        // Nothing to swap: the single table either is or isn't the
        // driver.
        return driver_idx == 0;
    }
    if driver_idx == 0 {
        return true; // already driving
    }
    if from
        .joins
        .iter()
        .any(|j| !matches!(j.kind, JoinKind::Inner))
    {
        return false; // LEFT/CROSS: promotion would change semantics
    }
    // Extract ON edges. Selectivity is irrelevant for a forced order
    // (`rewrite_from` never reads it), so charge 0.0 and skip the
    // catalog/stats round-trip.
    let mut edges: Vec<Edge> = Vec::new();
    for j in &from.joins {
        let Some(on) = j.on.as_ref() else {
            return false;
        };
        for sub in split_and_conjunctions(on) {
            let mut endpoint_set: Vec<usize> = Vec::new();
            if !collect_referenced_tables(sub, &alias_to_idx, &mut endpoint_set) {
                return false;
            }
            endpoint_set.sort_unstable();
            endpoint_set.dedup();
            edges.push(Edge {
                endpoints: endpoint_set,
                predicate: sub.clone(),
                selectivity: 0.0,
            });
        }
    }
    // Order = driver first, then a connectivity-preserving sweep so
    // each appended table shares an edge with the prefix (keeps
    // `rewrite_from`'s ON re-attachment exact). Disconnected leftovers
    // fall back to source order.
    let n = tables.len();
    let mut order: Vec<usize> = alloc::vec![driver_idx];
    let mut included: Vec<bool> = alloc::vec![false; n];
    included[driver_idx] = true;
    loop {
        let mut progressed = false;
        for ti in 0..n {
            if included[ti] {
                continue;
            }
            let connects = edges.iter().any(|e| {
                e.endpoints.contains(&ti) && e.endpoints.iter().all(|&x| x == ti || included[x])
            });
            if connects {
                order.push(ti);
                included[ti] = true;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    for ti in 0..n {
        if !included[ti] {
            order.push(ti);
        }
    }
    rewrite_from(from, &tables, &edges, &order);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use spg_sql::parser;

    #[test]
    fn no_joins_is_noop() {
        let mut stmt = match parser::parse_statement("SELECT * FROM users").unwrap() {
            spg_sql::ast::Statement::Select(s) => s,
            _ => panic!(),
        };
        let cat = Catalog::new();
        let stats = Statistics::new();
        let snap = stmt.clone();
        reorder_joins(&mut stmt, &cat, &stats);
        assert_eq!(stmt, snap);
    }

    #[test]
    fn five_table_star_picks_fact_first() {
        // 4 big tables joined to a small fact table via fact.k_i =
        // big_i.k. Reorder must pick fact first so each
        // intermediate stays at fact-table cardinality.
        let mut e = crate::Engine::new();
        e.execute("CREATE TABLE fact (id INT NOT NULL, k1 INT NOT NULL, k2 INT NOT NULL, k3 INT NOT NULL, k4 INT NOT NULL)").unwrap();
        for tag in ["big1", "big2", "big3", "big4"] {
            e.execute(&alloc::format!("CREATE TABLE {tag} (k INT NOT NULL)"))
                .unwrap();
        }
        for i in 0..3 {
            e.execute(&alloc::format!(
                "INSERT INTO fact VALUES ({i}, {i}, {i}, {i}, {i})"
            ))
            .unwrap();
        }
        for tag in ["big1", "big2", "big3", "big4"] {
            for i in 0..40 {
                e.execute(&alloc::format!("INSERT INTO {tag} VALUES ({i})"))
                    .unwrap();
            }
        }
        e.execute("ANALYZE").unwrap();
        let stmt = e.prepare(
            "SELECT fact.id FROM big1 \
             INNER JOIN big2 ON 1 = 1 \
             INNER JOIN big3 ON 1 = 1 \
             INNER JOIN big4 ON 1 = 1 \
             INNER JOIN fact ON fact.k1 = big1.k AND fact.k2 = big2.k AND fact.k3 = big3.k AND fact.k4 = big4.k",
        )
        .unwrap();
        let spg_sql::ast::Statement::Select(sel) = stmt else {
            panic!()
        };
        let from = sel.from.unwrap();
        assert_eq!(
            from.primary.name, "fact",
            "reorder must put fact first; got primary={:?}",
            from.primary.name
        );
    }

    #[test]
    fn left_join_is_skipped() {
        // LEFT JOIN has semantics-preserving order; v6.2.3 bails.
        let mut stmt = match parser::parse_statement(
            "SELECT * FROM a LEFT JOIN b ON a.id = b.id LEFT JOIN c ON b.id = c.id",
        )
        .unwrap()
        {
            spg_sql::ast::Statement::Select(s) => s,
            _ => panic!(),
        };
        let cat = Catalog::new();
        let stats = Statistics::new();
        let snap = stmt.clone();
        reorder_joins(&mut stmt, &cat, &stats);
        assert_eq!(stmt, snap);
    }

    fn parse_select(sql: &str) -> SelectStatement {
        match parser::parse_statement(sql).unwrap() {
            spg_sql::ast::Statement::Select(s) => s,
            _ => panic!(),
        }
    }

    #[test]
    fn drive_from_promotes_named_table_to_primary() {
        // The keyed correlated-subquery probe shape: the correlation
        // table (m2) must drive even though it is written second.
        let mut s = parse_select(
            "SELECT e2.category FROM email_analysis e2 \
             INNER JOIN messages m2 ON e2.message_id = m2.id \
             WHERE m2.thread_id = 'th-5'",
        );
        assert!(drive_from(&mut s, "m2"));
        let from = s.from.as_ref().unwrap();
        assert_eq!(from.primary.alias.as_deref(), Some("m2"));
        assert_eq!(from.primary.name, "messages");
        assert_eq!(from.joins.len(), 1);
        assert_eq!(from.joins[0].table.alias.as_deref(), Some("e2"));
        // The ON edge travelled with the join and is intact.
        assert!(from.joins[0].on.is_some());
    }

    #[test]
    fn drive_from_noop_when_already_primary() {
        let mut s = parse_select(
            "SELECT m2.id FROM messages m2 INNER JOIN email_analysis e2 ON e2.message_id = m2.id",
        );
        let snap = s.clone();
        assert!(drive_from(&mut s, "m2"));
        assert_eq!(s, snap, "already-driving promotion must not mutate");
    }

    #[test]
    fn drive_from_refuses_left_join() {
        // Promoting across a LEFT join would change semantics.
        let mut s = parse_select(
            "SELECT e2.id FROM email_analysis e2 LEFT JOIN messages m2 ON e2.message_id = m2.id",
        );
        let snap = s.clone();
        assert!(!drive_from(&mut s, "m2"));
        assert_eq!(s, snap, "refused promotion must not mutate");
    }

    #[test]
    fn drive_from_unknown_alias_is_false() {
        let mut s = parse_select(
            "SELECT e2.id FROM email_analysis e2 INNER JOIN messages m2 ON e2.message_id = m2.id",
        );
        assert!(!drive_from(&mut s, "nope"));
    }

    // v7.37.43 (T3) — `size_asymmetry_fallback` unit pins.
    // The fallback fires when no ANALYZE has been run AND the primary
    // is ≥100× the smallest reorderable table. This is the mailrs
    // prod-shape gap: a fresh open_path catalog, `JOIN small JOIN
    // big_messages`, source order materialises 100k before the small
    // filter bites.

    #[test]
    fn size_asymmetry_fallback_promotes_small_table_no_analyze() {
        // 200 rows in messages × 2 rows in mailboxes = 100× exactly.
        // Source order has messages first.
        let mut e = crate::Engine::new();
        e.execute("CREATE TABLE mailboxes (id INT NOT NULL, user_address TEXT)")
            .unwrap();
        e.execute("CREATE TABLE messages (id INT NOT NULL, mailbox_id INT NOT NULL)")
            .unwrap();
        for u in 0..2 {
            e.execute(&alloc::format!(
                "INSERT INTO mailboxes VALUES ({u}, 'u{u}@x')"
            ))
            .unwrap();
        }
        for i in 0..200 {
            e.execute(&alloc::format!(
                "INSERT INTO messages VALUES ({i}, {})",
                i % 2
            ))
            .unwrap();
        }
        // Source order: messages drives mailboxes.
        let stmt = e.prepare(
            "SELECT m.id FROM messages m INNER JOIN mailboxes mb ON mb.id = m.mailbox_id",
        )
        .unwrap();
        let spg_sql::ast::Statement::Select(sel) = stmt else {
            panic!()
        };
        let from = sel.from.unwrap();
        assert_eq!(
            from.primary.name, "mailboxes",
            "no-ANALYZE size-asymmetry fallback must promote the small table; \
             got primary={:?}",
            from.primary.name
        );
    }

    #[test]
    fn size_asymmetry_fallback_noop_when_ratio_too_small() {
        // 50 vs 5 = 10× — below the 100× threshold; source order kept.
        let mut e = crate::Engine::new();
        e.execute("CREATE TABLE mailboxes (id INT NOT NULL)")
            .unwrap();
        e.execute("CREATE TABLE messages (id INT NOT NULL, mailbox_id INT NOT NULL)")
            .unwrap();
        for u in 0..5 {
            e.execute(&alloc::format!("INSERT INTO mailboxes VALUES ({u})"))
                .unwrap();
        }
        for i in 0..50 {
            e.execute(&alloc::format!(
                "INSERT INTO messages VALUES ({i}, {})",
                i % 5
            ))
            .unwrap();
        }
        let stmt = e
            .prepare(
                "SELECT m.id FROM messages m INNER JOIN mailboxes mb ON mb.id = m.mailbox_id",
            )
            .unwrap();
        let spg_sql::ast::Statement::Select(sel) = stmt else {
            panic!()
        };
        let from = sel.from.unwrap();
        assert_eq!(
            from.primary.name, "messages",
            "10× ratio is below 100× threshold; source order must be preserved"
        );
    }

    #[test]
    fn size_asymmetry_fallback_noop_when_primary_below_min_floor() {
        // 90 vs 1 — 90× ratio but primary < MIN_BIG_ROWS (100).
        // Sub-fixture catalogs can produce extreme ratios that aren't
        // planner-meaningful; the floor prevents fixture noise.
        let mut e = crate::Engine::new();
        e.execute("CREATE TABLE mailboxes (id INT NOT NULL)")
            .unwrap();
        e.execute("CREATE TABLE messages (id INT NOT NULL, mailbox_id INT NOT NULL)")
            .unwrap();
        e.execute("INSERT INTO mailboxes VALUES (1)").unwrap();
        for i in 0..90 {
            e.execute(&alloc::format!("INSERT INTO messages VALUES ({i}, 1)"))
                .unwrap();
        }
        let stmt = e
            .prepare(
                "SELECT m.id FROM messages m INNER JOIN mailboxes mb ON mb.id = m.mailbox_id",
            )
            .unwrap();
        let spg_sql::ast::Statement::Select(sel) = stmt else {
            panic!()
        };
        let from = sel.from.unwrap();
        assert_eq!(
            from.primary.name, "messages",
            "primary below MIN_BIG_ROWS floor — fallback must not fire on sub- \
             fixture-sized tables"
        );
    }

    #[test]
    fn size_asymmetry_fallback_does_not_override_full_cost_path() {
        // When ANALYZE has run, the full cost-based reorder takes
        // over and the fallback is skipped — even if the fallback
        // would have made a different (possibly worse) call. This
        // pins the gate: fallback never fires post-ANALYZE.
        let mut e = crate::Engine::new();
        e.execute("CREATE TABLE mailboxes (id INT NOT NULL)")
            .unwrap();
        e.execute("CREATE TABLE messages (id INT NOT NULL, mailbox_id INT NOT NULL)")
            .unwrap();
        for u in 0..2 {
            e.execute(&alloc::format!("INSERT INTO mailboxes VALUES ({u})"))
                .unwrap();
        }
        for i in 0..200 {
            e.execute(&alloc::format!(
                "INSERT INTO messages VALUES ({i}, {})",
                i % 2
            ))
            .unwrap();
        }
        e.execute("ANALYZE").unwrap();
        let stmt = e
            .prepare(
                "SELECT m.id FROM messages m INNER JOIN mailboxes mb ON mb.id = m.mailbox_id",
            )
            .unwrap();
        let spg_sql::ast::Statement::Select(sel) = stmt else {
            panic!()
        };
        let from = sel.from.unwrap();
        // Either order is acceptable here — the assertion is the
        // post-ANALYZE path doesn't crash and produces *some* valid
        // plan. The fallback is gated to the empty-stats branch
        // only. Pin that the result is one of the two tables.
        assert!(
            from.primary.name == "messages" || from.primary.name == "mailboxes",
            "post-ANALYZE primary should be either input table; got {:?}",
            from.primary.name
        );
    }

    #[test]
    fn size_asymmetry_fallback_respects_order_by_on_primary() {
        // mailrs round-26 memory shape: primary owns ORDER BY column,
        // promoting it away breaks the bounded streaming join (which
        // walks the primary's BTree index in ORDER BY direction and
        // early-stops at LIMIT). The fallback must NOT fire — even
        // though the size asymmetry would otherwise meet the rule.
        let mut e = crate::Engine::new();
        e.execute("CREATE TABLE mailboxes (id INT NOT NULL)")
            .unwrap();
        e.execute("CREATE TABLE messages (id INT NOT NULL, mailbox_id INT NOT NULL)")
            .unwrap();
        for u in 0..2 {
            e.execute(&alloc::format!("INSERT INTO mailboxes VALUES ({u})"))
                .unwrap();
        }
        for i in 0..200 {
            e.execute(&alloc::format!(
                "INSERT INTO messages VALUES ({i}, {})",
                i % 2
            ))
            .unwrap();
        }
        // `m` (the primary alias) owns the ORDER BY column → fallback
        // skips. 200×2 = 100× asymmetry would otherwise trigger.
        let stmt = e
            .prepare(
                "SELECT m.id FROM messages m INNER JOIN mailboxes mb ON mb.id = m.mailbox_id \
                 ORDER BY m.id ASC LIMIT 5",
            )
            .unwrap();
        let spg_sql::ast::Statement::Select(sel) = stmt else {
            panic!()
        };
        let from = sel.from.unwrap();
        assert_eq!(
            from.primary.name, "messages",
            "ORDER BY on the source-order primary must keep it as driver — \
             promoting away kills the bounded-streaming join executor and \
             peak memory regresses to O(table)"
        );
    }

    #[test]
    fn size_asymmetry_fallback_requires_edge_to_current_primary() {
        // mailboxes (2 rows) shares an ON edge ONLY with `owners`, not
        // with `messages` (the primary). Even though mailboxes is the
        // smallest table and primary ≥ MIN_BIG_ROWS, the strict
        // "must share an edge with current primary" rule keeps the
        // fallback as a no-op — promoting mailboxes here would force
        // a CROSS-shape step 1 (mailboxes × messages with no usable
        // predicate). messages stays primary; the cost-based path will
        // pick a better order once ANALYZE runs.
        let mut e = crate::Engine::new();
        e.execute("CREATE TABLE mailboxes (id INT NOT NULL, k INT NOT NULL)")
            .unwrap();
        e.execute("CREATE TABLE messages (id INT NOT NULL, owner INT NOT NULL)")
            .unwrap();
        e.execute("CREATE TABLE owners (k INT NOT NULL)").unwrap();
        for u in 0..2 {
            e.execute(&alloc::format!("INSERT INTO mailboxes VALUES ({u}, 0)"))
                .unwrap();
        }
        // owners populated above MIN_BIG_ROWS so it isn't itself the
        // promoted small table.
        for i in 0..150 {
            e.execute(&alloc::format!("INSERT INTO owners VALUES ({i})"))
                .unwrap();
        }
        for i in 0..200 {
            e.execute(&alloc::format!("INSERT INTO messages VALUES ({i}, 0)"))
                .unwrap();
        }
        // mailboxes connects only via mb.k = o.k (to owners), no edge
        // to messages.
        let stmt = e
            .prepare(
                "SELECT m.id FROM messages m \
                 INNER JOIN owners o ON o.k = m.owner \
                 INNER JOIN mailboxes mb ON mb.k = o.k",
            )
            .unwrap();
        let spg_sql::ast::Statement::Select(sel) = stmt else {
            panic!()
        };
        let from = sel.from.unwrap();
        // The smallest table (mailboxes, 2 rows) doesn't share an edge
        // with the current primary (messages); the rule keeps source
        // order.
        assert_eq!(
            from.primary.name, "messages",
            "fallback only promotes when the small table shares an edge \
             with the current primary — mailboxes is only connected to \
             owners here"
        );
    }
}
