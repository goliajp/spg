// v7.37.42 (A3 Step 1) — SCALARSQ streaming-shape detector.
//
// Identifies the SCALARSQ shape:
//
//   SELECT col1 [, col2 ...] FROM <single_table> [WHERE …] [LIMIT N]
//
// where at least one projection item is an `Expr::ScalarSubquery`. For
// this shape the engine never has to materialise a `Vec<Row<'static>>` before
// emitting — each output row depends only on its outer row + the per-
// row scalar subquery evaluation. The plan-cached PK probe fast path
// (v7.37.37) makes the per-row inner evaluation a single index seek;
// what remains is the `tagged: Vec<…>` buffer and the wire-layer
// `Vec<Row<'static>>` round trip, both of which a streaming executor (Step 2
// of the (A3) plan) will skip directly into `wbuf` via an emit
// callback.
//
// EXCLUDED (must take the generic `exec_select_cancel` path):
//   - JOINs / set-returning FROM / view / CTE / UNION / EXCEPT
//   - GROUP BY / HAVING / DISTINCT / window functions
//   - ORDER BY (forces a sort; no streaming)
//   - LIMIT WITH TIES (requires ORDER BY semantics)
//   - any Wildcard SelectItem (column resolution still needs the
//     planner; the streaming path stays narrow until Step 2 picks up
//     wildcard handling natively)
//   - lateral subquery / UNNEST / generate_series in FROM
//   - AS OF SEGMENT cold-tier scans
//   - any projection item containing a top-level UNNEST SRF
//
// See `.claude/notes/v7.37.42-scalarsq-specialized-executor-design.md`
// for the full (A3) attack design.

use spg_sql::ast::{Expr, SelectItem, SelectStatement};

/// `true` iff `stmt` matches the SCALARSQ streaming shape — see the
/// module-level docs for the precise rules.
///
/// The detector is a pure AST inspection: ~10 boolean field checks
/// plus an items walk. Hot SELECT close paths can call it on every
/// query without measurable overhead.
pub fn is_scalarsq_streaming_shape(stmt: &SelectStatement) -> bool {
    // CTEs / DISTINCT / GROUP BY / HAVING / UNION / ORDER BY / WITH TIES —
    // any of these forces materialisation somewhere upstream of emit.
    if !stmt.ctes.is_empty() {
        return false;
    }
    if stmt.distinct {
        return false;
    }
    if stmt.group_by.is_some() || stmt.group_by_all {
        return false;
    }
    if stmt.having.is_some() {
        return false;
    }
    if !stmt.unions.is_empty() {
        return false;
    }
    if !stmt.order_by.is_empty() {
        return false;
    }
    if stmt.limit_with_ties {
        return false;
    }
    // FROM must be a regular single-table reference. UNNEST /
    // generate_series / LATERAL / AS OF SEGMENT each route through
    // their own scanners; the streaming executor stays narrow.
    let from = match &stmt.from {
        Some(f) => f,
        None => return false,
    };
    if !from.joins.is_empty() {
        return false;
    }
    let p = &from.primary;
    if p.as_of_segment.is_some()
        || p.unnest_expr.is_some()
        || p.generate_series_args.is_some()
        || p.lateral_subquery.is_some()
    {
        return false;
    }
    // Projection: no wildcard; at least one ScalarSubquery somewhere;
    // no disqualifying construct (window function, SRF, EXISTS, IN-
    // subquery — they're streaming-compatible per se but bring extra
    // executor surface the v0 streaming path won't cover).
    let mut has_scalar_sub = false;
    for item in &stmt.items {
        match item {
            SelectItem::Wildcard => return false,
            SelectItem::Expr { expr, .. } => {
                if expr_has_streaming_disqualifier(expr) {
                    return false;
                }
                if expr_has_scalar_subquery(expr) {
                    has_scalar_sub = true;
                }
            }
        }
    }
    has_scalar_sub
}

/// Walk `e` looking for an `Expr::ScalarSubquery` anywhere in the
/// tree. Sister of `subquery::expr_has_subquery` but narrowed to
/// only ScalarSubquery (Exists / InSubquery do NOT count for this
/// detector — they're already disqualifiers in `expr_has_streaming_
/// disqualifier`).
fn expr_has_scalar_subquery(e: &Expr) -> bool {
    match e {
        Expr::ScalarSubquery(_) => true,
        Expr::Exists { .. } | Expr::InSubquery { .. } => false,
        Expr::AggregateOrdered { call, order_by, .. } => {
            expr_has_scalar_subquery(call)
                || order_by.iter().any(|o| expr_has_scalar_subquery(&o.expr))
        }
        Expr::Binary { lhs, rhs, .. } => {
            expr_has_scalar_subquery(lhs) || expr_has_scalar_subquery(rhs)
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            expr_has_scalar_subquery(expr)
        }
        Expr::FunctionCall { args, .. } => args.iter().any(expr_has_scalar_subquery),
        Expr::Like { expr, pattern, .. } => {
            expr_has_scalar_subquery(expr) || expr_has_scalar_subquery(pattern)
        }
        Expr::Extract { source, .. } => expr_has_scalar_subquery(source),
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            args.iter().any(expr_has_scalar_subquery)
                || partition_by.iter().any(expr_has_scalar_subquery)
                || order_by.iter().any(|(e, _, _)| expr_has_scalar_subquery(e))
        }
        Expr::Literal(_) | Expr::Placeholder(_) | Expr::Column(_) => false,
        Expr::Array(items) => items.iter().any(expr_has_scalar_subquery),
        Expr::ArraySubscript { target, index } => {
            expr_has_scalar_subquery(target) || expr_has_scalar_subquery(index)
        }
        Expr::AnyAll { expr, array, .. } => {
            expr_has_scalar_subquery(expr) || expr_has_scalar_subquery(array)
        }
        Expr::InList { expr, list, .. } => {
            expr_has_scalar_subquery(expr) || list.iter().any(expr_has_scalar_subquery)
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            operand.as_deref().is_some_and(expr_has_scalar_subquery)
                || branches
                    .iter()
                    .any(|(w, t)| expr_has_scalar_subquery(w) || expr_has_scalar_subquery(t))
                || else_branch.as_deref().is_some_and(expr_has_scalar_subquery)
        }
    }
}

/// Walk `e` for any construct the streaming executor (Step 2) won't
/// cover: window functions (require partition materialisation),
/// Exists / InSubquery in projection (extra subquery dispatch
/// surface), top-level SRFs (one input row → many output rows).
///
/// FunctionCall args are walked but FunctionCall itself isn't flagged
/// — the projection-eval path handles SRF detection at the SELECT
/// level via `is_top_level_unnest`; that's wired into the executor,
/// not the detector, since the detector only sees the AST.
/// Concretely the detector here treats top-level `Expr::FunctionCall
/// { name = "unnest", ... }` as a disqualifier.
fn expr_has_streaming_disqualifier(e: &Expr) -> bool {
    match e {
        Expr::WindowFunction { .. } => true,
        Expr::Exists { .. } | Expr::InSubquery { .. } => true,
        // Top-level SRF. `is_top_level_unnest` (in select.rs) is the
        // authority on SRF detection at the SELECT level; for the
        // detector we recognise the canonical UNNEST call by name.
        Expr::FunctionCall { name, args, .. }
            if name.eq_ignore_ascii_case("unnest") && args.len() == 1 =>
        {
            true
        }
        Expr::FunctionCall { args, .. } => args.iter().any(expr_has_streaming_disqualifier),
        Expr::AggregateOrdered { call, order_by, .. } => {
            expr_has_streaming_disqualifier(call)
                || order_by
                    .iter()
                    .any(|o| expr_has_streaming_disqualifier(&o.expr))
        }
        Expr::Binary { lhs, rhs, .. } => {
            expr_has_streaming_disqualifier(lhs) || expr_has_streaming_disqualifier(rhs)
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            expr_has_streaming_disqualifier(expr)
        }
        Expr::Like { expr, pattern, .. } => {
            expr_has_streaming_disqualifier(expr) || expr_has_streaming_disqualifier(pattern)
        }
        Expr::Extract { source, .. } => expr_has_streaming_disqualifier(source),
        Expr::ScalarSubquery(_) => false,
        Expr::Literal(_) | Expr::Placeholder(_) | Expr::Column(_) => false,
        Expr::Array(items) => items.iter().any(expr_has_streaming_disqualifier),
        Expr::ArraySubscript { target, index } => {
            expr_has_streaming_disqualifier(target) || expr_has_streaming_disqualifier(index)
        }
        Expr::AnyAll { expr, array, .. } => {
            expr_has_streaming_disqualifier(expr) || expr_has_streaming_disqualifier(array)
        }
        Expr::InList { expr, list, .. } => {
            expr_has_streaming_disqualifier(expr)
                || list.iter().any(expr_has_streaming_disqualifier)
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            operand
                .as_deref()
                .is_some_and(expr_has_streaming_disqualifier)
                || branches.iter().any(|(w, t)| {
                    expr_has_streaming_disqualifier(w) || expr_has_streaming_disqualifier(t)
                })
                || else_branch
                    .as_deref()
                    .is_some_and(expr_has_streaming_disqualifier)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spg_sql::ast::Statement;
    use spg_sql::parser::parse_statement;

    fn select_from(sql: &str) -> SelectStatement {
        match parse_statement(sql).expect("parse") {
            Statement::Select(s) => s,
            other => panic!("expected SELECT, got {other:?}"),
        }
    }

    // ---- POSITIVE: shapes the streaming executor should pick up ----

    #[test]
    fn p1_bare_scalar_subq_in_projection() {
        let s = select_from("SELECT id, (SELECT 1) FROM t");
        assert!(is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn p2_correlated_scalar_subq_with_pk_probe() {
        let s = select_from("SELECT id, (SELECT name FROM t2 WHERE t2.pk = t.fk) FROM t");
        assert!(is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn p3_scalar_subq_with_where() {
        let s =
            select_from("SELECT id, (SELECT name FROM t2 WHERE t2.pk = t.fk) FROM t WHERE x = 1");
        assert!(is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn p4_scalar_subq_with_limit() {
        let s = select_from("SELECT id, (SELECT name FROM t2 WHERE t2.pk = t.fk) FROM t LIMIT 100");
        assert!(is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn p5_full_scalarsq_shape_where_and_limit() {
        let s = select_from(
            "SELECT id, (SELECT name FROM t2 WHERE t2.pk = t.fk) FROM t WHERE x = 1 LIMIT 100",
        );
        assert!(is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn p6_multi_projection_with_one_scalar_subq() {
        let s = select_from("SELECT id, name, (SELECT k FROM t2 WHERE t2.pk = t.fk), email FROM t");
        assert!(is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn p7_scalar_subq_in_binary_expr() {
        let s = select_from("SELECT id, (SELECT 1) + 1 FROM t");
        assert!(is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn p8_scalar_subq_in_case_branch() {
        let s = select_from("SELECT id, CASE WHEN id > 0 THEN (SELECT 1) ELSE 0 END FROM t");
        assert!(is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn p9_scalar_subq_in_function_arg() {
        let s = select_from("SELECT id, COALESCE((SELECT 1), 0) FROM t");
        assert!(is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn p10_offset_alone_still_streaming() {
        // OFFSET without ORDER BY is well-defined as "drop first N
        // rows of the scan output"; no sort required, so the
        // streaming executor can still serve it.
        let s = select_from("SELECT id, (SELECT 1) FROM t OFFSET 5");
        assert!(is_scalarsq_streaming_shape(&s));
    }

    // ---- NEGATIVE: shapes the streaming executor must skip ----

    #[test]
    fn n1_wildcard_projection() {
        let s = select_from("SELECT * FROM t");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n2_no_scalar_subquery() {
        let s = select_from("SELECT id, name FROM t");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n3_join_rejects_even_with_scalar_subq() {
        let s = select_from("SELECT t.id, (SELECT 1) FROM t INNER JOIN t2 ON t.id = t2.id");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n4_exists_in_projection_disqualifies() {
        let s = select_from("SELECT id, EXISTS(SELECT 1 FROM t2 WHERE t2.fk = t.id) FROM t");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n5_in_subquery_in_projection_disqualifies() {
        let s = select_from("SELECT id, x IN (SELECT y FROM t2) FROM t");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n6_order_by_forces_materialise() {
        let s = select_from("SELECT id, (SELECT 1) FROM t ORDER BY id");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n7_distinct_forces_dedupe() {
        let s = select_from("SELECT DISTINCT id, (SELECT 1) FROM t");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n8_group_by_rejects() {
        let s = select_from("SELECT id, (SELECT 1) FROM t GROUP BY id");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n9_having_rejects() {
        // HAVING requires GROUP BY; the parser still accepts it
        // alone (treats as a degenerate single-group filter) — either
        // way, having forces the aggregate pipeline.
        let s = select_from("SELECT id, (SELECT 1) FROM t GROUP BY id HAVING id > 0");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n10_union_chain_rejects() {
        let s = select_from("SELECT id, (SELECT 1) FROM t UNION ALL SELECT 2, 3 FROM t2");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n11_cte_present_rejects() {
        let s = select_from("WITH c AS (SELECT 1) SELECT id, (SELECT 1) FROM t");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n12_unnest_from_rejects() {
        let s = select_from("SELECT (SELECT 1) FROM unnest(ARRAY[1,2,3])");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n13_window_function_in_projection_rejects() {
        let s = select_from("SELECT id, ROW_NUMBER() OVER (PARTITION BY x), (SELECT 1) FROM t");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n14_unnest_in_projection_rejects() {
        let s = select_from("SELECT id, unnest(arr), (SELECT 1) FROM t");
        assert!(!is_scalarsq_streaming_shape(&s));
    }

    #[test]
    fn n15_no_from_rejects() {
        // `SELECT (SELECT 1)` — no FROM clause. The streaming
        // executor needs a table to scan; bare SELECT goes through
        // the generic path (it's cheap there anyway).
        let s = select_from("SELECT (SELECT 1)");
        assert!(!is_scalarsq_streaming_shape(&s));
    }
}
