//! EXPLAIN rendering and index suggestions — walk a SELECT plan into
//! human-readable lines, annotate row estimates, and suggest missing
//! indexes. Split out of `lib.rs` (v7.32 engine modularisation).

use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

use spg_sql::ast::{Expr, SelectItem, SelectStatement, UnionKind};

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
            matches!(i.kind, spg_storage::IndexKind::BTree(_))
                && i.column_position == col_pos
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
            let pos_to_name =
                |pos: usize| tbl.schema().columns.get(pos).map(|c| c.name.clone());
            let already_in_index = tbl.indices().iter().any(|i| {
                if !matches!(i.kind, spg_storage::IndexKind::BTree(_)) {
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
            let already_in_uc = tbl
                .schema()
                .uniqueness_constraints
                .iter()
                .any(|uc| {
                    let names: alloc::collections::BTreeSet<String> = uc
                        .columns
                        .iter()
                        .filter_map(|&p| pos_to_name(p))
                        .collect();
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

/// v6.2.4 — walk every line of the rendered plan tree and append
/// per-operator stats. Lines that name a known operator get `(rows=N)`
/// (the top-level operator's actual_rows equals the final result row
/// count; scans report their catalog row count as rows-considered).
/// Other lines — Filter / Join / GroupBy / OrderBy — are marked `(—)`
/// so the surface is complete-by-construction.
pub(crate) fn annotate_explain_lines(lines: &mut [String], total_rows: usize, engine: &Engine) {
    let catalog = engine.active_catalog();
    let cold_ids = catalog.cold_segment_ids_global();
    let any_cold = !cold_ids.is_empty();
    let cold_ids_repr = if any_cold {
        let mut s = alloc::string::String::from("[");
        for (i, id) in cold_ids.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&alloc::format!("{id}"));
        }
        s.push(']');
        s
    } else {
        alloc::string::String::new()
    };
    for (idx, line) in lines.iter_mut().enumerate() {
        let trimmed = line.trim_start();
        let is_top_level = idx == 0;
        if is_top_level {
            line.push_str(&alloc::format!(" (rows={total_rows})"));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("From: ") {
            let (name, scan_kind) = match rest.split_once(" [") {
                Some((n, k)) => (n.trim(), k.trim_end_matches(']')),
                None => (rest.trim(), ""),
            };
            let bare = name.split_whitespace().next().unwrap_or(name);
            let hot = catalog.get(bare).map(|t| t.rows().len());
            // v6.2.7 — `cold_segments=[id0,id1,…]` enumerates every
            // cold-tier segment the scan COULD have walked. v6.2.x
            // can tighten to per-table by walking the table's
            // BTree-index cold locators.
            let annot = match (hot, scan_kind) {
                (Some(h), "full scan") => {
                    let mut s = alloc::format!(" (hot_rows={h}");
                    if any_cold {
                        s.push_str(&alloc::format!(
                            ", cold_tier=present, cold_segments={cold_ids_repr}"
                        ));
                    }
                    s.push(')');
                    s
                }
                (Some(h), "index seek") => {
                    let mut s = alloc::format!(" (hot_rows≤{h}");
                    if any_cold {
                        s.push_str(&alloc::format!(
                            ", cold_tier=present, cold_segments={cold_ids_repr}"
                        ));
                    }
                    s.push(')');
                    s
                }
                _ => " (rows=—)".to_string(),
            };
            line.push_str(&annot);
            continue;
        }
        // Filter / GroupBy / Having / OrderBy / Limit / Join etc.
        line.push_str(" (rows=—)");
    }
}

/// v4.26: render a human-readable plan tree for `EXPLAIN <select>`.
/// Lines are pushed into `out`; `depth` controls indentation. We
/// describe the rewritten SELECT — what the executor *would* do —
/// using the engine handle to spot indexed lookups and table shapes.
#[allow(clippy::too_many_lines, clippy::format_push_string)]
pub(crate) fn explain_select(
    stmt: &SelectStatement,
    engine: &Engine,
    depth: usize,
    out: &mut Vec<String>,
) {
    let pad = "  ".repeat(depth);
    // 1) Top-level operator label.
    let top = if !stmt.ctes.is_empty() {
        if stmt.ctes.iter().any(|c| c.recursive) {
            "CTEScan (WITH RECURSIVE)"
        } else {
            "CTEScan (WITH)"
        }
    } else if !stmt.unions.is_empty() {
        "UnionScan"
    } else if select_has_window(stmt) {
        "WindowAgg"
    } else if aggregate::uses_aggregate(stmt) {
        "Aggregate"
    } else if stmt.distinct {
        "Distinct"
    } else if stmt.from.is_some() {
        "TableScan"
    } else {
        "Result"
    };
    out.push(alloc::format!("{pad}{top}"));
    let child = "  ".repeat(depth + 1);
    // 2) CTE bodies.
    for cte in &stmt.ctes {
        let head = if cte.recursive {
            alloc::format!("{child}CTE (recursive): {}", cte.name)
        } else {
            alloc::format!("{child}CTE: {}", cte.name)
        };
        out.push(head);
        // v7.37.43-T4.4 — modifying CTE bodies appear as a stub
        // node; the dispatch logic in the engine routes them
        // through dml.rs paths, not the recursive select planner.
        match &cte.body {
            spg_sql::ast::CteBody::Select(s) => explain_select(s, engine, depth + 2, out),
            spg_sql::ast::CteBody::Insert(s) => {
                out.push(alloc::format!(
                    "{}ModifyingCTE (INSERT {})",
                    "  ".repeat(depth + 2),
                    s.table
                ));
            }
            spg_sql::ast::CteBody::Update(s) => {
                out.push(alloc::format!(
                    "{}ModifyingCTE (UPDATE {})",
                    "  ".repeat(depth + 2),
                    s.table
                ));
            }
            spg_sql::ast::CteBody::Delete(s) => {
                out.push(alloc::format!(
                    "{}ModifyingCTE (DELETE {})",
                    "  ".repeat(depth + 2),
                    s.table
                ));
            }
        }
    }
    // 3) FROM details — primary table + joins, index hits.
    if let Some(from) = &stmt.from {
        let mut tag = alloc::format!("{child}From: {}", from.primary.name);
        if let Some(alias) = &from.primary.alias {
            tag.push_str(&alloc::format!(" AS {alias}"));
        }
        // v7.37.16 (16.10 [PG+]) — when the primary table is a
        // partition parent, ask the planner which children survive
        // the WHERE-clause prune pass and append that as an EXPLAIN
        // annotation. PG only shows "Partitions removed: N"; we
        // emit the kept children's actual names (which dashboards
        // and dogfood-replay flagged as the missing piece).
        if crate::partition::is_partition_parent(engine.active_catalog(), &from.primary.name) {
            tag.push_str(" [partition parent]");
            if let Some(kept) =
                engine.explain_partition_kept_children(&from.primary.name, stmt)
            {
                tag.push_str(&alloc::format!(
                    " kept=[{}]",
                    kept.join(", ")
                ));
            }
            out.push(tag);
        } else {
            // Try to detect an index-seek opportunity on WHERE against
            // the primary table — same heuristic the executor uses.
            if let Some(w) = &stmt.where_
                && let Some(table) = engine.active_catalog().get(&from.primary.name)
            {
                let alias = from.primary.alias.as_deref().unwrap_or(&from.primary.name);
                let cols = &table.schema().columns;
                if try_index_seek(
                    w,
                    cols,
                    engine.active_catalog(),
                    table,
                    alias,
                    &engine.current_snapshot(),
                )
                .is_some()
                {
                    tag.push_str(" [index seek]");
                } else {
                    tag.push_str(" [full scan]");
                }
            } else {
                tag.push_str(" [full scan]");
            }
            out.push(tag);
        }
        for j in &from.joins {
            let kind = match j.kind {
                spg_sql::ast::JoinKind::Inner => "INNER JOIN",
                spg_sql::ast::JoinKind::Left => "LEFT JOIN",
                spg_sql::ast::JoinKind::Cross => "CROSS JOIN",
            };
            let mut s = alloc::format!("{child}{kind}: {}", j.table.name);
            if let Some(alias) = &j.table.alias {
                s.push_str(&alloc::format!(" AS {alias}"));
            }
            if j.on.is_some() {
                s.push_str(" (ON …)");
            }
            out.push(s);
        }
    }
    // 4) WHERE / GROUP BY / HAVING / ORDER BY / LIMIT / OFFSET.
    if let Some(w) = &stmt.where_ {
        let mut s = alloc::format!("{child}Filter: {w}");
        if expr_has_subquery(w) {
            s.push_str(" [subquery]");
        }
        out.push(s);
    }
    if let Some(gs) = &stmt.group_by {
        let mut parts = Vec::new();
        for g in gs {
            parts.push(alloc::format!("{g}"));
        }
        out.push(alloc::format!("{child}GroupBy: {}", parts.join(", ")));
    }
    if let Some(h) = &stmt.having {
        out.push(alloc::format!("{child}Having: {h}"));
    }
    for o in &stmt.order_by {
        let dir = if o.desc { "DESC" } else { "ASC" };
        out.push(alloc::format!("{child}OrderBy: {} {dir}", o.expr));
    }
    if let Some(lim) = stmt.limit {
        out.push(alloc::format!("{child}Limit: {lim}"));
    }
    if let Some(off) = stmt.offset {
        out.push(alloc::format!("{child}Offset: {off}"));
    }
    // 5) Projection — collapse Wildcard or render N items.
    if stmt
        .items
        .iter()
        .any(|it| matches!(it, SelectItem::Wildcard))
    {
        out.push(alloc::format!("{child}Project: *"));
    } else {
        out.push(alloc::format!(
            "{child}Project: {} item(s)",
            stmt.items.len()
        ));
    }
    // 6) Recurse into UNION peers.
    for (kind, peer) in &stmt.unions {
        let label = match kind {
            UnionKind::All => "UNION ALL",
            UnionKind::Distinct => "UNION",
            UnionKind::Intersect => "INTERSECT",
            UnionKind::IntersectAll => "INTERSECT ALL",
            UnionKind::Except => "EXCEPT",
            UnionKind::ExceptAll => "EXCEPT ALL",
        };
        out.push(alloc::format!("{child}{label}"));
        explain_select(peer, engine, depth + 2, out);
    }
}

impl Engine {
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
        explain_select(&e.inner, self, 0, &mut lines);
        if e.suggest {
            // v6.8.3 — index advisor. Walks the SELECT's FROM
            // tables + WHERE column refs; for each (table, column)
            // pair that lacks an index, append a SUGGEST line with
            // a copy-pastable `CREATE INDEX` statement. This is a
            // pure-syntax heuristic — no cardinality estimation —
            // matching the v6.8.3 design intent of "tell the
            // operator where indexes are missing", not "give the
            // mathematically optimal index set".
            let suggestions = build_index_suggestions(&e.inner, self);
            for s in suggestions {
                lines.push(s);
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
            let started = self.clock.map(|f| f());
            let exec = self.exec_select_cancel(&e.inner, cancel)?;
            let elapsed_micros = match (self.clock, started) {
                (Some(f), Some(s)) => Some(f().saturating_sub(s)),
                _ => None,
            };
            let row_count = if let QueryResult::Rows { rows, .. } = &exec {
                rows.len()
            } else {
                0
            };
            annotate_explain_lines(&mut lines, row_count, self);
            let mut total = alloc::format!("Total: rows={row_count}");
            // Two independent gates suppress the wall-clock
            // `elapsed=…us` annotation:
            // - v7.37.7 C.1: `EXPLAIN (COSTS OFF)` (per-statement SQL
            //   option; PG-standard).
            // - v7.38 元机制 D: `SPG_TEST_EXPLAIN_NO_COSTS=1`
            //   (per-session env var; SPG-specific test-mode GUC).
            // Either gate active → skip the annotation. Both default
            // off in production builds.
            if !e.costs_off
                && !e.timing_off
                && !self.env_cfg().explain_no_costs
                && let Some(us) = elapsed_micros
            {
                total.push_str(&alloc::format!(" elapsed={us}us"));
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
                    let ratio_x10000 = (hot_rows.saturating_mul(10_000))
                        / total_rows;
                    alloc::format!(
                        "{}.{:02}",
                        ratio_x10000 / 100,
                        ratio_x10000 % 100
                    )
                };
                lines.push(alloc::format!(
                    "Buffers: hot_rows={hot_rows} cold_rows={cold_rows} cache_hit_ratio={ratio}"
                ));
            }
            lines.push(total);
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
                let mut body = alloc::string::String::from("[");
                for (i, l) in lines.iter().enumerate() {
                    if i > 0 {
                        body.push_str(", ");
                    }
                    body.push_str("{\"Plan Line\": ");
                    body.push_str(&json_string_lit(l));
                    body.push('}');
                }
                body.push(']');
                alloc::vec![Row::new(alloc::vec![Value::text(body)])]
            }
            spg_sql::ast::ExplainFormat::Xml => {
                let mut body = alloc::string::String::from(
                    "<explain xmlns=\"http://www.postgresql.org/2009/explain\">",
                );
                for l in &lines {
                    body.push_str("<line>");
                    body.push_str(&xml_escape(l));
                    body.push_str("</line>");
                }
                body.push_str("</explain>");
                alloc::vec![Row::new(alloc::vec![Value::text(body)])]
            }
            spg_sql::ast::ExplainFormat::Yaml => {
                let mut body = alloc::string::String::from("- Plan:\n");
                for l in &lines {
                    body.push_str("  - ");
                    body.push_str(&yaml_scalar(l));
                    body.push('\n');
                }
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
