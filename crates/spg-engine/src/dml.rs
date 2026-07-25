//! DML execution — the row-mutating statements (INSERT / UPDATE /
//! MERGE / DELETE) lifted out of `lib.rs` (v7.32 engine
//! modularisation). These `impl Engine` methods are dispatched from
//! `Engine::execute` and reach deep into the storage / catalog / WAL
//! internals, which `lib.rs` exposes pub(crate) for them.

use alloc::string::String;
use alloc::vec::Vec;

use spg_sql::ast::{Expr, InsertStatement, SelectItem};

/// v7.37.19 (19.13) — view auto-updatable redirect.
///
/// PG calls a view "simple-query" auto-updatable when its body has
/// the shape `SELECT col1, col2, ... FROM base_table` with no
/// joins / WHERE / GROUP BY / HAVING / DISTINCT / aggregates /
/// unions / ORDER BY / LIMIT / OFFSET. SPG mirrors PG's rule.
///
/// When `view_name` matches such a view in the active catalog,
/// returns `Some(base_table_name)` and the INSERT/UPDATE/DELETE
/// dispatcher rewrites stmt.table to the base. Column lists are
/// preserved as-is — the parsed view body has plain Column
/// references with the same names as the base table's columns
/// (column rename via the view's `columns:` field is rejected by
/// the auto-updatable check), so no per-column mapping is needed.
///
/// Returns None when:
///   - view_name is not a view (a real table → caller's existing path)
///   - the view's body is not a simple-query shape
///   - the view's `columns:` field is non-empty (column rename)
/// v7.38 (read01 P6.46) — resolve an auto-updatable simple view to its base
/// table plus the view's own WHERE predicate (if any). UPDATE / DELETE on the
/// view rewrite to the base table AND-ing the view's WHERE onto the caller's;
/// INSERT ignores the WHERE (no WITH CHECK OPTION support yet). Returns `None`
/// when the view is not a simple single-table projection.
/// v7.39 (round 136) — the resolved target of a write through an auto-updatable
/// view, following nested views down to the real base table.
/// v7.39 (round 154/155) — one computed (expression) column of a view,
/// as seen from the OUTERMOST redirected view.
#[derive(Clone)]
struct ComputedViewCol {
    /// The column's name at the outermost view.
    name: String,
    /// Defining expression, over base-table columns.
    def: spg_sql::ast::Expr,
    /// The view level that defines the expression and its column name
    /// THERE — PG attributes a write-target error to that level even
    /// when an outer view re-exports the column under another name.
    origin_view: String,
    origin_col: String,
}

struct ViewRedirect {
    /// The real base table (bottom of the view chain).
    base: String,
    /// Composed WHERE (every level's predicate AND-ed), in base columns. Filters
    /// which base rows an UPDATE / DELETE through the view may touch.
    where_at_base: Option<spg_sql::ast::Expr>,
    /// Outermost-view-col → base-table-col map (composed across the chain).
    /// Empty when no level renames columns (the common path).
    col_map: Vec<(String, String)>,
    /// v7.39 (round 154) — computed (expression) view columns. PG keeps
    /// such a view auto-updatable; a write TARGETING one of these errors
    /// ("View columns that are not columns of their base relation are
    /// not updatable"), reads of them rewrite to the expression. Round
    /// 155 — a plain projection over a computed view composes: the
    /// re-exported column carries the DEFINING level's view and column
    /// name (PG attributes the error there, through any outer rename).
    computed: Vec<ComputedViewCol>,
    /// v7.39 (round 154) — the view's full output column order,
    /// `(view_col, Some(base_col) = simple | None = computed)`. Filled
    /// only when `computed` is non-empty (positional writes can no
    /// longer be derived from `col_map` alone then).
    view_cols: Vec<(String, Option<String>)>,
    /// Per-level `(view_name, qual_at_base, check_option)`, outermost first, for
    /// WITH CHECK OPTION enforcement (only WHERE-bearing levels appear).
    check_chain: Vec<(String, spg_sql::ast::Expr, u8)>,
}

/// Compose two view column maps: `a` maps outer→mid, `b` maps mid→base; the
/// result maps outer→base. An empty map is the identity.
fn compose_view_maps(a: &[(String, String)], b: &[(String, String)]) -> Vec<(String, String)> {
    if a.is_empty() {
        return b.to_vec();
    }
    if b.is_empty() {
        return a.to_vec();
    }
    let bm: alloc::collections::BTreeMap<&String, &String> =
        b.iter().map(|(k, v)| (k, v)).collect();
    a.iter()
        .map(|(outer, mid)| {
            let base = bm
                .get(mid)
                .map(|s| (*s).clone())
                .unwrap_or_else(|| mid.clone());
            (outer.clone(), base)
        })
        .collect()
}

/// v7.39 (round 267) — why a view is not auto-updatable. PG reports a
/// specific DETAIL per reason, and when several apply it names exactly
/// one: measured precedence on PG 18.4 is set-op > DISTINCT > GROUP BY
/// > WITH > LIMIT/OFFSET > not-a-single-table, which is the order the
/// checks below run in.
///
/// `Unsupported` is the honest odd one out: the shape may well be
/// auto-updatable in PG, but SPG's redirect cannot express it yet. It
/// must not borrow a PG DETAIL it does not mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewNotUpdatable {
    SetOp,
    Distinct,
    GroupBy,
    With,
    LimitOffset,
    NotSingleTable,
    Unsupported,
}

impl ViewNotUpdatable {
    /// PG's DETAIL line, verbatim from PG 18.4.
    fn detail(self) -> &'static str {
        match self {
            Self::SetOp => {
                "Views containing UNION, INTERSECT, or EXCEPT are not automatically updatable."
            }
            Self::Distinct => "Views containing DISTINCT are not automatically updatable.",
            Self::GroupBy => "Views containing GROUP BY are not automatically updatable.",
            Self::With => "Views containing WITH are not automatically updatable.",
            Self::LimitOffset => {
                "Views containing LIMIT or OFFSET are not automatically updatable."
            }
            Self::NotSingleTable => {
                "Views that do not select from a single table or view are not automatically \
                 updatable."
            }
            Self::Unsupported => "This view shape is not auto-updatable in SPG.",
        }
    }
}

/// The verb-dependent halves of PG's non-updatable-view error.
struct WriteVerb {
    /// "insert into" / "update" / "delete from", as it appears after
    /// "cannot " in the primary message.
    cannot: &'static str,
    /// "inserting into" / "updating" / "deleting from", for the HINT.
    gerund: &'static str,
    /// "INSERT" / "UPDATE" / "DELETE", for the trigger and rule names.
    keyword: &'static str,
}

const INSERT_VERB: WriteVerb = WriteVerb {
    cannot: "insert into",
    gerund: "inserting into",
    keyword: "INSERT",
};
const UPDATE_VERB: WriteVerb = WriteVerb {
    cannot: "update",
    gerund: "updating",
    keyword: "UPDATE",
};
const DELETE_VERB: WriteVerb = WriteVerb {
    cannot: "delete from",
    gerund: "deleting from",
    keyword: "DELETE",
};

/// Build PG's full "cannot <verb> view" error for a non-updatable view.
/// MERGE names itself in the HINT and offers only the trigger, not the
/// rule — measured on PG 18.4.
fn view_not_updatable_error(
    view_name: &str,
    verb: &WriteVerb,
    reason: ViewNotUpdatable,
    via_merge: bool,
) -> EngineError {
    let hint = if via_merge {
        alloc::format!(
            "To enable {} the view using MERGE, provide an INSTEAD OF {} trigger.",
            verb.gerund,
            verb.keyword,
        )
    } else {
        alloc::format!(
            "To enable {} the view, provide an INSTEAD OF {} trigger or an unconditional ON {} \
             DO INSTEAD rule.",
            verb.gerund,
            verb.keyword,
            verb.keyword,
        )
    };
    EngineError::Unsupported(alloc::format!(
        "cannot {} view \"{}\" DETAIL: {} HINT: {}",
        verb.cannot,
        view_name,
        reason.detail(),
        hint,
    ))
}

/// v7.39 (round 267) — the single auto-updatability judgement. The write
/// paths and `information_schema` both go through it, so the catalog
/// cannot advertise an updatability the engine does not honour (before
/// this round it advertised NO for every view while happily writing
/// through the simple ones).
pub(crate) fn view_is_auto_updatable(catalog: &spg_storage::Catalog, view_name: &str) -> bool {
    view_redirect_checked(catalog, view_name).is_ok()
}

/// v7.39 (round 268) — the view's columns that are plain base columns,
/// i.e. legal write targets. Computed (expression) columns are excluded:
/// PG keeps such a view updatable but reports is_updatable = NO for that
/// one column, and a write targeting it errors. Reading it off the same
/// redirect the write path uses keeps the catalog's per-column answer
/// and the engine's behaviour in step.
pub(crate) fn view_simple_column_names(
    catalog: &spg_storage::Catalog,
    view_name: &str,
) -> Vec<String> {
    let Ok(vr) = view_redirect_checked(catalog, view_name) else {
        return Vec::new();
    };
    if !vr.view_cols.is_empty() {
        // Filled only when the view has computed columns; the pairing
        // says which is which.
        return vr
            .view_cols
            .iter()
            .filter(|(_, base)| base.is_some())
            .map(|(n, _)| n.clone())
            .collect();
    }
    if !vr.col_map.is_empty() {
        return vr.col_map.iter().map(|(v, _)| v.clone()).collect();
    }
    // The identity shape keeps col_map empty as a fast path: every
    // column passes through from the base table under its own name.
    catalog
        .get(&vr.base)
        .map(|t| t.schema().columns.iter().map(|c| c.name.clone()).collect())
        .unwrap_or_default()
}

fn view_redirect_to_simple_base(
    catalog: &spg_storage::Catalog,
    view_name: &str,
) -> Option<ViewRedirect> {
    view_redirect_checked(catalog, view_name).ok()
}

fn view_redirect_checked(
    catalog: &spg_storage::Catalog,
    view_name: &str,
) -> Result<ViewRedirect, ViewNotUpdatable> {
    let view = catalog
        .views_all()
        .get(view_name)
        .ok_or(ViewNotUpdatable::Unsupported)?;
    let this_check = view.check_option;
    // v7.39 (round 133) — a column-rename list maps the view's column names back
    // to the primary's columns.
    let rename_cols = view.columns.clone();
    let stmt = spg_sql::parser::parse_statement(&view.body)
        .map_err(|_| ViewNotUpdatable::Unsupported)?;
    let select = match stmt {
        spg_sql::ast::Statement::Select(s) => s,
        _ => return Err(ViewNotUpdatable::Unsupported),
    };
    // Simple-query shape. A WHERE is allowed, and so is ORDER BY — PG
    // auto-updates `SELECT a FROM t ORDER BY a` (measured on 18.4), and
    // ordering is meaningless to a write anyway. SPG rejected it until
    // round 267, which made an INSERT PG accepts fail here.
    //
    // The order of these checks is PG's reporting precedence, measured
    // by building views that break two rules at once.
    if !select.unions.is_empty() {
        return Err(ViewNotUpdatable::SetOp);
    }
    if select.distinct {
        return Err(ViewNotUpdatable::Distinct);
    }
    if select.group_by.is_some() || select.group_by_all || select.having.is_some() {
        return Err(ViewNotUpdatable::GroupBy);
    }
    if !select.ctes.is_empty() {
        return Err(ViewNotUpdatable::With);
    }
    if select.limit.is_some() || select.offset.is_some() {
        return Err(ViewNotUpdatable::LimitOffset);
    }
    let from = select.from.as_ref().ok_or(ViewNotUpdatable::NotSingleTable)?;
    if !from.joins.is_empty() {
        return Err(ViewNotUpdatable::NotSingleTable);
    }
    if from.primary.unnest_expr.is_some() || from.primary.as_of_segment.is_some() {
        return Err(ViewNotUpdatable::NotSingleTable);
    }
    let primary_name = from.primary.name.clone();
    let is_leaf = catalog.get(&primary_name).is_some();
    // Build the view's output columns in order: each is either a simple
    // primary column (writable) or — leaf level only, v7.39 round 154 —
    // a computed expression (readable, not a write target; PG keeps the
    // view auto-updatable). The view-side name is the rename-list entry
    // when present, else the item alias, else the column's own name.
    // `*` projects the primary's columns in declaration order.
    let mut out_cols: Vec<(String, Option<String>, Option<spg_sql::ast::Expr>)> = Vec::new(); // (view-side name, base col, computed expr)
    for item in &select.items {
        match item {
            spg_sql::ast::SelectItem::Wildcard | spg_sql::ast::SelectItem::QualifiedWildcard(_) => {
                // Leaf: resolve the primary's output columns (a base
                // table's schema). Over a nested view the wildcard is the
                // identity pass-through (no per-column info needed) —
                // renamed-wildcard-over-nested-view bails at the rename
                // length check below, as before.
                if is_leaf {
                    let base = catalog.get(&primary_name).ok_or(ViewNotUpdatable::Unsupported)?;
                    for c in &base.schema().columns {
                        out_cols.push((c.name.clone(), Some(c.name.clone()), None));
                    }
                }
            }
            spg_sql::ast::SelectItem::Expr { expr, alias } => match expr {
                spg_sql::ast::Expr::Column(c) => {
                    let name = alias.clone().unwrap_or_else(|| c.name.clone());
                    out_cols.push((name, Some(c.name.clone()), None));
                }
                other => {
                    // v7.39 (round 154) — a computed column keeps the view
                    // auto-updatable ONLY for plain expressions at the leaf
                    // level: an aggregate (`SELECT max(v) …`) makes the
                    // whole view non-updatable in PG, and window / SRF /
                    // subquery shapes stay conservatively rejected too.
                    if !is_leaf
                        || crate::aggregate::contains_aggregate(other)
                        || expr_mentions_subquery_or_window(other)
                    {
                        return Err(ViewNotUpdatable::Unsupported);
                    }
                    let name = alias.clone().unwrap_or_else(|| String::from("?column?"));
                    out_cols.push((name, None, Some(other.clone())));
                }
            },
        }
    }
    // A rename list overrides every view-side name positionally.
    if !rename_cols.is_empty() {
        if rename_cols.len() != out_cols.len() {
            return Err(ViewNotUpdatable::Unsupported);
        }
        for (slot, name) in out_cols.iter_mut().zip(rename_cols) {
            slot.0 = name;
        }
    }
    let computed: Vec<ComputedViewCol> = out_cols
        .iter()
        .filter_map(|(n, _, e)| {
            e.clone().map(|def| ComputedViewCol {
                name: n.clone(),
                def,
                origin_view: String::from(view_name),
                origin_col: n.clone(),
            })
        })
        .collect();
    // The simple view→base pairs. `col_map` stays EMPTY on the pure
    // identity shape (no rename, no alias, no computed column) so the
    // established fast path — no rewriting at all — is byte-identical.
    let all_simple: Vec<(String, String)> = out_cols
        .iter()
        .filter_map(|(n, b, _)| b.clone().map(|b| (n.clone(), b)))
        .collect();
    let identity = computed.is_empty() && all_simple.iter().all(|(v, b)| v.eq_ignore_ascii_case(b));
    let this_map: Vec<(String, String)> = if identity { Vec::new() } else { all_simple };
    let view_cols: Vec<(String, Option<String>)> = if computed.is_empty() {
        Vec::new()
    } else {
        out_cols
            .iter()
            .map(|(n, b, _)| (n.clone(), b.clone()))
            .collect()
    };
    let this_where = select.where_;

    // Leaf: the primary is a real base table.
    if catalog.get(&primary_name).is_some() {
        let mut chain = Vec::new();
        if let Some(w) = &this_where {
            chain.push((
                alloc::string::String::from(view_name),
                w.clone(),
                this_check,
            ));
        }
        return Ok(ViewRedirect {
            base: primary_name,
            where_at_base: this_where,
            col_map: this_map,
            computed,
            view_cols,
            check_chain: chain,
        });
    }
    // Nested: the primary is itself an auto-updatable view — recurse and compose.
    if catalog.has_view(&primary_name) {
        let inner = view_redirect_checked(catalog, &primary_name)?;
        let inner_map: alloc::collections::BTreeMap<String, String> =
            inner.col_map.iter().cloned().collect();
        let inner_cmap: alloc::collections::BTreeMap<String, spg_sql::ast::Expr> = inner
            .computed
            .iter()
            .map(|c| (c.name.clone(), c.def.clone()))
            .collect();
        // This view's WHERE references the inner view's columns; translate them
        // to base columns through the inner map (round 155 — a computed
        // reference substitutes its defining expression).
        let this_where_at_base = this_where.map(|mut w| {
            if !inner_map.is_empty() || !inner_cmap.is_empty() {
                rewrite_view_refs_to_base(&mut w, &inner_map, &inner_cmap, None);
            }
            w
        });
        let mut chain = Vec::new();
        if let Some(w) = &this_where_at_base {
            chain.push((
                alloc::string::String::from(view_name),
                w.clone(),
                this_check,
            ));
        }
        chain.extend(inner.check_chain);
        if inner.computed.is_empty() {
            return Ok(ViewRedirect {
                base: inner.base,
                where_at_base: and_optional_predicates(this_where_at_base, inner.where_at_base),
                col_map: compose_view_maps(&this_map, &inner.col_map),
                computed: Vec::new(),
                view_cols: Vec::new(),
                check_chain: chain,
            });
        }
        // v7.39 (round 155) — compose over a computed inner view: each of
        // this level's (all-simple — expressions bailed above for a
        // non-leaf) columns resolves through the inner's columns. A
        // re-exported computed column keeps the inner's origin for error
        // attribution. A wildcard pass-through (empty out_cols) adopts
        // the inner's columns unchanged.
        let inner_by_name: alloc::collections::BTreeMap<&str, &Option<String>> = inner
            .view_cols
            .iter()
            .map(|(n, b)| (n.as_str(), b))
            .collect();
        let mut composed: Vec<(String, Option<String>, Option<&ComputedViewCol>)> = Vec::new();
        if out_cols.is_empty() {
            for (n, b) in &inner.view_cols {
                let c = inner.computed.iter().find(|c| c.name == *n);
                composed.push((n.clone(), b.clone(), c));
            }
        } else {
            for (name, inner_ref, _) in &out_cols {
                let inner_ref = inner_ref.as_ref().ok_or(ViewNotUpdatable::Unsupported)?; // all-simple here
                if let Some(c) = inner.computed.iter().find(|c| c.name == *inner_ref) {
                    composed.push((name.clone(), None, Some(c)));
                } else {
                    let base = inner_by_name.get(inner_ref.as_str()).ok_or(ViewNotUpdatable::Unsupported)?;
                    // A simple inner column — its base name is right there.
                    composed.push((name.clone(), (*base).clone(), None));
                }
            }
        }
        let computed: Vec<ComputedViewCol> = composed
            .iter()
            .filter_map(|(n, _, c)| {
                c.map(|c| ComputedViewCol {
                    name: n.clone(),
                    def: c.def.clone(),
                    origin_view: c.origin_view.clone(),
                    origin_col: c.origin_col.clone(),
                })
            })
            .collect();
        let col_map: Vec<(String, String)> = composed
            .iter()
            .filter_map(|(n, b, _)| b.clone().map(|b| (n.clone(), b)))
            .collect();
        let view_cols: Vec<(String, Option<String>)> = composed
            .iter()
            .map(|(n, b, _)| (n.clone(), b.clone()))
            .collect();
        return Ok(ViewRedirect {
            base: inner.base,
            where_at_base: and_optional_predicates(this_where_at_base, inner.where_at_base),
            col_map,
            computed,
            view_cols,
            check_chain: chain,
        });
    }
    Err(ViewNotUpdatable::Unsupported)
}

/// v7.39 (round 154) — shapes that make a view non-auto-updatable even
/// as a "computed column": subqueries and window functions (aggregates
/// are checked separately via `contains_aggregate`).
fn expr_mentions_subquery_or_window(e: &spg_sql::ast::Expr) -> bool {
    if crate::subquery::expr_has_subquery(e) {
        return true;
    }
    let mut found = false;
    let mut probe = e.clone();
    crate::expr_analysis::rewrite_nodes_mut(&mut probe, &mut |n| {
        if matches!(n, spg_sql::ast::Expr::WindowFunction { .. }) {
            found = true;
        }
        false
    });
    found
}

/// v7.39 (round 154) — PG's error for a write that targets a computed
/// (expression) view column; the view stays auto-updatable otherwise.
/// `verb` is "insert into" / "update" / "merge into" (PG varies it by
/// statement kind).
fn view_computed_col_write_err(verb: &str, col: &str, view: &str) -> EngineError {
    EngineError::Unsupported(alloc::format!(
        "cannot {verb} column \"{col}\" of view \"{view}\" \
         DETAIL: View columns that are not columns of their base relation are not updatable."
    ))
}

/// v7.39 (round 154) — the round-133 ref rewriter extended for computed
/// view columns: a simple column renames to its base column, a computed
/// column substitutes its defining expression. `target_alias` selects
/// the context: `Some(alias)` (MERGE) rewrites only bare / alias-
/// qualified refs — source-alias refs must survive — and stamps the
/// alias onto every rewritten ref (the combined-row context resolves
/// target columns through it); `None` (single-table UPDATE / DELETE /
/// INSERT context) rewrites every ref and leaves them bare.
fn rewrite_view_refs_to_base(
    expr: &mut spg_sql::ast::Expr,
    map: &alloc::collections::BTreeMap<String, String>,
    computed: &alloc::collections::BTreeMap<String, spg_sql::ast::Expr>,
    target_alias: Option<&str>,
) {
    use spg_sql::ast::Expr;
    crate::expr_analysis::rewrite_nodes_mut(expr, &mut |n| {
        if let Expr::Column(c) = n {
            let eligible = match target_alias {
                None => true,
                Some(a) => {
                    c.qualifier.is_none()
                        || c.qualifier
                            .as_deref()
                            .is_some_and(|q| q.eq_ignore_ascii_case(a))
                }
            };
            if eligible {
                if let Some(base) = map.get(&c.name) {
                    c.name = base.clone();
                    c.qualifier = target_alias.map(Into::into);
                } else if let Some(def) = computed.get(&c.name) {
                    let mut sub = def.clone();
                    if let Some(q) = target_alias {
                        crate::expr_analysis::rewrite_nodes_mut(&mut sub, &mut |m| {
                            if let Expr::Column(mc) = m {
                                if mc.qualifier.is_none() {
                                    mc.qualifier = Some(q.into());
                                }
                                return true;
                            }
                            false
                        });
                    }
                    *n = sub;
                }
            }
            return true;
        }
        false
    });
}

/// v7.39 (round 133) — rewrite an auto-updatable view's column references in a
/// write predicate / assignment to the base table's columns (the view rename
/// map is view-col → base-col). Bare and view-qualified refs both remap; the
/// qualifier is dropped since the base is a single table.
fn rewrite_view_col_refs(
    expr: &mut spg_sql::ast::Expr,
    map: &alloc::collections::BTreeMap<String, String>,
) {
    use spg_sql::ast::Expr;
    match expr {
        Expr::Column(c) => {
            if let Some(base) = map.get(&c.name) {
                c.name = base.clone();
                c.qualifier = None;
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_view_col_refs(lhs, map);
            rewrite_view_col_refs(rhs, map);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            rewrite_view_col_refs(expr, map)
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                rewrite_view_col_refs(a, map);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                rewrite_view_col_refs(o, map);
            }
            for (c, v) in branches {
                rewrite_view_col_refs(c, map);
                rewrite_view_col_refs(v, map);
            }
            if let Some(x) = else_branch {
                rewrite_view_col_refs(x, map);
            }
        }
        _ => {}
    }
}

/// v7.39 (round 134) — rewrite a RETURNING projection on a column-renamed view:
/// each item's column refs map from view columns to base columns, but the
/// OUTPUT name stays the view column (PG labels `RETURNING a` "a", not the base
/// "id"). A bare `*` expands to `base AS view` for every mapped column.
fn rewrite_view_returning_items(
    items: &[spg_sql::ast::SelectItem],
    col_map: &[(String, String)],
    computed: &[ComputedViewCol],
    view_cols: &[(String, Option<String>)],
) -> Vec<spg_sql::ast::SelectItem> {
    use spg_sql::ast::{Expr, SelectItem};
    let map: alloc::collections::BTreeMap<String, String> = col_map.iter().cloned().collect();
    let cmap: alloc::collections::BTreeMap<String, Expr> = computed
        .iter()
        .map(|c| (c.name.clone(), c.def.clone()))
        .collect();
    let mut out: Vec<SelectItem> = Vec::new();
    for it in items {
        match it {
            // `RETURNING *` / `v.*` → the view's columns (base value, view
            // name). v7.39 (round 154) — a computed column projects its
            // defining expression (evaluated on the post-write row, as PG);
            // `view_cols` carries the declaration order when one exists.
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                if view_cols.is_empty() {
                    for (view_col, base_col) in col_map {
                        out.push(SelectItem::Expr {
                            expr: Expr::Column(spg_sql::ast::ColumnName {
                                qualifier: None,
                                name: base_col.clone(),
                            }),
                            alias: Some(view_col.clone()),
                        });
                    }
                } else {
                    for (view_col, base_col) in view_cols {
                        let expr = match base_col {
                            Some(b) => Expr::Column(spg_sql::ast::ColumnName {
                                qualifier: None,
                                name: b.clone(),
                            }),
                            None => cmap
                                .get(view_col)
                                .cloned()
                                .expect("view_cols and computed come from the same projection"),
                        };
                        out.push(SelectItem::Expr {
                            expr,
                            alias: Some(view_col.clone()),
                        });
                    }
                }
            }
            SelectItem::Expr { expr, alias } => {
                // Preserve the view column name as the output name before the
                // ref rewrite renames it to the base column.
                let out_alias = if alias.is_some() {
                    alias.clone()
                } else if let Expr::Column(c) = expr
                    && (map.contains_key(&c.name) || cmap.contains_key(&c.name))
                {
                    Some(c.name.clone())
                } else {
                    alias.clone()
                };
                let mut e = expr.clone();
                rewrite_view_refs_to_base(&mut e, &map, &cmap, None);
                out.push(SelectItem::Expr {
                    expr: e,
                    alias: out_alias,
                });
            }
        }
    }
    out
}

/// v7.39 (round 132/136) — a pending `WITH CHECK OPTION` to enforce on the base
/// rows a write through an auto-updatable view produces. `chain` is the per-view
/// `(name, qual_at_base, own_check_option)` list (outermost first) down the
/// nested-view stack; `written_opt` is the outermost (written) view's option.
#[derive(Clone)]
struct ViewCheck {
    written_opt: u8,
    chain: Vec<(String, spg_sql::ast::Expr, u8)>,
}

/// v7.38 (read01 P6.46) — AND two optional predicates (the view's WHERE and the
/// caller's WHERE) into one.
fn and_optional_predicates(
    a: Option<spg_sql::ast::Expr>,
    b: Option<spg_sql::ast::Expr>,
) -> Option<spg_sql::ast::Expr> {
    match (a, b) {
        (Some(x), Some(y)) => Some(spg_sql::ast::Expr::Binary {
            lhs: alloc::boxed::Box::new(x),
            op: spg_sql::ast::BinOp::And,
            rhs: alloc::boxed::Box::new(y),
        }),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}
use spg_storage::{ColumnSchema, DataType, Row, StorageError, Value};

use crate::eval::{EvalContext, EvalError};
use crate::{
    CancelToken, Engine, EngineError, QueryResult, any_column_changed, apply_fk_child_step,
    apply_on_conflict_assignments, build_projection, canonicalize_set_value, check_unsigned_range,
    coerce_value, enforce_check_constraints, enforce_enum_label, enforce_fk_inserts,
    enforce_not_null, enforce_unique_index_inserts, enforce_unique_updates,
    enforce_uniqueness_inserts, eval, eval_runtime_default_free, expr_has_subquery,
    literal_expr_to_value, literal_expr_to_value_in, lookup_row_position_by_keys,
    on_conflict_keys_exist, plan_fk_parent_deletions, plan_fk_parent_updates,
    resolve_column_default_free, triggers, try_index_seek_positions,
    try_pk_predicate, value_to_literal_expr_permissive,
};

/// Pre-borrow snapshots gathered by `prepare_insert_snapshots` for the
/// INSERT row loop, taken before the mutable catalog borrow opens.
struct InsertSnapshots {
    clock: Option<crate::ClockFn>,
    before_insert_triggers: Vec<(spg_storage::FunctionDef, String, String)>,
    after_insert_triggers: Vec<(spg_storage::FunctionDef, String, String)>,
    trigger_session_cfg: Option<String>,
    enum_label_lookup: alloc::collections::BTreeMap<usize, Vec<String>>,
    set_variant_lookup: alloc::collections::BTreeMap<usize, Vec<String>>,
    seq_floors: alloc::collections::BTreeMap<usize, i64>,
}

impl Engine {
    /// v4.4 `UPDATE <table> SET col = expr [, ...] [WHERE cond]`.
    /// Filter pass uses the same WHERE eval as `exec_select`. Per
    /// matched row, evaluate each RHS expression against the *old*
    /// row, then call `Table::update_row` which rebuilds indices.
    /// Indexed columns are correctly reflected because rebuild
    /// happens after the cell rewrite.
    /// v7.39 (round 132/136) — enforce a view's `WITH CHECK OPTION` over the base
    /// rows a write produced. Each row must satisfy the qual of every checked
    /// view in the chain (only a definite TRUE passes — NULL / FALSE fail,
    /// mirroring row visibility). PG's cascade rule: the written view's qual is
    /// always checked; an underlying view's qual is checked iff the written view
    /// is CASCADED or that underlying view itself has a check option (and a
    /// CASCADED underlying re-arms the cascade for its own underlyings). The
    /// error names the specific failing view (SQLSTATE 44000 + failing-row
    /// DETAIL). Constant `2` = CASCADED, `1` = LOCAL, `0` = none.
    fn enforce_view_check(
        &self,
        check: &ViewCheck,
        rows: &[Vec<Value<'static>>],
        schema_cols: &[ColumnSchema],
        base_table: &str,
    ) -> Result<(), EngineError> {
        let ctx = self.ev_ctx(schema_cols, Some(base_table));
        let cancel = CancelToken::none();
        for vals in rows {
            let row = Row::new(vals.clone());
            let mut cascade = check.written_opt == 2;
            for (view_name, qual, opt) in &check.chain {
                if cascade || *opt != 0 {
                    let v = self.eval_expr_with_correlated(qual, &row, &ctx, cancel, None)?;
                    if !matches!(v, Value::Bool(true)) {
                        let failing = crate::constraints::format_failing_row(vals);
                        return Err(EngineError::Unsupported(alloc::format!(
                            "new row violates check option for view \"{view_name}\" \
                             DETAIL: Failing row contains ({failing})."
                        )));
                    }
                }
                if *opt == 2 {
                    cascade = true;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn exec_update_cancel(
        &mut self,
        stmt: &spg_sql::ast::UpdateStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let table = stmt.table.clone();
        self.exec_update_cancel_inner(stmt, None, cancel)
            .map_err(|e| enrich_not_null(e, &table))
    }

    fn exec_update_cancel_inner(
        &mut self,
        stmt: &spg_sql::ast::UpdateStatement,
        view_check: Option<ViewCheck>,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        // v7.37.43-T4.4 — writable CTE outer body (UPDATE).
        if !stmt.ctes.is_empty() {
            return self.exec_update_with_ctes(stmt.clone(), cancel);
        }
        // v7.39 (RLS) Phase 2 — UPDATE USING visibility: AND the policy's USING
        // predicate into WHERE so a policy-subject session only updates rows it
        // can see (a hidden row is silently skipped). None for superuser / non-
        // RLS tables (no clone).
        let rls_upd;
        let stmt = match self.rls_write_using_predicate(&stmt.table, spg_storage::PolicyCmd::Update)
        {
            Some(pred) => {
                let mut s = stmt.clone();
                s.where_ = and_optional_predicates(s.where_.take(), Some(pred));
                rls_upd = s;
                &rls_upd
            }
            None => stmt,
        };
        // v7.39 (round 139/141) — DO INSTEAD NOTHING rules narrow the UPDATE.
        // Unconditional → AND a constant FALSE. Conditional (WHERE over NEW/OLD)
        // → AND `COALESCE(NOT(cond), TRUE)` with `NEW.col` rewritten to the SET
        // expression, so the predicate — evaluated against the pre-image row —
        // reproduces the rule's post-image test. RETURNING on a fully suppressed
        // statement is rejected as PG does.
        // v7.39 (round 333, V59) — the statement as the caller wrote it: a
        // conditional instead-command rule's action runs against THESE rows.
        let unnarrowed_upd = stmt;
        let rule_blocked_upd;
        let upd_block = if self.rule_blocks_statement(&stmt.table, "UPDATE") {
            if stmt.returning.is_some() {
                return Err(crate::rules::rule_returning_error("UPDATE", &stmt.table));
            }
            Some(spg_sql::ast::Expr::Literal(spg_sql::ast::Literal::Bool(
                false,
            )))
        } else {
            self.conditional_block_predicate(&stmt.table, "UPDATE", &stmt.assignments)?
        };
        let stmt = if let Some(pred) = upd_block {
            let mut s = stmt.clone();
            s.where_ = and_optional_predicates(s.where_.take(), Some(pred));
            rule_blocked_upd = s;
            &rule_blocked_upd
        } else {
            stmt
        };
        // v7.39 (round 140/142) — rule-command forms. DO INSTEAD <command>
        // replaces the UPDATE (original op never runs, tag `UPDATE 0`); DO ALSO
        // runs the real update first, then the commands. Both bind OLD + derived
        // NEW per matching row. The guard bounds a rule → update cycle.
        if !self.rule_rewrite_active {
            let instead_upd = self.instead_command_rules(&stmt.table, "UPDATE");
            // v7.39 (round 333, V59) — an UNCONDITIONAL instead-command rule
            // replaces the statement; a CONDITIONAL one only claims the rows
            // its WHERE holds for, and the rest still run the original (the
            // block predicate above has already removed them from it).
            let (uncond, cond): (Vec<_>, Vec<_>) = instead_upd
                .into_iter()
                .partition(|r| r.when_condition.is_empty());
            if !uncond.is_empty() {
                let mut all = uncond;
                all.extend(cond);
                return self.exec_update_instead_command(unnarrowed_upd, all, cancel);
            }
            if !cond.is_empty() {
                // The actions must see the rows the CALLER asked for, not the
                // narrowed set — those are exactly the rows they replace.
                self.run_update_instead_actions(unnarrowed_upd, &cond, cancel)?;
            }
            let also_upd = self.also_rules(&stmt.table, "UPDATE");
            if !also_upd.is_empty() {
                return self.exec_update_with_also(stmt, also_upd, view_check, cancel);
            }
        }
        // v7.39 (round 137, Phase 2) — INSTEAD OF UPDATE trigger on the target
        // view fires per matching view row instead of the auto-updatable
        // redirect. Takes precedence.
        let iof_upd = self.snapshot_row_triggers(&stmt.table, "UPDATE", "INSTEAD OF");
        if !iof_upd.is_empty() {
            return self.exec_update_view_instead_of(stmt, iof_upd, cancel);
        }
        // v7.37.19 (19.13) — auto-updatable view redirect. v7.38 (P6.46) — a
        // view WHERE is AND-ed onto the caller's so only rows visible through
        // the view are updated.
        // v7.39 (round 267) — the view exists but is not auto-updatable.
        // Without this the write fell through to the base-table lookup and
        // reported `relation "<view>" does not exist`, which is not merely
        // the wrong wording — it denies the existence of an object the
        // catalog plainly has.
        if let Err(reason) = view_redirect_checked(self.active_catalog(), &stmt.table) {
            if self.active_catalog().has_view(&stmt.table) {
                return Err(view_not_updatable_error(&stmt.table, &UPDATE_VERB, reason, false));
            }
        }
        if let Some(vr) = view_redirect_to_simple_base(self.active_catalog(), &stmt.table) {
            let ViewRedirect {
                base,
                where_at_base,
                col_map,
                computed,
                view_cols,
                check_chain,
            } = vr;
            // v7.39 (round 154) — a computed view column is never a write
            // target (PG: the view stays updatable, the column doesn't).
            for (target, _) in &stmt.assignments {
                if let Some(cc) = computed.iter().find(|c| c.name == *target) {
                    return Err(view_computed_col_write_err(
                        "update",
                        &cc.origin_col,
                        &cc.origin_view,
                    ));
                }
            }
            // v7.39 (round 132/136) — WITH CHECK OPTION: the updated row must
            // still satisfy the view chain's quals. The written (outermost)
            // view's own option drives the cascade.
            let written_opt = self
                .active_catalog()
                .views_all()
                .get(&stmt.table)
                .map_or(0, |v| v.check_option);
            // v7.39 (round 152) — a lower view carrying its OWN check
            // option enforces even when the written view has none (PG,
            // r152 probe P6; enforce_view_check already walks the chain
            // that way — the gate here just failed to arm it).
            let check = if written_opt != 0 || check_chain.iter().any(|(_, _, o)| *o != 0) {
                Some(ViewCheck {
                    written_opt,
                    chain: check_chain,
                })
            } else {
                None
            };
            let mut rewritten = stmt.clone();
            // v7.39 (round 133) — a column-renamed view: rewrite SET targets and
            // WHERE / assignment expressions from view columns to base columns
            // before AND-ing the (base-column) view WHERE. Round 154 — a
            // computed column READ substitutes its defining expression.
            if !col_map.is_empty() || !computed.is_empty() {
                let map: alloc::collections::BTreeMap<String, String> =
                    col_map.iter().cloned().collect();
                let cmap: alloc::collections::BTreeMap<String, Expr> = computed
                    .iter()
                    .map(|c| (c.name.clone(), c.def.clone()))
                    .collect();
                for (target, e) in &mut rewritten.assignments {
                    if let Some(b) = map.get(target) {
                        *target = b.clone();
                    }
                    rewrite_view_refs_to_base(e, &map, &cmap, None);
                }
                if let Some(w) = &mut rewritten.where_ {
                    rewrite_view_refs_to_base(w, &map, &cmap, None);
                }
                // v7.39 (round 134) — rewrite RETURNING view cols → base cols.
                if let Some(ret) = &rewritten.returning {
                    rewritten.returning = Some(rewrite_view_returning_items(
                        ret, &col_map, &computed, &view_cols,
                    ));
                }
            }
            rewritten.table = base;
            rewritten.where_ = and_optional_predicates(where_at_base, rewritten.where_);
            return self.exec_update_cancel_inner(&rewritten, check, cancel);
        }
        // v7.37 D.47 (partial) — UPDATE on a partition parent fans out to every
        // child (the parent holds no rows of its own, so a parent-targeted UPDATE
        // would otherwise affect nothing). An UPDATE whose SET list touches the
        // partition-key column may move a row across a partition boundary, which
        // needs row movement (delete-from-source + reinsert-through-routing) — a
        // focused follow-up. Until then, reject a key-touching parent UPDATE
        // honestly rather than fan it out in place and leave a row in the wrong
        // partition (silent-wrong). A non-key UPDATE is always safe to fan out.
        if crate::partition::is_partition_parent(self.active_catalog(), &stmt.table) {
            let key_cols: Vec<String> = {
                let parent = self.active_catalog().get(&stmt.table).ok_or_else(|| {
                    EngineError::Storage(StorageError::TableNotFound {
                        name: stmt.table.clone(),
                    })
                })?;
                match &parent.schema().partition_role {
                    Some(spg_storage::PartitionRole::Parent {
                        key_column_positions,
                        ..
                    }) => key_column_positions
                        .iter()
                        .filter_map(|&p| parent.schema().columns.get(p).map(|c| c.name.clone()))
                        .collect(),
                    _ => Vec::new(),
                }
            };
            let touches_key = stmt
                .assignments
                .iter()
                .any(|(col, _)| key_cols.iter().any(|k| k.eq_ignore_ascii_case(col)));
            if touches_key {
                return Err(EngineError::Unsupported(alloc::format!(
                    "UPDATE on partition parent {:?} that changes a partition-key \
                     column may move a row across partitions (row movement is a \
                     focused follow-up) — UPDATE the child partition directly, or \
                     use DELETE + INSERT",
                    stmt.table
                )));
            }
            let children = crate::partition::children_of_parent(self.active_catalog(), &stmt.table);
            let mut total_affected = 0usize;
            let mut ret_columns: Option<Vec<ColumnSchema>> = None;
            let mut ret_rows: Vec<Row<'static>> = Vec::new();
            for child in children {
                let mut child_stmt = stmt.clone();
                child_stmt.table = child;
                match self.exec_update_cancel(&child_stmt, cancel)? {
                    QueryResult::CommandOk { affected, .. } => total_affected += affected,
                    QueryResult::Rows { columns, rows } => {
                        total_affected += rows.len();
                        ret_columns = Some(columns);
                        ret_rows.extend(rows);
                    }
                }
            }
            return Ok(match ret_columns {
                Some(columns) => QueryResult::Rows {
                    columns,
                    rows: ret_rows,
                },
                None => QueryResult::CommandOk {
                    affected: total_affected,
                    modified_catalog: true,
                },
            });
        }
        // v7.12.5 — snapshot BEFORE/AFTER UPDATE row triggers + the
        // session FTS config before the table mut-borrow opens (the
        // INSERT path uses the same pattern). Empty vecs are the
        // common "no triggers on this table" fast path.
        // v7.13.0 — UPDATE triggers carry an optional `UPDATE OF
        // cols` filter. The filter is paired with each function so
        // the per-row fire loop can skip when no listed column
        // actually differs between OLD and NEW.
        let before_update_triggers = self.snapshot_update_row_triggers(&stmt.table, "BEFORE");
        let after_update_triggers = self.snapshot_update_row_triggers(&stmt.table, "AFTER");
        let trigger_session_cfg: Option<String> = self
            .session_params
            .get("default_text_search_config")
            .cloned();
        // v5.2.3: if the WHERE is a PK equality and matches a cold-
        // tier row, promote it back to the hot tier *before* the
        // hot-row walk. The promote pushes the row to the end of
        // `table.rows`, where the upcoming SET-evaluation loop will
        // pick it up and apply the assignments. Lookups for the key
        // never observe a gap because `promote_cold_row` inserts the
        // hot row before retiring the cold locator.
        if let Some(w) = &stmt.where_ {
            let schema_cols = self
                .active_catalog()
                .get(&stmt.table)
                .ok_or_else(|| {
                    EngineError::Storage(StorageError::TableNotFound {
                        name: stmt.table.clone(),
                    })
                })?
                .schema()
                .columns
                .clone();
            if let Some((col_pos, key)) = try_pk_predicate(w, &schema_cols, stmt.table.as_str())
                && let Some(idx_name) = self
                    .active_catalog()
                    .get(&stmt.table)
                    .and_then(|t| t.index_on(col_pos).map(|i| i.name.clone()))
            {
                // Promote may be a no-op (key is hot-only or absent);
                // we don't care about the return value here — the
                // subsequent hot walk will either match or not.
                let _ = self
                    .active_catalog_mut()
                    .promote_cold_row(&stmt.table, &idx_name, &key);
            } else {
                // v7.36 (cold-tier coverage) — UPDATE with a non-PK
                // WHERE on a cold-bearing table previously missed
                // cold-tier matching rows because the candidate walk
                // only iterates `(0..table.row_count())` (hot). Walk
                // each cold row, eval WHERE against it, and promote
                // every match to the hot tier so the regular SET
                // loop below picks it up. Costs one segment-page
                // read + decode per cold row scanned — bounded by
                // the table's cold-row count.
                let pre_promote_keys: Vec<spg_storage::IndexKey> = {
                    let mut keys = Vec::new();
                    // v7.39 (round 456) — `has_cold_rows_fast()` first.
                    //
                    // A profile of a range-predicated DELETE put 57.8% of
                    // self-time in `count_cold_locators`, which walks every
                    // (key, locator) pair of every BTree index — O(table) —
                    // to answer a question that is only ever asked as
                    // "> 0". That is the whole of this shape's cost scaling
                    // with table size: a one-row range DELETE costs 0.024 ms
                    // at 10k rows and 1.220 ms at 200k, while the same
                    // delete by equality (which never reaches here) stays at
                    // 0.006 ms.
                    //
                    // `has_cold_rows_fast` is the O(1) predicate v7.36 added
                    // for exactly this, and its own doc comment says the
                    // O(N) walk is "unsuitable per join stage". It is
                    // conservative — true when the cached count is stale —
                    // so it short-circuits rather than replaces: the exact
                    // walk still runs whenever cold rows might exist, and
                    // the answer is unchanged.
                    if let Some(t) = self.active_catalog().get(&stmt.table)
                        && t.has_cold_rows_fast()
                        && t.count_cold_locators() > 0
                    {
                        let ctx = eval::EvalContext::new(&schema_cols, Some(stmt.alias.as_deref().unwrap_or(stmt.table.as_str())));
                        for (key, row) in
                            crate::constraints::iter_cold_rows_with_pk_key(self.active_catalog(), t)
                        {
                            let cond = eval::eval_expr(w, &row, &ctx).map_err(EngineError::Eval)?;
                            if crate::eval::predicate_is_true(&cond, "WHERE", ctx.mysql_dialect)? {
                                keys.push(key);
                            }
                        }
                    }
                    keys
                };
                if !pre_promote_keys.is_empty()
                    && let Some(idx_name) = self
                        .active_catalog()
                        .get(&stmt.table)
                        .and_then(crate::constraints::pk_btree_index_name)
                {
                    for key in pre_promote_keys {
                        let _ = self.active_catalog_mut().promote_cold_row(
                            &stmt.table,
                            &idx_name,
                            &key,
                        );
                    }
                }
            }
        }

        // v7.12.1 — cache session FTS config before the table
        // mut-borrow (same reason as exec_delete).
        let ts_cfg: Option<String> = self
            .session_param("default_text_search_config")
            .map(String::from);
        // v7.17.0 Phase 2.1 — snapshot the clock pointer before
        // we hold the catalog mutably so ON UPDATE runtime
        // overrides see the engine wall clock.
        let clock_for_on_update = self.clock;
        // v7.31 (mailrs round-28) — the candidate-gathering phase
        // below is READ-ONLY (it builds `planned`; the mutation
        // happens after `let _ = table`). Borrow the catalog
        // SHARED here so a correlated scalar subquery in SET / WHERE
        // can re-enter the engine read path (`eval_expr_with_correlated`
        // also takes `&self`) with the target row as its outer
        // context. The apply phase re-acquires `active_catalog_mut()`.
        let table = self.active_catalog().get(&stmt.table).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound {
                name: stmt.table.clone(),
            })
        })?;
        let schema_cols: Vec<ColumnSchema> = table.schema().columns.clone();
        // Resolve each SET target to a column position once, validate
        // up front so a typo'd column doesn't leave a partial mutation
        // behind.
        let mut targets: Vec<(usize, &Expr)> = Vec::with_capacity(stmt.assignments.len());
        for (col, expr) in &stmt.assignments {
            let pos = schema_cols
                .iter()
                .position(|c| c.name == *col)
                .ok_or_else(|| {
                    EngineError::Eval(EvalError::ColumnNotFound { name: col.clone() })
                })?;
            targets.push((pos, expr));
        }
        // v7.17.0 Phase 2.1 — for every column with an
        // `ON UPDATE CURRENT_TIMESTAMP` binding that the caller
        // did NOT explicitly set, schedule an automatic override.
        // Reuses `eval_runtime_default_free` so the same
        // canonical runtime-expression whitelist (now /
        // current_timestamp / current_date / …) governs both
        // DEFAULT and ON UPDATE.
        let mut on_update_overrides: Vec<(usize, String)> = Vec::new();
        for (i, col) in schema_cols.iter().enumerate() {
            if targets.iter().any(|(p, _)| *p == i) {
                continue;
            }
            if let Some(src) = &col.on_update_runtime {
                on_update_overrides.push((i, src.clone()));
            }
        }
        // v7.39 (read01 round 54) — carry the catalog. Without it an
        // `m = 'happy'::mood` (or ::regclass / composite / domain) in an
        // UPDATE / DELETE WHERE failed outright with "unsupported cast target
        // `::mood`" — the same SELECT worked. Catalog::clone is a structural
        // Arc bump, so this is cheap and sidesteps the &mut self borrow.
        let cat_for_ctx = self.active_catalog().clone();
        let ctx = EvalContext::new(&schema_cols, Some(stmt.alias.as_deref().unwrap_or(stmt.table.as_str())))
            .with_default_text_search_config(ts_cfg.as_deref())
            .with_catalog(&cat_for_ctx);
        // Walk candidate rows, evaluate WHERE then SET
        // expressions. We gather (position, new_values) tuples
        // first and apply them afterwards so the WHERE/RHS
        // evaluation reads the original row state — matches PG
        // semantics (UPDATE doesn't see its own writes).
        //
        // v7.20 P4 — index seek: a single-column equality WHERE
        // on an indexed column narrows the walk from
        // O(table.rows()) to O(matches). The full WHERE still
        // re-evaluates per candidate (the seek may be an
        // over-approximation under AND-composites), so semantics
        // are unchanged. profile: the bench's `UPDATE … WHERE
        // id = $1` on a 5 000-row table was a ~1.3 ms full scan
        // per statement; with the seek it's ~2 µs.
        let seek_positions: Option<Vec<usize>> = stmt
            .where_
            .as_ref()
            .and_then(|w| try_index_seek_positions(w, &schema_cols, table, stmt.table.as_str()));
        let mut planned: Vec<(usize, Vec<Value<'static>>)> = Vec::new();
        let candidate_positions: Vec<usize> = match &seek_positions {
            Some(list) => list.clone(),
            None => {
                // v7.39 (round 455) — a mutation that finds no usable index
                // walks the table, and PG counts those tuples: its
                // `pg_stat_user_tables.seq_tup_read` covers DML, not just
                // SELECT. SPG only reported from `scan_visible`, which the
                // mutation paths do not use, so an UPDATE or DELETE that
                // scanned every row reported reading none — measured in
                // round 454 with a deliberately unindexed predicate over
                // 50k rows, which came back 0.
                //
                // Silently-wrong monitoring for any write workload, and the
                // instrument the round-452 investigation needs to tell a
                // range mutation's seek from a scan.
                table.note_seq_scan();
                (0..table.row_count()).collect()
            }
        };
        // v7.37.16 (gate-on inventory) — MVCC visibility gate for the
        // UPDATE target scan. Without it, a tombstoned old version
        // matched the WHERE again and entered `planned` alongside its
        // live successor — the whole gate-on UPDATE e2e fail cluster
        // (double-apply → spurious UNIQUE violations, double triggers,
        // doubled affected counts / redo). A no-op under the default
        // gate-off: every hot row is visible.
        let scan_snapshot = self.current_snapshot();
        for (loop_n, &i) in candidate_positions.iter().enumerate() {
            // v4.5: cooperative cancel checkpoint every 256 rows so
            // a runaway UPDATE without WHERE doesn't drag past the
            // server's query-timeout watchdog.
            if loop_n.is_multiple_of(256) {
                cancel.check()?;
            }
            if !table.is_row_visible(i, &scan_snapshot) {
                continue;
            }
            let Some(row) = table.rows().get(i) else {
                continue;
            };
            if let Some(w) = &stmt.where_ {
                // v7.31 (round-28) — correlated subqueries in the
                // UPDATE WHERE bind to the candidate row, like the
                // SELECT path; plain predicates keep the cheap
                // interpreter.
                let cond = if expr_has_subquery(w) {
                    self.eval_expr_with_correlated(w, row, &ctx, cancel, None)?
                } else {
                    eval::eval_expr(w, row, &ctx)?
                };
                if !crate::eval::predicate_is_true(&cond, "WHERE", ctx.mysql_dialect)? {
                    continue;
                }
            }
            let mut new_vals = row.values.clone();
            for (pos, expr) in &targets {
                // `SET col = DEFAULT` — the parser's marker call;
                // resolve the column's declared default here.
                if matches!(expr, Expr::FunctionCall { name, args }
                    if name == "__column_default" && args.is_empty())
                {
                    let v = resolve_column_default_free(&schema_cols[*pos], self.clock)?;
                    new_vals[*pos] = v;
                    continue;
                }
                // v7.31 (round-28) — correlated scalar subquery in
                // SET (`SET c = (SELECT … WHERE x = target.col)`)
                // binds the target row as its outer context. The
                // non-correlated case was materialised up front by
                // resolve_expr_subqueries at the UPDATE entry.
                let v = if expr_has_subquery(expr) {
                    self.eval_expr_with_correlated(expr, row, &ctx, cancel, None)?
                } else {
                    eval::eval_expr(expr, row, &ctx)?
                };
                let v = crate::conversions::normalize_composite_for_column(
                    v,
                    &schema_cols[*pos],
                    Some(&cat_for_ctx),
                )?;
                let coerced = coerce_value(v, schema_cols[*pos].ty, &schema_cols[*pos].name, *pos)?;
                let coerced = crate::conversions::truncate_to_column_fsp(coerced, &schema_cols[*pos]);
                check_unsigned_range(&coerced, &schema_cols[*pos], *pos)?;
                new_vals[*pos] = coerced;
            }
            // v7.17.0 Phase 2.1 — apply ON UPDATE overrides for
            // any column the SET clause didn't touch.
            for (pos, src) in &on_update_overrides {
                let v = eval_runtime_default_free(src, schema_cols[*pos].ty, clock_for_on_update)?;
                new_vals[*pos] = v;
            }
            planned.push((i, new_vals));
        }
        // v7.39 (round 413) — MySQL `UPDATE … ORDER BY … LIMIT n`. Sort the
        // matched rows by the ORDER BY keys (evaluated on the pre-image row),
        // truncate to the LIMIT, then restore position order so the FK /
        // trigger / apply passes below see the ascending-row invariant.
        // UPDATE's ctx above does not thread the engine (a long-standing site,
        // not touched here), so read the dialect straight off the session.
        if self.backslash_escapes
            && let Some(ol) = stmt.order_limit.as_deref()
        {
            if !ol.order_by.is_empty() {
                let mut tagged: Vec<(Vec<Value<'static>>, usize, Vec<Value<'static>>)> = planned
                    .into_iter()
                    .map(|(pos, new_vals)| {
                        let row = table
                            .rows()
                            .get(pos)
                            .expect("planned position was visible under scan_snapshot");
                        let keys: Vec<Value<'static>> = ol
                            .order_by
                            .iter()
                            .map(|o| eval::eval_expr(&o.expr, row, &ctx))
                            .collect::<Result<_, _>>()?;
                        Ok::<_, EngineError>((keys, pos, new_vals))
                    })
                    .collect::<Result<_, _>>()?;
                tagged.sort_by(|a, b| {
                    for (k, o) in ol.order_by.iter().enumerate() {
                        let cmp = crate::order_by_value_cmp_in(
                            o.desc,
                            o.nulls_first,
                            &a.0[k],
                            &b.0[k],
                            true,
                        );
                        if cmp != core::cmp::Ordering::Equal {
                            return cmp;
                        }
                    }
                    core::cmp::Ordering::Equal
                });
                planned = tagged.into_iter().map(|(_, i, v)| (i, v)).collect();
            }
            if let Some(n) = ol.limit {
                planned.truncate(n as usize);
            }
        }
        // planned must stay position-sorted: downstream passes
        // (FK pairing, trigger walks, the apply loop) iterate it
        // assuming ascending row order, which the full-scan path
        // guaranteed implicitly.
        planned.sort_by_key(|(i, _)| *i);
        // v7.37.7(sentori Epic 3 P1)— recompute stored generated
        // columns against each post-UPDATE candidate row, BEFORE
        // FK / CHECK / trigger passes so guards reason about the
        // computed value the same way they would for a literal cell.
        {
            let mut staged: Vec<Vec<Value<'static>>> = planned
                .iter()
                .map(|(_pos, new_vals)| new_vals.clone())
                .collect();
            apply_generated_stored_columns(&schema_cols, &mut staged)?;
            for ((_pos, new_vals), recomputed) in planned.iter_mut().zip(staged) {
                *new_vals = recomputed;
            }
        }
        // v7.6.6 — capture pre-update row values for the FK
        // enforcement passes below. `planned` carries new values
        // only; pair them with the old row.
        let plan_with_old: Vec<(usize, Vec<Value<'static>>, Vec<Value<'static>>)> = planned
            .iter()
            .map(|(pos, new_vals)| (*pos, table.rows()[*pos].values.clone(), new_vals.clone()))
            .collect();
        let self_fks = table.schema().foreign_keys.clone();
        // v7.12.5 — `affected` is computed post-BEFORE-trigger
        // below (triggers may RETURN NULL to skip individual
        // rows). The pre-trigger len shape is no longer accurate.
        // Release mutable borrow on `table` for the FK passes.
        let _ = table;
        // v7.6.6 — Stage 2a: outbound FK check. For every row whose
        // local FK columns changed, the new value must exist in the
        // parent.
        if !self_fks.is_empty() {
            let new_rows: Vec<Vec<Value<'static>>> = planned
                .iter()
                .map(|(_pos, new_vals)| new_vals.clone())
                .collect();
            // v7.39 (round 288) — a DEFERRED constraint is not checked
            // here; COMMIT (or SET CONSTRAINTS IMMEDIATE) runs it.
            let now = self.immediate_fks(&self_fks);
            if !now.is_empty() {
                enforce_fk_inserts(self.active_catalog(), &stmt.table, &now, &new_rows)?;
            }
        }
        // v7.13.0 — CHECK constraint enforcement on UPDATE
        // (mailrs round-5 G3). Predicates evaluated against the
        // candidate post-UPDATE row; false rejects the UPDATE.
        {
            let new_rows: Vec<Vec<Value<'static>>> = planned
                .iter()
                .map(|(_pos, new_vals)| new_vals.clone())
                .collect();
            // v7.39 (read01 round 117) — NOT NULL on the post-update rows,
            // pre-write with PG's `DETAIL: Failing row contains (...)`. Before
            // CHECK, matching PG's ordering.
            enforce_not_null(self.active_catalog(), &stmt.table, &new_rows)?;
            enforce_check_constraints(self.active_catalog(), &stmt.table, &new_rows)?;
            // v7.39 (RLS) Phase 2 — UPDATE WITH CHECK on the post-update rows.
            let cols = self
                .active_catalog()
                .get(&stmt.table)
                .map(|t| t.schema().columns.clone())
                .unwrap_or_default();
            self.rls_check_new_rows(
                &stmt.table,
                spg_storage::PolicyCmd::Update,
                &cols,
                &new_rows,
            )?;
            // v7.39 (round 132) — WITH CHECK OPTION on the post-update rows.
            if let Some(check) = &view_check {
                self.enforce_view_check(check, &new_rows, &cols, &stmt.table)?;
            }
        }
        // v7.38 (read01 U1) — UNIQUE / PRIMARY KEY + unique-index
        // enforcement on UPDATE. The pre-image of each updated row is
        // excluded from the existing-key set (see enforce_unique_updates),
        // and only keys whose columns actually changed are scanned.
        {
            let mut changed_cols: hashbrown::HashSet<usize> = hashbrown::HashSet::new();
            for (_pos, old_vals, new_vals) in &plan_with_old {
                for (i, (o, n)) in old_vals.iter().zip(new_vals.iter()).enumerate() {
                    if o != n {
                        changed_cols.insert(i);
                    }
                }
            }
            enforce_unique_updates(
                self.active_catalog(),
                &stmt.table,
                &planned,
                &changed_cols,
                self.backslash_escapes,
            )?;
            // v7.39 (round 210) — EXCLUDE constraints on the post-update rows;
            // each updated row's pre-image is excluded from the scan.
            let exclusions = self
                .active_catalog()
                .get(&stmt.table)
                .map(|t| t.schema().exclusion_constraints.clone())
                .unwrap_or_default();
            crate::constraints::enforce_exclusion_updates(
                self.active_catalog(),
                &stmt.table,
                &exclusions,
                &planned,
            )?;
        }
        // v7.6.6 — Stage 2b: inbound FK check. For every row that
        // changed value in a column that *some other table* uses as
        // a FK parent column, react per `on_update` action.
        let child_plan =
            plan_fk_parent_updates(self.active_catalog(), &stmt.table, &plan_with_old)?;
        // Stage 3a — apply each child-side action.
        for step in &child_plan {
            apply_fk_child_step(self.active_catalog_mut(), step)?;
        }
        // v7.37.15 (Phase C.3, step 4b) — read the in-place kill switch
        // and the writer version BEFORE the table mut borrow. When on,
        // UPDATE tombstones the old row version (xmax = v) and appends
        // the new version (xmin = v) instead of an in-place replace; the
        // now-uniformly-gated readers hide the old version and show the
        // new one, vacuum reclaims the old later. Default OFF → legacy
        // in-place update_row, byte-for-byte unchanged.
        let inplace = self.mvcc_inplace();
        let v = self.writer_version_for_current_stmt();
        // v7.39 (round 426) — read the dialect BEFORE the table mut-borrow
        // opens; the affected-count rule below needs it.
        let mysql_changed_count = self.backslash_escapes;
        // Stage 3b — apply the original UPDATE.
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        // v7.12.5 — fire BEFORE/AFTER UPDATE row-level triggers
        // around the apply loop. BEFORE sees NEW=candidate +
        // OLD=current; may rewrite NEW or RETURN NULL to skip.
        // AFTER sees NEW=post-write + OLD=pre-write (both read-
        // only).
        //
        // Filter `planned` through the BEFORE pass first so the
        // RETURNING snapshot reflects what actually got written
        // (triggers may rewrite cells, including a cancellation).
        let mut applied_after_before: Vec<(usize, Row, Row)> = Vec::with_capacity(planned.len());
        // v7.12.7 — embedded SQL queue.
        let mut deferred_embedded: Vec<triggers::DeferredEmbeddedStmt> = Vec::new();
        for (pos, new_vals) in &planned {
            let old_row = table.rows()[*pos].clone();
            let mut new_row = Row::new(new_vals.clone());
            let mut skip = false;
            for (fd, filter, when, tgname) in &before_update_triggers {
                // v7.13.0 — `UPDATE OF cols` filter (mailrs round-5
                // G7). Skip this trigger when the filter is set and
                // no listed column actually differs between OLD and
                // NEW for this row.
                if !filter.is_empty()
                    && !any_column_changed(filter, &schema_cols, &old_row, &new_row)
                {
                    continue;
                }
                // v7.39 (round 138) — WHEN filter over OLD / NEW.
                if !triggers::trigger_when_holds(
                    when,
                    Some(&new_row),
                    Some(&old_row),
                    &schema_cols,
                )? {
                    continue;
                }
                let (outcome, deferred) = triggers::fire_row_trigger(
                    fd,
                    Some(new_row.clone()),
                    Some(&old_row),
                    &stmt.table,
                    &schema_cols,
                    &[],
                    trigger_session_cfg.as_deref(),
                    false,
                    &triggers::TgMeta {
                        op: "UPDATE",
                        name: tgname,
                        level: "ROW",
                    },
                )
                .map_err(|e| EngineError::Storage(StorageError::Corrupt(alloc::format!("{e}"))))?;
                deferred_embedded.extend(deferred);
                match outcome {
                    triggers::TriggerOutcome::Row(r) => new_row = r,
                    triggers::TriggerOutcome::Skip => {
                        skip = true;
                        break;
                    }
                }
            }
            if !skip {
                // v7.39 (read01 round 82) — recompute stored generated columns
                // over the BEFORE trigger's OUTPUT. The pre-loop pass above ran
                // before the triggers, so a `w GENERATED AS (v*2)` kept the value
                // for the pre-trigger `v` when a BEFORE UPDATE trigger changed
                // `NEW.v`. PG's order is BEFORE trigger → generated → write.
                if !before_update_triggers.is_empty() {
                    let mut one = [core::mem::take(&mut new_row.values)];
                    apply_generated_stored_columns(&schema_cols, &mut one)?;
                    new_row.values = core::mem::take(&mut one[0]);
                }
                applied_after_before.push((*pos, new_row, old_row));
            }
        }
        // v7.9.4 — snapshot post-update values for RETURNING (post-
        // BEFORE-trigger because triggers can rewrite cells).
        let updated_for_returning: Vec<Vec<Value<'static>>> = if stmt.returning.is_some() {
            applied_after_before
                .iter()
                .map(|(_pos, new_row, _old)| new_row.values.clone())
                .collect()
        } else {
            Vec::new()
        };
        // v7.39 (read01 round 126) — pre-update snapshot for RETURNING OLD.*.
        let old_for_returning: Vec<Vec<Value<'static>>> = if stmt.returning.is_some() {
            applied_after_before
                .iter()
                .map(|(_pos, _new, old_row)| old_row.values.clone())
                .collect()
        } else {
            Vec::new()
        };
        // v7.39 (round 426) — MySQL reports rows CHANGED, not rows matched:
        // `UPDATE t SET v = v` over three rows answers `Rows matched: 3
        // Changed: 0`, and ROW_COUNT() reads the 0. PG's UPDATE tag counts
        // every matched row (each gets a new row version), so this is
        // dialect-gated. The comparison is against the pre-image the apply
        // loop already carries — no extra read.
        let affected = if mysql_changed_count {
            applied_after_before
                .iter()
                .filter(|(_pos, new_row, old_row)| new_row.values != old_row.values)
                .count()
        } else {
            applied_after_before.len()
        };
        // Apply, then fire AFTER triggers per row. AFTER runs read-
        // only against the freshly-written row; v7.12.4-shape
        // assignment errors with a clear message.
        // v7.37.17 (Phase E4 fix) — collect (old, new) RowId pairs for
        // the in-place UPDATEs so the RC rebase keeps each UPDATE's
        // tombstone+insert atomic under write-write conflicts.
        let mut update_rid_pairs: Vec<(
            spg_storage::row_header::RowId,
            spg_storage::row_header::RowId,
        )> = Vec::new();
        for (pos, new_row, old_row) in applied_after_before {
            if inplace {
                // MVCC: tombstone the old version (xmax = v) + append the
                // new version (xmin = v). Appending does not shift earlier
                // physical positions, so later `pos` values stay valid.
                let old_rid = table.rowids().get(pos).copied();
                let _ = table.mark_row_deleted(pos, v);
                table
                    .insert_with_xmin(Row::new(new_row.values.clone()), v)
                    .map_err(EngineError::Storage)?;
                if let (Some(o), Some(n)) = (
                    old_rid,
                    table
                        .rowids()
                        .get(table.rowids().len().wrapping_sub(1))
                        .copied(),
                ) {
                    update_rid_pairs.push((o, n));
                }
            } else {
                table.update_row(pos, new_row.values.clone())?;
            }
            for (fd, filter, when, tgname) in &after_update_triggers {
                if !filter.is_empty()
                    && !any_column_changed(filter, &schema_cols, &old_row, &new_row)
                {
                    continue;
                }
                // v7.39 (round 138) — WHEN filter over OLD / NEW.
                if !triggers::trigger_when_holds(
                    when,
                    Some(&new_row),
                    Some(&old_row),
                    &schema_cols,
                )? {
                    continue;
                }
                let (_outcome, deferred) = triggers::fire_row_trigger(
                    fd,
                    Some(new_row.clone()),
                    Some(&old_row),
                    &stmt.table,
                    &schema_cols,
                    &[],
                    trigger_session_cfg.as_deref(),
                    true,
                    &triggers::TgMeta {
                        op: "UPDATE",
                        name: tgname,
                        level: "ROW",
                    },
                )
                .map_err(|e| EngineError::Storage(StorageError::Corrupt(alloc::format!("{e}"))))?;
                deferred_embedded.extend(deferred);
            }
        }
        let _ = table;
        // v7.37.17 (Phase E4 fix) — persist the UPDATE pairs on the
        // open tx (no-op in autocommit).
        self.record_update_pairs(&stmt.table, update_rid_pairs);
        // v7.12.7 — drain trigger-emitted embedded SQL for this UPDATE.
        self.execute_deferred_trigger_stmts(deferred_embedded, cancel)?;
        // v6.2.1 — auto-analyze modified-row tracking for UPDATE.
        if !self.in_transaction() && affected > 0 {
            self.statistics
                .record_modifications(&stmt.table, affected as u64);
        }
        // v7.37.16 — autovacuum trigger: an in-place UPDATE tombstones
        // the pre-update versions; reclaim when the dead-row meter
        // crosses the threshold (no-op gate-off / in-tx / below it).
        if affected > 0 {
            self.maybe_autovacuum(&stmt.table);
        }
        // v7.9.4 — RETURNING projection. v7.39 (round 126) — UPDATE exposes both
        // OLD (pre-update) and NEW (post-update = the default row).
        if let Some(items) = &stmt.returning {
            let new_for_returning = updated_for_returning.clone();
            return self.build_returning_rows_old_new(
                &stmt.table,
                stmt.alias.as_deref(),
                items,
                updated_for_returning,
                Some(old_for_returning),
                Some(new_for_returning),
            );
        }
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: !self.in_transaction(),
        })
    }

    /// v4.4 `DELETE FROM <table> [WHERE cond]`. Collects matching
    /// positions then delegates to `Table::delete_rows` (single index
    /// rebuild for the batch).
    /// v7.17.0 Phase 3.P0-42 — SQL:2003 / PG 15+ `MERGE` execution.
    ///
    /// Semantics:
    ///   * Resolve `target` and `source` tables (catalog reads).
    ///   * Build a combined `(target_alias.col, source_alias.col)`
    ///     schema so the ON / WHEN AND / SET / VALUES expressions
    ///     resolve through the standard qualifier-aware resolver.
    ///   * Pass 1: walk every source row × every target hot row,
    ///     evaluate ON, then pick the first WHEN clause that fits
    ///     (`Matched` if any target row matched, `NotMatched`
    ///     otherwise; AND-condition must hold). Collect the action
    ///     plan as `(deletes, updates, inserts)` so the apply pass
    ///     reads the original target row state.
    ///   * Pass 2: apply the plan against the target's mutable row
    ///     vector. Deletes execute by index in descending order so
    ///     earlier indices remain stable; updates next; inserts
    ///     last (matching PG's "INSERT branch sees the post-delete
    ///     state" behaviour for the common upsert shape).
    ///
    /// v7.17 simplifications (documented limitations):
    ///   * No triggers / WAL plumbing (MVP); MERGE rows don't fire
    ///     INSERT / UPDATE / DELETE row triggers in v7.17.
    ///   * No cardinality check (PG-canonical: "MERGE command
    ///     cannot affect row a second time" — SPG silently applies
    ///     the last action for a target row covered twice).
    ///   * Source must be a catalog-resolvable table (no subquery
    ///     source); RETURNING / BY SOURCE / BY TARGET unsupported.
    pub(crate) fn exec_merge_cancel(
        &mut self,
        stmt: &spg_sql::ast::MergeStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        // v7.39 (round 149) — writable CTE outer body (MERGE): PG 15
        // allows `WITH <ctes> MERGE INTO …`; each CTE materialises
        // first and its alias resolves as a source relation.
        if !stmt.ctes.is_empty() {
            return self.exec_merge_with_ctes(stmt.clone(), cancel);
        }
        // v7.39 (round 148, PG17) — MERGE INTO an auto-updatable view rewrites
        // to the base table: column names map view → base (qualified by the
        // view's alias), the view's WHERE narrows which target rows the merge
        // can see, and a positional INSERT follows the VIEW's column order.
        // v7.39 (round 267) — as above; MERGE reports the verb of its
        // FIRST WHEN clause (measured on PG 18.4) and names MERGE in the
        // HINT instead of offering a rewrite rule.
        if let Err(reason) = view_redirect_checked(self.active_catalog(), &stmt.target) {
            if self.active_catalog().has_view(&stmt.target) {
                let verb = stmt
                    .clauses
                    .iter()
                    .find_map(|c| match c.action {
                        spg_sql::ast::MergeAction::Insert { .. } => Some(&INSERT_VERB),
                        spg_sql::ast::MergeAction::Update { .. } => Some(&UPDATE_VERB),
                        spg_sql::ast::MergeAction::Delete => Some(&DELETE_VERB),
                        _ => None,
                    })
                    .unwrap_or(&INSERT_VERB);
                return Err(view_not_updatable_error(&stmt.target, verb, reason, true));
            }
        }
        if let Some(vr) = view_redirect_to_simple_base(self.active_catalog(), &stmt.target) {
            let mut s = stmt.clone();
            let alias = s.target_alias.clone().unwrap_or_else(|| s.target.clone());
            // Keep resolving `<view>.col` references: the view name stays the
            // target's alias while the storage target becomes the base table.
            s.target_alias = Some(alias.clone());
            s.target = vr.base.clone();
            if !vr.col_map.is_empty() || !vr.computed.is_empty() {
                let map: alloc::collections::BTreeMap<String, String> =
                    vr.col_map.iter().cloned().collect();
                let cmap: alloc::collections::BTreeMap<String, Expr> = vr
                    .computed
                    .iter()
                    .map(|c| (c.name.clone(), c.def.clone()))
                    .collect();
                let rw = |e: &mut Expr| {
                    rewrite_view_refs_to_base(e, &map, &cmap, Some(&alias));
                };
                rw(&mut s.on);
                for cl in &mut s.clauses {
                    if let Some(cond) = &mut cl.condition {
                        rw(cond);
                    }
                    match &mut cl.action {
                        spg_sql::ast::MergeAction::Update { assignments } => {
                            for (col, e) in assignments {
                                // v7.39 (round 154) — a computed view column
                                // is never a write target.
                                if let Some(cc) = vr.computed.iter().find(|cc| cc.name == *col) {
                                    return Err(view_computed_col_write_err(
                                        "merge into",
                                        &cc.origin_col,
                                        &cc.origin_view,
                                    ));
                                }
                                if let Some(b) = map.get(col) {
                                    *col = b.clone();
                                }
                                rw(e);
                            }
                        }
                        spg_sql::ast::MergeAction::Insert { columns, values } => {
                            if columns.is_empty() {
                                // Positional tuples follow the VIEW's column
                                // order; make that explicit as base columns.
                                // Round 154 — with computed columns present
                                // the order comes from `view_cols`; a tuple
                                // value landing on a computed slot errors.
                                if vr.computed.is_empty() {
                                    *columns = vr.col_map.iter().map(|(_, b)| b.clone()).collect();
                                } else {
                                    let n = values.len();
                                    let mut cols: Vec<String> = Vec::with_capacity(n);
                                    for (view_col, base_col) in vr.view_cols.iter().take(n) {
                                        match base_col {
                                            Some(b) => cols.push(b.clone()),
                                            None => {
                                                let cc = vr
                                                    .computed
                                                    .iter()
                                                    .find(|cc| cc.name == *view_col)
                                                    .expect("computed slot has an entry");
                                                return Err(view_computed_col_write_err(
                                                    "merge into",
                                                    &cc.origin_col,
                                                    &cc.origin_view,
                                                ));
                                            }
                                        }
                                    }
                                    *columns = cols;
                                }
                            } else {
                                for c in columns.iter_mut() {
                                    if let Some(cc) = vr.computed.iter().find(|cc| cc.name == *c) {
                                        return Err(view_computed_col_write_err(
                                            "merge into",
                                            &cc.origin_col,
                                            &cc.origin_view,
                                        ));
                                    }
                                    if let Some(b) = map.get(c) {
                                        *c = b.clone();
                                    }
                                }
                            }
                            for e in values {
                                rw(e);
                            }
                        }
                        _ => {}
                    }
                }
                // v7.39 (round 152) — RETURNING through a column-renamed
                // view: rewrite view-column references to base columns
                // while keeping the VIEW name as the output name (PG).
                // Qualifier-aware: bare refs and refs qualified by the
                // view alias / OLD / NEW remap; source-alias refs don't.
                if let Some(items) = &mut s.returning {
                    // Round 154 — an OLD./NEW.-qualified reference to a
                    // COMPUTED view column would need the expression
                    // evaluated on the pre-/post-image specifically; the
                    // substitution below is post-image only. Honest error
                    // instead of a silently-wrong value.
                    for it in items.iter() {
                        if let spg_sql::ast::SelectItem::Expr { expr, .. } = it {
                            let mut bad = false;
                            crate::expr_analysis::rewrite_nodes_mut(&mut expr.clone(), &mut |n| {
                                if let Expr::Column(c) = n {
                                    if c.qualifier.as_deref().is_some_and(|q| {
                                        q.eq_ignore_ascii_case("old")
                                            || q.eq_ignore_ascii_case("new")
                                    }) && vr.computed.iter().any(|cc| cc.name == c.name)
                                    {
                                        bad = true;
                                    }
                                    return true;
                                }
                                false
                            });
                            if bad {
                                return Err(EngineError::Unsupported(alloc::format!(
                                    "OLD/NEW references to computed view column of view \"{}\" in MERGE RETURNING are not supported",
                                    stmt.target
                                )));
                            }
                        }
                    }
                    let is_target_q = |q: &Option<String>| match q.as_deref() {
                        None => true,
                        Some(x) => {
                            x.eq_ignore_ascii_case(&alias)
                                || x.eq_ignore_ascii_case("old")
                                || x.eq_ignore_ascii_case("new")
                        }
                    };
                    let mut out: alloc::vec::Vec<spg_sql::ast::SelectItem> =
                        alloc::vec::Vec::with_capacity(items.len());
                    let push_view_cols = |out: &mut alloc::vec::Vec<spg_sql::ast::SelectItem>| {
                        // Round 154 — `view_cols` carries the order when
                        // computed columns exist; a computed column
                        // projects its (target-qualified) expression.
                        if vr.view_cols.is_empty() {
                            for (view_col, base_col) in &vr.col_map {
                                out.push(spg_sql::ast::SelectItem::Expr {
                                    expr: Expr::Column(spg_sql::ast::ColumnName {
                                        qualifier: Some(alias.clone()),
                                        name: base_col.clone(),
                                    }),
                                    alias: Some(view_col.clone()),
                                });
                            }
                        } else {
                            for (view_col, base_col) in &vr.view_cols {
                                let expr = match base_col {
                                    Some(b) => Expr::Column(spg_sql::ast::ColumnName {
                                        qualifier: Some(alias.clone()),
                                        name: b.clone(),
                                    }),
                                    None => {
                                        let mut e = Expr::Column(spg_sql::ast::ColumnName {
                                            qualifier: None,
                                            name: view_col.clone(),
                                        });
                                        rewrite_view_refs_to_base(
                                            &mut e,
                                            &map,
                                            &cmap,
                                            Some(&alias),
                                        );
                                        e
                                    }
                                };
                                out.push(spg_sql::ast::SelectItem::Expr {
                                    expr,
                                    alias: Some(view_col.clone()),
                                });
                            }
                        }
                    };
                    for it in items.iter() {
                        match it {
                            spg_sql::ast::SelectItem::QualifiedWildcard(q)
                                if q.eq_ignore_ascii_case(&alias) =>
                            {
                                // `v.*` → the view's columns: base value,
                                // view-name output.
                                push_view_cols(&mut out);
                            }
                            // Bare `*` keeps PG's MERGE range-table order —
                            // source columns first, then the view's columns
                            // under their VIEW names.
                            spg_sql::ast::SelectItem::Wildcard => {
                                let src =
                                    s.source_alias.clone().unwrap_or_else(|| s.source.clone());
                                out.push(spg_sql::ast::SelectItem::QualifiedWildcard(src));
                                push_view_cols(&mut out);
                            }
                            spg_sql::ast::SelectItem::Expr { expr, alias: a } => {
                                let out_alias = if a.is_some() {
                                    a.clone()
                                } else if let Expr::Column(c) = expr
                                    && is_target_q(&c.qualifier)
                                    && (map.contains_key(&c.name) || cmap.contains_key(&c.name))
                                {
                                    Some(c.name.clone())
                                } else {
                                    a.clone()
                                };
                                let mut e = expr.clone();
                                crate::expr_analysis::rewrite_nodes_mut(&mut e, &mut |n| {
                                    if let Expr::Column(c) = n {
                                        if is_target_q(&c.qualifier) {
                                            if let Some(b) = map.get(&c.name) {
                                                c.name = b.clone();
                                            } else if cmap.contains_key(&c.name) {
                                                // Substitute the computed
                                                // column's expression,
                                                // target-qualified.
                                                let mut sub =
                                                    Expr::Column(spg_sql::ast::ColumnName {
                                                        qualifier: None,
                                                        name: c.name.clone(),
                                                    });
                                                rewrite_view_refs_to_base(
                                                    &mut sub,
                                                    &map,
                                                    &cmap,
                                                    Some(&alias),
                                                );
                                                *n = sub;
                                            }
                                        }
                                        return true;
                                    }
                                    false
                                });
                                out.push(spg_sql::ast::SelectItem::Expr {
                                    expr: e,
                                    alias: out_alias,
                                });
                            }
                            other => out.push(other.clone()),
                        }
                    }
                    *items = out;
                }
            }
            // v7.39 (round 152) — WITH CHECK OPTION rides the merge: the
            // outermost (written) view's own option drives the cascade,
            // and any lower view carrying its OWN option enforces
            // regardless of it (PG, probe P6).
            let written_opt = self
                .active_catalog()
                .views_all()
                .get(&stmt.target)
                .map_or(0, |v| v.check_option);
            let check = if written_opt != 0 || vr.check_chain.iter().any(|(_, _, o)| *o != 0) {
                Some(ViewCheck {
                    written_opt,
                    chain: vr.check_chain,
                })
            } else {
                None
            };
            return self.exec_merge_filtered(&s, vr.where_at_base, check, cancel);
        }
        self.exec_merge_filtered(stmt, None, None, cancel)
    }

    /// The MERGE executor proper. `target_filter` (the view's WHERE, over bare
    /// base columns) narrows which target rows the merge can see — rows outside
    /// it are neither matched nor eligible for a BY SOURCE clause, as in PG.
    fn exec_merge_filtered(
        &mut self,
        stmt: &spg_sql::ast::MergeStatement,
        target_filter: Option<Expr>,
        view_check: Option<ViewCheck>,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let target_alias = stmt
            .target_alias
            .clone()
            .unwrap_or_else(|| stmt.target.clone());
        let source_alias = stmt
            .source_alias
            .clone()
            .unwrap_or_else(|| stmt.source.clone());
        // v7.36 (cold-tier coverage) — MERGE mutates target rows in
        // place by hot row index. Cold-tier target rows can't be
        // mutated by this path (cold-row mutation is a v7.37 item),
        // so MATCHED clauses would silently skip them — losing the
        // INSERT-or-update semantic. PG and MariaDB never half-apply
        // a MERGE; surface the gap explicitly.
        // v7.39 (round 456) — O(1) predicate first; see the DELETE path.
        if let Some(t) = self.active_catalog().get(&stmt.target)
            && t.has_cold_rows_fast()
            && t.count_cold_locators() > 0
        {
            return Err(EngineError::Unsupported(alloc::format!(
                "MERGE INTO {:?}: cold-tier rows exist on target; \
                 cold-tier mutation by MERGE is a v7.37 candidate. \
                 Run COMPACT and retry.",
                stmt.target
            )));
        }
        let (target_cols, target_rows_snapshot) = {
            let t = self.active_catalog().get(&stmt.target).ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.target.clone(),
                })
            })?;
            // v7.37.15 Phase B — MERGE target snapshot consults
            // visibility so an in-flight delete on the target table
            // is not merged against. Phase B's `current_snapshot()`
            // returns unbounded (every row visible) — behaviour
            // matches pre-v7.37.15 exactly.
            // v7.39 (round 146) — keep each visible row's TRUE storage
            // position. The apply phase feeds update_row / delete_rows
            // storage positions; using the snapshot ordinal instead
            // mutated the wrong rows once dead versions (any prior MVCC
            // update/delete) preceded a target row.
            let snap = self.current_snapshot();
            let cols = t.schema().columns.clone();
            // v7.39 (round 148) — a view-redirected merge only sees the target
            // rows satisfying the view's WHERE (bare base-column predicate).
            let filter_ctx = EvalContext::new(&cols, None);
            let mut rows: Vec<(usize, Row<'static>)> = Vec::new();
            for (pos, r) in t.scan_visible(&snap) {
                if let Some(f) = &target_filter
                    && !matches!(eval::eval_expr(f, r, &filter_ctx), Ok(Value::Bool(true)))
                {
                    continue;
                }
                rows.push((pos, r.clone()));
            }
            (cols, rows)
        };
        let (source_cols, source_rows) = if let Some(sub) = &stmt.source_select {
            // v7.37 D.44 — `USING (SELECT …) alias` subquery source: materialise
            // the SELECT and use its result columns/rows as the merge input.
            let QueryResult::Rows { columns, rows } = self.exec_select_cancel(sub, cancel)? else {
                return Err(EngineError::Unsupported(alloc::format!(
                    "MERGE USING subquery did not return rows"
                )));
            };
            (columns, rows)
        } else {
            let s = self.active_catalog().get(&stmt.source).ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.source.clone(),
                })
            })?;
            // v7.36 — also fold cold-tier source rows into the merge
            // input. Source rows are read-only inputs (we never
            // mutate the source) so this is the same shape as
            // `materialise_table_ref`'s v7.35.1 cold-aware lift.
            // v7.37.15 Phase B — visibility-gated source scan.
            let snap = self.current_snapshot();
            let mut rows: Vec<Row<'static>> =
                s.scan_visible(&snap).map(|(_, r)| r.clone()).collect();
            rows.extend(crate::constraints::iter_cold_rows_of_parent(
                self.active_catalog(),
                s,
            ));
            (s.schema().columns.clone(), rows)
        };
        // Composite schema: target_alias.col ... source_alias.col ...
        let mut combined_schema: Vec<ColumnSchema> = Vec::new();
        for col in &target_cols {
            combined_schema.push(ColumnSchema::new(
                alloc::format!("{target_alias}.{}", col.name),
                col.ty,
                col.nullable,
            ));
        }
        for col in &source_cols {
            combined_schema.push(ColumnSchema::new(
                alloc::format!("{source_alias}.{}", col.name),
                col.ty,
                col.nullable,
            ));
        }
        let combined_ctx = EvalContext::new(&combined_schema, None);
        // Source-only context for WHEN NOT MATCHED actions (no
        // matched target row exists — the source-side qualified
        // columns must still resolve).
        let mut source_only_schema: Vec<ColumnSchema> = Vec::new();
        for col in &target_cols {
            source_only_schema.push(ColumnSchema::new(
                alloc::format!("{target_alias}.{}", col.name),
                col.ty,
                col.nullable,
            ));
        }
        for col in &source_cols {
            source_only_schema.push(ColumnSchema::new(
                alloc::format!("{source_alias}.{}", col.name),
                col.ty,
                col.nullable,
            ));
        }
        let source_only_ctx = EvalContext::new(&source_only_schema, None);
        let target_arity = target_cols.len();
        let source_arity = source_cols.len();

        // Resolve INSERT column positions once (validate names).
        // For each clause that's an INSERT, map column names → target positions.
        let mut delete_indices: Vec<usize> = Vec::new();
        let mut updates: Vec<(usize, Vec<Value<'static>>)> = Vec::new();
        let mut inserts: Vec<Vec<Value<'static>>> = Vec::new();
        let mut affected: usize = 0;
        // v7.39 (round 130) — RETURNING records, collected only when the
        // statement has a RETURNING clause.
        let want_returning = stmt.returning.is_some();
        let mut ret_records: Vec<MergeRetRecord> = Vec::new();
        // v7.39 (round 146, PG17) — target rows matched by ANY source row,
        // for the WHEN NOT MATCHED BY SOURCE pass below.
        let has_by_source = stmt
            .clauses
            .iter()
            .any(|c| matches!(c.matched, spg_sql::ast::MergeMatched::NotMatchedBySource));
        let mut ever_matched: alloc::collections::BTreeSet<usize> =
            alloc::collections::BTreeSet::new();

        for (src_idx, src_row) in source_rows.iter().enumerate() {
            if src_idx.is_multiple_of(256) {
                cancel.check()?;
            }
            // Find every matched target index (per the ON predicate).
            let mut matched_targets: Vec<usize> = Vec::new();
            for (t_idx, (_, t_row)) in target_rows_snapshot.iter().enumerate() {
                let mut combined_vals = t_row.values.clone();
                combined_vals.extend(src_row.values.iter().cloned());
                let combined_row = Row::new(combined_vals);
                let cond = eval::eval_expr(&stmt.on, &combined_row, &combined_ctx)?;
                if crate::eval::predicate_is_true(&cond, "JOIN/ON", combined_ctx.mysql_dialect)? {
                    matched_targets.push(t_idx);
                }
            }
            let is_matched = !matched_targets.is_empty();
            ever_matched.extend(matched_targets.iter().copied());
            // Pick the first WHEN clause whose kind agrees with
            // `is_matched` and whose AND condition (if any) holds.
            // AND condition for MATCHED: evaluated against the
            // first matched target row × source. For NOT MATCHED:
            // evaluated with target side NULL-padded.
            let fired_clause = stmt.clauses.iter().find(|c| {
                let kind_ok = match c.matched {
                    spg_sql::ast::MergeMatched::Matched => is_matched,
                    spg_sql::ast::MergeMatched::NotMatched => !is_matched,
                    // Fires from the target-side pass below, never per source row.
                    spg_sql::ast::MergeMatched::NotMatchedBySource => false,
                };
                if !kind_ok {
                    return false;
                }
                let Some(cond_expr) = &c.condition else {
                    return true;
                };
                let row = if is_matched {
                    let t = &target_rows_snapshot[matched_targets[0]].1;
                    let mut vals = t.values.clone();
                    vals.extend(src_row.values.iter().cloned());
                    Row::new(vals)
                } else {
                    let mut vals: Vec<Value<'static>> =
                        (0..target_arity).map(|_| Value::Null).collect();
                    vals.extend(src_row.values.iter().cloned());
                    Row::new(vals)
                };
                let ctx_ref = if is_matched {
                    &combined_ctx
                } else {
                    &source_only_ctx
                };
                matches!(
                    eval::eval_expr(cond_expr, &row, ctx_ref),
                    Ok(Value::Bool(true))
                )
            });
            let Some(clause) = fired_clause else { continue };
            match &clause.action {
                spg_sql::ast::MergeAction::DoNothing => {}
                spg_sql::ast::MergeAction::Delete => {
                    for &t_idx in &matched_targets {
                        let pos = target_rows_snapshot[t_idx].0;
                        if !delete_indices.contains(&pos) {
                            delete_indices.push(pos);
                            affected += 1;
                            if want_returning {
                                let t_row = &target_rows_snapshot[t_idx].1;
                                ret_records.push(MergeRetRecord {
                                    action: "DELETE",
                                    target_final: t_row.values.clone(),
                                    old: Some(t_row.values.clone()),
                                    new: None,
                                    source: src_row.values.clone(),
                                });
                            }
                        }
                    }
                }
                spg_sql::ast::MergeAction::Update { assignments } => {
                    // Pre-resolve SET targets to target column positions.
                    let mut planned_sets: Vec<(usize, &Expr)> =
                        Vec::with_capacity(assignments.len());
                    for (col, expr) in assignments {
                        let pos =
                            target_cols
                                .iter()
                                .position(|c| c.name == *col)
                                .ok_or_else(|| {
                                    EngineError::Eval(EvalError::ColumnNotFound {
                                        name: col.clone(),
                                    })
                                })?;
                        planned_sets.push((pos, expr));
                    }
                    for &t_idx in &matched_targets {
                        let (t_pos, t_row) = &target_rows_snapshot[t_idx];
                        let mut new_values = t_row.values.clone();
                        let mut combined_vals = t_row.values.clone();
                        combined_vals.extend(src_row.values.iter().cloned());
                        let combined_row = Row::new(combined_vals);
                        for (pos, expr) in &planned_sets {
                            let raw = eval::eval_expr(expr, &combined_row, &combined_ctx)?;
                            let coerced = coerce_value(
                                raw,
                                target_cols[*pos].ty,
                                &target_cols[*pos].name,
                                *pos,
                            )?;
                            new_values[*pos] = coerced;
                        }
                        if want_returning {
                            ret_records.push(MergeRetRecord {
                                action: "UPDATE",
                                target_final: new_values.clone(),
                                old: Some(t_row.values.clone()),
                                new: Some(new_values.clone()),
                                source: src_row.values.clone(),
                            });
                        }
                        updates.push((*t_pos, new_values));
                        affected += 1;
                    }
                }
                spg_sql::ast::MergeAction::Insert { columns, values } => {
                    // For INSERT NOT MATCHED, target side is NULL-padded.
                    let mut vals: Vec<Value<'static>> =
                        (0..target_arity).map(|_| Value::Null).collect();
                    vals.extend(src_row.values.iter().cloned());
                    let synth_row = Row::new(vals);
                    let mut new_row_values: Vec<Value<'static>> =
                        (0..target_arity).map(|_| Value::Null).collect();
                    // v7.39 (read01 round 123) — an omitted column list (`INSERT
                    // VALUES (…)`) maps the values positionally to every column
                    // in declaration order, like a plain INSERT.
                    if columns.is_empty() {
                        if values.len() > target_arity {
                            return Err(EngineError::Unsupported(alloc::format!(
                                "MERGE INSERT has more expressions ({}) than target columns ({target_arity})",
                                values.len()
                            )));
                        }
                        for (pos, expr) in values.iter().enumerate() {
                            let raw = eval::eval_expr(expr, &synth_row, &source_only_ctx)?;
                            let coerced = coerce_value(
                                raw,
                                target_cols[pos].ty,
                                &target_cols[pos].name,
                                pos,
                            )?;
                            new_row_values[pos] = coerced;
                        }
                    } else {
                        for (col, expr) in columns.iter().zip(values.iter()) {
                            let pos = target_cols.iter().position(|c| c.name == *col).ok_or_else(
                                || {
                                    EngineError::Eval(EvalError::ColumnNotFound {
                                        name: col.clone(),
                                    })
                                },
                            )?;
                            let raw = eval::eval_expr(expr, &synth_row, &source_only_ctx)?;
                            let coerced = coerce_value(
                                raw,
                                target_cols[pos].ty,
                                &target_cols[pos].name,
                                pos,
                            )?;
                            new_row_values[pos] = coerced;
                        }
                    }
                    if want_returning {
                        ret_records.push(MergeRetRecord {
                            action: "INSERT",
                            target_final: new_row_values.clone(),
                            old: None,
                            new: Some(new_row_values.clone()),
                            source: src_row.values.clone(),
                        });
                    }
                    inserts.push(new_row_values);
                    affected += 1;
                }
            }
        }
        // v7.39 (round 146, PG17) — WHEN NOT MATCHED BY SOURCE: a second pass
        // over the TARGET rows no source row matched. The source side does not
        // exist for these rows, so a source-alias column reference anywhere in
        // the clause is PG's "invalid reference" error, the eval row NULL-pads
        // the source columns, and the RETURNING record carries a NULL source.
        if has_by_source {
            for c in &stmt.clauses {
                if !matches!(c.matched, spg_sql::ast::MergeMatched::NotMatchedBySource) {
                    continue;
                }
                let mut quals: Vec<&str> = Vec::new();
                let mut all_q = true;
                if let Some(cond) = &c.condition {
                    crate::expr_analysis::collect_column_qualifiers(cond, &mut quals, &mut all_q);
                }
                if let spg_sql::ast::MergeAction::Update { assignments } = &c.action {
                    for (_, e) in assignments {
                        crate::expr_analysis::collect_column_qualifiers(e, &mut quals, &mut all_q);
                    }
                }
                if quals.iter().any(|q| q.eq_ignore_ascii_case(&source_alias)) {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "invalid reference to FROM-clause entry for table \"{source_alias}\" \
                         DETAIL: There is an entry for table \"{source_alias}\", but it cannot \
                         be referenced from this part of the query."
                    )));
                }
            }
            for (t_idx, (t_pos, t_row)) in target_rows_snapshot.iter().enumerate() {
                if ever_matched.contains(&t_idx) {
                    continue;
                }
                // Eval row: target values + NULL-padded source side.
                let mut vals = t_row.values.clone();
                vals.extend((0..source_arity).map(|_| Value::Null));
                let eval_row = Row::new(vals);
                let fired = stmt.clauses.iter().find(|c| {
                    if !matches!(c.matched, spg_sql::ast::MergeMatched::NotMatchedBySource) {
                        return false;
                    }
                    let Some(cond) = &c.condition else {
                        return true;
                    };
                    matches!(
                        eval::eval_expr(cond, &eval_row, &combined_ctx),
                        Ok(Value::Bool(true))
                    )
                });
                let Some(clause) = fired else { continue };
                let null_source: Vec<Value<'static>> =
                    (0..source_arity).map(|_| Value::Null).collect();
                match &clause.action {
                    spg_sql::ast::MergeAction::DoNothing => {}
                    spg_sql::ast::MergeAction::Delete => {
                        if !delete_indices.contains(t_pos) {
                            delete_indices.push(*t_pos);
                            affected += 1;
                            if want_returning {
                                ret_records.push(MergeRetRecord {
                                    action: "DELETE",
                                    target_final: t_row.values.clone(),
                                    old: Some(t_row.values.clone()),
                                    new: None,
                                    source: null_source,
                                });
                            }
                        }
                    }
                    spg_sql::ast::MergeAction::Update { assignments } => {
                        let mut new_values = t_row.values.clone();
                        for (col, expr) in assignments {
                            let pos = target_cols.iter().position(|c| c.name == *col).ok_or_else(
                                || {
                                    EngineError::Eval(EvalError::ColumnNotFound {
                                        name: col.clone(),
                                    })
                                },
                            )?;
                            let raw = eval::eval_expr(expr, &eval_row, &combined_ctx)?;
                            let coerced = coerce_value(
                                raw,
                                target_cols[pos].ty,
                                &target_cols[pos].name,
                                pos,
                            )?;
                            new_values[pos] = coerced;
                        }
                        if want_returning {
                            ret_records.push(MergeRetRecord {
                                action: "UPDATE",
                                target_final: new_values.clone(),
                                old: Some(t_row.values.clone()),
                                new: Some(new_values.clone()),
                                source: null_source,
                            });
                        }
                        updates.push((*t_pos, new_values));
                        affected += 1;
                    }
                    // The parser has no INSERT production under BY SOURCE.
                    spg_sql::ast::MergeAction::Insert { .. } => unreachable!(),
                }
            }
        }
        let _ = source_arity; // captured for symmetry; cancellation cost negligible.

        // v7.39 (round 152) — WITH CHECK OPTION through a MERGE: every
        // row an UPDATE or INSERT action produces must still satisfy
        // the view chain's quals (DELETE is exempt, as in PG). Checked
        // before anything applies so a violation leaves the table
        // untouched — PG never half-applies a MERGE.
        if let Some(check) = &view_check {
            let mut pending: alloc::vec::Vec<alloc::vec::Vec<Value<'static>>> =
                alloc::vec::Vec::with_capacity(updates.len() + inserts.len());
            for (_, new_vals) in &updates {
                pending.push(new_vals.clone());
            }
            for vals in &inserts {
                pending.push(vals.clone());
            }
            self.enforce_view_check(check, &pending, &target_cols, &stmt.target)?;
        }
        // v7.37.15 Phase C — fetch the writer version BEFORE
        // taking the table mut borrow (so we don't double-mut
        // self). Shared across MERGE INSERT / UPDATE / DELETE so
        // the whole statement commits atomically.
        let xmin = self.writer_version_for_current_stmt();
        // v7.37.15 (Phase C.3, step 4b) — in-place kill switch, read
        // before the table mut borrow (mirrors DELETE/UPDATE). When on,
        // MERGE UPDATE tombstones the matched old version + appends the
        // new version (both stamped with the shared `xmin`) instead of
        // an in-place replace. Default OFF → legacy update_row.
        let inplace = self.mvcc_inplace();
        // Apply the plan to the target table.
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.target)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.target.clone(),
                })
            })?;
        // Apply updates first (in-place), then deletes (one batch),
        // then inserts. The storage API uses `update_row(pos,
        // new_values)`, `delete_rows(&[positions])`, and `insert(row)`.
        for (idx, new_vals) in &updates {
            if inplace {
                // MVCC: tombstone old version (xmax = xmin) + append new
                // version (xmin). Appending keeps earlier positions valid,
                // so the remaining `idx` values in this loop stay correct.
                let _ = table.mark_row_deleted(*idx, xmin);
                table
                    .insert_with_xmin(Row::new(new_vals.clone()), xmin)
                    .map_err(EngineError::Storage)?;
            } else {
                table
                    .update_row(*idx, new_vals.clone())
                    .map_err(EngineError::Storage)?;
            }
        }
        if !delete_indices.is_empty() {
            table.delete_rows(&delete_indices);
        }
        // v7.37.15 Phase C — MERGE inserts share the pre-fetched
        // writer version (xmin captured above).
        for vals in inserts {
            table
                .insert_with_xmin(Row::new(vals), xmin)
                .map_err(EngineError::Storage)?;
        }
        let _ = table; // drop the mut borrow before building RETURNING.
        if let Some(items) = &stmt.returning {
            return self.build_merge_returning(
                &target_alias,
                &source_alias,
                &target_cols,
                &source_cols,
                ret_records,
                items,
            );
        }
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: affected > 0,
        })
    }

    /// v7.39 (round 130) — project a MERGE's `RETURNING` list over the actions
    /// it took. Each `MergeRetRecord` is flattened into a synthetic row whose
    /// columns cover the target alias (`t.col` = post-image), the source alias
    /// (`s.col`), the bare target names, `OLD.*`/`NEW.*` blocks, and a
    /// `__merge_action` cell; RETURNING expressions are rewritten onto those
    /// names (`merge_action()` → `__merge_action`, `OLD.c`/`NEW.c` → the block
    /// cells) and evaluated. Matches PG18 byte-identical.
    fn build_merge_returning(
        &self,
        target_alias: &str,
        source_alias: &str,
        target_cols: &[ColumnSchema],
        source_cols: &[ColumnSchema],
        records: Vec<MergeRetRecord>,
        items: &[SelectItem],
    ) -> Result<QueryResult, EngineError> {
        let t_arity = target_cols.len();
        // Synthetic schema: [t.col…] [s.col…] [bare target col…]
        //                   [__ret_old_col…] [__ret_new_col…] [__merge_action]
        let mut syn: Vec<ColumnSchema> = Vec::new();
        for c in target_cols {
            syn.push(ColumnSchema::new(
                alloc::format!("{target_alias}.{}", c.name),
                c.ty,
                c.nullable,
            ));
        }
        for c in source_cols {
            syn.push(ColumnSchema::new(
                alloc::format!("{source_alias}.{}", c.name),
                c.ty,
                c.nullable,
            ));
        }
        for c in target_cols {
            syn.push(ColumnSchema::new(c.name.clone(), c.ty, true));
        }
        for c in target_cols {
            syn.push(ColumnSchema::new(
                alloc::format!("__ret_old_{}", c.name),
                c.ty,
                true,
            ));
        }
        for c in target_cols {
            syn.push(ColumnSchema::new(
                alloc::format!("__ret_new_{}", c.name),
                c.ty,
                true,
            ));
        }
        syn.push(ColumnSchema::new(
            alloc::string::String::from("__merge_action"),
            DataType::Text,
            false,
        ));

        // Expand + rewrite the projection into flat (output_name, ty, expr)
        // triples against the synthetic schema.
        let expanded = expand_merge_returning_items(items, target_alias, source_alias, target_cols);
        let projection = build_projection(&expanded, &syn, "")?;
        let columns: Vec<ColumnSchema> = projection
            .iter()
            .map(|p| ColumnSchema::new(p.output_name.clone(), p.ty, p.nullable))
            .collect();

        let ctx = self.ev_ctx(&syn, None);
        let cancel = CancelToken::none();
        let mut out_rows: Vec<Row<'static>> = Vec::with_capacity(records.len());
        for rec in &records {
            let null_block = || alloc::vec![Value::Null; t_arity];
            let mut vals: Vec<Value<'static>> = Vec::with_capacity(syn.len());
            vals.extend(rec.target_final.iter().cloned()); // t.col
            vals.extend(rec.source.iter().cloned()); // s.col
            vals.extend(rec.target_final.iter().cloned()); // bare target col
            vals.extend(rec.old.clone().unwrap_or_else(null_block)); // __ret_old_
            vals.extend(rec.new.clone().unwrap_or_else(null_block)); // __ret_new_
            vals.push(Value::Text(rec.action.into())); // __merge_action
            let syn_row = Row::new(vals);
            let mut proj: Vec<Value<'static>> = Vec::with_capacity(projection.len());
            for p in &projection {
                proj.push(self.eval_expr_with_correlated(&p.expr, &syn_row, &ctx, cancel, None)?);
            }
            out_rows.push(Row::new(proj));
        }
        Ok(QueryResult::Rows {
            columns,
            rows: out_rows,
        })
    }

    pub(crate) fn exec_delete_cancel(
        &mut self,
        stmt: &spg_sql::ast::DeleteStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        // v7.37.43-T4.4 — writable CTE outer body (DELETE).
        if !stmt.ctes.is_empty() {
            return self.exec_delete_with_ctes(stmt.clone(), cancel);
        }
        // v7.39 (RLS) Phase 2 — DELETE USING visibility: only delete rows the
        // policy-subject session can see (a hidden row is silently skipped).
        let rls_del;
        let stmt = match self.rls_write_using_predicate(&stmt.table, spg_storage::PolicyCmd::Delete)
        {
            Some(pred) => {
                let mut s = stmt.clone();
                s.where_ = and_optional_predicates(s.where_.take(), Some(pred));
                rls_del = s;
                &rls_del
            }
            None => stmt,
        };
        // v7.39 (round 139/141) — DO INSTEAD NOTHING rules narrow the DELETE.
        // Unconditional → AND a constant FALSE (deletes nothing). Conditional
        // (WHERE over OLD) → AND `COALESCE(NOT(cond), TRUE)` so only rows the
        // rule does not block are deleted. Either way the normal path runs so the
        // `DELETE n` tag / RETURNING stay byte-identical. A RETURNING on a fully
        // (unconditionally) suppressed statement is rejected as PG does.
        // v7.39 (round 333, V59) — as the caller wrote it; the conditional
        // instead-command action runs against THESE rows.
        let unnarrowed_del = stmt;
        let rule_blocked_del;
        let del_block = if self.rule_blocks_statement(&stmt.table, "DELETE") {
            if stmt.returning.is_some() {
                return Err(crate::rules::rule_returning_error("DELETE", &stmt.table));
            }
            Some(spg_sql::ast::Expr::Literal(spg_sql::ast::Literal::Bool(
                false,
            )))
        } else {
            self.conditional_block_predicate(&stmt.table, "DELETE", &[])?
        };
        let stmt = if let Some(pred) = del_block {
            let mut s = stmt.clone();
            s.where_ = and_optional_predicates(s.where_.take(), Some(pred));
            rule_blocked_del = s;
            &rule_blocked_del
        } else {
            stmt
        };
        // v7.39 (round 140/142) — rule-command forms. DO INSTEAD <command>
        // replaces the DELETE (original op never runs, tag `DELETE 0`); DO ALSO
        // runs the real delete first, then the commands. Both bind OLD per
        // matching row. The guard bounds a rule → delete cycle.
        if !self.rule_rewrite_active {
            let instead_del = self.instead_command_rules(&stmt.table, "DELETE");
            // v7.39 (round 333, V59) — see the UPDATE path: unconditional
            // replaces the statement, conditional only claims its own rows.
            let (uncond, cond): (Vec<_>, Vec<_>) = instead_del
                .into_iter()
                .partition(|r| r.when_condition.is_empty());
            if !uncond.is_empty() {
                let mut all = uncond;
                all.extend(cond);
                return self.exec_delete_instead_command(unnarrowed_del, all, cancel);
            }
            if !cond.is_empty() {
                let (columns, old_rows) =
                    self.scan_relation_rows(&unnarrowed_del.table, &unnarrowed_del.where_, cancel)?;
                let rows: Vec<(Option<Row<'static>>, Option<Row<'static>>)> =
                    old_rows.into_iter().map(|r| (None, Some(r))).collect();
                self.run_also_rules(&cond, &columns, &rows, cancel)?;
            }
            let also_del = self.also_rules(&stmt.table, "DELETE");
            if !also_del.is_empty() {
                return self.exec_delete_with_also(stmt, also_del, cancel);
            }
        }
        // v7.39 (round 137, Phase 2) — INSTEAD OF DELETE trigger on the target
        // view fires per matching view row instead of the auto-updatable
        // redirect.
        let iof_del = self.snapshot_row_triggers(&stmt.table, "DELETE", "INSTEAD OF");
        if !iof_del.is_empty() {
            return self.exec_delete_view_instead_of(stmt, iof_del, cancel);
        }
        // v7.37.19 (19.13) — auto-updatable view redirect. v7.38 (P6.46) — a
        // view WHERE is AND-ed onto the caller's so only rows visible through
        // the view are deleted.
        // v7.39 (round 267) — the view exists but is not auto-updatable.
        // Without this the write fell through to the base-table lookup and
        // reported `relation "<view>" does not exist`, which is not merely
        // the wrong wording — it denies the existence of an object the
        // catalog plainly has.
        if let Err(reason) = view_redirect_checked(self.active_catalog(), &stmt.table) {
            if self.active_catalog().has_view(&stmt.table) {
                return Err(view_not_updatable_error(&stmt.table, &DELETE_VERB, reason, false));
            }
        }
        if let Some(vr) = view_redirect_to_simple_base(self.active_catalog(), &stmt.table) {
            // DELETE removes rows; there is no post-image to check, so
            // WITH CHECK OPTION does not apply. The composed WHERE (all nested
            // levels AND-ed) still restricts which base rows are visible.
            let mut rewritten = stmt.clone();
            // v7.39 (round 133) — column-renamed view: rewrite the WHERE's view
            // columns to base columns before AND-ing the view WHERE. Round 154
            // — a computed column READ substitutes its defining expression.
            if !vr.col_map.is_empty() || !vr.computed.is_empty() {
                let map: alloc::collections::BTreeMap<String, String> =
                    vr.col_map.iter().cloned().collect();
                let cmap: alloc::collections::BTreeMap<String, Expr> = vr
                    .computed
                    .iter()
                    .map(|c| (c.name.clone(), c.def.clone()))
                    .collect();
                if let Some(w) = &mut rewritten.where_ {
                    rewrite_view_refs_to_base(w, &map, &cmap, None);
                }
                // v7.39 (round 134) — rewrite RETURNING view cols → base cols.
                if let Some(ret) = &rewritten.returning {
                    rewritten.returning = Some(rewrite_view_returning_items(
                        ret,
                        &vr.col_map,
                        &vr.computed,
                        &vr.view_cols,
                    ));
                }
            }
            rewritten.table = vr.base;
            rewritten.where_ = and_optional_predicates(vr.where_at_base, rewritten.where_);
            return self.exec_delete_cancel(&rewritten, cancel);
        }
        // v7.37 D.46 — DELETE on a partition parent fans out to every child;
        // the parent table holds no rows of its own, so a parent-targeted DELETE
        // would otherwise silently affect nothing. Each child DELETE re-evaluates
        // the same WHERE against that child's rows (and recurses for sub-partitions,
        // since a child is not itself a parent unless further partitioned).
        if crate::partition::is_partition_parent(self.active_catalog(), &stmt.table) {
            let children = crate::partition::children_of_parent(self.active_catalog(), &stmt.table);
            let mut total_affected = 0usize;
            let mut ret_columns: Option<Vec<ColumnSchema>> = None;
            let mut ret_rows: Vec<Row<'static>> = Vec::new();
            for child in children {
                let mut child_stmt = stmt.clone();
                child_stmt.table = child;
                match self.exec_delete_cancel(&child_stmt, cancel)? {
                    QueryResult::CommandOk { affected, .. } => total_affected += affected,
                    QueryResult::Rows { columns, rows } => {
                        total_affected += rows.len();
                        ret_columns = Some(columns);
                        ret_rows.extend(rows);
                    }
                }
            }
            return Ok(match ret_columns {
                // DELETE … RETURNING: merge every child's returned rows.
                Some(columns) => QueryResult::Rows {
                    columns,
                    rows: ret_rows,
                },
                None => QueryResult::CommandOk {
                    affected: total_affected,
                    modified_catalog: true,
                },
            });
        }
        // v7.12.5 — snapshot BEFORE/AFTER DELETE row triggers + the
        // session FTS config before the mut borrow (same shape as
        // INSERT / UPDATE).
        let before_delete_triggers = self.snapshot_row_triggers(&stmt.table, "DELETE", "BEFORE");
        let after_delete_triggers = self.snapshot_row_triggers(&stmt.table, "DELETE", "AFTER");
        let trigger_session_cfg: Option<String> = self
            .session_params
            .get("default_text_search_config")
            .cloned();
        // v5.2.3: PK-targeted DELETE → first retire any cold-tier
        // locator for the key. The cold row body stays in the
        // segment (becoming shadowed garbage that a future
        // compaction pass reclaims) but the index no longer
        // resolves it. The shadow count contributes to the
        // affected total; the subsequent hot walk handles any hot
        // rows for the same key.
        let mut cold_shadow_count: usize = 0;
        if let Some(w) = &stmt.where_ {
            let schema_cols = self
                .active_catalog()
                .get(&stmt.table)
                .ok_or_else(|| {
                    EngineError::Storage(StorageError::TableNotFound {
                        name: stmt.table.clone(),
                    })
                })?
                .schema()
                .columns
                .clone();
            if let Some((col_pos, key)) = try_pk_predicate(w, &schema_cols, stmt.table.as_str())
                && let Some(idx_name) = self
                    .active_catalog()
                    .get(&stmt.table)
                    .and_then(|t| t.index_on(col_pos).map(|i| i.name.clone()))
            {
                cold_shadow_count = self
                    .active_catalog_mut()
                    .shadow_cold_row(&stmt.table, &idx_name, &key)
                    .unwrap_or(0);
            } else {
                // v7.36 (cold-tier coverage) — DELETE with a non-PK
                // WHERE on a cold-bearing table previously missed
                // cold-tier matching rows. Walk each cold row, eval
                // WHERE against it, and shadow the matching PK keys
                // (retiring the cold-tier index entries; the row
                // body becomes shadowed garbage that compaction
                // reclaims).
                let pre_shadow_keys: Vec<spg_storage::IndexKey> = {
                    let mut keys = Vec::new();
                    // v7.39 (round 456) — `has_cold_rows_fast()` first.
                    //
                    // A profile of a range-predicated DELETE put 57.8% of
                    // self-time in `count_cold_locators`, which walks every
                    // (key, locator) pair of every BTree index — O(table) —
                    // to answer a question that is only ever asked as
                    // "> 0". That is the whole of this shape's cost scaling
                    // with table size: a one-row range DELETE costs 0.024 ms
                    // at 10k rows and 1.220 ms at 200k, while the same
                    // delete by equality (which never reaches here) stays at
                    // 0.006 ms.
                    //
                    // `has_cold_rows_fast` is the O(1) predicate v7.36 added
                    // for exactly this, and its own doc comment says the
                    // O(N) walk is "unsuitable per join stage". It is
                    // conservative — true when the cached count is stale —
                    // so it short-circuits rather than replaces: the exact
                    // walk still runs whenever cold rows might exist, and
                    // the answer is unchanged.
                    if let Some(t) = self.active_catalog().get(&stmt.table)
                        && t.has_cold_rows_fast()
                        && t.count_cold_locators() > 0
                    {
                        let ctx = eval::EvalContext::new(&schema_cols, Some(stmt.alias.as_deref().unwrap_or(stmt.table.as_str())));
                        for (key, row) in
                            crate::constraints::iter_cold_rows_with_pk_key(self.active_catalog(), t)
                        {
                            let cond = eval::eval_expr(w, &row, &ctx).map_err(EngineError::Eval)?;
                            if crate::eval::predicate_is_true(&cond, "WHERE", ctx.mysql_dialect)? {
                                keys.push(key);
                            }
                        }
                    }
                    keys
                };
                if !pre_shadow_keys.is_empty()
                    && let Some(idx_name) = self
                        .active_catalog()
                        .get(&stmt.table)
                        .and_then(crate::constraints::pk_btree_index_name)
                {
                    for key in pre_shadow_keys {
                        cold_shadow_count += self
                            .active_catalog_mut()
                            .shadow_cold_row(&stmt.table, &idx_name, &key)
                            .unwrap_or(0);
                    }
                }
            }
        }

        // v7.37.17 (Phase E2) — a cold-tier shadow is a PHYSICAL index
        // edit inside the tx's shadow catalog, not a versioned row
        // write; the RC rebase cannot re-express it. Poison the rebase
        // so this tx keeps its frozen view (never loses the shadow).
        if cold_shadow_count > 0 && self.in_transaction() {
            self.poison_tx_rebase();
        }
        // v7.12.1 — cache the session FTS config as an owned
        // String before the mutable table borrow below; the
        // ctx-builder then references it via `as_deref` so the
        // immutable read of `session_params` doesn't conflict
        // with the mut borrow chain.
        let ts_cfg: Option<String> = self
            .session_param("default_text_search_config")
            .map(String::from);
        // A WHERE containing subqueries (DELETE … USING lowers to
        // EXISTS) cannot evaluate inside the &mut walk below — the
        // correlated resolver re-enters the engine read path.
        // Resolve the matching hot positions up front, immutably.
        let subquery_hits: Option<Vec<usize>> = if stmt
            .where_
            .as_ref()
            .is_some_and(|w| crate::subquery::expr_has_subquery(w))
        {
            let w = stmt.where_.as_ref().expect("guarded above");
            let table = self.active_catalog().get(&stmt.table).ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
            let schema_cols: Vec<ColumnSchema> = table.schema().columns.clone();
            // v7.39 (read01 round 54) — carry the catalog (see above).
            let cat_for_ctx = self.active_catalog().clone();
            let ctx = EvalContext::new(&schema_cols, Some(stmt.alias.as_deref().unwrap_or(stmt.table.as_str())))
                .with_default_text_search_config(ts_cfg.as_deref())
                .with_catalog(&cat_for_ctx);
            // v7.37.16 (gate-on inventory) — visibility gate, same as
            // the main walk below: a tombstoned version must not be
            // re-targeted. No-op under gate-off.
            let snap = self.current_snapshot();
            let mut hits: Vec<usize> = Vec::new();
            for i in 0..table.row_count() {
                if i.is_multiple_of(256) {
                    cancel.check()?;
                }
                if !table.is_row_visible(i, &snap) {
                    continue;
                }
                let Some(row) = table.rows().get(i) else {
                    continue;
                };
                let cond = self.eval_expr_with_correlated(w, row, &ctx, cancel, None)?;
                if crate::eval::predicate_is_true(&cond, "WHERE", ctx.mysql_dialect)? {
                    hits.push(i);
                }
            }
            Some(hits)
        } else {
            None
        };
        // v7.37.16 — snapshot for the DELETE target walk, taken BEFORE
        // the &mut table borrow below (current_snapshot needs &self).
        let scan_snapshot = self.current_snapshot();
        // v7.37.16 — old-row snapshot need, decided before the &mut
        // borrow too (reads the catalog): FK children, row triggers, or
        // RETURNING are the only consumers of `to_delete_rows`.
        let need_old_rows = !before_delete_triggers.is_empty()
            || !after_delete_triggers.is_empty()
            || stmt.returning.is_some()
            || crate::constraints::any_fk_child_references(self.active_catalog(), &stmt.table);
        // v7.39 (read01 round 54) — carry the catalog. Without it an
        // `m = 'happy'::mood` (or ::regclass / composite / domain) in an
        // UPDATE / DELETE WHERE failed outright with "unsupported cast target
        // `::mood`" — the same SELECT worked. Catalog::clone is a structural
        // Arc bump, so it is cheap; taken BEFORE the &mut borrow below.
        let cat_for_ctx = self.active_catalog().clone();
        // v7.39 (round 432) — MySQL `DELETE … ORDER BY … [LIMIT n]`. Read the
        // session dialect BEFORE the &mut catalog borrow below, which makes
        // `self` unreachable for the rest of the scan. (DELETE's ctx, like
        // UPDATE's, does not thread the engine — a long-standing site.)
        let mysql_order_limit = if self.backslash_escapes {
            stmt.order_limit.as_deref()
        } else {
            None
        };
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        let schema_cols: Vec<ColumnSchema> = table.schema().columns.clone();
        let ctx = EvalContext::new(&schema_cols, Some(stmt.alias.as_deref().unwrap_or(stmt.table.as_str())))
            .with_default_text_search_config(ts_cfg.as_deref())
            .with_catalog(&cat_for_ctx);
        let mut positions: Vec<usize> = Vec::new();
        // v7.6.3 — collect every to-delete row's full Value tuple
        // alongside its position, so the FK enforcement pass can
        // run after the mut borrow drops.
        // v7.37.16 — only when `need_old_rows` (see above): a plain
        // DELETE on an unreferenced, trigger-free table skips ~1 ms of
        // per-row clones on a 10k-row delete; plan_fk_parent_deletions
        // early-returns on the empty rows slice.
        let mut to_delete_rows: Vec<Vec<Value<'static>>> = Vec::new();
        // v7.20 P4 — index seek (same shape as exec_update_cancel):
        // an equality WHERE on an indexed column narrows the walk
        // to the matching hot positions; the full WHERE still
        // re-evaluates per candidate. Downstream passes assume
        // ascending position order, so the seek result is sorted.
        let seek_positions: Option<Vec<usize>> = stmt
            .where_
            .as_ref()
            .and_then(|w| try_index_seek_positions(w, &schema_cols, table, stmt.table.as_str()));
        let candidate_positions: Vec<usize> = match seek_positions {
            Some(mut list) => {
                list.sort_unstable();
                list
            }
            None => {
                // v7.39 (round 455) — a mutation that finds no usable index
                // walks the table, and PG counts those tuples: its
                // `pg_stat_user_tables.seq_tup_read` covers DML, not just
                // SELECT. SPG only reported from `scan_visible`, which the
                // mutation paths do not use, so an UPDATE or DELETE that
                // scanned every row reported reading none — measured in
                // round 454 with a deliberately unindexed predicate over
                // 50k rows, which came back 0.
                //
                // Silently-wrong monitoring for any write workload, and the
                // instrument the round-452 investigation needs to tell a
                // range mutation's seek from a scan.
                table.note_seq_scan();
                (0..table.row_count()).collect()
            }
        };
        for (loop_n, &i) in candidate_positions.iter().enumerate() {
            if loop_n.is_multiple_of(256) {
                cancel.check()?;
            }
            // v7.37.16 (gate-on inventory) — a DELETE must not re-target
            // a tombstoned version (double-delete / doubled affected
            // counts under gate-on). No-op under gate-off.
            if !table.is_row_visible(i, &scan_snapshot) {
                continue;
            }
            let Some(row) = table.rows().get(i) else {
                continue;
            };
            let keep = if let Some(hits) = &subquery_hits {
                hits.binary_search(&i).is_err()
            } else if let Some(w) = &stmt.where_ {
                let cond = eval::eval_expr(w, row, &ctx)?;
                !crate::eval::predicate_is_true(&cond, "WHERE", ctx.mysql_dialect)?
            } else {
                false
            };
            if !keep {
                positions.push(i);
                if need_old_rows {
                    to_delete_rows.push(row.values.clone());
                }
            }
        }
        // v7.39 (round 432) — MySQL `DELETE … ORDER BY … LIMIT n`: order the
        // MATCHED rows by the ORDER BY keys (evaluated on the pre-delete row),
        // keep the first `limit`, then restore position order — every pass
        // below (FK pairing, trigger walks, the apply loop) reads `positions`
        // assuming ascending row order. `to_delete_rows` is parallel to
        // `positions` when it is populated at all, so it moves in lockstep.
        if let Some(ol) = mysql_order_limit {
            if !ol.order_by.is_empty() {
                let mut keyed: Vec<(Vec<Value<'static>>, usize)> = positions
                    .iter()
                    .map(|&pos| {
                        let row = table
                            .rows()
                            .get(pos)
                            .expect("matched position was visible under scan_snapshot");
                        let keys: Vec<Value<'static>> = ol
                            .order_by
                            .iter()
                            .map(|o| eval::eval_expr(&o.expr, row, &ctx))
                            .collect::<Result<_, _>>()?;
                        Ok::<_, EngineError>((keys, pos))
                    })
                    .collect::<Result<_, _>>()?;
                keyed.sort_by(|a, b| {
                    for (k, o) in ol.order_by.iter().enumerate() {
                        let cmp = crate::order_by_value_cmp_in(
                            o.desc,
                            o.nulls_first,
                            &a.0[k],
                            &b.0[k],
                            true,
                        );
                        if cmp != core::cmp::Ordering::Equal {
                            return cmp;
                        }
                    }
                    core::cmp::Ordering::Equal
                });
                positions = keyed.into_iter().map(|(_, pos)| pos).collect();
            }
            if let Some(n) = ol.limit {
                positions.truncate(n as usize);
            }
            positions.sort_unstable();
            if need_old_rows {
                let kept: Vec<Vec<Value<'static>>> = positions
                    .iter()
                    .map(|&pos| {
                        table
                            .rows()
                            .get(pos)
                            .expect("kept position was matched above")
                            .values
                            .clone()
                    })
                    .collect();
                to_delete_rows = kept;
            }
        }
        // v7.6.3 / v7.6.4 — Stage 2: FK enforcement on the immutable
        // catalog. Release the mut borrow and run reverse-scan
        // against every child table whose FK targets this table.
        // RESTRICT / NoAction raise an error; CASCADE returns a
        // cascade plan that stage 3 applies after the primary delete.
        // SET NULL / SET DEFAULT remain Unsupported until v7.6.5.
        let _ = table;
        // v7.12.5 — BEFORE DELETE row-level triggers. Each fires
        // with NEW=None / OLD=pre-delete row; RETURN OLD (or NEW)
        // = proceed, RETURN NULL = skip the row entirely. The
        // filter must run BEFORE the FK cascade plan so cascaded
        // child rows track the trigger's skip-decision on the
        // parent.
        // v7.12.7 — embedded SQL queue.
        let mut deferred_embedded: Vec<triggers::DeferredEmbeddedStmt> = Vec::new();
        if !before_delete_triggers.is_empty() {
            let mut filtered_positions: Vec<usize> = Vec::with_capacity(positions.len());
            let mut filtered_old_rows: Vec<Vec<Value<'static>>> =
                Vec::with_capacity(to_delete_rows.len());
            for (pos, old_vals) in positions.iter().zip(to_delete_rows.iter()) {
                let old_row = Row::new(old_vals.clone());
                let mut cancel_this = false;
                for (fd, when, tgname) in &before_delete_triggers {
                    if !triggers::trigger_when_holds(when, None, Some(&old_row), &schema_cols)? {
                        continue;
                    }
                    let (outcome, deferred) = triggers::fire_row_trigger(
                        fd,
                        None,
                        Some(&old_row),
                        &stmt.table,
                        &schema_cols,
                        &[],
                        trigger_session_cfg.as_deref(),
                        false,
                        &triggers::TgMeta {
                            op: "DELETE",
                            name: tgname,
                            level: "ROW",
                        },
                    )
                    .map_err(|e| {
                        EngineError::Storage(StorageError::Corrupt(alloc::format!("{e}")))
                    })?;
                    deferred_embedded.extend(deferred);
                    if matches!(outcome, triggers::TriggerOutcome::Skip) {
                        cancel_this = true;
                        break;
                    }
                }
                if !cancel_this {
                    filtered_positions.push(*pos);
                    filtered_old_rows.push(old_vals.clone());
                }
            }
            positions = filtered_positions;
            to_delete_rows = filtered_old_rows;
        }
        let cascade_plan = plan_fk_parent_deletions(
            self.active_catalog(),
            &stmt.table,
            &positions,
            &to_delete_rows,
        )?;
        // Stage 3a — apply each FK child step (SET NULL / SET
        // DEFAULT / CASCADE delete) before deleting the parent.
        // The plan is already ordered: nulls/defaults first, then
        // cascade deletes (so a row mutated and later deleted
        // surfaces as deleted — though v7.6.5 doesn't produce
        // that overlap today).
        for step in &cascade_plan {
            apply_fk_child_step(self.active_catalog_mut(), step)?;
        }
        // Stage 3b — actually delete the original target rows.
        // v7.37.15 Phase C — stamp xmax on each row's header with
        // the enclosing tx's version BEFORE the physical removal.
        // Fetch the version BEFORE the table mut borrow so we
        // don't double-mut self. Shared across the whole DELETE
        // statement so all tombstones commit atomically.
        let xmax = self.writer_version_for_current_stmt();
        let _ = xmax; // pre-fetched; consumed below
        // v7.37.15 (Phase C.3, step 4a) — read the in-place kill switch
        // before the table mut borrow. When on, DELETE tombstones the
        // rows (xmax stamped, kept physically present) instead of
        // physically removing them: the now-uniformly-gated readers
        // skip them and vacuum reclaims later, giving concurrent
        // snapshots a consistent view. Default OFF → legacy physical
        // delete, byte-for-byte unchanged.
        //
        // KNOWN follow-ups before this gate can flip in production
        // (see plan appendix B): (6) WAL redo capture for the tombstone
        // — `mark_row_deleted` does not record a RowChange, so gate-on
        // is in-memory-only until Epic W wires tombstone redo; (1)
        // unique/FK constraint checks must adopt MVCC/dirty-snapshot
        // semantics so a re-insert of a tombstoned key succeeds; (8)
        // cold-tier tombstoning via the cold overlay (step 6).
        let inplace = self.mvcc_inplace();
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        let affected = if inplace {
            // v7.37.15 (Epic W durable-tombstone slice) — stamp xmax
            // ONLY on the in-place path; the gate-off path below removes
            // the rows via `delete_rows` (which records
            // `RowChange::Delete`), so tombstoning there too would
            // double-log the deletion.
            // v7.37.16 — BATCH tombstone: one pass, ONE multi-rowid
            // `RowChange::Tombstone` redo record instead of one per row
            // (~800 ns/row on a 10k-row DELETE). The return value counts
            // rows NEWLY tombstoned — positions come from the
            // visibility-gated WHERE scan above, so every entry is a
            // live, visible row and the count equals the distinct
            // position count the per-row loop used to compute.
            table.mark_rows_deleted(&positions, xmax) + cold_shadow_count
        } else {
            table.delete_rows(&positions) + cold_shadow_count
        };
        let _ = table;
        // v7.12.5 — AFTER DELETE row-level triggers fire post-write
        // with NEW=None / OLD=pre-delete row (each from the
        // already-snapshotted to_delete_rows). Return value is
        // ignored (matches PG AFTER semantics).
        if !after_delete_triggers.is_empty() {
            for old_vals in &to_delete_rows {
                let old_row = Row::new(old_vals.clone());
                for (fd, when, tgname) in &after_delete_triggers {
                    if !triggers::trigger_when_holds(when, None, Some(&old_row), &schema_cols)? {
                        continue;
                    }
                    let (_outcome, deferred) = triggers::fire_row_trigger(
                        fd,
                        None,
                        Some(&old_row),
                        &stmt.table,
                        &schema_cols,
                        &[],
                        trigger_session_cfg.as_deref(),
                        true,
                        &triggers::TgMeta {
                            op: "DELETE",
                            name: tgname,
                            level: "ROW",
                        },
                    )
                    .map_err(|e| {
                        EngineError::Storage(StorageError::Corrupt(alloc::format!("{e}")))
                    })?;
                    deferred_embedded.extend(deferred);
                }
            }
        }
        // v7.12.7 — drain trigger-emitted embedded SQL for this DELETE.
        self.execute_deferred_trigger_stmts(deferred_embedded, cancel)?;
        // v6.2.1 — auto-analyze modified-row tracking for DELETE.
        if !self.in_transaction() && affected > 0 {
            self.statistics
                .record_modifications(&stmt.table, affected as u64);
        }
        // v7.37.16 — autovacuum trigger (no-op gate-off / in-tx /
        // below the dead-row threshold).
        if affected > 0 {
            self.maybe_autovacuum(&stmt.table);
        }
        // v7.9.4 — RETURNING projection over the soon-to-be-gone
        // rows. `to_delete_rows` was snapshotted in stage 1 before
        // mutation, so the projection sees the pre-delete state
        // (matches PG semantics: DELETE RETURNING returns the row
        // as it was just before removal).
        if let Some(items) = &stmt.returning {
            // v7.39 (round 126) — DELETE: OLD = the deleted row (= default),
            // NEW = NULL (the row no longer exists).
            let old_for_returning = to_delete_rows.clone();
            return self.build_returning_rows_old_new(
                &stmt.table,
                stmt.alias.as_deref(),
                items,
                to_delete_rows,
                Some(old_for_returning),
                None,
            );
        }
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: !self.in_transaction(),
        })
    }

    /// Snapshot the per-INSERT catalog state the row loop depends on,
    /// taken while the catalog is still immutably borrowable (before the
    /// `get_mut` window): the clock fn, BEFORE/AFTER row triggers + their
    /// session config, the column ENUM / SET variant lookups, and the
    /// AUTO_INCREMENT sequence floors.
    fn prepare_insert_snapshots(&self, table_name: &str) -> Result<InsertSnapshots, EngineError> {
        // v7.9.21 — snapshot the clock fn pointer before the mut
        // borrow on the catalog opens; runtime DEFAULT eval needs
        // it inside the row hot loop.
        let clock = self.clock;
        // v7.12.4 — snapshot row-level triggers + their referenced
        // functions before the mut borrow on the catalog opens.
        // Cloned out so the row hot loop can fire them without
        // re-borrowing the catalog (which would conflict with
        // table.insert's mutable borrow).
        let before_insert_triggers = self.snapshot_row_triggers(table_name, "INSERT", "BEFORE");
        let after_insert_triggers = self.snapshot_row_triggers(table_name, "INSERT", "AFTER");
        let trigger_session_cfg: Option<alloc::string::String> = self
            .session_params
            .get("default_text_search_config")
            .cloned();
        // v7.17.0 Phase 1.4 — snapshot the enum label lookup BEFORE
        // opening the mutable borrow on the table below. We need
        // catalog-level read access (enum_types lives at the
        // catalog level, not the table) and the upcoming mutable
        // borrow shadows it.
        let pre_borrow_column_meta: Vec<ColumnSchema> = {
            let preview_table = self.active_catalog().get(table_name).ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: String::from(table_name),
                })
            })?;
            preview_table.schema().columns.clone()
        };
        let enum_label_lookup: alloc::collections::BTreeMap<usize, Vec<String>> =
            pre_borrow_column_meta
                .iter()
                .enumerate()
                .filter_map(|(i, col)| {
                    // v7.17.0 Phase 3.P0-36 — MySQL inline ENUM
                    // variant lists take priority over the PG
                    // catalog enum_types lookup (they're
                    // column-local and authoritative when set).
                    if let Some(inline) = &col.inline_enum_variants {
                        return Some((i, inline.clone()));
                    }
                    col.user_enum_type.as_ref().and_then(|ename| {
                        self.active_catalog()
                            .enum_types()
                            .get(ename)
                            .map(|e| (i, e.labels.clone()))
                    })
                })
                .collect();
        // v7.17.0 Phase 3.P0-37 — MySQL inline SET variant lists.
        // Distinct from enum_label_lookup: SET validates that
        // every comma-separated token is in the variant list, and
        // canonicalises the cell to definition-order de-duped text.
        let set_variant_lookup: alloc::collections::BTreeMap<usize, Vec<String>> =
            pre_borrow_column_meta
                .iter()
                .enumerate()
                .filter_map(|(i, col)| col.inline_set_variants.as_ref().map(|vs| (i, vs.clone())))
                .collect();
        // v7.29 (round-23a) - when the column's implicit sequence
        // exists (born on first nextval/setval address), a setval
        // above the table MAX moves the next auto-assigned id:
        // assign from max(table_max + 1, last_value + 1). Tables
        // whose sequence was never addressed keep the bare max+1
        // path (identical pre-7.29 behaviour, no lookup cost
        // beyond one map probe per auto column per statement).
        let mut seq_floors: alloc::collections::BTreeMap<usize, i64> =
            alloc::collections::BTreeMap::new();
        for (i, col) in pre_borrow_column_meta.iter().enumerate() {
            if col.auto_increment
                && let Some(sd) = self.active_catalog().sequence(&alloc::format!(
                    "{}_{}_seq",
                    table_name,
                    col.name
                ))
            {
                // is_called=false (fresh RESTART / setval(_, false))
                // means the NEXT value is last_value itself.
                let floor = if sd.is_called {
                    sd.last_value + 1
                } else {
                    sd.last_value
                };
                seq_floors.insert(i, floor);
            }
        }
        Ok(InsertSnapshots {
            clock,
            before_insert_triggers,
            after_insert_triggers,
            trigger_session_cfg,
            enum_label_lookup,
            set_variant_lookup,
            seq_floors,
        })
    }

    /// v7.39 (round 137) — INSERT through a view carrying an INSTEAD OF INSERT
    /// trigger: build a NEW row over the view's columns for each VALUES tuple and
    /// fire the trigger(s). The function body does the real write; no row is
    /// written to the view itself.
    fn exec_insert_view_instead_of(
        &mut self,
        stmt: &spg_sql::ast::InsertStatement,
        triggers_list: Vec<(
            spg_storage::FunctionDef,
            alloc::string::String,
            alloc::string::String,
        )>,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let view_def = self
            .active_catalog()
            .views_all()
            .get(&stmt.table)
            .cloned()
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        let body = match spg_sql::parser::parse_statement(&view_def.body) {
            Ok(spg_sql::ast::Statement::Select(b)) => b,
            _ => {
                return Err(EngineError::Unsupported(
                    "INSTEAD OF: view body is not a SELECT".into(),
                ));
            }
        };
        let cols = self.view_output_columns(&body, &view_def.columns)?;
        let col_schemas: Vec<ColumnSchema> = cols
            .iter()
            .map(|(n, t)| ColumnSchema::new(n.clone(), *t, true))
            .collect();
        let trigger_cfg: Option<String> = self
            .session_params
            .get("default_text_search_config")
            .cloned();
        let eval_ctx = self.ev_ctx(&[], None);
        let empty = Row::new(Vec::new());
        let mut deferred_all: Vec<triggers::DeferredEmbeddedStmt> = Vec::new();
        let mut returned: Vec<Row<'static>> = Vec::new();
        let mut affected = 0usize;
        for tuple in &stmt.rows {
            let mut new_vals: Vec<Value<'static>> = alloc::vec![Value::Null; col_schemas.len()];
            match &stmt.columns {
                Some(names) => {
                    for (name, expr) in names.iter().zip(tuple) {
                        let pos = col_schemas
                            .iter()
                            .position(|c| &c.name == name)
                            .ok_or_else(|| {
                                EngineError::Eval(EvalError::ColumnNotFound { name: name.clone() })
                            })?;
                        new_vals[pos] =
                            self.eval_expr_with_correlated(expr, &empty, &eval_ctx, cancel, None)?;
                    }
                }
                None => {
                    for (i, expr) in tuple.iter().enumerate() {
                        if let Some(slot) = new_vals.get_mut(i) {
                            *slot = self
                                .eval_expr_with_correlated(expr, &empty, &eval_ctx, cancel, None)?;
                        }
                    }
                }
            }
            // The row RETURNING projects over is the one the trigger returns
            // (chained through multiple INSTEAD OF triggers); RETURN NULL skips.
            let mut current = Row::new(new_vals);
            let mut skipped = false;
            for (fd, _when, tgname) in &triggers_list {
                let (outcome, deferred) = triggers::fire_row_trigger(
                    fd,
                    Some(current.clone()),
                    None,
                    &stmt.table,
                    &col_schemas,
                    &[],
                    trigger_cfg.as_deref(),
                    false,
                    &triggers::TgMeta {
                        op: "INSERT",
                        name: tgname,
                        level: "ROW",
                    },
                )
                .map_err(|e| EngineError::Storage(StorageError::Corrupt(alloc::format!("{e}"))))?;
                deferred_all.extend(deferred);
                match outcome {
                    triggers::TriggerOutcome::Row(r) => current = r,
                    triggers::TriggerOutcome::Skip => {
                        skipped = true;
                        break;
                    }
                }
            }
            if !skipped {
                returned.push(current);
                affected += 1;
            }
        }
        self.execute_deferred_trigger_stmts(deferred_all, cancel)?;
        if let Some(items) = &stmt.returning {
            return self.project_instead_of_returning(items, &stmt.table, &col_schemas, &returned);
        }
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: true,
        })
    }

    /// v7.39 (round 137, Phase 3) — project a RETURNING list over the rows an
    /// INSTEAD OF trigger returned (NEW for INSERT/UPDATE, OLD for DELETE). The
    /// items resolve against the view's column schema.
    fn project_instead_of_returning(
        &self,
        items: &[spg_sql::ast::SelectItem],
        view_name: &str,
        col_schemas: &[ColumnSchema],
        returned: &[Row<'static>],
    ) -> Result<QueryResult, EngineError> {
        let columns = self.derive_output_columns(items, col_schemas, view_name);
        let mut out: Vec<Row<'static>> = Vec::with_capacity(returned.len());
        for row in returned {
            out.push(self.project_row_simple(row, items, col_schemas, view_name)?);
        }
        Ok(QueryResult::Rows { columns, rows: out })
    }

    /// v7.39 (round 137/140) — materialise the OLD rows an UPDATE / DELETE
    /// targets: `SELECT * FROM <relation> [WHERE <pred>]`. Used both by the
    /// INSTEAD OF view path and by the DO ALSO rule wrapper (base tables).
    /// Returns the relation's output columns + the matching rows.
    fn scan_relation_rows(
        &self,
        view_name: &str,
        where_: &Option<spg_sql::ast::Expr>,
        cancel: CancelToken<'_>,
    ) -> Result<(Vec<ColumnSchema>, Vec<Row<'static>>), EngineError> {
        let sql = match where_ {
            Some(w) => alloc::format!("SELECT * FROM {view_name} WHERE {w}"),
            None => alloc::format!("SELECT * FROM {view_name}"),
        };
        let scan = match spg_sql::parser::parse_statement(&sql) {
            Ok(spg_sql::ast::Statement::Select(s)) => s,
            _ => {
                return Err(EngineError::Unsupported(
                    "INSTEAD OF: could not build the view scan".into(),
                ));
            }
        };
        match self.exec_select_cancel(&scan, cancel)? {
            QueryResult::Rows { columns, rows } => Ok((columns, rows)),
            _ => Err(EngineError::Unsupported(
                "INSTEAD OF: view scan did not return rows".into(),
            )),
        }
    }

    /// v7.39 (round 142) — a DELETE replaced by DO INSTEAD <command> rules: the
    /// matching rows are scanned only to bind OLD; the delete itself never runs.
    /// PG reports `DELETE 0` and rejects an outer RETURNING. The commands are
    /// full statements (they may themselves hit rules; the trigger-cascade
    /// recursion guard bounds a cycle).
    fn exec_delete_instead_command(
        &mut self,
        stmt: &spg_sql::ast::DeleteStatement,
        rules: Vec<spg_storage::RuleDef>,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        if stmt.returning.is_some() {
            return Err(crate::rules::rule_returning_error("DELETE", &stmt.table));
        }
        let (columns, old_rows) = self.scan_relation_rows(&stmt.table, &stmt.where_, cancel)?;
        let rows: Vec<(Option<Row<'static>>, Option<Row<'static>>)> =
            old_rows.into_iter().map(|r| (None, Some(r))).collect();
        self.run_also_rules(&rules, &columns, &rows, cancel)?;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
        })
    }

    /// v7.39 (round 142) — an UPDATE replaced by DO INSTEAD <command> rules:
    /// scan the matching rows for OLD, derive NEW from the SET assignments, and
    /// run the commands per row. The update itself never runs; PG reports
    /// `UPDATE 0` and rejects an outer RETURNING.
    fn exec_update_instead_command(
        &mut self,
        stmt: &spg_sql::ast::UpdateStatement,
        rules: Vec<spg_storage::RuleDef>,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        if stmt.returning.is_some() {
            return Err(crate::rules::rule_returning_error("UPDATE", &stmt.table));
        }
        let (columns, old_rows) = self.scan_relation_rows(&stmt.table, &stmt.where_, cancel)?;
        let mut pairs: Vec<(Option<Row<'static>>, Option<Row<'static>>)> =
            Vec::with_capacity(old_rows.len());
        {
            let ctx = self.ev_ctx(&columns, Some(&stmt.table));
            for old in &old_rows {
                let mut new_vals = old.values.clone();
                for (col, expr) in &stmt.assignments {
                    let pos = columns.iter().position(|c| &c.name == col).ok_or_else(|| {
                        EngineError::Eval(EvalError::ColumnNotFound { name: col.clone() })
                    })?;
                    new_vals[pos] =
                        self.eval_expr_with_correlated(expr, old, &ctx, cancel, None)?;
                }
                pairs.push((Some(Row::new(new_vals)), Some(old.clone())));
            }
        }
        self.run_also_rules(&rules, &columns, &pairs, cancel)?;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
        })
    }

    /// v7.39 (round 333, V59) — run a conditional `DO INSTEAD <command>`
    /// rule's action for the rows it claims, WITHOUT suppressing the
    /// original statement. `build_also_rule_stmts` applies each rule's own
    /// WHERE per row, so a row the condition misses contributes nothing
    /// here and is left to the (already narrowed) original.
    fn run_update_instead_actions(
        &mut self,
        stmt: &spg_sql::ast::UpdateStatement,
        rules: &[spg_storage::RuleDef],
        cancel: CancelToken<'_>,
    ) -> Result<(), EngineError> {
        let (columns, old_rows) = self.scan_relation_rows(&stmt.table, &stmt.where_, cancel)?;
        let mut pairs: Vec<(Option<Row<'static>>, Option<Row<'static>>)> =
            Vec::with_capacity(old_rows.len());
        {
            let ctx = self.ev_ctx(&columns, Some(&stmt.table));
            for old in &old_rows {
                let mut new_vals = old.values.clone();
                for (col, expr) in &stmt.assignments {
                    let pos = columns.iter().position(|c| &c.name == col).ok_or_else(|| {
                        EngineError::Eval(EvalError::ColumnNotFound { name: col.clone() })
                    })?;
                    new_vals[pos] =
                        self.eval_expr_with_correlated(expr, old, &ctx, cancel, None)?;
                }
                pairs.push((Some(Row::new(new_vals)), Some(old.clone())));
            }
        }
        self.run_also_rules(rules, &columns, &pairs, cancel)
    }

    /// v7.39 (round 140) — a DELETE carrying DO ALSO rules: capture the OLD rows
    /// first, run the real delete with rule rewrite suppressed, then fire each
    /// rule's command once per deleted row (OLD bound, no NEW).
    fn exec_delete_with_also(
        &mut self,
        stmt: &spg_sql::ast::DeleteStatement,
        also: Vec<spg_storage::RuleDef>,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let (columns, old_rows) = self.scan_relation_rows(&stmt.table, &stmt.where_, cancel)?;
        self.rule_rewrite_active = true;
        let res = self.exec_delete_cancel(stmt, cancel);
        self.rule_rewrite_active = false;
        let res = res?;
        let rows: Vec<(Option<Row<'static>>, Option<Row<'static>>)> =
            old_rows.into_iter().map(|r| (None, Some(r))).collect();
        self.run_also_rules(&also, &columns, &rows, cancel)?;
        Ok(res)
    }

    /// v7.39 (round 140) — an UPDATE carrying DO ALSO rules: capture OLD, derive
    /// NEW per row by applying the SET assignments (mirroring the INSTEAD OF view
    /// path), run the real update with rule rewrite suppressed, then fire each
    /// rule's command once per updated row (OLD + NEW bound).
    fn exec_update_with_also(
        &mut self,
        stmt: &spg_sql::ast::UpdateStatement,
        also: Vec<spg_storage::RuleDef>,
        view_check: Option<ViewCheck>,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let (columns, old_rows) = self.scan_relation_rows(&stmt.table, &stmt.where_, cancel)?;
        let mut pairs: Vec<(Option<Row<'static>>, Option<Row<'static>>)> =
            Vec::with_capacity(old_rows.len());
        {
            let ctx = self.ev_ctx(&columns, Some(&stmt.table));
            for old in &old_rows {
                let mut new_vals = old.values.clone();
                for (col, expr) in &stmt.assignments {
                    let pos = columns.iter().position(|c| &c.name == col).ok_or_else(|| {
                        EngineError::Eval(EvalError::ColumnNotFound { name: col.clone() })
                    })?;
                    new_vals[pos] =
                        self.eval_expr_with_correlated(expr, old, &ctx, cancel, None)?;
                }
                pairs.push((Some(Row::new(new_vals)), Some(old.clone())));
            }
        }
        self.rule_rewrite_active = true;
        let res = self.exec_update_cancel_inner(stmt, view_check, cancel);
        self.rule_rewrite_active = false;
        let res = res?;
        self.run_also_rules(&also, &columns, &pairs, cancel)?;
        Ok(res)
    }

    /// v7.39 (round 137, Phase 2) — UPDATE through a view with an INSTEAD OF
    /// UPDATE trigger: scan the view for matching OLD rows, derive NEW per row by
    /// applying the SET assignments, and fire the trigger(s). The function body
    /// does the real write.
    fn exec_update_view_instead_of(
        &mut self,
        stmt: &spg_sql::ast::UpdateStatement,
        triggers_list: Vec<(
            spg_storage::FunctionDef,
            alloc::string::String,
            alloc::string::String,
        )>,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let (columns, old_rows) = self.scan_relation_rows(&stmt.table, &stmt.where_, cancel)?;
        let trigger_cfg: Option<String> = self
            .session_params
            .get("default_text_search_config")
            .cloned();
        let mut deferred_all: Vec<triggers::DeferredEmbeddedStmt> = Vec::new();
        let mut returned: Vec<Row<'static>> = Vec::new();
        let mut affected = 0usize;
        {
            let ctx = self.ev_ctx(&columns, Some(&stmt.table));
            for old in &old_rows {
                // NEW = OLD with the SET assignments applied (assignments and
                // WHERE reference the view's columns).
                let mut new_vals = old.values.clone();
                for (col, expr) in &stmt.assignments {
                    let pos = columns.iter().position(|c| &c.name == col).ok_or_else(|| {
                        EngineError::Eval(EvalError::ColumnNotFound { name: col.clone() })
                    })?;
                    new_vals[pos] =
                        self.eval_expr_with_correlated(expr, old, &ctx, cancel, None)?;
                }
                let mut current = Row::new(new_vals);
                let mut skipped = false;
                for (fd, _when, tgname) in &triggers_list {
                    let (outcome, deferred) = triggers::fire_row_trigger(
                        fd,
                        Some(current.clone()),
                        Some(old),
                        &stmt.table,
                        &columns,
                        &[],
                        trigger_cfg.as_deref(),
                        false,
                        &triggers::TgMeta {
                            op: "UPDATE",
                            name: tgname,
                            level: "ROW",
                        },
                    )
                    .map_err(|e| {
                        EngineError::Storage(StorageError::Corrupt(alloc::format!("{e}")))
                    })?;
                    deferred_all.extend(deferred);
                    match outcome {
                        triggers::TriggerOutcome::Row(r) => current = r,
                        triggers::TriggerOutcome::Skip => {
                            skipped = true;
                            break;
                        }
                    }
                }
                if !skipped {
                    returned.push(current);
                    affected += 1;
                }
            }
        }
        self.execute_deferred_trigger_stmts(deferred_all, cancel)?;
        if let Some(items) = &stmt.returning {
            return self.project_instead_of_returning(items, &stmt.table, &columns, &returned);
        }
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: true,
        })
    }

    /// v7.39 (round 137, Phase 2) — DELETE through a view with an INSTEAD OF
    /// DELETE trigger: scan the view for matching OLD rows and fire the
    /// trigger(s) per row (OLD only).
    fn exec_delete_view_instead_of(
        &mut self,
        stmt: &spg_sql::ast::DeleteStatement,
        triggers_list: Vec<(
            spg_storage::FunctionDef,
            alloc::string::String,
            alloc::string::String,
        )>,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let (columns, old_rows) = self.scan_relation_rows(&stmt.table, &stmt.where_, cancel)?;
        let trigger_cfg: Option<String> = self
            .session_params
            .get("default_text_search_config")
            .cloned();
        let mut deferred_all: Vec<triggers::DeferredEmbeddedStmt> = Vec::new();
        let mut returned: Vec<Row<'static>> = Vec::new();
        let mut affected = 0usize;
        for old in &old_rows {
            // RETURNING on DELETE projects the OLD row the trigger returns.
            let mut current = old.clone();
            let mut skipped = false;
            for (fd, _when, tgname) in &triggers_list {
                let (outcome, deferred) = triggers::fire_row_trigger(
                    fd,
                    None,
                    Some(old),
                    &stmt.table,
                    &columns,
                    &[],
                    trigger_cfg.as_deref(),
                    false,
                    &triggers::TgMeta {
                        op: "DELETE",
                        name: tgname,
                        level: "ROW",
                    },
                )
                .map_err(|e| EngineError::Storage(StorageError::Corrupt(alloc::format!("{e}"))))?;
                deferred_all.extend(deferred);
                match outcome {
                    triggers::TriggerOutcome::Row(r) => current = r,
                    triggers::TriggerOutcome::Skip => {
                        skipped = true;
                        break;
                    }
                }
            }
            if !skipped {
                returned.push(current);
                affected += 1;
            }
        }
        self.execute_deferred_trigger_stmts(deferred_all, cancel)?;
        if let Some(items) = &stmt.returning {
            return self.project_instead_of_returning(items, &stmt.table, &columns, &returned);
        }
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: true,
        })
    }

    pub(crate) fn exec_insert(
        &mut self,
        stmt: InsertStatement,
    ) -> Result<QueryResult, EngineError> {
        // v7.39 (SQLSTATE fidelity) — qualify a NOT NULL rejection
        // with the relation, PG's full 23502 form (the storage layer
        // that raises it has no table name).
        let table = stmt.table.clone();
        self.exec_insert_inner(stmt)
            .map_err(|e| enrich_not_null(e, &table))
    }

    fn exec_insert_inner(&mut self, mut stmt: InsertStatement) -> Result<QueryResult, EngineError> {
        // v7.37.43-T4.4 — writable CTE outer body: materialise every
        // leading WITH clause first (running any modifying CTE
        // bodies against the active catalog so their writes land
        // in the same transaction as the outer INSERT), then
        // execute the INSERT against an enriched catalog where
        // the CTE alias resolves to the materialised RETURNING
        // rows. Sentori 0065's
        //   WITH new_scopes AS (INSERT … RETURNING id, name)
        //   INSERT INTO org_identity_scopes …
        //     SELECT … FROM orgs o JOIN new_scopes …
        // is the exact shape we exercise here.
        if !stmt.ctes.is_empty() {
            return self.exec_insert_with_ctes(stmt);
        }
        // v7.17.0 Phase 1.1 — pre-resolve any nextval / currval /
        // setval calls against the catalog before the row loop. We
        // walk each tuple expression and replace matching
        // FunctionCall nodes with their concrete Literal. This
        // keeps `literal_expr_to_value` free of `&mut self` and
        // lets multi-row INSERT VALUES (… nextval('seq') …)
        // mint a separate sequence value per row.
        for tuple in &mut stmt.rows {
            for cell in tuple.iter_mut() {
                self.resolve_sequence_calls_in_expr(cell)?;
            }
        }
        // v7.39 (round 139) — unconditional DO INSTEAD NOTHING rule blocks the
        // INSERT. PG rejects a RETURNING on a suppressed statement; otherwise
        // drop every source row so the normal path inserts nothing (the
        // `INSERT 0 0` tag stays byte-identical).
        if self.rule_blocks_statement(&stmt.table, "INSERT") {
            if stmt.returning.is_some() {
                return Err(crate::rules::rule_returning_error("INSERT", &stmt.table));
            }
            stmt.rows.clear();
            stmt.select_source = None;
        }
        // v7.39 (round 137) — INSTEAD OF INSERT trigger on the target view: fire
        // the trigger per row instead of the auto-updatable redirect. The
        // function body does the real write. Takes precedence over redirect.
        let iof_triggers = self.snapshot_row_triggers(&stmt.table, "INSERT", "INSTEAD OF");
        if !iof_triggers.is_empty() {
            return self.exec_insert_view_instead_of(&stmt, iof_triggers, CancelToken::none());
        }
        // v7.37.19 (19.13) — auto-updatable view redirect.
        // INSERT INTO simple_view (cols) VALUES (...) rewrites to
        // INSERT INTO base_table (cols) VALUES (...) when the view
        // is a simple-query shape (SELECT col1, col2, ... FROM base
        // with no joins / WHERE / GROUP BY / aggregates / etc).
        // v7.38 (P6.46) — INSERT into a WHERE-view goes straight to the base;
        // the view's WHERE only filters reads (no WITH CHECK OPTION yet).
        // v7.39 (round 132) — WITH CHECK OPTION: an inserted row must satisfy
        // the view's WHERE. Captured here (view name = the pre-redirect table),
        // enforced below once the full base rows are assembled.
        let mut view_check: Option<ViewCheck> = None;
        // v7.39 (round 267) — the view exists but is not auto-updatable.
        // Without this the write fell through to the base-table lookup and
        // reported `relation "<view>" does not exist`, which is not merely
        // the wrong wording — it denies the existence of an object the
        // catalog plainly has.
        if let Err(reason) = view_redirect_checked(self.active_catalog(), &stmt.table) {
            if self.active_catalog().has_view(&stmt.table) {
                return Err(view_not_updatable_error(&stmt.table, &INSERT_VERB, reason, false));
            }
        }
        if let Some(vr) = view_redirect_to_simple_base(self.active_catalog(), &stmt.table) {
            let ViewRedirect {
                base,
                where_at_base: _,
                col_map,
                computed,
                view_cols,
                check_chain,
            } = vr;
            // v7.39 (round 154) — a computed view column is never a write
            // target (PG: "cannot insert into column … of view …").
            if let Some(cols) = &stmt.columns {
                for c in cols {
                    if let Some(cc) = computed.iter().find(|cc| cc.name == *c) {
                        return Err(view_computed_col_write_err(
                            "insert into",
                            &cc.origin_col,
                            &cc.origin_view,
                        ));
                    }
                }
            }
            let written_opt = self
                .active_catalog()
                .views_all()
                .get(&stmt.table)
                .map_or(0, |v| v.check_option);
            // v7.39 (round 152) — arm the check whenever any level of the
            // chain carries its own option too (PG, r152 probe P6).
            if written_opt != 0 || check_chain.iter().any(|(_, _, o)| *o != 0) {
                view_check = Some(ViewCheck {
                    written_opt,
                    chain: check_chain,
                });
            }
            // v7.39 (round 133) — column-renamed view: translate the target
            // column list to base columns. An explicit `(a, b)` list maps each
            // name; a positional insert becomes an explicit base-column list in
            // view-column order (so a subset / reordered view lands correctly).
            // Round 154 — with computed columns present the order comes from
            // `view_cols`: the first N view columns cover N positional values
            // (PG takes a short tuple, probe P12) and hitting a computed slot
            // errors.
            if !col_map.is_empty() || !computed.is_empty() {
                let map: alloc::collections::BTreeMap<String, String> =
                    col_map.iter().cloned().collect();
                match &mut stmt.columns {
                    Some(cols) => {
                        for c in cols.iter_mut() {
                            if let Some(b) = map.get(c) {
                                *c = b.clone();
                            }
                        }
                    }
                    None if computed.is_empty() => {
                        stmt.columns = Some(col_map.iter().map(|(_, b)| b.clone()).collect());
                    }
                    None => {
                        let n = stmt.rows.iter().map(Vec::len).max().unwrap_or(0);
                        let mut cols: Vec<String> = Vec::with_capacity(n);
                        for (view_col, base_col) in view_cols.iter().take(n) {
                            match base_col {
                                Some(b) => cols.push(b.clone()),
                                None => {
                                    let cc = computed
                                        .iter()
                                        .find(|cc| cc.name == *view_col)
                                        .expect("computed slot has an entry");
                                    return Err(view_computed_col_write_err(
                                        "insert into",
                                        &cc.origin_col,
                                        &cc.origin_view,
                                    ));
                                }
                            }
                        }
                        stmt.columns = Some(cols);
                    }
                }
                // v7.39 (round 134) — rewrite RETURNING view cols → base cols.
                if let Some(ret) = &stmt.returning {
                    stmt.returning = Some(rewrite_view_returning_items(
                        ret, &col_map, &computed, &view_cols,
                    ));
                }
            }
            stmt.table = base;
        }
        // v7.37.6-B(sentori Epic 2 P0)— route INSERTs that target
        // a partition parent down to the matching child(`Range`
        // half-open hit, fall back to `Default`). Sequence-resolution
        // and INSERT…SELECT desugar above run first so the per-tuple
        // routing sees fully literal expressions.
        if crate::partition::is_partition_parent(self.active_catalog(), &stmt.table)
            && stmt.select_source.is_none()
        {
            return self.exec_insert_route_partition_parent(stmt);
        }
        // v7.13.0 — `INSERT INTO t [(cols)] SELECT …` (mailrs
        // round-5 G4). Execute the inner SELECT first, then route
        // back through the regular VALUES code path with the
        // materialised rows.
        if let Some(select) = stmt.select_source.clone() {
            let select_result = self.exec_select_cancel(&select, CancelToken::none())?;
            let rows = match select_result {
                QueryResult::Rows { rows, .. } => rows,
                other => {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "INSERT … SELECT: inner statement produced {other:?} instead of a row set"
                    )));
                }
            };
            let mut materialised: Vec<Vec<Expr>> = Vec::with_capacity(rows.len());
            for row in rows {
                let mut tuple: Vec<Expr> = Vec::with_capacity(row.values.len());
                for v in row.values {
                    tuple.push(value_to_literal_expr_permissive(v)?);
                }
                materialised.push(tuple);
            }
            let recurse = InsertStatement {
                ctes: Vec::new(),
                table: stmt.table,
                alias: stmt.alias,
                columns: stmt.columns,
                rows: materialised,
                select_source: None,
                on_conflict: stmt.on_conflict,
                returning: stmt.returning,
                overriding: stmt.overriding,
                // The materialised rows are still the IGNORE statement's.
                mysql_ignore: stmt.mysql_ignore,
            };
            return self.exec_insert(recurse);
        }
        // Snapshot everything the row loop needs from the catalog
        // before the mutable borrow below shadows it (clock, triggers,
        // enum/set variant lookups, sequence floors).
        let InsertSnapshots {
            clock,
            before_insert_triggers,
            after_insert_triggers,
            trigger_session_cfg,
            enum_label_lookup,
            set_variant_lookup,
            seq_floors,
        } = self.prepare_insert_snapshots(&stmt.table)?;
        // v7.39 (read01 round 55) — the catalog for user-named casts in VALUES.
        // Taken BEFORE the &mut borrow below (Catalog::clone is an Arc bump).
        let cat_for_insert = self.active_catalog().clone();
        let insert_mysql = self.backslash_escapes;
        // v7.39 (round 470) — read before the &mut borrow below, same
        // reason `cat_for_insert` is.
        let insert_non_strict = insert_mysql && !self.mysql_strict;
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        // v3.1.5: clone the columns vector only (not the whole
        // TableSchema — saves one String alloc for the table name).
        // We need an owned snapshot because we'll call `table.insert`
        // (mutable borrow on `table`) inside the row loop while
        // reading schema fields.
        let column_meta: Vec<ColumnSchema> = table.schema().columns.clone();
        let schema_cols_len = column_meta.len();
        let tuple_pos = build_tuple_pos(&stmt.columns, &column_meta, &stmt.table)?;
        let expected_tuple_len = stmt.columns.as_ref().map_or(schema_cols_len, Vec::len);
        // v7.6.2 — snapshot this table's FK list before the
        // mutable-borrow window so we can run parent lookups
        // against the immutable catalog after parsing. Empty vec is
        // the no-FK fast path; clone cost is O(fks * arity) which
        // is < 100 ns for typical schemas.
        let fks = table.schema().foreign_keys.clone();
        // Stage 1 — parse + AUTO_INC + coerce all rows under the
        // (immutable) table borrow.
        let overriding = stmt.overriding;
        let mut first_auto: Option<i64> = None;
        let mut all_values = parse_insert_rows(
            table,
            Some(&cat_for_insert),
            stmt.rows,
            &column_meta,
            &tuple_pos,
            expected_tuple_len,
            clock,
            &seq_floors,
            &enum_label_lookup,
            &set_variant_lookup,
            overriding,
            &mut first_auto,
            insert_mysql,
            // v7.39 (round 470) — `INSERT IGNORE` bends values, and so does
            // a non-strict `sql_mode`. Same conversion, different trigger.
            insert_mysql && stmt.mysql_ignore,
            insert_non_strict,
        )?;
        // Stage 2 — FK enforcement on the immutable catalog.
        // Non-lexical lifetimes release the mutable borrow on
        // `table` here since stage 1 was the last use. The
        // parent-table lookup runs before any row is committed.
        let uniqueness = table.schema().uniqueness_constraints.clone();
        // v7.39 (round 210) — EXCLUDE constraints run alongside uniqueness on
        // the post-ON-CONFLICT row set.
        let exclusions = table.schema().exclusion_constraints.clone();
        let _ = table;
        // v7.39 (round 347, M2) — MySQL's LAST_INSERT_ID() reports the
        // FIRST value this statement generated, and is left ALONE by a
        // statement that generated none (measured on MariaDB 11: an
        // explicit id, an UPDATE and an insert into a table with no
        // AUTO_INCREMENT column all leave it unchanged). Set once the
        // borrow on `table` is released.
        if let Some(v) = first_auto {
            self.last_insert_id
                .store(v, core::sync::atomic::Ordering::Relaxed);
        }
        // v7.37.7(sentori Epic 3 P1)— stored generated-column
        // evaluation runs AFTER ordinary INSERT values are coerced
        // (so the expression sees the materialised row)but BEFORE
        // FK / CHECK / UNIQUE so those guards reason about the
        // computed value the way they would for any literal column.
        apply_generated_stored_columns(&column_meta, &mut all_values)?;
        // v7.39 (round 141) — conditional DO INSTEAD NOTHING: drop the proposed
        // rows a rule's WHERE (over NEW = the post-default row) blocks, before any
        // constraint check, matching PG's query-rewrite ordering.
        let cond_ins = self.conditional_instead_nothing_rules(&stmt.table, "INSERT");
        // v7.39 (round 333, V59) — a conditional INSTEAD rule that carries a
        // COMMAND runs it for the rows it claims, and those rows are then
        // dropped from the original INSERT by the same filter that serves
        // the NOTHING form. Measured on PG 18.4: with `WHERE new.id < 0 DO
        // INSTEAD INSERT INTO q59 …`, inserting (1, -1, 2, -2) answers
        // `INSERT 0 2`, leaves 1 and 2 in the table and -1 / -2 in q59.
        {
            let cond_cmd: Vec<spg_storage::RuleDef> = cond_ins
                .iter()
                .filter(|r| !r.commands.is_empty())
                .cloned()
                .collect();
            if !cond_cmd.is_empty() {
                if stmt.returning.is_some() {
                    return Err(crate::rules::rule_returning_error("INSERT", &stmt.table));
                }
                // `build_also_rule_stmts` applies each rule's own WHERE per
                // row, so handing it every proposed row fires only the
                // claimed ones.
                let pairs: Vec<(Option<Row<'static>>, Option<Row<'static>>)> = all_values
                    .iter()
                    .map(|v| (Some(Row::new(v.clone())), None))
                    .collect();
                self.run_also_rules(&cond_cmd, &column_meta, &pairs, CancelToken::none())?;
            }
        }
        if !cond_ins.is_empty() {
            let mut kept: Vec<Vec<Value<'static>>> = Vec::with_capacity(all_values.len());
            for row in all_values {
                let new_row = Row::new(row.clone());
                let mut blocked = false;
                for r in &cond_ins {
                    if triggers::trigger_when_holds(
                        &r.when_condition,
                        Some(&new_row),
                        None,
                        &column_meta,
                    )? {
                        blocked = true;
                        break;
                    }
                }
                if !blocked {
                    kept.push(row);
                }
            }
            all_values = kept;
        }
        // v7.39 (round 142) — DO INSTEAD <command>: the original INSERT is
        // suppressed entirely (no constraint checks against this table — nothing
        // lands in it); the command runs once per proposed row with NEW = the
        // post-default tuple. PG reports the SOURCE row count in the tag
        // (`INSERT 0 n`) and rejects an outer RETURNING.
        let instead_ins: Vec<spg_storage::RuleDef> = self
            .instead_command_rules(&stmt.table, "INSERT")
            .into_iter()
            // v7.39 (round 333, V59) — only the UNCONDITIONAL ones replace
            // the statement; the conditional ones were handled above.
            .filter(|r| r.when_condition.is_empty())
            .collect();
        if !instead_ins.is_empty() {
            if stmt.returning.is_some() {
                return Err(crate::rules::rule_returning_error("INSERT", &stmt.table));
            }
            let n = all_values.len();
            let rows: Vec<(Option<Row<'static>>, Option<Row<'static>>)> = all_values
                .into_iter()
                .map(|v| (Some(Row::new(v)), None))
                .collect();
            self.run_also_rules(&instead_ins, &column_meta, &rows, CancelToken::none())?;
            return Ok(QueryResult::CommandOk {
                affected: n,
                modified_catalog: !self.in_transaction(),
            });
        }
        // v7.39 (read01 round 117) — NOT NULL, checked pre-write over the fully
        // assembled rows so a violation aborts the whole statement (no partial
        // rows) and carries PG's `DETAIL: Failing row contains (...)`. Runs
        // before FK / CHECK, matching PG's not-null-first ordering.
        enforce_not_null(self.active_catalog(), &stmt.table, &all_values)?;
        if !fks.is_empty() {
            // v7.39 (round 288) — same split on the INSERT path.
            let now = self.immediate_fks(&fks);
            if !now.is_empty() {
                enforce_fk_inserts(self.active_catalog(), &stmt.table, &now, &all_values)?;
            }
        }
        // v7.13.0 — CHECK constraint enforcement (mailrs round-5 G3).
        enforce_check_constraints(self.active_catalog(), &stmt.table, &all_values)?;
        // v7.39 (RLS) Phase 2 — INSERT WITH CHECK: a policy-subject session's
        // new rows must satisfy the combined WITH CHECK predicate.
        self.rls_check_new_rows(
            &stmt.table,
            spg_storage::PolicyCmd::Insert,
            &column_meta,
            &all_values,
        )?;
        // v7.39 (round 132) — WITH CHECK OPTION on the assembled base rows.
        if let Some(check) = &view_check {
            self.enforce_view_check(check, &all_values, &column_meta, &stmt.table)?;
        }
        // NOTE (mailrs embed round-12): UNIQUE / PRIMARY KEY and
        // UNIQUE INDEX enforcement moved BELOW the ON CONFLICT
        // resolution pass. Running them first made every
        // `ON CONFLICT … DO UPDATE` upsert fail with a uniqueness
        // violation before the conflict handler could route the row
        // to an UPDATE — PG resolves the conflict action first and
        // only errors on rows no arbiter matched.
        // v7.9.8 / v7.9.9 — ON CONFLICT handling.
        //   - `DO NOTHING` filters `all_values` to non-conflicting
        //     rows + drops within-batch duplicates.
        //   - `DO UPDATE SET …` ALSO filters, but for each
        //     conflicting row it queues an UPDATE on the existing
        //     row using the incoming row's values as `EXCLUDED.*`.
        let (pending_updates, skipped_count) = match &stmt.on_conflict {
            Some(clause) => {
                let (kept, pending, skipped) =
                    self.resolve_insert_on_conflict(
                        &stmt.table,
                        stmt.alias.as_deref(),
                        clause,
                        all_values,
                    )?;
                all_values = kept;
                (pending, skipped)
            }
            None => (Vec::new(), 0usize),
        };
        // v7.9.19 — composite UNIQUE / PRIMARY KEY enforcement.
        // v7.9.29 — CREATE UNIQUE INDEX [WHERE pred] enforcement.
        // Both run on the post-ON-CONFLICT row set: conflicting rows
        // already left `all_values` (DO NOTHING drop / DO UPDATE
        // reroute), so what remains must be genuinely unique.
        let mysql = self.backslash_escapes;
        // v7.39 (round 427) — which MySQL upsert spelling produced the
        // DO UPDATE clause. Both lower onto the same AST; REPLACE is the one
        // whose assignment list is EMPTY (round 419's "take the incoming
        // row"). They charge an unchanged row differently, so the kind has
        // to reach `insert_parsed_rows`.
        let mysql_upsert_kind = if mysql {
            match stmt.on_conflict.as_ref().map(|c| &c.action) {
                Some(spg_sql::ast::OnConflictAction::Update { assignments, .. }) => {
                    Some(if assignments.is_empty() {
                        MysqlUpsertCount::Replace
                    } else {
                        MysqlUpsertCount::OnDuplicate
                    })
                }
                _ => None,
            }
        } else {
            None
        };
        enforce_uniqueness_inserts(
            self.active_catalog(),
            &stmt.table,
            &uniqueness,
            &all_values,
            mysql,
        )?;
        enforce_unique_index_inserts(self.active_catalog(), &stmt.table, &all_values, mysql)?;
        crate::constraints::enforce_exclusion_inserts(
            self.active_catalog(),
            &stmt.table,
            &exclusions,
            &all_values,
        )?;
        // v7.39 (round 140) — DO ALSO INSERT rules: capture the post-image rows
        // (RETURNING is forced on so defaults / sequences are reflected) and run
        // each rule's command per inserted row after the write completes.
        let also_ins = self.also_rules(&stmt.table, "INSERT");
        // v7.37.15 Phase C — pre-fetch the writer version BEFORE
        // the table mut borrow; otherwise the call below would
        // need &mut self while `table` already holds it. Shared
        // across every row this statement inserts (atomic commit
        // at the tx boundary).
        let xmin_for_stmt = self.writer_version_for_current_stmt();
        // v7.37.15 (Phase C.3, step 4b) — in-place kill switch for the
        // ON CONFLICT DO UPDATE tombstone+insert path, read before the
        // table mut borrow and threaded into insert_parsed_rows.
        let inplace_for_stmt = self.mvcc_inplace();
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        // Stage 3 — insert the surviving rows + fire row triggers under
        // a fresh mutable borrow, then apply queued ON CONFLICT updates.
        let (returning_rows, deferred_embedded, affected, oc_pairs, oc_old_images) =
            insert_parsed_rows(
                table,
                xmin_for_stmt,
                // v7.39 (round 427) — MySQL's upsert row accounting. REPLACE
                // and ON DUPLICATE KEY UPDATE both lower onto the same
                // DO UPDATE clause; the parser distinguishes them by the
                // assignment list, which REPLACE leaves EMPTY ("take the
                // incoming row", round 419). They count an unchanged row
                // differently, so the distinction has to survive to here.
                mysql_upsert_kind,
                inplace_for_stmt,
                all_values,
                pending_updates,
                &before_insert_triggers,
                &after_insert_triggers,
                &column_meta,
                &stmt.table,
                trigger_session_cfg.as_deref(),
                stmt.returning.is_some() || !also_ins.is_empty(),
            )?;
        let _ = skipped_count;
        // v7.12.7 — drop the table mut borrow and drain any
        // trigger-emitted embedded SQL queued during this INSERT.
        // The borrow has to release first because each deferred
        // stmt may UPDATE / INSERT / DELETE the same (or another)
        // table — including, in principle, this one.
        let _ = table;
        // v7.37.17 (E4 r3) — persist ON CONFLICT update pairs on the tx
        // (after the table borrow drops).
        self.record_update_pairs(&stmt.table, oc_pairs);
        self.execute_deferred_trigger_stmts(deferred_embedded, CancelToken::none())?;
        // v7.39 (round 140) — fire DO ALSO INSERT rules per inserted row. NEW is
        // the post-image (defaults / sequences applied); there is no OLD.
        if !also_ins.is_empty() {
            let rows: Vec<(Option<Row<'static>>, Option<Row<'static>>)> = returning_rows
                .iter()
                .map(|v| (Some(Row::new(v.clone())), None))
                .collect();
            self.run_also_rules(&also_ins, &column_meta, &rows, CancelToken::none())?;
        }
        // v7.9.4/v7.9.9 — RETURNING streams the rows that ended
        // up in the table after this statement (insert or
        // post-update on conflict).
        if let Some(items) = &stmt.returning {
            // v7.39 (round 126/129) — INSERT: NEW = the inserted-or-updated row
            // (= default). OLD is all-NULL for a plain insert and the pre-update
            // conflicting row for an ON CONFLICT DO UPDATE (r129 threads the
            // per-row OLD image out of insert_parsed_rows).
            let new_for_returning = returning_rows.clone();
            return self.build_returning_rows_old_new(
                &stmt.table,
                stmt.alias.as_deref(),
                items,
                returning_rows,
                Some(oc_old_images),
                Some(new_for_returning),
            );
        }
        // v6.2.1 — auto-analyze: track per-table modified-row
        // counter so the background sweep can decide when to
        // re-ANALYZE. Cheap path on the autocommit-wrap hot loop
        // — one BTreeMap entry update per INSERT batch.
        if !self.in_transaction() && affected > 0 {
            self.statistics
                .record_modifications(&stmt.table, affected as u64);
        }
        Ok(QueryResult::CommandOk {
            affected,
            modified_catalog: !self.in_transaction(),
        })
    }

    /// (ON CONFLICT) Resolve a `DO NOTHING` / `DO UPDATE` clause against
    /// the post-parse row set: drop or reroute conflicting rows, queue
    /// updates for the existing rows they collide with, and return the
    /// rows that survive to be inserted plus those queued updates.
    #[allow(clippy::type_complexity)]
    fn resolve_insert_on_conflict(
        &self,
        table_name: &str,
        alias: Option<&str>,
        clause: &spg_sql::ast::OnConflictClause,
        all_values: Vec<Vec<Value<'static>>>,
    ) -> Result<
        (
            Vec<Vec<Value<'static>>>,
            // (target_pos, new_row, old_row) — the pre-update row is the OLD
            // image for `RETURNING OLD.*` on the DO UPDATE path (v7.39 r129).
            Vec<(usize, Vec<Value<'static>>, Vec<Value<'static>>)>,
            usize,
        ),
        EngineError,
    > {
        let mut pending_updates: Vec<(usize, Vec<Value<'static>>, Vec<Value<'static>>)> =
            Vec::new();
        let mut skipped_count = 0usize;
        // v7.37.17 (17.6 siblings) — `ON CONFLICT ON CONSTRAINT
        // <name>` resolves the name to the constraint's columns via
        // the same synthetic naming convention pg_constraint
        // synthesises ({t}_pkey for the primary key, {t}_uniq{i}
        // for the i-th non-PK unique).
        let named_columns: Vec<String> = if let Some(cname) = &clause.constraint_name {
            let table = self.active_catalog().get(table_name).ok_or_else(|| {
                spg_storage::StorageError::TableNotFound {
                    name: alloc::string::String::from(table_name),
                }
            })?;
            let schema = table.schema();
            let mut found: Option<Vec<String>> = None;
            for uc in &schema.uniqueness_constraints {
                let synth = crate::system_catalog::pg_unique_conname(table, uc, table_name);
                if synth.eq_ignore_ascii_case(cname) {
                    found = Some(
                        uc.columns
                            .iter()
                            .filter_map(|&pos| schema.columns.get(pos).map(|c| c.name.clone()))
                            .collect(),
                    );
                    break;
                }
            }
            found.ok_or_else(|| {
                EngineError::Unsupported(alloc::format!(
                    "ON CONFLICT ON CONSTRAINT: no unique or primary key \
                     constraint named {cname:?} on table {table_name:?}"
                ))
            })?
        } else {
            Vec::new()
        };
        let target_cols: &[String] = if named_columns.is_empty() {
            clause.target_columns.as_slice()
        } else {
            named_columns.as_slice()
        };
        // v7.39 (round 240) — DO UPDATE has one row to find, so PG requires
        // an explicit conflict target for it (42601).
        if target_cols.is_empty()
            && !clause.mysql_lowered
            && matches!(clause.action, spg_sql::ast::OnConflictAction::Update { .. })
        {
            return Err(EngineError::Unsupported(
                "ON CONFLICT DO UPDATE requires inference specification or constraint name".into(),
            ));
        }
        // The arbiter column sets this clause watches: exactly one for an
        // explicit target (validated against the table's unique
        // constraints), every unique constraint and index for the bare
        // form — which used to pick ONE, so a DO NOTHING let a conflict on
        // any other constraint escalate to a duplicate-key error.
        let arbiters = crate::constraints::on_conflict_arbiters(
            self.active_catalog(),
            table_name,
            target_cols,
            clause.constraint_name.is_some(),
        )?;
        let mut kept: Vec<Vec<Value<'static>>> = Vec::with_capacity(all_values.len());
        // Per-arbiter batch-local keys (a bare clause tracks several sets).
        let mut seen_keys: Vec<Vec<Vec<Value<'static>>>> = alloc::vec![Vec::new(); arbiters.len()];
        for values in all_values {
            // SQL spec: NULL in any conflict column means "no conflict
            // possible" (NULL ≠ NULL for uniqueness) — UNLESS the
            // constraint says NULLS NOT DISTINCT (v7.29; mailrs
            // migrate-013 replays its seed row ('super', NULL) under
            // exactly that declaration).
            let mut collides_with_table = false;
            let mut collides_with_batch = false;
            // Which arbiter hit — the DO UPDATE row lookup keys off it (a
            // MySQL-lowered bare clause can have several).
            let mut hit_arbiter = 0usize;
            for (ai, (cols, nnd)) in arbiters.iter().enumerate() {
                let kt: Vec<&Value> = cols.iter().map(|&c| &values[c]).collect();
                let has_null = !nnd && kt.iter().any(|v| matches!(v, Value::Null));
                if has_null {
                    continue;
                }
                if on_conflict_keys_exist(self.active_catalog(), table_name, cols, &kt) {
                    if !collides_with_table && !collides_with_batch {
                        hit_arbiter = ai;
                    }
                    collides_with_table = true;
                }
                let kt_owned: Vec<Value<'static>> = kt.iter().map(|v| (*v).clone()).collect();
                if seen_keys[ai].iter().any(|k| k == &kt_owned) {
                    if !collides_with_table && !collides_with_batch {
                        hit_arbiter = ai;
                    }
                    collides_with_batch = true;
                }
            }
            let conflict_cols = &arbiters
                .get(hit_arbiter)
                .map(|(c, _)| c.clone())
                .unwrap_or_default();
            let key_tuple: Vec<&Value> = conflict_cols.iter().map(|&c| &values[c]).collect();
            let key_tuple_owned: Vec<Value<'static>> =
                key_tuple.iter().map(|v| (*v).clone()).collect();
            let collides = collides_with_table || collides_with_batch;
            match (&clause.action, collides) {
                (_, false) => {
                    for (ai, (cols, nnd)) in arbiters.iter().enumerate() {
                        let kt: Vec<Value<'static>> =
                            cols.iter().map(|&c| values[c].clone()).collect();
                        if *nnd || !kt.iter().any(|v| matches!(v, Value::Null)) {
                            seen_keys[ai].push(kt);
                        }
                    }
                    kept.push(values);
                }
                (spg_sql::ast::OnConflictAction::Nothing, true) => {
                    skipped_count += 1;
                }
                (
                    spg_sql::ast::OnConflictAction::Update {
                        assignments,
                        where_,
                    },
                    true,
                ) => {
                    // v7.38 (read01 sweep) — PG refuses to touch a row twice in
                    // one command: if this conflict key already appeared in the
                    // batch (either inserted by an earlier row or updated by an
                    // earlier DO UPDATE), it is a cardinality violation
                    // ("ON CONFLICT DO UPDATE command cannot affect row a second
                    // time").
                    if collides_with_batch {
                        // v7.39 (round 240) — PG's own wording (21000); the
                        // shared CardinalityViolation display is the scalar
                        // subquery's message and reads as an internal leak
                        // here.
                        return Err(EngineError::Unsupported(
                            "ON CONFLICT DO UPDATE command cannot affect row a second time".into(),
                        ));
                    }
                    // Claim this key so a later duplicate in the same batch is
                    // caught above, on the arbiter that produced the hit.
                    seen_keys[hit_arbiter].push(key_tuple_owned);
                    let target_pos = lookup_row_position_by_keys(
                        self.active_catalog(),
                        table_name,
                        conflict_cols,
                        &key_tuple,
                    )
                    .ok_or_else(|| {
                        EngineError::Unsupported(
                            "ON CONFLICT DO UPDATE: conflict detected but row \
                                 position could not be resolved (cold-tier row?)"
                                .into(),
                        )
                    })?;
                    // Snapshot the pre-update row: PG's `RETURNING OLD.*` on a
                    // DO UPDATE returns the conflicting row as it was BEFORE the
                    // update applied.
                    let old_row_vals: Vec<Value<'static>> = self
                        .active_catalog()
                        .get(table_name)
                        .and_then(|t| t.rows().get(target_pos).map(|r| r.values.clone()))
                        .unwrap_or_default();
                    let updated = apply_on_conflict_assignments(
                        self.active_catalog(),
                        table_name,
                        alias,
                        target_pos,
                        &values,
                        assignments,
                        where_.as_ref(),
                    )?;
                    if let Some(new_row) = updated {
                        pending_updates.push((target_pos, new_row, old_row_vals));
                    } else {
                        skipped_count += 1;
                    }
                }
            }
        }
        Ok((kept, pending_updates, skipped_count))
    }

    /// v7.37.6-B(sentori Epic 2 P0)— route an INSERT whose target
    /// is a `PartitionRole::Parent` table down to the matching
    /// children. Each tuple's partition-key value picks one child
    /// (first hit on a `Range` child, else the `Default` child if
    /// any). Tuples land in per-child buckets that are re-issued
    /// through `exec_insert` against the child's name; the parent
    /// table itself never receives rows. Returns the summed
    /// `Modified { affected }` count across all children.
    ///
    /// v7.37.6-B contract caveats:
    ///   * ON CONFLICT / RETURNING on a partition-parent insert are
    ///     not supported(parser still allows them, but the engine
    ///     surfaces `Unsupported` here). PG ≤ 11 had the same
    ///     restriction; sentori doesn't rely on either against the
    ///     `events_partitioned` / `spans` tables.
    ///   * Column lists are honoured: the partition-key column must
    ///     be present in the INSERT column list(or omitted via the
    ///     "full schema order" form), otherwise routing has nothing
    ///     to read.
    pub(crate) fn exec_insert_route_partition_parent(
        &mut self,
        stmt: InsertStatement,
    ) -> Result<QueryResult, EngineError> {
        use spg_storage::PartitionRole;
        if stmt.on_conflict.is_some() {
            return Err(EngineError::Unsupported(alloc::format!(
                "INSERT INTO partition parent {:?} … ON CONFLICT: not supported \
                 at v7.37.6-B(route through the child explicitly)",
                stmt.table
            )));
        }
        if stmt.returning.is_some() {
            return Err(EngineError::Unsupported(alloc::format!(
                "INSERT INTO partition parent {:?} … RETURNING: not supported \
                 at v7.37.6-B(route through the child explicitly)",
                stmt.table
            )));
        }
        // Pull what we need from the parent + child catalog state
        // before any mutating call(per-child exec_insert below
        // takes &mut self).
        let parent_name = stmt.table.clone();
        let (parent_columns, key_position, parent_kind): (
            Vec<spg_storage::ColumnSchema>,
            usize,
            spg_storage::PartitionKind,
        ) = {
            let parent = self.active_catalog().get(&parent_name).ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: parent_name.clone(),
                })
            })?;
            let cols = parent.schema().columns.clone();
            let (key_position, kind) = match &parent.schema().partition_role {
                Some(PartitionRole::Parent {
                    key_column_positions,
                    kind,
                    ..
                }) => (
                    *key_column_positions.first().ok_or_else(|| {
                        EngineError::Unsupported(alloc::format!(
                            "partition parent {parent_name:?} has empty key column list"
                        ))
                    })?,
                    *kind,
                ),
                _ => {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "INSERT routing: {parent_name:?} is not a partition parent"
                    )));
                }
            };
            (cols, key_position, kind)
        };
        // Map the user's INSERT column list back to a parent-schema
        // position. Missing list means "every column in schema
        // order" — common shape; sentori's
        // INSERT INTO events_partitioned (project_id, received_at,
        //   payload) VALUES (…) takes the explicit branch.
        let tuple_key_index: usize = match &stmt.columns {
            None => key_position,
            Some(col_list) => {
                let key_col_name = parent_columns[key_position].name.as_str();
                col_list
                    .iter()
                    .position(|c| c.as_str().eq_ignore_ascii_case(key_col_name))
                    .ok_or_else(|| {
                        EngineError::Unsupported(alloc::format!(
                            "INSERT INTO {parent_name:?}: partition key column \
                             {key_col_name:?} is not in the INSERT column list, \
                             so the engine cannot route the tuple to a child"
                        ))
                    })?
            }
        };
        // Snapshot every child(name, role)so the per-row routing
        // loop only touches an immutable view of the catalog.
        let children = crate::partition::children_of_parent(self.active_catalog(), &parent_name);
        let mut range_children: Vec<(
            String,
            spg_storage::PartitionBound,
            spg_storage::PartitionBound,
        )> = Vec::new();
        // v7.37.16 (16.1) — LIST children: (name, accepted values).
        let mut list_children: Vec<(String, Vec<spg_storage::PartitionBound>)> = Vec::new();
        // v7.37.16 (16.2) — HASH children: (name, modulus, remainder).
        let mut hash_children: Vec<(String, u32, u32)> = Vec::new();
        let mut default_child: Option<String> = None;
        for child_name in &children {
            let Some(child) = self.active_catalog().get(child_name) else {
                continue;
            };
            match &child.schema().partition_role {
                Some(PartitionRole::Range { lower, upper, .. }) => {
                    range_children.push((child_name.clone(), lower.clone(), upper.clone()));
                }
                Some(PartitionRole::List { values, .. }) => {
                    list_children.push((child_name.clone(), values.clone()));
                }
                Some(PartitionRole::Hash {
                    modulus, remainder, ..
                }) => {
                    hash_children.push((child_name.clone(), *modulus, *remainder));
                }
                Some(PartitionRole::Default { .. }) => {
                    default_child = Some(child_name.clone());
                }
                _ => {}
            }
        }
        // Bucket tuples by destination child; preserve original row
        // order within each bucket so error messages reference the
        // tuple the user actually wrote.
        let mut buckets: alloc::collections::BTreeMap<String, Vec<Vec<Expr>>> =
            alloc::collections::BTreeMap::new();
        for tuple in stmt.rows {
            if tuple.len() <= tuple_key_index {
                return Err(EngineError::Unsupported(alloc::format!(
                    "INSERT INTO {parent_name:?}: tuple has {} expressions, \
                     partition key index is {tuple_key_index} — column list / \
                     value list shape mismatch",
                    tuple.len()
                )));
            }
            let key_expr = tuple[tuple_key_index].clone();
            let key_value = literal_expr_to_value(key_expr)?;
            if matches!(key_value, Value::Null) {
                return Err(EngineError::Unsupported(alloc::format!(
                    "INSERT INTO {parent_name:?}: partition key value is \
                     NULL, but the partition key is NOT NULL"
                )));
            }
            let target = match parent_kind {
                spg_storage::PartitionKind::Range => {
                    // v7.37 D.45 — route by the key's actual type (int-family /
                    // DATE / TIMESTAMPTZ / TEXT), not a forced TIMESTAMPTZ coercion.
                    let key_bound =
                        crate::partition::value_to_bound(&key_value).ok_or_else(|| {
                            EngineError::Unsupported(alloc::format!(
                                "INSERT INTO {parent_name:?}: partition key value \
                             {key_value:?} is not a supported RANGE key type"
                            ))
                        })?;
                    range_children
                        .iter()
                        .find(|(_, lo, hi)| crate::partition::value_in_range(&key_bound, lo, hi))
                        .map(|(name, _, _)| name.clone())
                        .or_else(|| default_child.clone())
                        .ok_or_else(|| {
                            EngineError::Unsupported(alloc::format!(
                                "no partition of relation {parent_name:?} found for \
                                 row with partition key value (no Range child matches \
                                 and there is no DEFAULT partition)"
                            ))
                        })?
                }
                // v7.37.16 (16.1) — LIST routing: pick the first
                // child whose values contains the key.
                spg_storage::PartitionKind::List => list_children
                    .iter()
                    .find(|(_, values)| values.iter().any(|b| b.equals_value(&key_value)))
                    .map(|(name, _)| name.clone())
                    .or_else(|| default_child.clone())
                    .ok_or_else(|| {
                        EngineError::Unsupported(alloc::format!(
                            "no partition of relation {parent_name:?} found for \
                             row with partition key value (no LIST child matches \
                             and there is no DEFAULT partition)"
                        ))
                    })?,
                // v7.37.16 (16.2) — HASH routing: hash(key) mod
                // modulus picks the child. All HASH siblings under
                // a parent share a single modulus (DDL gate).
                spg_storage::PartitionKind::Hash => {
                    let h = crate::partition::pg_compatible_hash(&key_value);
                    hash_children
                        .iter()
                        .find(|(_, m, r)| h.rem_euclid(u64::from(*m)) == u64::from(*r))
                        .map(|(name, _, _)| name.clone())
                        .or_else(|| default_child.clone())
                        .ok_or_else(|| {
                            EngineError::Unsupported(alloc::format!(
                                "no partition of relation {parent_name:?} found for \
                                 row with partition key value (no HASH child matches \
                                 and there is no DEFAULT partition)"
                            ))
                        })?
                }
            };
            buckets.entry(target).or_default().push(tuple);
        }
        let mut total_affected: usize = 0;
        for (child_name, rows) in buckets {
            let child_stmt = InsertStatement {
                ctes: Vec::new(),
                table: child_name,
                alias: stmt.alias.clone(),
                columns: stmt.columns.clone(),
                rows,
                select_source: None,
                on_conflict: None,
                returning: None,
                overriding: stmt.overriding,
                // Routing a row to its partition keeps the statement's IGNORE.
                mysql_ignore: stmt.mysql_ignore,
            };
            let result = self.exec_insert(child_stmt)?;
            if let QueryResult::CommandOk { affected, .. } = result {
                total_affected += affected;
            }
        }
        Ok(QueryResult::CommandOk {
            affected: total_affected,
            modified_catalog: !self.in_transaction(),
        })
    }

    /// v7.37.43-T4.4 — execute an INSERT carrying leading `WITH cte
    /// AS (…)` clauses (PG writable CTE outer). Materialises every
    /// CTE through `materialise_ctes` (running any modifying CTE
    /// bodies against the active engine so their mutations land in
    /// the current transaction), then runs the outer INSERT
    /// against an enriched catalog where each CTE alias resolves
    /// to its materialised rows. We add CTE temp tables to the
    /// active catalog for the duration of the outer execution and
    /// drop them when done — keeping the writes from the outer
    /// INSERT on the original tables intact.
    pub(crate) fn exec_insert_with_ctes(
        &mut self,
        mut stmt: InsertStatement,
    ) -> Result<QueryResult, EngineError> {
        let cte_defs = core::mem::take(&mut stmt.ctes);
        self.guard_dml_target_not_cte(&stmt.table, &cte_defs)?;
        let mut outer_reads = alloc::collections::BTreeSet::new();
        collect_insert_reads(&stmt, &mut outer_reads);
        Self::guard_no_returning_cte_refs(&cte_defs, &outer_reads)?;
        let mut cte_defs = cte_defs;
        // v7.39 (round 157) — CTE-shadows-table on the write path: rename
        // the shadow temps and rewrite this statement's read references,
        // then re-verify nothing still reads the old name.
        let shadow_renames = self.resolve_cte_table_shadows(&mut cte_defs)?;
        if !shadow_renames.is_empty() {
            for (old, new) in &shadow_renames {
                rename_rel_in_insert(&mut stmt, old, new);
            }
            let mut reads = alloc::collections::BTreeSet::new();
            collect_insert_reads(&stmt, &mut reads);
            shadow_rename_leak_check(&shadow_renames, &reads)?;
        }
        self.run_with_cte_temps(&cte_defs, |engine| engine.exec_insert(stmt))
    }

    /// v7.39 (round 150) — PG rejects REFERENCING a data-modifying CTE
    /// that has no RETURNING clause at parse analysis ("WITH query
    /// \"d\" does not have a RETURNING clause", 0A000,
    /// parse_relation.c addRangeTableEntryForCTE): the alias has no
    /// result shape to scan. An unreferenced no-RETURNING body stays
    /// legal and still runs. Checked BEFORE any CTE materialises —
    /// PG errors before execution, so no side effect may land.
    fn guard_no_returning_cte_refs(
        ctes: &[spg_sql::ast::Cte],
        outer_reads: &alloc::collections::BTreeSet<String>,
    ) -> Result<(), EngineError> {
        for (i, cte) in ctes.iter().enumerate() {
            let no_returning = match &cte.body {
                spg_sql::ast::CteBody::Select(_) => continue,
                spg_sql::ast::CteBody::Insert(b) => b.returning.is_none(),
                spg_sql::ast::CteBody::Update(b) => b.returning.is_none(),
                spg_sql::ast::CteBody::Delete(b) => b.returning.is_none(),
                spg_sql::ast::CteBody::Merge(b) => b.returning.is_none(),
            };
            if !no_returning {
                continue;
            }
            let named = |t: &String| t.eq_ignore_ascii_case(&cte.name);
            let referenced = outer_reads.iter().any(named)
                || ctes.iter().enumerate().any(|(j, other)| {
                    if j == i {
                        return false;
                    }
                    let mut reads = alloc::collections::BTreeSet::new();
                    collect_cte_body_reads(&other.body, &mut reads);
                    reads.iter().any(named)
                });
            if referenced {
                return Err(EngineError::Unsupported(alloc::format!(
                    "WITH query \"{}\" does not have a RETURNING clause",
                    cte.name
                )));
            }
        }
        Ok(())
    }

    /// v7.39 (round 157) — write-path half of the CTE-shadows-table
    /// support (the read path is round 156): rename each shadowing CTE's
    /// temp to a fresh `__cte_shadow_*` name and rewrite the LATER
    /// sibling bodies' read references (the shadowing CTE's own
    /// non-recursive body keeps seeing the real table; a RECURSIVE
    /// self-reference renames — it is the CTE). Returns the rename
    /// pairs; the CALLER must rewrite the outer statement's reads and
    /// re-verify with `shadow_rename_leak_check`. Later bodies are
    /// verified here: a surviving old-name read means the rewriter
    /// missed a spot — fail honestly rather than read the table.
    fn resolve_cte_table_shadows(
        &self,
        ctes: &mut [spg_sql::ast::Cte],
    ) -> Result<alloc::vec::Vec<(String, String)>, EngineError> {
        let mut renames: alloc::vec::Vec<(usize, String, String)> = alloc::vec::Vec::new();
        for i in 0..ctes.len() {
            let old = ctes[i].name.clone();
            if self.active_catalog().get(&old).is_none() {
                continue;
            }
            let mut new = alloc::format!("__cte_shadow_{old}");
            let mut k = 0usize;
            while self.active_catalog().get(&new).is_some()
                || ctes.iter().any(|c| c.name.eq_ignore_ascii_case(&new))
            {
                k += 1;
                new = alloc::format!("__cte_shadow_{old}_{k}");
            }
            ctes[i].name = new.clone();
            if ctes[i].recursive {
                rename_rel_in_cte_body(&mut ctes[i].body, &old, &new);
            }
            for c in ctes.iter_mut().skip(i + 1) {
                if c.name.eq_ignore_ascii_case(&old) {
                    break;
                }
                rename_rel_in_cte_body(&mut c.body, &old, &new);
            }
            renames.push((i, old, new));
        }
        // Leak check over the later sibling bodies (the shadowing CTE's
        // own body and EARLIER bodies legitimately read the real table).
        for (i, old, _) in &renames {
            for c in ctes.iter().skip(i + 1) {
                let mut reads = alloc::collections::BTreeSet::new();
                collect_cte_body_reads(&c.body, &mut reads);
                if reads.iter().any(|t| t.eq_ignore_ascii_case(old)) {
                    return Err(cte_shadow_err(old));
                }
            }
        }
        Ok(renames.into_iter().map(|(_, o, n)| (o, n)).collect())
    }

    /// v7.39 (round 149) — PG resolves a DML target only against real
    /// relations, never a CTE of the same statement (`WITH c AS (…)
    /// DELETE FROM c` errors "relation \"c\" does not exist"). SPG's
    /// temp-table CTE machinery would otherwise resolve the target to
    /// the just-installed temp — the write lands there and vanishes
    /// when the statement ends. Must run BEFORE the temps install
    /// (the catalog probe distinguishes a real same-named table).
    fn guard_dml_target_not_cte(
        &self,
        target: &str,
        ctes: &[spg_sql::ast::Cte],
    ) -> Result<(), EngineError> {
        if ctes.iter().any(|c| c.name.eq_ignore_ascii_case(target))
            && self.active_catalog().get(target).is_none()
        {
            return Err(EngineError::Storage(
                spg_storage::StorageError::TableNotFound {
                    name: target.into(),
                },
            ));
        }
        Ok(())
    }

    /// v7.37.43-T4.4 — UPDATE counterpart of `exec_insert_with_ctes`.
    pub(crate) fn exec_update_with_ctes(
        &mut self,
        mut stmt: spg_sql::ast::UpdateStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let cte_defs = core::mem::take(&mut stmt.ctes);
        self.guard_dml_target_not_cte(&stmt.table, &cte_defs)?;
        let mut outer_reads = alloc::collections::BTreeSet::new();
        collect_update_reads(&stmt, &mut outer_reads);
        Self::guard_no_returning_cte_refs(&cte_defs, &outer_reads)?;
        let mut cte_defs = cte_defs;
        let shadow_renames = self.resolve_cte_table_shadows(&mut cte_defs)?;
        if !shadow_renames.is_empty() {
            for (old, new) in &shadow_renames {
                rename_rel_in_update(&mut stmt, old, new);
            }
            let mut reads = alloc::collections::BTreeSet::new();
            collect_update_reads(&stmt, &mut reads);
            shadow_rename_leak_check(&shadow_renames, &reads)?;
        }
        self.run_with_cte_temps(&cte_defs, |engine| {
            // v7.39 (round 157) — uncorrelated-subquery materialisation
            // for SET / WHERE runs HERE, after the CTE temps installed
            // (the dispatch-level pass skips statements with a WITH).
            for (_, e) in &mut stmt.assignments {
                engine.resolve_expr_subqueries(e, cancel)?;
            }
            if let Some(w) = &mut stmt.where_ {
                engine.resolve_expr_subqueries(w, cancel)?;
            }
            engine.exec_update_cancel(&stmt, cancel)
        })
    }

    /// SELECT with a data-modifying CTE body (`WITH d AS (DELETE …
    /// RETURNING …) SELECT … FROM d`) — the writes must land
    /// transactionally, so the outer SELECT routes through the
    /// same &mut temp-table machinery the DML outers use.
    pub(crate) fn exec_select_with_modifying_ctes(
        &mut self,
        mut stmt: spg_sql::ast::SelectStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let cte_defs = core::mem::take(&mut stmt.ctes);
        let mut outer_reads = alloc::collections::BTreeSet::new();
        crate::acl::collect_read_tables(&stmt, &mut outer_reads);
        Self::guard_no_returning_cte_refs(&cte_defs, &outer_reads)?;
        let mut cte_defs = cte_defs;
        let shadow_renames = self.resolve_cte_table_shadows(&mut cte_defs)?;
        if !shadow_renames.is_empty() {
            for (old, new) in &shadow_renames {
                rename_rel_in_select(&mut stmt, old, new);
            }
            let mut reads = alloc::collections::BTreeSet::new();
            crate::acl::collect_read_tables(&stmt, &mut reads);
            shadow_rename_leak_check(&shadow_renames, &reads)?;
        }
        self.run_with_cte_temps(&cte_defs, |engine| engine.exec_select_cancel(&stmt, cancel))
    }

    /// v7.39 (round 149) — MERGE counterpart of `exec_insert_with_ctes`.
    pub(crate) fn exec_merge_with_ctes(
        &mut self,
        mut stmt: spg_sql::ast::MergeStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let cte_defs = core::mem::take(&mut stmt.ctes);
        self.guard_dml_target_not_cte(&stmt.target, &cte_defs)?;
        let mut outer_reads = alloc::collections::BTreeSet::new();
        collect_merge_reads(&stmt, &mut outer_reads);
        Self::guard_no_returning_cte_refs(&cte_defs, &outer_reads)?;
        let mut cte_defs = cte_defs;
        let shadow_renames = self.resolve_cte_table_shadows(&mut cte_defs)?;
        if !shadow_renames.is_empty() {
            for (old, new) in &shadow_renames {
                rename_rel_in_merge(&mut stmt, old, new);
            }
            let mut reads = alloc::collections::BTreeSet::new();
            collect_merge_reads(&stmt, &mut reads);
            shadow_rename_leak_check(&shadow_renames, &reads)?;
        }
        self.run_with_cte_temps(&cte_defs, |engine| engine.exec_merge_cancel(&stmt, cancel))
    }

    /// v7.37.43-T4.4 — DELETE counterpart of `exec_insert_with_ctes`.
    pub(crate) fn exec_delete_with_ctes(
        &mut self,
        mut stmt: spg_sql::ast::DeleteStatement,
        cancel: CancelToken<'_>,
    ) -> Result<QueryResult, EngineError> {
        let cte_defs = core::mem::take(&mut stmt.ctes);
        self.guard_dml_target_not_cte(&stmt.table, &cte_defs)?;
        let mut outer_reads = alloc::collections::BTreeSet::new();
        collect_delete_reads(&stmt, &mut outer_reads);
        Self::guard_no_returning_cte_refs(&cte_defs, &outer_reads)?;
        let mut cte_defs = cte_defs;
        let shadow_renames = self.resolve_cte_table_shadows(&mut cte_defs)?;
        if !shadow_renames.is_empty() {
            for (old, new) in &shadow_renames {
                rename_rel_in_delete(&mut stmt, old, new);
            }
            let mut reads = alloc::collections::BTreeSet::new();
            collect_delete_reads(&stmt, &mut reads);
            shadow_rename_leak_check(&shadow_renames, &reads)?;
        }
        self.run_with_cte_temps(&cte_defs, |engine| {
            // v7.39 (round 157) — see exec_update_with_ctes: the
            // uncorrelated-subquery pass runs after the temps install.
            if let Some(w) = &mut stmt.where_ {
                engine.resolve_expr_subqueries(w, cancel)?;
            }
            engine.exec_delete_cancel(&stmt, cancel)
        })
    }

    /// v7.37.43-T4.4 — install each CTE alias as a temp table on
    /// the active catalog (running modifying CTE bodies through
    /// `self` so writes land transactionally), execute the
    /// caller-supplied closure, then drop every CTE temp table
    /// regardless of success or failure (RAII-style cleanup
    /// keeps the catalog in a consistent shape after the outer
    /// statement returns).
    fn run_with_cte_temps<F>(
        &mut self,
        ctes: &[spg_sql::ast::Cte],
        body: F,
    ) -> Result<QueryResult, EngineError>
    where
        F: FnOnce(&mut Engine) -> Result<QueryResult, EngineError>,
    {
        // v7.39 (round 149) — a modifying CTE body's target must be a
        // real relation too, never a sibling CTE (PG: relation does
        // not exist). Checked before any temp installs so the catalog
        // probe still distinguishes a real same-named table.
        for cte in ctes {
            let body_target = match &cte.body {
                spg_sql::ast::CteBody::Select(_) => None,
                spg_sql::ast::CteBody::Insert(i) => Some(i.table.as_str()),
                spg_sql::ast::CteBody::Update(u) => Some(u.table.as_str()),
                spg_sql::ast::CteBody::Delete(d) => Some(d.table.as_str()),
                spg_sql::ast::CteBody::Merge(m) => Some(m.target.as_str()),
            };
            if let Some(t) = body_target {
                self.guard_dml_target_not_cte(t, ctes)?;
            }
        }
        // Phase 1 — execute / materialise each CTE in declaration
        // order, mutating self (real-table writes go through) and
        // capturing the rows that will populate the CTE alias.
        let mut installed: alloc::vec::Vec<String> = alloc::vec::Vec::with_capacity(ctes.len());
        for cte in ctes {
            if self.active_catalog().get(&cte.name).is_some() {
                self.drop_cte_temps(&installed);
                return Err(EngineError::Unsupported(alloc::format!(
                    "CTE name {:?} shadows an existing table; rename the CTE",
                    cte.name
                )));
            }
            let result = self.materialise_one_cte_to_temp(cte);
            let (columns, rows) = match result {
                Ok(out) => out,
                Err(e) => {
                    self.drop_cte_temps(&installed);
                    return Err(e);
                }
            };
            let inferred = crate::select::infer_column_types(&columns, &rows);
            let mut columns = inferred;
            if !cte.column_overrides.is_empty() {
                if cte.column_overrides.len() != columns.len() {
                    self.drop_cte_temps(&installed);
                    return Err(EngineError::Unsupported(alloc::format!(
                        "CTE {:?} column list has {} names but body returns {} columns",
                        cte.name,
                        cte.column_overrides.len(),
                        columns.len()
                    )));
                }
                for (col, name) in columns.iter_mut().zip(cte.column_overrides.iter()) {
                    col.name.clone_from(name);
                }
            }
            let schema = spg_storage::TableSchema::new(cte.name.clone(), columns);
            if let Err(e) = self.active_catalog_mut().create_table(schema) {
                self.drop_cte_temps(&installed);
                return Err(EngineError::Storage(e));
            }
            installed.push(cte.name.clone());
            let table = self
                .active_catalog_mut()
                .get_mut(&cte.name)
                .expect("just-created CTE temp must exist");
            for row in rows {
                if let Err(e) = table.insert(row) {
                    let installed_clone = installed.clone();
                    self.drop_cte_temps(&installed_clone);
                    return Err(EngineError::Storage(e));
                }
            }
        }
        // Phase 2 — execute the outer statement against the
        // enriched catalog.
        let outcome = body(self);
        // Phase 3 — drop CTE temp tables regardless of outcome.
        self.drop_cte_temps(&installed);
        outcome
    }

    fn drop_cte_temps(&mut self, names: &[String]) {
        for name in names {
            let _ = self.active_catalog_mut().drop_table(name);
        }
    }

    /// v7.37.43-T4.4 — materialise one CTE body, running any
    /// modifying statement against the active catalog and
    /// capturing the RETURNING projection.
    fn materialise_one_cte_to_temp(
        &mut self,
        cte: &spg_sql::ast::Cte,
    ) -> Result<
        (
            alloc::vec::Vec<spg_storage::ColumnSchema>,
            alloc::vec::Vec<Row<'static>>,
        ),
        EngineError,
    > {
        use spg_sql::ast::CteBody;
        let cancel = CancelToken::none();
        match &cte.body {
            CteBody::Select(s) if cte.recursive => {
                // Reuse the SELECT-side recursive helper by wrapping
                // through a synthetic Cte (CteBody::Select).
                let snapshot = self.active_catalog().clone();
                let synthetic = spg_sql::ast::Cte {
                    name: cte.name.clone(),
                    body: CteBody::Select(s.clone()),
                    recursive: true,
                    column_overrides: cte.column_overrides.clone(),
                    search: None,
                    cycle: None,
                };
                let (columns, rows) =
                    self.materialise_recursive_cte(&synthetic, &snapshot, cancel)?;
                Ok((columns, rows))
            }
            CteBody::Select(s) => {
                let result = self.exec_select_cancel(s, cancel)?;
                match result {
                    QueryResult::Rows { columns, rows } => Ok((columns, rows)),
                    other => Err(EngineError::Unsupported(alloc::format!(
                        "CTE {:?} SELECT body produced {other:?}",
                        cte.name
                    ))),
                }
            }
            CteBody::Insert(body) => {
                // round 151 — a WITH-headed body keeps its own ctes; the
                // body statement routes through its writable-CTE entry.
                let body = (**body).clone();
                let result = self.exec_insert(body)?;
                self.cte_returning_or_empty(&cte.name, result)
            }
            CteBody::Update(body) => {
                // round 151 — a WITH-headed body keeps its own ctes; the
                // body statement routes through its writable-CTE entry.
                let body = (**body).clone();
                let result = self.exec_update_cancel(&body, cancel)?;
                self.cte_returning_or_empty(&cte.name, result)
            }
            CteBody::Delete(body) => {
                // round 151 — a WITH-headed body keeps its own ctes; the
                // body statement routes through its writable-CTE entry.
                let body = (**body).clone();
                let result = self.exec_delete_cancel(&body, cancel)?;
                self.cte_returning_or_empty(&cte.name, result)
            }
            // v7.39 (round 149) — PG 17 allows MERGE as a CTE body;
            // without RETURNING the alias materialises empty, as with
            // the other data-modifying bodies.
            CteBody::Merge(body) => {
                // round 151 — a WITH-headed body keeps its own ctes; the
                // body statement routes through its writable-CTE entry.
                let body = (**body).clone();
                let result = self.exec_merge_cancel(&body, cancel)?;
                self.cte_returning_or_empty(&cte.name, result)
            }
        }
    }

    fn cte_returning_or_empty(
        &self,
        cte_name: &str,
        result: QueryResult,
    ) -> Result<
        (
            alloc::vec::Vec<spg_storage::ColumnSchema>,
            alloc::vec::Vec<Row<'static>>,
        ),
        EngineError,
    > {
        match result {
            QueryResult::Rows { columns, rows } => Ok((columns, rows)),
            QueryResult::CommandOk { .. } => {
                let placeholder = spg_storage::ColumnSchema::new(
                    alloc::format!("{cte_name}_returning_absent"),
                    spg_storage::DataType::Text,
                    true,
                );
                Ok((alloc::vec![placeholder], alloc::vec::Vec::new()))
            }
        }
    }
}

impl Engine {
    /// v7.9.4 — INSERT / UPDATE / DELETE RETURNING projector.
    /// Given the table name, the user-supplied projection items,
    /// and the mutated rows (post-insert / post-update values, or
    /// pre-delete snapshot), build a `QueryResult::Rows` whose
    /// schema describes the projected columns. Mailrs migration
    /// blocker #1.
    fn build_returning_rows(
        &self,
        table_name: &str,
        items: &[SelectItem],
        mutated_rows: Vec<Vec<Value<'static>>>,
    ) -> Result<QueryResult, EngineError> {
        self.build_returning_rows_old_new(table_name, None, items, mutated_rows, None, None)
    }

    /// v7.39 (read01 round 126) — RETURNING with optional `OLD.col` / `NEW.col`
    /// references (PG18). `default_rows` are the bare-column source (NEW for
    /// INSERT/UPDATE, OLD for DELETE); `old_rows` / `new_rows` back the `OLD.`
    /// / `NEW.` qualifiers (`None` = the row didn't exist on that side, so those
    /// columns read NULL — OLD for INSERT, NEW for DELETE). When no OLD/NEW
    /// reference is present this is the plain single-row projection.
    fn build_returning_rows_old_new(
        &self,
        table_name: &str,
        // v7.39 (round 241) — the target alias when the statement gave one:
        // the catalog lookup needs the real name, the expression qualifier
        // (and `alias.*` wildcards) the alias.
        alias: Option<&str>,
        items: &[SelectItem],
        default_rows: Vec<Vec<Value<'static>>>,
        old_rows: Option<Vec<Vec<Value<'static>>>>,
        new_rows: Option<Vec<Vec<Value<'static>>>>,
    ) -> Result<QueryResult, EngineError> {
        let table = self.active_catalog().get(table_name).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound {
                name: table_name.into(),
            })
        })?;
        let schema_cols = table.schema().columns.clone();
        // Output columns come from the ORIGINAL items: `derive_output_columns`
        // resolves `OLD.v` / `NEW.v` by name (ignoring the qualifier) to v's
        // name + type, matching PG.
        let qualifier = alias.unwrap_or(table_name);
        let columns = self.derive_output_columns(items, &schema_cols, qualifier);

        // Rewrite OLD./NEW. qualifiers to synthetic bare columns for value
        // resolution. If none appear, take the plain fast path.
        let mut items_rw = items.to_vec();
        let mut uses_old_new = false;
        for it in &mut items_rw {
            match it {
                SelectItem::Expr { expr, .. } => uses_old_new |= rewrite_returning_old_new(expr),
                // v7.39 (round 128) — `OLD.*` / `NEW.*` also need the synthetic
                // path (a plain `t.*` / `*` stays on the fast path).
                SelectItem::QualifiedWildcard(q) => {
                    let ql = q.to_ascii_lowercase();
                    uses_old_new |= ql == "old" || ql == "new";
                }
                SelectItem::Wildcard => {}
            }
        }
        if !uses_old_new {
            let mut out_rows: Vec<Row<'static>> = Vec::with_capacity(default_rows.len());
            for values in default_rows {
                let row = Row::new(values);
                let projected = self.project_row_simple(&row, items, &schema_cols, qualifier)?;
                out_rows.push(projected);
            }
            return Ok(QueryResult::Rows {
                columns,
                rows: out_rows,
            });
        }

        // Synthetic schema: [table cols | __ret_old_<c> | __ret_new_<c>].
        let arity = schema_cols.len();
        let mut syn_schema = schema_cols.clone();
        for c in &schema_cols {
            let mut oc = c.clone();
            oc.name = alloc::format!("__ret_old_{}", c.name);
            syn_schema.push(oc);
        }
        for c in &schema_cols {
            let mut nc = c.clone();
            nc.name = alloc::format!("__ret_new_{}", c.name);
            syn_schema.push(nc);
        }
        let n = default_rows.len();
        let null_block = || alloc::vec![alloc::vec![Value::Null; arity]; n];
        let old_rows = old_rows.unwrap_or_else(null_block);
        let new_rows = new_rows.unwrap_or_else(null_block);
        let ctx = self.ev_ctx(&syn_schema, Some(qualifier));
        let cancel = CancelToken::none();
        let mut out_rows: Vec<Row<'static>> = Vec::with_capacity(n);
        for i in 0..n {
            let mut syn_vals = default_rows[i].clone();
            syn_vals.extend(old_rows[i].iter().cloned());
            syn_vals.extend(new_rows[i].iter().cloned());
            let syn_row = Row::new(syn_vals);
            let mut vals: Vec<Value<'static>> = Vec::with_capacity(items_rw.len());
            for it in &items_rw {
                match it {
                    // A bare wildcard expands to the default (table) columns
                    // only, never the synthetic OLD/NEW blocks.
                    SelectItem::Wildcard => vals.extend(default_rows[i].iter().cloned()),
                    // `OLD.*` / `NEW.*` expand the pre-image / post-image block;
                    // a table-qualified `t.*` expands the default columns.
                    SelectItem::QualifiedWildcard(q) => match q.to_ascii_lowercase().as_str() {
                        "old" => vals.extend(old_rows[i].iter().cloned()),
                        "new" => vals.extend(new_rows[i].iter().cloned()),
                        _ => vals.extend(default_rows[i].iter().cloned()),
                    },
                    SelectItem::Expr { expr, .. } => {
                        vals.push(
                            self.eval_expr_with_correlated(expr, &syn_row, &ctx, cancel, None)?,
                        );
                    }
                }
            }
            out_rows.push(Row::new(vals));
        }
        Ok(QueryResult::Rows {
            columns,
            rows: out_rows,
        })
    }
}

/// v7.39 (read01 round 126) — rewrite `OLD.col` / `NEW.col` references in a
/// RETURNING expression to synthetic bare columns (`__ret_old_col` /
/// `__ret_new_col`) so they resolve against the OLD/NEW blocks of the synthetic
/// RETURNING row. Returns whether any OLD/NEW reference was found. Covers the
/// expression shapes a RETURNING item realistically uses.
fn rewrite_returning_old_new(expr: &mut Expr) -> bool {
    match expr {
        Expr::Column(c) => {
            if let Some(q) = &c.qualifier {
                let ql = q.to_ascii_lowercase();
                if ql == "old" || ql == "new" {
                    c.name = alloc::format!("__ret_{ql}_{}", c.name);
                    c.qualifier = None;
                    return true;
                }
            }
            false
        }
        Expr::Binary { lhs, rhs, .. } => {
            let a = rewrite_returning_old_new(lhs);
            rewrite_returning_old_new(rhs) || a
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            rewrite_returning_old_new(expr)
        }
        Expr::FunctionCall { args, .. } => {
            let mut found = false;
            for a in args {
                found |= rewrite_returning_old_new(a);
            }
            found
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            let mut found = false;
            if let Some(o) = operand {
                found |= rewrite_returning_old_new(o);
            }
            for (c, v) in branches {
                found |= rewrite_returning_old_new(c);
                found |= rewrite_returning_old_new(v);
            }
            if let Some(x) = else_branch {
                found |= rewrite_returning_old_new(x);
            }
            found
        }
        _ => false,
    }
}

/// v7.39 (round 130) — is `e` a bare `merge_action()` call?
fn is_merge_action_call(e: &Expr) -> bool {
    matches!(e, Expr::FunctionCall { name, args }
        if name.eq_ignore_ascii_case("merge_action") && args.is_empty())
}

/// v7.39 (round 130) — replace `merge_action()` calls anywhere in `expr` with a
/// reference to the synthetic `__merge_action` column (the per-row action
/// string). Mirrors `rewrite_returning_old_new`'s tree walk.
fn rewrite_merge_action(expr: &mut Expr) {
    match expr {
        Expr::FunctionCall { name, args }
            if name.eq_ignore_ascii_case("merge_action") && args.is_empty() =>
        {
            *expr = Expr::Column(spg_sql::ast::ColumnName {
                qualifier: None,
                name: String::from("__merge_action"),
            });
        }
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_merge_action(lhs);
            rewrite_merge_action(rhs);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            rewrite_merge_action(expr)
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                rewrite_merge_action(a);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                rewrite_merge_action(o);
            }
            for (c, v) in branches {
                rewrite_merge_action(c);
                rewrite_merge_action(v);
            }
            if let Some(x) = else_branch {
                rewrite_merge_action(x);
            }
        }
        _ => {}
    }
}

/// v7.39 (round 130) — expand a MERGE RETURNING list against the synthetic
/// schema `build_merge_returning` builds. `*` → bare target columns; `OLD.*` /
/// `NEW.*` → the pre/post-image blocks (aliased back to the bare column name);
/// `t.*` / `s.*` are left for `build_projection`'s prefix expansion. Scalar
/// expressions have `merge_action()` and `OLD.`/`NEW.` qualifiers rewritten onto
/// the synthetic column names.
fn expand_merge_returning_items(
    items: &[SelectItem],
    target_alias: &str,
    source_alias: &str,
    target_cols: &[ColumnSchema],
) -> Vec<SelectItem> {
    let mut out: Vec<SelectItem> = Vec::new();
    for it in items {
        match it {
            // PG expands a bare `*` in MERGE RETURNING to the source columns
            // first, then the target columns (the MERGE range-table order).
            SelectItem::Wildcard => {
                out.push(SelectItem::QualifiedWildcard(source_alias.into()));
                out.push(SelectItem::QualifiedWildcard(target_alias.into()));
            }
            SelectItem::QualifiedWildcard(q) => {
                let ql = q.to_ascii_lowercase();
                if ql == "old" || ql == "new" {
                    let prefix = if ql == "old" {
                        "__ret_old_"
                    } else {
                        "__ret_new_"
                    };
                    for c in target_cols {
                        out.push(SelectItem::Expr {
                            expr: Expr::Column(spg_sql::ast::ColumnName {
                                qualifier: None,
                                name: alloc::format!("{prefix}{}", c.name),
                            }),
                            alias: Some(c.name.clone()),
                        });
                    }
                } else {
                    out.push(SelectItem::QualifiedWildcard(q.clone()));
                }
            }
            SelectItem::Expr { expr, alias } => {
                let mut e = expr.clone();
                // PG labels an unaliased column by its bare name (dropping the
                // qualifier) and a bare `merge_action()` "merge_action". Capture
                // that from the ORIGINAL expr, before the OLD./NEW. rewrite
                // mangles the name into `__ret_old_*`.
                let out_alias = if alias.is_some() {
                    alias.clone()
                } else if is_merge_action_call(&e) {
                    Some(String::from("merge_action"))
                } else if let Expr::Column(c) = &e {
                    Some(c.name.clone())
                } else {
                    None
                };
                rewrite_merge_action(&mut e);
                rewrite_returning_old_new(&mut e);
                out.push(SelectItem::Expr {
                    expr: e,
                    alias: out_alias,
                });
            }
        }
    }
    out
}

/// Build the INSERT column permutation `tuple_pos[c] = Some(j)` (schema
/// column `c` is filled from the `j`-th tuple slot; `None` = fill with
/// NULL / DEFAULT). `None` overall means the 1-1 fast path. Validates
/// the column list once for reuse across every row.
/// v7.38 (read01) — the parser's `DEFAULT`-in-VALUES marker: a `__column_default()`
/// call with no args. `INSERT … VALUES (…, DEFAULT, …)` uses it for a slot, and
/// the INSERT executor resolves it against the target column's declared default.
fn is_column_default_marker(e: &Expr) -> bool {
    matches!(e, Expr::FunctionCall { name, args } if name == "__column_default" && args.is_empty())
}

/// v7.39 (round 433) — does this AUTO_INCREMENT column need a generated
/// value for the supplied cell?
///
/// PG's answer is "only when the cell is NULL". MySQL's — measured on
/// MariaDB 11 — also counts an explicit **zero**: `INSERT INTO t(id) VALUES
/// (0)` stores the next generated id, exactly as `VALUES (NULL)` does, and
/// LAST_INSERT_ID() reports the generated value. Legacy code and several
/// ORMs write 0 for "please assign one", so treating it literally stored a
/// row with id 0 AND left every later id one short of MySQL's.
///
/// The zero is recognised through the same widening every integer literal
/// gets, so `'0'` (MariaDB generates for it too) and a zero of any integer
/// width all qualify. A float / decimal zero does not: MySQL only applies
/// this to integer AUTO_INCREMENT columns, which is the only shape SPG
/// allows the attribute on.
///
/// MySQL's `NO_AUTO_VALUE_ON_ZERO` sql_mode turns this off and stores the 0
/// literally. SPG tracks only the escaping bit of sql_mode today, so that
/// mode is not honoured — the default (this behaviour) is what a session
/// gets. Noted rather than guessed at.
fn auto_increment_needs_value(col: &ColumnSchema, raw: &Value<'_>, mysql: bool) -> bool {
    if !col.auto_increment {
        return false;
    }
    if raw.is_null() {
        return true;
    }
    mysql
        && match raw {
            Value::SmallInt(n) => *n == 0,
            Value::Int(n) => *n == 0,
            Value::BigInt(n) => *n == 0,
            Value::Text(s) => s.trim().parse::<i64>() == Ok(0),
            _ => false,
        }
}

/// v7.39 (round 433) — the value an AUTO_INCREMENT cell was explicitly
/// given, as an integer, or None when it is not an integer at all. Same
/// widening as [`auto_increment_needs_value`] reads.
fn explicit_auto_value(raw: &Value<'_>) -> Option<i64> {
    match raw {
        Value::SmallInt(n) => Some(i64::from(*n)),
        Value::Int(n) => Some(i64::from(*n)),
        Value::BigInt(n) => Some(*n),
        Value::Text(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// v7.39 (round 433) — the statement-local AUTO_INCREMENT cursor's starting
/// point for `col`: the table's next value, floored by any sequence restart.
/// Extracted so the generate path and the explicit-value bump path below can
/// never seed from different places.
fn auto_cursor_seed(
    table: &spg_storage::Table,
    i: usize,
    col: &ColumnSchema,
    seq_floors: &alloc::collections::BTreeMap<usize, i64>,
) -> Result<i64, EngineError> {
    let base = table.next_auto_value(i).ok_or_else(|| {
        EngineError::Unsupported(alloc::format!(
            "AUTO_INCREMENT applies to integer columns only (column `{}`)",
            col.name
        ))
    })?;
    Ok(base.max(seq_floors.get(&i).copied().unwrap_or(i64::MIN)))
}

/// The `DEFAULT`-slot marker expression (`__column_default()`), used to
/// force a column to its declared default / sequence value.
fn column_default_marker() -> Expr {
    Expr::FunctionCall {
        name: String::from("__column_default"),
        args: Vec::new(),
    }
}

fn build_tuple_pos(
    columns: &Option<Vec<String>>,
    column_meta: &[ColumnSchema],
    table_name: &str,
) -> Result<Option<Vec<Option<usize>>>, EngineError> {
    let schema_cols_len = column_meta.len();
    // Build a permutation `tuple_pos[c] = Some(j)` meaning schema
    // column `c` is filled from the `j`-th tuple slot; `None` means
    // "fill with NULL". Validated once and reused for every row.
    let tuple_pos: Option<Vec<Option<usize>>> = match columns {
        None => None, // 1-1 mapping, fast path
        Some(cols) => {
            let mut map = alloc::vec![None; schema_cols_len];
            for (j, name) in cols.iter().enumerate() {
                let idx = column_meta
                    .iter()
                    .position(|c| c.name == *name)
                    .ok_or_else(|| {
                        // v7.39 (read01 round 88) — PG names the relation in an
                        // INSERT target-column miss: `column "x" of relation "t"
                        // does not exist` (42703). The bare ColumnNotFound
                        // ("column \"x\" does not exist") dropped the relation.
                        EngineError::Unsupported(alloc::format!(
                            "column \"{}\" of relation \"{}\" does not exist",
                            name,
                            table_name
                        ))
                    })?;
                if map[idx].is_some() {
                    // v7.39 (read01 round 89) — a column named twice in the
                    // INSERT target list is PG's 42701, not an arity error:
                    // `column "a" specified more than once`.
                    return Err(EngineError::Unsupported(alloc::format!(
                        "column \"{name}\" specified more than once"
                    )));
                }
                map[idx] = Some(j);
            }
            // v7.39 (read01 round 117) — the omitted-column NOT NULL pre-check
            // used to fire here (before row values existed), so it could not
            // carry PG's `DETAIL: Failing row contains (...)`. It is now
            // subsumed by `enforce_not_null` over the fully assembled rows: an
            // omitted no-default not-null column lands as NULL and is caught
            // there (pre-write, with the row for the DETAIL), together with an
            // explicitly-supplied NULL — one path, one message shape.
            Some(map)
        }
    };
    Ok(tuple_pos)
}

/// v7.37.7(sentori Epic 3 P1)— for every column with a stored
/// `generated_stored_expr`, parse the cached Display-form source,
/// evaluate it against each candidate row, coerce to the column
/// type, and overwrite whatever the caller passed in that slot.
/// Mirrors PG's "GENERATED ALWAYS AS … STORED" semantics — the
/// user has no say in the cell's value, the expression always wins.
///
/// Called from the INSERT and UPDATE row-assembly paths after the
/// regular column-value coercion runs, so the expression sees
/// fully-typed sibling cells.
pub(crate) fn apply_generated_stored_columns(
    column_meta: &[ColumnSchema],
    rows: &mut [Vec<Value<'static>>],
) -> Result<(), EngineError> {
    use spg_engine_no_alias::ParsedExpr;
    let mut parsed: Vec<Option<ParsedExpr>> = Vec::with_capacity(column_meta.len());
    for col in column_meta {
        if let Some(src) = &col.generated_stored_expr {
            let expr = spg_sql::parser::parse_expression(src).map_err(EngineError::Parse)?;
            parsed.push(Some(ParsedExpr {
                expr,
                ty: col.ty,
                col_name: col.name.clone(),
            }));
        } else {
            parsed.push(None);
        }
    }
    if parsed.iter().all(Option::is_none) {
        return Ok(());
    }
    // Empty params slice + no sequence resolver — a stored generated
    // expression is row-local and can't reference $N or sequences.
    let no_params: [spg_storage::Value; 0] = [];
    for row_values in rows.iter_mut() {
        let row = spg_storage::Row::new(row_values.clone());
        for (idx, slot) in parsed.iter().enumerate() {
            let Some(pe) = slot else {
                continue;
            };
            let ctx = crate::eval::EvalContext {
                columns: column_meta,
                table_alias: None,
                params: &no_params,
                default_text_search_config: None,
                sequence_resolver: None,
                catalog: None,
                mysql_dialect: false,
                session_gucs: None,
                users: None,
                fn_depth: 0,
                engine: None,
                sample_rng: None,
                recursion_base: core::cell::Cell::new(0),
                render_style: crate::eval::RenderStyle::default(),
                tz_offset_fn: None,
                tz_localize_fn: None,
                tz_abbrev_fn: None,
                // This DEFAULT-expression eval path has no engine handle;
                // gen_random_bytes / uuidv7 aren't expected in a column
                // DEFAULT, so the deterministic fallbacks are acceptable here.
                salt_fn: None,
                backend_pid_fn: None,
                backend_signal_fn: None,
                clock: None,
                xact: None,
                assigned_xid: core::cell::Cell::new(None),
            };
            let value = crate::eval::eval_expr(&pe.expr, &row, &ctx).map_err(EngineError::Eval)?;
            let coerced = crate::coerce_value(value, pe.ty, &pe.col_name, idx)?;
            row_values[idx] = coerced;
        }
    }
    Ok(())
}

/// Module alias to keep the parser-form on hand while we walk many
/// rows; the `mod` boundary is only here to dodge an "Expr too
/// large for the borrow checker" complaint about reusing a parsed
/// expression across the per-row loop above without cloning each
/// iteration(Expr is `Clone` and we already pay one clone per
/// `apply` call to materialise the row view; the per-row inner
/// loop borrows the parsed Expr).
mod spg_engine_no_alias {
    use spg_sql::ast::Expr;
    use spg_storage::DataType;
    pub(crate) struct ParsedExpr {
        pub expr: Expr,
        pub ty: DataType,
        pub col_name: alloc::string::String,
    }
}

/// Stage 1 — parse every INSERT tuple into a coerced row of `Value`s:
/// apply the column permutation, mint AUTO_INCREMENT ids (statement-
/// scoped cursors), run DEFAULT / ENUM / SET / unsigned-range checks.
/// Reads the table immutably (`next_auto_value`); no row is written yet.
#[allow(clippy::too_many_arguments)]
fn parse_insert_rows(
    table: &spg_storage::Table,
    // v7.39 (read01 round 55) — the catalog, so a user-named cast in an
    // INSERT's VALUES (`ROW(1,2)::pt`, `5::posint`) resolves. Without it the
    // whole INSERT failed with "unsupported cast target `::pt`".
    catalog: Option<&spg_storage::Catalog>,
    mut rows: Vec<Vec<Expr>>,
    column_meta: &[ColumnSchema],
    tuple_pos: &Option<Vec<Option<usize>>>,
    expected_tuple_len: usize,
    clock: Option<crate::ClockFn>,
    seq_floors: &alloc::collections::BTreeMap<usize, i64>,
    enum_label_lookup: &alloc::collections::BTreeMap<usize, Vec<String>>,
    set_variant_lookup: &alloc::collections::BTreeMap<usize, Vec<String>>,
    overriding: spg_sql::ast::Overriding,
    // v7.39 (round 347, M2) — the FIRST auto-generated value of this
    // statement, which is exactly what MySQL's LAST_INSERT_ID() reports
    // (measured: a three-row insert reports the first id, not the last).
    // `None` when the statement generated none, in which case MariaDB
    // leaves the session's previous value alone.
    first_auto: &mut Option<i64>,
    // v7.39 (round 367, M20) — the session dialect, so a `0x…` / `X'…'`
    // binary-string literal coerces into its target column the MySQL way
    // (big-endian number into a numeric column, bytes-as-string into a
    // text column) instead of failing the byte→column type check.
    mysql: bool,
    // v7.39 (round 434) — the statement was spelled `INSERT IGNORE`, so a
    // value the ordinary path would reject is bent to fit instead (see
    // `mysql_ignore_fit`). MySQL-only; a PG session never sets it.
    ignore: bool,
    // v7.39 (round 470) — the session's `sql_mode` has no STRICT_ flag, so
    // a value that would raise is bent to fit. NOT the same trigger as
    // `ignore`, and measured on MariaDB 11 to differ in one place: a
    // non-strict session still raises 1048 on an EXPLICIT NULL into a NOT
    // NULL column, while `INSERT IGNORE` stores 0 for it. Only an OMITTED
    // column gets the implicit default.
    non_strict: bool,
) -> Result<Vec<Vec<Value<'static>>>, EngineError> {
    use spg_sql::ast::Overriding;
    let schema_cols_len = column_meta.len();
    let mut all_values: Vec<Vec<Value<'static>>> = Vec::with_capacity(rows.len());
    // v7.24 (round-16 collateral) — statement-scoped serial
    // cursors. next_auto_value() is a max+1 scan over COMMITTED
    // rows; multi-row `INSERT … VALUES (…),(…)` computed it per
    // tuple BEFORE any insertion, so every row drew the SAME id
    // (then sailed through, compounding with the inline-PK
    // enforcement gap). First use per column seeds from the
    // table; subsequent rows increment.
    let mut auto_cursors: alloc::collections::BTreeMap<usize, i64> =
        alloc::collections::BTreeMap::new();
    // v7.38 (read01 P6.41) — PG rejects an EXPLICIT value for a generated
    // column ("cannot insert a non-DEFAULT value into column …"), but a
    // `DEFAULT` marker for it is allowed (the column recomputes). So a
    // generated column is refused only when SOME row supplies a non-DEFAULT
    // value at its slot.
    // v7.38 (read01 sweep) — with no column list a short row omits its trailing
    // positions, so a slot is "reached" only when a row is long enough.
    for (i, col) in column_meta.iter().enumerate() {
        if col.generated_stored_expr.is_none() {
            continue;
        }
        let slot: Option<usize> = match tuple_pos {
            Some(map) => map.get(i).copied().flatten(),
            None => Some(i),
        };
        let Some(j) = slot else { continue };
        let has_explicit_value = rows
            .iter()
            .any(|row| row.get(j).is_some_and(|e| !is_column_default_marker(e)));
        if has_explicit_value {
            return Err(EngineError::Unsupported(alloc::format!(
                "cannot insert a non-DEFAULT value into column {:?} — it is a generated column",
                col.name
            )));
        }
    }
    // v7.38 (read01) — OVERRIDING USER VALUE: ignore any explicit value on an
    // identity column and generate from the sequence. We flatten a supplied
    // value to the DEFAULT marker at the affected slots so the existing
    // auto-increment path fills them. Done before the ALWAYS-reject check so a
    // USER override on an ALWAYS column generates rather than erroring.
    if overriding == Overriding::User {
        for (i, col) in column_meta.iter().enumerate() {
            if !col.auto_increment {
                continue;
            }
            let slot: Option<usize> = match tuple_pos {
                Some(map) => map.get(i).copied().flatten(),
                None => Some(i),
            };
            let Some(j) = slot else { continue };
            for row in rows.iter_mut() {
                if let Some(e) = row.get_mut(j) {
                    if !is_column_default_marker(e) {
                        *e = column_default_marker();
                    }
                }
            }
        }
    }
    // v7.38 (read01) — GENERATED ALWAYS AS IDENTITY. PG rejects an
    // explicit non-DEFAULT value for such a column unless the statement
    // carries OVERRIDING SYSTEM VALUE. A DEFAULT marker is always allowed
    // (the sequence fills it). OVERRIDING USER VALUE already flattened any
    // explicit value above, so it too passes here and generates. BY DEFAULT
    // identity columns (auto_increment && !identity_always) accept an
    // explicit value unmodified — no check here.
    if overriding != Overriding::System {
        for (i, col) in column_meta.iter().enumerate() {
            if !col.identity_always {
                continue;
            }
            let slot: Option<usize> = match tuple_pos {
                Some(map) => map.get(i).copied().flatten(),
                None => Some(i),
            };
            let Some(j) = slot else { continue };
            let has_explicit_value = rows
                .iter()
                .any(|row| row.get(j).is_some_and(|e| !is_column_default_marker(e)));
            if has_explicit_value {
                // v7.39 (read01 round 45) — PG's three-part message
                // (428C9 at the wire): the DETAIL/HINT tail splits into
                // its own PG_DIAG fields on the wire.
                let name = &col.name;
                return Err(EngineError::Unsupported(alloc::format!(
                    "cannot insert a non-DEFAULT value into column {name:?} \
                     DETAIL: Column {name:?} is an identity column defined as GENERATED ALWAYS.\n\
                     HINT:  Use OVERRIDING SYSTEM VALUE to override."
                )));
            }
        }
    }
    for tuple in rows {
        // v7.38 (read01 sweep) — with no explicit column list, PG lets a row
        // supply FEWER values than the table has columns; the trailing columns
        // take their DEFAULT (or NULL). A row is still rejected for supplying
        // MORE values than columns, and an explicit column list must match its
        // value count exactly.
        let too_many = tuple.len() > expected_tuple_len;
        let list_mismatch = tuple_pos.is_some() && tuple.len() != expected_tuple_len;
        if too_many || list_mismatch {
            // v7.39 (read01 round 88) — PG's 42601 wording, distinguishing the
            // two directions. Before, both came out as SPG's generic
            // "row arity mismatch: expected N columns, got M", which no client
            // matches. (More values → "more expressions than target columns";
            // a column list with fewer values → "more target columns than
            // expressions".)
            let msg = if tuple.len() > expected_tuple_len {
                "INSERT has more expressions than target columns"
            } else {
                "INSERT has more target columns than expressions"
            };
            return Err(EngineError::Unsupported(msg.into()));
        }
        // Fast path: no column-list permutation → tuple slot j
        // maps to schema column j. We can zip schema with tuple
        // and skip the `raw_tuple` staging allocation entirely.
        let values: Vec<Value<'static>> = if let Some(map) = &tuple_pos {
            // Permuted path: still need raw_tuple to index by `map[i]`. A
            // `DEFAULT` marker maps to None here and is resolved per-column
            // below (it has no column-free value).
            let raw_tuple: Vec<Option<Value<'static>>> = tuple
                .into_iter()
                .map(|e| {
                    if is_column_default_marker(&e) {
                        Ok(None)
                    } else {
                        literal_expr_to_value_in(e, catalog).map(Some)
                    }
                })
                .collect::<Result<_, _>>()?;
            let mut out = Vec::with_capacity(schema_cols_len);
            for (i, col) in column_meta.iter().enumerate() {
                // v7.39 (round 470) — a column the statement did not name,
                // or named as `DEFAULT`, is "omitted" for MySQL's purposes.
                let omitted = match map[i] {
                    Some(j) => raw_tuple[j].is_none(),
                    None => true,
                };
                let mut raw = match map[i] {
                    Some(j) => match &raw_tuple[j] {
                        Some(v) => v.clone(),
                        None => resolve_column_default_free(col, clock)?,
                    },
                    None => resolve_column_default_free(col, clock)?,
                };
                if auto_increment_needs_value(col, &raw, mysql) {
                    let next = match auto_cursors.get(&i) {
                        Some(n) => *n,
                        None => auto_cursor_seed(table, i, col, seq_floors)?,
                    };
                    auto_cursors.insert(i, next + 1);
                    if first_auto.is_none() {
                        *first_auto = Some(next);
                    }
                    raw = Value::BigInt(next);
                } else if mysql
                    && col.auto_increment
                    && let Some(n) = explicit_auto_value(&raw)
                {
                    // v7.39 (round 433) — an explicit value inside a
                    // multi-row INSERT raises the counter for the LATER rows
                    // of the same statement, measured on MariaDB 11:
                    // `(NULL,·),(7,·),(NULL,·)` yields 1, 7, 8. SPG derives
                    // the next value from the table's current max, which has
                    // not moved yet mid-statement, so the third row used to
                    // land on 2 — a value the statement had already handed
                    // out on a wider table, and a silent drift from MySQL's
                    // ids either way. A LOWER explicit value never pulls the
                    // counter back (`(50,·),(3,·),(NULL,·)` yields 51), so
                    // the seed still comes from the table.
                    let cursor = match auto_cursors.get(&i) {
                        Some(c) => *c,
                        None => auto_cursor_seed(table, i, col, seq_floors)?,
                    };
                    auto_cursors.insert(i, cursor.max(n.saturating_add(1)));
                }
                // v7.39 (round 263) — a COMPOSITE column relabels + coerces
                // its value through the declared type first; ROW()'s
                // placeholder names would otherwise be stored and the
                // read side, which looks fields up by name, would rebuild
                // an all-NULL record.
                let raw = crate::conversions::normalize_composite_for_column(
                    raw.into_owned(),
                    col,
                    catalog,
                )?;
                let raw = crate::conversions::mysql_bytes_for_column(raw, col.ty, mysql);
                // v7.39 (round 434) — `INSERT IGNORE` bends a value that
                // would otherwise raise, so a MySQL bulk load never stops
                // mid-file. Values the ordinary path accepts are untouched.
                // v7.39 (round 470) — a non-strict `sql_mode` bends the same
                // values `INSERT IGNORE` bends. Same conversion, different
                // trigger: one is per-statement, the other per-session.
                // v7.39 (round 470) — a strict MySQL session refuses an
                // OMITTED NOT NULL column with its own error, not the
                // not-null one: MariaDB says `Field 'n' doesn't have a
                // default value` (1364), which is what a migration tool
                // branches on, where an explicit NULL is 1048. The generic
                // not-null check downstream cannot tell the two apart —
                // only here is it known that the column was never named.
                if mysql && !non_strict && !ignore && omitted && raw.is_null() && !col.nullable {
                    return Err(EngineError::Unsupported(alloc::format!(
                        "Field '{}' doesn't have a default value",
                        col.name
                    )));
                }
                // v7.39 (round 470) — non-strict bends a SUPPLIED value that
                // would not fit, and fills an OMITTED column; it does not
                // bend an explicit NULL, which MariaDB still rejects with
                // 1048 whatever the mode.
                let raw = if ignore || (non_strict && (omitted || !raw.is_null())) {
                    crate::conversions::mysql_ignore_fit(raw, col)
                } else {
                    raw
                };
                let coerced = coerce_value(raw, col.ty, &col.name, i)?;
                enforce_enum_label(enum_label_lookup, i, &col.name, &coerced)?;
                let coerced = canonicalize_set_value(set_variant_lookup, i, &col.name, coerced)?;
                let coerced = crate::conversions::truncate_to_column_fsp(coerced, col);
                check_unsigned_range(&coerced, col, i)?;
                out.push(coerced);
            }
            out
        } else {
            // 1-1 mapping fast path: single Vec alloc, no raw_tuple.
            // v7.38 (read01 sweep) — a short row (fewer values than columns, no
            // column list) fills its trailing columns from their DEFAULT / NULL.
            let mut out = Vec::with_capacity(schema_cols_len);
            let tuple_len = tuple.len();
            let mut tuple_iter = tuple.into_iter();
            for (i, col) in column_meta.iter().enumerate() {
                let mut raw = if i < tuple_len {
                    let e = tuple_iter.next().expect("i < tuple_len has a value");
                    if is_column_default_marker(&e) {
                        resolve_column_default_free(col, clock)?
                    } else {
                        literal_expr_to_value_in(e, catalog)?
                    }
                } else {
                    resolve_column_default_free(col, clock)?
                };
                if auto_increment_needs_value(col, &raw, mysql) {
                    let next = match auto_cursors.get(&i) {
                        Some(n) => *n,
                        None => auto_cursor_seed(table, i, col, seq_floors)?,
                    };
                    auto_cursors.insert(i, next + 1);
                    if first_auto.is_none() {
                        *first_auto = Some(next);
                    }
                    raw = Value::BigInt(next);
                } else if mysql
                    && col.auto_increment
                    && let Some(n) = explicit_auto_value(&raw)
                {
                    // v7.39 (round 433) — an explicit value inside a
                    // multi-row INSERT raises the counter for the LATER rows
                    // of the same statement, measured on MariaDB 11:
                    // `(NULL,·),(7,·),(NULL,·)` yields 1, 7, 8. SPG derives
                    // the next value from the table's current max, which has
                    // not moved yet mid-statement, so the third row used to
                    // land on 2 — a value the statement had already handed
                    // out on a wider table, and a silent drift from MySQL's
                    // ids either way. A LOWER explicit value never pulls the
                    // counter back (`(50,·),(3,·),(NULL,·)` yields 51), so
                    // the seed still comes from the table.
                    let cursor = match auto_cursors.get(&i) {
                        Some(c) => *c,
                        None => auto_cursor_seed(table, i, col, seq_floors)?,
                    };
                    auto_cursors.insert(i, cursor.max(n.saturating_add(1)));
                }
                let raw =
                    crate::conversions::normalize_composite_for_column(raw, col, catalog)?;
                let raw = crate::conversions::mysql_bytes_for_column(raw, col.ty, mysql);
                // v7.39 (round 434) — `INSERT IGNORE` bends a value that
                // would otherwise raise, so a MySQL bulk load never stops
                // mid-file. Values the ordinary path accepts are untouched.
                // v7.39 (round 470) — a non-strict `sql_mode` bends the same
                // values `INSERT IGNORE` bends. Same conversion, different
                // trigger: one is per-statement, the other per-session.
                // v7.39 (round 470) — the positional path names every column,
                // so nothing here is omitted and an explicit NULL stays an
                // explicit NULL.
                let raw = if ignore || (non_strict && !raw.is_null()) {
                    crate::conversions::mysql_ignore_fit(raw, col)
                } else {
                    raw
                };
                let coerced = coerce_value(raw, col.ty, &col.name, i)?;
                enforce_enum_label(enum_label_lookup, i, &col.name, &coerced)?;
                let coerced = canonicalize_set_value(set_variant_lookup, i, &col.name, coerced)?;
                let coerced = crate::conversions::truncate_to_column_fsp(coerced, col);
                check_unsigned_range(&coerced, col, i)?;
                out.push(coerced);
            }
            out
        };
        all_values.push(values);
    }
    Ok(all_values)
}

/// Stage 3 — insert the surviving rows under a mutable table borrow,
/// firing BEFORE / AFTER row triggers (which may rewrite or skip a row
/// and emit deferred embedded SQL), then apply the queued ON CONFLICT
/// DO UPDATE rewrites. Returns the RETURNING projection rows, the
/// deferred trigger statements, and the affected-row count.
// v7.37.15 Phase C — the `xmin` parameter below carries the writer
// version every row in this batch stamps on its `xmin`. The caller
// pre-fetches it via `Engine::writer_version_for_current_stmt` so
// all rows in the same statement share xmin (atomic commit at the
// tx boundary).
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
/// v7.39 (round 130) — one per action a MERGE actually took, captured so a
/// trailing `RETURNING` can project `merge_action()` / `OLD.*` / `NEW.*` / the
/// target+source aliases after the plan applies.
struct MergeRetRecord {
    action: &'static str,
    /// The row as `t.*` sees it: NEW for UPDATE/INSERT, the deleted row for DELETE.
    target_final: Vec<Value<'static>>,
    /// Pre-image (UPDATE/DELETE); `None` (→ NULL block) for INSERT.
    old: Option<Vec<Value<'static>>>,
    /// Post-image (UPDATE/INSERT); `None` (→ NULL block) for DELETE.
    new: Option<Vec<Value<'static>>>,
    /// The source row that drove this action (for `s.*` references).
    source: Vec<Value<'static>>,
}

/// v7.39 (round 427) — how MySQL counts a row an upsert resolved by
/// UPDATING an existing one. Measured on MariaDB 11, per row:
///
/// | shape                                    | ON DUPLICATE | REPLACE |
/// |------------------------------------------|--------------|---------|
/// | no conflict (plain insert)               | 1            | 1       |
/// | conflict, row CHANGED                    | 2            | 2       |
/// | conflict, row identical to what was there| 0            | 1       |
///
/// (`ON DUPLICATE` counts the update as delete+insert only when it really
/// changed something; `REPLACE` still charges for the insert either way.)
/// `None` is the PG dialect, where every affected row counts 1.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MysqlUpsertCount {
    OnDuplicate,
    Replace,
}

fn insert_parsed_rows(
    table: &mut spg_storage::Table,
    xmin: u64,
    // v7.39 (round 427) — MySQL's per-row upsert accounting; None keeps
    // PG's "one per affected row".
    mysql_upsert: Option<MysqlUpsertCount>,
    // v7.37.15 (Phase C.3, step 4b) — in-place kill switch, threaded
    // from the caller (fetched before the table mut borrow). When on,
    // the ON CONFLICT DO UPDATE pass below tombstones + inserts instead
    // of in-place update_row. Default OFF → legacy update_row.
    inplace: bool,
    all_values: Vec<Vec<Value<'static>>>,
    pending_updates: Vec<(usize, Vec<Value<'static>>, Vec<Value<'static>>)>,
    before_insert_triggers: &[(
        spg_storage::FunctionDef,
        alloc::string::String,
        alloc::string::String,
    )],
    after_insert_triggers: &[(
        spg_storage::FunctionDef,
        alloc::string::String,
        alloc::string::String,
    )],
    column_meta: &[ColumnSchema],
    table_name: &str,
    trigger_session_cfg: Option<&str>,
    returning_enabled: bool,
) -> Result<
    (
        Vec<Vec<Value<'static>>>,
        Vec<triggers::DeferredEmbeddedStmt>,
        usize,
        // v7.37.17 (E4 r3) — (old, new) RowId pairs from the
        // ON CONFLICT DO UPDATE tombstone+insert branch, for the
        // RC rebase's pair-atomic conflict handling.
        Vec<(
            spg_storage::row_header::RowId,
            spg_storage::row_header::RowId,
        )>,
        // v7.39 (r129) — per-returning-row OLD image, aligned 1:1 with the
        // returning rows: a NULL block for a plain insert, the pre-update row
        // for an ON CONFLICT DO UPDATE. Empty when RETURNING is off.
        Vec<Vec<Value<'static>>>,
    ),
    EngineError,
> {
    let arity = column_meta.len();
    let mut affected = 0usize;
    let mut oc_pairs: Vec<(
        spg_storage::row_header::RowId,
        spg_storage::row_header::RowId,
    )> = Vec::new();
    // v7.9.4 — keep RETURNING projection rows separate per
    // INSERT and per UPDATE branch so DO UPDATE pushes the new
    // post-update state, not the incoming-only values.
    let mut returning_rows: Vec<Vec<Value<'static>>> = Vec::new();
    // v7.39 (r129) — OLD image aligned with returning_rows (see return type).
    let mut old_images: Vec<Vec<Value<'static>>> = Vec::new();
    // v7.12.7 — collect embedded SQL emitted by any trigger
    // fire across the row loop; engine drains the queue after
    // the table mut borrow drops.
    let mut deferred_embedded: Vec<triggers::DeferredEmbeddedStmt> = Vec::new();
    'rowloop: for values in all_values {
        let mut row = Row::new(values);
        // v7.12.4 — BEFORE INSERT row-level triggers. Each
        // trigger may rewrite NEW cells (e.g. populate
        // `search_vector := to_tsvector(...)`) and may return
        // NULL to skip the row entirely.
        for (fd, when, tgname) in before_insert_triggers {
            // v7.39 (round 138) — WHEN filter over the incoming NEW row.
            if !triggers::trigger_when_holds(when, Some(&row), None, column_meta)? {
                continue;
            }
            let (outcome, deferred) = triggers::fire_row_trigger(
                fd,
                Some(row.clone()),
                None,
                table_name,
                column_meta,
                &[],
                trigger_session_cfg,
                false,
                &triggers::TgMeta {
                    op: "INSERT",
                    name: tgname,
                    level: "ROW",
                },
            )
            .map_err(|e| EngineError::Storage(StorageError::Corrupt(alloc::format!("{e}"))))?;
            deferred_embedded.extend(deferred);
            match outcome {
                triggers::TriggerOutcome::Row(r) => row = r,
                triggers::TriggerOutcome::Skip => continue 'rowloop,
            }
        }
        // v7.39 (read01 round 82) — a stored generated column is computed AFTER
        // the BEFORE trigger runs, not before: PG's order is BEFORE trigger →
        // generated columns → write. The pre-loop `apply_generated_stored_columns`
        // saw only the incoming NEW, so `w GENERATED AS (v*2)` kept the value for
        // the ORIGINAL v when a BEFORE trigger changed `NEW.v`. Recompute here
        // over the trigger's output. (No-op when there are no generated columns.)
        if !before_insert_triggers.is_empty() {
            let mut one = [core::mem::take(&mut row.values)];
            apply_generated_stored_columns(column_meta, &mut one)?;
            row.values = core::mem::take(&mut one[0]);
        }
        if returning_enabled {
            returning_rows.push(row.values.clone());
            // A plain insert has no prior row — OLD is all-NULL.
            old_images.push(alloc::vec![Value::Null; arity]);
        }
        // v7.12.4 — clone for the AFTER trigger view; insert
        // moves the row into the table.
        let inserted = row.clone();
        // v7.37.15 Phase C — every row in this insert statement
        // shares the caller-supplied xmin so concurrent readers
        // see all the rows commit together (autocommit) or stay
        // hidden until the enclosing tx COMMIT (explicit tx).
        table.insert_with_xmin(row, xmin)?;
        affected += 1;
        // v7.12.4 — AFTER INSERT row-level triggers fire post-
        // write. Return value is ignored (PG semantics); we
        // surface any error from the body up to the caller.
        for (fd, when, tgname) in after_insert_triggers {
            // v7.39 (round 138) — WHEN filter over the inserted NEW row.
            if !triggers::trigger_when_holds(when, Some(&inserted), None, column_meta)? {
                continue;
            }
            let (_outcome, deferred) = triggers::fire_row_trigger(
                fd,
                Some(inserted.clone()),
                None,
                table_name,
                column_meta,
                &[],
                trigger_session_cfg,
                true,
                &triggers::TgMeta {
                    op: "INSERT",
                    name: tgname,
                    level: "ROW",
                },
            )
            .map_err(|e| EngineError::Storage(StorageError::Corrupt(alloc::format!("{e}"))))?;
            deferred_embedded.extend(deferred);
        }
    }
    // v7.9.9 — apply ON CONFLICT DO UPDATE rewrites collected
    // in the conflict-resolution pass. update_row handles
    // index maintenance + body re-encoding.
    for (pos, new_row, old_row) in pending_updates {
        // v7.39 (round 427) — MySQL charges an upsert-resolved-by-update as
        // delete+insert (2) only when the row really changed; an ON
        // DUPLICATE that rewrote identical values counts 0, while a REPLACE
        // still charges 1 for its insert. Compute before `old_row` moves.
        let row_changed = new_row != old_row;
        if returning_enabled {
            returning_rows.push(new_row.clone());
            // DO UPDATE: OLD is the pre-update conflicting row.
            old_images.push(old_row);
        }
        if inplace {
            // MVCC: tombstone the conflicting old version (xmax = xmin)
            // + append the DO UPDATE result as a new version (xmin).
            let old_rid = table.rowids().get(pos).copied();
            let _ = table.mark_row_deleted(pos, xmin);
            table
                .insert_with_xmin(Row::new(new_row), xmin)
                .map_err(EngineError::Storage)?;
            if let (Some(o), Some(n)) = (
                old_rid,
                table
                    .rowids()
                    .get(table.rowids().len().wrapping_sub(1))
                    .copied(),
            ) {
                oc_pairs.push((o, n));
            }
        } else {
            table.update_row(pos, new_row)?;
        }
        affected += match (mysql_upsert, row_changed) {
            (Some(_), true) => 2,
            (Some(MysqlUpsertCount::OnDuplicate), false) => 0,
            (Some(MysqlUpsertCount::Replace), false) => 1,
            (None, _) => 1,
        };
    }
    Ok((
        returning_rows,
        deferred_embedded,
        affected,
        oc_pairs,
        old_images,
    ))
}

/// v7.39 (SQLSTATE fidelity) — PG's full 23502 message needs the
/// relation name the storage error can't carry; DML entry points
/// wrap their results through this.
fn enrich_not_null(e: EngineError, table: &str) -> EngineError {
    match e {
        EngineError::Storage(spg_storage::StorageError::NullInNotNull { column }) => {
            EngineError::Unsupported(alloc::format!(
                "null value in column \"{column}\" of relation \"{table}\" \
                 violates not-null constraint"
            ))
        }
        other => other,
    }
}

/// v7.39 (round 150) — relation reads of a CTE body, for the
/// no-RETURNING-reference guard. Modifying bodies reuse the deep
/// SELECT walker via a synthetic SELECT carrying their expression
/// slots (the acl privilege pass uses the same trick), so subquery
/// references are seen too.
pub(crate) fn collect_cte_body_reads(
    body: &spg_sql::ast::CteBody,
    into: &mut alloc::collections::BTreeSet<String>,
) {
    match body {
        spg_sql::ast::CteBody::Select(s) => crate::acl::collect_read_tables(s, into),
        spg_sql::ast::CteBody::Insert(b) => collect_insert_reads(b, into),
        spg_sql::ast::CteBody::Update(b) => collect_update_reads(b, into),
        spg_sql::ast::CteBody::Delete(b) => collect_delete_reads(b, into),
        spg_sql::ast::CteBody::Merge(b) => collect_merge_reads(b, into),
    }
}

fn push_items(sub: &mut spg_sql::ast::SelectStatement, items: &[SelectItem]) {
    for item in items {
        sub.items.push(item.clone());
    }
}

fn push_expr(sub: &mut spg_sql::ast::SelectStatement, e: &spg_sql::ast::Expr) {
    sub.items.push(SelectItem::Expr {
        expr: e.clone(),
        alias: None,
    });
}

pub(crate) fn collect_insert_reads(
    i: &spg_sql::ast::InsertStatement,
    into: &mut alloc::collections::BTreeSet<String>,
) {
    let mut sub = spg_sql::ast::SelectStatement::default();
    for row in &i.rows {
        for e in row {
            push_expr(&mut sub, e);
        }
    }
    if let Some(oc) = &i.on_conflict
        && let spg_sql::ast::OnConflictAction::Update {
            assignments,
            where_,
        } = &oc.action
    {
        for (_, e) in assignments {
            push_expr(&mut sub, e);
        }
        if let Some(w) = where_ {
            push_expr(&mut sub, w);
        }
    }
    if let Some(r) = &i.returning {
        push_items(&mut sub, r);
    }
    crate::acl::collect_read_tables(&sub, into);
    if let Some(src) = &i.select_source {
        crate::acl::collect_read_tables(src, into);
    }
}

pub(crate) fn collect_update_reads(
    u: &spg_sql::ast::UpdateStatement,
    into: &mut alloc::collections::BTreeSet<String>,
) {
    let mut sub = spg_sql::ast::SelectStatement::default();
    for (_, e) in &u.assignments {
        push_expr(&mut sub, e);
    }
    sub.where_ = u.where_.clone();
    if let Some(r) = &u.returning {
        push_items(&mut sub, r);
    }
    crate::acl::collect_read_tables(&sub, into);
}

pub(crate) fn collect_delete_reads(
    d: &spg_sql::ast::DeleteStatement,
    into: &mut alloc::collections::BTreeSet<String>,
) {
    let mut sub = spg_sql::ast::SelectStatement::default();
    sub.where_ = d.where_.clone();
    if let Some(r) = &d.returning {
        push_items(&mut sub, r);
    }
    crate::acl::collect_read_tables(&sub, into);
}

pub(crate) fn collect_merge_reads(
    m: &spg_sql::ast::MergeStatement,
    into: &mut alloc::collections::BTreeSet<String>,
) {
    if let Some(src) = &m.source_select {
        crate::acl::collect_read_tables(src, into);
    } else {
        into.insert(m.source.clone());
    }
    let mut sub = spg_sql::ast::SelectStatement::default();
    push_expr(&mut sub, &m.on);
    for clause in &m.clauses {
        if let Some(c) = &clause.condition {
            push_expr(&mut sub, c);
        }
        match &clause.action {
            spg_sql::ast::MergeAction::Insert { values, .. } => {
                for v in values {
                    push_expr(&mut sub, v);
                }
            }
            spg_sql::ast::MergeAction::Update { assignments } => {
                for (_, e) in assignments {
                    push_expr(&mut sub, e);
                }
            }
            spg_sql::ast::MergeAction::Delete | spg_sql::ast::MergeAction::DoNothing => {}
        }
    }
    if let Some(r) = &m.returning {
        push_items(&mut sub, r);
    }
    crate::acl::collect_read_tables(&sub, into);
}

// ---------------------------------------------------------------------------
// v7.39 (round 157) — write-path CTE-shadows-table support. The write-path
// CTE machinery installs temps on the LIVE catalog, so a CTE that shadows a
// real table gets a renamed temp (`__cte_shadow_<name>`) and every READ
// reference to the name is rewritten to it, honouring PG's WITH scoping:
//   * the shadowing CTE's own (non-recursive) body still sees the real
//     table (probe P2/P8); a RECURSIVE self-reference is the CTE (P6);
//   * later sibling bodies and the outer statement see the CTE (P1-P5);
//   * DML TARGETS are never renamed — they resolve to real relations only
//     (probe P6/P7: `INSERT INTO t … FROM t` writes the table, reads the CTE);
//   * a nested WITH that REDEFINES the name owns it for its whole subtree.
// After rewriting, the caller re-collects the statement's read set: any
// surviving reference to the old name means the rewriter missed a spot —
// fail with the historic honest error instead of silently reading the table.

/// The historic honest error for an unsupported shadow shape.
fn cte_shadow_err(name: &str) -> EngineError {
    EngineError::Unsupported(alloc::format!(
        "CTE name {name:?} shadows an existing table; rename the CTE"
    ))
}

/// v7.39 (round 157) — post-rewrite verification: any surviving read of a
/// renamed (shadowed) name means the rewriter missed a reference — fail
/// honestly instead of silently reading the real table.
fn shadow_rename_leak_check(
    renames: &[(String, String)],
    reads: &alloc::collections::BTreeSet<String>,
) -> Result<(), EngineError> {
    for (old, _) in renames {
        if reads.iter().any(|t| t.eq_ignore_ascii_case(old)) {
            return Err(cte_shadow_err(old));
        }
    }
    Ok(())
}

fn rename_rel_in_table_ref(t: &mut spg_sql::ast::TableRef, old: &str, new: &str) {
    if let Some(sub) = &mut t.lateral_subquery {
        rename_rel_in_select(sub, old, new);
        return;
    }
    let synthetic = t.unnest_expr.is_some()
        || t.generate_series_args.is_some()
        || t.jsonb_each_text_arg.is_some()
        || t.table_fn_call.is_some();
    if synthetic {
        // The synthetic sources aren't tables, but their argument
        // expressions may carry subqueries that read the name.
        if let Some(e) = &mut t.unnest_expr {
            rename_rel_in_expr(e, old, new);
        }
        if let Some(args) = &mut t.generate_series_args {
            for a in args {
                rename_rel_in_expr(a, old, new);
            }
        }
        if let Some((_, e)) = &mut t.jsonb_each_text_arg {
            rename_rel_in_expr(e, old, new);
        }
        if let Some(b) = &mut t.table_fn_call {
            for a in &mut b.1 {
                rename_rel_in_expr(a, old, new);
            }
        }
        return;
    }
    if t.name.eq_ignore_ascii_case(old) {
        // Keep the user-visible alias: an unaliased table ref is referred
        // to by its name in column qualifiers, which must keep resolving.
        if t.alias.is_none() {
            t.alias = Some(t.name.clone());
        }
        t.name = new.into();
    }
}

fn rename_rel_in_expr(e: &mut spg_sql::ast::Expr, old: &str, new: &str) {
    use spg_sql::ast::Expr;
    crate::expr_analysis::rewrite_nodes_mut(e, &mut |n| match n {
        Expr::ScalarSubquery(s) => {
            rename_rel_in_select(s, old, new);
            true
        }
        Expr::Exists { subquery, .. } => {
            rename_rel_in_select(subquery, old, new);
            true
        }
        Expr::InSubquery { expr, subquery, .. } => {
            rename_rel_in_expr(expr, old, new);
            rename_rel_in_select(subquery, old, new);
            true
        }
        Expr::RowInSubquery { row, subquery, .. } | Expr::RowCmpSubquery { row, subquery, .. } => {
            for x in row {
                rename_rel_in_expr(x, old, new);
            }
            rename_rel_in_select(subquery, old, new);
            true
        }
        _ => false,
    });
}

pub(crate) fn rename_rel_in_select(s: &mut spg_sql::ast::SelectStatement, old: &str, new: &str) {
    // A nested WITH redefining the name owns it for this whole subtree.
    if s.ctes.iter().any(|c| c.name.eq_ignore_ascii_case(old)) {
        return;
    }
    for cte in &mut s.ctes {
        rename_rel_in_cte_body(&mut cte.body, old, new);
    }
    if let Some(from) = &mut s.from {
        rename_rel_in_table_ref(&mut from.primary, old, new);
        for j in &mut from.joins {
            rename_rel_in_table_ref(&mut j.table, old, new);
            if let Some(on) = &mut j.on {
                rename_rel_in_expr(on, old, new);
            }
        }
    }
    for item in &mut s.items {
        if let SelectItem::Expr { expr, .. } = item {
            rename_rel_in_expr(expr, old, new);
        }
    }
    if let Some(w) = &mut s.where_ {
        rename_rel_in_expr(w, old, new);
    }
    if let Some(h) = &mut s.having {
        rename_rel_in_expr(h, old, new);
    }
    if let Some(gs) = &mut s.group_by {
        for g in gs {
            rename_rel_in_expr(g, old, new);
        }
    }
    for o in &mut s.order_by {
        rename_rel_in_expr(&mut o.expr, old, new);
    }
    for (_, peer) in &mut s.unions {
        rename_rel_in_select(peer, old, new);
    }
}

fn rename_rel_in_returning(items: &mut Option<Vec<SelectItem>>, old: &str, new: &str) {
    if let Some(items) = items {
        for it in items.iter_mut() {
            if let SelectItem::Expr { expr, .. } = it {
                rename_rel_in_expr(expr, old, new);
            }
        }
    }
}

pub(crate) fn rename_rel_in_insert(i: &mut InsertStatement, old: &str, new: &str) {
    // `i.table` is the DML target — never renamed.
    for row in &mut i.rows {
        for e in row {
            rename_rel_in_expr(e, old, new);
        }
    }
    if let Some(src) = &mut i.select_source {
        rename_rel_in_select(src, old, new);
    }
    if let Some(oc) = &mut i.on_conflict
        && let spg_sql::ast::OnConflictAction::Update {
            assignments,
            where_,
        } = &mut oc.action
    {
        for (_, e) in assignments {
            rename_rel_in_expr(e, old, new);
        }
        if let Some(w) = where_ {
            rename_rel_in_expr(w, old, new);
        }
    }
    rename_rel_in_returning(&mut i.returning, old, new);
}

pub(crate) fn rename_rel_in_update(u: &mut spg_sql::ast::UpdateStatement, old: &str, new: &str) {
    for (_, e) in &mut u.assignments {
        rename_rel_in_expr(e, old, new);
    }
    if let Some(w) = &mut u.where_ {
        rename_rel_in_expr(w, old, new);
    }
    rename_rel_in_returning(&mut u.returning, old, new);
}

pub(crate) fn rename_rel_in_delete(d: &mut spg_sql::ast::DeleteStatement, old: &str, new: &str) {
    if let Some(w) = &mut d.where_ {
        rename_rel_in_expr(w, old, new);
    }
    rename_rel_in_returning(&mut d.returning, old, new);
}

pub(crate) fn rename_rel_in_merge(m: &mut spg_sql::ast::MergeStatement, old: &str, new: &str) {
    // `m.target` is the DML target — never renamed. The SOURCE is a read.
    if let Some(src) = &mut m.source_select {
        rename_rel_in_select(src, old, new);
    } else if m.source.eq_ignore_ascii_case(old) {
        if m.source_alias.is_none() {
            m.source_alias = Some(m.source.clone());
        }
        m.source = new.into();
    }
    rename_rel_in_expr(&mut m.on, old, new);
    for cl in &mut m.clauses {
        if let Some(c) = &mut cl.condition {
            rename_rel_in_expr(c, old, new);
        }
        match &mut cl.action {
            spg_sql::ast::MergeAction::Insert { values, .. } => {
                for v in values {
                    rename_rel_in_expr(v, old, new);
                }
            }
            spg_sql::ast::MergeAction::Update { assignments } => {
                for (_, e) in assignments {
                    rename_rel_in_expr(e, old, new);
                }
            }
            spg_sql::ast::MergeAction::Delete | spg_sql::ast::MergeAction::DoNothing => {}
        }
    }
    rename_rel_in_returning(&mut m.returning, old, new);
}

pub(crate) fn rename_rel_in_cte_body(body: &mut spg_sql::ast::CteBody, old: &str, new: &str) {
    match body {
        spg_sql::ast::CteBody::Select(s) => rename_rel_in_select(s, old, new),
        spg_sql::ast::CteBody::Insert(b) => rename_rel_in_insert(b, old, new),
        spg_sql::ast::CteBody::Update(b) => rename_rel_in_update(b, old, new),
        spg_sql::ast::CteBody::Delete(b) => rename_rel_in_delete(b, old, new),
        spg_sql::ast::CteBody::Merge(b) => rename_rel_in_merge(b, old, new),
    }
}
