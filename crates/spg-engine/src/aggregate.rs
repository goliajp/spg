//! Aggregate executor.
//!
//! Handles `SELECT … <aggs> … [GROUP BY …]` queries. The planning strategy
//! is straightforward:
//!
//! 1. Walk the SELECT (and ORDER BY) expressions to find every aggregate
//!    function call. Dedupe by AST equality and assign each `__agg_<i>`.
//! 2. Same for every `GROUP BY` expression: assign `__grp_<j>`.
//! 3. Stream the WHERE-filtered rows, group by the tuple of GROUP BY
//!    values, and update per-group aggregate state.
//! 4. Materialise a synthetic per-group row containing
//!    `[__grp_0..__grp_K, __agg_0..__agg_N]` and rewrite the user's
//!    SELECT / ORDER BY expressions to reference those synthetic columns
//!    instead of the originals.
//! 5. Evaluate the rewritten expressions against the synthetic schema and
//!    emit results.
//!
//! v1.8 implements `count(*)`, `count(expr)`, `sum`, `min`, `max`, `avg`.
//! NULL semantics follow PG: aggregates skip NULL inputs (except
//! `count(*)`, which counts rows). `sum(int)` widens to `BigInt`;
//! `avg(int|bigint)` returns `Float`.

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_sql::ast::{Expr, SelectItem, SelectStatement};
use spg_storage::{ColumnSchema, DataType, Row, Value};

use crate::eval::{self, EvalContext, EvalError};
use crate::join::RowRef;

/// True if this statement should go through the aggregate path.
pub fn uses_aggregate(stmt: &SelectStatement) -> bool {
    if stmt.group_by.is_some() || stmt.having.is_some() {
        return true;
    }
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item
            && contains_aggregate(expr)
        {
            return true;
        }
    }
    for o in &stmt.order_by {
        if contains_aggregate(&o.expr) {
            return true;
        }
    }
    if let Some(h) = &stmt.having
        && contains_aggregate(h)
    {
        return true;
    }
    false
}

pub fn contains_aggregate(e: &Expr) -> bool {
    match e {
        Expr::FunctionCall { name, args } => {
            is_aggregate_name(name) || args.iter().any(contains_aggregate)
        }
        Expr::AggregateOrdered { .. } => true,
        Expr::Binary { lhs, rhs, .. } => contains_aggregate(lhs) || contains_aggregate(rhs),
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            contains_aggregate(expr)
        }
        Expr::Like { expr, pattern, .. } => contains_aggregate(expr) || contains_aggregate(pattern),
        Expr::Extract { source, .. } => contains_aggregate(source),
        // v4.10 subqueries + v4.12 window functions / Literal /
        // Column — all non-aggregate leaves from the regular
        // aggregate planner's POV. Window-bearing projections are
        // routed to exec_select_with_window before this runs.
        Expr::ScalarSubquery(_)
        | Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::WindowFunction { .. }
        | Expr::Literal(_)
        | Expr::Placeholder(_)
        | Expr::Column(_) => false,
        // v7.10.10 — recurse into array constructor / subscript /
        // ANY/ALL children. Aggregates inside `ARRAY[SUM(x)]` are
        // valid PG and must be detected here.
        Expr::Array(items) => items.iter().any(contains_aggregate),
        Expr::ArraySubscript { target, index } => {
            contains_aggregate(target) || contains_aggregate(index)
        }
        Expr::AnyAll { expr, array, .. } => contains_aggregate(expr) || contains_aggregate(array),
        Expr::InList { expr, list, .. } => {
            contains_aggregate(expr) || list.iter().any(contains_aggregate)
        }
        // v7.13.0 — CASE WHEN … END. Recurse into operand,
        // every (WHEN, THEN) pair, and the ELSE branch.
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            operand.as_deref().is_some_and(contains_aggregate)
                || branches
                    .iter()
                    .any(|(w, t)| contains_aggregate(w) || contains_aggregate(t))
                || else_branch.as_deref().is_some_and(contains_aggregate)
        }
    }
}

pub fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count"
            | "count_star"
            | "sum"
            | "min"
            | "max"
            | "avg"
            // v7.17.0 — variadic / collection aggregates. ORM
            // reports (Hibernate / Rails / Django) emit these in
            // GROUP BY rollups; pre-7.17 SPG hit "unknown
            // aggregate".
            | "string_agg"
            | "array_agg"
            // v7.17.0 — boolean aggregates. `every` is SQL-standard
            // alias for `bool_and`.
            | "bool_and"
            | "bool_or"
            | "every"
            // v7.32 (round-29) — statistical aggregates (every BI /
            // dashboard emits these in rollups).
            | "stddev" | "stddev_samp" | "stddev_pop"
            | "variance" | "var_samp" | "var_pop"
            // v7.32 (round-29) — bitwise aggregates.
            | "bit_and" | "bit_or" | "bit_xor"
            // v7.32 (round-29) — ordered-set aggregates (used with
            // `WITHIN GROUP (ORDER BY …)`).
            | "percentile_cont" | "percentile_disc" | "mode"
            // v7.32 (round-29) — hypothetical-set aggregates (also
            // `WITHIN GROUP`): the rank the direct args WOULD have.
            | "rank" | "dense_rank" | "percent_rank" | "cume_dist"
            // v7.32 (round-29) — two-argument regression family.
            | "covar_pop" | "covar_samp" | "corr"
            | "regr_count" | "regr_avgx" | "regr_avgy" | "regr_slope"
            | "regr_intercept" | "regr_r2" | "regr_sxx" | "regr_syy" | "regr_sxy"
            // v7.32 (round-29) — JSON aggregates.
            | "json_agg" | "jsonb_agg" | "json_object_agg" | "jsonb_object_agg"
    )
}

/// v7.32 (round-29) — two-argument regression aggregates `f(Y, X)`.
fn is_regression_name(name: &str) -> bool {
    matches!(
        name,
        "covar_pop"
            | "covar_samp"
            | "corr"
            | "regr_count"
            | "regr_avgx"
            | "regr_avgy"
            | "regr_slope"
            | "regr_intercept"
            | "regr_r2"
            | "regr_sxx"
            | "regr_syy"
            | "regr_sxy"
    )
}

/// v7.32 (round-29) — aggregates that consume a second positional
/// argument: `string_agg(v, sep)`, the regression family `f(Y, X)`, and
/// `json_object_agg(key, value)`.
fn agg_uses_second_arg(name: &str) -> bool {
    name == "string_agg"
        || name == "json_object_agg"
        || name == "jsonb_object_agg"
        || is_regression_name(name)
}

/// v7.32 (round-29) — ordered-set aggregates: the value to aggregate
/// comes from the `WITHIN GROUP (ORDER BY …)` sort spec, and any
/// in-parens arguments are *direct* arguments (the percentile fraction).
/// `mode()` takes no direct argument.
pub fn is_ordered_set_name(name: &str) -> bool {
    // v7.32 — `eq_ignore_ascii_case` instead of `to_ascii_lowercase()`:
    // these classifiers run in the aggregate row/group loop, where the
    // old per-call `String` allocation showed up as ~16% of the inbox's
    // aggregate path in a sampled profile (the names are constant).
    ["percentile_cont", "percentile_disc", "mode"]
        .iter()
        .any(|k| name.eq_ignore_ascii_case(k))
}

/// v7.32 (round-29) — hypothetical-set aggregates: `rank(args) WITHIN
/// GROUP (ORDER BY …)` and friends compute the rank the hypothetical
/// row would have. Like ordered-set, the value stream comes from the
/// sort spec and the in-parens args are direct (the hypothetical row).
pub fn is_hypothetical_set_name(name: &str) -> bool {
    ["rank", "dense_rank", "percent_rank", "cume_dist"]
        .iter()
        .any(|k| name.eq_ignore_ascii_case(k))
}

/// v7.32 (round-29) — every aggregate that takes its value stream from
/// a `WITHIN GROUP (ORDER BY …)` clause (ordered-set + hypothetical-set).
pub fn is_within_group_name(name: &str) -> bool {
    is_ordered_set_name(name) || is_hypothetical_set_name(name)
}

/// v7.37.4 (R34) — pre-computed aggregate kind. Replaces per-row
/// string matches in `update_state` with a single `match` on a
/// `Copy` enum (compiles to a jump table). For the mailrs prod
/// `/api/conversations` shape (14 aggregates × 100 k rows = 1.4 M
/// inner-loop iterations) this is the dominant per-row cost.
///
/// Lowered from `AggSpec::name` at spec build time via
/// [`classify_agg_name`]; populated by the three `AggSpec`
/// construction sites (window+ORDER, plain, `first_ordered`
/// `array_agg`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum AggKind {
    CountStar,
    Count,
    Sum,
    Avg,
    Min,
    Max,
    StringAgg,
    ArrayAgg,
    BoolAnd,
    BoolOr,
    /// stddev / stddev_samp / stddev_pop / variance / var_samp / var_pop.
    StddevFamily,
    BitAnd,
    BitOr,
    BitXor,
    /// ordered-set (`percentile_cont/disc`, `mode`) +
    /// hypothetical-set (`rank`/`dense_rank`/etc.) aggregates that
    /// share the WITHIN-GROUP collection path.
    WithinGroup,
    /// covar_samp / covar_pop / corr / regr_*.
    Regression,
    JsonAgg,
    JsonObjectAgg,
}

/// v7.37.4 (R34) — name → kind, called once per spec at build time.
/// Hot path (`update_state_kind`) only sees the enum; the canonical
/// string still travels with the spec so `finalize` and errors can
/// quote it.
fn classify_agg_name(name: &str) -> AggKind {
    match name {
        "count_star" => AggKind::CountStar,
        "count" => AggKind::Count,
        "sum" => AggKind::Sum,
        "avg" => AggKind::Avg,
        "min" => AggKind::Min,
        "max" => AggKind::Max,
        "string_agg" => AggKind::StringAgg,
        "array_agg" => AggKind::ArrayAgg,
        "bool_and" => AggKind::BoolAnd,
        "bool_or" => AggKind::BoolOr,
        "stddev" | "stddev_samp" | "stddev_pop" | "variance" | "var_samp" | "var_pop" => {
            AggKind::StddevFamily
        }
        "bit_and" => AggKind::BitAnd,
        "bit_or" => AggKind::BitOr,
        "bit_xor" => AggKind::BitXor,
        "json_agg" | "jsonb_agg" => AggKind::JsonAgg,
        "json_object_agg" | "jsonb_object_agg" => AggKind::JsonObjectAgg,
        n if is_within_group_name(n) => AggKind::WithinGroup,
        n if is_regression_name(n) => AggKind::Regression,
        other => panic!("classify_agg_name: unknown aggregate {other}"),
    }
}

/// Per-aggregate running state.
#[derive(Debug, Default, Clone)]
struct AggState {
    count: i64,
    sum_int: i64,
    sum_float: f64,
    extreme: Option<Value>,
    use_float: bool,
    /// v7.17.0 — running collection for string_agg / array_agg.
    /// Each entry is one row's contribution (NULL preserved as
    /// `Value::Null`; string_agg's finalize step drops them, but
    /// array_agg keeps them). Pushing in insertion order matches
    /// PG behaviour when no `ORDER BY` is given inside the
    /// aggregate call.
    items: Vec<Value>,
    /// v7.25 (round-17) — per-group dedupe set for DISTINCT
    /// aggregates (encoded values; NULLs never reach it because
    /// the caller's skip runs after the per-aggregate NULL rules).
    /// v7.37.4 measured `hashbrown::HashSet` as worse at this
    /// shape — the per-(group × distinct-spec) hash table alloc
    /// overhead beats the lookup-speed gain when each set is
    /// small. Sticking with `BTreeSet`; the dispatch-side enum
    /// fix in `update_state` is the R34 win.
    seen: BTreeSet<String>,
    /// v7.24 (round-16 A) — per-item ORDER BY key tuples, parallel
    /// to `items` (pushed under the same skip/keep conditions).
    /// Empty when the aggregate carries no internal ordering.
    item_keys: Vec<Vec<Value>>,
    /// v7.17.0 — captured separator for string_agg. PG accepts a
    /// non-constant separator expression but in practice every
    /// caller passes a literal; the engine snapshots the last
    /// non-NULL text it sees, which matches PG's "use the latest
    /// row's value" behaviour.
    separator: Option<String>,
    /// v7.17.0 — running boolean accumulator for bool_and /
    /// bool_or / every. `None` until the first non-NULL input;
    /// at finalize None → SQL NULL.
    bool_acc: Option<bool>,
    /// v7.32 (round-29) — sum of squares for the variance / stddev
    /// family (`sum_float` carries the running sum; `count` the n).
    sum_sq: f64,
    /// v7.32 (round-29) — running accumulator for bit_and / bit_or /
    /// bit_xor. `None` until the first non-NULL input → SQL NULL.
    bit_acc: Option<i64>,
    /// v7.32 (round-29) — two-argument regression family
    /// (`covar_*` / `corr` / `regr_*`), PG arg order `f(Y, X)`. Only
    /// rows where BOTH inputs are non-NULL contribute (`count` is the
    /// paired n, independent of the single-arg `sum_*`).
    reg_n: i64,
    reg_sx: f64,
    reg_sy: f64,
    reg_sxx: f64,
    reg_syy: f64,
    reg_sxy: f64,
    /// v7.32 (round-29) — second value stream for `json_object_agg`
    /// (`items` holds the keys, `aux_items` the values).
    aux_items: Vec<Value>,
    /// v7.33 (array_agg argmax) — for a `first_ordered` spec
    /// (`(array_agg(x ORDER BY y))[1]`), the running first-by-order
    /// (sort-key tuple, value). Replaced only when a new row's key sorts
    /// strictly before the current best (ties keep the earliest row, =
    /// the stable-sort `[1]`). No items/item_keys array is built.
    first_best: Option<(Vec<Value>, Value)>,
}

#[derive(Debug, Clone)]
struct AggSpec {
    name: String, // lowercased
    /// First argument (value expression) for every aggregate
    /// except `count(*)`. `None` for `count_star`.
    arg: Option<Expr>,
    /// v7.17.0 — second argument. Only `string_agg(value, sep)`
    /// uses it today. `None` for every other aggregate (or for
    /// `array_agg`, which is single-arg). Carried in the spec so
    /// per-row evaluation can re-use the same separator
    /// expression across calls.
    arg2: Option<Expr>,
    /// v7.25 (round-17) — `COUNT(DISTINCT x)` & friends: dedupe
    /// the input stream per group before accumulation.
    distinct: bool,
    /// v7.24 (round-16 A) — aggregate-internal ORDER BY keys
    /// (`array_agg(x ORDER BY y DESC NULLS LAST)`). Empty for the
    /// plain form. Only the collection aggregates honour it;
    /// other aggregates are order-insensitive and ignore it (PG
    /// accepts the syntax everywhere too).
    order_by: Vec<spg_sql::ast::OrderBy>,
    /// v7.32 (round-29) — `FILTER (WHERE cond)`: a per-row predicate
    /// evaluated against the source row before accumulation. A row
    /// whose `cond` is not TRUE (false or NULL) is excluded from this
    /// aggregate only. `None` for the unfiltered form.
    filter: Option<Expr>,
    /// v7.32 (round-29) — ordered-set aggregates only: the *direct*
    /// argument (the percentile fraction for `percentile_cont/disc`).
    /// PG requires it constant, so it is evaluated once. `None` for
    /// `mode()` and for every non-ordered-set aggregate.
    direct_arg: Option<Expr>,
    /// v7.33 (array_agg argmax) — set when this spec came from
    /// `(array_agg(x ORDER BY y))[1]`: accumulate only the first-by-order
    /// element (a running argmax/argmin) and finalise to that scalar
    /// value, instead of collecting + sorting + materialising the whole
    /// per-group array just to take element 1. Returns the element type,
    /// not the array type.
    first_ordered: bool,
    /// v7.37.4 (R34) — derived from `name` at spec build time so the
    /// per-row inner loop dispatches via a `match` on `Copy` enum
    /// instead of a string compare for every (row × aggregate)
    /// iteration.
    kind: AggKind,
}

/// Output of running the aggregate path. Schema describes one row per
/// group; rows are not yet ORDER BY-sorted (caller does it).
#[derive(Debug)]
pub struct AggResult {
    pub columns: Vec<ColumnSchema>,
    pub rows: Vec<Row>,
    /// v7.31 (perf — PG lesson #1, post-LIMIT subquery projection):
    /// select-list items whose rewritten expr carries a subquery and
    /// is referenced by neither ORDER BY nor HAVING. Their output
    /// cells hold NULL placeholders; the caller truncates to
    /// LIMIT+OFFSET first and only then evaluates these for the
    /// surviving rows (PG runs the same shape with SubPlan loops=50
    /// instead of loops=24000). `(output_col, rewritten_expr)`.
    pub deferred: Vec<(usize, Expr)>,
    /// Synthetic group rows aligned 1:1 with `rows`; populated only
    /// when `deferred` is non-empty.
    pub synth_rows: Vec<Row>,
    /// Schema the deferred exprs evaluate against.
    pub synth_schema: Vec<ColumnSchema>,
}

/// Execute aggregate logic against an already-WHERE-filtered iterator of
/// rows. `table_alias` is the alias accepted by column resolution.
#[allow(clippy::too_many_lines)]
/// v7.25.2 (round-19 A) — caller-injected evaluator for synth-row
/// expressions that still carry subquery nodes after the rewrite
/// (correlated subqueries in the select list / HAVING / aggregate
/// ORDER BY of a GROUP BY query). The engine passes its
/// correlated-aware evaluator; pure-library callers pass None and
/// surviving subqueries keep erroring loudly.
pub type CorrelatedEval<'a> = &'a dyn Fn(&Expr, &Row, &EvalContext<'_>) -> Result<Value, EvalError>;

/// Output of the per-group projection stage (`project_groups`): the
/// output schema, the projected rows, the synth rows kept alongside
/// them for post-LIMIT deferred evaluation, the deferred subquery
/// items, and the rewritten ORDER BY exprs (shared with the sort).
struct Projection {
    columns: Vec<ColumnSchema>,
    out_rows: Vec<Row>,
    kept_synth: Vec<Row>,
    deferred: Vec<(usize, Expr)>,
    order_rewritten: Vec<Expr>,
    /// v7.37.x — when `defer_projection` is requested, `out_rows`
    /// carries empty placeholders and the caller runs the per-item
    /// eval pass after sort+truncate over the surviving ≤ keep_n
    /// rows. `None` when projection was performed inline.
    deferred_project: Option<DeferredProject>,
}

struct DeferredProject {
    items_rewritten: Vec<Option<Expr>>,
    items_compiled: Vec<Option<eval::CompiledExpr>>,
}

/// v7.35.0 — detect the `SELECT COUNT(*) FROM … [WHERE …]` shape
/// (single item, no GROUP BY / HAVING / ORDER BY / DISTINCT /
/// LIMIT WITH TIES / FILTER / window). For this shape the answer
/// is exactly `rows.len()` as `BigInt`, no group state needed.
/// Returns `None` for any deviation so the caller's full pipeline
/// runs verbatim.
///
/// v7.35.2 — also short-circuit `COUNT(<literal>)` (e.g.
/// `COUNT(1)`) and `COUNT(<column>)` when the column is declared
/// NOT NULL on the input schema. PG handles both cases as
/// `COUNT(*)` (the non-null filter is a no-op), so doing the same
/// here keeps every `count this thing` shape on the same fast path
/// instead of routing the literal / non-null-col variants through
/// the four-stage aggregate pipeline.
fn try_pure_count_star_short_circuit(
    stmt: &SelectStatement,
    rows: &[RowRef<'_>],
    schema_cols: &[ColumnSchema],
    table_alias: Option<&str>,
) -> Option<AggResult> {
    if stmt.distinct
        || stmt.limit_with_ties
        || stmt.group_by.is_some()
        || stmt.having.is_some()
        || !stmt.order_by.is_empty()
    {
        return None;
    }
    if stmt.items.len() != 1 {
        return None;
    }
    let SelectItem::Expr { expr, alias } = &stmt.items[0] else {
        return None;
    };
    let Expr::FunctionCall { name, args } = expr else {
        return None;
    };
    if !name.eq_ignore_ascii_case("count") && !name.eq_ignore_ascii_case("count_star") {
        return None;
    }
    let count_star_shape = match args.as_slice() {
        // `COUNT(*)` parses to `count_star` with no args.
        [] if name.eq_ignore_ascii_case("count_star") => true,
        // `COUNT(<literal>)` — the per-row test is "is this literal
        // non-null?" which is constant, so it's COUNT(*) when the
        // literal is non-null.
        [Expr::Literal(lit)] => !matches!(lit, spg_sql::ast::Literal::Null),
        // `COUNT(<column>)` — same answer as COUNT(*) when the
        // column is statically declared NOT NULL on the input
        // schema. Resolve through the alias if one is set.
        [Expr::Column(c)] => {
            if let Some(q) = c.qualifier.as_deref()
                && let Some(alias) = table_alias
                && !q.eq_ignore_ascii_case(alias)
            {
                return None;
            }
            schema_cols
                .iter()
                .find(|s| s.name.eq_ignore_ascii_case(&c.name))
                .is_some_and(|s| !s.nullable)
        }
        _ => return None,
    };
    if !count_star_shape {
        return None;
    }
    let col_name = alias.clone().unwrap_or_else(|| "count".to_string());
    let count = i64::try_from(rows.len()).unwrap_or(i64::MAX);
    Some(AggResult {
        columns: alloc::vec![ColumnSchema::new(col_name, DataType::BigInt, false)],
        rows: alloc::vec![Row::new(alloc::vec![Value::BigInt(count)])],
        deferred: Vec::new(),
        synth_rows: Vec::new(),
        synth_schema: Vec::new(),
    })
}

pub(crate) fn run(
    stmt: &SelectStatement,
    rows: &[RowRef<'_>],
    schema_cols: &[ColumnSchema],
    table_alias: Option<&str>,
    correlated_eval: Option<CorrelatedEval<'_>>,
) -> Result<AggResult, EvalError> {
    // v7.35.0 — pure `SELECT COUNT(*) FROM … WHERE …` short-circuit.
    // The caller already filtered rows by WHERE (we run on the
    // post-WHERE survivor set), so for the canonical pure-COUNT(*)
    // shape (no GROUP BY / HAVING / ORDER BY / DISTINCT / FILTER /
    // window) the answer is simply `rows.len()`. The four-stage
    // aggregate pipeline below (accumulate_groups → build_synth_schema
    // → finalize_synth_rows → project_groups) collapses to a single
    // BigInt cell when there's a single group, but each stage still
    // pays its own allocation tax — group state map, synth schema
    // vec, finalize loop. `exists_in_60` (mailrs prod #4 baseline)
    // is exactly this shape on a 25 k-row JOIN.
    if let Some(short) = try_pure_count_star_short_circuit(stmt, rows, schema_cols, table_alias) {
        return Ok(short);
    }
    let group_exprs: Vec<Expr> = stmt.group_by.clone().unwrap_or_default();

    // Collect aggregate sub-expressions across items + order_by.
    let mut agg_specs: Vec<AggSpec> = Vec::new();
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            collect_aggregates(expr, &mut agg_specs);
        }
    }
    for o in &stmt.order_by {
        collect_aggregates(&o.expr, &mut agg_specs);
    }
    if let Some(h) = &stmt.having {
        collect_aggregates(h, &mut agg_specs);
    }
    // v7.17.0 — arity validation. The collector tolerates an
    // arbitrary positional-arg count; here we enforce the
    // per-aggregate contract so a malformed call (e.g.
    // `array_agg()` or `string_agg(x)`) surfaces as a SQL error
    // rather than silently coercing to a degenerate aggregate.
    validate_agg_arities(stmt, &agg_specs)?;
    validate_within_group(&agg_specs)?;

    // (1) Stream the WHERE-filtered rows into insertion-ordered group state.
    let order = accumulate_groups(
        rows,
        &group_exprs,
        &agg_specs,
        schema_cols,
        table_alias,
        correlated_eval,
    )?;

    // (2) Build the synthetic per-group schema and finalise each group's row.
    let synth_schema =
        build_synth_schema(rows, &group_exprs, &agg_specs, schema_cols, table_alias)?;
    let synth_rows = finalize_synth_rows(
        &order,
        &agg_specs,
        &synth_schema,
        rows,
        schema_cols,
        table_alias,
    )?;

    // v7.37.x (mailrs Track A 100k attack) — defer the bound
    // per-item SELECT projection on the synth rows until AFTER
    // sort + LIMIT truncation. On a `GROUP BY t ORDER BY agg DESC
    // LIMIT 50` with 20 000 groups (the mailrs minimal 100k shape)
    // pre-defer ran 20 000 × N_items compiled-VM evals + Row
    // allocations before discarding 99.75 % at the sort truncation
    // step. HAVING still runs inline on every group because it
    // filters BEFORE the LIMIT; we only skip the SELECT-list eval.
    let defer_projection = !stmt.order_by.is_empty()
        && !stmt.distinct
        && !stmt.limit_with_ties
        && stmt.having.is_none()
        && stmt.limit_literal().is_some_and(|l| {
            let off = stmt.offset_literal().unwrap_or(0) as usize;
            let k = (l as usize).saturating_add(off);
            k > 0 && k < synth_rows.len()
        });

    // (3) Rewrite the user's expressions, filter groups by HAVING and project.
    let Projection {
        columns,
        mut out_rows,
        mut kept_synth,
        deferred,
        order_rewritten,
        deferred_project,
    } = project_groups(
        synth_rows,
        stmt,
        &group_exprs,
        &agg_specs,
        &synth_schema,
        correlated_eval,
        defer_projection,
    )?;

    // (4) ORDER BY on the aggregated output (the caller applies LIMIT).
    //
    // v7.37.3 (mailrs prod /api/contacts 3.21× regression — and the
    // general inbox-listing-shape SPG-vs-PG gap) — top-K sink for
    // `ORDER BY <agg> [DESC] LIMIT k`. Pre-7.37.3 this stage ran a
    // full O(N log N) sort over every surviving group, then the
    // caller truncated to `k`. With high-cardinality GROUP BY (a
    // sender column with hundreds-thousands of distinct values) the
    // truncated set is a tiny fraction of `N` — keep an O(k) top-K
    // sink and never sort the discarded majority. Matches PG /
    // MySQL / MariaDB's standard "LIMIT k under ORDER BY agg"
    // optimisation; SPG previously implemented it only on the
    // streamed inner-join path (`try_streamed_inner_join_topn`)
    // and not on the aggregate output.
    //
    // Gate: needs a literal LIMIT (placeholder LIMIT we can't bound
    // statically here), no DISTINCT (would need post-dedup, can't
    // truncate during sort), no LIMIT WITH TIES (which extends past
    // the literal k by run-time tie-key comparison).
    let keep_n: Option<usize> =
        if !stmt.order_by.is_empty() && !stmt.distinct && !stmt.limit_with_ties {
            stmt.limit_literal().map(|l| {
                let off = stmt.offset_literal().unwrap_or(0) as usize;
                (l as usize).saturating_add(off)
            })
        } else {
            None
        };
    if !stmt.order_by.is_empty() {
        let (sorted_synth, sorted_out) = sort_synth_by_order_by(
            &synth_schema,
            &stmt.order_by,
            &order_rewritten,
            kept_synth,
            out_rows,
            correlated_eval,
            keep_n,
        )?;
        kept_synth = sorted_synth;
        out_rows = sorted_out;
    }

    // v7.37.x — run deferred SELECT-list projection on the truncated
    // top-K survivors. For `GROUP BY thread_id ORDER BY MAX(date) DESC
    // LIMIT 50` against 20 000 groups, this turns ~40 000 compiled-VM
    // evals + Row allocations into 100, saving ~2-3 ms on the mailrs
    // minimal 100k shape.
    if let Some(DeferredProject {
        items_rewritten,
        items_compiled,
    }) = deferred_project
    {
        let synth_ctx = EvalContext::new(&synth_schema, None);
        let mut stack: Vec<Value> = Vec::new();
        for (idx, srow) in kept_synth.iter().enumerate() {
            let mut values: Vec<Value> = Vec::with_capacity(columns.len());
            for (i, rewritten) in items_rewritten.iter().enumerate() {
                let Some(rewritten) = rewritten else { continue };
                if deferred.iter().any(|(c, _)| *c == i) {
                    values.push(Value::Null);
                    continue;
                }
                values.push(if let Some(cc) = &items_compiled[i] {
                    eval::eval_compiled(cc, srow, &synth_ctx, &mut stack)?
                } else {
                    match correlated_eval {
                        Some(f) if crate::expr_has_subquery(rewritten) => {
                            f(rewritten, srow, &synth_ctx)?
                        }
                        _ => eval::eval_expr(rewritten, srow, &synth_ctx)?,
                    }
                });
            }
            out_rows[idx] = Row::new(values);
        }
    }

    let (synth_rows_out, synth_schema_out) = if deferred.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        (kept_synth, synth_schema.clone())
    };
    Ok(AggResult {
        columns,
        rows: out_rows,
        deferred,
        synth_rows: synth_rows_out,
        synth_schema: synth_schema_out,
    })
}

/// v7.32 (round-29) — validate the structural requirements of WITHIN
/// GROUP (ordered-set / hypothetical-set) aggregates up front, so a
/// malformed call surfaces as a SQL error rather than a silently
/// degenerate aggregate.
fn validate_within_group(agg_specs: &[AggSpec]) -> Result<(), EvalError> {
    // v7.32 (round-29) — WITHIN GROUP aggregates require the clause (PG
    // raises a hard error otherwise rather than silently degrading), and
    // SPG supports the single-sort-key form only.
    for spec in agg_specs {
        if is_within_group_name(&spec.name) {
            if spec.order_by.is_empty() {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{}() requires WITHIN GROUP (ORDER BY …)", spec.name),
                });
            }
            // mode() is the only WITHIN GROUP aggregate with no direct
            // argument; the rest carry one (percentile fraction /
            // hypothetical value).
            if spec.name != "mode" && spec.direct_arg.is_none() {
                return Err(EvalError::TypeMismatch {
                    detail: format!("{}() requires a direct argument", spec.name),
                });
            }
            // Multi-key WITHIN GROUP (multiple sort keys / hypothetical
            // args) is not supported yet — error loudly instead of
            // silently using only the first key.
            if spec.order_by.len() > 1 {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "{}() with multiple WITHIN GROUP sort keys is not supported yet",
                        spec.name
                    ),
                });
            }
        }
    }
    Ok(())
}

/// (1) Stream the WHERE-filtered rows, group by the GROUP BY value
/// tuple, and update per-group aggregate state. Returns the groups in
/// insertion order. See `run` for the bind-once fast path rationale.
#[allow(clippy::too_many_lines, clippy::type_complexity)]
fn accumulate_groups(
    rows: &[RowRef<'_>],
    group_exprs: &[Expr],
    agg_specs: &[AggSpec],
    schema_cols: &[ColumnSchema],
    table_alias: Option<&str>,
    correlated_eval: Option<CorrelatedEval<'_>>,
) -> Result<Vec<(Vec<Value>, Vec<AggState>)>, EvalError> {
    let ctx = EvalContext::new(schema_cols, table_alias);
    // Map group key (vec of values, encoded as canonical string) -> group state.
    // v7.32 (architecture v2, P2b) — insertion-ordered group state in
    // a Vec; the hash map only maps key → index. Removes the parallel
    // `key_order: Vec<String>` (a second per-group key clone) and the
    // per-group re-probe `groups[k]` at finalize (24k hash lookups for
    // the inbox shape). The map owns its key once on vacant insert.
    let mut order: Vec<(Vec<Value>, Vec<AggState>)> = Vec::new();
    let mut groups: hashbrown::HashMap<String, usize> = hashbrown::HashMap::new();
    // v7.37.x (mailrs Track A perf — SPGE ≫ PG18) — single-Text GROUP
    // BY column fast path. The canonical-string encode (`S<text>|`)
    // + `encode_key_refs_into` reuse-buffer churn dominated the 30 k-
    // row mailrs minimal probe (~3-4 ms / 30 k). For `GROUP BY t` on
    // a TEXT column (the inbox-listing / conversation-grouping shape)
    // the column text IS the canonical key — no encoder, no prefix
    // byte, no `refs` Vec rebuild per row. The fallback `groups` map
    // above is retained for multi-col / non-Text / collation paths;
    // this map only fires when the schema and value structurally
    // permit it. `null_group_idx` collects NULL group rows (SQL groups
    // all NULLs into one bucket).
    let mut groups_text: hashbrown::HashMap<String, usize> = hashbrown::HashMap::new();
    let mut null_group_idx: Option<usize> = None;
    // When there are no GROUP BY exprs *and* there is at least one aggregate,
    // every row collapses into a single anonymous group keyed by "".
    if rows.is_empty() && group_exprs.is_empty() {
        // Single empty-aggregate group: count=0, sum=0, max=NULL, etc.
        // No rows follow, so the map is never probed — seed `order` only.
        let init: Vec<AggState> = (0..agg_specs.len()).map(|_| AggState::default()).collect();
        order.push((Vec::new(), init));
    }

    // v7.30 (perf campaign) - hoist the per-row work that doesn't
    // depend on the row: which group exprs need collation folding
    // (none, for most queries - the old code cloned the whole
    // group_vals vec per row just in case).
    // v7.30 (perf campaign) - the no-tax row loop. When a group
    // expr or an aggregate argument is a bare column reference
    // (the overwhelmingly common shape), bind its position ONCE
    // and read row cells by offset in the loop - no per-row tree
    // walk, no owned-Value clone out of resolve_column. Anything
    // more complex keeps the eval path.
    let col_pos = |e: &Expr| -> Option<usize> {
        // Qualified references only: the bare-name resolver carries
        // alias/ambiguity logic the bind-once path must not fork.
        if let Expr::Column(c) = e
            && c.qualifier.is_some()
        {
            eval::find_column_pos(c, &ctx)
        } else {
            None
        }
    };
    let group_pos: Vec<Option<usize>> = group_exprs.iter().map(col_pos).collect();
    let all_groups_bound = group_pos.iter().all(Option::is_some);
    // v7.37.x — single-col GROUP BY on a TEXT-typed column lets the
    // hot loop key the hash map by the column text directly. Resolved
    // once from the bound position against `schema_cols`.
    let single_text_group_col: bool = group_pos.len() == 1
        && group_pos[0].is_some_and(|p| {
            schema_cols
                .get(p)
                .is_some_and(|c| matches!(c.ty, spg_storage::DataType::Text))
        });
    let arg_pos: Vec<Option<usize>> = agg_specs
        .iter()
        .map(|spec| spec.arg.as_ref().and_then(|e| col_pos(e)))
        .collect();
    // v7.37.x (mailrs Track A 100k attack) — dedicated tight loop
    // for the "single-Text GROUP BY + single MAX(bound numeric arg)"
    // shape. This is the mailrs `/api/conversations` minimal shape
    // (`GROUP BY thread_id, MAX(internal_date)`) and an inbox-listing
    // staple across the SPG customer set. Skipping the per-row spec
    // loop, FILTER / arg2 / order_keys checks, and the union-typed
    // `update_state` enum jump saves ~80-100 ns/row at 100 k input
    // — the gap closing the SPGE vs PG18 ratio at this scale.
    let dedicated_max_loop: bool = single_text_group_col
        && agg_specs.len() == 1
        && matches!(agg_specs[0].kind, AggKind::Max)
        && agg_specs[0].filter.is_none()
        && agg_specs[0].arg2.is_none()
        && agg_specs[0].order_by.is_empty()
        && !agg_specs[0].distinct
        && !agg_specs[0].first_ordered
        && arg_pos[0].is_some();
    // v7.36 (perf — mailrs Ask 1 SUM(LENGTH(text_body)) 18ms → ?) —
    // pre-compile every aggregate arg that's a `fully_compilable`
    // PURE expression over bound columns. Without this, `LENGTH(col)`
    // / `COALESCE(col, '')` / `CAST(col AS BIGINT)` etc. ALL fell
    // through to the `(None, Some(e)) => eval_arg(e, mat, ...)` slow
    // path that materialises a Cow<Row> per input row — for a 25k-row
    // JOIN that's 25k full-row clones for one column read. The Step
    // VM (`eval_compiled_ref`) reads columns by RowRef::get and runs
    // the same `apply_function` dispatcher with zero materialisation.
    let arg_compiled: Vec<Option<eval::CompiledExpr>> = agg_specs
        .iter()
        .enumerate()
        .map(|(i, spec)| match (&arg_pos[i], &spec.arg) {
            (Some(_), _) => None,
            (None, Some(e)) if eval::fully_compilable(e) => Some(eval::compile_expr(e, &ctx)),
            _ => None,
        })
        .collect();
    // v7.37.4 (L1 — executor-time CSE / mailrs P0) — dedupe
    // compiled aggregate-arg expressions across specs. mailrs's
    // `/api/conversations` SQL has 14 aggregates whose compiled
    // CASE/CAST arg expressions overlap heavily (`m.message_id != ''`
    // re-appears 4×, the inner `CASE WHEN m.message_id != '' THEN
    // m.message_id ELSE CAST(m.id AS TEXT) END` re-appears 3×). Each
    // dup currently costs one Step-VM walk per row — 100k rows ×
    // ~3-4 redundant evals = ~300-400k wasted Step-VM runs.
    //
    // Dedupe key = source `Expr` (PartialEq). `CompiledExpr` itself
    // is not `Hash` / `Eq`, but n_specs is small (≤ ~20 in practice);
    // O(n²) PartialEq probe cost = ~196 cmp per query, vs millions
    // of saved per-row evals. `fully_compilable` requires PURE
    // scalars (no NOW / RANDOM / sequence accessors), so an earlier
    // eval has identical observable semantics to the original.
    //
    // `arg_slot[i] = Some(s)` means spec `i`'s compiled arg lives in
    // slot `s` of `arg_unique_idx` (which points back into
    // `arg_compiled` for the canonical owner). Per-row cache fills
    // LAZILY — preserves the current FILTER semantics where an arg
    // whose spec is filtered out is never evaluated (and never
    // surfaces a type error). Reset to `None` at the top of each row.
    let mut arg_unique_idx: Vec<usize> = Vec::new();
    let mut arg_slot: Vec<Option<usize>> = Vec::with_capacity(agg_specs.len());
    arg_slot.resize(agg_specs.len(), None);
    for (i, spec) in agg_specs.iter().enumerate() {
        if arg_pos[i].is_some() || arg_compiled[i].is_none() {
            continue;
        }
        let src = spec.arg.as_ref().expect("arg_compiled => spec.arg is Some");
        let pos = arg_unique_idx
            .iter()
            .position(|&j| agg_specs[j].arg.as_ref().is_some_and(|other| other == src));
        arg_slot[i] = Some(match pos {
            Some(p) => p,
            None => {
                arg_unique_idx.push(i);
                arg_unique_idx.len() - 1
            }
        });
    }
    let mut row_eval_cache: Vec<Option<Value>> = Vec::with_capacity(arg_unique_idx.len());
    row_eval_cache.resize(arg_unique_idx.len(), None);
    // v7.33 (array_agg perf) — bound positions for each spec's internal
    // ORDER BY keys, so an ordered aggregate (`array_agg(x ORDER BY y)`)
    // reads the sort key by reference (RowRef::get) instead of
    // materialising the whole combined join row per input row just to
    // eval one bound column. Mirrors arg_pos. On the inbox shape this
    // turned 24k full-row (~1 KB each) clones into 24k single-cell reads.
    let order_pos: Vec<Vec<Option<usize>>> = agg_specs
        .iter()
        .map(|spec| spec.order_by.iter().map(|o| col_pos(&o.expr)).collect())
        .collect();
    // Does any spec need the fully-materialised row in the bound fast
    // path — a FILTER, a non-bound value arg, a second arg, or a non-bound
    // ORDER key? When false (every aggregate arg/key is a bound column —
    // the inbox shape) the bound fast path never materialises a row.
    let needs_mat = agg_specs.iter().enumerate().any(|(i, s)| {
        s.filter.is_some()
            || (s.arg.is_some() && arg_pos[i].is_none() && arg_compiled[i].is_none())
            || s.arg2.is_some()
            || order_pos[i].iter().any(Option::is_none)
    });
    let ci_positions: Vec<usize> = group_exprs
        .iter()
        .enumerate()
        .filter(|(_, g)| {
            matches!(
                eval::column_collation(g, &ctx),
                Some(spg_storage::Collation::CaseInsensitive)
            )
        })
        .map(|(i, _)| i)
        .collect();
    // v7.31 (perf 3e) — per-row scratch buffers. The fast path used
    // to allocate a key String (and a refs Vec) for EVERY row just
    // to probe the group map; hits — the overwhelming case — now
    // touch the allocator zero times.
    let mut keybuf_s = String::new();
    // v7.36 — reused Step VM eval stack for compiled aggregate args.
    let mut eval_stack: Vec<Value> = Vec::new();
    let mut dkeybuf = String::new();
    let mut refs: Vec<&Value> = Vec::with_capacity(group_pos.len());
    // v7.32 (round-31) — an aggregate's argument / FILTER / second arg /
    // ORDER key may itself be a *correlated* subquery, e.g.
    // `MAX((SELECT i.v FROM inner i WHERE i.fk = o.id))`. A non-correlated
    // subquery is pre-resolved to a literal before this loop, but a
    // correlated one survives as a subquery node and must be evaluated per
    // outer row through the correlated evaluator — the same hook the
    // select-list / HAVING / ORDER finalisers already use below. Plain
    // `eval_expr` would hit "subquery reached row eval".
    //
    // The `any_agg_subquery` gate is computed once here so the common case
    // (no subquery anywhere in the aggregate args — including every hot
    // scan/group aggregate) short-circuits before the per-row
    // `expr_has_subquery` walk: `eval_arg` is then exactly `eval_expr`.
    let any_agg_subquery = correlated_eval.is_some()
        && agg_specs.iter().any(|s| {
            s.filter
                .as_ref()
                .is_some_and(|e| crate::expr_has_subquery(e))
                || s.arg.as_ref().is_some_and(|e| crate::expr_has_subquery(e))
                || s.arg2.as_ref().is_some_and(|e| crate::expr_has_subquery(e))
                || s.order_by.iter().any(|o| crate::expr_has_subquery(&o.expr))
        });
    let eval_arg = |e: &Expr, r: &Row, c: &EvalContext<'_>| -> Result<Value, EvalError> {
        match correlated_eval {
            Some(f) if any_agg_subquery && crate::expr_has_subquery(e) => f(e, r, c),
            _ => eval::eval_expr(e, r, c),
        }
    };
    // v7.36 (perf — mailrs Phase 1, post u64-hash) — single
    // anonymous group fast path. When the query has no GROUP BY
    // (`SELECT SUM(LENGTH(col)) FROM ...`, COUNT, AVG, etc.) the
    // whole input collapses into one group. The fast path below
    // still pays one `groups.get("")` hash probe per row plus
    // `entry = &mut order[0]` reindex even when the empty-key
    // path encodes nothing — measured ~50 ns/row across 25 k rows
    // = ~1.25 ms of pure bookkeeping on the user_storage_usage
    // baseline.
    //
    // Bypass: lift `entry` outside the loop and feed every row
    // straight into it. Same `update_state` machinery, zero
    // per-row hash work, zero per-row index lookup.
    let single_anon_group = group_exprs.is_empty() && !rows.is_empty();
    if single_anon_group {
        // Seed the single group at idx 0 once.
        let init: Vec<AggState> = (0..agg_specs.len()).map(|_| AggState::default()).collect();
        order.clear();
        order.push((Vec::new(), init));
    }
    // v7.36 (perf — mailrs Phase 1, count_messages 2.58 → ?) —
    // `COUNT(*)` short-circuit. For a single-anon-group `COUNT(*)`
    // with no FILTER / DISTINCT, every survivor counts once — the
    // answer IS `rows.len()`. Skips the 25 k iterations of
    // `update_state("count_star", …)` on the mailrs count_messages
    // shape; the JOIN already produced exactly the set of rows
    // that must be counted.
    if single_anon_group
        && agg_specs.len() == 1
        && agg_specs[0].name == "count_star"
        && agg_specs[0].filter.is_none()
        && agg_specs[0].arg.is_none()
        && agg_specs[0].arg2.is_none()
        && agg_specs[0].order_by.is_empty()
        && !agg_specs[0].distinct
    {
        let state = &mut order[0].1[0];
        state.count = rows.len() as i64;
        return Ok(order);
    }
    // v7.36 (perf — mailrs Phase 1) — `COUNT(<bound col>)` (non-`*`)
    // collapses to: read the cell, increment when not NULL. Skips
    // the per-row spec dispatch + `update_state("count", …)`.
    if single_anon_group
        && agg_specs.len() == 1
        && agg_specs[0].name == "count"
        && agg_specs[0].filter.is_none()
        && agg_specs[0].arg2.is_none()
        && agg_specs[0].order_by.is_empty()
        && !agg_specs[0].distinct
        && arg_pos[0].is_some()
    {
        let p = arg_pos[0].unwrap();
        let mut count: i64 = 0;
        for row in rows {
            if !matches!(row.get(p), Some(Value::Null) | None) {
                count += 1;
            }
        }
        let state = &mut order[0].1[0];
        state.count = count;
        return Ok(order);
    }
    // v7.36 (perf — mailrs Phase 1, user_storage_usage 7.5 → ?) —
    // single-aggregate streaming accumulator. For
    // `SUM(<compiled-expr>)` / `SUM(<bound col>)` with no GROUP BY,
    // no FILTER, no arg2, no ORDER BY, no DISTINCT, the whole
    // per-row work collapses to: eval the arg, match the Value
    // variant, accumulate. Skips the spec-dispatch loop +
    // `update_state` per-row name match. On a 25 k-row JOIN
    // (user_storage_usage `SUM(LENGTH(text_body))`) that's
    // ~50-100 ns/row of pure spec-dispatch overhead removed.
    if single_anon_group
        && agg_specs.len() == 1
        && agg_specs[0].filter.is_none()
        && agg_specs[0].arg2.is_none()
        && agg_specs[0].order_by.is_empty()
        && !agg_specs[0].distinct
        && (agg_specs[0].name == "sum" || agg_specs[0].name == "avg")
        && (arg_pos[0].is_some() || arg_compiled[0].is_some())
    {
        let arg_pos0 = arg_pos[0];
        let arg_c0 = &arg_compiled[0];
        let mut sum_int: i64 = 0;
        let mut sum_float: f64 = 0.0;
        let mut use_float = false;
        let mut count: i64 = 0;
        // Borrow-aware fast inner: avoid the per-row clone when arg
        // is a bound column position.
        if let Some(p) = arg_pos0 {
            for row in rows {
                let v_ref = row.get(p).unwrap_or(&Value::Null);
                match v_ref {
                    Value::Null => continue,
                    Value::SmallInt(n) => {
                        sum_int += i64::from(*n);
                        count += 1;
                    }
                    Value::Int(n) => {
                        sum_int += i64::from(*n);
                        count += 1;
                    }
                    Value::BigInt(n) => {
                        sum_int += *n;
                        count += 1;
                    }
                    Value::Float(x) => {
                        sum_float += *x;
                        use_float = true;
                        count += 1;
                    }
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: format!("sum/avg need numeric, got {:?}", other.data_type()),
                        });
                    }
                }
            }
        } else if let Some(p) = arg_c0.as_ref().and_then(|c| c.as_single_column_length()) {
            // v7.36 (perf — mailrs Phase 1, user_storage_usage hot
            // inner) — `SUM(LENGTH(<text col>))` collapses to a
            // straight scan: read the cell by ref, branch on the
            // variant, do an ASCII probe + `len()` (or
            // `chars().count()` on non-ASCII), accumulate. No Step
            // VM, no stack push/pop, no `BigInt` boxing on the way
            // out — pure i64 sum. The original Step VM path keeps
            // running for everything outside this shape (`SUM(col)`,
            // `SUM(expr)`, multi-step compiled args).
            for row in rows {
                let Some(v_ref) = row.get(p) else {
                    continue;
                };
                let n = match v_ref {
                    Value::Null => continue,
                    Value::Text(s) => {
                        if s.is_ascii() {
                            s.len() as i64
                        } else {
                            s.chars().count() as i64
                        }
                    }
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: format!("length() needs text, got {:?}", other.data_type()),
                        });
                    }
                };
                sum_int += n;
                count += 1;
            }
        } else {
            let c = arg_c0.as_ref().unwrap();
            for row in rows {
                let v = eval::eval_compiled_ref(c, row, &ctx, &mut eval_stack)?;
                match v {
                    Value::Null => continue,
                    Value::SmallInt(n) => {
                        sum_int += i64::from(n);
                        count += 1;
                    }
                    Value::Int(n) => {
                        sum_int += i64::from(n);
                        count += 1;
                    }
                    Value::BigInt(n) => {
                        sum_int += n;
                        count += 1;
                    }
                    Value::Float(x) => {
                        sum_float += x;
                        use_float = true;
                        count += 1;
                    }
                    other => {
                        return Err(EvalError::TypeMismatch {
                            detail: format!("sum/avg need numeric, got {:?}", other.data_type()),
                        });
                    }
                }
            }
        }
        let state = &mut order[0].1[0];
        state.count = count;
        state.sum_int = sum_int;
        state.sum_float = sum_float;
        state.use_float = use_float;
        return Ok(order);
    }
    // v7.37.x (mailrs Track A 100k attack) — tight inlined loop for
    // the "single-Text GROUP BY + single MAX(bound numeric arg)"
    // shape. See `dedicated_max_loop` above for the gate. Returns
    // straight to the caller; the rest of the function (single-anon,
    // bound-fast, eval-slow paths) is skipped.
    if dedicated_max_loop && !single_anon_group {
        let gpos = group_pos[0].expect("dedicated_max_loop gates on Some");
        let apos = arg_pos[0].expect("dedicated_max_loop gates on Some");
        for row in rows {
            let kv = row.get(gpos).unwrap_or(&Value::Null);
            let idx = match kv {
                Value::Text(s) => match groups_text.get(s.as_str()) {
                    Some(&i) => i,
                    None => {
                        let i = order.len();
                        order.push((
                            alloc::vec![Value::Text(s.clone())],
                            alloc::vec![AggState::default()],
                        ));
                        groups_text.insert(s.clone(), i);
                        i
                    }
                },
                Value::Null => match null_group_idx {
                    Some(i) => i,
                    None => {
                        let i = order.len();
                        order.push((alloc::vec![Value::Null], alloc::vec![AggState::default()]));
                        null_group_idx = Some(i);
                        i
                    }
                },
                _ => {
                    // Schema said Text but value isn't — fall back to
                    // the generic encoded path for correctness.
                    refs.clear();
                    refs.push(kv);
                    encode_key_refs_into(&refs, &mut keybuf_s);
                    match groups.get(keybuf_s.as_str()) {
                        Some(&i) => i,
                        None => {
                            let i = order.len();
                            order.push((alloc::vec![kv.clone()], alloc::vec![AggState::default()]));
                            groups.insert(keybuf_s.clone(), i);
                            i
                        }
                    }
                }
            };
            // Inline MAX accumulator — skip the union-typed
            // `update_state` enum jump and per-spec arg dispatch.
            let av = row.get(apos).unwrap_or(&Value::Null);
            if !matches!(av, Value::Null) {
                let st = &mut order[idx].1[0];
                let upd = match &st.extreme {
                    None => true,
                    Some(prev) => value_cmp(av, prev) == core::cmp::Ordering::Greater,
                };
                if upd {
                    st.extreme = Some(av.clone());
                }
            }
        }
        return Ok(order);
    }

    for row in rows {
        // v7.37.4 (L1 CSE) — reset per-row cache for shared compiled
        // aggregate-arg evals. No-op when no dedupe (empty vec).
        for slot in row_eval_cache.iter_mut() {
            *slot = None;
        }
        if single_anon_group {
            let entry = &mut order[0];
            let mat: Option<Cow<'_, Row>> = if needs_mat { Some(row.as_row()) } else { None };
            for (i, spec) in agg_specs.iter().enumerate() {
                if let Some(f) = &spec.filter
                    && !matches!(
                        eval_arg(f, mat.as_deref().expect("needs_mat for FILTER"), &ctx)?,
                        Value::Bool(true)
                    )
                {
                    continue;
                }
                let arg_owned: Value;
                let arg_ref: &Value = match (&arg_pos[i], arg_slot[i], &spec.arg) {
                    (Some(p), _, _) => row.get(*p).unwrap_or(&Value::Null),
                    (None, None, None) => {
                        arg_owned = Value::Bool(true);
                        &arg_owned
                    }
                    (None, Some(s), _) => {
                        if row_eval_cache[s].is_none() {
                            let c = arg_compiled[arg_unique_idx[s]]
                                .as_ref()
                                .expect("arg_unique_idx points at a compiled spec");
                            let v = eval::eval_compiled_ref(c, row, &ctx, &mut eval_stack)?;
                            row_eval_cache[s] = Some(v);
                        }
                        row_eval_cache[s].as_ref().expect("just filled above")
                    }
                    (None, None, Some(e)) => {
                        arg_owned = eval_arg(
                            e,
                            mat.as_deref().expect("needs_mat for non-bound arg"),
                            &ctx,
                        )?;
                        &arg_owned
                    }
                };
                let arg2_val = match &spec.arg2 {
                    None => None,
                    Some(e) => Some(eval_arg(
                        e,
                        mat.as_deref().expect("needs_mat for arg2"),
                        &ctx,
                    )?),
                };
                let order_keys = if spec.order_by.is_empty() {
                    None
                } else {
                    let mut keys = Vec::with_capacity(spec.order_by.len());
                    for (k, o) in spec.order_by.iter().enumerate() {
                        let v = if let Some(p) = order_pos[i][k] {
                            row.get(p).cloned().unwrap_or(Value::Null)
                        } else {
                            eval_arg(
                                &o.expr,
                                mat.as_deref().expect("needs_mat for ORDER key"),
                                &ctx,
                            )?
                        };
                        keys.push(v);
                    }
                    Some(keys)
                };
                // v7.36 (perf — bugfix v7.36.1 candidate) — first_ordered
                // was missing from the single_anon_group fast path,
                // sending `(array_agg(x ORDER BY y))[1]` values into
                // `update_state(array_agg, …)` whose finalize ignored
                // the absent `first_best` and returned `[]`. The slow
                // path below has the same branch — keep them aligned.
                if spec.first_ordered {
                    if let Some(keys) = order_keys {
                        let st = &mut entry.1[i];
                        let better = match &st.first_best {
                            None => true,
                            Some((bk, _)) => {
                                cmp_order_keys(&spec.order_by, &keys, bk)
                                    == core::cmp::Ordering::Less
                            }
                        };
                        if better {
                            st.first_best = Some((keys, arg_ref.clone()));
                        }
                    }
                    continue;
                }
                if spec.distinct {
                    // v7.37.x (mailrs Track A 100k distinct_aggs attack)
                    // — single-Text DISTINCT fast path. Within a single
                    // distinct spec all input values come from one
                    // expression and share one type, so the encode-
                    // prefix (`S<text>|`) is redundant: the column
                    // text alone is collision-free within this spec's
                    // `seen` set. Skips encode_one + 2-walk
                    // contains+insert; only Text arms apply, others
                    // ride the encoded path unchanged.
                    if let Value::Text(s) = arg_ref {
                        if entry.1[i].seen.contains(s.as_str()) {
                            continue;
                        }
                        entry.1[i].seen.insert(s.clone());
                    } else {
                        encode_key_refs_into(core::slice::from_ref(&arg_ref), &mut dkeybuf);
                        if entry.1[i].seen.contains(dkeybuf.as_str()) {
                            continue;
                        }
                        entry.1[i].seen.insert(dkeybuf.clone());
                    }
                }
                // v7.37.x (mailrs Track A 100k attack) — inline the
                // common aggregate kinds (MAX / MIN / Count / CountStar
                // / BoolOr / BoolAnd) here instead of dispatching
                // through `update_state`'s enum jump + per-kind branch.
                // Skipping the function-call overhead saves ~20-30 ns
                // per spec per row at 100 k; the slow kinds keep the
                // dispatched call.
                match spec.kind {
                    AggKind::Max => {
                        if !matches!(arg_ref, Value::Null) {
                            let st = &mut entry.1[i];
                            let upd = match &st.extreme {
                                None => true,
                                Some(prev) => {
                                    value_cmp(arg_ref, prev) == core::cmp::Ordering::Greater
                                }
                            };
                            if upd {
                                st.extreme = Some(arg_ref.clone());
                            }
                        }
                    }
                    AggKind::Min => {
                        if !matches!(arg_ref, Value::Null) {
                            let st = &mut entry.1[i];
                            let upd = match &st.extreme {
                                None => true,
                                Some(prev) => value_cmp(arg_ref, prev) == core::cmp::Ordering::Less,
                            };
                            if upd {
                                st.extreme = Some(arg_ref.clone());
                            }
                        }
                    }
                    AggKind::CountStar => {
                        entry.1[i].count += 1;
                    }
                    AggKind::Count => {
                        if !matches!(arg_ref, Value::Null) {
                            entry.1[i].count += 1;
                        }
                    }
                    AggKind::BoolOr => {
                        if let Value::Bool(b) = arg_ref {
                            let st = &mut entry.1[i];
                            st.bool_acc = Some(st.bool_acc.unwrap_or(false) || *b);
                        }
                    }
                    AggKind::BoolAnd => {
                        if let Value::Bool(b) = arg_ref {
                            let st = &mut entry.1[i];
                            st.bool_acc = Some(st.bool_acc.unwrap_or(true) && *b);
                        }
                    }
                    _ => {
                        update_state(
                            &mut entry.1[i],
                            spec.kind,
                            &spec.name,
                            arg_ref,
                            arg2_val.as_ref(),
                            order_keys,
                        )?;
                    }
                }
            }
            continue;
        }
        // Fast key: bound positions + no ci folding -> encode
        // straight from borrowed cells; group_vals materialise
        // only when the group is NEW.
        if all_groups_bound && ci_positions.is_empty() {
            // v7.37.x — single-Text fast path uses the raw text as the
            // map key (no encode_one's `S<text>|` prefix/suffix push,
            // no refs Vec rebuild). NULL values land in a dedicated
            // slot so SQL's "all NULLs share one group" semantics hold.
            let idx = if single_text_group_col {
                let v = row.get(group_pos[0].unwrap()).unwrap_or(&Value::Null);
                match v {
                    Value::Text(s) => match groups_text.get(s.as_str()) {
                        Some(&i) => i,
                        None => {
                            let i = order.len();
                            let init: Vec<AggState> =
                                (0..agg_specs.len()).map(|_| AggState::default()).collect();
                            order.push((alloc::vec![Value::Text(s.clone())], init));
                            groups_text.insert(s.clone(), i);
                            i
                        }
                    },
                    Value::Null => match null_group_idx {
                        Some(i) => i,
                        None => {
                            let i = order.len();
                            let init: Vec<AggState> =
                                (0..agg_specs.len()).map(|_| AggState::default()).collect();
                            order.push((alloc::vec![Value::Null], init));
                            null_group_idx = Some(i);
                            i
                        }
                    },
                    _ => {
                        // Schema says Text but value is something else
                        // (coercion edge case). Fall back to the encoded
                        // path for correctness — same logic as the
                        // non-single-Text branch below.
                        refs.clear();
                        refs.push(v);
                        encode_key_refs_into(&refs, &mut keybuf_s);
                        match groups.get(keybuf_s.as_str()) {
                            Some(&i) => i,
                            None => {
                                let i = order.len();
                                let init: Vec<AggState> =
                                    (0..agg_specs.len()).map(|_| AggState::default()).collect();
                                order.push((alloc::vec![v.clone()], init));
                                groups.insert(keybuf_s.clone(), i);
                                i
                            }
                        }
                    }
                }
            } else {
                refs.clear();
                refs.extend(
                    group_pos
                        .iter()
                        .map(|p| row.get(p.unwrap()).unwrap_or(&Value::Null)),
                );
                encode_key_refs_into(&refs, &mut keybuf_s);
                match groups.get(keybuf_s.as_str()) {
                    Some(&i) => i,
                    None => {
                        let i = order.len();
                        let init: Vec<AggState> =
                            (0..agg_specs.len()).map(|_| AggState::default()).collect();
                        let owned: Vec<Value> = refs.iter().map(|v| (*v).clone()).collect();
                        order.push((owned, init));
                        groups.insert(keybuf_s.clone(), i);
                        i
                    }
                }
            };
            let entry = &mut order[idx];
            // v7.33 (array_agg perf) — materialise the combined row AT
            // MOST once per input row, and only when a spec actually
            // needs the eval path (FILTER / non-bound arg / arg2 / non-
            // bound ORDER key). Bound args and bound ORDER keys read
            // cells by reference below, so the inbox shape (all bound)
            // never materialises — killing the per-row ~1 KB clone that
            // dominated the ordered-aggregate cost.
            let mat: Option<Cow<'_, Row>> = if needs_mat { Some(row.as_row()) } else { None };
            for (i, spec) in agg_specs.iter().enumerate() {
                // v7.32 (round-29) — FILTER (WHERE cond): exclude rows
                // where cond is not TRUE before they reach this
                // aggregate's accumulator (and before DISTINCT dedup).
                if let Some(f) = &spec.filter
                    && !matches!(
                        eval_arg(f, mat.as_deref().expect("needs_mat for FILTER"), &ctx)?,
                        Value::Bool(true)
                    )
                {
                    continue;
                }
                let arg_owned: Value;
                let arg_ref: &Value = match (&arg_pos[i], arg_slot[i], &spec.arg) {
                    (Some(p), _, _) => row.get(*p).unwrap_or(&Value::Null),
                    (None, None, None) => {
                        arg_owned = Value::Bool(true);
                        &arg_owned
                    }
                    (None, Some(s), _) => {
                        // v7.37.4 (L1 CSE) — shared compiled-arg slot.
                        // First spec that needs slot `s` this row pays
                        // the Step-VM eval; siblings reading the same
                        // slot get the cached Value for free. Preserves
                        // FILTER semantics: a spec filtered out above
                        // never reaches here, so its arg stays unevaled.
                        if row_eval_cache[s].is_none() {
                            let c = arg_compiled[arg_unique_idx[s]]
                                .as_ref()
                                .expect("arg_unique_idx points at a compiled spec");
                            let v = eval::eval_compiled_ref(c, row, &ctx, &mut eval_stack)?;
                            row_eval_cache[s] = Some(v);
                        }
                        row_eval_cache[s].as_ref().expect("just filled above")
                    }
                    (None, None, Some(e)) => {
                        arg_owned = eval_arg(
                            e,
                            mat.as_deref().expect("needs_mat for non-bound arg"),
                            &ctx,
                        )?;
                        &arg_owned
                    }
                };
                let arg2_val = match &spec.arg2 {
                    None => None,
                    Some(e) => Some(eval_arg(
                        e,
                        mat.as_deref().expect("needs_mat for arg2"),
                        &ctx,
                    )?),
                };
                let order_keys = if spec.order_by.is_empty() {
                    None
                } else {
                    let mut keys = Vec::with_capacity(spec.order_by.len());
                    for (k, o) in spec.order_by.iter().enumerate() {
                        // Bound ORDER key → read the cell by reference; only
                        // a non-bound key falls to the materialised eval path.
                        keys.push(match order_pos[i][k] {
                            Some(p) => row.get(p).cloned().unwrap_or(Value::Null),
                            None => eval_arg(
                                &o.expr,
                                mat.as_deref().expect("needs_mat for non-bound ORDER key"),
                                &ctx,
                            )?,
                        });
                    }
                    Some(keys)
                };
                // v7.33 (array_agg argmax) — first_ordered: keep only the
                // running first-by-order element (strict-less replacement
                // = ties keep the earliest row, matching the stable-sort
                // `[1]`), no array build.
                if spec.first_ordered {
                    if let Some(keys) = order_keys {
                        let st = &mut entry.1[i];
                        let better = match &st.first_best {
                            None => true,
                            Some((bk, _)) => {
                                cmp_order_keys(&spec.order_by, &keys, bk)
                                    == core::cmp::Ordering::Less
                            }
                        };
                        if better {
                            st.first_best = Some((keys, arg_ref.clone()));
                        }
                    }
                    continue;
                }
                if spec.distinct {
                    // v7.37.x — single-Text DISTINCT fast path (see
                    // bound fast path counterpart above). Per-spec
                    // type invariance lets us use the column text as
                    // the `seen` key directly, no `S<text>|` prefix.
                    if let Value::Text(s) = arg_ref {
                        if entry.1[i].seen.contains(s.as_str()) {
                            continue;
                        }
                        entry.1[i].seen.insert(s.clone());
                    } else {
                        encode_key_refs_into(core::slice::from_ref(&arg_ref), &mut dkeybuf);
                        if entry.1[i].seen.contains(dkeybuf.as_str()) {
                            continue;
                        }
                        entry.1[i].seen.insert(dkeybuf.clone());
                    }
                }
                // v7.37.x (mailrs Track A 100k attack) — inline the
                // common aggregate kinds (MAX / MIN / Count / CountStar
                // / BoolOr / BoolAnd) here instead of dispatching
                // through `update_state`'s enum jump + per-kind branch.
                // Skipping the function-call overhead saves ~20-30 ns
                // per spec per row at 100 k; the slow kinds keep the
                // dispatched call.
                match spec.kind {
                    AggKind::Max => {
                        if !matches!(arg_ref, Value::Null) {
                            let st = &mut entry.1[i];
                            let upd = match &st.extreme {
                                None => true,
                                Some(prev) => {
                                    value_cmp(arg_ref, prev) == core::cmp::Ordering::Greater
                                }
                            };
                            if upd {
                                st.extreme = Some(arg_ref.clone());
                            }
                        }
                    }
                    AggKind::Min => {
                        if !matches!(arg_ref, Value::Null) {
                            let st = &mut entry.1[i];
                            let upd = match &st.extreme {
                                None => true,
                                Some(prev) => value_cmp(arg_ref, prev) == core::cmp::Ordering::Less,
                            };
                            if upd {
                                st.extreme = Some(arg_ref.clone());
                            }
                        }
                    }
                    AggKind::CountStar => {
                        entry.1[i].count += 1;
                    }
                    AggKind::Count => {
                        if !matches!(arg_ref, Value::Null) {
                            entry.1[i].count += 1;
                        }
                    }
                    AggKind::BoolOr => {
                        if let Value::Bool(b) = arg_ref {
                            let st = &mut entry.1[i];
                            st.bool_acc = Some(st.bool_acc.unwrap_or(false) || *b);
                        }
                    }
                    AggKind::BoolAnd => {
                        if let Value::Bool(b) = arg_ref {
                            let st = &mut entry.1[i];
                            st.bool_acc = Some(st.bool_acc.unwrap_or(true) && *b);
                        }
                    }
                    _ => {
                        update_state(
                            &mut entry.1[i],
                            spec.kind,
                            &spec.name,
                            arg_ref,
                            arg2_val.as_ref(),
                            order_keys,
                        )?;
                    }
                }
            }
            continue;
        }
        // v7.32 (P4 increment 2) — eval (non-bound) path: present the
        // row as a borrowed Row once (Owned → zero-cost borrow; a join
        // tuple materialises here exactly once, never on the bound fast
        // path above), then the original eval loop runs unchanged.
        let row_materialised = row.as_row();
        let row: &Row = &row_materialised;
        let group_vals: Vec<Value> = group_exprs
            .iter()
            .map(|g| eval::eval_expr(g, row, &ctx))
            .collect::<Result<_, _>>()?;
        // v7.17.0 Phase 2.5b — case-insensitive group keying: fold
        // only the ci columns, and only when any exist. Display
        // value (`group_vals`) stays original — only the key folds.
        let key = if ci_positions.is_empty() {
            encode_key(&group_vals)
        } else {
            let mut key_vals = group_vals.clone();
            for &i in &ci_positions {
                if let Value::Text(s) = &key_vals[i] {
                    key_vals[i] = Value::Text(s.to_ascii_lowercase());
                }
            }
            encode_key(&key_vals)
        };
        // Probe by index; the map owns the key once on vacant insert.
        let idx = match groups.get(key.as_str()) {
            Some(&i) => i,
            None => {
                let i = order.len();
                let init: Vec<AggState> =
                    (0..agg_specs.len()).map(|_| AggState::default()).collect();
                order.push((group_vals.clone(), init));
                groups.insert(key, i);
                i
            }
        };
        let entry = &mut order[idx];
        for (i, spec) in agg_specs.iter().enumerate() {
            // v7.32 (round-29) — FILTER (WHERE cond): exclude rows where
            // cond is not TRUE before accumulation (and before DISTINCT).
            if let Some(f) = &spec.filter
                && !matches!(eval_arg(f, row, &ctx)?, Value::Bool(true))
            {
                continue;
            }
            let arg_val = match &spec.arg {
                None => Value::Bool(true), // count_star: sentinel non-null
                Some(e) => eval_arg(e, row, &ctx)?,
            };
            // v7.17.0 — `string_agg(value, separator)` evaluates the
            // separator per row but PG treats it as constant; we
            // pass the per-row value into update_state so a future
            // varying-separator caller still sees correct output,
            // even though SPG (like PG) only uses the most recent.
            let arg2_val = match &spec.arg2 {
                None => None,
                Some(e) => Some(eval_arg(e, row, &ctx)?),
            };
            // v7.24 (round-16 A) — aggregate-internal ORDER BY:
            // evaluate the key tuple against the source row.
            let order_keys = if spec.order_by.is_empty() {
                None
            } else {
                let mut keys = Vec::with_capacity(spec.order_by.len());
                for o in &spec.order_by {
                    keys.push(eval_arg(&o.expr, row, &ctx)?);
                }
                Some(keys)
            };
            // v7.33 (array_agg argmax) — first_ordered: keep the running
            // first-by-order element only (mirrors the bound fast path).
            if spec.first_ordered {
                if let Some(keys) = order_keys {
                    let st = &mut entry.1[i];
                    let better = match &st.first_best {
                        None => true,
                        Some((bk, _)) => {
                            cmp_order_keys(&spec.order_by, &keys, bk) == core::cmp::Ordering::Less
                        }
                    };
                    if better {
                        st.first_best = Some((keys, arg_val.clone()));
                    }
                }
                continue;
            }
            // v7.25 (round-17) — DISTINCT: drop repeated inputs
            // before they reach the accumulator. NULLs flow through
            // (each aggregate's own NULL rule applies; PG also
            // treats NULL as a single distinct value for array_agg).
            // v7.37.x — single-Text fast path same shape as the
            // bound/slow paths above.
            if spec.distinct {
                let inserted = match &arg_val {
                    Value::Text(s) => entry.1[i].seen.insert(s.clone()),
                    _ => {
                        let key = encode_key(core::slice::from_ref(&arg_val));
                        entry.1[i].seen.insert(key)
                    }
                };
                if !inserted {
                    continue;
                }
            }
            update_state(
                &mut entry.1[i],
                spec.kind,
                &spec.name,
                &arg_val,
                arg2_val.as_ref(),
                order_keys,
            )?;
        }
    }
    Ok(order)
}

/// (2a) Build the synthetic per-group schema: `__grp_0..K` then
/// `__agg_0..N`. Group types are probed from the first row; aggregate
/// types from each spec.
fn build_synth_schema(
    rows: &[RowRef<'_>],
    group_exprs: &[Expr],
    agg_specs: &[AggSpec],
    schema_cols: &[ColumnSchema],
    table_alias: Option<&str>,
) -> Result<Vec<ColumnSchema>, EvalError> {
    let ctx = EvalContext::new(schema_cols, table_alias);
    // Build synthetic schema: __grp_0..K then __agg_0..N.
    let group_types: Vec<DataType> = if rows.is_empty() {
        // Use Text as a safe stand-in — empty result means schema isn't
        // observable. Avoids needing to evaluate group exprs on no row.
        group_exprs.iter().map(|_| DataType::Text).collect()
    } else {
        let probe_row = rows[0].as_row();
        let probe: &Row = &probe_row;
        group_exprs
            .iter()
            .map(|g| {
                eval::eval_expr(g, probe, &ctx).map(|v| v.data_type().unwrap_or(DataType::Text))
            })
            .collect::<Result<_, _>>()?
    };
    let agg_types: Vec<DataType> = agg_specs
        .iter()
        .map(|spec| infer_agg_type(spec, schema_cols))
        .collect();
    let mut synth_schema: Vec<ColumnSchema> = Vec::new();
    for (i, ty) in group_types.iter().enumerate() {
        synth_schema.push(ColumnSchema::new(format!("__grp_{i}"), *ty, true));
    }
    for (i, ty) in agg_types.iter().enumerate() {
        synth_schema.push(ColumnSchema::new(format!("__agg_{i}"), *ty, true));
    }
    Ok(synth_schema)
}

/// (2b) Materialise one synthetic row per group (insertion order):
/// apply each aggregate's internal ORDER BY, then finalise the running
/// state into the group + aggregate cells.
/// v7.33 — compare two aggregate-internal ORDER BY key tuples under the
/// per-key DESC / NULLS directives. This is the exact comparator the
/// finalize sort uses, factored out so the `first_ordered` argmax
/// accumulator's "keep first" decision is provably identical to taking
/// element `[1]` of the fully-sorted array.
fn cmp_order_keys(
    order_by: &[spg_sql::ast::OrderBy],
    a: &[Value],
    b: &[Value],
) -> core::cmp::Ordering {
    for (k, o) in order_by.iter().enumerate() {
        let cmp = crate::order_by_value_cmp(o.desc, o.nulls_first, &a[k], &b[k]);
        if cmp != core::cmp::Ordering::Equal {
            return cmp;
        }
    }
    core::cmp::Ordering::Equal
}

fn finalize_synth_rows(
    order: &[(Vec<Value>, Vec<AggState>)],
    agg_specs: &[AggSpec],
    synth_schema: &[ColumnSchema],
    rows: &[RowRef<'_>],
    schema_cols: &[ColumnSchema],
    table_alias: Option<&str>,
) -> Result<Vec<Row>, EvalError> {
    let ctx = EvalContext::new(schema_cols, table_alias);
    // v7.32 (round-29) — ordered-set direct arguments (the percentile
    // fraction) are constant per PG, so evaluate each once up front.
    let direct_arg_vals: Vec<Option<Value>> = agg_specs
        .iter()
        .map(|spec| match (&spec.direct_arg, rows.first()) {
            (Some(e), Some(r)) => eval::eval_expr(e, &r.as_row(), &ctx).map(Some),
            _ => Ok(None),
        })
        .collect::<Result<_, _>>()?;

    // Materialise synthetic rows (insertion order = `order`).
    let mut synth_rows: Vec<Row> = Vec::new();
    for (gvals, states) in order {
        let mut values: Vec<Value> = Vec::with_capacity(synth_schema.len());
        values.extend(gvals.iter().cloned());
        for (i, st) in states.iter().enumerate() {
            // v7.33 (array_agg argmax) — first_ordered: the running
            // first-by-order value IS the result; no array build/sort.
            if agg_specs[i].first_ordered {
                values.push(
                    st.first_best
                        .as_ref()
                        .map_or(Value::Null, |(_, v)| v.clone()),
                );
                continue;
            }
            // v7.24 (round-16 A) — order the collected items per the
            // aggregate-internal ORDER BY before finalize consumes
            // them.
            let st_sorted;
            let st_final: &AggState =
                if !agg_specs[i].order_by.is_empty() && st.item_keys.len() == st.items.len() {
                    let mut idx: Vec<usize> = (0..st.items.len()).collect();
                    let ob = &agg_specs[i].order_by;
                    idx.sort_by(|&x, &y| cmp_order_keys(ob, &st.item_keys[x], &st.item_keys[y]));
                    let mut sorted = st.clone();
                    sorted.items = idx.iter().map(|&j| st.items[j].clone()).collect();
                    st_sorted = sorted;
                    &st_sorted
                } else {
                    st
                };
            // Ordered-set aggregates compute from the sorted items + the
            // direct fraction; everything else uses the running state.
            let v = if is_within_group_name(&agg_specs[i].name) {
                finalize_ordered_set(
                    &agg_specs[i].name,
                    st_final,
                    direct_arg_vals[i].as_ref(),
                    agg_specs[i].order_by.first(),
                )
            } else {
                finalize(&agg_specs[i].name, st_final)
            };
            values.push(v);
        }
        synth_rows.push(Row::new(values));
    }
    Ok(synth_rows)
}

/// (3) Rewrite the user's SELECT items + HAVING to reference the
/// synthetic columns, filter groups by HAVING, and project each
/// surviving group into an output row. The synth rows ride alongside
/// (`kept_synth`) so post-LIMIT deferred subqueries can evaluate later.
#[allow(clippy::too_many_lines)]
fn project_groups(
    synth_rows: Vec<Row>,
    stmt: &SelectStatement,
    group_exprs: &[Expr],
    agg_specs: &[AggSpec],
    synth_schema: &[ColumnSchema],
    correlated_eval: Option<CorrelatedEval<'_>>,
    defer_projection: bool,
) -> Result<Projection, EvalError> {
    // Rewrite the user's SELECT items + ORDER BY to reference synthetic
    // columns. After rewriting, every remaining `Expr::Column` must
    // resolve against the synthetic schema (i.e. must have been a GROUP
    // BY expression).
    let columns: Vec<ColumnSchema> = stmt
        .items
        .iter()
        .map(|item| match item {
            SelectItem::Wildcard => Err(EvalError::TypeMismatch {
                detail: "SELECT * with aggregates is not supported".into(),
            }),
            SelectItem::Expr { expr, alias } => {
                let rewritten = rewrite_expr(expr, group_exprs, agg_specs);
                let name = alias.clone().unwrap_or_else(|| expr.to_string());
                Ok(ColumnSchema::new(
                    name,
                    agg_or_group_type(&rewritten, synth_schema),
                    true,
                ))
            }
        })
        .collect::<Result<_, _>>()?;

    // Project per synthetic row. HAVING filters out groups *before*
    // we keep the projected row — same semantics as PG: HAVING runs
    // against the aggregated row (so `HAVING count(*) > 1` works) and
    // sees only group-by'd columns plus aggregate values.
    let synth_ctx = EvalContext::new(synth_schema, None);
    let having_rewritten = stmt
        .having
        .as_ref()
        .map(|h| rewrite_expr(h, group_exprs, agg_specs));
    // v7.30 (phase 3e-1) - rewrite SELECT items ONCE. This ran per
    // GROUP (23.5k x 9 items of AST cloning = ~48% of the inbox
    // query in sampled stacks); the rewrite is group-independent.
    // Stable addresses also let the per-expression subquery plans
    // (v7.29 3c) hit across groups instead of rebuilding.
    let items_rewritten: alloc::vec::Vec<Option<Expr>> = stmt
        .items
        .iter()
        .map(|item| match item {
            SelectItem::Expr { expr, .. } => Some(rewrite_expr(expr, group_exprs, agg_specs)),
            SelectItem::Wildcard => None,
        })
        .collect();
    // v7.31 (perf — PG lesson #1): subquery-bearing select items
    // deferred to post-LIMIT, when no sort/filter key can observe
    // them. ORDER BY rewrites are hoisted here so the safety check
    // and the sort below share one rewrite pass.
    let order_rewritten: Vec<Expr> = stmt
        .order_by
        .iter()
        .map(|o| rewrite_expr(&o.expr, group_exprs, agg_specs))
        .collect();
    let defer_enabled = correlated_eval.is_some()
        && !stmt.distinct
        && !having_rewritten
            .as_ref()
            .is_some_and(crate::expr_has_subquery)
        && !order_rewritten.iter().any(crate::expr_has_subquery);
    let deferred: Vec<(usize, Expr)> = if defer_enabled {
        items_rewritten
            .iter()
            .enumerate()
            .filter_map(|(i, r)| {
                r.as_ref()
                    .filter(|e| crate::expr_has_subquery(e))
                    .map(|e| (i, e.clone()))
            })
            .collect()
    } else {
        Vec::new()
    };
    // v7.32 (architecture v2, P2) — compile the per-group synth-row
    // expressions ONCE. The projection / HAVING here run per GROUP
    // (24k for the inbox shape) × per item; the rewritten exprs are
    // mostly `Column(__agg_N)` / `Column(__grp_K)` against the synth
    // schema — flat step programs, no tree walk per group.
    let having_compiled = having_rewritten
        .as_ref()
        .filter(|h| eval::fully_compilable(h))
        .map(|h| eval::compile_expr(h, &synth_ctx));
    let items_compiled: Vec<Option<eval::CompiledExpr>> = items_rewritten
        .iter()
        .enumerate()
        .map(|(i, r)| {
            r.as_ref()
                .filter(|e| !deferred.iter().any(|(c, _)| *c == i) && eval::fully_compilable(e))
                .map(|e| eval::compile_expr(e, &synth_ctx))
        })
        .collect();
    let mut kept_synth: Vec<Row> = Vec::new();
    let mut out_rows: Vec<Row> = Vec::new();
    let mut stack: Vec<Value> = Vec::new();
    for srow in synth_rows {
        if let Some(hc) = &having_compiled {
            let cond = eval::eval_compiled(hc, &srow, &synth_ctx, &mut stack)?;
            if !matches!(cond, Value::Bool(true)) {
                continue;
            }
        } else if let Some(h) = &having_rewritten {
            let cond = match correlated_eval {
                Some(f) if crate::expr_has_subquery(h) => f(h, &srow, &synth_ctx)?,
                _ => eval::eval_expr(h, &srow, &synth_ctx)?,
            };
            if !matches!(cond, Value::Bool(true)) {
                continue;
            }
        }
        // v7.37.x — when caller pre-truncates via ORDER BY+LIMIT, skip
        // per-item projection here; the caller fills the placeholder
        // out_rows from the top-K survivors below.
        if defer_projection {
            kept_synth.push(srow);
            out_rows.push(Row::new(Vec::new()));
            continue;
        }
        let mut values: Vec<Value> = Vec::with_capacity(columns.len());
        for (i, rewritten) in items_rewritten.iter().enumerate() {
            let Some(rewritten) = rewritten else { continue };
            if deferred.iter().any(|(c, _)| *c == i) {
                values.push(Value::Null);
                continue;
            }
            values.push(if let Some(cc) = &items_compiled[i] {
                eval::eval_compiled(cc, &srow, &synth_ctx, &mut stack)?
            } else {
                match correlated_eval {
                    Some(f) if crate::expr_has_subquery(rewritten) => {
                        f(rewritten, &srow, &synth_ctx)?
                    }
                    _ => eval::eval_expr(rewritten, &srow, &synth_ctx)?,
                }
            });
        }
        kept_synth.push(srow);
        out_rows.push(Row::new(values));
    }
    let deferred_project_state = if defer_projection {
        Some(DeferredProject {
            items_rewritten,
            items_compiled,
        })
    } else {
        None
    };
    Ok(Projection {
        columns,
        out_rows,
        kept_synth,
        deferred,
        order_rewritten,
        deferred_project: deferred_project_state,
    })
}

/// (4) Sort the projected output by the rewritten ORDER BY keys. The
/// synth rows ride through the sort so deferred subqueries evaluate
/// against the surviving groups after the caller's LIMIT truncation.
fn sort_synth_by_order_by(
    synth_schema: &[ColumnSchema],
    order_by: &[spg_sql::ast::OrderBy],
    order_rewritten: &[Expr],
    mut kept_synth: Vec<Row>,
    mut out_rows: Vec<Row>,
    correlated_eval: Option<CorrelatedEval<'_>>,
    keep_n: Option<usize>,
) -> Result<(Vec<Row>, Vec<Row>), EvalError> {
    let synth_ctx = EvalContext::new(synth_schema, None);
    // v6.4.0 — multi-key ORDER BY on aggregate output. Each key
    // gets its own rewrite + per-key DESC flag. (Rewrites hoisted
    // above as `order_rewritten` — shared with the deferral
    // safety check.)
    let keys_meta: Vec<(bool, Option<bool>)> =
        order_by.iter().map(|o| (o.desc, o.nulls_first)).collect();
    // P2: compile order-by keys once (per-group sort keys are
    // the same `__agg_N` / `__grp_K` shape as the projection).
    let order_compiled: Vec<Option<eval::CompiledExpr>> = order_rewritten
        .iter()
        .map(|e| {
            Some(e)
                .filter(|e| eval::fully_compilable(e))
                .map(|e| eval::compile_expr(e, &synth_ctx))
        })
        .collect();
    // The synth row rides through the sort so deferred exprs can
    // evaluate against the surviving groups after the caller's
    // LIMIT truncation.
    let mut keystack: Vec<Value> = Vec::new();
    let mut tagged: Vec<(Vec<Value>, Row, Row)> = Vec::with_capacity(kept_synth.len());
    for (s, o) in kept_synth.into_iter().zip(out_rows) {
        let mut keys = Vec::with_capacity(order_rewritten.len());
        for (e, oc) in order_rewritten.iter().zip(&order_compiled) {
            keys.push(if let Some(oc) = oc {
                eval::eval_compiled(oc, &s, &synth_ctx, &mut keystack)?
            } else {
                match correlated_eval {
                    Some(f) if crate::expr_has_subquery(e) => f(e, &s, &synth_ctx)?,
                    _ => eval::eval_expr(e, &s, &synth_ctx)?,
                }
            });
        }
        tagged.push((keys, s, o));
    }
    let cmp = |a: &(Vec<Value>, Row, Row), b: &(Vec<Value>, Row, Row)| {
        use core::cmp::Ordering;
        for (i, (ka, kb)) in a.0.iter().zip(b.0.iter()).enumerate() {
            let (desc, nf) = keys_meta[i];
            let c = crate::order_by_value_cmp(desc, nf, ka, kb);
            if c != Ordering::Equal {
                return c;
            }
        }
        Ordering::Equal
    };
    // v7.37.3 — top-K partial sort when `keep_n` is small enough to
    // matter (`Some(k)` with `k < tagged.len()` and `k > 0`).
    // `select_nth_unstable_by` partitions in O(N), then we sort the
    // surviving prefix in O(K log K). Total = O(N + K log K) vs
    // O(N log N) the full sort would pay — matches the inbox-listing
    // shape PG uses.
    //
    match keep_n {
        Some(k) if k < tagged.len() && k > 0 => {
            let pivot = k - 1;
            tagged.select_nth_unstable_by(pivot, cmp);
            tagged[..k].sort_by(cmp);
            tagged.truncate(k);
        }
        _ => {
            tagged.sort_by(cmp);
        }
    }
    kept_synth = Vec::with_capacity(tagged.len());
    out_rows = Vec::with_capacity(tagged.len());
    for (_, s, o) in tagged {
        kept_synth.push(s);
        out_rows.push(o);
    }
    Ok((kept_synth, out_rows))
}

/// v7.17.0 — walk the statement again to validate the positional
/// arity of every aggregate call site. Done after AST collection
/// rather than inside `collect_aggregates` so the collector stays
/// infallible; callers in `run()` can do a single early-error
/// exit before any per-row work.
fn validate_agg_arities(stmt: &SelectStatement, _specs: &[AggSpec]) -> Result<(), EvalError> {
    fn walk(e: &Expr) -> Result<(), EvalError> {
        if let Expr::FunctionCall { name, args } = e {
            let lower = name.to_ascii_lowercase();
            let expected: Option<usize> = match lower.as_str() {
                "count_star" => Some(0),
                "count" | "sum" | "avg" | "min" | "max" | "array_agg"
                // v7.17.0 — boolean aggregates also take exactly
                // one arg. `every` is an alias normalised inside
                // collect_aggregates / rewrite_expr.
                | "bool_and" | "bool_or" | "every"
                // v7.32 (round-29) — statistical + bitwise aggregates
                // + single-arg JSON aggregate.
                | "stddev" | "stddev_samp" | "stddev_pop"
                | "variance" | "var_samp" | "var_pop"
                | "bit_and" | "bit_or" | "bit_xor"
                | "json_agg" | "jsonb_agg" => Some(1),
                // v7.32 (round-29) — two-argument aggregates: string_agg,
                // the regression family f(Y, X), and json_object_agg.
                "string_agg"
                | "covar_pop" | "covar_samp" | "corr"
                | "regr_count" | "regr_avgx" | "regr_avgy" | "regr_slope"
                | "regr_intercept" | "regr_r2" | "regr_sxx" | "regr_syy" | "regr_sxy"
                | "json_object_agg" | "jsonb_object_agg" => Some(2),
                _ => None,
            };
            if let Some(want) = expected
                && args.len() != want
            {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::format!("{lower}() takes {want} arg(s), got {}", args.len()),
                });
            }
            for a in args {
                walk(a)?;
            }
        } else if let Expr::Binary { lhs, rhs, .. } = e {
            walk(lhs)?;
            walk(rhs)?;
        } else if let Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. } = e
        {
            walk(expr)?;
        }
        Ok(())
    }
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            walk(expr)?;
        }
    }
    for o in &stmt.order_by {
        walk(&o.expr)?;
    }
    if let Some(h) = &stmt.having {
        walk(h)?;
    }
    Ok(())
}

/// v7.33 (array_agg argmax) — recognise `(array_agg(x ORDER BY y))[1]`,
/// the argmax/argmin idiom: a non-DISTINCT ordered `array_agg`
/// subscripted by the constant 1. Returns `(value_arg, order_by,
/// filter)` on a match. When matched, the whole per-group array build +
/// sort + materialise is replaced by a running first-by-order scalar
/// accumulator and the subscript node is consumed (replaced by the
/// synthetic column). collect_aggregates and rewrite_expr share this one
/// matcher so their `__agg_<i>` assignment stays in lockstep.
fn first_ordered_array_agg(e: &Expr) -> Option<(&Expr, &[spg_sql::ast::OrderBy], Option<&Expr>)> {
    let Expr::ArraySubscript { target, index } = e else {
        return None;
    };
    if !matches!(
        index.as_ref(),
        Expr::Literal(spg_sql::ast::Literal::Integer(1))
    ) {
        return None;
    }
    let Expr::AggregateOrdered {
        call,
        order_by,
        distinct,
        filter,
    } = target.as_ref()
    else {
        return None;
    };
    if *distinct || order_by.is_empty() {
        return None;
    }
    let Expr::FunctionCall { name, args } = call.as_ref() else {
        return None;
    };
    if !name.eq_ignore_ascii_case("array_agg") || args.len() != 1 {
        return None;
    }
    Some((&args[0], order_by, filter.as_deref()))
}

fn collect_aggregates(e: &Expr, out: &mut Vec<AggSpec>) {
    match e {
        // v7.24 (round-16 A) — ordered aggregate: register the inner
        // call's spec with the ordering attached.
        Expr::AggregateOrdered {
            call,
            order_by,
            distinct,
            filter,
        } => {
            if let Expr::FunctionCall { name, args } = call.as_ref() {
                let lower = name.to_ascii_lowercase();
                if is_aggregate_name(&lower) {
                    let canonical = if lower == "every" {
                        "bool_and".to_string()
                    } else {
                        lower
                    };
                    // Ordered-set aggregates (`percentile_cont(f)
                    // WITHIN GROUP (ORDER BY x)`) take the value to
                    // aggregate from the sort spec and the in-parens
                    // arg as the direct (fraction) argument.
                    let ordered_set = is_within_group_name(&canonical);
                    let (arg, direct_arg) = if ordered_set {
                        (
                            order_by.first().map(|o| o.expr.clone()),
                            args.first().cloned(),
                        )
                    } else {
                        (args.first().cloned(), None)
                    };
                    let spec = AggSpec {
                        kind: classify_agg_name(&canonical),
                        name: canonical.clone(),
                        arg,
                        arg2: if agg_uses_second_arg(&canonical) {
                            args.get(1).cloned()
                        } else {
                            None
                        },
                        distinct: *distinct,
                        order_by: order_by.clone(),
                        filter: filter.as_deref().cloned(),
                        direct_arg,
                        first_ordered: false,
                    };
                    if !out.iter().any(|s| {
                        s.name == spec.name
                            && s.arg == spec.arg
                            && s.arg2 == spec.arg2
                            && s.distinct == spec.distinct
                            && s.order_by == spec.order_by
                            && s.filter == spec.filter
                            && s.direct_arg == spec.direct_arg
                            && s.first_ordered == spec.first_ordered
                    }) {
                        out.push(spec);
                    }
                    return;
                }
            }
            collect_aggregates(call, out);
            for o in order_by {
                collect_aggregates(&o.expr, out);
            }
        }
        Expr::FunctionCall { name, args } => {
            let lower = name.to_ascii_lowercase();
            if is_aggregate_name(&lower) {
                let arg = if lower == "count_star" {
                    None
                } else {
                    args.first().cloned()
                };
                // v7.17.0 — second positional arg for
                // `string_agg(value, separator)`; v7.32 — also the
                // regression family `f(Y, X)` and `json_object_agg`.
                let arg2 = if agg_uses_second_arg(&lower) {
                    args.get(1).cloned()
                } else {
                    None
                };
                // v7.17.0 — `every` is the SQL-standard alias for
                // `bool_and`; collapse at collection time so
                // update_state / finalize need only one arm.
                let canonical = if lower == "every" {
                    "bool_and".to_string()
                } else {
                    lower
                };
                let spec = AggSpec {
                    kind: classify_agg_name(&canonical),
                    name: canonical,
                    arg: arg.clone(),
                    arg2: arg2.clone(),
                    distinct: false,
                    order_by: Vec::new(),
                    filter: None,
                    direct_arg: None,
                    first_ordered: false,
                };
                if !out.iter().any(|s| {
                    s.name == spec.name
                        && s.arg == spec.arg
                        && s.arg2 == spec.arg2
                        && !s.distinct
                        && s.order_by == spec.order_by
                        && s.filter.is_none()
                        && !s.first_ordered
                }) {
                    out.push(spec);
                }
                // Don't recurse into the arg — nested aggregates are
                // illegal in standard SQL.
            } else {
                for a in args {
                    collect_aggregates(a, out);
                }
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            collect_aggregates(lhs, out);
            collect_aggregates(rhs, out);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::IsNull { expr, .. } => {
            collect_aggregates(expr, out);
        }
        Expr::Like { expr, pattern, .. } => {
            collect_aggregates(expr, out);
            collect_aggregates(pattern, out);
        }
        Expr::InList { expr, list, .. } => {
            collect_aggregates(expr, out);
            for item in list {
                collect_aggregates(item, out);
            }
        }
        Expr::Extract { source, .. } => collect_aggregates(source, out),
        // v4.10 subquery + v4.12 window / Literal / Column —
        // non-recursing leaves for the aggregate collector.
        Expr::ScalarSubquery(_)
        | Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::WindowFunction { .. }
        | Expr::Literal(_)
        | Expr::Placeholder(_)
        | Expr::Column(_) => {}
        // v7.10.10 — recurse into array constructor children +
        // subscript / ANY/ALL operands.
        Expr::Array(items) => {
            for elem in items {
                collect_aggregates(elem, out);
            }
        }
        Expr::ArraySubscript { target, index } => {
            // v7.33 (array_agg argmax) — `(array_agg(x ORDER BY y))[1]`
            // collects as a first_ordered spec; the subscript is consumed
            // here (do NOT recurse into the array_agg, or it would also
            // register a plain full-array spec).
            if let Some((arg, order_by, filter)) = first_ordered_array_agg(e) {
                let spec = AggSpec {
                    kind: AggKind::ArrayAgg,
                    name: "array_agg".to_string(),
                    arg: Some(arg.clone()),
                    arg2: None,
                    distinct: false,
                    order_by: order_by.to_vec(),
                    filter: filter.cloned(),
                    direct_arg: None,
                    first_ordered: true,
                };
                if !out.iter().any(|s| {
                    s.name == spec.name
                        && s.arg == spec.arg
                        && s.order_by == spec.order_by
                        && s.filter == spec.filter
                        && s.first_ordered
                }) {
                    out.push(spec);
                }
                return;
            }
            collect_aggregates(target, out);
            collect_aggregates(index, out);
        }
        Expr::AnyAll { expr, array, .. } => {
            collect_aggregates(expr, out);
            collect_aggregates(array, out);
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                collect_aggregates(o, out);
            }
            for (w, t) in branches {
                collect_aggregates(w, out);
                collect_aggregates(t, out);
            }
            if let Some(e) = else_branch {
                collect_aggregates(e, out);
            }
        }
    }
}

fn update_state(
    st: &mut AggState,
    kind: AggKind,
    name: &str,
    v: &Value,
    arg2: Option<&Value>,
    order_keys: Option<Vec<Value>>,
) -> Result<(), EvalError> {
    let is_null = matches!(v, Value::Null);
    // v7.37.4 (R34) — dispatch by pre-classified `kind` (`Copy`
    // enum), not by per-row string match. Hot inner loop on
    // multi-aggregate queries (mailrs `/api/conversations`: 14
    // aggregates × 100 k rows = 1.4 M dispatches) sees an enum
    // jump table instead of a sequence of `eq_str` checks. `name`
    // is still threaded through for error messages so the user-
    // facing wording is unchanged.
    match kind {
        AggKind::CountStar => st.count += 1,
        AggKind::Count => {
            if !is_null {
                st.count += 1;
            }
        }
        AggKind::Sum | AggKind::Avg => {
            if is_null {
                return Ok(());
            }
            st.count += 1;
            match v {
                Value::Int(n) => st.sum_int += i64::from(*n),
                Value::BigInt(n) => st.sum_int += *n,
                Value::Float(x) => {
                    st.use_float = true;
                    st.sum_float += *x;
                }
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!("sum/avg need numeric, got {:?}", other.data_type()),
                    });
                }
            }
        }
        AggKind::Min => {
            if is_null {
                return Ok(());
            }
            match &st.extreme {
                None => st.extreme = Some(v.clone()),
                Some(cur) => {
                    if value_cmp(v, cur) == core::cmp::Ordering::Less {
                        st.extreme = Some(v.clone());
                    }
                }
            }
        }
        AggKind::Max => {
            if is_null {
                return Ok(());
            }
            match &st.extreme {
                None => st.extreme = Some(v.clone()),
                Some(cur) => {
                    if value_cmp(v, cur) == core::cmp::Ordering::Greater {
                        st.extreme = Some(v.clone());
                    }
                }
            }
        }
        // v7.17.0 — string_agg(value, separator). NULL value is
        // skipped (PG aggregate-skip-null). Separator captured
        // from the latest row that flows through; matches PG's
        // semantics of evaluating the separator per row but using
        // the last value at finalize time (in practice it's
        // constant). count is bumped so we can distinguish "empty
        // group → NULL" from "all-NULL group → NULL".
        AggKind::StringAgg => {
            if let Some(sep) = arg2
                && let Value::Text(s) = sep
            {
                st.separator = Some(s.clone());
            }
            if is_null {
                return Ok(());
            }
            if let Value::Text(s) = v {
                st.items.push(Value::Text(s.clone()));
                if let Some(k) = order_keys {
                    st.item_keys.push(k);
                }
                st.count += 1;
            } else {
                return Err(EvalError::TypeMismatch {
                    detail: format!("string_agg requires text value, got {:?}", v.data_type()),
                });
            }
        }
        // v7.17.0 — array_agg(value). Unlike string_agg, NULL
        // elements are KEPT in the array (PG behaviour); the
        // result is NULL only when ZERO rows fed in. Element type
        // is locked from the first row's value type; subsequent
        // rows must match (PG also rejects mixed-type array_agg).
        AggKind::ArrayAgg => {
            st.items.push(v.clone());
            if let Some(k) = order_keys {
                st.item_keys.push(k);
            }
            st.count += 1;
        }
        // v7.17.0 — bool_and(p): TRUE iff every non-NULL input is
        // TRUE. NULL skipped; running accumulator stays at TRUE
        // until the first non-NULL FALSE.
        AggKind::BoolAnd => {
            if is_null {
                return Ok(());
            }
            let b = match v {
                Value::Bool(b) => *b,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!("bool_and requires bool, got {:?}", other.data_type()),
                    });
                }
            };
            st.bool_acc = Some(st.bool_acc.map_or(b, |acc| acc && b));
        }
        // v7.17.0 — bool_or(p): TRUE iff any non-NULL input is
        // TRUE. NULL skipped.
        AggKind::BoolOr => {
            if is_null {
                return Ok(());
            }
            let b = match v {
                Value::Bool(b) => *b,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!("bool_or requires bool, got {:?}", other.data_type()),
                    });
                }
            };
            st.bool_acc = Some(st.bool_acc.map_or(b, |acc| acc || b));
        }
        // v7.32 (round-29) — variance / stddev family. Accumulate the
        // running sum (sum_float) and sum of squares (sum_sq) over the
        // non-NULL numeric inputs; finalize divides by n or n-1.
        AggKind::StddevFamily => {
            if is_null {
                return Ok(());
            }
            let x = match v {
                Value::Int(n) => f64::from(*n),
                Value::SmallInt(n) => f64::from(*n),
                Value::BigInt(n) => *n as f64,
                Value::Float(x) => *x,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!("{name} needs numeric, got {:?}", other.data_type()),
                    });
                }
            };
            st.count += 1;
            st.sum_float += x;
            st.sum_sq += x * x;
        }
        // v7.32 (round-29) — bitwise aggregates over integer inputs.
        AggKind::BitAnd | AggKind::BitOr | AggKind::BitXor => {
            if is_null {
                return Ok(());
            }
            let n = match v {
                Value::Int(n) => i64::from(*n),
                Value::SmallInt(n) => i64::from(*n),
                Value::BigInt(n) => *n,
                other => {
                    return Err(EvalError::TypeMismatch {
                        detail: format!("{name} needs integer, got {:?}", other.data_type()),
                    });
                }
            };
            st.bit_acc = Some(match (st.bit_acc, kind) {
                (None, _) => n,
                (Some(acc), AggKind::BitAnd) => acc & n,
                (Some(acc), AggKind::BitOr) => acc | n,
                (Some(acc), _) => acc ^ n, // BitXor
            });
        }
        // v7.32 (round-29) — WITHIN GROUP aggregates (ordered-set +
        // hypothetical-set) collect the sort value (NULLs ignored, per
        // PG) into `items`, sorted at finalize by the parallel
        // `item_keys`.
        AggKind::WithinGroup => {
            if is_null {
                return Ok(());
            }
            st.items.push(v.clone());
            if let Some(k) = order_keys {
                st.item_keys.push(k);
            }
            st.count += 1;
        }
        // v7.32 (round-29) — regression family f(Y, X). Only rows with
        // BOTH inputs non-NULL contribute (PG semantics). `v` is Y,
        // `arg2` is X.
        AggKind::Regression => {
            let (Some(y), Some(x)) = (agg_value_to_f64(v), arg2.and_then(agg_value_to_f64)) else {
                return Ok(()); // NULL (or non-numeric) in either input
            };
            st.reg_n += 1;
            st.reg_sx += x;
            st.reg_sy += y;
            st.reg_sxx += x * x;
            st.reg_syy += y * y;
            st.reg_sxy += x * y;
        }
        // v7.32 (round-29) — json_agg / jsonb_agg collect every input
        // (NULL becomes JSON null, per PG) in row order.
        AggKind::JsonAgg => {
            st.items.push(v.clone());
            st.count += 1;
        }
        // v7.32 (round-29) — json_object_agg(key, value): keys in
        // `items`, values in `aux_items`. A NULL key is skipped (PG
        // raises; we drop it rather than abort the whole query).
        AggKind::JsonObjectAgg => {
            if is_null {
                return Ok(());
            }
            st.items.push(v.clone());
            st.aux_items.push(arg2.cloned().unwrap_or(Value::Null));
            st.count += 1;
        }
    }
    Ok(())
}

#[allow(clippy::cast_precision_loss)]
fn finalize(name: &str, st: &AggState) -> Value {
    match name {
        "count" | "count_star" => Value::BigInt(st.count),
        "sum" => {
            if st.count == 0 {
                Value::Null
            } else if st.use_float {
                Value::Float(st.sum_float + (st.sum_int as f64))
            } else {
                Value::BigInt(st.sum_int)
            }
        }
        "avg" => {
            if st.count == 0 {
                Value::Null
            } else {
                let total = if st.use_float {
                    st.sum_float + (st.sum_int as f64)
                } else {
                    st.sum_int as f64
                };
                Value::Float(total / (st.count as f64))
            }
        }
        "min" | "max" => st.extreme.clone().unwrap_or(Value::Null),
        // v7.17.0 — string_agg: join all collected text items with
        // the captured separator. Empty / all-NULL group → NULL
        // (PG semantics).
        "string_agg" => {
            if st.items.is_empty() {
                return Value::Null;
            }
            let sep = st.separator.clone().unwrap_or_default();
            let mut out = String::new();
            for (i, item) in st.items.iter().enumerate() {
                if i > 0 {
                    out.push_str(&sep);
                }
                if let Value::Text(s) = item {
                    out.push_str(s);
                }
            }
            Value::Text(out)
        }
        // v7.17.0 — array_agg: collect into a typed array. NULL
        // elements are preserved per PG. Result type is decided
        // by the first non-NULL element seen (or Text fallback
        // when the whole group is NULL — PG would surface the
        // declared input type, but SPG hasn't yet wired the
        // aggregate's static input-type from `describe`).
        "array_agg" => {
            if st.items.is_empty() {
                return Value::Null;
            }
            let probe = st.items.iter().find(|v| !v.is_null());
            match probe.and_then(spg_storage::Value::data_type) {
                Some(DataType::Int) | Some(DataType::SmallInt) => {
                    let items: Vec<Option<i32>> = st
                        .items
                        .iter()
                        .map(|v| match v {
                            Value::Int(n) => Some(*n),
                            Value::SmallInt(n) => Some(i32::from(*n)),
                            _ => None,
                        })
                        .collect();
                    Value::IntArray(items)
                }
                Some(DataType::BigInt) => {
                    let items: Vec<Option<i64>> = st
                        .items
                        .iter()
                        .map(|v| match v {
                            Value::BigInt(n) => Some(*n),
                            _ => None,
                        })
                        .collect();
                    Value::BigIntArray(items)
                }
                _ => {
                    let items: Vec<Option<String>> = st
                        .items
                        .iter()
                        .map(|v| match v {
                            Value::Text(s) => Some(s.clone()),
                            Value::Null => None,
                            other => Some(format!("{other:?}")),
                        })
                        .collect();
                    Value::TextArray(items)
                }
            }
        }
        // v7.17.0 — bool_and / bool_or finalize: lazy-init pattern
        // means `None` is exactly "empty group or all-NULL", which
        // PG surfaces as SQL NULL.
        "bool_and" | "bool_or" => st.bool_acc.map_or(Value::Null, Value::Bool),
        // v7.32 (round-29) — variance / stddev. PG: `variance` ==
        // `var_samp`, `stddev` == `stddev_samp`. samp needs n >= 2
        // (n < 2 → NULL); pop needs n >= 1 (n == 1 → 0).
        "variance" | "var_samp" | "var_pop" | "stddev" | "stddev_samp" | "stddev_pop" => {
            let n = st.count;
            if n == 0 {
                return Value::Null;
            }
            let nf = n as f64;
            // Sum of squared deviations from the mean.
            let ss = st.sum_sq - (st.sum_float * st.sum_float) / nf;
            let pop = name.ends_with("_pop");
            let denom = if pop { nf } else { nf - 1.0 };
            if denom <= 0.0 {
                // var_samp / stddev (samp) with n == 1 → NULL.
                return Value::Null;
            }
            let var = (ss / denom).max(0.0); // clamp fp noise below 0
            if name.starts_with("stddev") {
                Value::Float(crate::eval::f64_sqrt(var))
            } else {
                Value::Float(var)
            }
        }
        // v7.32 (round-29) — bitwise aggregates: None (empty / all-NULL)
        // → SQL NULL.
        "bit_and" | "bit_or" | "bit_xor" => st.bit_acc.map_or(Value::Null, Value::BigInt),
        // v7.32 (round-29) — regression family. `regr_count` is the
        // paired n; everything else is NULL over an empty set. Terms
        // are the mean-centred sums of squares / cross-products.
        "regr_count" => Value::BigInt(st.reg_n),
        "covar_pop" | "covar_samp" | "corr" | "regr_avgx" | "regr_avgy" | "regr_slope"
        | "regr_intercept" | "regr_r2" | "regr_sxx" | "regr_syy" | "regr_sxy" => {
            let n = st.reg_n;
            if n == 0 {
                return Value::Null;
            }
            let nf = n as f64;
            let sxx = st.reg_sxx - st.reg_sx * st.reg_sx / nf;
            let syy = st.reg_syy - st.reg_sy * st.reg_sy / nf;
            let sxy = st.reg_sxy - st.reg_sx * st.reg_sy / nf;
            let avgx = st.reg_sx / nf;
            let avgy = st.reg_sy / nf;
            let out = match name {
                "regr_avgx" => Some(avgx),
                "regr_avgy" => Some(avgy),
                "regr_sxx" => Some(sxx),
                "regr_syy" => Some(syy),
                "regr_sxy" => Some(sxy),
                "covar_pop" => Some(sxy / nf),
                "covar_samp" => (n >= 2).then(|| sxy / (nf - 1.0)),
                "regr_slope" => (sxx != 0.0).then(|| sxy / sxx),
                "regr_intercept" => (sxx != 0.0).then(|| avgy - (sxy / sxx) * avgx),
                "corr" => {
                    let d = sxx * syy;
                    (d > 0.0).then(|| sxy / crate::eval::f64_sqrt(d))
                }
                // PG: NULL when sxx==0; 1 when syy==0 (and sxx>0).
                "regr_r2" => {
                    if sxx == 0.0 {
                        None
                    } else if syy == 0.0 {
                        Some(1.0)
                    } else {
                        Some((sxy * sxy) / (sxx * syy))
                    }
                }
                _ => None,
            };
            out.map_or(Value::Null, Value::Float)
        }
        // v7.32 (round-29) — json_agg / jsonb_agg: a JSON array of every
        // collected element in row order; empty set → SQL NULL.
        "json_agg" | "jsonb_agg" => {
            if st.items.is_empty() {
                return Value::Null;
            }
            let mut out = String::from("[");
            for (i, item) in st.items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&crate::json::value_to_json_text(item));
            }
            out.push(']');
            Value::Json(out)
        }
        // v7.32 (round-29) — json_object_agg: a JSON object built from
        // the parallel key (`items`) / value (`aux_items`) streams.
        "json_object_agg" | "jsonb_object_agg" => {
            if st.items.is_empty() {
                return Value::Null;
            }
            let mut out = String::from("{");
            for (i, key) in st.items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                // Object keys are always JSON strings (PG coerces).
                let key_text = match key {
                    Value::Text(s) | Value::Json(s) => s.clone(),
                    other => crate::json::value_to_json_text(other),
                };
                out.push_str(&crate::json::value_to_json_text(&Value::Text(key_text)));
                out.push_str(": ");
                let val = st.aux_items.get(i).unwrap_or(&Value::Null);
                out.push_str(&crate::json::value_to_json_text(val));
            }
            out.push('}');
            Value::Json(out)
        }
        // Ordered-set aggregates are finalized in `run` (they need the
        // sorted items + the direct fraction argument), never here.
        _ => unreachable!(),
    }
}

/// v7.32 (round-29) — numeric coercion for the percentile interpolation.
fn agg_value_to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(n) => Some(f64::from(*n)),
        Value::SmallInt(n) => Some(f64::from(*n)),
        Value::BigInt(n) => Some(*n as f64),
        Value::Float(x) => Some(*x),
        _ => None,
    }
}

/// v7.32 (round-29) — finalize a WITHIN GROUP aggregate. `st.items` is
/// already sorted by the `WITHIN GROUP (ORDER BY …)` spec. `direct` is
/// the evaluated direct argument: the fraction for `percentile_*`, the
/// hypothetical value for the hypothetical-set family (`rank` etc.),
/// and unused by `mode`. `order` is the (single) sort key, needed by
/// the hypothetical-set family to compare in the sort direction.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn finalize_ordered_set(
    name: &str,
    st: &AggState,
    direct: Option<&Value>,
    order: Option<&spg_sql::ast::OrderBy>,
) -> Value {
    let fraction = direct;
    let items = &st.items;
    if items.is_empty() {
        // A hypothetical row ranks first over an empty group; the
        // distribution functions are 0 / divide-by-(n+1).
        return match name {
            "rank" | "dense_rank" => Value::BigInt(1),
            "percent_rank" => Value::Float(0.0),
            "cume_dist" => Value::Float(1.0),
            _ => Value::Null,
        };
    }
    let n = items.len();
    match name {
        // v7.32 (round-29) — hypothetical-set: the rank the direct value
        // would have if inserted into the group, in the sort direction.
        "rank" | "dense_rank" | "percent_rank" | "cume_dist" => {
            let Some(h) = fraction else {
                return Value::Null;
            };
            let (desc, nulls_first) = order.map_or((false, None), |o| (o.desc, o.nulls_first));
            let mut before = 0usize; // sort strictly before h
            let mut before_or_eq = 0usize; // sort before-or-peer with h
            let mut distinct_before = 0usize;
            let mut last_before: Option<&Value> = None;
            for it in items {
                match crate::order_by_value_cmp(desc, nulls_first, it, h) {
                    core::cmp::Ordering::Less => {
                        before += 1;
                        before_or_eq += 1;
                        if last_before
                            .is_none_or(|p| value_cmp(p, it) != core::cmp::Ordering::Equal)
                        {
                            distinct_before += 1;
                            last_before = Some(it);
                        }
                    }
                    core::cmp::Ordering::Equal => before_or_eq += 1,
                    core::cmp::Ordering::Greater => {}
                }
            }
            let nn = n as f64;
            match name {
                "rank" => Value::BigInt((before + 1) as i64),
                "dense_rank" => Value::BigInt((distinct_before + 1) as i64),
                "percent_rank" => Value::Float(before as f64 / nn),
                "cume_dist" => Value::Float((before_or_eq as f64 + 1.0) / (nn + 1.0)),
                _ => unreachable!(),
            }
        }
        // Most frequent value; equal values are adjacent in the sorted
        // run, and a frequency tie resolves to the earliest run (the
        // smallest value under an ascending sort), matching PG.
        "mode" => {
            let (mut best_i, mut best_cnt) = (0usize, 1usize);
            let (mut run_i, mut run_cnt) = (0usize, 1usize);
            for i in 1..n {
                if value_cmp(&items[i], &items[run_i]) == core::cmp::Ordering::Equal {
                    run_cnt += 1;
                } else {
                    run_i = i;
                    run_cnt = 1;
                }
                if run_cnt > best_cnt {
                    best_cnt = run_cnt;
                    best_i = run_i;
                }
            }
            items[best_i].clone()
        }
        // The first value whose cumulative fraction reaches `f`.
        "percentile_disc" => {
            let f = fraction
                .and_then(agg_value_to_f64)
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            let idx = if f <= 0.0 {
                0
            } else {
                (crate::eval::f64_ceil(f * n as f64) as usize)
                    .saturating_sub(1)
                    .min(n - 1)
            };
            items[idx].clone()
        }
        // Linear interpolation between the two bracketing values.
        "percentile_cont" => {
            let f = fraction
                .and_then(agg_value_to_f64)
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            let Some(nums) = items
                .iter()
                .map(agg_value_to_f64)
                .collect::<Option<Vec<f64>>>()
            else {
                return Value::Null; // non-numeric ordered set
            };
            if n == 1 {
                return Value::Float(nums[0]);
            }
            let rank = f * (n as f64 - 1.0);
            let lo = crate::eval::f64_floor(rank) as usize;
            let hi = crate::eval::f64_ceil(rank) as usize;
            let frac = rank - lo as f64;
            Value::Float(nums[lo] + (nums[hi] - nums[lo]) * frac)
        }
        _ => unreachable!(),
    }
}

fn infer_agg_type(spec: &AggSpec, schema_cols: &[ColumnSchema]) -> DataType {
    // v7.26 (round-20 C) — the argument's statically-derived shape
    // types MIN/MAX/SUM/array_agg properly; RowDescription used to
    // report TEXT for these, breaking every sqlx typed decode.
    let arg_ty = spec
        .arg
        .as_ref()
        .and_then(|a| crate::describe::describe_expr(a, schema_cols))
        .map(|shape| shape.ty);
    // v7.33 (array_agg argmax) — `(array_agg(x ORDER BY y))[1]` yields the
    // ELEMENT type (x), not the array type.
    if spec.first_ordered {
        return arg_ty.unwrap_or(DataType::Text);
    }
    match spec.name.as_str() {
        "count" | "count_star" => DataType::BigInt,
        "sum" => match arg_ty {
            Some(DataType::Float) => DataType::Float,
            _ => DataType::BigInt,
        },
        "avg" => DataType::Float,
        // v7.17.0 — string_agg always returns TEXT.
        "string_agg" => DataType::Text,
        "array_agg" => match arg_ty {
            Some(DataType::Int | DataType::SmallInt) => DataType::IntArray,
            Some(DataType::BigInt) => DataType::BigIntArray,
            _ => DataType::TextArray,
        },
        // v7.17.0 — boolean aggregates always return BOOL (nullable
        // — empty / all-NULL group → NULL).
        "bool_and" | "bool_or" => DataType::Bool,
        // v7.32 (round-29) — variance / stddev are floating point;
        // percentile_cont interpolates to float; the regression family
        // (except regr_count) is floating point.
        "stddev" | "stddev_samp" | "stddev_pop" | "variance" | "var_samp" | "var_pop"
        | "percentile_cont" | "covar_pop" | "covar_samp" | "corr" | "regr_avgx" | "regr_avgy"
        | "regr_slope" | "regr_intercept" | "regr_r2" | "regr_sxx" | "regr_syy" | "regr_sxy" => {
            DataType::Float
        }
        // v7.32 (round-29) — bitwise aggregates, regr_count, and the
        // integer hypothetical-set ranks return an integer.
        "bit_and" | "bit_or" | "bit_xor" | "regr_count" | "rank" | "dense_rank" => DataType::BigInt,
        // v7.32 (round-29) — hypothetical-set distribution functions.
        "percent_rank" | "cume_dist" => DataType::Float,
        // v7.32 (round-29) — JSON aggregates return JSON.
        "json_agg" | "jsonb_agg" | "json_object_agg" | "jsonb_object_agg" => DataType::Json,
        // min/max, percentile_disc, mode, and anything pass-through:
        // the argument's shape (for ordered-set aggs `spec.arg` is the
        // WITHIN GROUP value expression).
        _ => arg_ty.unwrap_or(DataType::Text),
    }
}

fn agg_or_group_type(e: &Expr, synth: &[ColumnSchema]) -> DataType {
    if let Expr::Column(c) = e
        && let Some(s) = synth.iter().find(|s| s.name == c.name)
    {
        return s.ty;
    }
    // v7.26 (round-20 C) — compound expressions over aggregates
    // (COALESCE(BOOL_OR(…), false), (array_agg(…))[1], CASE …)
    // derive their shape statically against the synth schema; the
    // old Text fallback broke sqlx typed decodes of exactly these
    // columns.
    crate::describe::describe_expr(e, synth)
        .map(|shape| shape.ty)
        .unwrap_or(DataType::Text)
}

fn rewrite_expr(e: &Expr, group_exprs: &[Expr], aggs: &[AggSpec]) -> Expr {
    // v7.33 (array_agg argmax) — `(array_agg(x ORDER BY y))[1]` rewrites
    // to its first_ordered synth column, consuming the subscript. Checked
    // before the AggregateOrdered/recursion arms (which would otherwise
    // rewrite the inner array_agg and leave the subscript). Same matcher
    // as collect_aggregates, so the spec it finds is the one collected.
    if let Some((arg, order_by, filter)) = first_ordered_array_agg(e) {
        let arg_owned = Some(arg.clone());
        let filter_owned = filter.cloned();
        for (i, spec) in aggs.iter().enumerate() {
            if spec.first_ordered
                && spec.name == "array_agg"
                && spec.arg == arg_owned
                && spec.order_by == *order_by
                && spec.filter == filter_owned
            {
                return Expr::Column(spg_sql::ast::ColumnName {
                    qualifier: None,
                    name: format!("__agg_{i}"),
                });
            }
        }
    }
    // v7.24 (round-16 A) — ordered aggregate: match on the inner
    // call PLUS the ordering keys.
    if let Expr::AggregateOrdered {
        call,
        order_by,
        distinct,
        filter,
    } = e
        && let Expr::FunctionCall { name, args } = call.as_ref()
    {
        let lower = name.to_ascii_lowercase();
        if is_aggregate_name(&lower) {
            let canonical: &str = if lower == "every" { "bool_and" } else { &lower };
            // Mirror collect_aggregates: ordered-set aggregates take the
            // value from the sort spec and the in-parens arg as direct.
            let (arg, direct_arg) = if is_within_group_name(canonical) {
                (
                    order_by.first().map(|o| o.expr.clone()),
                    args.first().cloned(),
                )
            } else {
                (args.first().cloned(), None)
            };
            let arg2 = if agg_uses_second_arg(canonical) {
                args.get(1).cloned()
            } else {
                None
            };
            let filter_owned = filter.as_deref().cloned();
            for (i, spec) in aggs.iter().enumerate() {
                if spec.name == canonical
                    && spec.arg == arg
                    && spec.arg2 == arg2
                    && spec.distinct == *distinct
                    && spec.order_by == *order_by
                    && spec.filter == filter_owned
                    && spec.direct_arg == direct_arg
                {
                    return Expr::Column(spg_sql::ast::ColumnName {
                        qualifier: None,
                        name: format!("__agg_{i}"),
                    });
                }
            }
        }
    }
    // Match aggregate FunctionCalls first — they sit outside group_by.
    if let Expr::FunctionCall { name, args } = e {
        let lower = name.to_ascii_lowercase();
        if is_aggregate_name(&lower) {
            let arg = if lower == "count_star" {
                None
            } else {
                args.first().cloned()
            };
            // v7.17.0 — match the spec we registered for
            // string_agg(value, separator) on the full pair; v7.32 also
            // the regression family and json_object_agg.
            let arg2 = if agg_uses_second_arg(&lower) {
                args.get(1).cloned()
            } else {
                None
            };
            // v7.17.0 — `every` collapses into `bool_and` at
            // collection; mirror that here so the rewrite finds
            // the matching synth column.
            let canonical: &str = if lower == "every" {
                "bool_and"
            } else {
                lower.as_str()
            };
            for (i, spec) in aggs.iter().enumerate() {
                if spec.name == canonical
                    && spec.arg == arg
                    && spec.arg2 == arg2
                    && !spec.distinct
                    && spec.order_by.is_empty()
                {
                    return Expr::Column(spg_sql::ast::ColumnName {
                        qualifier: None,
                        name: format!("__agg_{i}"),
                    });
                }
            }
        }
    }
    // Match a group_by expression by AST equality.
    for (i, g) in group_exprs.iter().enumerate() {
        if g == e {
            return Expr::Column(spg_sql::ast::ColumnName {
                qualifier: None,
                name: format!("__grp_{i}"),
            });
        }
    }
    // Recurse into children.
    match e {
        Expr::AggregateOrdered {
            call,
            order_by,
            distinct,
            filter,
        } => Expr::AggregateOrdered {
            call: Box::new(rewrite_expr(call, group_exprs, aggs)),
            distinct: *distinct,
            order_by: order_by
                .iter()
                .map(|o| spg_sql::ast::OrderBy {
                    expr: rewrite_expr(&o.expr, group_exprs, aggs),
                    desc: o.desc,
                    nulls_first: o.nulls_first,
                })
                .collect(),
            // The filter is evaluated against SOURCE rows during
            // accumulation, never against synth rows — keep it as-is.
            filter: filter.clone(),
        },
        Expr::Binary { lhs, op, rhs } => Expr::Binary {
            lhs: Box::new(rewrite_expr(lhs, group_exprs, aggs)),
            op: *op,
            rhs: Box::new(rewrite_expr(rhs, group_exprs, aggs)),
        },
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: Box::new(rewrite_expr(expr, group_exprs, aggs)),
        },
        Expr::Cast { expr, target } => Expr::Cast {
            expr: Box::new(rewrite_expr(expr, group_exprs, aggs)),
            target: target.clone(),
        },
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(rewrite_expr(expr, group_exprs, aggs)),
            negated: *negated,
        },
        Expr::FunctionCall { name, args } => Expr::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| rewrite_expr(a, group_exprs, aggs))
                .collect(),
        },
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => Expr::Like {
            expr: Box::new(rewrite_expr(expr, group_exprs, aggs)),
            pattern: Box::new(rewrite_expr(pattern, group_exprs, aggs)),
            negated: *negated,
            case_insensitive: *case_insensitive,
        },
        Expr::Extract { field, source } => Expr::Extract {
            field: *field,
            source: Box::new(rewrite_expr(source, group_exprs, aggs)),
        },
        // v7.25.2 (round-19 A) — subquery nodes: rewrite group-key
        // references INSIDE the body to `__grp_N` so the correlated
        // resolver can substitute them against the synthesised group
        // row (aggs are NOT matched inside the body — a COUNT in the
        // subquery is the subquery's own aggregate).
        Expr::ScalarSubquery(s) => {
            Expr::ScalarSubquery(Box::new(rewrite_group_keys_in_select(s, group_exprs)))
        }
        Expr::Exists { subquery, negated } => Expr::Exists {
            subquery: Box::new(rewrite_group_keys_in_select(subquery, group_exprs)),
            negated: *negated,
        },
        Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => Expr::InSubquery {
            expr: Box::new(rewrite_expr(expr, group_exprs, aggs)),
            subquery: Box::new(rewrite_group_keys_in_select(subquery, group_exprs)),
            negated: *negated,
        },
        // v4.12 window / Literal / Column — clone-pass (these don't
        // participate in aggregate rewrite).
        Expr::WindowFunction { .. } | Expr::Literal(_) | Expr::Placeholder(_) | Expr::Column(_) => {
            e.clone()
        }
        // v7.10.10 — recurse children for array nodes.
        Expr::Array(items) => Expr::Array(
            items
                .iter()
                .map(|elem| rewrite_expr(elem, group_exprs, aggs))
                .collect(),
        ),
        Expr::ArraySubscript { target, index } => Expr::ArraySubscript {
            target: Box::new(rewrite_expr(target, group_exprs, aggs)),
            index: Box::new(rewrite_expr(index, group_exprs, aggs)),
        },
        Expr::AnyAll {
            expr,
            op,
            array,
            is_any,
        } => Expr::AnyAll {
            expr: Box::new(rewrite_expr(expr, group_exprs, aggs)),
            op: *op,
            array: Box::new(rewrite_expr(array, group_exprs, aggs)),
            is_any: *is_any,
        },
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(rewrite_expr(expr, group_exprs, aggs)),
            list: list
                .iter()
                .map(|item| rewrite_expr(item, group_exprs, aggs))
                .collect(),
            negated: *negated,
        },
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => Expr::Case {
            operand: operand
                .as_deref()
                .map(|o| Box::new(rewrite_expr(o, group_exprs, aggs))),
            branches: branches
                .iter()
                .map(|(w, t)| {
                    (
                        rewrite_expr(w, group_exprs, aggs),
                        rewrite_expr(t, group_exprs, aggs),
                    )
                })
                .collect(),
            else_branch: else_branch
                .as_deref()
                .map(|e| Box::new(rewrite_expr(e, group_exprs, aggs))),
        },
    }
}

/// v7.25.2 (round-19 A) — rewrite group-key references inside a
/// subquery body to `__grp_N` synthetic columns (aggregates are
/// not touched: empty spec list). Runs through the canonical
/// Select walker so every expression slot is covered.
fn rewrite_group_keys_in_select(
    s: &spg_sql::ast::SelectStatement,
    group_exprs: &[Expr],
) -> spg_sql::ast::SelectStatement {
    let mut out = s.clone();
    let _ = crate::walk_select_exprs_mut(&mut out, &mut |e| {
        *e = rewrite_expr(e, group_exprs, &[]);
        Ok(())
    });
    out
}

/// Canonical string key for a tuple of group values. Used as map key.
/// Per-value group-key encoding (shared by owned and borrowed paths).
fn encode_one(out: &mut String, v: &Value) {
    use core::fmt::Write;
    match v {
        Value::Null => out.push_str("N|"),
        // v7.36 (perf — mailrs Phase 1) — switch the integer / float
        // encoders to `write!`. `n.to_string()` allocates a fresh
        // `String` per cell just to push its bytes into the
        // (already-cleared) reuse buffer — for the 25 k-row JOIN
        // probe in `count_messages` that's 25 k heap allocs per
        // query. `write!(&mut String, ...)` formats straight into
        // the buffer; no intermediate alloc.
        Value::SmallInt(n) => {
            let _ = write!(out, "s{n}|");
        }
        Value::Int(n) => {
            let _ = write!(out, "I{n}|");
        }
        Value::BigInt(n) => {
            let _ = write!(out, "B{n}|");
        }
        Value::Float(x) => {
            let _ = write!(out, "F{x}|");
        }
        Value::Bool(b) => {
            out.push(if *b { 'T' } else { 'f' });
            out.push('|');
        }
        Value::Text(s) => {
            out.push('S');
            out.push_str(s);
            out.push('|');
        }
        Value::Vector(v) => {
            out.push('V');
            for x in v {
                out.push_str(&x.to_string());
                out.push(',');
            }
            out.push('|');
        }
        // v6.0.1: GROUP BY on a `VECTOR(N) USING SQ8` column.
        // Two cells with byte-identical `(min, max, bytes)`
        // share the same group; equivalence is byte-equality
        // (same as f32 grouping today — neither path tries to
        // normalise nan/-0).
        Value::Sq8Vector(q) => {
            out.push('Q');
            out.push_str(&q.min.to_string());
            out.push('@');
            out.push_str(&q.max.to_string());
            out.push(':');
            for b in &q.bytes {
                out.push_str(&b.to_string());
                out.push(',');
            }
            out.push('|');
        }
        // v6.0.3: GROUP BY on a `VECTOR(N) USING HALF` column.
        // Byte-equality over the raw u16 bits; matches the SQ8
        // path's byte-key model.
        Value::HalfVector(h) => {
            out.push('H');
            for b in &h.bytes {
                out.push_str(&b.to_string());
                out.push(',');
            }
            out.push('|');
        }
        Value::Numeric { scaled, scale } => {
            out.push('D');
            out.push_str(&scaled.to_string());
            out.push('@');
            out.push_str(&scale.to_string());
            out.push('|');
        }
        Value::Date(d) => {
            out.push('d');
            out.push_str(&d.to_string());
            out.push('|');
        }
        Value::Timestamp(t) => {
            out.push('t');
            out.push_str(&t.to_string());
            out.push('|');
        }
        Value::Interval {
            months,
            days,
            micros,
        } => {
            out.push('i');
            out.push_str(&months.to_string());
            out.push('m');
            out.push_str(&days.to_string());
            out.push('d');
            out.push_str(&micros.to_string());
            out.push('|');
        }
        Value::Json(s) => {
            out.push('j');
            out.push_str(s);
            out.push('|');
        }
        // v7.5.0 — Value is #[non_exhaustive] for downstream
        // forward-compat. Any future variant lacking explicit
        // handling here will share a debug-derived group key,
        // which is observably wrong but won't crash.
        _ => {
            out.push('?');
            out.push_str(&format!("{v:?}"));
            out.push('|');
        }
    }
}

/// v7.30 (perf campaign) - encode from borrowed cells without
/// materialising an owned Vec<Value> first.
pub(crate) fn encode_key_refs(vals: &[&Value]) -> String {
    let mut out = String::new();
    for v in vals {
        encode_one(&mut out, v);
    }
    out
}

/// v7.31 (perf 3e) — encode into a caller-owned scratch buffer.
/// The per-row key paths (group hash, DISTINCT set, join build/
/// probe) ran 24k+ String allocations per query through the
/// allocator just to LOOK UP a map; the scratch form allocates
/// only when a map actually has to take ownership (vacant insert).
pub(crate) fn encode_key_refs_into(vals: &[&Value], out: &mut String) {
    out.clear();
    for v in vals {
        encode_one(out, v);
    }
}

pub(crate) fn encode_key(vals: &[Value]) -> String {
    let mut out = String::new();
    for v in vals {
        encode_one(&mut out, v);
    }
    out
}

#[allow(clippy::cast_precision_loss)]
fn value_cmp(a: &Value, b: &Value) -> core::cmp::Ordering {
    use core::cmp::Ordering::Equal;
    match (a, b) {
        (Value::Null, Value::Null) => Equal,
        (Value::Null, _) => core::cmp::Ordering::Greater, // NULLs last
        (_, Value::Null) => core::cmp::Ordering::Less,
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::BigInt(x), Value::BigInt(y)) => x.cmp(y),
        (Value::Int(x), Value::BigInt(y)) => i64::from(*x).cmp(y),
        (Value::BigInt(x), Value::Int(y)) => x.cmp(&i64::from(*y)),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or(Equal),
        (Value::Int(x), Value::Float(y)) => f64::from(*x).partial_cmp(y).unwrap_or(Equal),
        (Value::Float(x), Value::Int(y)) => x.partial_cmp(&f64::from(*y)).unwrap_or(Equal),
        (Value::BigInt(x), Value::Float(y)) => (*x as f64).partial_cmp(y).unwrap_or(Equal),
        (Value::Float(x), Value::BigInt(y)) => x.partial_cmp(&(*y as f64)).unwrap_or(Equal),
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => Equal,
    }
}
